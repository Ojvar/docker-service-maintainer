#!/usr/bin/env bash
# Lists MongoDB backup files on the backup host. Used by GET /backups and the backups dashboard.
# Output: one file per line, newest first: <modified unix time>\t<size in bytes>\t<file name>
# Env: BACKUP_SSH_TARGET (default qeng@192.168.0.83), BACKUP_SSH_KEY (default ~/.ssh/id_qeng_2027), BACKUP_SSH_PORT (default 10022), BACKUP_REMOTE_DIR (default /home/qeng/mongo-backups)
set -euo pipefail

target="${BACKUP_SSH_TARGET:-qeng@192.168.0.83}"
key="${BACKUP_SSH_KEY:-$HOME/.ssh/id_qeng_2027}"
port="${BACKUP_SSH_PORT:-10022}"
dir="${BACKUP_REMOTE_DIR:-/home/qeng/mongo-backups}"

ssh -i "$key" -p "$port" -o IdentitiesOnly=yes -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new "$target" \
  "find $(printf '%q' "$dir") -maxdepth 1 -type f -printf '%T@\t%s\t%f\n'" | sort -rn
