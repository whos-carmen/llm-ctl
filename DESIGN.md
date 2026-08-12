# llm-ctl — Design

Control plane + session-aware proxy for llama.cpp on this machine.
A single Rust binary that is the **only** `llama-server` manager on the host,
spawns/loads one model at a time for a single client session, rebuilds llama.cpp
from source, downloads models from HuggingFace, and serves a Tailwind web panel
with a dark "isotope" theme.

Status: **design** - nothing built yet.

---

## 1. Goals

Same feature set today lives in `llama/scripts/{llama-proxy.py, llama-monitor.py}`
plus the `serve-*.sh` launchers. They work but are three separate, half-manual
Python pieces. `llm-ctl` collapses them into one fast binary with a web UI.

1.  **Proxy (Thing 1).** An OpenAI-compatible endpoint local agents connect to
    (`:8082/v1/*`) that serves the locally running llama-server instance.
    Transparent streaming pass-through, plus per-request logging.
2.  **Control + sessions (Thing 2).** A web panel that can:
    - load / stop the model (one model at a time),
    - **download models from HuggingFace** (using existing `hf` CLI auth),
    - rebuild and relaunch the latest llama.cpp from source,
    - store session/cache info on the server side so **clients** can query
      cache hit/miss and timing for their own sessions.

Constraints from the user (all decided):

- Whole server **not in Python**; implement in **Rust**.
- Tailwind CSS front end.
- Dark scheme matching Plasma theme / Konsole "isotope" (base16-isotope).
- **`llm-ctl` is the only llama(**cpp**) server on this machine.** It kills and
  cleans up any other llama-server processes on startup; no coexistence.
- **Models can be downloaded from HuggingFace** (user already auth'd via
  `uvx hf auth`).
- **Serve 1 model at a time to a single client.** One model loaded, one slot
  (`-np 1`), one client at a time. Loading another model stops the current one.

Non-goals:

- No changes to the `llama.cpp` source tree (its AGENTS.md forbids agents from
  committing/pushing/opening PRs; this project never does that either).
- No multi-machine / remote GPU management (local only).
- No multi-model concurrency (deliberately single-slot, see constraint above).
- No fine-tuning, quantising, or model conversion in the UI (scripts exist for
  this; out of scope).

---

## 2. Existing environment (ground truth)

| Thing | Value |
| --- | --- |
| OS / env | Linux, `/home/pixie`, bash |
| CPU | AMD Ryzen 9 7900X (12C), 61 GiB RAM |
| GPU | AMD Radeon RX 7900 XTX (`gfx1100`), ROCm/HIP build |
| Toolchains present | `cmake`, `ninja`, Node v26 / npm 12, `uvx` (uv) |
| Rust / Zig | **not installed** (rustup install required) |
| llama.cpp | `/home/pixie/llama`, clean `master`, HIP build in `build/` |
| Running now | a legacy `llama-server` on `:8080` (Nemotron IQ4_XS) - **reaped on `llm-ctl` startup** |
| Current proxy | `scripts/llama-proxy.py` (aiohttp) on `:8082`, SQLite sessions |
| HF auth | present via `uvx hf auth`; token in `~/.cache/huggingface/token`; models cached in `~/.cache/huggingface/hub/` |
| Proxmox | single-node PVE 9.2.2, node **`px360` @ `192.168.4.111`**; API token `agent-admin@pam!agent-admin` (**dev-only**), secret in `~/.keys` as `PROXMOX_AGENT_ADMIN` |
| PVE storage | `local` (dir), `nvme1` (lvm, 512 GiB, images+rootdir), `local-lvm` (lvmthin), `backup` (disabled) |
| PVE existing CTs | `100 ilo-fan` / `103 deb-dns` (tag `system`), `300 quake-live` / `301 star-tech` (tag `games`) - all stopped |

