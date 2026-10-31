//! Structured service list (`GET /services`) used by the admin dashboard.
//!
//! Combines `docker service ls` (via `list-docker-services-json.sh`) with the
//! maintenance scripts found in `SCRIPTS_DIR`.

use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    AppState,
    scripts::{RunResult, is_valid_service_name, run_script, run_script_with_args},
};

/// Script (inside `SCRIPTS_DIR`) that prints one `docker service ls` JSON object per line.
pub const LIST_SERVICES_JSON_SCRIPT: &str = "list-docker-services-json.sh";

/// Helper scripts that are not per-service maintenance scripts.
const HELPER_SCRIPTS: [&str; 3] = ["list-docker-services", "list-docker-services-json", "reload-docker-service"];

/// Script (inside `SCRIPTS_DIR`) that force-updates one Docker service, given its name as `$1`.
pub const RELOAD_SCRIPT: &str = "reload-docker-service.sh";

/// Health derived from the replica count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// All desired replicas are running.
    Healthy,
    /// Some, but not all, desired replicas are running.
    Degraded,
    /// No replicas running while some are desired.
    Down,
    /// Not a Docker service, or replicas could not be read.
    Unknown,
}

/// One service as shown on the dashboard.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ServiceInfo {
    /// Service name (Docker service name or script name).
    pub name: String,
    /// Docker service ID, empty for script-only entries.
    pub id: String,
    /// `replicated` or `global`, empty for script-only entries.
    pub mode: String,
    /// Raw replicas text, e.g. `1/1`.
    pub replicas: String,
    /// Running replicas, when known.
    pub running: Option<u32>,
    /// Desired replicas, when known.
    pub desired: Option<u32>,
    /// Image including tag.
    pub image: String,
    /// Published ports as reported by Docker.
    pub ports: String,
    /// Derived health.
    pub health: Health,
    /// `true` when it is listed by `docker service ls`.
    pub in_docker: bool,
    /// `true` when `SCRIPTS_DIR/<name>.sh` exists, so it can be run.
    pub has_script: bool,
}

/// Response of `GET /services`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ServiceList {
    /// All known services, sorted by name.
    pub services: Vec<ServiceInfo>,
    /// Error from the Docker listing, if it failed (scripts are still listed).
    pub docker_error: Option<String>,
}

/// One line of `docker service ls --format '{{json .}}'`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerServiceRow {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    replicas: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    ports: String,
}

