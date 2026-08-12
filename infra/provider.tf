# Proxmox VE provider (bpg). Auth via the agent-admin token.
# The SECRET half is injected by run.sh from ~/.keys (PROXMOX_AGENT_ADMIN),
# never committed here.
provider "proxmox" {
  endpoint  = var.pve_endpoint     # e.g. "https://192.168.4.111:8006/"
  username  = var.pve_user         # e.g. "agent-admin@pam"
  api_token = var.pve_api_token    # the token secret (UUID); dev-only
  insecure  = true                 # PVE self-signed cert
}
