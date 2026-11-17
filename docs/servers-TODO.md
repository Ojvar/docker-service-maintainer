# Servers tab — TODO

Deferred or skipped while building the Servers feature (scripts/servers, src/servers.rs, templates/servers.html).

## Servers / SSH
- [ ] **Server 5 (192.168.0.85) unreachable**: it answers ping, but every TCP connection (ports 22, 10022, 2222) is accepted and then reset (`kex_exchange_identification: Connection reset by peer`). Likely a firewall, fail2ban, hosts.deny or a broken sshd. Fix it on the host, then update the port/user in `scripts/servers/servers.conf` if needed.
- [ ] Container deploys: the image needs an `ssh` client, the key mounted at `SERVERS_SSH_KEY`, and all 8 host keys in `known_hosts`. BatchMode is on and there is no accept-new.

## Known issues / data
- [ ] Server 2 `/mnt/qeng_0_26` is at 98.5% and shows as critical. That's real, not a bug.
- [ ] No server exposes temperature sensors (all VMs). This is treated as informational and doesn't mark a server partial.
- [ ] Top-process CPU % comes from `ps` and is a lifetime average, not instantaneous.
- [ ] Results are kept only in memory and are lost on restart. Card sparklines use browser localStorage only.

## Untested paths
- [ ] Browser UI not clicked through manually (only the HTML was served and the JS built). Check the detail tabs, filters, RTL and light theme, and auto-refresh.
- [ ] Docker `no_permission` / `daemon_down` branches (every reachable host has Docker readable).
- [ ] Hosts without `ip`, `lsblk`, `systemctl` or `resolvectl` (all current hosts have them).
- [ ] The overall timeout (`SERVERS_COLLECT_TIMEOUT`) firing on a hung host.
- [ ] Running behind Traefik with `BASE_PATH` (relative URLs should work, but not verified).

## Polish / nice-to-haves
- [ ] README: document `/servers*` endpoints, `servers.conf`, and the `SERVERS_*` env vars.
- [ ] Persist the last results and a history (file or DB). Add server-side CPU/RAM/disk trend charts.
- [ ] Alerts (Slack, email or webhook) when a server goes offline or crosses the 80/90% thresholds. Make the thresholds configurable via env.
- [ ] Optional scheduled background collection on the server side. Today auto-refresh is driven by the browser.
- [ ] Concurrency cap or semaphore if the inventory grows well beyond 8 hosts.
- [ ] Auth hardening: a per-user token or roles, rate-limiting POST refresh, and audit logging.
- [ ] Edit the inventory from the UI (currently it's file-only, re-read on each request).
- [ ] Per-container detail and links from containers to the Services tab. Show failed-unit logs.
- [ ] Table view or CSV export for the fleet, like the Services tab has.
