# Servers tab — TODO

Deferred or skipped while building the Servers feature (scripts/servers, src/servers.rs, templates/servers.html).

## Servers / SSH
- [x] **Server 5 (192.168.0.85) unreachable**: removed from inventory; .85 sshd needs host-side fix (2026-11-17: connections from .121/.83/.122 are refused on ports 22 and 10022; from the dev machine they are reset).
- [x] Deployment on 192.168.0.121 (systemd unit `qeng-service-maintainer`, runs as `qeng`): 'Host key verification failed' because `~/.ssh/known_hosts` only had .83. Fixed 2026-11-17 by adding `ssh-keyscan -p 10022` keys for every inventory host, checked against the local known_hosts. OpenSSH there is 7.4, so `StrictHostKeyChecking=accept-new` is not available.
- [x] Added `scripts/servers/scan-host-keys.sh [--dry-run] [id...]`: keyscan plus skip-existing, mismatch refusal, timestamped backup and fingerprints. Also added the optional `SERVERS_KNOWN_HOSTS` setting, which `collect-server.sh` passes as `UserKnownHostsFile`.
- [x] Container deploys (N/A: deployed as systemd unit qeng-service-maintainer on .121): the image needs an `ssh` client, the key mounted at `SERVERS_SSH_KEY`, and all 8 host keys in `known_hosts`. BatchMode is on and there is no accept-new.

## Known issues / data
- [x] Server 2 `/mnt/qeng_0_26` at 98.5%: investigated 2026-11-17 (read-only). It is not a .122 disk; see "Findings: Server 2 /mnt/qeng_0_26" below.
- [x] Exclude or flag network mounts (cifs/smb3/nfs/fuse.sshfs) in the probe so a remote share can't make a server warning/critical; show them as "network share" in Storage. — done: probe adds `network` flag; excluded from level, badge in Storage.
- [x] No server exposes temperature sensors (all VMs). This is treated as informational and doesn't mark a server partial. — done: thermal zones + hwmon + `sensors -u`, with source shown; stays info-only.
- [x] Top-process CPU % comes from `ps` and is a lifetime average, not instantaneous. — done: sampled from /proc over ~1 s (`top_cpu_mode`), ps lifetime fallback.
- [x] Results are kept only in memory and are lost on restart. Card sparklines use browser localStorage only. — done: resolved by persistence (SERVERS_DATA_DIR); sparklines use server-side `recent`.

## Untested paths
- [ ] Browser UI not clicked through manually (only the HTML was served and the JS built). Check the detail tabs, filters, RTL and light theme, and auto-refresh.
- [x] Docker `no_permission` / `daemon_down` branches (every reachable host has Docker readable). — done: scripts/servers/test-probe.sh (fake docker: no_permission, daemon_down, running, not installed).
- [x] Hosts without `ip`, `lsblk`, `systemctl` or `resolvectl` (all current hosts have them). — done: /sys and /proc fallbacks, covered by test-probe.sh (PROBE_DISABLE).
- [x] The overall timeout (`SERVERS_COLLECT_TIMEOUT`) firing on a hung host. — done: verified against a hung listener; fixed a pipefail bug that dropped the JSON when ssh had no stderr.
- [x] Running behind Traefik with `BASE_PATH` (relative URLs should work, but not verified). — done: verified via a /maintain prefix-stripping proxy (dashboard, API, /api redirect, X-Forwarded-For).

## Polish / nice-to-haves
- [ ] README: document `/servers*` endpoints, `servers.conf`, and the `SERVERS_*` env vars.
- [x] Persist the last results and a history (file or DB). Add server-side CPU/RAM/disk trend charts. — done: JSON/JSONL in SERVERS_DATA_DIR, SERVERS_HISTORY_DAYS retention, GET /servers/{id}/history, trend chart.
- [x] Alerts (Slack, email or webhook) when a server goes offline or crosses the 80/90% thresholds. Make the thresholds configurable via env. — done: SERVERS_WARNING/CRITICAL_PERCENT, generic JSON webhook (SERVERS_ALERT_WEBHOOK_URL) with dedupe + SERVERS_ALERT_COOLDOWN; Slack/email via webhook only.
- [ ] Optional scheduled background collection on the server side. Today auto-refresh is driven by the browser.
- [x] Concurrency cap or semaphore if the inventory grows well beyond 8 hosts. — done: SERVERS_CONCURRENCY (default 8).
- [x] Auth hardening: a per-user token or roles, rate-limiting POST refresh, and audit logging. — done: rate limit (SERVERS_REFRESH_MIN_INTERVAL, 429) + audit log (GET /servers/audit). Skipped per-user tokens/roles: the single shared token is used by every page and route; the audit log records IP, X-Forwarded-For and User-Agent instead.
- [ ] Edit the inventory from the UI (currently it's file-only, re-read on each request).
- [x] Per-container detail and links from containers to the Services tab. Show failed-unit logs. — done: container inspect summary (no env/logs) and unit status + last 50 journal lines. Skipped container logs (secrets risk).
- [x] Table view or CSV export for the fleet, like the Services tab has. — done: table view toggle + CSV export.

## Findings: Server 2 /mnt/qeng_0_26 (2026-11-17, read-only)
- It's a **read-only CIFS mount** of `//192.168.0.26/d$` (the D: admin share of a Windows host): 3.5T in total, 3.4T used, 53G free. Nothing on .122 fills it, and nothing can be cleaned from .122 (the mount is `ro`).
- 26 top-level folders. 21 were measured at ~5.8 GB in total: Web 3.0G, MojSolution 1.5G, Sign18 568M, Elevator 218M, PlanControl-Release 210M, $RECYCLE.BIN 78M, excel-tmp 59M; the rest are under 41M.
- 5 folders took over 75s to size over SMB (many small files): BackupProject, Education, NewApp, OtherApplication, PortalApp-Release. Their top levels are source and release trees, unlikely to hold terabytes.
- So almost all of the ~3.4T is probably not visible through this share: hidden or ACL-restricted folders, VSS shadow storage under `System Volume Information`, or files outside these folders. Check on 192.168.0.26 itself (TreeSize/WizTree, `vssadmin list shadowstorage`).
- Cleanup candidates (on the Windows host, owner's call): `$RECYCLE.BIN` ~78M (low risk), `excel-tmp` ~59M (low-medium risk). Both are negligible; the real reclaim depends on the host-side scan.
- Expansion is not needed on .122. Whether D: on 192.168.0.26 needs space depends on the host-side scan; 53G free on a 3.5T volume is tight.

