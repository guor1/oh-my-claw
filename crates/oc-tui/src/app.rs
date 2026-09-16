//! TUI 应用（设计 §8）。ratatui + crossterm。
//!
//! 布局：消息流区 + 输入区 + 状态栏。事件循环 select 键盘输入与 daemon 帧。

use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use oc_proto::{
    ChatSendParams, ClientKind, CommandParams, ConnectParams, Event, Frame, LifecyclePhase, Method,
    Req, ReqId, ResResult, RunId, SessionId, PROTO_VERSION,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame as UiFrame, Terminal};

use crate::client::{ClientTransport, ConnectTo};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// PageUp/PageDown 每次滚动的保守行数（draw 时按可视高度再夹紧）。
const SCROLL_PAGE: u16 = 10;

/// 一行消息（用于渲染）。
struct Msg {
    who: &'static str,
    text: String,
}

pub struct App {
    client: ClientTransport,
    msgs: Vec<Msg>,
    input: String,
    status: String,
    connected: bool,
    next_req: u64,
    should_quit: bool,
    /// 待处理审批 id（非 None 时输入区进入 y/n 审批模式）。
    pending_approval: Option<oc_proto::ApprovalId>,
    /// 待处理用户输入 id（ask_user，非 None 时输入区进入自由文本回答模式）。
    pending_input: Option<oc_proto::InputId>,
    /// `msgs` 末尾那条助手消息所属的 run。
    ///
    /// 流式增量据此判断是追加还是新起一条——排队的多轮会连续吐回复，
    /// 中间没有别的消息类型分隔，只看 `who` 会把它们拼成一行。
    last_assistant_run: Option<RunId>,
    /// 当前活跃会话（chat.send 归属；事件按此过滤显示）。
    current_session: SessionId,
    /// 上下文用量提示（已用/窗口），随 Usage 事件更新，显示在状态栏。
    usage_hint: Option<String>,
    /// 消息区滚动：距底部的行数偏移。0 = 跟随底部（自动滚到最新消息）；
    /// >0 = 向上回滚查看历史。draw 时按可视高度夹紧。
    scroll_back: u16,
    /// 「排队中」去抖：发送后若该轮迟迟未起步（未收到 Lifecycle::Start），
    /// 到此时刻才显示「排队中…」。Some=有一个已提交但未起步的轮在等待此截止点；
    /// 收到 Start 或该轮终态时清空。避免车道空闲时「排队中」一闪而过。
    pending_queue_hint: Option<std::time::Instant>,
    /// thinking 模型的推理内容尾部（状态栏滚动显示）。有界，只留末尾。
    reasoning_tail: String,
    /// 是否渲染 reasoning（键位切换；仅控制显示，事件照收）。
    show_reasoning: bool,
}

/// 「排队中…」去抖延迟：发送后超过此时长仍未起步才提示排队。
const QUEUE_HINT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

impl App {
    pub async fn connect(to: &ConnectTo) -> Result<Self> {
        let mut client = ClientTransport::connect(to).await?;
        // 建连握手。
        let req = Req {
            id: ReqId::new("connect-0"),
            method: Method::Connect(ConnectParams {
                proto_version: PROTO_VERSION,
                token: None,
                client_kind: ClientKind::Interactive,
            }),
            idempotency_key: None,
        };
        client.send(&Frame::Req(req)).await?;

        let mut app = Self {
            client,
            msgs: Vec::new(),
            input: String::new(),
            status: "连接中…".to_string(),
            connected: false,
            next_req: 1,
            should_quit: false,
            pending_approval: None,
            pending_input: None,
            last_assistant_run: None,
            current_session: SessionId::main(),
            usage_hint: None,
            scroll_back: 0,
            pending_queue_hint: None,
            reasoning_tail: String::new(),
            show_reasoning: true,
        };
        // 等 hello。
        if let Some(Frame::Res(res)) = app.client.recv().await? {
            match res.result {
                ResResult::Ok(_) => {
                    app.connected = true;
                    app.status = "已连接".to_string();
                    app.push_sys("已连接到 oc daemon。输入消息回车发送，/help 看指令，Ctrl-T 开关思考显示，Ctrl-C 退出。");
                }
                ResResult::Err(e) => {
                    app.status = format!("连接被拒: {}", e.message);
                }
            }
        }
        Ok(app)
    }

