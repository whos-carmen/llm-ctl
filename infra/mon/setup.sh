#!/usr/bin/env bash
# Idempotent provisioner for llm-ctl-mon (CT 202, 192.168.7.52).
# Run on the CT as root after staging the sibling files to /root:
#   PROXMOX_AGENT_ADMIN=<admin-token> bash /root/setup.sh
# PROXMOX_AGENT_ADMIN is the workstation's dev token, needed only on the first
# run to bootstrap the read-only `pve-mon@pam` token (PVEAuditor). The admin
# token never lands on this box; only the returned pve-mon secret is written to
# /etc/prometheus/pve.yml (0600).
set -euo pipefail

PVE_HOST=192.168.4.111
PVE_API="https://${PVE_HOST}:8006/api2/json"
PROM_DIR=/etc/prometheus
PVE_YML=$PROM_DIR/pve.yml

echo "==> packages"
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  prometheus prometheus-node-exporter python3-venv python3-pip curl

echo "==> PVE reachability"
# Any HTTP response (including the expected 401 for unauthenticated /version)
# proves TCP+TLS to the PVE API works. Only connection-level errors fail.
if curl -ks -o /dev/null -m 5 "${PVE_API}/version"; then
  echo "    PVE reachable"
else
  echo "    PVE not reachable from this CT" >&2; exit 1
fi

echo "==> bootstrap read-only pve-mon token (first run only)"
if [[ -f "$PVE_YML" ]] && grep -q 'token_value' "$PVE_YML" \
      && ! grep -q 'token_value: REPLACED' "$PVE_YML"; then
  echo "    pve.yml already provisioned; skipping bootstrap"
else
  [[ -n "${PROXMOX_AGENT_ADMIN:-}" ]] || { echo "    PROXMOX_AGENT_ADMIN unset" >&2; exit 1; }
  AUTH="Authorization: PVEAPIToken=agent-admin@pam!agent-admin=${PROXMOX_AGENT_ADMIN}"
  USER_ID=pve-mon@pve   # 'pve' realm: created in PVE's own DB (PAM users can't
                        # be created with a password via the API)
  TKN=monitor
  # ensure user exists (ignore "already exists")
  curl -ks -H "$AUTH" -d "userid=${USER_ID}&password=unused-not-for-login&enable=1" \
       "${PVE_API}/access/users" >/dev/null || true
  # recreate token idempotently; tokenid goes in the URL path, not a form field.
  # Global token (privsep=0): pve-mon@pve has only the read-only PVEAuditor
  # role, and a privsep token (privsep=1) additionally 403s on /cluster/status
  # that the default pve-exporter collectors need.
  curl -ks -H "$AUTH" -X DELETE \
       "${PVE_API}/access/users/${USER_ID}/token/${TKN}" >/dev/null || true
  RESP=$(curl -ks -H "$AUTH" -X POST -d "privsep=0" \
              "${PVE_API}/access/users/${USER_ID}/token/${TKN}")
  VALUE=$(printf '%s' "$RESP" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["data"]["value"])')
  # grant PVEAuditor on / (guest/node/datastore audit only); /access/acl uses PUT
  curl -ks -H "$AUTH" -X PUT -d "path=/&roles=PVEAuditor&users=${USER_ID}" \
       "${PVE_API}/access/acl" >/dev/null || true
  install -d -m 0755 "$PROM_DIR"   # keep dir 0755 so the prometheus user (not root) can read it
  sed "s/REPLACED_BY_SETUP_SH/${VALUE}/" /root/pve.yml.example > "$PVE_YML"
  chmod 0600 "$PVE_YML"            # only the token file is root-only
  echo "    pve-mon token provisioned"
fi

echo "==> prometheus.yml"
install -m 0644 /root/prometheus.yml "$PROM_DIR/prometheus.yml"

echo "==> pve-exporter (venv + systemd unit, 127.0.0.1:9221)"
if [[ ! -x /opt/pve-exporter/bin/pve_exporter ]]; then
  python3 -m venv /opt/pve-exporter
  /opt/pve-exporter/bin/pip install --quiet prometheus-pve-exporter
fi
cat > /etc/systemd/system/pve-exporter.service <<'UNIT'
[Unit]
Description=Proxmox VE Prometheus exporter
After=network.target

[Service]
# Global (privsep=0) read-only pve-mon@pve token lets the default collector set
# read PVE (node/resources/version/etc) with no 403s.
ExecStart=/opt/pve-exporter/bin/pve_exporter --web.listen-address 127.0.0.1:9221 --config.file /etc/prometheus/pve.yml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now pve-exporter

echo "==> prometheus retention (7d) + services"
echo 'ARGS="--storage.tsdb.retention.time=7d"' > /etc/default/prometheus
systemctl enable --now prometheus prometheus-node-exporter
systemctl restart prometheus pve-exporter

echo "==> done. UI: http://192.168.7.52:9090 ; targets: 127.0.0.1:9090/api/v1/targets"