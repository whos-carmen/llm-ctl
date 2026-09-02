# llm-ctl

A single Rust binary that is the **sole manager** of `llama-server` on a machine:
an OpenAI-compatible proxy, model lifecycle supervisor, HF downloader, llama.cpp
re-builder, and a dark-themed web panel — all in one daemon.

Serves one model at a time to a single client, with session tracking,
GPU/CPU monitoring, and a Proxmox-hosted ops tier (PostgreSQL, reverse proxy,
Prometheus).

---

## Architecture

```
┌─ LLM host (workstation) ──────────────────────────────────────────────┐
│                                                                        │
│   llm-ctl  (one Rust binary, port :8082)                               │
│   ┌──────────────┬──────────────────┬──────────────┐                   │
│   │  Proxy face  │  Control API     │  Web panel   │                   │
│   │  /v1/*       │  /api/*          │  /           │                   │
│   │  (OpenAI)    │  (JSON)          │  (Tailwind)  │                   │
│   └──────┬───────┴──────┬───────────┴──────┬───────┘                   │
│          │              │                  │                            │
│   ┌──────▼──────┐ ┌────▼─────┐   ┌────────▼────────┐                  │
│   │ supervisor  │ │ store.rs │   │ download.rs     │                  │
│   │ (lifecycle) │ │ (Postges)│   │ rebuild.rs      │                  │
│   │ reap.rs     │ │ CT 201   │   │ (HF + build)    │                  │
│   └──────┬──────┘ └──────────┘   └─────────────────┘                  │
│          │                                                             │
│   ┌──────▼─────────────────────────────────────────┐                   │
│   │  llama-server (loopback :8080, one model/slot)  │                   │
│   └────────────────────────────────────────────────┘                   │
└────────────────────────────────────────────────────────────────────────┘

┌─ Ops tier (Proxmox node px360) ─────────────────────────────────────┐
│  CT 200: nginx reverse proxy  (192.168.7.50)                        │
│  CT 201: PostgreSQL            (192.168.7.51) — session store       │
│  CT 202: Prometheus + exporters (192.168.7.52) — monitoring         │
└──────────────────────────────────────────────────────────────────────┘
```

### Key design decisions

- **One model at a time** — no multi-model routing. Load a model, serve it,
  switch when needed. No GPU sharing.
- **Single client** — hard-reject (409) a second completion while one is in
  flight. The RAII `InFlight` guard ensures the busy flag always clears.
- **Sole ownership** — on startup, `reap.rs` kills any `llama-server` not
  spawned by this daemon (matched by `/proc/<pid>/comm`).
- **Sessions by explicit id** — bound via `X-Session-Id` header only, or
  via the pi extension's `/api/session-active` endpoint. No implicit
  30-minute heuristic, no body-`user` fallback.
- **Ops tier on Proxmox** — the LLM host runs inference; everything else
  (Postgres, nginx, Prometheus) lives in LXC containers on a separate node.
- **LAN-only, no auth** — plain HTTP, private network. The nginx face
  is the only public entry. `/api` POSTs require `X-LLM-CTL: 1` (CSRF guard).

---

## Features

### Proxy (`/v1/*`)
- OpenAI-compatible chat completions endpoint
- Streaming (SSE) and non-streaming pass-through
- Model-must-be-active enforcement
- Per-turn capture into PostgreSQL: tokens, timings, cache hit rate,
  session rollups

### Supervisor
- Spawn/stop/switch the single `llama-server` worker
- Health polling (`Starting` → `Ready` / `Crashed`)
- Auto-restart on crash with configurable cap and backoff (5s cooldown)
- `/slots` + `/metrics` polling (2s interval)
- Graceful shutdown on SIGTERM/SIGINT (drain → stop worker → exit 0)

### HuggingFace downloads
- Browse a repo's GGUF files via HF tree API (sorted by size)
- Download a specific file or pattern via `uvx hf download`
- Auto-register downloaded models into the runtime list (persisted to config)
- Live log streamed to the web panel
- 30-minute watchdog kills hung downloads

### Rebuild
- `git pull --ff-only` + `cmake --build` of llama.cpp
- Non-disruptive — the running worker keeps serving
- Persistent build history (SHA before/after, timestamps) to disk
- 30-minute watchdog on each step

