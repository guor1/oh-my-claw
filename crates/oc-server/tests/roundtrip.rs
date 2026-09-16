//! M2 端到端传输测试：起 server，经真实传输（unix socket / 命名管道）
//! 跑一次 connect + chat.send，验证 hello 应答与 echo 事件流。
//!
//! 双平台各验一次（cfg 分派 client 连接），对应 M2 硬验收。

use oc_proto::{
    ChatSendParams, ClientKind, ConnectParams, Frame, LifecyclePhase, Method, Req, ReqId, ResResult,
    PROTO_VERSION,
};
use oc_server::TransportKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 连到 server 的 client 流（按平台）。
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
            // 管道可能尚未就绪，重试几次。
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(name.as_str()) {
                    Ok(c) => return Box::new(c),
                    Err(_) if attempts < 20 => {
                        attempts += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(e) => panic!("connect pipe: {e}"),
                }
            }
        }
        #[allow(unreachable_patterns)]
        _ => panic!("本平台不支持该传输"),
    }
}

/// 抽象读写（unix stream / 命名管道 client 都实现 AsyncRead+AsyncWrite）。
trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ClientStream for T {}

/// 唯一传输端点。`tag` 用于区分同一进程内并发运行的多个测试，
/// 避免命名管道/socket 名冲突（`first_pipe_instance` 下第二个 server 绑定会失败）。
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
async fn connect_and_echo_roundtrip() {
    let kind = test_transport("echo");

    // 起 server（mock provider 回显含"你好"的文本，供断言）。
    let server_kind = kind.clone();
    let server = tokio::spawn(async move {
        use std::sync::Arc;
        use std::time::Duration;
        let provider = Arc::new(oc_llm::mock::MockProvider::echo_text("你好，我在。"));
        let cfg = oc_server::testing::test_cfg();
        let _ = oc_server::serve_with(server_kind, provider, cfg, Duration::from_secs(60), oc_store::Store::open_memory().unwrap()).await;
    });

    // 等 listener 就绪。
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let stream = connect(&kind).await;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    // 1) connect → hello
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("c0"),
        method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Interactive }),
        idempotency_key: None,
    })).await;

    let hello = recv(&mut reader).await;
    match hello {
        Frame::Res(res) => {
            assert_eq!(res.id.as_str(), "c0");
            assert!(matches!(res.result, ResResult::Ok(_)), "connect 应成功");
        }
        other => panic!("期望 Res(hello)，得到 {other:?}"),
    }

    // 2) chat.send → run_id 应答 + echo 事件流
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("m1"),
        method: Method::ChatSend(ChatSendParams { session: None, text: "你好".into() }),
        idempotency_key: Some(oc_proto::IdemKey::new("k1")),
    })).await;

    // 收集若干帧，找 run_id 应答、assistant echo、lifecycle end。
    let mut got_run_id = false;
    let mut got_echo = false;
    let mut got_end = false;

    for _ in 0..6 {
        let f = tokio::time::timeout(std::time::Duration::from_secs(2), recv(&mut reader))
            .await
            .expect("不应超时");
        match f {
            Frame::Res(res) if res.id.as_str() == "m1" => {
                assert!(matches!(res.result, ResResult::Ok(_)));
                got_run_id = true;
            }
            Frame::Event(oc_proto::Event::Assistant { delta, .. }) => {
                assert!(delta.contains("你好"), "echo 应含原文");
                got_echo = true;
            }
            Frame::Event(oc_proto::Event::Lifecycle { phase: LifecyclePhase::End, .. }) => {
                got_end = true;
            }
            _ => {}
        }
        if got_run_id && got_echo && got_end {
            break;
        }
    }

    assert!(got_run_id, "应收到 chat.send 的 run_id 应答");
    assert!(got_echo, "应收到 assistant echo 事件");
    assert!(got_end, "应收到 lifecycle end 事件");

    server.abort();
}

