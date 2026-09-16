//! Chat endpoints: send (SSE streaming), abort, and the two reply kinds.

use std::pin::Pin;

use axum::{
    extract::State as AxumState,
    response::{sse, IntoResponse, Response as AxumResponse, Sse},
    Json,
};
use futures_util::Stream;
use oc_proto::{
    ApprovalReplyParams, ChatAbortParams, ChatResumeParams, ChatSendParams, Event, Frame,
    LifecyclePhase, Method, MethodOk, RunId, SessionId, UserReplyParams,
};
use serde::Deserialize;

use crate::conn_pool::NdjsonConn;
use crate::error::{HttpError, HttpResult};
use crate::proto::{await_res, handshake, send_req};
use crate::server::AppState;

#[derive(Deserialize)]
pub struct SendReq {
    #[serde(default)]
    pub session: Option<String>,
    pub text: String,
}

#[derive(Deserialize)]
pub struct AbortReq {
    pub run_id: String,
    #[serde(default)]
    pub hard: bool,
}

#[derive(Deserialize)]
pub struct ResumeQuery {
    pub run_id: String,
    #[serde(default)]
    pub session: Option<String>,
}

/// POST /api/v1/chat/send  →  SSE stream of native `Event`s.
///
/// The response streams rather than returning `{run_id}` and letting the client
/// pick events up on `GET /events`: inline run events are delivered only to the
/// connection that issued `chat.send` (see this module's parent docs), so the
/// turn and its reply are inseparable.
///
/// The `run_id` is still available immediately — it is sent as the first SSE
/// event (`event: accepted`), so a client can abort a turn it has just started.
pub async fn send(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<SendReq>,
) -> HttpResult<AxumResponse> {
    if req.text.trim().is_empty() {
        return Err(HttpError::BadRequest("text must not be empty".into()));
    }
    let session = req
        .session
        .as_deref()
        .map(crate::adapter::validate_session_key)
        .transpose()?
        .unwrap_or_else(SessionId::main);

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::ChatSend(ChatSendParams { session: Some(session.clone()), text: req.text }),
        Some(uuid::Uuid::now_v7().to_string()),
    )
    .await?;

    let run_id = match await_res(&mut conn).await? {
        MethodOk::ChatSend { run_id } => run_id,
        other => return Err(HttpError::Protocol(format!("expected chat_send ok, got {other:?}"))),
    };

    let stream = stream_native(conn, run_id, session);
    Ok(Sse::new(stream)
        .keep_alive(sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response())
}

/// Relay this run's events verbatim until it reaches a terminal phase.
///
/// Takes `conn` by value so the pool permit is released when axum drops the
/// stream — including on client disconnect. This path never calls
/// `ConnPool::release`; the drop is what returns the permit.
fn stream_native(
    conn: NdjsonConn,
    run_id: RunId,
    session: SessionId,
) -> Pin<Box<dyn Stream<Item = HttpResult<sse::Event>> + Send>> {
    Box::pin(async_stream::stream! {
        let mut conn = conn;

        // Emitted before any daemon event so the client can abort immediately.
        yield Ok(sse::Event::default()
            .event("accepted")
            .data(serde_json::json!({ "run_id": run_id.as_str(), "session": session.as_str() }).to_string()));

        loop {
            match conn.rx.recv().await {
                Some(Frame::Event(ev)) => {
                    if !belongs(&ev, &run_id, &session) {
                        continue;
                    }
                    let terminal = matches!(
                        &ev,
                        Event::Lifecycle { phase: LifecyclePhase::End | LifecyclePhase::Error { .. }, .. }
                    );
                    match serde_json::to_string(&ev) {
                        Ok(json) => yield Ok(sse::Event::default().event(event_name(&ev)).data(json)),
                        Err(e) => tracing::warn!(error = %e, "native sse serialize failed"),
                    }
                    if terminal {
                        break;
                    }
                }
                Some(_) => {}
                None => {
                    yield Err(HttpError::Protocol("daemon closed connection mid-stream".into()));
                    break;
                }
            }
        }
    })
}

