# llm-ctl — Full architecture + deep code review (qwen3.8-max)

Reviewed 2026-08-12 in 3 slices (A: architecture/main/supervisor/config, B:
concurrency/robustness, C: security/web/pi — pending). Findings reference code
as of this date (post-M10, session-binding, single-client, GGUF-picker).

Accepted constraints (not flagged): no auth / plain HTTP / LAN-only; one
llama-server worker (daemon is sole manager); header+specific `/api/session-active`
session binding; hardcoded dev PVE token; non-destructive migrations.

---

## Slice A — Architecture

### Architecture overview / data flow
- Single Axum daemon: `/api/*` control (CSRF-guarded POSTs) + static panel +
  everything else falls through to `proxy::fallback` (the `/v1` inferene path).
- `AppState` holds `cfg`, worker mutex, supervisor, optional Postgres store,
  runtime model registry, download/rebuild managers, http client, active_session.
- Worker lifecycle: optional autostart; spawn writes to XDG log; child-wait task
  marks `Stopped`/`Crashed` on exit; 2s poll loop does health + restart + slots/metrics.
- Model switch = stop-then-start on the same port (`start_model`).
- `/api/status-rollup` aggregates worker snapshot, models, sessions, jobs, cross-tier.
- Completion: client → `proxy::fallback` → local worker; `in_flight` + session id
  enforce the accepted single-active-client 409 gate.
- Postgres store is connected+migrated at boot; `/api/pve`, health, rollup fan out
  to PVE (1s timeout) and Postgres probe each call.

### Structural risks / SPOFs (real)
1. Daemon is a hard SPOF (owns supervision + proxy + session state); legacy llama
   reclamation only at boot.
2. Model switch has a real service-disruption window (stop-then-start same port).
3. **Postgres store is latched at boot** — if CT201 down at startup, `store` is
   `None` forever; `/api/db` can reconnect but doesn't restore the live store.
4. health/status-rollup fan out to cross-tier each call (can degrade the UI).
5. `/api/health` always returns `"status":"ok"` even when worker `Crashed`.
6. `active_session` is a global `Option<String>` hint with no TTL/lease — stale
   hints can persist if the proxy relies on it.
7. Runtime model registry is in-memory only — restart loses auto-registered models.
8. `pid_alive` relies on `/proc/<pid>/cmdline` containing `llama-server`.
9. Blocking fs/process calls (`worker_log_file`, `stop`->`/bin/kill`, `pid_alive`)
   in async paths.

### Slice-A TOP issues
1. **Restart cap broken**: `spawn()` resets `st.restarts=0`; `maybe_restart()` calls
   `spawn()`, so every restart resets the cap → `max_restarts` can't bound a crash
   loop. Also `WorkerState.restarts: u8` vs `Worker.max_restarts: u32`.
2. **Failed spawn wedges `Starting`**: state set to `Starting` before `cmd.spawn()?`;
   on spawn error state stays `Starting`, no PID; `poll`/`maybe_restart` ignore it.
3. **Lifecycle not fully serialized**: `start_model` holds `lifecycle`; `stop`,
   `maybe_restart`, shutdown `sup.stop()` don't → concurrent stop/start/restart
   can conflict on the port / resurrect an old model.
4. **Single-active-client gate not observable in this slice** (resolved in M11:
   `in_flight` set/cleared in `proxy.rs`).
5. **Postgres outage at boot silently disables persistence** until restart.

---

## Slice B — Concurrency / robustness

