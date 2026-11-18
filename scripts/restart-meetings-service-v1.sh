#!/usr/bin/env bash
# Restarts the meetings-service (v1) stack: removes it, waits until it is gone, deploys it again.
set -euo pipefail

DEPLOY_DIR="/opt/docker-deployer/qeng/v1/meetings-service"
STACK="q-meeting-service-v1"

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
