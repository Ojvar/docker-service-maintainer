//! service-maintainer: a small, token-protected HTTP API that runs per-service
//! bash scripts and reports Docker service status.
//!
//! Routes (all require the `x-security-token` header):
//! - `POST /services/{service_name}/run` runs `scripts/<service_name>.sh`
//! - `GET  /services` lists services as JSON (used by the dashboard)
//! - `POST /services/{service_name}/reload` force-updates a Docker service
//! - `GET  /services/status` returns an HTML report of Docker services
//!
//! Public: `GET /api` serves Swagger UI, generated from the code by `utoipa`,
//! and `GET /dashboard` serves the admin dashboard (it asks for the token).

mod auth;
mod config;
mod dashboard;
mod docs;
mod scripts;
mod services;
mod status;

use std::sync::Arc;

use axum::{
    Router, middleware,
    routing::{get, post},
};

use config::Config;

/// State shared by all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
}

/// Builds the router: service routes behind the token check, Swagger UI public.
fn app(state: AppState) -> Router {
    let base_path = state.config.base_path.clone();
    let swagger = docs::swagger_ui(&base_path);
    Router::new()
        .route("/services", get(services::list_services))
        .route("/services/status", get(status::services_status))
        .route("/services/{service_name}/run", post(scripts::run_service_script))
        .route("/services/{service_name}/reload", post(services::reload_service))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_token))
        .with_state(state)
        .route("/dashboard", get(dashboard::dashboard))
        .merge(swagger)
        .layer(middleware::from_fn_with_state(base_path, docs::redirect_bare_api))
}

/// App version, taken from `version` in `Cargo.toml` at build time.
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("service-maintainer {VERSION}");
        return;
    }

    let config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("config error: {e}");
        std::process::exit(1);
    });

    let addr = format!("{}:{}", config.host, config.port);
    let state = AppState { config: Arc::new(config) };

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        eprintln!("cannot bind {addr}: {e}");
        std::process::exit(1);
    });

    println!("service-maintainer v{VERSION} listening on http://{addr}");
    axum::serve(listener, app(state)).await.expect("server error");
}

/// Minimal self-deleting temp directory, so tests need no extra crate.
#[cfg(test)]
pub(crate) struct TestDir(pub std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "service-maintainer-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// End-to-end tests through the real router, using a temporary scripts directory.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "test-token";

    /// Creates a router whose scripts dir is a fresh temp dir with the given scripts.
    fn test_app(scripts: &[(&str, &str)]) -> (Router, TestDir) {
        let dir = TestDir::new();
        for (name, body) in scripts {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let config = Config {
            host: "127.0.0.1".into(),
            port: 0,
            security_token: TOKEN.into(),
            scripts_dir: dir.path().to_path_buf(),
            base_path: String::new(),
        };
        (app(AppState { config: Arc::new(config) }), dir)
    }

    async fn send(app: Router, method: &str, uri: &str, token: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header(auth::TOKEN_HEADER, t);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn swagger_is_public() {
        let (app, _dir) = test_app(&[]);
        let (status, body) = send(app, "GET", "/api/openapi.json", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/services/status"));
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "GET", "/services/status", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let (app, _dir) = test_app(&[("auth-service.sh", "echo hi")]);
        let (status, _) = send(app, "POST", "/services/auth-service/run", Some("nope")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn runs_service_script() {
        let (app, _dir) = test_app(&[("auth-service.sh", "echo restarted")]);
        let (status, body) = send(app, "POST", "/services/auth-service/run", Some(TOKEN)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""success":true"#));
        assert!(body.contains(r#""stdout":"restarted\n""#));
    }

    #[tokio::test]
    async fn reports_failing_script() {
        let (app, _dir) = test_app(&[("bad.sh", "echo boom >&2; exit 2")]);
        let (status, body) = send(app, "POST", "/services/bad/run", Some(TOKEN)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""success":false"#));
        assert!(body.contains(r#""exit_code":2"#));
    }

    #[tokio::test]
    async fn unknown_service_is_404() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "POST", "/services/missing/run", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn path_traversal_is_400() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "POST", "/services/..%2Fetc/run", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_renders_script_output() {
        let (app, _dir) = test_app(&[("list-docker-services.sh", "echo 'NAME web 1/1'")]);
        let (status, body) = send(app, "GET", "/services/status", Some(TOKEN)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<pre>NAME web 1/1\n</pre>"));
    }

    #[tokio::test]
    async fn status_fails_without_script() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "GET", "/services/status", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
    #[tokio::test]
    async fn dashboard_is_public_html() {
        let (app, _dir) = test_app(&[]);
        let (status, body) = send(app, "GET", "/dashboard", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Service Maintainer"));
    }

    #[tokio::test]
    async fn services_requires_token() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "GET", "/services", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn services_lists_docker_and_scripts() {
        let (app, _dir) = test_app(&[
            (services::LIST_SERVICES_JSON_SCRIPT, r#"echo '{"ID":"x1","Name":"web","Mode":"replicated","Replicas":"1/2","Image":"nginx","Ports":""}'"#),
            ("backup.sh", "true"),
        ]);
        let (status, body) = send(app, "GET", "/services", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""name":"web""#) && body.contains(r#""health":"degraded""#));
        assert!(body.contains(r#""name":"backup""#));
        assert!(body.contains(r#""docker_error":null"#));
    }

    #[tokio::test]
    async fn services_reports_docker_error() {
        let (app, _dir) = test_app(&[("backup.sh", "true")]);
        let (status, body) = send(app, "GET", "/services", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""docker_error":"missing script"#));
    }
    const LIST_WEB: &str = r#"echo '{"ID":"x1","Name":"web","Mode":"replicated","Replicas":"1/1","Image":"nginx","Ports":""}'"#;

    #[tokio::test]
    async fn reload_requires_token() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "POST", "/services/web/reload", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn reload_rejects_invalid_name() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "POST", "/services/..%2Fetc;rm/reload", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reload_unknown_service_is_404() {
        let (app, _dir) = test_app(&[(services::LIST_SERVICES_JSON_SCRIPT, LIST_WEB), (services::RELOAD_SCRIPT, "echo \"$1\"")]);
        let (status, _) = send(app, "POST", "/services/api/reload", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reload_runs_script_with_name() {
        let (app, _dir) = test_app(&[(services::LIST_SERVICES_JSON_SCRIPT, LIST_WEB), (services::RELOAD_SCRIPT, "echo \"reloaded $1\"")]);
        let (status, body) = send(app, "POST", "/services/web/reload", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""success":true"#) && body.contains("reloaded web"));
    }

    #[tokio::test]
    async fn reload_script_not_listed_as_service() {
        let (app, _dir) = test_app(&[(services::RELOAD_SCRIPT, "true")]);
        let (_, body) = send(app, "GET", "/services", Some(TOKEN)).await;
        assert!(!body.contains("reload-docker-service"));
    }
}
