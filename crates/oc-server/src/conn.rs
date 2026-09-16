//! 单连接处理（设计 §7.6 背压 / §2.5 传输）。
//!
//! 每连接三条逻辑：
//! - **读**：收 req → dispatch → 把 res 送入出站队列
//! - **事件转发**：订阅 EventBus → 把 event 送入出站队列
//! - **写**：单一写任务从出站队列取帧写出（串行化，避免交错）
//!
//! 出站队列有界；慢 client 的事件转发在 `Lagged` 时提示重连（不阻塞 run）。

use std::sync::Arc;

use oc_proto::{Frame, Method, Res};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::debug;

use crate::codec::{FrameReader, FrameWriter};
use crate::dispatch;
use crate::error::ServerResult;
use crate::state::ServerState;
use crate::transport::Conn;

/// 出站队列容量。
const OUTBOUND_CAP: usize = 256;

pub async fn handle(conn: Conn, state: Arc<ServerState>) -> ServerResult<()> {
    let (read_half, write_half) = tokio::io::split(conn);
    let mut reader = FrameReader::new(read_half);

    // 出站帧队列：res 与 event 都经此串行写出。
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(OUTBOUND_CAP);

    // 写任务。
    let writer_handle = tokio::spawn(async move {
        let mut writer = FrameWriter::new(write_half);
        while let Some(frame) = out_rx.recv().await {
            if writer.write_frame(&frame).await.is_err() {
                break; // 对端断开，结束写任务。
            }
        }
    });

    // 事件转发任务。
    let mut event_rx = state.subscribe();
    let event_out = out_tx.clone();
    let event_handle = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(ev) => {
                    if event_out.send(Frame::Event(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    debug!(dropped = n, "事件订阅滞后，client 应重新 status 拉快照");
                    // 继续；不阻塞。
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 读循环。
    let read_result = read_loop(&mut reader, &state, &out_tx).await;

    // 收尾：关掉本连接持有的出站发送端。
    drop(out_tx);
    event_handle.abort();
    // 关键：不能 `await` 写任务排空——活跃 run 可能仍持有 `out_tx` 的 clone
    // （RunSink::Conn），只要 run 未收敛，`out_rx` 就不会关闭，写任务会永久挂在
    // `recv().await` 上，这里的 await 也随之卡死（表现：断连后车道迟迟不释放，
    // 直到空闲看门狗兜底）。改为主动 abort：写任务的 `write_half` 立即 drop，
    // run 侧的 `out_tx.closed()` 随即完成 → 静默等待（ask_user/审批）立即收敛。
    writer_handle.abort();
    let _ = writer_handle.await;
    read_result
}

async fn read_loop<R>(
    reader: &mut FrameReader<R>,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
) -> ServerResult<()>
where
    R: AsyncReadExt + Unpin,
{
    // 连接级客户端类型：Connect 握手帧确定，此后本连接所有 chat.send 沿用。
    // 缺省 Interactive——旧客户端或漏发字段时保持原有断连语义。
    let mut client_kind = oc_proto::ClientKind::Interactive;
    while let Some(frame) = reader.read_frame().await? {
        match frame {
            Frame::Req(req) => {
                if let Method::Connect(p) = &req.method {
                    client_kind = p.client_kind;
                }
                let result =
                    dispatch::handle_req(&req, state, out_tx, client_kind).await;
                let res = Res {
                    id: req.id,
                    result,
                };
                // 出站满则 client 太慢，直接断开该连接。
                if out_tx.send(Frame::Res(res)).await.is_err() {
                    break;
                }
            }
            // server 不处理来自 client 的 res/event。
            Frame::Res(_) | Frame::Event(_) => {
                debug!("忽略来自 client 的非请求帧");
            }
        }
    }
    Ok(())
}