/// Whether an event belongs to this run.
///
/// Run-scoped events are matched on `run_id`. `Usage` carries session scope only
/// (no run id), so it is matched on session — the finest scope the protocol
/// offers. `Proactive` and `Task` are ambient and belong on `GET /events`.
fn belongs(ev: &Event, run_id: &RunId, session: &SessionId) -> bool {
    match ev {
        Event::Lifecycle { run_id: r, .. }
        | Event::Assistant { run_id: r, .. }
        | Event::Reasoning { run_id: r, .. }
        | Event::Tool { run_id: r, .. }
        | Event::Approval { run_id: r, .. }
        | Event::UserInput { run_id: r, .. } => r == run_id,
        Event::Usage { session: s, .. } => s == session,
        Event::Proactive { .. } | Event::Task { .. } => false,
    }
}

/// SSE `event:` name for an event, matching its `oc-proto` tag.
pub(crate) fn event_name(ev: &Event) -> &'static str {
    match ev {
        Event::Lifecycle { .. } => "lifecycle",
        Event::Assistant { .. } => "assistant",
        Event::Reasoning { .. } => "reasoning",
        Event::Tool { .. } => "tool",
        Event::Proactive { .. } => "proactive",
        Event::Task { .. } => "task",
        Event::Usage { .. } => "usage",
        Event::Approval { .. } => "approval",
        Event::UserInput { .. } => "user_input",
    }
}

/// POST /api/v1/chat/abort
pub async fn abort(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<AbortReq>,
) -> HttpResult<impl IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::ChatAbort(ChatAbortParams { run_id: RunId::new(req.run_id), hard: req.hard }),
        None,
    )
    .await?;
    await_res(&mut conn).await?;
    state.pool.release(conn).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/v1/chat/resume → SSE：接续一个在途 Detached run 的剩余内联事件流。
pub async fn resume(
    AxumState(state): AxumState<AppState>,
    axum::extract::Query(q): axum::extract::Query<ResumeQuery>,
) -> HttpResult<AxumResponse> {
    let session = q
        .session
        .as_deref()
        .map(crate::adapter::validate_session_key)
        .transpose()?
        .unwrap_or_else(SessionId::main);
    let run_id = RunId::new(q.run_id);

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::ChatResume(ChatResumeParams { session: session.clone(), run_id: run_id.clone() }),
        None,
    )
    .await?;
    match await_res(&mut conn).await? {
        MethodOk::ChatResume { .. } => {}
        other => return Err(HttpError::Protocol(format!("expected chat_resume ok, got {other:?}"))),
    }

    // 复用 stream_native：按 run_id 过滤，Lifecycle::End/Error 终态停。
    let stream = stream_native(conn, run_id, session);
    Ok(Sse::new(stream)
        .keep_alive(sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response())
}

#[derive(Deserialize)]
pub struct ApprovalReq {
    pub approval_id: String,
    pub allow: bool,
}

/// POST /api/v1/approval/reply
///
/// Resolves through daemon state rather than the originating connection, so it
/// works from any connection — the Web UI replies on a fresh request while the
/// run's own stream stays blocked waiting for it.
pub async fn approval_reply(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<ApprovalReq>,
) -> HttpResult<impl IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::ApprovalReply(ApprovalReplyParams {
            approval_id: oc_proto::ApprovalId::new(req.approval_id),
            allow: req.allow,
        }),
        None,
    )
    .await?;
    await_res(&mut conn).await?;
    state.pool.release(conn).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct UserReplyReq {
    pub input_id: String,
    #[serde(default)]
    pub text: Option<String>,
}

/// POST /api/v1/user/reply
pub async fn user_reply(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<UserReplyReq>,
) -> HttpResult<impl IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::UserReply(UserReplyParams {
            input_id: oc_proto::InputId::new(req.input_id),
            text: req.text,
        }),
        None,
    )
    .await?;
    await_res(&mut conn).await?;
    state.pool.release(conn).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
