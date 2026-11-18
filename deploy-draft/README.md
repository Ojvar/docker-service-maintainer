# service-maintainer (Swarm stack)

Token-protected admin API (`x-security-token`) that runs the per-service bash scripts:
`docker service`/`docker stack` commands, MongoDB backup list/restore over ssh, and
server monitoring over ssh.

- Public URL: https://api.qeng.ir/maintain  (Swagger: /maintain/api, dashboard: /maintain/dashboard)
- Stack: `service-maintainer`, service `service-maintainer_service-maintainer`, 1 replica, pinned to `api.qeng.ir`
- Image: `docker.qeng.ir/qeng/service-maintainer:<version>` (built from the repo `Dockerfile`)
- Scripts: **baked into the image** at `/app/scripts` (`SCRIPTS_DIR`), from the repo's `scripts/`.
  They are NOT mounted; changing a script = commit + new image + deploy.
- Routing: Traefik docker/swarm provider via the `deploy.labels` in `docker-compose.yaml`
  (rule `Host(api.qeng.ir) && PathPrefix(/maintain)`, stripPrefix `/maintain`, port 18080).
  No host port is published.

## Files

| Path | Purpose |
|------|---------|
| `docker-compose.yaml` | stack definition (labels, mounts, healthcheck, limits) |
| `deploy.sh` | `bash ./deploy.sh` deploy/update, `bash ./deploy.sh remove` remove |
| `.env` | runtime settings + `SECURITY_TOKEN` (mode 600, never in the image) |

## Mounts

| Host | Container | Why |
|------|-----------|-----|
| `/opt/docker-data/service-maintainer/data` | `/app/data` | servers state/history/alerts/audit (`SERVERS_DATA_DIR`) |
| `/var/run/docker.sock` | same | `docker service ls/update`, `docker stack deploy/rm` |
| `/opt/docker-deployer` | same (ro) | `restart-*.sh` run `deploy.sh` there |
| `/opt/docker-data/dotenvs` | same (ro) | env_file paths of the stacks those scripts deploy |
| `/home/qeng/.docker/config.json` | same (ro) | `--with-registry-auth` for docker.qeng.ir |
| `/home/qeng/.ssh/id_qeng_2027`, `known_hosts` | same (ro) | ssh to backup host and monitored servers |

Note: while the single task restarts, Traefik (swarm poll every 15 s) briefly returns 404 for /maintain.

Runs as uid 1000 (qeng), gid 994 (docker). PID 1 is `tini`; SIGTERM -> graceful shutdown
(in-flight requests finish, `stop_grace_period: 60s`).

## Everyday commands

```bash
cd /opt/docker-deployer/base/service-maintainer
bash ./deploy.sh                                                  # deploy / apply changes
docker service ps service-maintainer_service-maintainer --no-trunc
docker service logs -f --tail 100 service-maintainer_service-maintainer
docker service update --force service-maintainer_service-maintainer   # restart
bash ./deploy.sh remove                                           # stop & remove
curl -s -o /dev/null -w '%{http_code}\n' https://api.qeng.ir/maintain/api/openapi.json
```

Scripts are part of the image (`/app/scripts`, root-owned, `*.sh` mode 755). To change one:

```bash
# on your PC, in the repo
vim scripts/<name>.sh && git add scripts && git commit -m "update script"
# bump version in Cargo.toml (e.g. 0.1.2), cargo update -p service-maintainer, then build + push (below)
# on the server: set the new tag in docker-compose.yaml, then
bash ./deploy.sh
docker exec $(docker ps -qf name=service-maintainer_service-maintainer) ls -l /app/scripts
```
`/opt/qeng-services/service-maintainer/scripts` on the host is no longer used (kept only as the
rollback source for the old systemd service); editing it changes nothing.
Settings: edit `.env`, then `bash ./deploy.sh` (env changes roll the task).

## Updating the image

```bash
# on a build machine with the repo
# --provenance/--sbom=false: Nexus rejects BuildKit attestation manifests ("error from registry: unknown")
docker build --provenance=false --sbom=false \
  -t docker.qeng.ir/qeng/service-maintainer:<ver> -t docker.qeng.ir/qeng/service-maintainer:latest .
docker push docker.qeng.ir/qeng/service-maintainer:<ver>
docker push docker.qeng.ir/qeng/service-maintainer:latest
# on api.qeng.ir: set the tag in docker-compose.yaml, then
bash ./deploy.sh        # update_config: stop-first, rollback on failure
```

## Rollback to systemd (old setup kept)

```bash
bash ./deploy.sh remove
# re-add the service-maintainer router/service/middleware to /opt/docker-data/traefik/dynamic.config.yaml
#   (cat /opt/docker-data/traefik/dynamic.config.yaml.bak-20261118-122845-pre-maintainer > /opt/docker-data/traefik/dynamic.config.yaml)
sudo systemctl enable --now qeng-service-maintainer
```

## Migration record (2026-11-18)

- Backups: `/opt/qeng-services/backups/service-maintainer-20261118-122845/` (unit file, tarball of the old dir),
  `/opt/docker-data/traefik/dynamic.config.yaml.bak-20261118-122845-pre-maintainer`
- Old systemd unit `qeng-service-maintainer` stopped + disabled; unit file and `/opt/qeng-services/service-maintainer/` kept.
- 2026-11-18 12:5x (Tehran): v0.1.1, scripts baked into the image; scripts bind mount removed
  (compose backup: docker-compose.yaml.bak-20261118-125055-pre-0.1.1).