### Findings
| Severity | File:line | Title | Impact | Rec |
|---|---:|---|---|---|
| High | proxy.rs:170,181-228 | `in_flight` not drop/panic-safe | Cancel/panic after `in_flight=true` leaves 409 wedge until restart | Atomic/RAII busy guard that clears on drop; wrap stream task in `catch_unwind` |
| High | proxy.rs:419-477 | Streaming task no timeout; busy released only after DB record | Stalled SSE/tCP/hung DB wedges single-client slot forever | Per-chunk idle + overall turn timeout; timeout `record_turn` |
| High | download.rs:152-154; rebuild.rs:127-129 | Sequential stdout-then-stderr reads | Child blocks on full stderr pipe while stdout open → deadlock, job `running` forever | `tokio::join!` both pipes concurrently |
| High | store.rs:39-44,66-74,80-151 | No DB timeouts in completion path | Slow/down PG hangs `resolve_session`/`record_turn` while busy → wedge | Pool acquire + connect + `statement_timeout` |
| High | download.rs/rebuild.rs jobs | State can remain `running` on hang/panic/cancel | New jobs rejected forever | Kill handle + watchdog timeout + set failed on drop/panic |
| Med | store.rs:92-130 | `turn_index=MAX+1` race | Concurrent insert → unique violation, turn lost | `FOR UPDATE`/advisory lock or retry |
| Med | proxy.rs:227-228 | Non-stream records before busy clear/response | Slow PG delays response + holds slot | Record async / after busy clear, with timeout |
| Med | proxy.rs:442-453 | Client disconnect still drains when no recording | Abandoned empty-session request occupies worker to EOF | Abort drain if `client_gone && session_id empty` |
| Med | download.rs/rebuild.rs | Unbounded log collection before cap | Verbose jobs allocate unbounded memory | Cap during collection |
| Med | reap.rs:38-41 | Kills any proc with `llama-server` in cmdline | Collateral kill of shells/editors/grep | Match `/proc/comm`/exe basename |
| Med | proxy.rs:227,322-354 | Records non-2xx upstream as turns | Error JSON pollutes rollups | Record only success / persist status |
| Low | proxy.rs:152-160 | 409 overlap updates `last_request_at` | Idle-gating sees rejected activity | Move marker after busy acquire |
| Low | store.rs:67-69 | Session upsert doesn't update `model` | Stale model on switch | `DO UPDATE SET model=excluded.model` |
| Low | proxy.rs:436-438 | Oversized SSE line dropped | Turn recorded missing final stats | Truncate/parse boundaries |
| Low | proxy.rs:88-105 | No timeout on forwarded responses | Slow/huge upstream pins connection/memory | Add timeouts/size guard |
| Low | proxy.rs:27,35,108,... | Response builder `.unwrap()` | Panic not clean 500 | Handle builder error |
| Low | store.rs:208-215 | Cache% may double-count cached tokens | Misleading hit rate | Define token semantics |
| Low | reap.rs:13 | Blocking `thread::sleep(3)` | Blocks async runtime | `spawn_blocking`/async sleep |

### Slice-B per-subsystem notes
- **proxy.rs**: busy-flag lifecycle is the main risk (not panic/drop-safe;
  streaming task can hang in `stream.next()` or `record_turn`; `collector.lock().unwrap()`
  panics skip busy clear). Client disconnect correctly keeps draining, but does so
  even when `session_id` is empty. Byte-level SSE split is UTF-8-safe.
- **store.rs**: transaction shape sound, but `turn_index` allocation isn't
  concurrency-proof; no DB timeouts; `model` not updated on session upsert.
- **download/rebuild.rs**: job claim under lock correct, but pipe reads are
  sequential (deadlock risk), no cancellation/watchdog, logs unbounded,
  `cfg.cli[0]` can panic if empty.
- **reap.rs**: cmdline-substring matching too broad; blocking sleep in startup.

### Slice-B TOP 5 fixes
1. Make the single-client busy flag cancellation/panic-safe (RAII guard, atomic
   CAS, `catch_unwind`; fixes proxy 170/204/223/228/476).
2. Add hard timeouts around worker I/O, SSE drain, and DB writes.
3. Read child stdout/stderr concurrently + cap logs live (join + ring buffer).
4. Decouple Postgres recording from the busy slot; make `turn_index` race-safe.
5. Make download/rebuild jobs cancellable + fail-safe (watchdog, kill handle,
   not-stuck-`running`).

---

## Slice C — Security + web/pi + data

