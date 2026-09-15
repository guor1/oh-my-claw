//! 认证中间件与绑定地址策略的单元测试 + token 注入的端到端回归。
//!
//! `auth.rs` 里已有 `check_bind_requires_token` 的纯函数测试；这里补两个
//! 需要真 axum 路由的用例：token 下 API 被拒 / 放行，以及 token 注入到
//! index.html 的 `window.__OC_TOKEN__`。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::MockProvider;
use oc_server::testing::TestDaemon;

async fn spawn_with(tag: &str, token: Option<&str>) -> (String, TestDaemon) {
    let daemon = TestDaemon::start(tag, Arc::new(MockProvider::echo_text("ok"))).await;
    let pool = oc_http::conn_pool::ConnPool::new(daemon.transport(), 4, 4);
    let app = oc_http::create_app(
        pool,
        "mock".into(),
        oc_http::AppConfig {
            token: token.map(String::from),
            web_ui: true,
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑端口");
    let addr = listener.local_addr().expect("取端口");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), daemon)
}

/// 配置了 token 时：无凭据应 401，带 Bearer 应放行，带 ?token= 也应放行。
#[tokio::test]
async fn token_gates_api_routes() {
    let (base, _daemon) = spawn_with("web-token", Some("s3cret")).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v1/sessions");

    // 无凭据 → 401
    let unauth = client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("应有应答");
    assert_eq!(unauth.status(), 401, "无 token 应 401");

    // 错 token → 401
    let bad = client
        .get(&url)
        .header("Authorization", "Bearer wrong")
        .send()
        .await
        .expect("应有应答");
    assert_eq!(bad.status(), 401, "错 token 应 401");

    // 对 token → 200
    let good = client
        .get(&url)
        .header("Authorization", "Bearer s3cret")
        .send()
        .await
        .expect("应有应答");
    assert_eq!(good.status(), 200, "带正确 Bearer 应放行");

    // ?token= 回退（EventSource 无自定义头）→ 200
    let query = client
        .get(format!("{base}/api/v1/sessions?token=s3cret"))
        .send()
        .await
        .expect("应有应答");
    assert_eq!(query.status(), 200, "?token= 回退应放行");
}

/// 无 token（loopback 模式）：API 全放行，index.html 注入空 token。
#[tokio::test]
async fn no_token_allows_all_and_injects_empty() {
    let (base, _daemon) = spawn_with("web-none", None).await;
    let client = reqwest::Client::new();

    let ok = client
        .get(format!("{base}/api/v1/sessions"))
        .send()
        .await
        .expect("应有应答");
    assert_eq!(ok.status(), 200, "无鉴权模式 API 应放行");

    let html = client
        .get(&base)
        .send()
        .await
        .expect("应有应答")
        .text()
        .await
        .expect("应能读到 index.html");
    assert!(
        html.contains(r#"window.__OC_TOKEN__ = "";"#),
        "无 token 时应注入空串：{html}"
    );
}

/// 配置了 token 时，index.html 应注入真实 token，浏览器才能带凭据调 API。
#[tokio::test]
async fn token_is_injected_into_index() {
    let (base, _daemon) = spawn_with("web-inject", Some("s3cret")).await;
    let client = reqwest::Client::new();

    let html = client
        .get(&base)
        .send()
        .await
        .expect("应有应答")
        .text()
        .await
        .expect("应能读到 index.html");
    assert!(
        html.contains(r#"window.__OC_TOKEN__ = "s3cret";"#),
        "token 应注入到 index.html：{html}"
    );
    // 占位符本身不该残留。
    assert!(!html.contains("__OC_TOKEN__ = \"__OC_TOKEN__\""), "占位符应已被替换");
}

/// 静态资源 fallback 不应把 API 路径 404 变成 HTML。
#[tokio::test]
async fn api_404_is_not_html() {
    let (base, _daemon) = spawn_with("web-404", None).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/v1/does-not-exist"))
        .send()
        .await
        .expect("应有应答");
    assert_eq!(resp.status(), 404);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        !ct.contains("text/html"),
        "API 404 不该返回 HTML（否则 fetch 调用方解析失败）：{ct}"
    );
}

use oc_llm::mock::ScriptStep;
use oc_llm::{Delta, FinishReason};

/// Responses 协议没有 thinking 事件：reasoning 必须被静默，只吐可见文本。
#[tokio::test]
async fn responses_endpoint_silently_drops_reasoning() {
    use oc_llm::mock::MockProvider;
    let daemon = TestDaemon::start(
        "web-reas",
        Arc::new(MockProvider::scripted(vec![
            ScriptStep { delay: Duration::from_millis(20), delta: Delta::Reasoning("隐藏的思考".into()) },
            ScriptStep { delay: Duration::from_millis(20), delta: Delta::Text("可见回答".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ])),
    )
    .await;
    let pool = oc_http::conn_pool::ConnPool::new(daemon.transport(), 4, 4);
    let app = oc_http::create_app(pool, "mock".into(), oc_http::AppConfig { token: None, web_ui: false });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑端口");
    let addr = listener.local_addr().expect("取端口");
    tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();
    let body = client
        .post(format!("{base}/v1/responses"))
        .json(&serde_json::json!({ "input": "hi", "stream": false }))
        .send()
        .await
        .expect("应有应答")
        .text()
        .await
        .expect("读 body");
    assert!(body.contains("可见回答"), "Responses 应含可见文本：{body}");
    assert!(!body.contains("隐藏的思考"), "reasoning 不得泄漏进 Responses：{body}");
}
