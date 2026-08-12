### 1. FINDINGS TABLE

| Severity | file:line | title | one-line impact | exploit/logic path |
|---|---|---|---|---|
| **High** | `supervisor.rs:131` | Slow startup marked as Crashed | Worker killed/stuck during model load | `poll()` checks health every 2s. If `llama-server` takes >2s to load the model, health returns 503/refused, state flips to `Crashed`. No restart logic exists, so it stays dead. |
| **High** | `proxy.rs:118,164` | `reqwest::Client` created per request | Port exhaustion, massive perf degradation | `Client::new()` builds a new connection pool per request. Under load, this exhausts ephemeral ports and file descriptors, causing 502s and daemon instability. |
| **High** | `store.rs:111` | `turn_index` race condition | Duplicate turn indices, broken UI ordering | `record_turn` reads `MAX(turn_index)+1` without a transaction or unique constraint. Concurrent completions for the same session will insert duplicate indices. |
| **Med** | `proxy.rs:265` | UTF-8 split corruption in SSE | Mangled emojis/Unicode in streamed output | `String::from_utf8_lossy(&b)` is called per chunk. If a multi-byte char is split across TCP chunks, it's replaced with ``, corrupting the stream and DB record. |
| **Med** | `supervisor.rs:105` | `start_model` TOCTOU race | Orphaned processes, port bind failures | Two concurrent `/start` requests drop the lock between check and `stop()`/`spawn()`. Both spawn a child, second fails to bind port, first might be killed. |
| **Med** | `download.rs:85` | Concurrent job guard race | Multiple HF downloads/rebuilds run simultaneously | Lock is dropped before state is set to `"running"`. Two fast API calls bypass the `status == "running"` check and spawn two background tasks. |
| **Med** | `main.rs:115` | Health endpoint blocks for 8s | Health check timeouts, false negatives | `health()` awaits `pve::list_containers()`. If PVE is down, the 8s reqwest timeout blocks the axum handler, stalling the health endpoint. |
| **Low** | `download.rs:133` | Blocking `std::fs` in async context | Tokio worker thread starvation | `resolve_gguf` recursively walks the HF cache using synchronous `std::fs::read_dir`. Large caches will block the tokio runtime. |
| **Low** | `supervisor.rs:180` | Missing `restart_on_crash` logic | Dead worker requires manual restart | Config has `restart_on_crash` and `max_restarts`, but the poller never checks or acts on them. |
| **Low** | `config.rs:30` | `~` not expanded for paths | Confusing "file not found" errors | `expand_tilde` is only used in `pve.rs`. `llama.repo` and `hf.cache` treat `~` as a literal directory name. |

---

### 2. Deep subsystem checks

#### proxy.rs
- **Fallback routing**: Axum 0.7 `matchit` handles `/api/models/:id/start` (4 segments) and `/api/models/stop` (3 segments) correctly. No shadowing.
- **Streaming channel**: The `client_gone` drain path is logically sound, but the UTF-8 split bug (`from_utf8_lossy` per chunk) is a real data corruption issue for non-ASCII outputs.
- **Non-stream parse**: `upstream.bytes()` reads the whole body. Bounded by `max_tokens`, so OOM is unlikely, but technically unbounded if upstream misbehaves.
- **Model enforcement**: Empty `model` field correctly falls back to `active`. Not exploitable.
- **`/v1/models` leak**: Pass-through leaks absolute GGUF paths. Acceptable for LAN-only single-user, but worth noting.

#### supervisor.rs
- **Spawn/stop races**: The TOCTOU in `start_model` is real. The lock is dropped between the `pid.is_some()` check and the actual `stop()`/`spawn()` calls.
- **PID reuse**: `/proc/{pid}/cmdline` check is a solid guard against killing unrelated processes.
- **Wait-task guard**: Correctly checks `st.pid == pid` to avoid overwriting a newly spawned process if the old one exits late.
- **Health poll**: The 2s interval + immediate `Crashed` on failure is the biggest logic flaw. Model loading takes 10-30s; the supervisor will kill it.

#### store.rs
- **SQLi**: All queries use parameterized bindings (`$1`, `$2`). Safe.
- **`record_turn`**: The `turn_index` race is real. Also, the `UPDATE sessions` rollup is not in a transaction with the `INSERT`, so a crash between them leaves inconsistent totals.
- **Session heuristic**: 30-min reuse is fine for single-user. 64-bit entropy for session IDs is plenty.

#### download.rs / rebuild.rs
- **Argv injection**: `Command::new` + `.arg()` prevents shell injection. Repo names starting with `-` will just cause `uvx` to print help and exit.
- **Path traversal**: `repo.replace('/', "--")` prevents `../` traversal because the `/` is stripped before `Path::join`. Safe.
- **Blocking I/O**: `resolve_gguf` uses `std::fs` in a `tokio::spawn`. Should use `spawn_blocking`.

