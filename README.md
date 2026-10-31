# service-maintainer

A small HTTP API that runs per-service bash scripts and shows Docker service status.
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

## Scripts

Put one script per service in `scripts/`, named after the service:

```
scripts/
├── auth-service.sh            # run by POST /services/auth-service/run
├── file-service.sh
└── list-docker-services.sh    # used by GET /services/status
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
| `400` | Invalid service name                           |
| `401` | Missing or wrong `x-security-token`            |
| `404` | No script found for that service               |
| `500` | Script couldn't start, or status script failed |

## Tests

```bash
cargo test
```

## Version

The version comes from `version` in `Cargo.toml`. It is printed at startup (`service-maintainer v0.1.0 listening on ...`), shown in Swagger UI, and available with `./service-maintainer --version`.
