#!/usr/bin/env bash
# pg_dump backup of the llm_ctl database on CT 201, run by the postgres user
# via /etc/cron.d/llm-ctl-backup (daily 02:17). Atomic (dump to temp then mv),
# logged, pruned after 14 days.
set -euo pipefail

BACKUP_DIR=/var/backups/llm_ctl
LOG=/var/log/llm-ctl-backup.log
STAMP=$(date +%F_%H%M%S)
TMP="${BACKUP_DIR}/.llm_ctl_${STAMP}.partial"
OUT="${BACKUP_DIR}/llm_ctl_${STAMP}.dump"

mkdir -p "$BACKUP_DIR"
chmod 0700 "$BACKUP_DIR"

if ! pg_isready -q; then
  echo "$(date +%FT%T) ERROR pg_isready failed" >> "$LOG"
  exit 1
fi

if pg_dump -Fc -d llm_ctl -f "$TMP"; then
  mv "$TMP" "$OUT"
  chmod 0600 "$OUT"
  echo "$(date +%FT%T) OK $OUT ($(du -h "$OUT" | cut -f1))" >> "$LOG"
else
  rm -f "$TMP"
  echo "$(date +%FT%T) ERROR pg_dump failed" >> "$LOG"
  exit 1
fi

# retention: keep 14 days
find "$BACKUP_DIR" -name 'llm_ctl_*.dump' -mtime +14 -delete