/// `GET /services`
///
/// Lists Docker services and maintenance scripts as JSON. If the Docker
/// listing fails, script-only entries are still returned with `docker_error` set.
#[utoipa::path(
    get,
    path = "/services",
    responses(
        (status = 200, description = "Service list", body = ServiceList),
        (status = 401, description = "Missing or wrong x-security-token"),
    )
)]
pub async fn list_services(State(state): State<AppState>) -> Result<Json<ServiceList>, (StatusCode, String)> {
    let scripts = script_names(&state);
    let (rows, docker_error) = match docker_rows(&state).await {
        Ok(rows) => (rows, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    Ok(Json(ServiceList { services: merge(rows, &scripts), docker_error }))
}

/// `POST /services/{service_name}/reload`
///
/// Force-redeploys a Docker Swarm service (`docker service update --force`)
/// via `reload-docker-service.sh`. The name must be a currently listed Docker service.
///
/// A reload that runs but fails still returns `200`, with `success: false`.
#[utoipa::path(
    post,
    path = "/services/{service_name}/reload",
    params(("service_name" = String, Path, description = "Docker service name (letters, digits, `-` and `_`)", example = "web")),
    responses(
        (status = 200, description = "Reload finished", body = RunResult),
        (status = 400, description = "Invalid service name"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Not a listed Docker service"),
        (status = 500, description = "Listing failed or reload script missing / could not start"),
    )
)]
pub async fn reload_service(
    State(state): State<AppState>,
    Path(service_name): Path<String>,
) -> Result<Json<RunResult>, (StatusCode, String)> {
    if !is_valid_service_name(&service_name) {
        return Err((StatusCode::BAD_REQUEST, "invalid service name".into()));
    }
    ensure_docker_service(&state, &service_name).await?;
    let output = run_reload(&state, &service_name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(RunResult {
        service: service_name,
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }))
}

/// Errors with `404` unless `name` is in the current `docker service ls` output.
async fn ensure_docker_service(state: &AppState, name: &str) -> Result<(), (StatusCode, String)> {
    let rows = docker_rows(state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if rows.iter().any(|r| r.name == name) {
        Ok(())
    } else {
        Err((StatusCode::NOT_FOUND, format!("no docker service '{name}'")))
    }
}

/// Runs the reload script with the service name as its only argument.
async fn run_reload(state: &AppState, name: &str) -> Result<std::process::Output, String> {
    let script = state.config.scripts_dir.join(RELOAD_SCRIPT);
    if !script.is_file() {
        return Err(format!("missing script: {}", script.display()));
    }
    run_script_with_args(&script, &[name]).await
}

/// Runs the JSON listing script and parses its output.
async fn docker_rows(state: &AppState) -> Result<Vec<DockerServiceRow>, String> {
    let script = state.config.scripts_dir.join(LIST_SERVICES_JSON_SCRIPT);
    if !script.is_file() {
        return Err(format!("missing script: {}", script.display()));
    }
    let output = run_script(&script).await?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(parse_rows(&String::from_utf8_lossy(&output.stdout)))
}

/// Parses JSON lines, skipping blank or malformed ones.
fn parse_rows(text: &str) -> Vec<DockerServiceRow> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Names of the per-service `.sh` scripts, without helpers.
fn script_names(state: &AppState) -> BTreeSet<String> {
    std::fs::read_dir(&state.config.scripts_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".sh")).map(str::to_string))
                .filter(|n| !HELPER_SCRIPTS.contains(&n.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// Combines Docker rows and scripts into one sorted list.
fn merge(rows: Vec<DockerServiceRow>, scripts: &BTreeSet<String>) -> Vec<ServiceInfo> {
    let docker_names: BTreeSet<String> = rows.iter().map(|r| r.name.clone()).collect();
    let mut services: Vec<ServiceInfo> = rows
        .into_iter()
        .map(|row| from_docker(row, scripts))
        .chain(scripts.iter().filter(|n| !docker_names.contains(*n)).map(|n| script_only(n)))
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    services
}

/// Builds a [`ServiceInfo`] from a Docker row.
fn from_docker(row: DockerServiceRow, scripts: &BTreeSet<String>) -> ServiceInfo {
    let (running, desired) = parse_replicas(&row.replicas);
    ServiceInfo {
        has_script: scripts.contains(&row.name),
        health: health_of(running, desired),
        name: row.name,
        id: row.id,
        mode: row.mode,
        replicas: row.replicas,
        running,
        desired,
        image: row.image,
        ports: row.ports,
        in_docker: true,
    }
}

/// Builds a [`ServiceInfo`] for a script with no Docker service.
fn script_only(name: &str) -> ServiceInfo {
    ServiceInfo {
        name: name.to_string(),
        id: String::new(),
        mode: String::new(),
        replicas: String::new(),
        running: None,
        desired: None,
        image: String::new(),
        ports: String::new(),
        health: Health::Unknown,
        in_docker: false,
        has_script: true,
    }
}

/// Reads `running/desired` from text like `2/3` or `1/1 (max 1 per node)`.
fn parse_replicas(text: &str) -> (Option<u32>, Option<u32>) {
    let first = text.split_whitespace().next().unwrap_or_default();
    match first.split_once('/') {
        Some((r, d)) => (r.parse().ok(), d.parse().ok()),
        None => (None, None),
    }
}

/// Derives health from replica counts.
fn health_of(running: Option<u32>, desired: Option<u32>) -> Health {
    match (running, desired) {
        (Some(r), Some(d)) if r >= d => Health::Healthy,
        (Some(0), Some(_)) => Health::Down,
        (Some(_), Some(_)) => Health::Degraded,
        _ => Health::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replicas() {
        assert_eq!(parse_replicas("2/3"), (Some(2), Some(3)));
        assert_eq!(parse_replicas("1/1 (max 1 per node)"), (Some(1), Some(1)));
        assert_eq!(parse_replicas(""), (None, None));
    }

    #[test]
    fn derives_health() {
        assert_eq!(health_of(Some(1), Some(1)), Health::Healthy);
        assert_eq!(health_of(Some(1), Some(3)), Health::Degraded);
        assert_eq!(health_of(Some(0), Some(2)), Health::Down);
        assert_eq!(health_of(None, None), Health::Unknown);
    }

    #[test]
    fn parses_json_lines_and_merges_scripts() {
        let text = r#"{"ID":"abc","Image":"nginx:1","Mode":"replicated","Name":"web","Ports":"*:80->80/tcp","Replicas":"1/1"}
not json

{"ID":"def","Image":"api:2","Mode":"global","Name":"api","Ports":"","Replicas":"0/1"}"#;
        let scripts: BTreeSet<String> = ["web".to_string(), "backup".to_string()].into();
        let services = merge(parse_rows(text), &scripts);

        let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["api", "backup", "web"]);
        assert_eq!(services[0].health, Health::Down);
        assert!(!services[0].has_script);
        assert!(!services[1].in_docker && services[1].has_script);
        assert!(services[2].has_script && services[2].health == Health::Healthy);
    }
}