### Findings
| Sev | File:line | Title | Impact | Rec |
|---|---:|---|---|---|
| High | pve.rs:84-87,96 | PVE token leaked if TLS not fail-closed | If `cert_file` unset → `danger_accept_invalid_certs(true)`; if `proxmox.api` is `http`, token cleartext → PVE/CT control | Require https + cert_file; disable insecure fallback |
| Med | pve.rs:69-78 | Fragile PEM "pinning" parser | Bundle/chain mishandling → false sense of pinning | Use `rustls-pemfile::certs()` |
| Med | pve.rs:42-57 | Secret file path/perm hardening | `~/.keys` world-readable; `HOME` fallback to `.`; env leak | Enforce 0600, absolute path, systemd credential, redact in logs |
| Med | web/app.js:65-67,137 | Unescaped API fields in innerHTML | `r.requests`, `r.prompt_tokens`, `r.completion_tokens`, `f.size_mb` raw → DOM XSS if API/MITM | `Number(...)`/`esc(...)` everything |
| Med | infra/nginx/llm-ctl-ops.conf | No CSP/security headers | No defense-in-depth on XSS | Add strict CSP + headers in nginx |
| Med | web/app.js + backend | HF repo/include privileged inputs | Command injection / path traversal / malicious model if shelled | Validate repo regex, no shell, allowlist |
| Med | migrations | Stored prompts = PII/secrets, unbounded | Chat history leaks if DB reached | Prepared stmts, length limits, retention, DB TLS+SCRAM |
| Low | nginx/.nf/metrics | `/metrics` exposed via public .50 | LAN clients scrape metrics | Restrict to .52 |
| Low | firewall-llmctl.nft | Policy-accept beyond 8082 | Other host services exposed | Default-deny non-required |

### Notes
- SQLi safe IF all writes are parameterized (`$1..$n`); never string-concat session ids.
- Treat `X-Session-Id` as untrusted: validate `^[A-Za-z0-9_-]{1,64}$`, reject CR/LF/NUL, don't log raw.
- Storing `request_messages` is a data-leak decision — don't reflect into HTML/SSE/logs without redaction.
- SSE: `proxy_buffering off` is correct; JSON-encode event payloads, strip CR/LF.
- CSP additions: `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'`.

### Slice-C TOP 5
1. PVE TLS fail-closed (require https + pinned CA; drop `danger_accept_invalid_certs`).
2. PVE secret hardening (0600, no CWD fallback, redact).
3. Panel XSS hardening (escape/coerce all innerHTML fields + strict CSP headers).
4. Enforce `X-LLM-CTL`/JSON-only on mutating APIs; restrict `/metrics` to CT 202.
5. DB data protection (prepared stmts, validate ids/bodies, retention, DB TLS+SCRAM).

### Verdict
Mostly sane for a LAN-only no-auth control plane. The real risks are blast-radius:
unverified/plaintext PVE connection can leak an admin token; weak secret-file
handling; a few unescaped panel fields; stored prompts are high-value if Postgres
is reached. Fix PVE TLS/secret first, then panel CSP/escaping and DB retention.

---

## Consolidated TOP findings (all slices, prioritized)

**Correctness/concurrency (fix these first — real bugs):**
1. **Restart cap broken** (A) — `spawn()` resets `restarts=0`, defeating `max_restarts`.
2. **Failed spawn wedges `Starting` forever** (A) — no PID, `poll`/`maybe_restart` skip.
3. **`in_flight` not panic/cancel-safe** (B) — a dropped/panicked handler leaves a permanent 409 wedge.
4. **Streaming task + DB writes have no timeouts** (B) — stalled worker/PG wedges the single slot.
5. **download/rebuild pipe deadlock** (B) — sequential stdout-then-stderr reads can deadlock → job stuck `running`.
6. **Lifecycle not fully serialized** (A) — `stop`/`maybe_restart`/shutdown skip the `lifecycle` mutex.
7. **No DB timeouts in completion path** (B).
8. **reap.rs cmdline-substring match too broad** (B).

**Security/hardening (accepted-risk blast radius):**
9. PVE TLS fail-closed / danger_accept_invalid_certs fallback (C).
10. PVE secret file 0600 + redaction (C).
11. Panel XSS: unescaped numeric fields + no CSP in nginx (C).
12. Restrict `/metrics` to CT 202 (C).

**Polish:**
13. `turn_index` race on concurrent inserts (B) — mitigated by single-active replay.
14. Cache% token-semantics double count (B).
15. Unbounded job log collection (B).