//! HTTP server: POST /v1/responses, POST /v1/responses/:id/cancel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, State as AxumState},
    http::HeaderMap,
    response::{IntoResponse, Response as AxumResponse, Sse},
    routing::post,
    Json, Router,
};
use dashmap::DashMap;

use oc_proto::{ChatAbortParams, ChatSendParams, Frame, Method, MethodOk, RunId, SessionId};

use crate::{
    adapter::{self, ResponseSessions},
    conn_pool::{ConnPool, NdjsonConn},
    error::{HttpError, HttpResult},
    proto::{await_res, handshake, send_req},
    sse::{stream_sse, SseState},
    types::*,
};

/// What a `response_id` maps to: the session it ran in, and its run id
/// (needed to target `chat.abort`).
#[derive(Clone)]
pub struct ResponseRecord {
    pub session: SessionId,
    pub run_id: RunId,
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) pool: ConnPool,
    /// response_id → session, for `previous_response_id` continuity.
    sessions: ResponseSessions,
    /// response_id → run record, for cancel.
    records: Arc<DashMap<String, ResponseRecord>>,
    default_model: String,
    /// Bearer token. `None` means loopback-only no-auth mode. Never injected
    /// into the Web UI anymore — the browser obtains access via `/auth/login`,
    /// which exchanges the token for an HttpOnly session cookie.
    pub(crate) token: Option<String>,
    /// login session id → creation time, backing the HttpOnly cookie auth.
    pub(crate) login_sessions: Arc<DashMap<String, Instant>>,
    /// Lifetime of a login session cookie.
    pub(crate) login_ttl: Duration,
}

impl AppState {
    pub fn new(pool: ConnPool, default_model: String, token: Option<String>) -> Self {
        Self {
            pool,
            sessions: Arc::new(DashMap::new()),
            records: Arc::new(DashMap::new()),
            default_model,
            token,
            login_sessions: Arc::new(DashMap::new()),
            login_ttl: crate::auth::LOGIN_TTL,
        }
    }
}

/// The OpenAI Responses API routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/responses", post(create_response))
        .route("/v1/responses/:id/cancel", post(cancel_response))
}

/// Read [`adapter::SESSION_KEY_HEADER`], if present.
///
/// A non-UTF8 header value is an error rather than being ignored: the caller
/// meant to route somewhere specific, and guessing would put the turn in the
/// wrong conversation.
fn extract_session_key(headers: &HeaderMap) -> HttpResult<Option<SessionId>> {
    let Some(raw) = headers.get(adapter::SESSION_KEY_HEADER) else {
        return Ok(None);
    };
    let value = raw.to_str().map_err(|_| {
        HttpError::BadRequest(format!(
            "{} must be valid UTF-8",
            adapter::SESSION_KEY_HEADER
        ))
    })?;
    adapter::validate_session_key(value).map(Some)
}

async fn create_response(
    AxumState(state): AxumState<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateResponseReq>,
) -> HttpResult<AxumResponse> {
    adapter::reject_unsupported(&req)?;
    adapter::validate_tools(&req.tools)?;

    let session_key = extract_session_key(&headers)?;
    let session_id = adapter::resolve_session(&req, session_key, &state.sessions);
    let extracted = adapter::extract_input(&req)?;

    // Per-request instructions and file content ride along as a turn prefix.
    let prefix = adapter::build_turn_prefix(&extracted);
    let text = format!("{prefix}{}", extracted.text);

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;

    send_req(
        &mut conn,
        Method::ChatSend(ChatSendParams {
            session: Some(session_id.clone()),
            text,
        }),
        Some(uuid::Uuid::now_v7().to_string()),
    )
    .await?;

    // The run id lets event filtering be exact rather than session-wide.
    let run_id = match await_res(&mut conn).await? {
        MethodOk::ChatSend { run_id } => run_id,
        other => {
            return Err(HttpError::Protocol(format!(
                "expected chat_send ok, got {other:?}"
            )))
        }
    };

    let response_id = format!("resp_{}", uuid::Uuid::now_v7());
    state
        .sessions
        .insert(response_id.clone(), session_id.clone());
    state.records.insert(
        response_id.clone(),
        ResponseRecord {
            session: session_id.clone(),
            run_id: run_id.clone(),
        },
    );

    let model = state.default_model.clone();
    let created_at = adapter::now_secs();

    if req.stream.unwrap_or(false) {
        let sse_state = SseState::new(response_id, run_id, session_id, model, created_at);
        let stream = stream_sse(conn, sse_state);
        Ok(Sse::new(stream)
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(std::time::Duration::from_secs(15)),
            )
            .into_response())
    } else {
        let response = accumulate_response(
            &mut conn,
            response_id,
            &run_id,
            &session_id,
            model,
            created_at,
        )
        .await?;
        // Connection is at rest again (run reached a terminal phase), so it is
        // safe to hand back to the pool.
        state.pool.release(conn).await;
        Ok(Json(response).into_response())
    }
}

