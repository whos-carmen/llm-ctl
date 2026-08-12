# llm-ctl infra - ops tier on Proxmox (OpenTofu)

Declarative definition of the "everything else" containers (LLM stays on the
workstation). All containers are tagged `ai` and number from vmid 200.

| vmid | name         | ip            | role |
| ---- | ------------ | ------------- | ---- |
| 200  | llm-ctl-ops  | 192.168.7.50  | reverse proxy / TLS |
| 201  | llm-ctl-db   | 192.168.7.51  | PostgreSQL (session store) |
| 202  | llm-ctl-mon  | 192.168.7.52  | monitoring / logs |

## Prereqs

- [OpenTofu](https://opentofu.org) >= 1.7 (not installed on this host yet)
- `~/.keys` containing `PROXMOX_AGENT_ADMIN=<uuid>` (the dev token secret)
- An SSH pubkey (default `~/.ssh/id_ed25519.pub`)

## Usage

```sh
cp terraform.tfvars.example terraform.tfvars   # fill ssh key / sizes if needed
./run.sh init                                   # pulls bpg/proxmox provider
./run.sh validate                               # confirms the provider schema
./run.sh plan                                   # diff against live PVE (dry run)
./run.sh apply                                  # create/update the containers
./run.sh destroy                                # remove them
```

`run.sh` injects `TF_VAR_pve_api_token` from `~/.keys` (never committed);
`terraform.tfvars` holds non-secret settings and is gitignored.

## Notes

- `main.tf` field names are the `bpg/proxmox` provider's contract; the exact
  layout was written by hand and should be confirmed with `tofu validate`
  against the pinned provider version before the first `apply`.
- Base is the updated Debian 13 template
  `local:vztmpl/debian-13-standard_13.6-1_amd64.tar.zst` (pulled fresh); swap
  ubuntu-26.04 or alpine-3.24 by changing `template_file_id` + the
  `operating_system.type`.
- The vmid/name/ip/tag mapping is the single source of truth: keep it in sync
  with `../DESIGN.md` and the Rust daemon's `[proxmox.cts]`.