Model files live in `/home/pixie/llama/models/`, `/home/pixie/llama/logs/`
(converted in place) and `~/.cache/huggingface/hub/` (HF-downloaded). The
machine is single-GPU with a large RAM pool; big MoE models can be partially
offloaded, but only one is resident at a time by design.

### Deployment topology

- **This host** (`192.168.7.23`, a workstation): runs `llm-ctl`, llama.cpp and
  HF models - the **LLM tier**. Everything GPU/LM lives here.
- **Proxmox node `px360`** (`192.168.4.111`): hosts *everything else* for this
  project (the **ops tier**) as LXC containers.
- **Container numbering/labeling convention**: this project's containers are
  numbered starting at **200**, ascending, and tagged **`ai`**. Existing
  unrelated CTs occupy `100, 103` (tag `system`) and `300, 301` (tag `games`);
  the free run `200, 201, 202, ...` is collision-free.
  Ops-tier services for this project (one named service per CT; IPs allocated
  from **`192.168.7.50`** upward on the PVE bridge):
  - `200 llm-ctl-ops`  `192.168.7.50` - reverse proxy / public TLS face,
  - `201 llm-ctl-db`   `192.168.7.51` - PostgreSQL (session/cache store),
  - `202 llm-ctl-mon`  `192.168.7.52` - monitoring + log aggregation.
  (Range is collision-free against this host at `192.168.7.23`.)
- Access to PVE uses the `agent-admin` API token in `~/.keys`
  (`PROXMOX_AGENT_ADMIN`). **Development only** - replace with a least-privilege
  credential and a real secret store before anything leaves localhost.

---

## 3. Architecture

One binary, one listen socket `:8082`, three faces over a shared model state.
Because the host serves **one model to one client**, there is exactly **one**
worker child and a fixed worker port.

```
                      llm-ctl  (one Rust binary / one port :8082)
  ┌────────────────────────────────────────────────────────────────────┐
  │  Proxy face              Control API face            Web panel      │
  │  /v1/*  /models          /api/*                     "/"  + static   │
  │  /instances/*            (JSON)                     (Tailwind UI)   │
  │        └───────────┬───────────────────────────────────┘            │
  │                    ▼                                                 │
  │              ┌──────────────┐   ┌───────────────┐   ┌─────────────┐  │
  │              │supervisor.rs │   │    store.rs   │   │ rebuild.rs  │  │
  │              │ (single act- │   │ (Pg @ 201)   │   │ (build job) │  │
  │              │  ive model)  │   └───────────────┘   │ download.rs │  │
  │              └──────┬───────┘   ┌───────────────┐   │ (HF job)    │  │
  │                     │           │    reap.rs    │   └─────────────┘  │
  │                     │           │ (sole-owner)  │                     │
  └─────────────────────┼───────────┴───────────────┴────────────────────┘
                        │ spawn / stop / health / streams stdout
                        ▼
              ┌─────────────────────────┐
              │  llama-server 127.0.0.1 │   (one worker, one model,
              │  :8080  -np 1  (slot 1) │    single client)
              └─────────────────────────┘
```

- `supervisor.rs` keeps a single **active** model slot. Starting a model while
  another is running = confirm, then stop the current worker and start the new
  one on the same fixed port.
- `download.rs` runs HuggingFace downloads as a job (like `rebuild.rs`).
- `reap.rs` (or logic inside supervisor startup) takes sole ownership: kills
  any pre-existing `llama-server` processes and cleans stale state/logs.

### Ports

| Port | Owner | Purpose |
| --- | --- | --- |
| `:8082` | llm-ctl | single listen socket: proxy `/v1/*`, JSON API `/api/*`, web panel `/`. Binds `0.0.0.0` (LAN-open, headless host reached from other devices) - NO auth, so any LAN device can use it. |
| `:8080` | llama-server | the single worker (fixed; configurable) |

The worker binds `--host 127.0.0.1` (loopback only) since llm-ctl is the only
public face. `llm-ctl` is the parent of the worker (`tokio::process`) so it can
track the pid, restart on crash (capped), and clean up on exit.

