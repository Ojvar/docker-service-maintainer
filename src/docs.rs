//! OpenAPI docs generated from the handlers with `utoipa`.
//!
//! Swagger UI is served at `/api` and the JSON spec at `/api/openapi.json`.
//! Both are public; click "Authorize" in Swagger UI to send `x-security-token`.

use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};
use utoipa_swagger_ui::SwaggerUi;

use crate::{auth::TOKEN_HEADER, scripts, status};

/// The API description, collected from the `#[utoipa::path]` annotations.
#[derive(OpenApi)]
#[openapi(
    info(title = "service-maintainer", description = "Runs per-service bash scripts and reports Docker service status."),
    paths(scripts::run_service_script, status::services_status),
    components(schemas(scripts::RunResult)),
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
pub fn swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api").url("/api/openapi.json", ApiDoc::openapi())
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
}
