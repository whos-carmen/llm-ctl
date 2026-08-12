#!/usr/bin/env bash
# Wrap `tofu` with the PVE token secret injected from ~/.keys so it is never
# committed. Usage: ./run.sh init | plan | apply | destroy | validate
set -euo pipefail
cd "$(dirname "$0")"

SECRET="$(awk -F= '/^PROXMOX_AGENT_ADMIN=/{print $2}' "$HOME/.keys" 2>/dev/null || true)"
if [[ -z "$SECRET" ]]; then
  echo "error: PROXMOX_AGENT_ADMIN not found in \$HOME/.keys" >&2
  exit 1
fi
export TF_VAR_pve_api_token="$SECRET"
export TF_VAR_pve_user="agent-admin@pam"

exec tofu "$@"