### Sole ownership (kill & clean)

On daemon startup, `llm-ctl` asserts control over this host's llama.cpp:

1. Scan `/proc` (or `pgrep -f <llama-server binary path>`) for existing
   `llama-server` processes, including the legacy one on `:8080`.
2. SIGTERM each, wait a grace period, SIGKILL stragglers.
3. Optionally rotate/clear stale worker log files from a previous `llm-ctl`
   run so the live-ring buffer starts clean.

This makes `llm-ctl` the single source of truth for llama-server lifecycle;
nothing else may keep a llama-server running. (This is intentional and
requested; the current manually-started `:8080` instance is the first casualty.)

---

## 4. Module breakdown

### 4.1 `config.rs`
TOML config, e.g. `~/.config/llm-ctl/config.toml` (overridable with
`LLM_CTL_CONFIG` env):

```toml
listen = "127.0.0.1:8082"

[llama]
repo            = "/home/pixie/llama"
build_dir       = "/home/pixie/llama/build"
binary          = "/home/pixie/llama/build/bin/llama-server"
git_remote      = "origin"
git_branch      = "master"

[worker]
port = 8080
# flags applied to the single spawned llama-server
default_args = ["-ngl", "99", "--flash-attn", "on", "-c", "131072",
                "-ctk", "q8_0", "-ctv", "q8_0", "-np", "1", "--metrics",
                "--host", "127.0.0.1"]
gpu_env = { HIP_VISIBLE_DEVICES = "0" }
restart_on_crash = true
max_restarts = 5

[hf]
# downloaded via `uvx hf download`, auth from ~/.cache/huggingface/token
cli = ["uvx", "hf"]            # invoked as "uvx hf download ..."
cache = "~/.cache/huggingface"
autoregister = true            # add finished downloads to the model list

[proxmox]
# hardcoded for now; single-node PVE - hosts the ops tier (everything but LLM).
api = "https://192.168.4.111:8006/api2/json"
node = "px360"
token_id = "agent-admin@pam!agent-admin"   # DEV ONLY - secret in ~/.keys (PROXMOX_AGENT_ADMIN)
ct_storage = "nvme1"           # lvm, images+rootdir - used for this project's CTs
ct_tag = "ai"                  # container tag for this project
ct_vmid_start = 200            # number this project's containers from here

# Ops-tier containers for this project (all tag "ai"), one named service each:
[proxmox.cts]
# vmid = "name,ip"  - IPs assigned from 192.168.7.50 upward
"200" = "llm-ctl-ops,192.168.7.50"    # reverse proxy / public TLS face
"201" = "llm-ctl-db,192.168.7.51"     # PostgreSQL (session/cache store)
"202" = "llm-ctl-mon,192.168.7.52"    # monitoring / log aggregation

# session/cache store - PostgreSQL on the ops tier (CT 201 llm-ctl-db @ 192.168.7.51)
[db]
pgurl = "postgres://llm_ctl:REDACTED@192.168.7.51:5432/llm_ctl"   # creds filled at provision time
schema_migrate = true

# Candidate models (only ONE is active at a time; pick via the UI/model field)
# at least one should have autostart = true
[[models]]
id          = "nemotron-iq4xs"
model       = "/home/pixie/.cache/huggingface/hub/models--bartowski--NVIDIA-Nemotron-3.5-Lightning-30B-A3B-GGUF/snapshots/.../NVIDIA-Nemotron-3.5-Lightning-30B-A3B-IQ4_XS.gguf"
autostart   = true
description = "Nemotron 3.5 Lightning 30B-A3B, IQ4_XS"
```

Model ids are the `model` value clients send. Downloads from HF are added to
this list automatically (`autoregister`) once their GGUF path is resolved.