### Web panel
- Tailwind v4 with a dark base16-isotope theme
- Live monitor: CPU%, GPU%, VRAM usage, GPU temperature
- Model cards: status dot, active model metrics, start/stop controls
- HF GGUF picker with repo browser and download jobs
- Rebuild trigger with SHA history and live log
- Sessions table with cache hit %, token counts, timing
- Fetch-polling (~2.5s) of `/api/status-rollup`

### Session store (PostgreSQL on CT 201)
- Sessions table with rollups (requests, tokens, cache hit %)
- Turns table with per-request timings, cache stats, full telemetry
- Atomic `pg_advisory_xact_lock` serialization for turn_index
- Migrations via `sqlx`

### Monitoring (CT 202)
- Prometheus scraping: llama `/metrics` via nginx, host node metrics,
  PVE guest metrics via `pve-mon@pve` token
- Prometheus UI on `:9090`, 7d retention
- DB backups: daily `pg_dump -Fc` at 02:17, 14-day retention, verifyable

### Host stats (no GPU or infra dependency)
- CPU utilization from `/proc/stat` deltas
- AMD GPU stats from `amdgpu` sysfs (gpu_busy_percent, VRAM, temp)
- PVE and Postgres reachability probes

---

## Quick start

### Prerequisites

- Rust 1.97+ (`rustup`)
- A llama.cpp checkout with a ROCm/HIP or CUDA build
- HuggingFace auth (`uvx hf auth` or token in `~/.cache/huggingface/token`)

### Configuration

```sh
cp config.example.toml config.toml
# edit config.toml: adjust paths, model, worker args
```

The config is structured:
- `[listen]` — daemon bind address
- `[llama]` — repo path, build dir, binary location
- `[worker]` — port, llama-server args, GPU env, restart policy
- `[hf]` — CLI tool, cache path, auto-registration
- `[proxmox]` — PVE API, token, container definitions
- `[db]` — Postgres connection URL
- `[[models]]` — candidate model entries (one active at a time)

### Build and run

```sh
cargo build --release
./target/release/llm-ctl
```

The daemon binds to the configured address (default `0.0.0.0:8082`), kills any
existing `llama-server`, loads the `autostart` model (if configured), and
starts serving the proxy, API, and web panel.

```sh
# Override config path
LLM_CTL_CONFIG=/path/to/config.toml ./target/release/llm-ctl
```

### Run with nohup

```sh
cd ~/llm-ctl && nohup ./target/release/llm-ctl > /tmp/llmctl.log 2>&1 &
```

---

