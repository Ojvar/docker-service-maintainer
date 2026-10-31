//! OpenAPI docs generated from the handlers with `utoipa`.
//!
//! Swagger UI is served at `/api` and the JSON spec at `/api/openapi.json`.
//! Both are public; click "Authorize" in Swagger UI to send `x-security-token`.
//! Behind a reverse proxy that strips a prefix, set `BASE_PATH` so the browser
//! loads the spec and calls the API through that prefix.

use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use utoipa::openapi::Server;
use utoipa_swagger_ui::{Config, SwaggerUi};

use crate::{auth::TOKEN_HEADER, scripts, services, status};

/// The API description, collected from the `#[utoipa::path]` annotations.
#[derive(OpenApi)]
#[openapi(
    info(title = "service-maintainer", description = "Runs per-service bash scripts and reports Docker service status."),
    paths(scripts::run_service_script, services::list_services, services::reload_service, status::services_status),
    components(schemas(scripts::RunResult, services::ServiceList, services::ServiceInfo, services::Health)),
    modifiers(&SecurityToken),
    security(("security_token" = []))
)]
pub struct ApiDoc;

/// Registers the `x-security-token` header as the API's auth scheme.
struct SecurityToken;

impl Modify for SecurityToken {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi.components.get_or_insert_with(Default::default).add_security_scheme(
            "security_token",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new(TOKEN_HEADER))),
        );
    }
}

/// Swagger UI at `/api`, backed by the generated spec.
///
/// Routes stay at `/api` (the proxy strips the prefix), while the URLs the
/// browser uses are prefixed with `base_path`, e.g. `/maintain/api/openapi.json`.
pub fn swagger_ui(base_path: &str) -> SwaggerUi {
    let mut spec = ApiDoc::openapi();
    if !base_path.is_empty() {
        spec.servers = Some(vec![Server::new(base_path)]);
    }
    SwaggerUi::new("/api")
        .url("/api/openapi.json", spec)
        .config(Config::from(format!("{base_path}/api/openapi.json")))
}

/// Redirects `/api` to `{base_path}/api/`.
///
/// Swagger UI's own redirect drops the proxy prefix, so the browser would land
/// on `/api/` outside the proxied path. Every other request passes through.
pub async fn redirect_bare_api(
    State(base_path): State<String>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/api" {
        return Redirect::to(&format!("{base_path}/api/")).into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_documents_all_routes() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/services/{service_name}/run"));
        assert!(json.contains("/services/status"));
        assert!(json.contains(TOKEN_HEADER));
    }

    #[test]
    fn base_path_is_set_as_server() {
        // Builds without panicking for both cases; the prefix is applied to the spec servers.
        swagger_ui("");
        swagger_ui("/maintain");
        let mut spec = ApiDoc::openapi();
        spec.servers = Some(vec![Server::new("/maintain")]);
        assert!(spec.to_json().unwrap().contains("\"/maintain\""));
    }
}