### 4.2 `store.rs` - sessions/cache store (PostgreSQL, sqlx)
By decision, the session/cache store lives on the **ops tier**: PostgreSQL on
container **`llm-ctl-db` (201)**. `llm-ctl` runs on the LLM tier and connects
over the LAN through `sqlx` (async connection pool); every write goes through a
background writer task (channel) so the proxy thread never blocks on a network
round-trip. Schema is ported from `llama-proxy.py` verbatim (all fields there
are kept); only the field types become PostgreSQL-native (see note below).

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    name TEXT,
    created_at REAL,
    updated_at REAL,
    model TEXT,
    total_requests INTEGER DEFAULT 0,
    total_prompt_tokens INTEGER DEFAULT 0,
    total_completion_tokens INTEGER DEFAULT 0,
    total_cache_tokens INTEGER DEFAULT 0,
    total_prompt_ms REAL DEFAULT 0,
    total_completion_ms REAL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS turns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    turn_index INTEGER NOT NULL,
    timestamp REAL,
    request_model TEXT,
    request_messages TEXT,          -- JSON
    request_max_tokens INTEGER,
    request_temperature REAL,
    response_id TEXT,
    response_content TEXT,
    response_finish_reason TEXT,
    -- llama-server timings object
    cache_n INTEGER DEFAULT 0,      -- tokens served from KV cache (hit)
    prompt_n INTEGER DEFAULT 0,     -- tokens actually evaluated (miss)
    prompt_ms REAL DEFAULT 0,
    prompt_per_second REAL DEFAULT 0,
    predicted_n INTEGER DEFAULT 0,
    predicted_ms REAL DEFAULT 0,
    predicted_per_second REAL DEFAULT 0,
    -- usage object
    usage_prompt_tokens INTEGER DEFAULT 0,
    usage_completion_tokens INTEGER DEFAULT 0,
    usage_cached_tokens INTEGER DEFAULT 0,
    duration_ms REAL DEFAULT 0,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);
```

Schema above is shown in the SQLite-ish source form for reference. Rendered for
PostgreSQL: `turns.id` -> `BIGINT GENERATED ALWAYS AS IDENTITY` (`AUTOINCREMENT`
is SQLite-only), timestamps/timings as `TIMESTAMPTZ` / `DOUBLE PRECISION`,
`request_messages` as `JSONB`, `response_content` as `TEXT`. The proxy-facing
column set and the cache-hit semantics are unchanged.

Session binding: a client is bound to a session by id, either client-supplied
via `X-Session-Id` header or derived by the existing "active session within
30 min" heuristic (drop-in compatible with today).

Cache hit rate = `cache_n / (cache_n + prompt_n)` or, from usage,
`prompt_tokens_details.cached_tokens / prompt_tokens`. Cache counts live both
per-turn and rolled up on `sessions.*`.

### 4.3 `proxy.rs` - OpenAI-compatible reverse proxy
- Single worker: forwards to `http://127.0.0.1:{worker.port}`.
- Reads the `model` field and checks it equals the **active** model id. If no
  model is active, or the requested model differs from the active one, respond
  `404`/`409` with a hint ("model X not loaded; load it via the panel"). No
  cross-model routing (only one is ever live).
- Single client: relies on the worker's `-np 1` single slot (llama-server
  serializes/deferes). Optionally track a "busy" flag to reject/queue a second
  distinct client until the current turn finishes (see open question SS8).
- Passes through non-JSON / GET endpoints (`/v1/models`, `/health`, `/props`)
  and forwards `/metrics`, `/slots`, `/props` of the worker.
- Streaming: pipes SSE chunks straight back to the client while a tee
  collector reassembles the final `timings` + `usage` for logging (same
  technique as `llama-proxy.py`, line 517+).
- Non-streaming: forward, capture body, parse `timings` + `usage`, log, return.
- Records one `turns` row per request (fields in SS4.2).
- Adds a small caching-class field to streamed `data:` chunks when the upstream
  does not, so clients can surface cache hit/miss live in `timings`/`usage`.

