//! P0-1 回归：>256 个 delta 的长回复经真实连接完整送达，不因广播溢出而截断。
//!
//! 旧设计所有 run 事件走一条 cap=256 的共享 broadcast，长回复的 delta 超过 256
//! 且消费稍慢即 `Lagged`，中间段被覆盖丢失（表现为「截断」）。改用 per-run
//! 背压 sink（`RunSink::Conn`）后，慢消费只会背压、不丢字。
//!
//! 本测试构造 400 个文本 delta（远超 256），经真实传输逐帧读取，断言累计文本
//! 长度与内容完整。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_proto::{
    ChatSendParams, ClientKind, ConnectParams, Frame, LifecyclePhase, Method, Req, ReqId, ResResult,
    PROTO_VERSION,
};
use oc_server::TransportKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// delta 条数（远超旧 broadcast 容量 256）。
const N_DELTAS: usize = 400;

async fn connect(kind: &TransportKind) -> Box<dyn ClientStream> {
    match kind {
        #[cfg(unix)]
        TransportKind::Unix(path) => {
            let s = tokio::net::UnixStream::connect(path).await.expect("connect unix");
            Box::new(s)
        }
        #[cfg(windows)]
        TransportKind::Pipe(name) => {
            use tokio::net::windows::named_pipe::ClientOptions;
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(name.as_str()) {
                    Ok(c) => return Box::new(c),
                    Err(_) if attempts < 20 => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(e) => panic!("connect pipe: {e}"),
                }
            }
        }
        #[allow(unreachable_patterns)]
        _ => panic!("本平台不支持该传输"),
    }
}

trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ClientStream for T {}

fn test_transport(tag: &str) -> TransportKind {
    #[cfg(windows)]
    {
        TransportKind::Pipe(format!(r"\\.\pipe\oc-test-{}-{tag}", std::process::id()))
    }
    #[cfg(not(windows))]
    {
        let dir = std::env::temp_dir().join(format!("oc-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TransportKind::Unix(dir.join("oc.sock"))
    }
}

#[tokio::test]
async fn long_reply_not_truncated() {
    let kind = test_transport("longreply");

    // 400 个文本 delta，每个 "tok{i};"，末尾 Done。
    let mut script: Vec<ScriptStep> = (0..N_DELTAS)
        .map(|i| ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Text(format!("[{i}]")),
        })
        .collect();
    script.push(ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) });

    let server_kind = kind.clone();
    let server = tokio::spawn(async move {
        let provider = Arc::new(MockProvider::scripted(script));
        let cfg = oc_server::testing::test_cfg();
        let _ = oc_server::serve_with(
            server_kind,
            provider,
            cfg,
            Duration::from_secs(60),
            oc_store::Store::open_memory().unwrap(),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    let stream = connect(&kind).await;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    // connect → hello
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("c0"),
        method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Interactive }),
        idempotency_key: None,
    })).await;
    let _ = recv(&mut reader).await;

    // chat.send
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("m1"),
        method: Method::ChatSend(ChatSendParams { session: None, text: "长回复".into() }),
        idempotency_key: None,
    })).await;

    // 累积所有 assistant delta，直到 lifecycle end。
    let mut acc = String::new();
    let mut got_end = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let f = tokio::time::timeout(Duration::from_secs(5), recv(&mut reader))
            .await
            .expect("不应超时");
        match f {
            Frame::Event(oc_proto::Event::Assistant { delta, .. }) => acc.push_str(&delta),
            Frame::Event(oc_proto::Event::Lifecycle { phase: LifecyclePhase::End, .. }) => {
                got_end = true;
                break;
            }
            Frame::Event(oc_proto::Event::Lifecycle { phase: LifecyclePhase::Error { message, .. }, .. }) => {
                panic!("run 异常终态: {message}");
            }
            Frame::Res(res) if res.id.as_str() == "m1" => {
                assert!(matches!(res.result, ResResult::Ok(_)));
            }
            _ => {}
        }
    }

    assert!(got_end, "应收到 lifecycle end");
    // 完整性断言：每个 delta 都在，且顺序正确。
    let expected: String = (0..N_DELTAS).map(|i| format!("[{i}]")).collect();
    assert_eq!(acc, expected, "长回复应完整无缺、顺序正确（P0-1）");

    server.abort();
}

async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) {
    let mut s = serde_json::to_string(frame).unwrap();
    s.push('\n');
    w.write_all(s.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

async fn recv<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Frame {
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).await.unwrap();
        assert!(n > 0, "对端关闭");
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        return serde_json::from_str(t).unwrap();
    }
}
