//! HTTP server for oh-my-claw.
//!
//! Hosts two independent route sets on the same port and connection pool:
//!
//! - **OpenAI Responses API** (`/v1/responses`): compatibility layer for
//!   external tools built against the OpenAI SDK.
//!
//! - **Native API** (`/api/v1/*`): thin REST+SSE wrapper over `oc-proto`,
//!   for the Web UI and any client that wants direct access to daemon
//!   concepts (sessions, approval, user-reply, etc.).
//!
//! - **Web UI** (`/`): embedded Vue bundle, served as static assets.
//!
//! All three share one [`conn_pool::ConnPool`] and one [`server::AppState`].

pub mod types;
pub mod adapter;
pub mod server;
pub mod error;
pub mod sse;
pub mod conn_pool;
pub mod proto;
pub mod native;
pub mod auth;
pub mod assets;

pub use error::{HttpError, HttpResult};

use axum::{middleware, routing::get, routing::post};
use tower_http::cors::CorsLayer;

/// Configuration for the full HTTP app.
pub struct AppConfig {
    /// Bearer token required for `/api/v1/*` and `/v1/*`.
    /// `None` means no auth (loopback-only mode).
    pub token: Option<String>,
    /// Serve Web UI at `/`. Pass `false` for `--no-web`.
    pub web_ui: bool,
}

/// Assemble the full axum app: OpenAI compat + native API + optional Web UI.
pub fn create_app(pool: conn_pool::ConnPool, default_model: String, cfg: AppConfig) -> axum::Router {
    let state = server::AppState::new(pool, default_model, cfg.token.clone());

    let mut router = axum::Router::new()
        .merge(server::routes())
        .merge(native::routes())
        .route("/auth/login", post(auth::login))
        .route("/health", get(health));

    if cfg.web_ui {
        router = router.merge(assets::routes());
    }

    // The auth middleware reads `token` + the login-session table off AppState,
    // so it's wired with the same state the routes use.
    router
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_token))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

