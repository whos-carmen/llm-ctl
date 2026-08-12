## 1. Code Review

### High

**`supervisor.rs:141-146` — `std::thread::sleep` blocks tokio runtime**
```rust
for _ in 0..10 {
    if !pid_alive(pid) { break; }
    std::thread::sleep(Duration::from_millis(500)); // BLOCKS
}
```
Blocks the entire async runtime for up to 5s during worker shutdown. All health polls, proxy requests, and API calls stall.
**Fix:** `tokio::time::sleep(Duration::from_millis(500)).await`

**`proxy.rs:63,119` — Unbounded body reads (`usize::MAX`)**
```rust
let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
```
Any LAN client can exhaust memory by sending a multi-GB request body. LLM prompts are typically <1MB.
**Fix:** `axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await` (10MB limit)

### Medium

**`pve.rs:38` — TLS verification disabled**
```rust
.danger_accept_invalid_certs(true)
```
MITM on LAN trivial. Proxmox token exposed.
**Fix:** Pin the Proxmox host cert fingerprint, or use a local CA. For quick fix: fetch cert once, store SHA256, verify manually.

**`supervisor.rs:95-98` — Predictable `/tmp` log path**
```rust
.open("/tmp/llmctl-worker.log")
```
Symlink attack: attacker pre-creates `/tmp/llmctl-worker.log` as symlink to `/etc/passwd`, worker appends to it.
**Fix:** Use `$XDG_STATE_HOME/llm-ctl/worker.log` or `tempfile::NamedTempFile`.

**`supervisor.rs:155-157` — PID reuse race**
```rust
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
```
If worker crashes and PID is reused within 5s, `stop()` kills an unrelated process.
**Fix:** Verify `/proc/{pid}/cmdline` contains `llama-server` before killing.

**`proxy.rs:195-201` — Unbounded streaming buffer**
```rust
buf.push_str(&String::from_utf8_lossy(&b));
```
If upstream sends a chunk without newlines (e.g., 100MB JSON blob), `buf` grows unbounded.
**Fix:** Process line-by-line without accumulating: `while let Some(pos) = buf.find('\n') { ... }` then `buf.clear()` if no newline found after N bytes.

**`store.rs:165-170` — Predictable session IDs**
```rust
let n = SystemTime::now().duration_since(UNIX_EPOCH)...as_nanos();
let p = std::process::id() as u128;
format!("{:012x}", (n ^ (p << 32)) & 0xFFFF_FFFF_FFFF)
```
Trivially guessable. Attacker can enumerate sessions.
**Fix:** `use rand::Rng; format!("{:012x}", rand::thread_rng().gen::<u64>())`

### Low

**`proxy.rs:66,122` — `reqwest::Client::new()` per request**
Creates new TCP connection pool per request. Performance hit, not security.
**Fix:** Store `reqwest::Client` in `AppState`.

**`store.rs:108-145` — No transaction in `record_turn`**
Three separate queries. If third fails, session updated but turn not recorded.
**Fix:** Wrap in `sqlx::query("BEGIN")...sqlx::query("COMMIT")` or use `pool.begin()`.

**`pve.rs:28-36` — `~/.keys` permissions not checked**
If world-readable, Proxmox token leaks to any local user.
**Fix:** `assert!(metadata.permissions().mode() & 0o077 == 0)` or warn.

**`main.rs:56-58` — No auth on `/api/health`, `/api/pve`, etc.**
Any LAN device can query Proxmox API, list sessions, start/stop models.
**Fix:** Add auth middleware (see M8 plan).

### Streaming collector correctness

**`proxy.rs:220-240` — `Collector::feed`**
- `serde_json::from_str::<Value>(data)` on every SSE line: allocates heavily. For high-throughput streaming, consider `simd-json` or skip parsing non-final chunks.
- `buf.drain(..=pos).collect()` creates new `String` per line. Use `buf.drain(..=pos)` directly or `&buf[..=pos]`.
- No crash risk: malformed JSON handled with `let Ok(v) = ... else { return }`.
- No hang risk: `stream.next().await` has implicit timeout from `reqwest` (default 30s).

---

