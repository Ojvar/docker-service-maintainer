use axum::{http::StatusCode, response::Html};
use serde::Deserialize;
use tokio::process::Command;

/// One row of `docker service ls --format '{{json .}}'`.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerService {
    name: String,
    mode: String,
    replicas: String,
    image: String,
    #[serde(default)]
    ports: String,
}

/// `GET /services/status` — HTML report of all Docker services.
pub async fn services_status() -> Result<Html<String>, (StatusCode, String)> {
    let services = list_docker_services().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Html(render_report(&services)))
}

async fn list_docker_services() -> Result<Vec<DockerService>, String> {
    let output = Command::new("docker")
        .args(["service", "ls", "--format", "{{json .}}"])
        .output()
        .await
        .map_err(|e| format!("failed to run docker: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }

    let mut services: Vec<DockerService> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("unexpected docker output: {e}"))?;

    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

fn render_report(services: &[DockerService]) -> String {
    let rows: String = services
        .iter()
        .map(|s| {
            let class = if is_healthy(&s.replicas) { "ok" } else { "bad" };
            format!(
                "<tr class=\"{class}\"><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&s.name),
                escape(&s.mode),
                escape(&s.replicas),
                escape(&s.image),
                escape(&s.ports),
            )
        })
        .collect();

    let healthy = services.iter().filter(|s| is_healthy(&s.replicas)).count();

    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Docker services status</title>
<style>
  body {{ font-family: system-ui, sans-serif; margin: 2rem; }}
  table {{ border-collapse: collapse; width: 100%; }}
  th, td {{ padding: .5rem .75rem; border-bottom: 1px solid #ddd; text-align: left; }}
  th {{ background: #f4f4f4; }}
  tr.ok td:nth-child(3) {{ color: #1a7f37; font-weight: 600; }}
  tr.bad {{ background: #fff0f0; }}
  tr.bad td:nth-child(3) {{ color: #cf222e; font-weight: 600; }}
</style></head>
<body>
<h1>Docker services</h1>
<p>{healthy} of {total} services running all replicas.</p>
<table>
<thead><tr><th>Name</th><th>Mode</th><th>Replicas</th><th>Image</th><th>Ports</th></tr></thead>
<tbody>{rows}</tbody>
</table>
</body></html>"#,
        total = services.len(),
    )
}

/// "2/2" is healthy, "0/1" or "1/2" is not.
fn is_healthy(replicas: &str) -> bool {
    let running_part = replicas.split_whitespace().next().unwrap_or_default();
    matches!(running_part.split_once('/'), Some((running, desired)) if running == desired && running != "0")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
