# Traefik change: /opt/docker-data/traefik/dynamic.config.yaml

Traefik (stack `traefik`, v2.11.15) already has both providers:
- `providers.docker` (swarmMode: true, network server-net, exposedByDefault: false, watch: true)
- `providers.file` filename /traefik/dynamic.config.yaml, **watch: true**

After the `service-maintainer` stack is up, the route comes from its labels
(`service-maintainer@docker`, `service-maintainer-secure@docker`). Remove ONLY the three
service-maintainer blocks below from the file provider. Nothing else changes
(rmq, rmq-ui, sentry, auto-deployer, shared middlewares, tls stay as they are).

## Lines to remove (current line numbers)

Router (lines 3-12):
```yaml
    service-maintainer-router:
      entrypoints:
        - http
        - https
      rule: "Host(`api.qeng.ir`) && PathPrefix(`/maintain`)"
      service: service-maintainer-service
      middlewares:
        - service-maintainer-strip-prefix
      tls:
        options: default
```

Service (lines 49-52):
```yaml
    service-maintainer-service:
      loadBalancer:
        servers:
          - url: "http://host.docker.internal:18080"
```

Middleware (lines 76-79):
```yaml
    service-maintainer-strip-prefix:
      stripPrefix:
        prefixes:
          - "/maintain"
```

## IMPORTANT: edit in place (same inode)

The file is bind-mounted as a single file into the Traefik container. `sed -i`, vim's
default save, or `mv new old` create a NEW inode; the container keeps seeing the old file
and watch never fires. Write the edited copy back with `cat new > dynamic.config.yaml`
(or `cp new dynamic.config.yaml`, which truncates in place).

```bash
cd /opt/docker-data/traefik
cp -p dynamic.config.yaml dynamic.config.yaml.bak-$(date +%Y%m%d-%H%M%S)-pre-maintainer
cp -p dynamic.config.yaml /tmp/dyn.new
# delete the 3 blocks (by name, not line numbers, in case the file changed meanwhile)
awk '
  /^    service-maintainer-(router|service|strip-prefix):$/ {skip=1; next}
  skip && /^    [^ ]/ {skip=0}
  skip && /^  [^ ]/  {skip=0}
  !skip
' /tmp/dyn.new > /tmp/dyn.edited
diff -u dynamic.config.yaml /tmp/dyn.edited     # review: only the 3 blocks removed
cat /tmp/dyn.edited > dynamic.config.yaml        # in-place write -> Traefik reloads (watch)
docker exec $(docker ps -qf name=traefik_traefik) sh -c 'md5sum /traefik/dynamic.config.yaml' ; md5sum dynamic.config.yaml
```

Rollback: `cat dynamic.config.yaml.bak-<ts>-pre-maintainer > dynamic.config.yaml`
