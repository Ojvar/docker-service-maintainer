//! Running per-service maintenance scripts from the scripts directory.

use std::{path::Path as FsPath, process::Output};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;
use utoipa::ToSchema;
use tokio::process::Command;

use crate::AppState;

/// JSON body returned by [`run_service_script`].
#[derive(Debug, Serialize, ToSchema)]
pub struct RunResult {
    /// The requested service name.
    pub service: String,
    /// `true` when the script exited with status 0.
    pub success: bool,
    /// Exit code, or `None` if the script was killed by a signal.
    pub exit_code: Option<i32>,
    /// Everything the script wrote to stdout.
    pub stdout: String,
    /// Everything the script wrote to stderr.
    pub stderr: String,
}

/// `POST /services/{service_name}/run`
///
/// Runs `<SCRIPTS_DIR>/<service_name>.sh` with bash and returns its result.
///
/// - `400` if the name contains anything but letters, digits, `-` or `_`
/// - `404` if no script exists for that service
/// - `500` if the script could not be started
///
/// A script that runs but fails still returns `200`, with `success: false`.
#[utoipa::path(
    post,
    path = "/services/{service_name}/run",
    params(("service_name" = String, Path, description = "Letters, digits, `-` and `_` only", example = "auth-service")),
    responses(
        (status = 200, description = "Script finished", body = RunResult),
        (status = 400, description = "Invalid service name"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "No script for that service"),
        (status = 500, description = "Script could not be started"),
    )
)]
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

    let output = run_script(&script)
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

/// Runs a script with bash and captures its exit status, stdout and stderr.
///
/// Errors only when the process can't be started; a non-zero exit is returned
/// as a normal [`Output`].
pub async fn run_script(script: &FsPath) -> Result<Output, String> {
    Command::new("bash")
        .arg(script)
        .output()
        .await
        .map_err(|e| format!("failed to run {}: {e}", script.display()))
}

/// Accepts only letters, digits, `-` and `_` (max 100 chars), so a service
/// name can never point outside the scripts directory.
fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_service_names() {
        for name in ["auth-service", "file_service", "svc2", "A-b_C"] {
            assert!(is_valid_service_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_unsafe_service_names() {
        let too_long = "a".repeat(101);
        for name in ["", "../etc/passwd", "a/b", "a.sh", "a b", "a;rm", "سرویس", too_long.as_str()] {
            assert!(!is_valid_service_name(name), "{name:?} should be rejected");
        }
    }

    #[tokio::test]
    async fn run_script_captures_output_and_exit_code() {
        let dir = crate::TestDir::new();
        let script = dir.path().join("t.sh");
        std::fs::write(&script, "echo out; echo err >&2; exit 3").unwrap();

        let output = run_script(&script).await.unwrap();

        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
    }
}
