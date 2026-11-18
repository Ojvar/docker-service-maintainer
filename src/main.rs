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
//! - `GET  /servers` lists the monitored servers with their last collected data
//! - `GET  /servers/{server_id}` returns one server with the full collected data
//! - `POST /servers/refresh` collects all servers over ssh in the background
//! - `POST /servers/{server_id}/refresh` collects one server in the background
//! - `GET  /servers/{server_id}/history` returns stored samples for trend charts
//! - `GET  /servers/{server_id}/containers/{c}`, `/units/{u}`: read-only detail lookups
//! - `GET  /servers/alerts`, `GET /servers/audit`: recent alerts and the refresh/lookup log
//!
//! Public: `GET /api` serves Swagger UI, generated from the code by `utoipa`,
//! `GET /dashboard` serves the admin dashboard, `GET /backups/dashboard`
//! the backups page and `GET /servers/dashboard` the servers page (all ask for the token).

mod auth;
mod backups;
mod config;
mod dashboard;
mod docs;
mod scripts;
mod servers;
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
    /// Collected data, history, alerts and running collections of the monitored servers.
    pub servers: Arc<servers::Servers>,
}

impl AppState {
    /// Wraps the config with fresh shared state.
    pub fn new(config: Config) -> Self {
        let servers = Arc::new(servers::Servers::new(config.servers.clone()));
        Self {
            config: Arc::new(config),
            restoring: Arc::new(AtomicBool::new(false)),
            servers,
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
        .route("/servers", get(servers::list_servers))
        .route("/servers/refresh", post(servers::refresh_all))
        .route("/servers/alerts", get(servers::alerts::list_alerts))
        .route("/servers/audit", get(servers::list_audit))
        .route("/servers/{server_id}", get(servers::get_server))
        .route(
            "/servers/{server_id}/refresh",
            post(servers::refresh_server),
        )
        .route("/servers/{server_id}/history", get(servers::server_history))
        .route(
            "/servers/{server_id}/containers/{container}",
            get(servers::detail::container_detail),
        )
        .route(
            "/servers/{server_id}/units/{unit}",
            get(servers::detail::unit_detail),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        .with_state(state)
        .route("/dashboard", get(dashboard::dashboard))
        .route("/backups/dashboard", get(backups::backups_dashboard))
        .route("/servers/dashboard", get(servers::servers_dashboard))
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
    // Peer addresses are recorded in the Servers audit log.
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");
    println!("service-maintainer stopped");
}

/// Resolves on SIGTERM (`docker stop`, `systemctl stop`) or Ctrl+C, so `axum::serve`
/// stops accepting connections and lets in-flight requests finish before exiting.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                eprintln!("cannot install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    println!("shutdown signal received, finishing in-flight requests");
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
            servers: servers::Settings::default(),
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

    /// Writes `servers/servers.conf` and a fake `servers/collect-server.sh` into the scripts dir.
    fn add_servers(dir: &TestDir, collector: &str) {
        let servers_dir = dir.path().join(servers::SERVERS_DIR);
        std::fs::create_dir_all(&servers_dir).unwrap();
        std::fs::write(
            servers_dir.join(servers::INVENTORY_FILE),
            "# test inventory\nweb-1 10.0.0.1 22 qeng Web One\ndb-1 10.0.0.2 22 qeng Database\n",
        )
        .unwrap();
        std::fs::write(servers_dir.join(servers::COLLECT_SCRIPT), collector).unwrap();
    }

    /// Fake collector: `web-1` is reachable, everything else is offline.
    const FAKE_COLLECTOR: &str = r#"if [ "$1" = web-1 ]; then
  echo '{"id":"web-1","reachable":true,"duration_ms":5,"data":{"system":{"hostname":"web"},"cpu":{"usage_percent":50.0},"memory":{"used_percent":85.0},"disks":[{"mount":"/","used_percent":10.0}],"unavailable":["temperatures"]}}'
else
  echo '{"id":"'"$1"'","reachable":false,"duration_ms":8000,"exit_code":255,"error":"Connection timed out"}'
fi"#;

    /// Polls `GET /servers` until the latest run has finished (at most ~10s).
    async fn wait_for_job(app: Router) -> String {
        for _ in 0..200 {
            let (_, body) = send(app.clone(), "GET", "/servers", Some(TOKEN)).await;
            if body.contains(r#""running":false"#) {
                return body;
            }
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(50))
            })
            .await
            .unwrap();
        }
        panic!("collection did not finish");
    }

    #[tokio::test]
    async fn servers_requires_token() {
        let (app, _dir) = test_app(&[]);
        for (method, uri) in [
            ("GET", "/servers"),
            ("POST", "/servers/refresh"),
            ("GET", "/servers/web-1"),
            ("POST", "/servers/web-1/refresh"),
        ] {
            let (status, _) = send(app.clone(), method, uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn servers_lists_inventory_before_collection() {
        let (app, dir) = test_app(&[]);
        add_servers(&dir, FAKE_COLLECTOR);
        let (status, body) = send(app, "GET", "/servers", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(
                r#""id":"web-1","name":"Web One","host":"10.0.0.1","port":22,"status":"unknown""#
            ),
            "{body}"
        );
        assert!(body.contains(r#""job":null"#) && body.contains(r#""warning_percent":80.0"#));
        assert!(!body.contains("qeng"), "ssh user is not exposed");
    }

    #[tokio::test]
    async fn servers_without_inventory_is_500() {
        let (app, _dir) = test_app(&[]);
        let (status, _) = send(app, "GET", "/servers", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn server_ids_are_validated() {
        let (app, dir) = test_app(&[]);
        add_servers(&dir, FAKE_COLLECTOR);
        let (status, _) = send(app.clone(), "GET", "/servers/Bad%20Id", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            app.clone(),
            "POST",
            "/servers/..%2Fetc/refresh",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(app, "POST", "/servers/other-1/refresh", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_all_collects_in_background() {
        let (app, dir) = test_app(&[]);
        add_servers(&dir, FAKE_COLLECTOR);
        let (status, body) = send(app.clone(), "POST", "/servers/refresh", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains(r#""total":2"#), "{body}");

        let body = wait_for_job(app.clone()).await;
        assert!(body.contains(r#""done":2"#), "{body}");
        assert!(
            body.contains(r#""status":"online""#) && body.contains(r#""status":"offline""#),
            "{body}"
        );
        assert!(body.contains(r#""level":"warning""#) && body.contains("Connection timed out"));

        let (status, body) = send(app, "GET", "/servers/web-1", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#""data":{"#) && body.contains(r#""unavailable":["temperatures"]"#),
            "{body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_one_server() {
        let (app, dir) = test_app(&[]);
        add_servers(&dir, FAKE_COLLECTOR);
        let (status, body) = send(app.clone(), "POST", "/servers/db-1/refresh", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains(r#""collecting":true"#), "{body}");
        for _ in 0..200 {
            let (_, body) = send(app.clone(), "GET", "/servers/db-1", Some(TOKEN)).await;
            if body.contains(r#""collecting":false"#) {
                assert!(
                    body.contains(r#""status":"offline""#) && body.contains(r#""stale":false"#),
                    "{body}"
                );
                return;
            }
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(50))
            })
            .await
            .unwrap();
        }
        panic!("collection did not finish");
    }

    /// Fake collector with a container, a failed unit and the detail modes.
    const DETAIL_COLLECTOR: &str = r#"case "$2" in
  container) echo '{"id":"web-1","reachable":true,"data":{"name":"'"$3"'","state":{"status":"running"}}}' ;;
  unit) echo '{"id":"web-1","reachable":true,"data":{"unit":"'"$3"'","logs_available":true,"logs":["boom"]}}' ;;
  *) echo '{"id":"web-1","reachable":true,"duration_ms":5,"data":{"cpu":{"usage_percent":95.0},"docker":{"containers":[{"id":"abc123","name":"web.1"}]},"services":{"running":[],"failed":[{"unit":"bad.service"}]},"unavailable":[]}}' ;;
esac"#;

    /// Router whose servers settings allow back-to-back refreshes unless `min_interval` is set.
    fn servers_app(collector: &str, min_interval: u64) -> (Router, TestDir) {
        let (mut state, dir) = test_state(&[]);
        add_servers(&dir, collector);
        let mut config = (*state.config).clone();
        config.servers.refresh_min_interval_secs = min_interval;
        state = AppState::new(config);
        (app(state), dir)
    }

    #[tokio::test]
    async fn new_servers_routes_require_token() {
        let (app, _dir) = test_app(&[]);
        for uri in [
            "/servers/alerts",
            "/servers/audit",
            "/servers/web-1/history",
            "/servers/web-1/containers/web.1",
            "/servers/web-1/units/bad.service",
        ] {
            let (status, _) = send(app.clone(), "GET", uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_is_rate_limited_and_audited() {
        let (app, _dir) = servers_app(FAKE_COLLECTOR, 60);
        let (status, _) = send(app.clone(), "POST", "/servers/refresh", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        wait_for_job(app.clone()).await;
        let req = Request::builder()
            .method("POST")
            .uri("/servers/refresh")
            .header(auth::TOKEN_HEADER, TOKEN)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry: u64 = res.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=60).contains(&retry));
        let (status, _) = send(app.clone(), "POST", "/servers/web-1/refresh", Some(TOKEN)).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "per-server limit too"
        );

        let (status, body) = send(app, "GET", "/servers/audit", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#""result":"rate_limited""#) && body.contains(r#""result":"started""#),
            "{body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_and_alerts_after_collection() {
        let (app, _dir) = servers_app(DETAIL_COLLECTOR, 0);
        send(app.clone(), "POST", "/servers/refresh", Some(TOKEN)).await;
        let body = wait_for_job(app.clone()).await;
        assert!(body.contains(r#""recent":["#), "{body}");
        let (status, body) = send(
            app.clone(),
            "GET",
            "/servers/web-1/history?hours=6",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#""hours":6"#) && body.contains(r#""cpu":95.0"#),
            "{body}"
        );
        let (_, body) = send(app.clone(), "GET", "/servers/alerts", Some(TOKEN)).await;
        assert!(
            body.contains(r#""kind":"critical""#)
                && body.contains(r#""delivery":"not_configured""#),
            "{body}"
        );
        let (status, _) = send(app, "GET", "/servers/nope-1/history", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detail_lookups_only_for_collected_targets() {
        let (app, _dir) = servers_app(DETAIL_COLLECTOR, 0);
        let (status, _) = send(
            app.clone(),
            "GET",
            "/servers/web-1/containers/web.1",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "nothing collected yet");
        send(app.clone(), "POST", "/servers/refresh", Some(TOKEN)).await;
        wait_for_job(app.clone()).await;

        let (status, body) = send(
            app.clone(),
            "GET",
            "/servers/web-1/containers/web.1",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""name":"web.1""#), "{body}");
        let (status, body) = send(
            app.clone(),
            "GET",
            "/servers/web-1/units/bad.service",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""logs":["boom"]"#), "{body}");
        for (uri, expected) in [
            ("/servers/web-1/containers/other", StatusCode::NOT_FOUND),
            ("/servers/web-1/units/ssh.service", StatusCode::NOT_FOUND),
            ("/servers/web-1/containers/%24(id)", StatusCode::BAD_REQUEST),
            ("/servers/web-1/units/x.socket", StatusCode::BAD_REQUEST),
        ] {
            let (status, _) = send(app.clone(), "GET", uri, Some(TOKEN)).await;
            assert_eq!(status, expected, "{uri}");
        }
        let (_, body) = send(app, "GET", "/servers/audit", Some(TOKEN)).await;
        assert!(
            body.contains(r#""action":"container_detail""#)
                && body.contains(r#""action":"unit_detail""#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn servers_dashboard_is_public_html() {
        let (app, _dir) = test_app(&[]);
        let (status, body) = send(app, "GET", "/servers/dashboard", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Service Maintainer · Servers"));
    }
}