### 4.4 `supervisor.rs` - single active model + lifecycle
State: one active slot, plus the list of candidate models.

```rust
struct Active {
    id: String,
    model_path: String,
    pid: Option<u32>,
    status: Status,             // Stopped | Starting | Ready | Stopping | Crashed |
    started_at: Option<Instant>,
    restarts: u8,
    log_ring: RingBuffer<u8>,   // live tail for the UI
    last_slot: Option<SlotSnapshot>,   // /slots + /metrics
}
```

Operations:
- `claim_ownership()` - reap any pre-existing llama-server processes (SS3).
- `start(id)` - if another model is active, refuse unless `force` (UI confirms);
  build argv (`default_args + -m <model> --port <port>`), set GPU env, spawn,
  poll `/health` until ready, mark `Ready`.
- `stop(reason)` - SIGTERM, wait, SIGKILL after grace; clear pid.
- `switch(new_id)` - stop current (drain/confirm), then `start(new)`.
- restart-on-crash with `max_restarts` cap + backoff.
- poller task: each ready worker is polled for `/metrics` + `/slots` (cache hit,
  ctx used/max, requests_processing/deferred, busy slots) into `last_slot`.
- `list()` - candidate models with `status` (Stopped / Ready) + whether active.

### 4.5 `download.rs` - HuggingFace model downloads
A job manager (one at a time) that fetches a GGUF from HuggingFace using the
existing CLI auth, then registers it as a launchable model.

- Invokes `uvx hf download <repo> --include '<pattern>'` (reuses the token from
  `~/.cache/huggingface/token`; `hf` handles LFS, checksums, resume, caching).
  `uvx` boot cost (~1s) is acceptable for an interactive download job.
  Selection/quant done by repo + `--include '*.gguf'` or explicit filename.
- Captures combined stdout/stderr into a live, tail-capable buffer streamed to
  the UI (SSE), same pattern as `rebuild.rs`.
- On success, resolves the GGUF from
  `~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<rev>/` and, if
  `hf.autoregister`, appends it to the model list with a derived `id`.
  Optionally record file size + repo for the Models view.
- Api: `POST /api/hf/download`, `GET /api/hf/download` (status+log), SSE events.
- Note for later: a native Rust downloader (HF Hub API + streaming + token
  reuse) is a possible replacement; out of scope for v1.

### 4.6 `rebuild.rs` - pull + build job manager
- One job at a time. Job = `git pull --ff-only origin master` then
  `cmake --build build -j$(nproc)` in the `llama/` tree.
- Captures combined stdout/stderr and appends to a live, tail-capable log
  buffer streamed to the UI (SSE).
- Records before/after git SHA (`git rev-parse HEAD`) for the UI.
- On success, binary path is updated so the next launch/relaunch uses the fresh
  build; the running worker keeps serving until explicitly relaunched
  (non-disruptive).
- Never runs `git push`; never modifies source files (AGENTS.md compliance).

### 4.7 `api.rs` - JSON API (feeding the panel)

| Method / path | Purpose |
| --- | --- |
| `GET  /api/health` | daemon + worker health |
| `GET  /api/models` | candidate models: id, path, status, active?, gpu, metrics |
| `POST /api/models/{id}/start` | load a model (switches if another is active) |
| `POST /api/models/{id}/stop` | stop the running worker |
| `GET  /api/models/{id}/log?limit=` | tail of live log |
| `POST /api/rebuild` | start pull+build job |
| `GET  /api/rebuild` | job status + live log (SSE) |
| `GET  /api/rebuild/events` | SSE stream for rebuild log |
| `POST /api/hf/download` | start an HF download (repo + optional include/filename) |
| `GET  /api/hf/download` | download job status + live log (SSE) |
| `GET  /api/hf/download/events` | SSE stream for download log |
| `GET  /api/sessions?limit=&model=` | session table |
| `GET  /api/sessions/{id}` | session summary + per-turn breakdown |
| `GET  /api/sessions/{id}/turns` | raw turns (what **clients** consume for cache stats) |
| `GET  /api/worker/metrics` | forwarded `/metrics` |
| `GET  /api/worker/slots` | forwarded `/slots` |
| `GET  /api/events` | SSE: live model/session/download/rebuild events |

