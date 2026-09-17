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
    Box::pin(stream_native_after(conn, run_id, session, Vec::new(), true))
}

/// [`stream_native`] with an optional `head` of already-buffered `Frame`s.
///
/// The resume path reads frames off `conn.rx` until `Res(ChatResume)`, buffering
/// any `Frame::Event` it meets along the way — those are the replayed (pre-refresh)
/// events the daemon flushed *before* the `Res`. They must be yielded first, or
/// the refresh loses the text streamed before the disconnect.
fn stream_native_after(
    conn: NdjsonConn,
    run_id: RunId,
    session: SessionId,
    head: Vec<Frame>,
    emit_accepted: bool,
) -> Pin<Box<dyn Stream<Item = HttpResult<sse::Event>> + Send>> {
    Box::pin(async_stream::stream! {
        let mut conn = conn;

        // Emitted before any daemon event so the client can abort immediately.
        if emit_accepted {
            yield Ok(sse::Event::default()
                .event("accepted")
                .data(serde_json::json!({ "run_id": run_id.as_str(), "session": session.as_str() }).to_string()));
        }

        for frame in head {
            if let Frame::Event(ev) = frame {
                if let Some((event, terminal)) = sse_for_event(&ev, &run_id, &session) {
                    yield Ok(event);
                    if terminal {
                        return;
                    }
                }
            }
        }

        loop {
            match conn.rx.recv().await {
                Some(Frame::Event(ev)) => {
                    let Some((event, terminal)) = sse_for_event(&ev, &run_id, &session) else {
                        continue;
                    };
                    yield Ok(event);
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

/// Whether an event is a run's terminal lifecycle.
fn is_terminal(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Lifecycle { phase: LifecyclePhase::End | LifecyclePhase::Error { .. }, .. }
    )
}

/// Serialize a run event into its SSE frame, if it belongs to this run/session.
/// Returns `(event, terminal)`; `None` if the event is filtered out or fails to
/// serialize.
fn sse_for_event(ev: &Event, run_id: &RunId, session: &SessionId) -> Option<(sse::Event, bool)> {
    if !belongs(ev, run_id, session) {
        return None;
    }
    let terminal = is_terminal(ev);
    match serde_json::to_string(ev) {
        Ok(json) => Some((sse::Event::default().event(event_name(ev)).data(json), terminal)),
        Err(e) => {
            tracing::warn!(error = %e, "native sse serialize failed");
            None
        }
    }
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
        Method::ChatResume(ChatResumeParams { session: session.clone(), run_id: run_id.clone(), since_seq: 0 }),
        None,
    )
    .await?;

    // 不能盲用 `await_res`：SessionCmd::Resume 在 `reply.send(hit)` 之前就 spawn 了
    // 回放任务，回放的 Event 会先于 Res(ChatResume) 到达本连接。await_res 会把
    // 这些交错事件丢掉——刷新前已 stream 的文本就此无声丢失。改成读到
    // Res(ChatResume) 为止，途中遇到的 Event 缓存下来，先于续流吐出。
    let mut buffered: Vec<Frame> = Vec::new();
    loop {
        match conn.rx.recv().await {
            Some(Frame::Res(res)) => match res.result {
                oc_proto::ResResult::Ok(MethodOk::ChatResume { .. }) => break,
                oc_proto::ResResult::Ok(other) => {
                    return Err(HttpError::Protocol(format!(
                        "expected chat_resume ok, got {other:?}"
                    )))
                }
                oc_proto::ResResult::Err(e) => return Err(HttpError::from_proto(e)),
            },
            Some(Frame::Event(ev)) => buffered.push(Frame::Event(ev)),
            Some(_) => {}
            None => {
                return Err(HttpError::Protocol("daemon closed before resume ok".into()))
            }
        }
    }

    // 复用 stream_native_after：先吐回放的 buffered 事件，再按 run_id 过滤续流，
    // Lifecycle::End/Error 终态停。resume 无 accepted 事件（run_id 由调用方已知）。
    let stream = stream_native_after(conn, run_id, session, buffered, false);
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
