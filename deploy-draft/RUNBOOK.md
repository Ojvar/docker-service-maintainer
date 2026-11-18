# Migration runbook: qeng-service-maintainer (systemd) -> Swarm stack + Traefik labels

Host: api.qeng.ir (192.168.0.121), Swarm leader. Steps marked **[CONFIRM]** change something.
Times are Asia/Tehran. Never `cat` the .env files; never paste the token into a command line.

## 0. Pre-checks (read-only)
```bash
systemctl is-active qeng-service-maintainer               # active
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/api/openapi.json   # 200
curl -sk -o /dev/null -w '%{http_code}\n' --resolve api.qeng.ir:443:127.0.0.1 https://api.qeng.ir/maintain/api/openapi.json  # 200
docker service ls | grep -c . ; docker stack ls | grep -c .   # note counts for later comparison
```

## 1. Image  **[CONFIRM: pushes to docker.qeng.ir]**
Built and smoke-tested locally as `service-maintainer:local-test` (includes the graceful-shutdown change; commit it first).
```bash
# local machine, repo root
docker build --provenance=false --sbom=false -t docker.qeng.ir/qeng/service-maintainer:0.1.0 -t docker.qeng.ir/qeng/service-maintainer:latest .   # Nexus rejects attestation manifests
docker push docker.qeng.ir/qeng/service-maintainer:0.1.0
# server
docker pull docker.qeng.ir/qeng/service-maintainer:0.1.0
```
(Alternative without the registry: copy the repo to the server and `docker build` there; its daemon
mirror registry.docker.ir serves debian:bookworm-slim / docker:25-cli. But `docker stack deploy`
works best with a registry image.)

## 2. Backups + prepare  **[CONFIRM: server writes, nothing running is touched]**
```bash
TS=$(date +%Y%m%d-%H%M%S); B=/opt/qeng-services/backups/service-maintainer-$TS
mkdir -p $B && chmod 700 $B
sudo cp -p /etc/systemd/system/qeng-service-maintainer.service $B/
tar -C /opt/qeng-services -czpf $B/service-maintainer-dir.tgz service-maintainer   # binary, .env, scripts, data
cp -p /opt/docker-data/traefik/dynamic.config.yaml /opt/docker-data/traefik/dynamic.config.yaml.bak-$TS-pre-maintainer
chmod 600 $B/*

mkdir -p /opt/docker-deployer/base/service-maintainer /opt/docker-data/service-maintainer/data
# copy docker-compose.yaml, deploy.sh, README.md and the .env draft (as .env.draft) from deploy-draft/
cd /opt/docker-deployer/base/service-maintainer
# inject the token from the old .env without printing it
awk -F= 'NR==FNR{if($1=="SECURITY_TOKEN")t=substr($0,length($1)+2);next}
         /^SECURITY_TOKEN=/{print "SECURITY_TOKEN=" t; next} 1' \
    /opt/qeng-services/service-maintainer/.env .env.draft > .env && chmod 600 .env && rm .env.draft
grep -c '^SECURITY_TOKEN=__COPY' .env    # must print 0
docker stack config -c docker-compose.yaml > /dev/null && echo compose-ok
```