`/api/sessions/*` are the "serve that info to clients" surface: any client
holding a session id can pull its cache hit/miss + timings. Backed by Postgres
on container `201`, so history survives `llm-ctl` restarts and is shared with
the ops tier.

### 4.8 `web.rs` + `web/` - ops panel (Tailwind v4)
- Static assets built once by Tailwind (CSS + a bit of JS), embedded via
  `include_str!` / `rust-embed` so the binary is self-contained.
- Views:
  - **Models** - card for the active worker: status dot, GPU/VRAM, cache hit %,
    prompt & eval tps, ctx used/max, open questions for switching, Start/Stop,
    live log tail. Plus the list of candidate (downloaded/configured) models.
  - **HF** - download a repo: repo id + filename/pattern, live progress log,
    auto-registered models.
  - **Rebuild** - current SHA, "Pull + build" button, streaming log, ready flag.
  - **Sessions** - table (id, model, requests, cache%, prompt/output tok, last
    active), click-through to per-turn breakdown with cache% + tps per turn.
- Live updates via **plain fetch polling** (~2.5s) of `/api/status-rollup`.
  Deviation from earlier drafts: SSE was considered but rejected in favor of
  polling for a single-user LAN tool (per the qwen3.7-max M9 consultation,
  `docs/qwen-m9-consult-2026-08-12.md`).

---

## 5. Stack and rationale

| Piece | Choice | Why |
| --- | --- | --- |
| Language | Rust | Covers proxy + DB + process mgmt + embedded web in one binary; axum/tokio ecosystem is proven; same stack as llama-swap. Zig rejected for the daemon: HTTP/JSON/streaming would be largely hand-built. |
| HTTP server | axum (tokio/hyper) | Matched handlers, SSE support, easy static serving + state sharing. |
| Streaming client | reqwest (or raw hyper) | Forward SSE with minimal buffering; large-body safety. |
| DB | sqlx + PostgreSQL on ops tier (CT `201`) | Session/cache store lives on Proxmox by decision; async pool + schema migrations; proxy writes fire-and-forget over a channel. |
| Process mgmt | `tokio::process` | Async spawn/wait, clean ownership of children; used for worker + rebuild/download jobs. |
| Config | `toml` + `serde` | Simple editable config mirroring today's flags. |
| HF downloads | `uvx hf download` (shell) | Reuses existing `hf` CLI auth; handles LFS/checksum/resume. Rust-native downloader deferred. |
| PVE access | reqwest -> Proxmox REST API (`:8006`, token auth) | `Authorization: PVEAPIToken=...`; thin read view of ops-tier CTs now, full control in a follow-up. |
| Front end | Tailwind v4 (`@tailwindcss/cli`) | Standalone build step; single CSS artifact. Plain TS/vanilla (or htmx) - a dashboard, not a JS app. |

Rust toolchain is not installed yet: bootstrap via rustup (`~/cargo`, `~/rustup`).

---

## 6. Theme - "isotope" -> Tailwind tokens

Sourced from the real theme files on disk:
`~/.dotfiles/plasma/IsotopeDark.colors` (base16-isotope, Plasma 6 port) and
`~/.dotfiles/konsole/Isotope.colorscheme`, plus the curated
`~/.kimi-code/themes/isotope.json`.

Tailwind v4 CSS-first theme (sketch):

