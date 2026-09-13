//! web 工具（设计 §6.1）：`web_fetch` 抓取 URL、`web_search` 联网搜索。
//!
//! - **web_fetch**：GET 一个 URL，返回**净化 + 截断**的正文（粗略去 HTML 标签）。
//! - **web_search**：走 Bing HTML 端点（无需 key），解析结果标题+链接+摘要。
//!   DuckDuckGo 的 `html.duckduckgo.com` 在部分网络（如国内）TCP 不可达，Bing 更稳。
//!
//! 抓来的内容是 **Untrusted**（provenance），由调用方按需归类；本层只做传输 + 净化。
//! 需 `web` feature（默认开）。无 feature 时工具不注册。

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::sanitize::sanitize;
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// 抓取正文最大字符数（防拉爆上下文）。
const MAX_BODY_CHARS: usize = 8000;
/// 搜索结果最多条数。
const MAX_RESULTS: usize = 8;

fn client() -> ToolResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("oc-assistant/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::Failed(format!("构造 HTTP client 失败: {e}")))
}

// ── web_fetch ───────────────────────────────────────────────────

pub struct WebFetchTool;

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_string(),
            description: "抓取一个 URL 的网页正文（返回纯文本，已截断）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "http(s) URL" }
                },
                "required": ["url"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(35),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: FetchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        if !(args.url.starts_with("http://") || args.url.starts_with("https://")) {
            return Err(ToolError::BadArgs("url 必须以 http:// 或 https:// 开头".into()));
        }
        cx.update(format!("抓取 {}\n", args.url));

        let resp = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = client()?.get(&args.url).send() => r.map_err(|e| ToolError::Failed(format!("请求失败: {e}")))?,
        };
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("读取正文失败: {e}")))?;

        let text = html_to_text(&body);
        let truncated: String = text.chars().take(MAX_BODY_CHARS).collect();
        let note = if text.chars().count() > MAX_BODY_CHARS {
            "\n…[正文已截断]"
        } else {
            ""
        };
        Ok(ToolOutput::ok(format!(
            "[{status}] {}\n\n{}{note}",
            args.url,
            sanitize(&truncated)
        )))
    }
}

// ── web_search ──────────────────────────────────────────────────

pub struct WebSearchTool;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "联网搜索，返回结果标题与链接列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(35),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        cx.update(format!("搜索「{}」\n", args.query));

        // Bing HTML 端点（无需 key）。DuckDuckGo 的 html.duckduckgo.com 在部分网络
        // TCP 不可达，故改走 Bing；带浏览器 UA 以拿全量结果页而非跳转。
        let url = "https://www.bing.com/search";
        let resp = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = client()?.get(url).query(&[("q", args.query.as_str())]).send() => {
                r.map_err(|e| ToolError::Failed(format!("搜索请求失败: {e}")))?
            }
        };
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("读取搜索结果失败: {e}")))?;

        let results = parse_bing_results(&body);
        if results.is_empty() {
            return Ok(ToolOutput::ok(format!("「{}」无搜索结果。", args.query)));
        }
        let mut out = format!("「{}」搜索结果：\n", args.query);
        for (i, (title, link, snippet)) in results.iter().take(MAX_RESULTS).enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n", i + 1, title, link));
            if !snippet.is_empty() {
                out.push_str(&format!("   {}\n", snippet));
            }
        }
        Ok(ToolOutput::ok(sanitize(&out)))
    }
}

// ── 解析辅助 ────────────────────────────────────────────────────

/// 极简 HTML → 文本：剥 script/style，去标签，压缩空白。非完美，够喂模型。
fn html_to_text(html: &str) -> String {
    let mut s = strip_block(html, "<script", "</script>");
    s = strip_block(&s, "<style", "</style>");

    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // 解 HTML 实体（常见几个）。
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ");
    // 压缩连续空白/空行。
    let mut result = String::with_capacity(out.len());
    let mut prev_blank = false;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !prev_blank {
                result.push('\n');
            }
            prev_blank = true;
        } else {
            result.push_str(t);
            result.push('\n');
            prev_blank = false;
        }
    }
    result
}

/// 去掉 `<tag ...>...</close>` 区块（大小写不敏感的起始匹配）。
fn strip_block(s: &str, open: &str, close: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if lower[i..].starts_with(open) {
            if let Some(end) = lower[i..].find(close) {
                i += end + close.len();
                continue;
            } else {
                break; // 无闭合，丢弃剩余
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 从 Bing HTML 结果里抽 (标题, 链接, 摘要)。按 `<li class="b_algo">` 切块，
/// 每块内标题在 `<h2><a href>`、摘要在小记段落（`b_lineclamp`）、出处 `<cite>`。
fn parse_bing_results(html: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    // 先按结果块边界切（首块前可能有非结果内容，直接 skip(1) 丢掉）。
    for chunk in html.split(r#"<li class="b_algo""#).skip(1) {
        // 标题 + 链接：结果锚 `<h2 ...><a ... href="...">标题</a>`。
        let title_link = chunk
            .split_once("<h2")
            .and_then(|(_, rest)| rest.split_once("href=\""))
            .map(|(_, rest)| {
                let href = rest.split('"').next().unwrap_or_default();
                let title = rest
                    .split_once('>')
                    .and_then(|(_, t)| t.split_once("</a>"))
                    .map(|(t, _)| strip_tags(t).trim().to_string())
                    .unwrap_or_default();
                (href.to_string(), title)
            })
            .filter(|(_, t)| !t.is_empty());

        let (Some((link, title)), _) = (title_link, 0) else {
            continue;
        };
        // 摘要：`<p class="b_lineclamp...">...</p>`，实体已在 html_to_text 里解过，
        // 这里复用 strip_tags + trim。
        let snippet = chunk
            .split_once("b_lineclamp")
            .and_then(|(_, rest)| rest.split_once('>'))
            .and_then(|(_, rest)| rest.split_once("</p>"))
            .map(|(s, _)| collapse_ws(s))
            .unwrap_or_default();
        out.push((title, link, snippet));
    }
    out
}

/// 把一段含标签/实体的片段压成单行纯文本（摘要用）。
fn collapse_ws(s: &str) -> String {
    let s = strip_tags(s);
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&ensp;", " ")
        .replace("&nbsp;", " ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_strips_tags_and_scripts() {
        let html = "<html><head><style>x{}</style></head><body><p>你好<script>bad()</script>世界</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("你好"));
        assert!(text.contains("世界"));
        assert!(!text.contains("bad()"), "script 应被剥除");
        assert!(!text.contains("x{}"), "style 应被剥除");
    }

    #[test]
    fn entity_decoding() {
        assert_eq!(html_to_text("<p>a &amp; b &lt;c&gt;</p>").trim(), "a & b <c>");
    }

    #[test]
    fn parse_bing_extracts_title_link_and_snippet() {
        let html = r#"<li class="b_algo" data-id><h2 class=""><a target="_blank" href="https://rust-lang.org/" h="ID=SERP,5127.2">Rust <b>官网</b></a></h2><p class="b_lineclamp2">Rust is blazingly fast &amp; memory-efficient…</p></li>"#;
        let r = parse_bing_results(html);
        assert_eq!(r.len(), 1, "应抽出 1 条结果: {r:?}");
        assert_eq!(r[0].0, "Rust 官网");
        assert_eq!(r[0].1, "https://rust-lang.org/");
        assert!(r[0].2.contains("blazingly fast"), "摘要应含正文: {}", r[0].2);
    }
}