/// sessions.list 经真实传输往返：验证返回 Vec 的 MethodOk 能被序列化
/// （回归 MethodOk 内部标签无法序列化 newtype-包-Vec 的 bug）。
#[tokio::test]
async fn sessions_list_roundtrip() {
    let kind = test_transport("sessions");

    let server_kind = kind.clone();
    let server = tokio::spawn(async move {
        use std::sync::Arc;
        use std::time::Duration;
        let provider = Arc::new(oc_llm::mock::MockProvider::echo_text("hi"));
        let cfg = oc_server::testing::test_cfg();
        let _ = oc_server::serve_with(server_kind, provider, cfg, Duration::from_secs(60), oc_store::Store::open_memory().unwrap()).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let stream = connect(&kind).await;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    // connect（registry 预建 main 会话）。
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("c0"),
        method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Interactive }),
        idempotency_key: None,
    })).await;
    let _ = recv(&mut reader).await;

    // sessions.list → 应成功返回含 main 的列表（此前会序列化失败并断连）。
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("s1"),
        method: Method::SessionsList,
        idempotency_key: None,
    })).await;

    let f = tokio::time::timeout(std::time::Duration::from_secs(2), recv(&mut reader))
        .await
        .expect("sessions.list 不应超时/断连");
    match f {
        Frame::Res(res) => {
            assert_eq!(res.id.as_str(), "s1");
            match res.result {
                ResResult::Ok(oc_proto::MethodOk::Sessions(list)) => {
                    assert!(list.iter().any(|s| s.id.as_str() == "main"), "应含 main 会话");
                }
                other => panic!("期望 Sessions 应答，得到 {other:?}"),
            }
        }
        other => panic!("期望 Res，得到 {other:?}"),
    }

    server.abort();
}

/// diagnostics 经真实传输往返：验证 DiagnosticsSnapshot 能被序列化，且预建的
/// main 会话出现在快照里（回归 MethodOk 新变体的传输契约）。
///
/// 顺带钉住 `idem_entries` 这条计数真的从 state 穿过协议到客户端（P2-3 的内存
/// 可观测抓手；语义本身在 tests/gc.rs 覆盖，这里只管传输契约）。
#[tokio::test]
async fn diagnostics_roundtrip() {
    let kind = test_transport("diag");

    let server_kind = kind.clone();
    let server = tokio::spawn(async move {
        use std::sync::Arc;
        use std::time::Duration;
        let provider = Arc::new(oc_llm::mock::MockProvider::echo_text("hi"));
        let cfg = oc_server::testing::test_cfg();
        let _ = oc_server::serve_with(server_kind, provider, cfg, Duration::from_secs(60), oc_store::Store::open_memory().unwrap()).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let stream = connect(&kind).await;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("c0"),
        method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Interactive }),
        idempotency_key: None,
    })).await;
    let _ = recv(&mut reader).await;

    // 先发一条**带幂等键**的请求，让幂等缓存里确实有东西可数。
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("h1"),
        method: Method::Health,
        idempotency_key: Some(oc_proto::IdemKey::new("idem-probe")),
    })).await;
    let _ = recv(&mut reader).await;

    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("d1"),
        method: Method::Diagnostics,
        idempotency_key: None,
    })).await;

    let f = tokio::time::timeout(std::time::Duration::from_secs(2), recv(&mut reader))
        .await
        .expect("diagnostics 不应超时/断连");
    match f {
        Frame::Res(res) => {
            assert_eq!(res.id.as_str(), "d1");
            match res.result {
                ResResult::Ok(oc_proto::MethodOk::Diagnostics(snap)) => {
                    assert!(snap.store_writer_alive, "写线程应存活");
                    assert!(
                        snap.sessions.iter().any(|s| s.session_id.as_str() == "main"),
                        "快照应含预建的 main 会话"
                    );
                    assert_eq!(
                        snap.idem_entries, 1,
                        "上面那条带幂等键的请求应被计入 idem_entries"
                    );
                }
                other => panic!("期望 Diagnostics 应答，得到 {other:?}"),
            }
        }
        other => panic!("期望 Res，得到 {other:?}"),
    }

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