```css
@import "tailwindcss";

@theme {
  --color-base: #0a0a0a;      /* near-black (konsole #000, softened) */
  --color-surface: #101010;   /* Plasma Window normal */
  --color-raised: #1a1a1a;    /* Plasma View / Button normal */
  --color-raised2: #1c1c1c;   /* Plasma alternate rows */
  --color-line: #3c3c3c;      /* focus/border (kimi border) */
  --color-text: #d0d0d0;      /* foreground */
  --color-muted: #808080;     /* foreground dim */
  --color-faint: #6b6b6b;     /* muted */
  --color-strong: #ffffff;    /* textStrong / Color7Intense */

  --color-cyan: #00ffff;      /* brand/links/active (Color6, kimi primary) */
  --color-lime: #33ff00;      /* success/loaded (Color2, kimi accent) */
  --color-magenta: #ff0099;   /* warning/user (Color3) */
  --color-red: #ff0000;       /* error (Color1) */
  --color-blue: #0066ff;      /* selection / link (Color4) */
  --color-purple: #cc00ff;    /* shell/aux (Color5) */
  --color-orange: #ff9900;    /* neutral/link-visited (base09) */
}
```

Text on near-black backgrounds; section each panel with a `surface` bg and a
thin `line` border. Use cyan + lime sparingly for status only (active = lime,
error = red, busy/downloading = cyan), never for layout chrome, to keep the
"neon-on-black" scheme from becoming noisy.

---

## 7. Milestones / build order

Ordered so the Proxmox **ops tier** (which holds the session DB) exists before
the LLM-tier work that depends on it. Tags: **(A)** LLM tier (this host),
**(B)** ops tier (`px360`) / **glue** (cross-tier channel). The spine M1-M5 is
fixed; M6-M9 can be re-ordered to taste.

1. **(B) Provision the PVE ops tier (as IaC).** Drive it from version-controlled
   config in this repo (`infra/`, see below) rather than ad-hoc clicks, so the
   tier is reproducible and reviewed. Create three LXC containers on `nvme1`,
   all tagged `ai`, static IPs allocated from **`192.168.7.50`** upward:
   - `200 llm-ctl-ops` `192.168.7.50` - reverse proxy / TLS,
   - `201 llm-ctl-db`  `192.168.7.51` - PostgreSQL (session store),
   - `202 llm-ctl-mon` `192.168.7.52` - monitoring / logs.
   Install base packages + SSH. Verify from the workstation: the PVE API sees
   the three CTs with `tags=ai`; VMIDs 200-202 and IPs .50-.52 are collision-free
   (existing CTs are `100,103,300,301`).

2. **(A + glue) Bootstrap + sole-owner + cross-tier plumbing.** Install
   rustup/cargo; `cargo new`; axum hello. Implement `claim_ownership()` (reap
   legacy llama-server incl. the `:8080` Nemotron; SIGTERM -> grace -> SIGKILL);
   spawn the configured worker; proxy `/v1/*` to it - local plumbing proven.
   Then prove the A<->B channel: load `[proxmox]` + `[db]` config, have `llm-ctl`
   list its `ai` CTs via the PVE API, and open a TCP/`SELECT 1` to Postgres on
   container `201`.

3. **(B) Session store on Postgres.** Bring up PostgreSQL on `llm-ctl-db` (201);
   apply the sessions/turns schema (PostgreSQL types) via sqlx migration; async
   writer task. Seed + read back a fake turn end-to-end (llm-ctl -> 201 -> back).

4. **(A) Full proxy.** Streaming + non-streaming pass-through, per-turn capture
   of `timings` + `usage`, model-must-be-active enforcement, single-client
   handling, `/v1/models`. Each turn persists to Postgres on `201`.

5. **(A) Supervisor lifecycle.** Single-active-model start/stop/switch (stop the
   current worker when loading another), health checks, `/metrics` + `/slots`
   polling.

6. **(A) HF download.** `uvx hf download` job with live log; auto-register to the
   model list. (Cache stays on the LLM tier, near the GPU.)

7. **(A) Rebuild.** git pull + cmake build with streaming log, SHA before/after,
   non-disruptive swap.

