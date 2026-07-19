mod client;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{CommandFactory, Parser};
use clap_complete::{self, Generator, Shell};
use iocraft::prelude::*;
use crossterm::event::MouseButton;
use temple_protocol::*;
use uuid::Uuid;

const TEMPLE_ART: &str = include_str!("../../assets/temple.asc");

#[derive(Parser)]
#[command(name = "temple", about = "renco TUI client")]
struct Cli {
    #[arg(short, long, default_value = "127.0.0.1:42123")]
    server: String,
    #[arg(short, long, default_value = ".")]
    cwd: String,
    #[arg(short = 'C', long)]
    client_id: Option<String>,
    #[arg(long, value_enum)]
    generate_completions: Option<Shell>,
}

impl Cli {
    fn print_completions(&self) {
        if let Some(shell) = &self.generate_completions {
            let mut cmd = Self::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, name, &mut std::io::stdout());
            std::process::exit(0);
        }
    }
}

// ── Chat entries ──

#[derive(Debug, Clone)]
enum ChatEntry {
    /// User message
    User(String),
    /// Assistant message (streams in), with optional stats footer
    Assistant { content: String, stats: Option<String> },
    /// System/info line
    System(String),
    /// Tool activity
    Tool { name: String, status: ToolStatus, detail: String },
}

#[derive(Default)]
struct AppState {
    session_id: Uuid,
    entries: Vec<ChatEntry>,
    status: String,
    model: String,
    prompt: String,
    permission: Option<(Uuid, Uuid, String)>,
    running: bool,
    editor_pending: bool,
    /// lines to scroll up from bottom (0 = follow tail)
    scroll: usize,
    /// mouse selection anchor (line, col) while dragging
    sel_anchor: Option<(usize, usize)>,
    /// finished/active selection ((line,col),(line,col)) normalized
    selection: Option<((usize, usize), (usize, usize))>,
    /// plain-text of currently visible chat lines (for hit-testing)
    last_lines: Vec<String>,
    /// transient banner text (e.g. "copied 142 chars")
    banner: Option<(String, std::time::Instant)>,
}

// ── Background WebSocket thread ──

fn spawn_ws(
    server: String, cwd: String, client_id: String,
    state: Arc<Mutex<AppState>>,
    ui_cmd: mpsc::Receiver<ClientMessage>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let sess = SessionOpen {
                client_id,
                cwd: std::fs::canonicalize(&cwd).unwrap().to_string_lossy().into(),
                hostname: whoami::hostname(),
                username: whoami::username(),
            };
            let (mut conn, _) = match client::connect(&server, sess).await {
                Ok(c) => c,
                Err(e) => {
                    state.lock().unwrap().entries.push(ChatEntry::System(
                        format!("Connection failed: {e}"),
                    ));
                    return;
                }
            };
            let tx = conn.write;
            let tx_ping = tx.clone();

            // Reader: server → shared state
            let s = state.clone();
            tokio::spawn(async move {
                while let Some(msg) = conn.incoming.recv().await {
                    let mut s = s.lock().unwrap();
                    match msg {
                        ServerMessage::SessionOpened { session_id, info } => {
                            s.session_id = session_id;
                            s.entries.push(ChatEntry::System(format!(
                                "session on {}", info.hostname
                            )));
                            s.status = format!("renco · {}", info.hostname);
                        }
                        ServerMessage::ChatDelta { delta, done, .. } => {
                            if done {
                                // stream finished; nothing to do (stats come separately)
                            } else if let Some(ChatEntry::Assistant { content, .. }) =
                                s.entries.last_mut()
                            {
                                content.push_str(&delta);
                            } else {
                                s.entries.push(ChatEntry::Assistant {
                                    content: delta,
                                    stats: None,
                                });
                            }
                        }
                        ServerMessage::ChatError { error, .. } => {
                            s.entries.push(ChatEntry::System(format!("error: {error}")));
                        }
                        ServerMessage::ChatStats {
                            model,
                            prompt_tokens,
                            completion_tokens,
                            duration_ms,
                            context_length,
                            prefill_tps,
                            decode_tps,
                            ..
                        } => {
                            let stats = format!(
                                "{model} · prefill {:.0} tok/s · decode {:.1} tok/s · {} ctx · {}+{} · {:.1}s",
                                prefill_tps,
                                decode_tps,
                                fmt_k(context_length),
                                fmt_k(prompt_tokens),
                                fmt_k(completion_tokens),
                                duration_ms as f64 / 1000.0,
                            );
                            if let Some(ChatEntry::Assistant { stats: st, .. }) =
                                s.entries.last_mut()
                            {
                                *st = Some(stats);
                            }
                            s.model = model;
                        }
                        ServerMessage::ToolEvent { name, status, detail, .. } => {
                            // Merge with previous tool entry if same tool in progress
                            s.entries.push(ChatEntry::Tool { name, status, detail });
                        }
                        ServerMessage::ModelList { models } => {
                            s.entries.push(ChatEntry::System(format!(
                                "models ({}):",
                                models.len()
                            )));
                            for m in &models {
                                s.entries.push(ChatEntry::System(format!("  {}", m.id)));
                            }
                        }
                        ServerMessage::ModelChanged { model, .. } => {
                            s.model = model.clone();
                            s.entries.push(ChatEntry::System(format!("model → {model}")));
                        }
                        ServerMessage::PermissionRequired(r) => {
                            s.permission = Some((r.request_id, r.session_id, r.path));
                            s.status = "permission? (y/N)".into();
                        }
                        ServerMessage::PermissionResult(_) => {
                            s.permission = None;
                            s.status = "ready".into();
                        }
                        ServerMessage::ModeChanged { mode, .. } => {
                            s.entries.push(ChatEntry::System(format!("mode → {mode:?}")));
                        }
                        ServerMessage::Shutdown => {
                            s.entries.push(ChatEntry::System("server shutdown".into()));
                            s.running = false;
                        }
                        ServerMessage::Pong | ServerMessage::SessionClosed { .. } => {}
                    }
                }
            });

            // Writer: UI commands → server
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    while let Ok(cmd) = ui_cmd.try_recv() {
                        if tx.send(cmd).is_err() {
                            return;
                        }
                    }
                }
            });

            // Keep-alive pings to temple-server
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if tx_ping.send(ClientMessage::Ping).is_err() {
                    break;
                }
            }
        });
    });
}

