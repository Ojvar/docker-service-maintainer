//! Docker services status report.

use axum::{extract::State, http::StatusCode, response::Html};

use crate::{AppState, scripts::run_script};

/// Script (inside `SCRIPTS_DIR`) whose output becomes the report body.
const LIST_SERVICES_SCRIPT: &str = "list-docker-services.sh";

/// HTML template, embedded at compile time so the binary needs no runtime files.
/// Placeholders: `{{generated_at}}` and `{{output}}`.
const REPORT_TEMPLATE: &str = include_str!("../templates/status.html");

/// `GET /services/status`
///
/// Runs `list-docker-services.sh` and returns its output as an HTML page.
/// Responds `500` if the script is missing or exits with a non-zero status.
#[utoipa::path(
    get,
    path = "/services/status",
    responses(
        (status = 200, description = "HTML report", body = String, content_type = "text/html"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 500, description = "Status script missing or failed"),
    )
)]
pub async fn services_status(State(state): State<AppState>) -> Result<Html<String>, (StatusCode, String)> {
    let output = list_docker_services(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Html(render_report(&output, &now())))
}

/// Runs `list-docker-services.sh` and returns its stdout, or its stderr as the error.
async fn list_docker_services(state: &AppState) -> Result<String, String> {
    let script = state.config.scripts_dir.join(LIST_SERVICES_SCRIPT);
    if !script.is_file() {
        return Err(format!("missing script: {}", script.display()));
    }

    let output = run_script(&script).await?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fills the HTML template. The script output is escaped, so it always shows as plain text.
fn render_report(output: &str, generated_at: &str) -> String {
    REPORT_TEMPLATE
        .replace("{{generated_at}}", &escape(generated_at))
        .replace("{{output}}", &escape(output))
}

/// Current server-local time, taken from `date` to avoid pulling in a time crate.
fn now() -> String {
    std::process::Command::new("date")
        .arg("+%F %T %Z")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Escapes the characters that are special in HTML text and attributes.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_neutralises_html() {
        assert_eq!(
            escape(r#"<b class="x">a & b</b>"#),
            "&lt;b class=&quot;x&quot;&gt;a &amp; b&lt;/b&gt;"
        );
    }

    #[test]
    fn render_report_fills_placeholders() {
        let html = render_report("NAME  REPLICAS\nweb   1/1", "2026-01-01 00:00:00");

        assert!(html.contains("NAME  REPLICAS\nweb   1/1"));
        assert!(html.contains("2026-01-01 00:00:00"));
        assert!(!html.contains("{{"), "no placeholder should remain");
    }

    #[test]
    fn render_report_escapes_script_output() {
        let html = render_report("<script>alert(1)</script>", "now");
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