## 2. Security Design for M8

### Authentication: Static Bearer Token

**Recommendation:** Single static bearer token for all endpoints.

**Rationale:**
- Single-user, LAN-only: no need for multi-user auth, OAuth, or mTLS.
- Static token is simple, reversible, and sufficient for threat model.
- Prevents accidental exposure (IoT devices, guests, misconfigured routers).

**Implementation:**
```rust
// config.toml
[auth]
token = "env:LLM_CTL_TOKEN"  // or inline: "token = 'secret123'"

// middleware
async fn auth_middleware(
    State(st): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let expected = st.cfg.auth.token.as_deref().unwrap_or("");
    let got = req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if got == Some(expected) && !expected.is_empty() {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}
```

**Token storage:**
- Env var `LLM_CTL_TOKEN` (preferred, no file permissions issues).
- Fallback: `config.toml` with `token = "..."` (ensure `chmod 600 config.toml`).

**Tradeoffs:**
- **Pro:** Simple, no external dependencies, reversible.
- **Con:** Token leak = full access. Mitigate: rotate by restarting daemon, don't log token.

### TLS: Plain HTTP

**Recommendation:** Plain HTTP, no TLS.

**Rationale:**
- LAN is trusted. MITM requires LAN access (implies compromised device or ARP spoofing).
- Self-signed certs add friction: clients need `-k` or trust store updates.
- Local CA (step-ca/mkcert) is overkill for single-user.

**If TLS required later:**
- Self-signed cert generated at startup, fingerprint logged.
- Clients use `curl -k` or add to trust store once.
- No need for Let's Encrypt or local CA.

### Threat Model

**Assume:**
- LAN is trusted but imperfect. IoT devices, guests, or compromised laptops may exist.
- Attacker has LAN access but not root on the LLM host.

**Defend against:**
- Accidental exposure: bind to `127.0.0.1` by default, require explicit `listen.host = "0.0.0.0"` for LAN access.
- Casual snooping: bearer token prevents unauthorized API use.
- Proxmox token theft: pin cert fingerprint (see Medium fix above).

**Accept:**
- Sophisticated LAN attacks (ARP spoofing, compromised devices). Not worth defending against for single-user home.
- Local privilege escalation: if attacker has shell access, game over.

### M8 Plan

**1. Add bearer token auth (all endpoints)**
- `config.toml`: `[auth] token = "env:LLM_CTL_TOKEN"`
- Middleware: check `Authorization: Bearer <token>` on all routes.
- If token empty/missing, log warning and allow (backward compat).

**2. Bind to `127.0.0.1` by default**
- `config.example.toml`: `listen = { host = "127.0.0.1", port = 8082 }`
- Document: change to `0.0.0.0` only if LAN access needed.

**3. No CORS**
- Not a browser API. Omit `Access-Control-*` headers.

**4. No rate limiting**
- Single-user. Rate limiting adds complexity without benefit.

**5. Proxmox dev token**
- Keep as-is for now. Document in `config.example.toml`:
  ```toml
  # DEV token with full privileges. For production, create a restricted token:
  # pveum role add AgentRole -privs "VM.Audit,VM.PowerMgmt"
  # pveum user token add agent-admin@pam!agent-admin --privsep 0
  ```

**6. Fix High/Medium issues from code review**
- `tokio::time::sleep` in `supervisor::stop()`
- Bound body reads to 10MB
- Pin Proxmox cert or use local CA
- Predictable `/tmp` path → user-specific dir
- PID reuse → verify cmdline
- Unbounded streaming buffer → process incrementally
- Predictable session IDs → use `rand`

**7. Minimal changes, reversible**
- All changes are config-driven or simple code fixes.
- No new dependencies (except `rand` for session IDs).
- Can roll back by removing `[auth]` section from config.

### What NOT to do

- **No mTLS:** Overkill for single-user. Adds cert management burden.
- **No IP allowlist:** Fragile if IPs change. Bearer token sufficient.
- **No OAuth/OIDC:** No external auth provider needed.
- **No rate limiting:** Single-user, no benefit.
- **No audit logging:** Overkill. `tracing` already logs key events.