    fn push_sys(&mut self, s: &str) {
        self.msgs.push(Msg {
            who: "系统",
            text: s.to_string(),
        });
    }

    /// 主事件循环。
    pub async fn run(&mut self, term: &mut Term) -> Result<()> {
        let mut keys = EventStream::new();
        term.draw(|f| self.draw(f))?;

        while !self.should_quit {
            // 注：draw 需 &mut self（夹紧 scroll_back），下面两处 term.draw 同。
            tokio::select! {
                // 终端事件（键盘 / 缩放）
                maybe_ev = keys.next() => {
                    match maybe_ev {
                        Some(Ok(CtEvent::Key(key))) => {
                            // Windows 控制台会同时上报 Press/Release（甚至 Repeat），
                            // 只处理 Press，否则一次按键被处理多次（字符重复/多空格）。
                            if key.kind == KeyEventKind::Press {
                                self.on_key(key.code, key.modifiers).await?;
                            }
                        }
                        // 缩放：强制整屏清屏再重绘，清掉旧尺寸残留（否则输入会串到
                        // 边框横线上、状态行出现前后帧重影）。ratatui 差量渲染不会
                        // 自动清除缩放后的陈旧单元格。
                        Some(Ok(CtEvent::Resize(_, _))) => {
                            term.clear()?;
                        }
                        _ => {}
                    }
                }
                // daemon 帧
                frame = self.client.recv() => {
                    match frame? {
                        Some(f) => self.on_frame(f),
                        None => {
                            self.status = "daemon 已断开".to_string();
                            self.connected = false;
                        }
                    }
                }
                // 「排队中」去抖到点：截止时仍未起步 → 显示排队提示。
                // 无待定提示时该分支永久挂起（不参与 select）。
                _ = sleep_until_opt(self.pending_queue_hint) => {
                    self.pending_queue_hint = None;
                    self.status = "排队中…".to_string();
                }
            }
            term.draw(|f| self.draw(f))?;
        }
        Ok(())
    }

