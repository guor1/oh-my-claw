//! 客户端传输：连 daemon（unix socket / 命名管道）+ NDJSON 帧读写。

use std::path::PathBuf;

use anyhow::{Context, Result};
use oc_proto::Frame;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

/// 客户端连接方式。
#[derive(Debug, Clone)]
pub enum ConnectTo {
    Unix(PathBuf),
    Pipe(String),
}

impl ConnectTo {
    /// 平台默认端点。`OC_SOCKET` 若已设置则优先——必须与 server 侧
    /// （`oc_server::transport`）保持同一套解析规则，否则 CLI 会连错端点。
    ///
    /// oc-tui 不依赖 oc-server，故常量在此重复一份；两处都改才算改对。
    pub fn platform_default(oc_home: &std::path::Path) -> Self {
        if let Some(s) = std::env::var("OC_SOCKET")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            #[cfg(windows)]
            {
                return ConnectTo::Pipe(s);
            }
            #[cfg(not(windows))]
            {
                return ConnectTo::Unix(PathBuf::from(s));
            }
        }
        #[cfg(windows)]
        {
            let _ = oc_home;
            ConnectTo::Pipe(r"\\.\pipe\oc-daemon".to_string())
        }
        #[cfg(not(windows))]
        {
            ConnectTo::Unix(oc_home.join("run").join("oc.sock"))
        }
    }
}

/// 已连接的传输，拆成读/写半。
pub struct ClientTransport {
    reader: BufReader<ReadHalf<Stream>>,
    writer: WriteHalf<Stream>,
    line: String,
}

/// 底层流（按平台分派）。
pub enum Stream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl ClientTransport {
    pub async fn connect(to: &ConnectTo) -> Result<Self> {
        let stream = match to {
            #[cfg(unix)]
            ConnectTo::Unix(path) => Stream::Unix(
                tokio::net::UnixStream::connect(path)
                    .await
                    .with_context(|| format!("连接 {} 失败", path.display()))?,
            ),
            #[cfg(windows)]
            ConnectTo::Pipe(name) => {
                use tokio::net::windows::named_pipe::ClientOptions;
                Stream::Pipe(
                    ClientOptions::new()
                        .open(name)
                        .with_context(|| format!("连接管道 {name} 失败"))?,
                )
            }
            #[cfg(not(unix))]
            ConnectTo::Unix(_) => anyhow::bail!("本平台不支持 unix socket"),
            #[cfg(not(windows))]
            ConnectTo::Pipe(_) => anyhow::bail!("本平台不支持命名管道"),
        };

        let (r, w) = tokio::io::split(stream);
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
            line: String::new(),
        })
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        let mut s = serde_json::to_string(frame)?;
        s.push('\n');
        self.writer.write_all(s.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// 读下一帧；`Ok(None)` 表示对端关闭。
    pub async fn recv(&mut self) -> Result<Option<Frame>> {
        loop {
            self.line.clear();
            let n = self.reader.read_line(&mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            if let Some(frame) = decode_frame(&self.line)? {
                return Ok(Some(frame));
            }
            // decode_frame 返回 None：空行或坏帧，继续读下一行。
        }
    }
}

/// 解码一行 NDJSON。空行返回 `Ok(None)`（跳过）；坏帧 skip + warn 后也返回
/// `Ok(None)`，不让一条脏行杀掉整条连接（对齐 oc-http `conn_pool.rs` 的宽容策略）。
fn decode_frame(line: &str) -> Result<Option<Frame>> {
    let t = line.trim_end();
    if t.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Frame>(t) {
        Ok(f) => Ok(Some(f)),
        Err(e) => {
            eprintln!("oc-tui: 跳过无法解析的帧（{e}）");
            Ok(None)
        }
    }
}

// Stream 的 AsyncRead/AsyncWrite 转发。
impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            Stream::Pipe(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            Stream::Pipe(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            Stream::Pipe(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            Stream::Pipe(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_proto::{Event, LifecyclePhase};

    #[test]
    fn decode_skips_unknown_frame_kind() {
        // 服务端将来新增的 kind 不该让旧 client 断连。
        let r = decode_frame(r#"{"kind":"future_kind","x":1}"#);
        assert!(r.unwrap().is_none(), "未知 kind 应被跳过");
    }

    #[test]
    fn decode_skips_empty_and_malformed_lines() {
        assert!(decode_frame("   ").unwrap().is_none(), "空行应跳过");
        assert!(decode_frame("{not json").unwrap().is_none(), "坏 JSON 应跳过");
    }

    #[test]
    fn decode_parses_valid_event() {
        let f = decode_frame(
            r#"{"kind":"event","event":"lifecycle","session":"main","run_id":"r1","phase":{"phase":"end"}}"#,
        )
        .unwrap()
        .expect("合法帧应解析");
        match f {
            Frame::Event(Event::Lifecycle { phase: LifecyclePhase::End, .. }) => {}
            other => panic!("应解析成 lifecycle end，得到 {other:?}"),
        }
    }
}
