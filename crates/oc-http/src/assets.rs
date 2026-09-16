//! 静态资源：/ 服务内嵌 Vue bundle。
//!
//! 编译期经 rust-embed 嵌入 `ui/dist`（含 hashed assets，已提交），release
//! 二进制自包含，无需 Node。页面加载后由前端调用 `/auth/login` 换取
//! HttpOnly 会话 cookie，再带 cookie 调 `/api/v1/*`——token 本身不再注入
//! index.html，避免被任何能访问 `/` 的未授权客户端读走。

use axum::{
    extract::State as AxumState,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::Embed;

use crate::server::AppState;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/ui/dist"]
struct Assets;

/// Web UI 路由：`/` 加任意 bundle 资源路径。
///
/// 用 fallback 而非逐文件注册，SPA 自己管客户端路由：未知路径回 index.html，
/// 深链 `/session/abc` 也能正常启动。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .fallback(get(serve_asset))
}

async fn index(AxumState(_state): AxumState<AppState>) -> Response {
    serve_path("index.html")
}

async fn serve_asset(AxumState(_state): AxumState<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // API 路径绝不回退 index.html：那儿的 404 就该是 404，不能返回 HTML
    // 让 fetch() 调用方解析失败。
    if path.starts_with("api/") || path.starts_with("v1/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if Assets::get(path).is_some() {
        serve_path(path)
    } else {
        // SPA 深链：交还应用外壳，让客户端路由接管。
        serve_path("index.html")
    }
}

fn serve_path(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            let body: Vec<u8> = file.data.into_owned();

            // hashed assets 内容可寻址，可以不可变缓存；HTML 外壳不嵌入任何
            // 机密，但仍用 no-store 避免旧外壳被共享缓存长期复用。
            let cache_control = if path.ends_with(".html") {
                "no-store"
            } else {
                "public, max-age=31536000, immutable"
            };

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, cache_control),
                ],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Web UI bundle not found",
        )
            .into_response(),
    }
}

/// 按扩展名定 content type。不引 mime_guess：Vite 产物只有这几种类型。
fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
