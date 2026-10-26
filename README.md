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