#### pve.rs
- **Secret handling**: Reads `~/.keys`. Standard for personal scripts.
- **Cert pin**: `split("-----BEGIN CERTIFICATE-----")` works, but `reqwest::Certificate::from_pem` might fail if there's trailing garbage. It logs and skips, which is fine.

#### main.rs
- **Route table**: No conflicts.
- **Poller**: Lacks restart logic. The `restart_on_crash` config is completely ignored.

---

### 3. CORRECTNESS + AVAILABILITY

- **Blocking calls in async paths**:
  - `resolve_gguf` (`download.rs:133`) recursively walks the HF cache using synchronous `std::fs::read_dir`. **High risk**: Large caches will block the tokio worker thread, starving the proxy and health checks.
  - `pid_alive` (`supervisor.rs:188`) and `worker_log_file` (`supervisor.rs:168`) use `std::fs`. Low risk (tiny files), but technically blocking.
- **Unbounded memory/queue growth**:
  - SSE `buf` is capped at 64KB. `Collector` strings grow with response, but bounded by LLM context. Log buffers capped at 500/800 lines. All safe.
- **Postgres down at request time**:
  - `resolve_session` fails gracefully, logs a warning, returns empty `session_id`. The proxy still forwards the request to `llama-server`. **Excellent availability design.**
- **Proxmox down**:
  - `health` endpoint awaits `pve::list_containers()`. If PVE is unreachable, the 8s reqwest timeout blocks the axum handler, stalling the health endpoint and potentially triggering false negatives in upstream monitors.

---

### 4. TOP 10 PRIORITIZED FIXES

**1. Move `reqwest::Client` to `AppState` (proxy.rs:118, 164)**
Creating a client per request leaks connection pools and exhausts ports.
```rust
// In AppState: pub http_client: reqwest::Client,
// In main.rs: http_client: reqwest::Client::new(),
// In proxy.rs: let upstream = st.http_client.request(...).send().await;
```

**2. Fix health poll to tolerate `Starting` state (supervisor.rs:131)**
Don't mark as `Crashed` immediately. Track `started_at` and only flip to `Crashed` if health fails after e.g. 60 seconds, or if the PID is dead.
```rust
if ok { st.status = WorkerStatus::Ready; } 
else if st.status != WorkerStatus::Starting { st.status = WorkerStatus::Crashed(...); }
```

**3. Wrap `record_turn` in a SQL transaction (store.rs:111)**
Prevents duplicate `turn_index` and inconsistent rollups under concurrent requests.
```rust
let mut tx = self.pool.begin().await?;
// ... run queries using &mut *tx ...
tx.commit().await?;
```

**4. Fix UTF-8 split corruption in SSE (proxy.rs:265)**
Buffer raw bytes and only convert to string after finding `\n`, or use `tokio_util::codec::LinesCodec`.
```rust
// Quick fix: keep `buf` as Vec<u8>, find b'\n', then String::from_utf8_lossy(&line_bytes).
```

**5. Hold mutex across `start_model` lifecycle (supervisor.rs:105)**
Prevents TOCTOU race where two concurrent requests spawn two workers.
```rust
let mut st = self.state.lock().await;
if st.model_id.as_deref() == Some(model.id.as_str()) && st.pid.is_some() { return Ok(()); }
if st.pid.is_some() { /* stop logic inline or release/reacquire carefully */ }
```
*Better: use a dedicated `tokio::sync::Mutex` just for the spawn/stop lifecycle.*

**6. Fix concurrent job guard race (download.rs:85, rebuild.rs:73)**
Hold the lock while updating the state to `"running"`.
```rust
let mut st = self.state.lock().await;
if st.status == "running" { return Err(...); }
st.status = "running".into();
st.started_at = Some(now());
// drop lock, then spawn
```

**7. Add restart logic to the poller (main.rs:115)**
Actually use the `restart_on_crash` config.
```rust
if let WorkerStatus::Crashed(_) = st.status {
    if st.restarts < st.max_restarts { sup.spawn(...).await; }
}
```

**8. Wrap `resolve_gguf` in `spawn_blocking` (download.rs:133)**
Prevents blocking the tokio runtime during large directory walks.
```rust
let path = tokio::task::spawn_blocking(move || resolve_gguf(&cache, &repo)).await.unwrap();
```

**9. Use a shorter timeout for PVE health check (main.rs:115 / pve.rs)**
Don't let the health endpoint hang for 8s.
```rust
// In health handler:
let pve = tokio::time::timeout(Duration::from_secs(1), pve::list_containers(&st.cfg)).await.is_ok();
```

**10. Apply `expand_tilde` to all config paths (config.rs:30)**
Prevents confusing "file not found" errors when users put `~/llama` in config.
```rust
// In Config::load, post-process llama.repo, llama.build_dir, hf.cache with expand_tilde.
```