8. **(B) Public face + auth.** Stand up the TLS reverse proxy on `200` to expose
   the panel and `/v1/*` (llm-ctl stays bound to loopback locally). Replace the
   dev-only PVE token with a least-privilege credential; set a shared token for
   `/api/*` + `/v1/*`.

9. **(A) Web panel - DONE (406a34e).** Tailwind v4 + isotope theme; Models /
   HF / Rebuild / Sessions views wired to `/api/*` via `/api/status-rollup`;
   live updates by fetch polling (SSE dropped - see SS4.8). Sessions served
   from Postgres on `201`.

10. **(A/B) Observability + hardening.** Feed llama `/metrics` + PVE guest
    metrics into `llm-ctl-mon` (202); DB backups on the ops tier; graceful
    shutdown (SIGTERM worker on daemon exit), restart caps, env overrides.

### Infra as code / version control

The whole control plane is git-versioned at `/home/pixie/llm-ctl` (this design,
the config, the Rust daemon). The PVE ops tier is tracked the same way instead
of point-and-click, so VMIDs, tags, IPs and roles are reviewed and reproducible:

- `infra/` - **OpenTofu** (chosen; `bpg/proxmox` provider) declares the
  containers: vmid 200-202, names, tag `ai`, IPs `192.168.7.50+`, storage
  `nvme1`, base template. State is a workspace file (local, or a configured
  backend later). The token is injected from `PROXMOX_AGENT_ADMIN` (env) at
  apply time - **never committed**.
- `ansible/` - playbooks for *inside* the containers: base packages, Postgres
  install/config on `201`, reverse-proxy TLS on `200`, monitoring on `202`, and
  drift repair. Ansible gives idempotent day-2 config that Terraform does not.
- If full TF/Ansible feels heavy for three CTs, a minimal fallback is a
  versioned `scripts/prov_ct.sh` driving the PVE API directly - the definitions
  (vmids/names/ips/tags) stay in git either way.
- Secrets are referenced, never committed: read `~/.keys` / env at apply time.

The vmid/name/tag/IP scheme is the **single source of truth** shared by the Rust
config (`[proxmox.cts]`) and the IaC, so one change stays in lockstep.

---

## 8. Open questions (resolve during implementation)

- **Session binding**: keep the "active session within 30 min" heuristic as
  default, with explicit `X-Session-Id` header override? (Design SS4.2 assumes yes.)
- **Single-client strictness**: with `-np 1`, llama-server already defers extra
  requests in a queue. Do we accept that soft single-client (overlap queues) or
  hard-reject a second distinct client while one turn is in flight? Default:
  soft (queue), simplest and matches llama-server semantics. A client id / busy
  flag would be needed for hard mode.
- **Model-not-active request**: respond 404/409 with a hint (default), or
  auto-load the requested model from config? Default: error + hint; the UI is
  the intended way to load.
- **HF download granularity**: pick the wanted quant after listing the repo's
  GGUF files (nicer) vs. just `--include '*.gguf'` (simpler). Likely: list then
  pick.
- **Auth**: localhost-only bind makes auth low priority; add a shared token for
  `/api/*` + `/v1/*` if exposed beyond loopback. Out of scope until then.
- **PVE integration scope**: does `llm-ctl` manage the ops-tier containers
  (create/label/start/stop via the web panel), or is Proxmox purely an external
  placement decision with numbering/tagging done by hand? Draft: `llm-ctl`
  exposes a thin `/api/pve/*` read view (list containers + tags) and records the
  `200+` / `ai` convention; full create/start/stop is a follow-up.
- **PVE credential**: token `agent-admin` is **dev-only** and its papered usage
  is hardcoded in config. Before anything is reachable beyond this host, swap to
  a scoped least-privilege token/ACL and pull the secret from an env var or
  secret store, not `~/.keys`.
- Binary/project name: `llm-ctl` is the working title - rename freely.
