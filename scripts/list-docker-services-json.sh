#!/usr/bin/env bash
# One JSON object per line, used by GET /services and the dashboard.
set -euo pipefail
docker service ls --format '{{json .}}'
