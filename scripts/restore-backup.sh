#!/usr/bin/env bash
# Restores one MongoDB backup on the backup host. Used by POST /backups/restore.
# Usage: restore-backup.sh <restore_db_name> <backup_file_name>
# Runs on the host: restore-db.sh <restore_db_name> <BACKUP_REMOTE_DIR>/<backup_file_name>
# Env: BACKUP_SSH_TARGET (default qeng@192.168.0.83), BACKUP_REMOTE_DIR (default /home/qeng/mongo-backups),
#      BACKUP_RESTORE_SCRIPT (default /home/qeng/restore-db.sh)
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <restore_db_name> <backup_file_name>" >&2
  exit 64
fi

db="$1"
backup="$2"
if [[ ! "$db" =~ ^[A-Za-z0-9_-]{1,64}$ ]]; then
  echo "invalid database name: $db" >&2
  exit 64
fi
if [[ ! "$backup" =~ ^[A-Za-z0-9_-][A-Za-z0-9._-]*$ ]]; then
  echo "invalid backup file name: $backup" >&2
  exit 64
fi

target="${BACKUP_SSH_TARGET:-qeng@192.168.0.83}"
dir="${BACKUP_REMOTE_DIR:-/home/qeng/mongo-backups}"
restore="${BACKUP_RESTORE_SCRIPT:-/home/qeng/restore-db.sh}"

echo "restoring $backup into $db on $target"
ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new "$target" \
  "$(printf '%q %q %q' "$restore" "$db" "$dir/$backup")"
