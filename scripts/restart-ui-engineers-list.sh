#!/usr/bin/env bash
# Restarts the engineers-list (ui) stack: removes it, waits until it is gone, deploys it again.
set -euo pipefail

DEPLOY_DIR="/opt/docker-deployer/qeng/ui/engineers-list"
STACK="q-ui-engineers-list"

cd "$DEPLOY_DIR"
echo "Removing stack $STACK"
bash ./deploy.sh remove

# docker stack rm returns before services and networks are gone; wait up to 60s.
for _ in $(seq 1 30); do
  [ -z "$(docker stack ls --format '{{.Name}}' | grep -x "$STACK" || true)" ] && \
  [ -z "$(docker network ls -q --filter label=com.docker.stack.namespace=$STACK)" ] && break
  sleep 2
done

echo "Deploying stack $STACK"
bash ./deploy.sh
