//! Native REST + SSE API over `oc-proto`, for the Web UI and other first-class clients.
//!
//! Unlike the OpenAI compatibility layer (`server.rs`), this exposes daemon
//! concepts directly: sessions, transcript history, approval / user-input
//! replies, reset, compact. Payloads are `oc-proto` types serialized verbatim,
//! so the wire format tracks the protocol without a translation layer.
//!
//! # Why chat streaming rides the POST response
//!
//! oc-server routes a run's **inline** events (`Assistant`, `Lifecycle`,
//! `Tool`, `Approval`, `UserInput`) through `RunSink::Conn` — a bounded channel
//! wired to the connection that issued `chat.send` (see oc-server `sink.rs`).
//! They never reach the global broadcast. So a separate `GET /events` stream
//! *cannot* observe them; the reply must stream back on the same daemon
//! connection that sent the turn, which means the same HTTP response.
//!
//! `GET /api/v1/events` therefore carries only the broadcast events (`Usage`,
//! `Proactive`) — ambient state a client wants regardless of who sent a turn.

mod chat;
mod command;
mod events;
mod sessions;

use axum::{
    routing::{get, post},
    Router,
};

use crate::server::AppState;

/// All native API routes, mounted under `/api/v1`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/sessions", get(sessions::list))
        .route("/api/v1/sessions/:id/history", get(sessions::history))
        .route("/api/v1/sessions/:id/reset", post(sessions::reset))
        .route("/api/v1/sessions/:id/compact", post(sessions::compact))
        .route("/api/v1/chat/send", post(chat::send))
        .route("/api/v1/chat/resume", get(chat::resume))
        .route("/api/v1/chat/abort", post(chat::abort))
        .route("/api/v1/command", post(command::run))
        .route("/api/v1/approval/reply", post(chat::approval_reply))
        .route("/api/v1/user/reply", post(chat::user_reply))
        .route("/api/v1/status", get(sessions::status))
        .route("/api/v1/events", get(events::stream))
}