## 3. Validate the image on a non-conflicting port (no Traefik, no data writes)  **[CONFIRM]**
```bash
cd /opt/docker-deployer/base/service-maintainer
docker run -d --name sm-validate --user 1000:994 -p 127.0.0.1:28080:18080 --env-file .env \
  -e SCRIPTS_DIR=/app/scripts -e SERVERS_DATA_DIR=off \
  -v /etc/localtime:/etc/localtime:ro \
  -v /opt/qeng-services/service-maintainer/scripts:/app/scripts:ro \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /opt/docker-deployer:/opt/docker-deployer:ro -v /opt/docker-data/dotenvs:/opt/docker-data/dotenvs:ro \
  -v /home/qeng/.docker/config.json:/home/qeng/.docker/config.json:ro \
  -v /home/qeng/.ssh/id_qeng_2027:/home/qeng/.ssh/id_qeng_2027:ro \
  -v /home/qeng/.ssh/known_hosts:/home/qeng/.ssh/known_hosts:ro \
  docker.qeng.ir/qeng/service-maintainer:0.1.0
sleep 15; docker inspect sm-validate --format '{{.State.Health.Status}}'          # healthy
H="x-security-token: $(grep '^SECURITY_TOKEN=' .env | cut -d= -f2-)"
curl -s -o /dev/null -w 'openapi %{http_code}\n' http://127.0.0.1:28080/api/openapi.json   # 200
curl -s -H "$H" http://127.0.0.1:28080/services | head -c 300; echo                        # docker via socket, no docker_error
curl -s -H "$H" http://127.0.0.1:28080/backups  | head -c 300; echo                        # ssh to 192.168.0.83 works
docker exec sm-validate sh -c 'id; ls /app/scripts | wc -l; docker stack ls | head -3'
docker rm -f sm-validate; unset H
```
Do NOT call /services/*/run, /reload, /backups/restore or /servers/refresh here.

## 4. Cutover  **[CONFIRM: ~15-30 s outage of /maintain only]**
```bash
sudo systemctl stop qeng-service-maintainer                         # a) old instance down (no duplicates)
rsync -a /opt/qeng-services/service-maintainer/data/ /opt/docker-data/service-maintainer/data/   # b) consistent copy
cd /opt/docker-deployer/base/service-maintainer && bash ./deploy.sh  # c) start stack
watch -n2 'docker service ps service-maintainer_service-maintainer --format "{{.CurrentState}} {{.Error}}"'  # Running, healthy
curl -s http://127.0.0.1:38080/api/http/routers | grep -o '"name":"service-maintainer[^"]*"'   # d) @docker routers present
curl -sk -o /dev/null -w '%{http_code}\n' --resolve api.qeng.ir:443:127.0.0.1 https://api.qeng.ir/maintain/api/openapi.json  # 200
# e) remove the 3 file-provider blocks IN PLACE (see traefik-dynamic.config.change.md)
# f) after everything below passes:
sudo systemctl disable qeng-service-maintainer                      # unit file stays for rollback
```

## 5. Validation
- `docker service ps service-maintainer_service-maintainer` -> 1/1 Running, healthy; `docker inspect` health = healthy
- `docker exec $(docker ps -qf name=service-maintainer_) ls /app/scripts` -> the scripts; `ls /app/data` -> state files
- Traefik API: `service-maintainer@docker`, `service-maintainer-secure@docker` present, no `service-maintainer-router@file`
- `https://api.qeng.ir/maintain/api` (Swagger), `/maintain/dashboard`, `/maintain/servers/dashboard` -> 200
- With token: `GET /maintain/services`, `GET /maintain/backups` -> 200 with data
- Other routes unchanged: `/api/v2/auth-service/`, `/auto-deployer`, rmq/sentry hosts; `docker service ls` count unchanged
- Restart policy: `docker kill --signal KILL <task>` -> Swarm starts a new task (on-failure)
- Logs: `docker service logs --tail 50 service-maintainer_service-maintainer`
- Graceful stop: `docker service update --force ...` -> log shows "shutdown signal received" / "stopped"
- `ss -ltnp | grep 18080` -> nothing on the host

## Rollback (any time)
```bash
cd /opt/docker-deployer/base/service-maintainer && bash ./deploy.sh remove
cat /opt/docker-data/traefik/dynamic.config.yaml.bak-<TS>-pre-maintainer > /opt/docker-data/traefik/dynamic.config.yaml   # in place
sudo systemctl enable --now qeng-service-maintainer
curl -sk -o /dev/null -w '%{http_code}\n' --resolve api.qeng.ir:443:127.0.0.1 https://api.qeng.ir/maintain/api/openapi.json
```
(Data written while on Docker lives in /opt/docker-data/service-maintainer/data; copy it back if wanted.)