## API overview

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/` | Web panel |
| GET | `/health` | Daemon + worker health |
| GET | `/api/status-rollup` | Aggregated panel payload |
| GET | `/api/models` | List models with status |
| POST | `/api/models/:id/start` | Start/switch to a model |
| POST | `/api/models/stop` | Stop the active model |
| POST | `/api/hf/download` | Start HF download |
| GET | `/api/hf/download` | Download job status |
| GET | `/api/hf/files?repo=` | List GGUF files in a repo |
| POST | `/api/rebuild` | Start rebuild job |
| GET | `/api/rebuild` | Rebuild job status |
| GET | `/api/sessions` | Recent sessions with rollups |
| GET | `/api/pve` | PVE container list |
| GET | `/api/db` | Postgres connectivity probe |
| POST | `/api/session-active` | Set active session hint |
| POST | `/v1/chat/completions` | OpenAI-compatible proxy |

All `/api` POST endpoints require the `X-LLM-CTL: 1` header (CSRF guard).

---

## Project layout

```
├── Cargo.toml              # Rust project (axum, sqlx, tokio, reqwest)
├── config.example.toml     # Example configuration
├── DESIGN.md               # Full architecture and design decisions
├── HANDOFF.md              # Agent handoff notes (internal)
├── README.md               # This file
├── src/
│   ├── main.rs             # Axum app, routes, graceful shutdown
│   ├── config.rs           # TOML config deserialization
│   ├── proxy.rs            # OpenAI-compatible reverse proxy
│   ├── supervisor.rs       # Worker lifecycle (spawn/stop/restart)
│   ├── store.rs            # PostgreSQL session/turn store
│   ├── download.rs         # HuggingFace download job manager
│   ├── rebuild.rs          # llama.cpp rebuild job manager
│   ├── reap.rs             # Sole-ownership: kill external llama-server
│   ├── collect.rs          # Capped line collector for job logs
│   ├── stats.rs            # Host CPU/GPU metrics
│   ├── pve.rs              # Proxmox VE API client
│   └── db.rs               # Postgres TCP probe
├── web/
│   ├── index.html          # Web panel (Tailwind v4 + isotope theme)
│   └── assets/
│       ├── app.css         # Compiled Tailwind stylesheet
│       └── app.js          # Panel logic (fetch-polling, job display)
├── migrations/
│   ├── 0001_init.sql       # Sessions + turns schema
│   └── 0002_turn_index_unique.sql  # Unique session/turn constraint
├── infra/
│   ├── main.tf             # OpenTofu: PVE container definitions
│   ├── outputs.tf
│   ├── provider.tf
│   ├── variables.tf
│   ├── versions.tf
│   ├── terraform.tfvars.example
│   ├── run.sh              # Inject PROXMOX_AGENT_ADMIN from ~/.keys
│   ├── README.md           # Infra-specific docs
│   ├── firewall-llmctl.nft # nftables rules (only nginx can reach :8082)
│   ├── nginx/llm-ctl-ops.conf
│   ├── mon/                # Prometheus, exporters, setup
│   └── db/backup.sh        # pg_dump + 14-day retention
├── pi-ext/
│   ├── llama-stats.ts      # pi extension: cache%/ctx% widget + session relay
│   └── pi-bar-config.toml  # pi-bar isotope theme config
└── docs/
    ├── CHANNEL-ISSUES.md   # Tool-channel behavior notes
    ├── qwen-review-2026-08-12.md
    ├── qwen-full-review-2026-08-12.md
    ├── qwen-review-max-2026-08-12.md
    └── qwen-m9-consult-2026-08-12.md
```

---

## Ops tier (Proxmox)

The ops tier is provisioned with OpenTofu (`infra/`) and consists of three
LXC containers on an Ubuntu/Debian Proxmox node:

| VMID | Name | IP | Role |
|------|------|----|------|
| 200 | llm-ctl-ops | 192.168.7.50 | nginx reverse proxy / public face |
| 201 | llm-ctl-db  | 192.168.7.51 | PostgreSQL (session store) |
| 202 | lm-ctl-mon   | 192.168.7.52 | Prometheus + exporters |

See `infra/README.md` for provisioning instructions.

### Security posture

- **LAN-only, no auth** — the panel and API are accessible from any device on
  the private network. The workstation's `:8082` is firewalled to only the
  nginx container (`infra/firewall-llmctl.nft`). All `/api` POSTs need the
  `X-LLM-CTL: 1` header (CSRF guard).
- **CSP headers** — `default-src 'self'`, script and style constrained.
  `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`.
- **PVE TLS pinned** — the Proxmox API certificate is pinned to a file;
  the daemon fails closed (no `danger_accept_invalid_certs`).
- **Dev token** — the PVE API token lives in `~/.keys` with `chmod 600`.
  The daemon warns if the file is group/other readable.

---

## Client integration

The repo includes a **pi extension** (`pi-ext/llama-stats.ts`) that:

- Publishes cache% and ctx% to the pi-bar status widget
- Reports pi's real session id via `POST /api/session-active` so turns are
  keyed per-pi-session instead of a heuristic

Configure the pi provider:

```json
{
  "provider": "llamacpp",
  "baseUrl": "http://localhost:8082/v1",
  "model": "lfm2.5-8b"
}
```

---

## Dependencies

- **Rust crates**: axum, tokio, sqlx (Postgres), reqwest, tower-http,
  serde/serde_json, toml, tracing, futures-util, rand, anyhow
- **External**: llama.cpp (ROCm/HIP or CUDA build), PostgreSQL, OpenTofu
  (for infra provisioning), `uvx` + `hf` (for downloads)
- **Front-end**: Tailwind v4 (`@tailwindcss/cli`), isotope dark theme

---

## License

MIT