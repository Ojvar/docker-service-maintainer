#!/usr/bin/env bash
# Collects every server of servers.conf in parallel and prints a JSON array of
# collect-server.sh results. For the command line; the API collects servers itself.
# Usage: collect-all.sh
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
inventory="${SERVERS_INVENTORY:-$dir/servers.conf}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

ids=()
while read -r id _; do
  [[ -z "${id:-}" || "$id" == \#* ]] && continue
  ids+=("$id")
  bash "$dir/collect-server.sh" "$id" > "$tmp/$id.json" 2>"$tmp/$id.err" &
done < "$inventory"
wait || true

printf '['
first=1
for id in "${ids[@]}"; do
  [[ $first -eq 1 ]] || printf ','
  first=0
  if [[ -s "$tmp/$id.json" ]]; then
    tr -d '\n' < "$tmp/$id.json"
  else
    printf '{"id":"%s","reachable":false,"error":"collector failed"}' "$id"
  fi
done
printf ']\n'
