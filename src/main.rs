mod auth;
mod config;
mod scripts;
mod status;

use std::sync::Arc;

use axum::{
    Router, middleware,
    routing::{get, post},
};

use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
}

#[tokio::main]
async fn main() {
    let config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("config error: {e}");
        std::process::exit(1);
    });

    let addr = format!("{}:{}", config.host, config.port);
    let state = AppState { config: Arc::new(config) };

    let app = Router::new()
        .route("/services/status", get(status::services_status))
        .route("/services/{service_name}/run", post(scripts::run_service_script))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_token))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        eprintln!("cannot bind {addr}: {e}");
        std::process::exit(1);
    });

    println!("service-maintainer listening on http://{addr}");
    axum::serve(listener, app).await.expect("server error");
}
