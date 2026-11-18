#!/usr/bin/env bash
# Collects system information from one server of servers.conf. Used by POST /servers/refresh,
# POST /servers/{id}/refresh and the Servers dashboard.
# Usage: collect-server.sh <server id>                     full read-only probe (remote-probe.sh)
#        collect-server.sh <server id> container <name>    docker inspect summary (remote-detail.sh)
#        collect-server.sh <server id> unit <name>.service  systemd status + recent journal (remote-detail.sh)
# Runs remote-probe.sh on the host over ssh (read-only) and prints one JSON object:
#   {"id":"server-1","reachable":true,"duration_ms":4210,"data":{...probe output...}}
#   {"id":"server-5","reachable":false,"duration_ms":8012,"exit_code":255,"error":"ssh: ... Connection timed out"}
# The script exits 0 whenever it printed that JSON; 64 for a bad id, 66 for a missing inventory/key.
# Env: SERVERS_SSH_KEY (default ~/.ssh/id_qeng_2027), SERVERS_SSH_CONNECT_TIMEOUT (seconds, default 8),
#      SERVERS_COLLECT_TIMEOUT (seconds for the whole collection, default 60),
#      SERVERS_INVENTORY (default servers.conf next to this script),
#      SERVERS_KNOWN_HOSTS (optional known_hosts file; default: ssh's own, ~/.ssh/known_hosts)
#      Add missing host keys with scan-host-keys.sh.
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
inventory="${SERVERS_INVENTORY:-$dir/servers.conf}"
key="${SERVERS_SSH_KEY:-$HOME/.ssh/id_qeng_2027}"
key="${key/#\~/$HOME}"
connect_timeout="${SERVERS_SSH_CONNECT_TIMEOUT:-8}"
collect_timeout="${SERVERS_COLLECT_TIMEOUT:-60}"
known_hosts_opt=()
if [[ -n "${SERVERS_KNOWN_HOSTS:-}" ]]; then
  known_hosts_opt=(-o "UserKnownHostsFile=${SERVERS_KNOWN_HOSTS/#\~/$HOME}")
fi

if [[ $# -ne 1 && $# -ne 3 ]] || [[ ! "$1" =~ ^[a-z0-9_-]{1,32}$ ]]; then
  echo "usage: $0 <server id> [container <name> | unit <name>.service]" >&2
  exit 64
fi
id="$1"
remote_script="$dir/remote-probe.sh"
remote_cmd="bash -s"
if [[ $# -eq 3 ]]; then
  case "$2" in
    container) [[ "$3" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,254}$ ]] || { echo "invalid container name" >&2; exit 64; } ;;
    unit) [[ "$3" =~ ^[A-Za-z0-9@_.:-]{1,200}\.service$ ]] || { echo "invalid unit name" >&2; exit 64; } ;;
    *) echo "unknown detail kind: $2" >&2; exit 64 ;;
  esac
  remote_script="$dir/remote-detail.sh"
  remote_cmd="bash -s -- $(printf '%q %q' "$2" "$3")"
  collect_timeout="${SERVERS_DETAIL_TIMEOUT:-30}"
fi
[[ "$connect_timeout" =~ ^[0-9]{1,3}$ ]] || connect_timeout=8
[[ "$collect_timeout" =~ ^[0-9]{1,4}$ ]] || collect_timeout=60
[[ -r "$inventory" ]] || { echo "inventory not found: $inventory" >&2; exit 66; }

# JSON string with exact escaping.
js() {
  local s=$1
  s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\t'/ }; s=${s//$'\r'/}; s=${s//$'\n'/\\n}
  printf '"%s"' "$s"
}
now_ms() { date +%s%3N; }

host="" port="" user=""
while read -r f_id f_host f_port f_user _; do
  [[ -z "${f_id:-}" || "$f_id" == \#* ]] && continue
  if [[ "$f_id" == "$id" ]]; then host="$f_host" port="$f_port" user="$f_user"; break; fi
done < "$inventory"

if [[ -z "$host" ]]; then
  echo "unknown server id: $id" >&2
  exit 64
fi
if [[ ! "$host" =~ ^[A-Za-z0-9.-]{1,253}$ || ! "$port" =~ ^[0-9]{1,5}$ || ! "$user" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]]; then
  printf '{"id":%s,"reachable":false,"duration_ms":0,"exit_code":null,"error":"invalid inventory entry"}\n' "$(js "$id")"
  exit 0
fi
[[ -r "$key" ]] || { echo "ssh key not readable (set SERVERS_SSH_KEY)" >&2; exit 66; }

started=$(now_ms)
err_file=$(mktemp)
trap 'rm -f "$err_file"' EXIT
set +e
out=$(timeout --kill-after=5 "$collect_timeout" ssh -T -i "$key" -p "$port" \
  -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR -o ConnectTimeout="$connect_timeout" \
  -o ServerAliveInterval=5 -o ServerAliveCountMax=2 -o ForwardAgent=no -o ForwardX11=no \
  -o RequestTTY=no -o ClearAllForwardings=yes ${known_hosts_opt[@]+"${known_hosts_opt[@]}"} \
  "$user@$host" "$remote_cmd" < "$remote_script" 2>"$err_file")
rc=$?
set -e
duration=$(( $(now_ms) - started ))

if [[ $rc -eq 0 && "$out" == \{*\} ]]; then
  printf '{"id":%s,"reachable":true,"duration_ms":%s,"data":%s}\n' "$(js "$id")" "$duration" "$out"
  exit 0
fi

# Last lines of ssh's stderr only (no key paths or contents are ever printed by ssh here).
# `|| true`: no stderr at all (e.g. a host that never answers) must still give the JSON below.
err=$( { grep -v '^\s*$' "$err_file" || true; } | tail -n 3 | tr '\n' ' ' | cut -c1-400)
case $rc in
  124|137) err="timed out after ${collect_timeout}s${err:+: $err}" ;;
  0) err="invalid output from remote probe${err:+: $err}" ;;
esac
[[ -n "$err" ]] || err="ssh exited with code $rc"
printf '{"id":%s,"reachable":false,"duration_ms":%s,"exit_code":%s,"error":%s}\n' "$(js "$id")" "$duration" "$rc" "$(js "$err")"
