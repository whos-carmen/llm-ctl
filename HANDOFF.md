# llm-ctl — Project Handoff

Status snapshot for picking up in a fresh agent session. Read `DESIGN.md` in
this repo for the full architecture; this file is the "where we are / what's
next" handoff. Last updated 2026-08-12 (M5 done; M6 = HF download job).

---

## 1. What this is

A single-purpose Rust control-plane + OpenAI-compatible proxy for llama.cpp on
one machine, with the "everything else" ops tier hosted on a Proxmox server.

- **LLM tier = this host** (`192.168.7.23`, workstation): `llm-ctl` daemon +
  llama.cpp (HIP) + models + the LFM2.5 worker. One model at a time, one client.
- **Ops tier = Proxmox node `px360`** (`192.168.4.111`): the three `ai`-tagged
  LXC containers (Postgres, reverse-proxy, monitoring).

Key decisions locked in DESIGN.md: **Rust** daemon, **own supervisor** (one
llama-server child), **Postgres-on-PVE** for the session store, **single model /
single client**, HF downloads, sole-owner ("kills other llama-server on
startup"), isotope (base16) dark theme + Tailwind for the future web UI.

## 2. Repository layout (`/home/pixie/llm-ctl`)

```
Cargo.toml            # NOT yet, see M3 #2
src/main.rs           # axum app; crate builds & runs (M2 verified)
src/{config,reap,supervisor,proxy,pve,db}.rs
src/store.rs          # SQL* Persistence stub - written, NOT wired to main yet
migrations/0001_init.sql  # sessions/turns (PG types) - written
config.toml           # LIVE config (gitignored): points at LFM2.5 worker
config.example.toml   # committed template (also LFM2.5 now)
DESIGN.md             # authoritative design
infra/                # OpenTofu: provision the ops-tier CTs
docs/CHANNEL-ISSUES.md
```

### Git history (head)
```
a182a4b  config.example: point default model at LFM2.5
cf2cebc  llm-ctl M2: sole-owner daemon, worker supervisor, /v1 proxy, PVE+DB plumbing
e62e6af  infra: keep nesting feature flags (unset requires root@pam)
4fff31c  infra: drop nesting feature [reverted e62e6af]
f2496dc  Initial design + OpenTofu ops-tier scaffold
```

Untracked / not committed yet next session: `docs/CHANNEL-ISSUES.md`,
`HANDOFF.md`. Committed so far: M2 (cf2cebc), M3 (7807395), M4 (1706aab),
M5 (next commit). Commits use `gh` identity `whos-carmen`
(`87442520+whos-carmen@users.noreply.github.com`).
Daemon runs detached (`nohup ./target/debug/llm-ctl > /tmp/llmctl.log 2>&1 &`).

## 3. Environment / tooling

- **Rust**: installed via rustup; `~/.cargo/bin` on PATH (rustc 1.97.1, cargo 1.97.1).
- **OpenTofu**: `~/.local/bin/tofu` (1.12.5), provider `bpg/proxmox v0.111.1`.
  `infra/run.sh` injects the PVE token secret from `~/.keys` (env `PROXMOX_AGENT_ADMIN`).
  Real vars in gitignored `infra/terraform.tfvars`.
- **llama.cpp**: `/home/pixie/llama`, fresh clone HEAD `0b1bad14f`, **ROCm/HIP
  build** (gfx1100). Binary `/home/pixie/llama/build/bin/llama-server`.
  Config: `cmake -S . -B build -DGGML_HIP=ON -DAMDGPU_TARGETS=gfx1100 -DCMAKE_BUILD_TYPE=Release`.
- **SSH**: `~/.ssh/id_ed25519` key injected into all three CTs (root access).
- **Secrets**: PVE token `PROXMOX_AGENT_ADMIN` in `~/.keys` (dev-only);
  HF token `~/.cache/huggingface/token`; Postgres password was generated at
  `/tmp/llm_pgpw.txt` but `/tmp` is not guaranteed to survive a reboot — treat
  it as lost and regenerate (see M3 #1).

## 4. Infra state (M1 — DONE)

Three LXC CTs on `nvme1`, Debian 13, tag **`ai`**, running, with root SSH:

| vmid | name         | ip            | storage/disk | note                     |
|------|--------------|---------------|--------------|--------------------------|
| 200  | llm-ctl-ops  | 192.168.7.50  | nvme1 / 20G  | reverse proxy / TLS      |
| 201  | llm-ctl-db   | 192.168.7.51  | nvme1 / 50G  | PostgreSQL (M3)          |
| 202  | llm-ctl-mon  | 192.168.7.52  | nvme1 / 20G  | monitoring (future)      |

Network `vmbr0` = `192.168.4.0/22`, gateway `192.168.4.1`. Existing non-project
CTs: `100/103`(system), `300/301`(games). Note: `features.nesting` is set on the
CTs; changing any feature flag requires `root@pam` (dev token can't) — keep the
`features { nesting = true }` block in `infra/main.tf` to avoid OpenTofu drift.

## 5. M2 — DONE (verified)

- Daemon `llm-ctl` on `127.0.0.1:8082`: `/`, `/api/health`, `/api/pve`,
  `/api/db` (TCP probe), streaming proxy fallback for `/v1/*`, `/models`,
  `/health`, `/props`, `/metrics`, `/slots`.
- Sole-owner `reap` (kills external llama-server at startup), supervisor spawns
  the single worker (HIP env, log -> `/tmp/llmctl-worker.log`).
- Verified end-to-end: `/v1/chat/completions` through the proxy returned
  LFM2.5 output; prompt ~55 t/s, decode ~243 t/s. PVE cross-tier reachable.
- **Known gap**: worker status stays `Starting` (no health poll to flip `Ready`
  yet) — that is part of **M5**.

## 6. M3 — IN PROGRESS (pick up here)

Goal: Postgres on `201` + `sqlx` session store; `/api/db` should flip
`postgres_reachable: true`.

**Done so far:**
- Postgres **17 installed AND configured** on `201`: `listen_addresses='*'`,
  `pg_hba` allows `llm_ctl` from `192.168.7.23/32` (scram), role `llm_ctl` +
  database `llm_ctl` created. **`192.168.7.51:5432` is reachable (`PG_OPEN`).**
- Postgres password set in the gitignored `config.toml` `pgurl` (was `REDACTED`).
  The generated password is also at `/tmp/llm_pgpw.txt` (regenerate if /tmp cleared).
- `src/store.rs` (`Store{Pool}`, `connect/migrate/ping/record_fake_turn/count_turns`)
  and `migrations/0001_init.sql` **written** (PostgreSQL types).

**Still to do (blocks completion):**
1. **`sqlx` in `Cargo.toml` — VERIFY/redo.** An Edit adding
   `sqlx = { version="0.8", default-features=false, features=["runtime-tokio","postgres","migrate"] }`
   was issued but unverified; check with `grep -c '^sqlx' Cargo.toml` and
   re-add if it returns 0.
2. **Wire `main.rs`**: add `mod store;`, route
   `.route("/api/session-test", get(handle_session_test))`, replace `handle_db`
   with a real connect+migrate+`SELECT 1`; add `handle_session_test` calling
   `store.record_fake_turn("m3-test-session","lfm2.5-8b")` (expect `{"turns":1}`).
3. `cargo build`, run daemon, verify `/api/db` -> `true` and `/api/session-test`.
4. **Commit M3** (also commit this HANDOFF.md + docs/CHANNEL-ISSUES.md).

**Daemon run gotcha:** a `run_in_background` bash that execs a long-running
server hits the default timeout and gets killed. Relaunch the daemon with
`nohup`/`setsid` (or `systemd`) rather than a plain background task:
```bash
cd ~/llm-ctl && nohup ./target/debug/llm-ctl > /tmp/llmctl.log 2>&1 &
```

## 7. Milestones still ahead (DESIGN.md §7)

- **M4 — DONE (1706aab)**: full proxy per-turn capture (streaming + non-streaming)
  into Postgres, model-must-be-active (409), `/api/sessions` rollups incl. cache %.
- **M5 — DONE**: supervisor lifecycle — health poll `Starting -> Ready` (or
  Crashed), stop/switch via `POST /api/models/:id/start` + `/api/models/stop`,
  `/slots` + `/metrics` polled (2s) and exposed in `/api/health`.
- **M6** HF download job (`uvx hf download`), live log, auto-register; cache local.
- **M7** rebuild job (git pull + cmake build) with streaming log + non-disruptive swap.
- **M8** reverse-proxy/TLS public face on CT `200`; replace dev-only token,
  add shared API token.
- **M9** web panel (Tailwind v4 + isotope theme) wired to `/api/*`, SSE.
- **M10** observability into `202` + DB backups + hardening.

Open questions live at DESIGN.md §8 (single-client strictness, auto-load vs
404, HF quant-picking, PVE container mgmt scope, dev-token hygiene).

## 8. Gotchas / notes for the next agent

- **Channel issue:** see `docs/CHANNEL-ISSUES.md` — tool results intermittently
  mangle/drop. Mitigation: keep commands tiny and single-purpose; verify with
  minimal `test`/`grep -c`; re-run lost ops; a `Write` may succeed despite a
  mangled confirmation (confirm with a `Read`/`test`).
- **Model:** only LFM2.5 exists now (`~/.cache/huggingface/hub/models--unsloth--
  LFM2.5-8B-A1B-GGUF/snapshots/37563684…/LFM2.5-8B-A1B-UD-Q5_K_XL.gguf`). Old
  Nemotron/Ling/gemma GGUF files were deleted with the old `llama/` tree.
- **llama.cpp AGENTS.md:** do NOT commit/push/PR to llama.cpp; do not submit
  upstream changes. Fresh copies are fine.
- **Sole-owner daemon:** llm-ctl kills any external llama-server on startup;
  only run it when you intend it to own the worker.
- **dev token:** PVE token `agent-admin` is dev-only; rotate to least-privilege
  before anything leaves localhost.
