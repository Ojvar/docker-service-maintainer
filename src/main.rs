//! service-maintainer: a small, token-protected HTTP API that runs per-service
//! bash scripts and reports Docker service status.
//!
//! Routes (all require the `x-security-token` header):
//! - `POST /services/{service_name}/run` runs `scripts/<service_name>.sh`
//! - `GET  /services` lists services as JSON (used by the dashboard)
//! - `POST /services/{service_name}/reload` force-updates a Docker service
//! - `GET  /services/status` returns an HTML report of Docker services
//! - `GET  /backups` lists MongoDB backup files on the backup host
//! - `POST /backups/restore` restores one backup into a database
//!
//! Public: `GET /api` serves Swagger UI, generated from the code by `utoipa`,
//! `GET /dashboard` serves the admin dashboard and `GET /backups/dashboard`
//! the backups page (both ask for the token).

mod auth;
mod backups;
mod config;
mod dashboard;
mod docs;
mod scripts;
mod services;
mod status;

use std::sync::{Arc, atomic::AtomicBool};

use axum::{
    Router, middleware,
    routing::{get, post},
};

use config::Config;

/// State shared by all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Set while a backup restore is running, so only one runs at a time.
    pub restoring: Arc<AtomicBool>,
}

impl AppState {
    /// Wraps the config with fresh shared state.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            restoring: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Builds the router: service routes behind the token check, Swagger UI public.
fn app(state: AppState) -> Router {
    let base_path = state.config.base_path.clone();
    let swagger = docs::swagger_ui(&base_path);
    Router::new()
        .route("/services", get(services::list_services))
        .route("/services/status", get(status::services_status))
        .route(
            "/services/{service_name}/run",
            post(scripts::run_service_script),
        )
        .route(
            "/services/{service_name}/reload",
            post(services::reload_service),
        )
        .route("/backups", get(backups::list_backups))
        .route("/backups/restore", post(backups::restore_backup))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        .with_state(state)
        .route("/dashboard", get(dashboard::dashboard))
        .route("/backups/dashboard", get(backups::backups_dashboard))
        .merge(swagger)
        .layer(middleware::from_fn_with_state(
            base_path,
            docs::redirect_bare_api,
        ))
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
    let state = AppState::new(config);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("cannot bind {addr}: {e}");
            std::process::exit(1);
        });

    println!("service-maintainer v{VERSION} listening on http://{addr}");
    axum::serve(listener, app(state))
        .await
        .expect("server error");
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
        let (state, dir) = test_state(scripts);
        (app(state), dir)
    }

    /// Like [`test_app`], but returns the state so a test can inspect or change it.
    fn test_state(scripts: &[(&str, &str)]) -> (AppState, TestDir) {
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
        (AppState::new(config), dir)
    }

    async fn send(
        app: Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header(auth::TOKEN_HEADER, t);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// Sends a JSON `POST` body with the test token.
    async fn post_json(app: Router, uri: &str, json: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(auth::TOKEN_HEADER, TOKEN)
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
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
            (
                services::LIST_SERVICES_JSON_SCRIPT,
                r#"echo '{"ID":"x1","Name":"web","Mode":"replicated","Replicas":"1/2","Image":"nginx","Ports":""}'"#,
            ),
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
        let (app, _dir) = test_app(&[
            (services::LIST_SERVICES_JSON_SCRIPT, LIST_WEB),
            (services::RELOAD_SCRIPT, "echo \"$1\""),
        ]);
        let (status, _) = send(app, "POST", "/services/api/reload", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reload_runs_script_with_name() {
        let (app, _dir) = test_app(&[
            (services::LIST_SERVICES_JSON_SCRIPT, LIST_WEB),
            (services::RELOAD_SCRIPT, "echo \"reloaded $1\""),
        ]);
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

    /// Fake `list-backups.sh`: two files, printed oldest first.
    const LIST_BACKUPS: &str =
        r#"printf '1700000000.5\t10\told.archive.gz\n1800000000.1\t2048\tnew.archive.gz\n'"#;

    /// Fake `restore-backup.sh` that echoes its arguments.
    const RESTORE_ECHO: &str = r#"echo "restored $2 into $1""#;

    const RESTORE_NEW: &str = r#"{"backup":"new.archive.gz","database":"qeng_test"}"#;

    #[tokio::test]
    async fn backups_requires_token() {
        let (app, _dir) = test_app(&[(backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS)]);
        let (status, _) = send(app, "GET", "/backups", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn backups_lists_newest_first() {
        let (app, _dir) = test_app(&[(backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS)]);
        let (status, body) = send(app, "GET", "/backups", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        let new = body.find("new.archive.gz").unwrap();
        let old = body.find("old.archive.gz").unwrap();
        assert!(new < old, "newest first: {body}");
        assert!(body.contains(r#""size":2048,"modified":1800000000,"restorable":true"#));
        assert!(body.contains(r#""restore_running":false"#));
    }

    #[tokio::test]
    async fn backups_list_failure_is_500() {
        let (app, _dir) = test_app(&[(
            backups::LIST_BACKUPS_SCRIPT,
            "echo 'ssh: timeout' >&2; exit 255",
        )]);
        let (status, body) = send(app, "GET", "/backups", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("ssh: timeout"));
    }

    #[tokio::test]
    async fn backup_scripts_not_listed_as_services() {
        let (app, _dir) = test_app(&[
            (backups::LIST_BACKUPS_SCRIPT, "true"),
            (backups::RESTORE_BACKUP_SCRIPT, "true"),
        ]);
        let (_, body) = send(app, "GET", "/services", Some(TOKEN)).await;
        assert!(!body.contains("list-backups") && !body.contains("restore-backup"));
    }

    #[tokio::test]
    async fn restore_requires_token() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "POST", "/backups/restore", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn restore_rejects_invalid_names() {
        for body in [
            r#"{"backup":"../etc/passwd","database":"qeng_test"}"#,
            r#"{"backup":".hidden","database":"qeng_test"}"#,
            r#"{"backup":"a;rm -rf ~","database":"qeng_test"}"#,
            r#"{"backup":"new.archive.gz","database":"qeng test"}"#,
            r#"{"backup":"new.archive.gz","database":""}"#,
        ] {
            let (app, _dir) = test_app(&[
                (backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS),
                (backups::RESTORE_BACKUP_SCRIPT, RESTORE_ECHO),
            ]);
            let (status, _) = post_json(app, "/backups/restore", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
    }

    #[tokio::test]
    async fn restore_unknown_backup_is_404() {
        let (app, _dir) = test_app(&[
            (backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS),
            (backups::RESTORE_BACKUP_SCRIPT, RESTORE_ECHO),
        ]);
        let body = r#"{"backup":"missing.archive.gz","database":"qeng_test"}"#;
        let (status, _) = post_json(app, "/backups/restore", body).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn restore_runs_script_with_database_and_backup() {
        let (state, _dir) = test_state(&[
            (backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS),
            (backups::RESTORE_BACKUP_SCRIPT, RESTORE_ECHO),
        ]);
        let flag = state.restoring.clone();
        let (status, body) = post_json(app(state), "/backups/restore", RESTORE_NEW).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""success":true"#), "{body}");
        assert!(body.contains("restored new.archive.gz into qeng_test"));
        assert!(body.contains(r#""database":"qeng_test""#));
        assert!(
            !flag.load(std::sync::atomic::Ordering::SeqCst),
            "lock released"
        );
    }

    #[tokio::test]
    async fn restore_reports_failing_script() {
        let (app, _dir) = test_app(&[
            (backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS),
            (
                backups::RESTORE_BACKUP_SCRIPT,
                "echo 'mongorestore failed' >&2; exit 1",
            ),
        ]);
        let (status, body) = post_json(app, "/backups/restore", RESTORE_NEW).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""success":false"#) && body.contains(r#""exit_code":1"#));
        assert!(body.contains("mongorestore failed"));
    }

    #[tokio::test]
    async fn concurrent_restore_is_409() {
        let (state, _dir) = test_state(&[
            (backups::LIST_BACKUPS_SCRIPT, LIST_BACKUPS),
            (backups::RESTORE_BACKUP_SCRIPT, RESTORE_ECHO),
        ]);
        state
            .restoring
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (status, _) = post_json(app(state), "/backups/restore", RESTORE_NEW).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn backups_dashboard_is_public_html() {
        let (app, _dir) = test_app(&[]);
        let (status, body) = send(app, "GET", "/backups/dashboard", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Service Maintainer · Backups"));
    }
}
