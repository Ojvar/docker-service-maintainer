#!/usr/bin/env bash
# Force-redeploys one Docker Swarm service. Used by POST /services/{name}/reload.
set -euo pipefail
docker service update --force --detach "$1"
