//! Shared daemon protocol helpers used by both OpenAI and native adapters.

use oc_proto::{
    ClientKind, ConnectParams, Frame, IdemKey, Method, MethodOk, Req, ReqId, ResResult, PROTO_VERSION,
};

use crate::conn_pool::NdjsonConn;
use crate::error::{HttpError, HttpResult};

/// Perform the `connect` handshake on a fresh connection.
pub(crate) async fn handshake(conn: &mut NdjsonConn) -> HttpResult<()> {
    send_req(conn, Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Detached }), None).await?;
    match await_res(conn).await? {
        MethodOk::Hello { .. } => Ok(()),
        other => Err(HttpError::Protocol(format!("expected hello, got {other:?}"))),
    }
}

/// Send a `Req` frame with an optional idempotency key.
pub(crate) async fn send_req(conn: &mut NdjsonConn, method: Method, idempotency_key: Option<String>) -> HttpResult<()> {
    let frame = Frame::Req(Req {
        id: ReqId::new(uuid::Uuid::now_v7().to_string()),
        method,
        idempotency_key: idempotency_key.map(IdemKey::new),
    });
    conn.tx.send(frame).await.map_err(|_| HttpError::Protocol("daemon connection closed".into()))
}

/// Read frames until the next `Res`, returning its payload. Interleaved events are dropped.
pub(crate) async fn await_res(conn: &mut NdjsonConn) -> HttpResult<MethodOk> {
    loop {
        match conn.rx.recv().await {
            Some(Frame::Res(res)) => return match res.result {
                ResResult::Ok(ok) => Ok(ok),
                ResResult::Err(e) => Err(HttpError::from_proto(e)),
            },
            Some(_) => continue,
            None => return Err(HttpError::Protocol("daemon closed connection before responding".into())),
        }
    }
}
