#!/usr/bin/env bash
# Adds the ssh host keys of the servers in servers.conf to known_hosts, so collect-server.sh
# (BatchMode=yes) can connect. Works with OpenSSH 7.4 (no accept-new needed).
# Usage: scan-host-keys.sh [--dry-run] [server id...]   (no ids = every server)
# - Keys already in the file are skipped (ssh-keygen -F, hashed entries included).
# - A host whose scanned key differs from a key of the same type already in the file is
#   reported as a MISMATCH and nothing is added for it (check it by hand: possible MITM).
# - The file is backed up (<file>.bak-<timestamp>) before anything is appended.
# - Prints the fingerprint of every new key; compare them with the hosts before trusting.
# Env: SERVERS_KNOWN_HOSTS (default ~/.ssh/known_hosts), SERVERS_INVENTORY (default servers.conf next to this script)
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
inventory="${SERVERS_INVENTORY:-$dir/servers.conf}"
known_hosts="${SERVERS_KNOWN_HOSTS:-$HOME/.ssh/known_hosts}"
known_hosts="${known_hosts/#\~/$HOME}"

dry_run=0
ids=()
for arg in "$@"; do
  case "$arg" in
    --dry-run|-n) dry_run=1 ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown option: $arg" >&2; exit 64 ;;
    *)
      if [[ ! "$arg" =~ ^[a-z0-9_-]{1,32}$ ]]; then echo "invalid server id: $arg" >&2; exit 64; fi
      ids+=("$arg") ;;
  esac
done
[[ -r "$inventory" ]] || { echo "inventory not found: $inventory" >&2; exit 66; }
command -v ssh-keyscan >/dev/null || { echo "ssh-keyscan not found" >&2; exit 69; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
new_keys="$tmp/new"
: > "$new_keys"
[[ -e "$known_hosts" ]] || known_hosts_missing=1

declare -A wanted=()
for id in "${ids[@]}"; do wanted[$id]=1; done
declare -A found=()
mismatches=0 failures=0 present=0 added=0

while read -r id host port _user _; do
  [[ -z "${id:-}" || "$id" == \#* ]] && continue
  if (( ${#ids[@]} )) && [[ -z "${wanted[$id]:-}" ]]; then continue; fi
  found[$id]=1
  if [[ ! "$host" =~ ^[A-Za-z0-9.-]{1,253}$ || "$host" == -* || ! "$port" =~ ^[0-9]{1,5}$ ]]; then
    echo "$id: invalid inventory entry, skipped" >&2; failures=$((failures + 1)); continue
  fi
  # known_hosts writes non-default ports as [host]:port.
  if [[ "$port" == 22 ]]; then name="$host"; else name="[$host]:$port"; fi

  scan="$tmp/scan-$id"
  if ! ssh-keyscan -T 5 -p "$port" "$host" 2>/dev/null | grep -v '^#' > "$scan" || [[ ! -s "$scan" ]]; then
    echo "$id ($name): no keys (unreachable or ssh-keyscan failed)" >&2
    failures=$((failures + 1)); continue
  fi

  existing=""
  [[ -z "${known_hosts_missing:-}" ]] && existing=$(ssh-keygen -F "$name" -f "$known_hosts" 2>/dev/null | grep -v '^#' || true)
  host_new="$tmp/hostnew-$id"
  : > "$host_new"
  host_bad=0
  while read -r _h type key; do
    [[ -n "${type:-}" && -n "${key:-}" ]] || continue
    if awk -v t="$type" -v k="$key" '$2==t && $3==k {f=1} END{exit !f}' <<<"$existing"; then
      present=$((present + 1))
    elif awk -v t="$type" '$2==t {f=1} END{exit !f}' <<<"$existing"; then
      echo "$id ($name): MISMATCH for $type, the host presents a different key than $known_hosts has" >&2
      host_bad=1
    else
      printf '%s %s %s\n' "$name" "$type" "$key" >> "$host_new"
    fi
  done < "$scan"
  if (( host_bad )); then mismatches=$((mismatches + 1)); continue; fi
  if [[ -s "$host_new" ]]; then
    while read -r line; do
      printf '%s\n' "$line" > "$tmp/one"
      echo "$id: new $(ssh-keygen -lf "$tmp/one" 2>/dev/null || echo "$line")"
    done < "$host_new"
    cat "$host_new" >> "$new_keys"
  else
    echo "$id ($name): all keys already present"
  fi
done < "$inventory"

for id in "${ids[@]}"; do
  [[ -n "${found[$id]:-}" ]] || { echo "unknown server id: $id" >&2; failures=$((failures + 1)); }
done

added=$(grep -c . "$new_keys" || true)
if (( added == 0 )); then
  echo "nothing to add ($present keys already present, $failures failed, $mismatches mismatched)"
elif (( dry_run )); then
  echo "dry run: would add $added keys to $known_hosts ($present already present, $failures failed, $mismatches mismatched)"
else
  mkdir -p "$(dirname "$known_hosts")"
  if [[ -e "$known_hosts" ]]; then
    backup="$known_hosts.bak-$(date +%Y%m%d-%H%M%S)"
    cp -p "$known_hosts" "$backup"
    echo "backup: $backup"
  fi
  cat "$new_keys" >> "$known_hosts"
  chmod 600 "$known_hosts" 2>/dev/null || true
  echo "added $added keys to $known_hosts ($present already present, $failures failed, $mismatches mismatched)"
fi
(( mismatches == 0 && failures == 0 ))