fn fmt_k(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

// ── Mouse selection helpers ──

/// Normalize a selection so start <= end (line-major).
fn norm_sel(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Extract selected text from visible lines.
fn extract_selection(lines: &[String], sel: ((usize, usize), (usize, usize))) -> String {
    let ((sl, sc), (el, ec)) = sel;
    if sl >= lines.len() {
        return String::new();
    }
    let el = el.min(lines.len() - 1);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(el + 1).skip(sl) {
        let chars: Vec<char> = line.chars().collect();
        let from = if i == sl { sc.min(chars.len()) } else { 0 };
        let to = if i == el { ec.min(chars.len()) } else { chars.len() };
        if to > from {
            out.push_str(&chars[from..to].iter().collect::<String>());
        }
        if i != el {
            out.push('\n');
        }
    }
    out
}

/// Copy to clipboard via OSC 52 (terminal-native: kitty, foot, wezterm,
/// alacritty, xterm with allowWindowOps). Works over SSH too.
fn osc52_copy(text: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    // c = clipboard selection
    print!("\x1b]52;c;{b64}\x07");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

// ── Text wrapping & markdown-lite rendering ──

struct StyledLine {
    text: String,
    kind: LineKind,
}

#[derive(PartialEq, Clone, Copy)]
enum LineKind {
    Normal,
    Code,
    Dim,
}

/// Render an assistant message body into display lines:
/// - wraps long lines to `width`
/// - ``` fenced blocks become indented code lines
fn render_markdown_lite(content: &str, width: usize) -> Vec<StyledLine> {
    let mut out = Vec::new();
    let mut in_code = false;

    for raw_line in content.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }

        if in_code {
            // code lines: indent, no wrap processing (truncate if needed)
            let display = format!("  {raw_line}");
            if display.chars().count() <= width {
                out.push(StyledLine { text: display, kind: LineKind::Code });
            } else {
                out.push(StyledLine {
                    text: display.chars().take(width).collect(),
                    kind: LineKind::Code,
                });
            }
        } else {
            // normal text: word-wrap
            for chunk in wrap_text(raw_line, width.max(10)) {
                out.push(StyledLine { text: chunk, kind: LineKind::Normal });
            }
        }
    }

    if out.is_empty() {
        out.push(StyledLine { text: String::new(), kind: LineKind::Normal });
    }
    out
}

fn wrap_text(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in line.split(' ') {
        let wlen = word.chars().count();
        let need = if current.is_empty() { wlen } else { wlen + 1 };
        if current_w + need > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_w += 1;
        }
        // hard-split very long words
        if wlen > width {
            let mut rest = word;
            while rest.chars().count() > width {
                let cut: String = rest.chars().take(width).collect();
                lines.push(cut.clone());
                rest = &rest[cut.len()..];
            }
            current.push_str(rest);
            current_w = rest.chars().count();
        } else {
            current.push_str(word);
            current_w += wlen;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ── Props ──

#[derive(Props, Clone)]
struct TempleProps {
    state: Arc<Mutex<AppState>>,
    cmd_tx: mpsc::Sender<ClientMessage>,
}

impl Default for TempleProps {
    fn default() -> Self {
        let (tx, _) = mpsc::channel::<ClientMessage>();
        Self {
            state: Arc::new(Mutex::new(AppState::default())),
            cmd_tx: tx,
        }
    }
}

// ── Main component ──

#[component]
fn Temple(props: &TempleProps, mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let (width, height) = hooks.use_terminal_size();
    let mut system = hooks.use_context_mut::<SystemContext>();

    // Periodic re-render for streaming updates
    let mut tick = hooks.use_state(|| 0u64);
    hooks.use_future({
        let mut t = tick;
        async move {
            loop {
                smol::Timer::after(Duration::from_millis(40)).await;
                t.set(t.get() + 1);
            }
        }
    });

    let cmd_tx = props.cmd_tx.clone();
    let state = props.state.clone();

    // Exit checks
    {
        let s = state.lock().unwrap();
        if s.editor_pending || !s.running {
            drop(s);
            system.exit();
        }
    }

    // Keyboard + mouse
    let state_h = state.clone();
    let cmd_h = cmd_tx.clone();
    hooks.use_terminal_events(move |event| {
        use KeyCode::*;
        match event {
            TerminalEvent::FullscreenMouse(m) => {
                let mut s = state_h.lock().unwrap();
                let chat_top = 1usize + if s.entries.len() < 3 { TEMPLE_ART.lines().count() } else { 0 };
                let line_idx = (m.row as usize).saturating_sub(chat_top);

                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if line_idx < s.last_lines.len() {
                            s.sel_anchor = Some((line_idx, m.column as usize));
                            s.selection = None;
                            s.banner = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(anchor) = s.sel_anchor {
                            if line_idx < s.last_lines.len() {
                                let cur = (line_idx, m.column as usize);
                                s.selection = Some(norm_sel(anchor, cur));
                            }
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(sel) = s.selection.take() {
                            let text = extract_selection(&s.last_lines, sel);
                            if !text.trim().is_empty() {
                                let n = text.chars().count();
                                osc52_copy(&text);
                                s.banner = Some((
                                    format!("⧉ copied {n} chars to clipboard"),
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        s.sel_anchor = None;
                    }
                    MouseEventKind::ScrollUp => { s.scroll = s.scroll.saturating_add(3); }
                    MouseEventKind::ScrollDown => { s.scroll = s.scroll.saturating_sub(3); }
                    _ => {}
                }
                return;
            }
            TerminalEvent::Key(k) => {
                if k.kind == KeyEventKind::Release { return; }
                let mut s = state_h.lock().unwrap();

            // Permission prompt
            if s.permission.is_some() {
                match k.code {
                    Char('y') | Char('Y') => {
                        let (rid, sid, _) = s.permission.take().unwrap();
                        cmd_h.send(ClientMessage::PermissionReply {
                            request_id: rid, session_id: sid, granted: true,
                        }).ok();
                        s.status = "ready".into();
                    }
                    Enter | Char('n') | Char('N') | Esc => {
                        let (rid, sid, _) = s.permission.take().unwrap();
                        cmd_h.send(ClientMessage::PermissionReply {
                            request_id: rid, session_id: sid, granted: false,
                        }).ok();
                        s.status = "ready".into();
                    }
                    _ => {}
                }
                return;
            }

            match k.code {
                Char('g') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.editor_pending = true;
                }
                Char('l') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.entries.clear();
                    s.scroll = 0;
                }
                Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.scroll = s.scroll.saturating_add(10);
                }
                Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.scroll = s.scroll.saturating_sub(10);
                }
                Up => { s.scroll = s.scroll.saturating_add(1); }
                Down => { s.scroll = s.scroll.saturating_sub(1); }
                PageUp => { s.scroll = s.scroll.saturating_add(10); }
                PageDown => { s.scroll = s.scroll.saturating_sub(10); }
                Enter => {
                    let content = std::mem::take(&mut s.prompt);
                    s.scroll = 0;
                    if content.trim().is_empty() { return; }
                    match content.as_str() {
                        ":q" | ":quit" => { s.running = false; }
                        "/models" => { cmd_h.send(ClientMessage::ListModels).ok(); return; }
                        "/quit" | "/exit" => { s.running = false; }
                        "/help" => {
                            s.entries.push(ChatEntry::System(
                                "/models · /model X · /mode X · /initial (onboarding) · /help · Ctrl+G editor · Ctrl+U/D scroll · :q quit".into(),
                            ));
                            return;
                        }
                        _ => {
                            if let Some(m) = content.strip_prefix("/model ") {
                                cmd_h.send(ClientMessage::SetModel {
                                    session_id: s.session_id, model: m.trim().into(),
                                }).ok();
                                return;
                            }
                            if let Some(m) = content.strip_prefix("/mode ") {
                                let mode = match m.trim() {
                                    "default" => PermissionMode::Default,
                                    "ask" => PermissionMode::Ask,
                                    "lockdown" => PermissionMode::Lockdown,
                                    "yolo" => PermissionMode::Yolo,
                                    _ => {
                                        s.entries.push(ChatEntry::System(format!("bad mode: {m}")));
                                        return;
                                    }
                                };
                                cmd_h.send(ClientMessage::SetPermissionMode {
                                    session_id: s.session_id, mode,
                                }).ok();
                                return;
                            }
                            if content.starts_with('/') {
                                s.entries.push(ChatEntry::System(format!("unknown: {content}")));
                                return;
                            }
                        }
                    }
                    s.entries.push(ChatEntry::User(content.clone()));
                    cmd_h.send(ClientMessage::ChatInput {
                        session_id: s.session_id, content,
                    }).ok();
                }
                Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => s.prompt.push(c),
                Backspace => { s.prompt.pop(); }
                Esc => { s.prompt.clear(); }
                _ => {}
            }
            }
        _ => {}
        }
    });

    // ── Render ──
    let s = state.lock().unwrap();
    let w = width.max(20) as usize;
    let h = height.max(5) as usize;

    let show_art = s.entries.len() < 3;
    let art_lines = if show_art { TEMPLE_ART.lines().count() } else { 0 };

    // Build all display lines
    let mut lines: Vec<StyledLine> = Vec::new();
    let content_width = w.saturating_sub(2);

    for entry in &s.entries {
        match entry {
            ChatEntry::User(text) => {
                for (i, l) in render_markdown_lite(text, content_width.saturating_sub(6)).iter().enumerate() {
                    lines.push(StyledLine {
                        text: if i == 0 { format!("you › {}", l.text) } else { format!("    {}", l.text) },
                        kind: l.kind,
                    });
                }
            }
            ChatEntry::Assistant { content, stats } => {
                let body = render_markdown_lite(content, content_width.saturating_sub(9));
                for (i, l) in body.iter().enumerate() {
                    let prefix = if i == 0 { "renco › " } else { "       " };
                    lines.push(StyledLine {
                        text: format!("{prefix}{}", l.text),
                        kind: l.kind,
                    });
                }
                if let Some(st) = stats {
                    lines.push(StyledLine {
                        text: format!("       ⏱ {st}"),
                        kind: LineKind::Dim,
                    });
                }
            }
            ChatEntry::System(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(StyledLine { text: format!("• {l}"), kind: LineKind::Dim });
                }
            }
            ChatEntry::Tool { name, status, detail } => {
                let icon = match status {
                    ToolStatus::Started => "⚙",
                    ToolStatus::Finished => "✓",
                    ToolStatus::Failed => "✗",
                };
                let d: String = detail.chars().take(80).collect();
                lines.push(StyledLine {
                    text: format!("  {icon} {name} {d}"),
                    kind: LineKind::Dim,
                });
            }
        }
    }

    // Scroll window
    let prompt_h = 3usize;
    let status_h = 1usize;
    let view_h = h.saturating_sub(prompt_h + status_h + art_lines);
    let total = lines.len();
    let max_scroll = total.saturating_sub(view_h);
    let scroll = s.scroll.min(max_scroll);
    let start = max_scroll.saturating_sub(scroll);
    let visible: Vec<StyledLine> = lines
        .into_iter()
        .skip(start)
        .take(view_h.max(1))
        .collect();

    // Store plain text of visible lines for mouse hit-testing.
    let mut s = s;
    s.last_lines = visible.iter().map(|l| l.text.clone()).collect();
    let selection = s.selection;
    let s_model = s.model.clone();
    let s_status = s.status.clone();
    let s_prompt = s.prompt.clone();
    let s_permission = s.permission.clone();
    let banner = match &s.banner {
        Some((text, at)) if at.elapsed() < std::time::Duration::from_secs(3) => {
            Some(text.clone())
        }
        _ => None,
    };
    drop(s);

    let mut children: Vec<AnyElement<'static>> = Vec::new();

    if show_art {
        for line in TEMPLE_ART.lines() {
            children.push(element! {
                View(height: 1u16) {
                    Text(content: line.to_string())
                }
            }.into());
        }
    }

    // Status line (banner takes over briefly)
    let status_text = if let Some(b) = banner {
        format!(" {b}")
    } else {
        let model_info = if s_model.is_empty() { String::new() } else { format!(" · {s_model}") };
        let scroll_info = if scroll > 0 { format!(" · ↑{scroll}") } else { String::new() };
        format!(" {s_status}{model_info}{scroll_info}")
    };
    children.push(element! {
        View(height: 1u16) {
            Text(content: status_text)
        }
    }.into());

    // Chat lines (highlight selection with a subtle background)
    for (i, l) in visible.iter().enumerate() {
        let in_sel = selection.map(|((sl, _), (el, _))| i >= sl && i <= el).unwrap_or(false);
        let color = match l.kind {
            LineKind::Normal => None,
            LineKind::Code => Some(Color::DarkGrey),
            LineKind::Dim => Some(Color::DarkGrey),
        };
        let elem = match (color, in_sel) {
            (Some(c), true) => element! {
                View(key: i as u64, height: 1u16, background_color: Color::DarkGrey) {
                    Text(content: l.text.clone(), color: Color::Black)
                }
            },
            (Some(c), false) => element! {
                View(key: i as u64, height: 1u16) {
                    Text(content: l.text.clone(), color: c)
                }
            },
            (None, true) => element! {
                View(key: i as u64, height: 1u16, background_color: Color::DarkGrey) {
                    Text(content: l.text.clone(), color: Color::Black)
                }
            },
            (None, false) => element! {
                View(key: i as u64, height: 1u16) {
                    Text(content: l.text.clone())
                }
            },
        };
        children.push(elem.into());
    }

    // Prompt
    let prompt_text = if let Some((_, _, ref path)) = s_permission {
        format!("Allow {path}? (y/N)")
    } else {
        format!("› {}", s_prompt)
    };
    children.push(element! {
        View(height: 1u16, border_style: BorderStyle::None) {
            Text(content: prompt_text)
        }
    }.into());
    children.push(element! {
        View(height: 1u16) {
            Text(content: "─".repeat(w))
        }
    }.into());

    element! {
        View(
            width,
            height,
            flex_direction: FlexDirection::Column,
        ) {
            #(children.into_iter())
        }
    }
}

fn main() {
    let cli = Cli::parse();
    cli.print_completions();

    let cwd = std::fs::canonicalize(&cli.cwd)
        .unwrap_or_else(|_| std::path::PathBuf::from(&cli.cwd));
    let cwd_str = cwd.to_string_lossy().to_string();
    let client_id = cli.client_id.unwrap_or_else(whoami::hostname);

    let state = Arc::new(Mutex::new(AppState {
        session_id: Uuid::nil(),
        entries: Vec::new(),
        status: "connecting…".into(),
        model: String::new(),
        prompt: String::new(),
        permission: None,
        running: true,
        editor_pending: false,
        scroll: 0,
        sel_anchor: None,
        selection: None,
        last_lines: Vec::new(),
        banner: None,
    }));

    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>();
    spawn_ws(cli.server, cwd_str, client_id, state.clone(), cmd_rx);

    smol::block_on(async {
        loop {
            {
                let s = state.lock().unwrap();
                if !s.running { break; }
                if s.editor_pending {
                    let prompt = s.prompt.clone();
                    drop(s);

                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
                    let tmp = std::env::temp_dir().join("temple_prompt.md");
                    std::fs::write(&tmp, &prompt).ok();
                    std::process::Command::new(&editor).arg(&tmp).status().ok();
                    let edited = std::fs::read_to_string(&tmp)
                        .unwrap_or_default().trim_end().to_string();
                    std::fs::remove_file(&tmp).ok();

                    let mut s = state.lock().unwrap();
                    s.prompt = edited;
                    s.editor_pending = false;
                    continue;
                }
            }

            let mut app = element! {
                Temple(
                    state: state.clone(),
                    cmd_tx: cmd_tx.clone(),
                )
            };

            if let Err(e) = app.fullscreen().enable_mouse_capture().await {
                eprintln!("temple: {e}");
                break;
            }
        }
    });

    let _ = cmd_tx.send(ClientMessage::CloseSession {
        session_id: state.lock().unwrap().session_id,
    });
}
