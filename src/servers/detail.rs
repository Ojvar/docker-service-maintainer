//! On-demand, read-only lookups on one server: a container's `docker inspect` summary (never
//! environment variables or logs) and a systemd unit's status with its last 50 journal lines.
//! Only containers and units seen in that server's last collection can be looked up.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::Value;

use super::{ApiError, ApiResult, COLLECT_SCRIPT, Client, SERVERS_DIR, find_entry, truncate};
use crate::{AppState, scripts::run_script_with_args};

/// `GET /servers/{server_id}/containers/{container}`
///
/// Read-only `docker inspect` summary of one running container (state, health, restarts,
/// limits, mounts, networks, ports). The container must be in the server's last collection.
#[utoipa::path(
    get,
    path = "/servers/{server_id}/containers/{container}",
    params(
        ("server_id" = String, Path, description = "Inventory id", example = "server-2"),
        ("container" = String, Path, description = "Container name or short id from the last collection"),
    ),
    responses(
        (status = 200, description = "Inspect summary (or an `error` field from docker)", body = Object),
        (status = 400, description = "Invalid server id or container name"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Unknown server, or container not in the last collection"),
        (status = 502, description = "Server unreachable"),
    )
)]
pub async fn container_detail(
    State(state): State<AppState>,
    Path((server_id, container)): Path<(String, String)>,
    client: Client,
) -> ApiResult<Json<Value>> {
    if !is_valid_container(&container) {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid container name".to_string(),
        )
            .into());
    }
    lookup(&state, &client, &server_id, "container", &container).await
}

/// `GET /servers/{server_id}/units/{unit}`
///
/// Read-only status of one systemd service (`systemctl show`) and its last 50 journal lines,
/// when the ssh user may read the journal. The unit must be in the server's last collection.
#[utoipa::path(
    get,
    path = "/servers/{server_id}/units/{unit}",
    params(
        ("server_id" = String, Path, description = "Inventory id", example = "server-2"),
        ("unit" = String, Path, description = "Unit name ending in .service", example = "docker.service"),
    ),
    responses(
        (status = 200, description = "Unit properties and recent journal", body = Object),
        (status = 400, description = "Invalid server id or unit name"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Unknown server, or unit not in the last collection"),
        (status = 502, description = "Server unreachable"),
    )
)]
pub async fn unit_detail(
    State(state): State<AppState>,
    Path((server_id, unit)): Path<(String, String)>,
    client: Client,
) -> ApiResult<Json<Value>> {
    if !is_valid_unit(&unit) {
        return Err((StatusCode::BAD_REQUEST, "invalid unit name".to_string()).into());
    }
    lookup(&state, &client, &server_id, "unit", &unit).await
}

/// Checks the target against the last collection, runs `collect-server.sh <id> <kind> <name>`
/// and audits the request.
async fn lookup(
    state: &AppState,
    client: &Client,
    server_id: &str,
    kind: &str,
    name: &str,
) -> ApiResult<Json<Value>> {
    let entry = find_entry(state, server_id)?;
    let action = format!("{kind}_detail");
    let known = {
        let servers = state.servers.lock();
        servers
            .records
            .get(&entry.id)
            .and_then(|r| r.data.as_ref())
            .is_some_and(|d| {
                if kind == "container" {
                    has_container(d, name)
                } else {
                    has_unit(d, name)
                }
            })
    };
    if !known {
        state
            .servers
            .audit(client, &action, Some(&entry.id), Some(name), "not_found");
        return Err((
            StatusCode::NOT_FOUND,
            format!("no {kind} '{name}' in the last collection of {}", entry.id),
        )
            .into());
    }
    let script = state
        .config
        .scripts_dir
        .join(SERVERS_DIR)
        .join(COLLECT_SCRIPT);
    let output = run_script_with_args(&script, &[&entry.id, kind, name])
        .await
        .map_err(|e| ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let parsed: Option<Value> = serde_json::from_slice(&output.stdout).ok();
    let result = match parsed {
        Some(mut v) if v.get("reachable") == Some(&Value::Bool(true)) => {
            Ok(v.get_mut("data").map(Value::take).unwrap_or(Value::Null))
        }
        Some(v) => Err(truncate(
            v.get("error")
                .and_then(Value::as_str)
                .unwrap_or("unreachable"),
        )),
        None => Err(truncate(&String::from_utf8_lossy(&output.stderr))),
    };
    match result {
        Ok(data) => {
            state
                .servers
                .audit(client, &action, Some(&entry.id), Some(name), "ok");
            Ok(Json(data))
        }
        Err(error) => {
            state.servers.audit(
                client,
                &action,
                Some(&entry.id),
                Some(name),
                &format!("error: {error}"),
            );
            Err((StatusCode::BAD_GATEWAY, error).into())
        }
    }
}

/// `true` when the container (by name or short id) is in the collected container list.
fn has_container(data: &Value, name: &str) -> bool {
    data.pointer("/docker/containers")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter().any(|c| {
                c.get("name").and_then(Value::as_str) == Some(name)
                    || c.get("id").and_then(Value::as_str) == Some(name)
            })
        })
}

/// `true` when the unit is among the collected running or failed services.
fn has_unit(data: &Value, unit: &str) -> bool {
    ["/services/running", "/services/failed"]
        .iter()
        .any(|path| {
            data.pointer(path)
                .and_then(Value::as_array)
                .is_some_and(|list| {
                    list.iter()
                        .any(|u| u.get("unit").and_then(Value::as_str) == Some(unit))
                })
        })
}

/// Docker container name or id: letter/digit first, then letters, digits, `_`, `.`, `-` (max 255).
pub(crate) fn is_valid_container(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 255
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// systemd service name: letters, digits, `@ _ . : -`, ending in `.service` (max 200).
pub(crate) fn is_valid_unit(unit: &str) -> bool {
    unit.len() <= 200
        && unit.len() > ".service".len()
        && unit.ends_with(".service")
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '.' | ':' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_names() {
        assert!(is_valid_container("q-auth_1.abc") && is_valid_container("3b3fe4c399cc"));
        for bad in ["", "-x", ".x", "a b", "a;rm", "a/b", "$(id)"] {
            assert!(!is_valid_container(bad), "{bad:?}");
        }
        assert!(is_valid_unit("docker.service") && is_valid_unit("getty@tty1.service"));
        for bad in [
            "docker",
            ".service",
            "a b.service",
            "../x.service",
            "x;rm.service",
            "x.socket",
        ] {
            assert!(!is_valid_unit(bad), "{bad:?}");
        }
    }

    #[test]
    fn finds_collected_targets() {
        let data = json!({
            "docker": {"containers": [{"id": "abc123", "name": "web.1"}]},
            "services": {"running": [{"unit": "docker.service"}], "failed": [{"unit": "bad.service"}]},
        });
        assert!(
            has_container(&data, "web.1")
                && has_container(&data, "abc123")
                && !has_container(&data, "db")
        );
        assert!(
            has_unit(&data, "docker.service")
                && has_unit(&data, "bad.service")
                && !has_unit(&data, "ssh.service")
        );
    }
}
