#!/usr/bin/env bash
# Posts one Servers alert (JSON object in $1) to SERVERS_ALERT_WEBHOOK_URL with curl.
# Called by the API only when SERVERS_ALERT_WEBHOOK_URL is set; otherwise no outgoing calls are made.
set -euo pipefail
url="${SERVERS_ALERT_WEBHOOK_URL:-}"
if [[ -z "$url" ]]; then echo "SERVERS_ALERT_WEBHOOK_URL is not set" >&2; exit 64; fi
if [[ ! "$url" =~ ^https?://[^[:space:]]+$ ]]; then echo "invalid webhook url" >&2; exit 64; fi
if [[ $# -ne 1 ]]; then echo "usage: $0 <json>" >&2; exit 64; fi
printf '%s' "$1" | curl -fsS -m 10 -X POST -H 'content-type: application/json' --data-binary @- -o /dev/null "$url"