    async fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<()> {
        let code = normalize_key(code, mods);
        // 审批模式：y/n 优先处理（Ctrl-C 仍可退出）。
        if self.pending_approval.is_some()
            && !(matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL))
        {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') if is_text_char(mods) => {
                    self.reply_approval(true).await?;
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') if is_text_char(mods) => {
                    self.reply_approval(false).await?;
                    return Ok(());
                }
                _ => return Ok(()), // 审批期间忽略其它输入
            }
        }
        // 输入模式（ask_user）：自由文本回答。Enter 发送，Esc 取消，Ctrl-C 仍可退出。
        if self.pending_input.is_some()
            && !(matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL))
        {
            match code {
                KeyCode::Enter => {
                    let text = self.input.trim().to_string();
                    // 空 = 取消（未作答）；否则回传文本。
                    self.reply_input(if text.is_empty() { None } else { Some(text) }).await?;
                    self.input.clear();
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.reply_input(None).await?; // 取消
                    self.input.clear();
                    return Ok(());
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    return Ok(());
                }
                KeyCode::Char(c) if is_text_char(mods) => {
                    self.input.push(c);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    // 空行：什么都不做。
                } else if text.starts_with('/') {
                    // 斜杠指令：不做本地解析，整条发给 daemon（唯一解析器）。
                    // 结果经 CommandResult 回执应用视图副作用（切会话/清屏）。
                    if self.connected {
                        self.send_command(text).await?;
                    }
                } else if self.connected {
                    self.msgs.push(Msg { who: "你", text: text.clone() });
                    self.send_chat(text).await?;
                    // 去抖：不立即显示「排队中…」，只记一个截止点。若 150ms 内收到
                    // 本轮 Lifecycle::Start（车道空、秒起步），直接进「助手思考中…」，
                    // 排队提示从不出现；只有超时仍未起步（真在排队）才显示，
                    // 避免一闪而过。
                    self.pending_queue_hint =
                        Some(std::time::Instant::now() + QUEUE_HINT_DELAY);
                }
                // 发送后回到底部跟随，确保看到自己的消息与后续回复。
                self.scroll_back = 0;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            // 消息区滚动。PageUp/Down 翻一屏（用保守步长，draw 时再按可视高度夹紧）；
            // Ctrl+Home 跳最顶，Ctrl+End 回底部跟随。
            KeyCode::PageUp => {
                self.scroll_back = self.scroll_back.saturating_add(SCROLL_PAGE);
            }
            KeyCode::PageDown => {
                self.scroll_back = self.scroll_back.saturating_sub(SCROLL_PAGE);
            }
            KeyCode::Home if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll_back = u16::MAX; // draw 夹到顶部最大偏移
            }
            KeyCode::End if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll_back = 0;
            }
            KeyCode::Char('t') if mods.contains(KeyModifiers::CONTROL) => {
                self.show_reasoning = !self.show_reasoning;
                if !self.show_reasoning {
                    self.reasoning_tail.clear();
                }
            }
            KeyCode::Char(c) if is_text_char(mods) => {
                self.input.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    async fn reply_approval(&mut self, allow: bool) -> Result<()> {
        let Some(id) = self.pending_approval.take() else {
            return Ok(());
        };
        let req_id = format!("appr-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(req_id),
            method: Method::ApprovalReply(oc_proto::ApprovalReplyParams {
                approval_id: id,
                allow,
            }),
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        self.msgs.push(Msg {
            who: "系统",
            text: if allow { "已批准".into() } else { "已拒绝".into() },
        });
        self.status = "已连接".to_string();
        Ok(())
    }

    /// 回执 ask_user 的用户输入（`None` = 取消/未作答）。
    async fn reply_input(&mut self, text: Option<String>) -> Result<()> {
        let Some(id) = self.pending_input.take() else {
            return Ok(());
        };
        let req_id = format!("uinput-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(req_id),
            method: Method::UserReply(oc_proto::UserReplyParams {
                input_id: id,
                text: text.clone(),
            }),
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        self.msgs.push(Msg {
            who: "你",
            text: text.unwrap_or_else(|| "（已取消回答）".into()),
        });
        self.status = "已连接".to_string();
        Ok(())
    }

    async fn send_chat(&mut self, text: String) -> Result<()> {
        let id = format!("req-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(id),
            method: Method::ChatSend(ChatSendParams {
                session: Some(self.current_session.clone()),
                text,
            }),
            idempotency_key: Some(oc_proto::IdemKey::new(uuid_like(self.next_req))),
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    /// 发一条斜杠指令给 daemon（唯一解析器），应答经 `on_frame` 的 `MethodOk::Command` 应用。
    async fn send_command(&mut self, text: String) -> Result<()> {
        let id = format!("cmd-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(id),
            method: Method::Command(CommandParams {
                session: Some(self.current_session.clone()),
                text,
            }),
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Event(ev) => self.on_event(ev),
            Frame::Res(res) => match res.result {
                ResResult::Ok(oc_proto::MethodOk::Command(result)) => {
                    // 应用视图副作用，再显示命令输出。
                    if let Some(id) = &result.clear_view {
                        if id == &self.current_session {
                            self.msgs.clear();
                        }
                    }
                    if let Some(id) = &result.switch_session {
                        self.current_session = id.clone();
                        self.last_assistant_run = None;
                    }
                    if result.text.is_empty() {
                        self.push_sys("（空）");
                    } else {
                        for line in result.text.lines() {
                            self.push_sys(line);
                        }
                    }
                }
                // 请求被拒（如「会话繁忙：队列已满」/ 未知指令）。必须显示：被拒的轮不会产生
                // 任何 Lifecycle 事件，若静默吞掉，界面会一直停在「排队中…」等一个
                // 永不起步的 run。同时清掉排队提示的截止点。
                ResResult::Err(e) => {
                    self.pending_queue_hint = None;
                    self.status = format!("错误: {}", e.message);
                    self.push_sys(&format!("请求被拒: {}", e.message));
                }
                _ => {}
            },
            Frame::Req(_) => {}
        }
    }

    fn on_event(&mut self, ev: Event) {
        // 只显示归属当前会话的事件（Proactive 归属 main，见下单独放行）。
        if let Some(sid) = event_session(&ev) {
            let is_proactive = matches!(ev, Event::Proactive { .. });
            if sid != &self.current_session && !is_proactive {
                return;
            }
        }
        match ev {
            Event::Lifecycle { phase, .. } => match phase {
                LifecyclePhase::Start => {
                    // 本轮已起步：取消待定的排队提示，直接进思考态。
                    self.pending_queue_hint = None;
                    self.status = "助手思考中…".to_string();
                }
                LifecyclePhase::End => {
                    self.pending_queue_hint = None;
                    self.reasoning_tail.clear();
                    self.status = "已连接".to_string();
                }
                LifecyclePhase::Error { message, .. } => {
                    self.pending_queue_hint = None;
                    self.reasoning_tail.clear();
                    self.status = format!("错误: {message}");
                }
            },
            Event::Assistant { delta, run_id, .. } => {
                self.reasoning_tail.clear();
                // 流式增量：只追加到**同一个 run** 的最后一条助手消息，否则新起一条。
                //
                // 只判 `who == "助手"` 会把不同 run 的回复拼成一段读不通的话：
                // 排队的几轮依次起步，之间没有别的消息类型插进 msgs，于是三条
                // 回复首尾相接挤在一行里（真机上「…生成这份 PPT。OLD你好！…」）。
                let same_run = self
                    .msgs
                    .last()
                    .is_some_and(|m| m.who == "助手" && self.last_assistant_run.as_ref() == Some(&run_id));
                if same_run {
                    if let Some(last) = self.msgs.last_mut() {
                        last.text.push_str(&delta);
                    }
                    return;
                }
                self.last_assistant_run = Some(run_id);
                self.msgs.push(Msg { who: "助手", text: delta });
            }
            Event::Proactive { text, .. } => {
                if !text.is_empty() {
                    self.msgs.push(Msg { who: "主动提醒", text });
                }
            }
            Event::Tool { phase, .. } => match phase {
                oc_proto::ToolPhase::Start { name, args } => {
                    self.reasoning_tail.clear();
                    self.msgs.push(Msg {
                        who: "工具",
                        text: format!("{name}: {}", truncate(&args, 200)),
                    });
                }
                oc_proto::ToolPhase::Update { chunk } => {
                    if let Some(last) = self.msgs.last_mut() {
                        if last.who == "工具" {
                            last.text.push_str(&chunk);
                            return;
                        }
                    }
                    self.msgs.push(Msg { who: "工具", text: chunk });
                }
                oc_proto::ToolPhase::End { .. } => {}
            },
            Event::Approval { approval_id, summary, command, .. } => {
                self.msgs.push(Msg {
                    who: "审批",
                    text: format!("{summary}\n  命令: {command}\n  批准执行？(y/n)"),
                });
                self.pending_approval = Some(approval_id);
                self.status = "等待审批：按 y 批准 / n 拒绝".to_string();
            }
            Event::UserInput { input_id, prompt, .. } => {
                self.msgs.push(Msg {
                    who: "提问",
                    text: format!("{prompt}\n  （输入回答后回车；Esc 取消）"),
                });
                self.pending_input = Some(input_id);
                self.status = "助手提问：输入回答后回车 / Esc 取消".to_string();
            }
            Event::Task { .. } => {}
            Event::Usage { input_tokens, context_window, .. } => {
                // 实时更新上下文用量提示（显示在状态栏）。
                self.usage_hint = Some(format_usage(input_tokens, context_window));
            }
            Event::Reasoning { delta, .. } => {
                if !self.show_reasoning {
                    return;
                }
                // 换行压成空格，避免推理内容里带进换行把状态栏撑成多行。
                self.reasoning_tail.push_str(&delta.replace(['\r', '\n'], " "));
                // 有界：只留末尾，防长推理无限增长。
                const TAIL_CAP: usize = 512;
                let len = self.reasoning_tail.chars().count();
                if len > TAIL_CAP {
                    self.reasoning_tail = self.reasoning_tail.chars().skip(len - TAIL_CAP).collect();
                }
            }
        }
    }

    /// 消息区标题：回滚查看历史时提示当前偏移，让用户知道未在底部。
    fn msg_title(&self) -> String {
        if self.scroll_back > 0 {
            format!("oc [↑历史 -{} 行  PgDn/Ctrl-End 回底部]", self.scroll_back)
        } else {
            "oc".to_string()
        }
    }

    fn draw(&mut self, f: &mut UiFrame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(f.area());

        // 消息流
        let lines: Vec<Line> = self
            .msgs
            .iter()
            .map(|m| {
                let color = match m.who {
                    "你" => Color::Cyan,
                    "助手" => Color::Green,
                    "主动提醒" => Color::Yellow,
                    "审批" => Color::Red,
                    "提问" => Color::Blue,
                    "工具" => Color::Magenta,
                    _ => Color::DarkGray,
                };
                Line::from(vec![
                    Span::styled(format!("{}: ", m.who), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::raw(m.text.clone()),
                ])
            })
            .collect();

        // 可视区（去掉上下边框各 1 行）与内容宽度（去左右边框各 1 列）。
        let area = chunks[0];
        let visible = area.height.saturating_sub(2);
        let inner_w = area.width.saturating_sub(2);

        // 先用无边框副本按内容宽度算 wrap 后真实行数（含中文双宽），据此夹紧
        // scroll_back，再构建带标题的正式段落——标题读的是夹紧后的值。
        let body = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let total = body.line_count(inner_w) as u16;
        let max_off = total.saturating_sub(visible);
        // 夹紧回滚偏移：0..=max_off。scroll_back=0 贴底跟随最新。
        self.scroll_back = self.scroll_back.min(max_off);
        let y = max_off - self.scroll_back;

        let msgs = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(self.msg_title()))
            .wrap(Wrap { trim: false })
            .scroll((y, 0));
        f.render_widget(msgs, area);

        // 输入
        let input = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("输入"));
        f.render_widget(input, chunks[1]);

        // 状态栏：thinking 活跃时让位给推理滚动；否则沿用原有状态 + 用量。
        let status_line = if self.show_reasoning && !self.reasoning_tail.is_empty() {
            let max_cols = chunks[2].width.saturating_sub(3) as usize; // 留 💭 前缀
            format!("💭 {}", tail_display(&self.reasoning_tail, max_cols))
        } else {
            match &self.usage_hint {
                Some(u) => format!("{}  |  ctx {}", self.status, u),
                None => self.status.clone(),
            }
        };
        let status = Paragraph::new(status_line).style(Style::default().fg(Color::DarkGray));
        f.render_widget(status, chunks[2]);
    }
}

/// 格式化上下文用量：「12.3K/64K (19%)」。
fn format_usage(used: u32, window: u32) -> String {
    let pct = if window > 0 { used as f64 / window as f64 * 100.0 } else { 0.0 };
    format!("{}/{} ({:.0}%)", short_k(used), short_k(window), pct)
}

/// 紧凑显示 token 数：1234→1.2K，65536→64K。
fn short_k(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// 取字符串的末尾 `max_cols` 个显示列宽（中文双宽按列算，不按字符数）。
/// delta 持续追加、显示始终取尾部，天然形成左滚，无需动画定时器。
fn tail_display(s: &str, max_cols: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max_cols {
        return s.to_string();
    }
    let skip = s.width() - max_cols; // 从左边跳过的列宽
    let mut cols = 0usize;
    let mut start = s.len();
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if cols + w > skip {
            start = i;
            break;
        }
        cols += w;
        start = i + c.len_utf8();
    }
    s[start..].to_string()
}

/// 非文本修饰键（Ctrl/Alt/Super/Hyper/Meta）；带这些的 `Char` 不是可输入字符。
const NON_TEXT_MODS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SUPER)
    .union(KeyModifiers::HYPER)
    .union(KeyModifiers::META);

/// 该 `Char` 事件是否为可插入输入框的字面字符（只允许无修饰或 Shift）。
///
/// 不判这个的话，Ctrl/Alt 组合会漏成字面字母塞进输入框——Linux 下退格
/// 变 `h` 就是这么来的（见 [`normalize_key`]）。
fn is_text_char(mods: KeyModifiers) -> bool {
    !mods.intersects(NON_TEXT_MODS)
}

/// 归一化平台差异的按键上报。
///
/// Linux/Unix 下 crossterm 逐字节解析 stdin：DEL(`0x7F`) → `Backspace`，但
/// BS(`0x08`) 落在 `0x01..=0x1A` 控制码区间，被解析成 `Ctrl+H`；Ctrl+I/Ctrl+M
/// 同理会变成 `Ctrl+I`/`Ctrl+M` 而非 Tab/Enter。而 xterm（`backarrowKey`）、
/// tmux、部分串口/远程会话的退格键发的正是 BS，于是退格在 Linux 上不删字符、
/// 反而输入一个 `h`。Windows 走 winapi 路径直接上报 `Backspace`，所以只在
/// Linux 复现。这里把这三个控制码还原成对应的功能键。
fn normalize_key(code: KeyCode, mods: KeyModifiers) -> KeyCode {
    if !mods.contains(KeyModifiers::CONTROL) {
        return code;
    }
    match code {
        KeyCode::Char('h') | KeyCode::Char('H') => KeyCode::Backspace, // BS 0x08
        KeyCode::Char('i') | KeyCode::Char('I') => KeyCode::Tab,       // HT 0x09
        KeyCode::Char('m') | KeyCode::Char('M') => KeyCode::Enter,     // CR 0x0D
        other => other,
    }
}

/// 取事件归属的会话 id（所有变体都带 session）。
fn event_session(ev: &Event) -> Option<&SessionId> {
    Some(match ev {
        Event::Lifecycle { session, .. }
        | Event::Assistant { session, .. }
        | Event::Reasoning { session, .. }
        | Event::Tool { session, .. }
        | Event::Proactive { session, .. }
        | Event::Task { session, .. }
        | Event::Usage { session, .. }
        | Event::Approval { session, .. }
        | Event::UserInput { session, .. } => session,
    })
}

/// 简易唯一键（避免为 TUI 引入 uuid 依赖）。
fn uuid_like(n: u64) -> String {
    format!("tui-{}-{}", std::process::id(), n)
}

/// 截断过长文本用于单行显示（工具参数预览）。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// select 辅助：到给定时刻醒来；`None` 则永久挂起（该 select 分支不参与）。
async fn sleep_until_opt(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux 下退格发 BS(0x08)，crossterm 报成 Ctrl+H：必须还原成 Backspace，
    /// 否则退格会往输入框里塞一个 `h`。
    #[test]
    fn ctrl_h_normalizes_to_backspace() {
        assert_eq!(
            normalize_key(KeyCode::Char('h'), KeyModifiers::CONTROL),
            KeyCode::Backspace
        );
        assert_eq!(
            normalize_key(KeyCode::Char('i'), KeyModifiers::CONTROL),
            KeyCode::Tab
        );
        assert_eq!(
            normalize_key(KeyCode::Char('m'), KeyModifiers::CONTROL),
            KeyCode::Enter
        );
    }

    /// 无 Ctrl 时 `h` 仍是可输入字符，不能被吞掉。
    #[test]
    fn plain_chars_pass_through() {
        assert_eq!(
            normalize_key(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyCode::Char('h')
        );
        assert_eq!(
            normalize_key(KeyCode::Char('H'), KeyModifiers::SHIFT),
            KeyCode::Char('H')
        );
        assert_eq!(
            normalize_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyCode::Char('c') // Ctrl-C 退出，不改写
        );
    }

    #[test]
    fn only_unmodified_or_shifted_chars_are_text() {
        assert!(is_text_char(KeyModifiers::NONE));
        assert!(is_text_char(KeyModifiers::SHIFT));
        assert!(!is_text_char(KeyModifiers::CONTROL));
        assert!(!is_text_char(KeyModifiers::ALT));
    }

    #[test]
    fn tail_display_keeps_tail_columns() {
        // "你好世界" = 8 列宽（每字 2）。取尾 4 列应得 "好世"？不——"你好世界" 四字各 2 列，
        // 尾 4 列 = 后两个字 "世界"。
        assert_eq!(tail_display("你好世界", 4), "世界");
        // 全 ASCII，尾 3 = "xyz"
        assert_eq!(tail_display("abcdefxyz", 3), "xyz");
    }

    #[test]
    fn tail_display_short_enough_unchanged() {
        assert_eq!(tail_display("短", 10), "短");
        assert_eq!(tail_display("abc", 3), "abc");
    }

    #[test]
    fn tail_display_handles_mixed_width() {
        // "a好b" = 1 + 2 + 1 = 4 列。尾 3 列 = "好b"。
        assert_eq!(tail_display("a好b", 3), "好b");
    }
}
