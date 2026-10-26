use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;
use tokio::process::Command;

use crate::AppState;

#[derive(Serialize)]
pub struct RunResult {
    service: String,
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `POST /services/{service_name}/run` — runs `scripts/<service_name>.sh`.
pub async fn run_service_script(
    State(state): State<AppState>,
    Path(service_name): Path<String>,
) -> Result<Json<RunResult>, (StatusCode, String)> {
    if !is_valid_service_name(&service_name) {
        return Err((StatusCode::BAD_REQUEST, "invalid service name".into()));
    }

    let script = state.config.scripts_dir.join(format!("{service_name}.sh"));
    if !script.is_file() {
        return Err((StatusCode::NOT_FOUND, format!("no script for '{service_name}'")));
    }

    let output = Command::new("bash")
        .arg(&script)
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to run script: {e}")))?;

    Ok(Json(RunResult {
        service: service_name,
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }))
}

/// Only letters, digits, `-` and `_`, so the name can't escape the scripts directory.
fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