/// Drain events until the run reaches a terminal phase, then build the Response.
async fn accumulate_response(
    conn: &mut NdjsonConn,
    response_id: String,
    run_id: &RunId,
    run_session: &SessionId,
    model: String,
    created_at: i64,
) -> HttpResult<Response> {
    use oc_proto::{Event, LifecyclePhase};

    let mut text = String::new();
    let mut input_tokens = 0u32;
    let mut status = ResponseStatus::Completed;
    let mut error = None;

    loop {
        match conn.rx.recv().await {
            Some(Frame::Event(Event::Lifecycle { run_id: rid, phase, .. })) if &rid == run_id => {
                match phase {
                    LifecyclePhase::Start => {}
                    LifecyclePhase::End => break,
                    LifecyclePhase::Error { message, kind } => {
                        status = match kind {
                            oc_proto::RunErrorKind::Aborted => ResponseStatus::Cancelled,
                            _ => ResponseStatus::Failed,
                        };
                        error = Some(ResponseError {
                            message,
                            code: error_code(kind).to_string(),
                        });
                        break;
                    }
                }
            }
            Some(Frame::Event(Event::Assistant { run_id: rid, delta, .. })) if &rid == run_id => {
                text.push_str(&delta);
            }
            // Filtered by session for the same reason as the SSE path: `Usage`
            // rides the daemon's global broadcast, so an unfiltered match takes
            // token counts from whatever run reported last — TUI, cron, or a
            // concurrent request. See `sse.rs` for why session is the finest
            // scope available.
            Some(Frame::Event(Event::Usage { session, input_tokens: n, .. }))
                if &session == run_session =>
            {
                input_tokens = n;
            }
            Some(_) => {}
            None => {
                return Err(HttpError::Protocol(
                    "daemon closed connection before the run finished".into(),
                ))
            }
        }
    }

    let output_tokens = estimate_tokens(&text);
    let output = if text.is_empty() {
        vec![]
    } else {
        vec![OutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::now_v7()),
            status: "completed".into(),
            role: "assistant".into(),
            content: vec![ContentPart::OutputText {
                text,
                annotations: vec![],
            }],
        }]
    };

    Ok(Response {
        id: response_id,
        object: "response".into(),
        created_at,
        status,
        completed_at: Some(adapter::now_secs()),
        error,
        output,
        usage: Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        },
        model,
    })
}

/// Rough token estimate for output. The daemon's `Usage` event reports input
/// tokens only, so output is approximated the same way oc-server does (chars/4)
/// rather than reported as 0.
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() / 4).max(if text.is_empty() { 0 } else { 1 })) as u32
}

fn error_code(kind: oc_proto::RunErrorKind) -> &'static str {
    use oc_proto::RunErrorKind as K;
    match kind {
        K::Aborted => "cancelled",
        K::Failed => "internal_error",
        K::Panicked => "internal_error",
        K::LoopDetected => "loop_detected",
        K::Timeout => "timeout",
        K::Truncated => "truncated",
    }
}

async fn cancel_response(
    AxumState(state): AxumState<AppState>,
    Path(response_id): Path<String>,
) -> HttpResult<AxumResponse> {
    let record = state
        .records
        .get(&response_id)
        .map(|e| e.clone())
        .ok_or_else(|| HttpError::NotFound(format!("response not found: {response_id}")))?;

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;

    send_req(
        &mut conn,
        Method::ChatAbort(ChatAbortParams {
            run_id: record.run_id,
            hard: true,
        }),
        None,
    )
    .await?;

    match await_res(&mut conn).await? {
        MethodOk::Empty => {
            state.pool.release(conn).await;
            Ok(Json(serde_json::json!({
                "id": response_id,
                "object": "response",
                "status": "cancelled",
            }))
            .into_response())
        }
        other => Err(HttpError::Protocol(format!(
            "expected empty ok, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn token_estimate_is_zero_only_for_empty() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1, "non-empty text is at least 1 token");
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(adapter::SESSION_KEY_HEADER, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn missing_session_key_header_is_none() {
        assert!(extract_session_key(&HeaderMap::new()).unwrap().is_none());
    }

    #[test]
    fn session_key_header_is_read_and_validated() {
        let got = extract_session_key(&headers_with("notes")).unwrap();
        assert_eq!(got, Some(SessionId::new("notes")));

        assert!(
            extract_session_key(&headers_with("cron:x")).is_err(),
            "reserved namespaces are rejected at the handler boundary too"
        );
    }

    #[test]
    fn non_utf8_session_key_header_is_rejected() {
        let mut h = HeaderMap::new();
        h.insert(
            adapter::SESSION_KEY_HEADER,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(extract_session_key(&h).is_err());
    }
}
