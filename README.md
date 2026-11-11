# service-maintainer

A small HTTP API that runs per-service bash scripts, shows Docker service status and
lists / restores MongoDB backups.
Every request must send a shared secret in the `x-security-token` header.

## Setup

```bash
cp .env.example .env        # then edit the values
cargo build --release
./target/release/service-maintainer
```

### `.env`

| Variable         | Required | Default   | Description                                     |
|------------------|----------|-----------|-------------------------------------------------|
| `PORT`           | yes      |           | Port to listen on                               |
| `HOST`           | no       | `0.0.0.0` | Interface to bind to                            |
| `SECURITY_TOKEN` | yes      |           | Value expected in the `x-security-token` header |
| `SCRIPTS_DIR`    | no       | `scripts` | Folder containing the `.sh` scripts             |
| `BASE_PATH`      | no       | (empty)   | Public prefix behind a proxy, e.g. `/maintain`, so Swagger works |
| `BACKUP_SSH_TARGET`     | no | `qeng@192.168.0.83`          | ssh target of the backup host (used by the backup scripts) |
| `BACKUP_SSH_KEY`        | no | `~/.ssh/id_qeng_2027`        | Private key the backup scripts use for ssh     |
| `BACKUP_SSH_PORT`       | no | `10022`                      | ssh port of the backup host                    |
| `BACKUP_REMOTE_DIR`     | no | `/home/qeng/mongo-backups`   | Backups folder on that host                    |
| `BACKUP_RESTORE_SCRIPT` | no | `/home/qeng/restore-db.sh`   | Restore script on that host (`<db> <file>`)    |

The `BACKUP_*` variables are read by the scripts, not by the server; values in `.env`
reach them because the server passes its environment on.

## Scripts

Put one script per service in `scripts/`, named after the service:

```
scripts/
├── auth-service.sh            # run by POST /services/auth-service/run
├── file-service.sh
├── list-docker-services.sh    # used by GET /services/status
├── list-backups.sh            # used by GET /backups
└── restore-backup.sh          # used by POST /backups/restore
```

Service names may contain only letters, digits, `-` and `_`.

## API

### Run a service script

```
POST /services/{service-name}/run
```

```bash
curl -X POST -H "x-security-token: $TOKEN" http://localhost:8080/services/auth-service/run
```

Response:

```json
{
  "service": "auth-service",
  "success": true,
  "exit_code": 0,
  "stdout": "restarted\n",
  "stderr": ""
}
```

A script that runs but fails still returns `200` with `"success": false`.

### Docker services status

```
GET /services/status
```

```bash
curl -H "x-security-token: $TOKEN" http://localhost:8080/services/status > status.html
```

Returns an HTML page with the output of `scripts/list-docker-services.sh`.
The page layout lives in `templates/status.html` (rebuild after editing it).

### Services list (JSON)

```
GET /services
```

Lists Docker services (from `scripts/list-docker-services-json.sh`) merged with the
per-service scripts. Each entry has `name`, `id`, `mode`, `replicas`, `running`,
`desired`, `image`, `ports`, `health` (`healthy`/`degraded`/`down`/`unknown`),
`in_docker` and `has_script`. If Docker fails, scripts are still listed and
`docker_error` is set.

### Reload a Docker service

```
POST /services/{service-name}/reload
```

Runs `scripts/reload-docker-service.sh <name>` (`docker service update --force --detach`).
The name must be a service currently listed by Docker (`404` otherwise, `400` for unsafe names).
Returns the same JSON as `/run`.

### MongoDB backups (list)

```
GET /backups
```

Runs `scripts/list-backups.sh`, which connects to the backup host over ssh and lists
`BACKUP_REMOTE_DIR`. Response, newest first (`modified` is Unix seconds; `restorable` is
`false` for names the restore route refuses):

```json
{
  "backups": [
    { "name": "qeng_db-backup-2026-11-11_06-00-20.archive.gz", "size": 52428800, "modified": 1794370220, "restorable": true }
  ],
  "restore_running": false
}
```

`500` if the script is missing or fails (e.g. ssh error; the message is in the body).

### MongoDB backups (restore)

```
POST /backups/restore
Content-Type: application/json

{ "backup": "qeng_db-backup-2026-11-11_06-00-20.archive.gz", "database": "qeng_test" }
```

Runs `scripts/restore-backup.sh <database> <backup>`, which runs on the backup host:
`restore-db.sh qeng_test ~/mongo-backups/qeng_db-backup-2026-11-11_06-00-20.archive.gz`.
**The target database is overwritten.**

```json
{
  "backup": "qeng_db-backup-2026-11-11_06-00-20.archive.gz",
  "database": "qeng_test",
  "success": true,
  "exit_code": 0,
  "stdout": "...",
  "stderr": "",
  "duration_ms": 41230
}
```

- `backup`: letters, digits, `.`, `-`, `_`, not starting with `.`, and it must be in the current list
- `database`: letters, digits, `-`, `_`, up to 64 characters
- `400` invalid name, `404` backup not listed, `409` another restore is running,
  `422` malformed body, `500` listing failed or the script couldn't start
- No timeout is applied, and the restore keeps running if the client disconnects.
  Only one restore runs at a time. A restore that runs but fails returns `200` with `"success": false`.

Requirements on the machine running service-maintainer (e.g. inside its container):
an `ssh` client, and a key that logs in to `BACKUP_SSH_TARGET` without a password
(the scripts use `BatchMode=yes`; unknown host keys are accepted on first connect).

### Backups dashboard

Open `http://<HOST>:<PORT>/backups/dashboard` (behind a proxy: `<BASE_PATH>/backups/dashboard`).
Same token, theme and RTL setting as the admin dashboard, with tabs to switch between them.
Features: summary cards (count, total size, latest/oldest backup), search, age filter,
sorting, pagination, human-readable sizes, relative and absolute times, newest backup
highlighted, auto-refresh, and a Restore button per file. Restoring asks for the target
database (default `qeng_test`) and requires typing it a second time, then shows progress
and the script output. Recent restores are remembered in the browser.
No external assets; the page is embedded from `templates/backups.html`.

### Admin dashboard

Open `http://<HOST>:<PORT>/dashboard` (behind a proxy: `<BASE_PATH>/dashboard`).
The page is public but holds no data: enter the token in the header and it is kept
in the browser's localStorage. Features: summary cards, search, filters (health,
mode, source), sorting, pagination, table/card views, detail drawer with a
"Run maintenance script" button, per-service Reload buttons (Docker services only), CSV export, auto-refresh, dark/light theme and RTL toggle.
No external assets; the page is embedded from `templates/dashboard.html`.

### API docs (Swagger)

Open `http://<HOST>:<PORT>/api/` for Swagger UI (no token needed to view it).
Click **Authorize** and enter your token to try requests from the page.
The docs are generated from the code with `utoipa`, so they always match the handlers.
The JSON spec is at `/api/openapi.json`.

### Status codes

| Code  | Meaning                                        |
|-------|------------------------------------------------|
| `200` | OK                                             |
| `400` | Invalid service, backup or database name       |
| `401` | Missing or wrong `x-security-token`            |
| `404` | No script found for that service, or unknown backup |
| `409` | A backup restore is already running            |
| `500` | Script couldn't start, or status/list script failed |

## Tests

```bash
cargo test
```

## Version

The version comes from `version` in `Cargo.toml`. It is printed at startup (`service-maintainer v0.1.0 listening on ...`), shown in Swagger UI, and available with `./service-maintainer --version`.
