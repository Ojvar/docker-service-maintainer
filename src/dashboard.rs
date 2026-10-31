//! Admin dashboard page (`GET /dashboard`).
//!
//! The page itself is public static HTML/CSS/JS with no data in it; it asks
//! for the security token and calls the protected JSON API from the browser.

use axum::response::Html;

/// Dashboard page, embedded at compile time so the binary needs no runtime files.
const DASHBOARD_HTML: &str = include_str!("../templates/dashboard.html");

/// `GET /dashboard` — serves the admin dashboard.
pub async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}
