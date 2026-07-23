mod client;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{CommandFactory, Parser};
use clap_complete::{self, Shell};
use crossterm::event::MouseButton;
use iocraft::prelude::*;
use temple_protocol::*;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

const TEMPLE_ART: &str = include_str!("../../assets/temple.asc");

/// Braille spinner frames for the working indicator.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Parser)]
#[command(name = "temple", about = "renco TUI client")]
struct Cli {
    #[arg(short, long, default_value = "127.0.0.1:42123")]
    server: String,
    #[arg(short, long, default_value = ".")]
    cwd: String,
    #[arg(short = 'C', long)]
    client_id: Option<String>,
    /// Auth token (or set TEMPLE_TOKEN env var)
    #[arg(short = 't', long, env = "TEMPLE_TOKEN")]
    token: Option<String>,
    /// Resume the most recent persisted session on connect
    #[arg(long)]
    r#continue: bool,
    /// Use TLS (wss://) — set automatically if server starts with https://
    #[arg(long)]
    tls: bool,
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

/// A pending y/N prompt. Two kinds: server-side permission requests
/// (reply goes back over the wire) and client-side tool consent (the
/// server asked us to run something dangerous locally — the reply
/// executes or denies it right here).
#[derive(Debug, Clone)]
enum PromptKind {
    /// Server asked permission for a path/command — y/N goes to the server.
    Server { request_id: Uuid, session_id: Uuid },
    /// Server asked the CLIENT to execute something risky — y executes it
    /// locally and returns the ToolResult; N denies it.
    Local {
        request_id: Uuid,
        session_id: Uuid,
        name: String,
        args_json: String,
    },
}

#[derive(Debug, Clone)]
struct PromptState {
    text: String,
    at: std::time::Instant,
    kind: PromptKind,
}

/// Internal-only messages routed from the smol UI thread to the Tokio
/// WebSocket thread. These must never be serialized to the wire.
#[derive(Debug)]
enum InternalCmd {
    /// Execute a local tool on the Tokio thread and send the result
    /// string back via the responder channel.
    ExecuteLocalTool {
        name: String,
        args_json: String,
        cwd: String,
        responder: tokio::sync::mpsc::Sender<String>,
    },
}

#[derive(Debug, Clone)]
enum ChatEntry {
    /// User message
    User(String),
    /// Assistant message (streams in), with optional stats footer
    Assistant {
        content: String,
        stats: Option<String>,
    },
    /// System/info line (command output, session notices — cyan)
    System(String),
    /// Error line (red)
    Error(String),
    /// Tool activity
    Tool {
        name: String,
        status: ToolStatus,
        detail: String,
    },
    /// Session task list (replaced in place on each update)
    Todo {
        items: Vec<temple_protocol::TodoItem>,
    },
}

#[derive(Default)]
struct AppState {
    session_id: Uuid,
    entries: Vec<ChatEntry>,
    status: String,
    model: String,
    prompt: String,
    /// Character index into `prompt` (0..=prompt.chars().count())
    prompt_cursor: usize,
    permission: Option<PromptState>,
    running: bool,
    editor_pending: bool,
    /// Working directory for local tool execution
    cwd: String,
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
    /// previously sent prompts, recalled with Up/Down
    input_history: Vec<String>,
    /// position while cycling input history (None = editing new prompt)
    history_pos: Option<usize>,
    /// stashed in-progress prompt while cycling history
    history_stash: String,
    /// last session listing (for /session <n> selection)
    last_sessions: Vec<SessionMeta>,
    /// an agent loop is in flight (set on send, cleared on stats/error)
    working: bool,
    /// when the current agent loop started (for the elapsed indicator)
    work_started: Option<std::time::Instant>,
    /// total display lines at last render (scroll anchoring)
    last_total: usize,
    /// Current permission mode (from server), shown in the status bar and
    /// cycled with Shift+Tab.
    mode: PermissionMode,
}

// ── Background WebSocket thread ──

#[allow(clippy::too_many_arguments)] // Connection setup needs each of these.
fn spawn_ws(
    server: String,
    cwd: String,
    client_id: String,
    token: Option<String>,
    do_continue: bool,
    force_tls: bool,
    state: Arc<Mutex<AppState>>,
    ui_cmd: mpsc::Receiver<ClientMessage>,
    mut internal_rx: tokio::sync::mpsc::Receiver<InternalCmd>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let sess = SessionOpen {
                client_id,
                cwd: std::fs::canonicalize(&cwd)
                    .unwrap_or_else(|_| std::path::PathBuf::from(&cwd))
                    .to_string_lossy()
                    .into(),
                hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown".into()),
                username: whoami::username(),
            };
            let (mut conn, _) = match client::connect(&server, &token, sess, force_tls).await {
                Ok(c) => c,
                Err(e) => {
                    let mut s = state.lock().unwrap();
                    s.entries.push(ChatEntry::System(
                        format!("Connection failed: {e}"),
                    ));
                    s.status = "connection failed".into();
                    return;
                }
            };
            let tx = conn.write;
            let tx_ping = tx.clone();
            let tx_session = tx.clone();

            // Reader: server → shared state
            let s = state.clone();
            tokio::spawn(async move {
                let mut do_continue = do_continue;
                while let Some(msg) = conn.incoming.recv().await {
                    // Handle ToolRequest outside the lock to avoid holding
                    // std::sync::MutexGuard (which is !Send) across .await
                    if let ServerMessage::ToolRequest { request_id, session_id, ref name, ref args_json } = msg {
                        // Session binding: ignore tool requests for sessions
                        // other than the active one — a misbehaving server
                        // must not drive tools through a stale channel.
                        let (cwd, active_sid) = {
                            let g = s.lock().unwrap();
                            (g.cwd.clone(), g.session_id)
                        };
                        if session_id != active_sid {
                            let _ = tx_session.send(ClientMessage::ToolResult {
                                request_id, session_id,
                                result: "Error: tool request for inactive session".into(),
                            });
                            continue;
                        }

                        // ── Client-side consent gate ──
                        // The server's permission system gates server-side
                        // operations; this gate is the last line of defense
                        // for THIS machine. Reads are always allowed (low
                        // risk, and the server gates them anyway). Writes
                        // outside the sandbox root and non-safe commands get
                        // a local y/N prompt regardless of the server's mode.
                        let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
                        let needs_consent = match name.as_str() {
                            "write_file" | "edit_file" => {
                                let path = args["path"].as_str().unwrap_or("");
                                match resolve_tool_path(path, &cwd) {
                                    Ok(resolved) => !resolved.starts_with(&cwd),
                                    Err(_) => true,
                                }
                            }
                            "execute_command" => {
                                let command = args["command"].as_str().unwrap_or("");
                                !temple_protocol::command::is_safe_command(command)
                            }
                            _ => false,
                        };

                        if needs_consent {
                            let text = match name.as_str() {
                                "execute_command" => format!("run: {}", args["command"].as_str().unwrap_or("")),
                                _ => format!("{}: {}", name, args["path"].as_str().unwrap_or("")),
                            };
                            let mut g = s.lock().unwrap();
                            // One local prompt at a time — deny the newcomer
                            // rather than silently dropping the older one.
                            if g.permission.is_some() {
                                let _ = tx_session.send(ClientMessage::ToolResult {
                                    request_id, session_id,
                                    result: format!("Error: {name} denied — another prompt is pending"),
                                });
                                continue;
                            }
                            g.permission = Some(PromptState {
                                text,
                                at: std::time::Instant::now(),
                                kind: PromptKind::Local {
                                    request_id,
                                    session_id,
                                    name: name.clone(),
                                    args_json: args_json.clone(),
                                },
                            });
                            g.status = "permission? (y/N)".into();
                            continue;
                        }

                        let result = execute_local_tool(name, args_json, &cwd).await;
                        let _ = tx_session.send(ClientMessage::ToolResult { request_id, session_id, result });
                        continue;
                    }

                    let mut s = s.lock().unwrap();
                    match msg {
                        ServerMessage::SessionOpened { session_id, info } => {
                            s.session_id = session_id;
                            // NOTE: deliberately NOT trusting info.cwd as the
                            // tool sandbox root — the client confines tools to
                            // its own startup directory (s.cwd, set once at
                            // launch). A server-supplied root would let a
                            // compromised server authorize writes anywhere.
                            s.mode = info.permissions;
                            s.entries.push(ChatEntry::System(format!(
                                "session on {} · cwd: {}", info.hostname, info.cwd
                            )));
                            s.status = format!("renco · {}", info.hostname);
                            // Auto-resume the most recent session if --continue
                            if do_continue {
                                let _ = tx_session.send(ClientMessage::ListSessions);
                            }
                        }
                        ServerMessage::ChatDelta { session_id, delta, done, .. } => {
                            // Stale message for a session we switched away
                            // from — ignore rather than corrupt the view.
                            if session_id != s.session_id { continue; }
                            if done {
                                // stream finished; nothing to do (stats come separately)
                            } else if let Some(ChatEntry::Assistant { content, .. }) =
                                s.entries.last_mut()
                            {
                                content.push_str(&delta);
                            } else if !delta.trim().is_empty() {
                                // Don't open an assistant bubble for
                                // whitespace-only deltas — tool-call-only
                                // rounds emit stray newlines, leaving empty
                                // "renco ·" headers all over the scrollback.
                                s.entries.push(ChatEntry::Assistant {
                                    content: delta,
                                    stats: None,
                                });
                            }
                        }
                        ServerMessage::ChatError { session_id, error, .. } => {
                            if session_id != s.session_id { continue; }
                            s.working = false;
                            s.work_started = None;
                            s.entries.push(ChatEntry::Error(format!("error: {error}")));
                        }
                        ServerMessage::ChatStats {
                            session_id,
                            model,
                            prompt_tokens,
                            completion_tokens,
                            duration_ms,
                            context_length,
                            prefill_tps,
                            decode_tps,
                            ..
                        } => {
                            if session_id != s.session_id { continue; }
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
                                s.entries.iter_mut().rev()
                                    .find(|e| matches!(e, ChatEntry::Assistant { .. }))
                            {
                                *st = Some(stats);
                            }
                            s.model = model;
                            s.working = false;
                            s.work_started = None;
                        }
                        ServerMessage::ToolEvent { session_id, name, status, detail, .. } => {
                            if session_id != s.session_id { continue; }
                            // A Started entry is updated in place when its
                            // tool finishes — one line per tool call. Merge
                            // iff the most recent entry for THIS tool is
                            // still Started (an Assistant entry may have
                            // been pushed in between).
                            let can_merge = status != ToolStatus::Started && matches!(
                                s.entries.iter().rev().find(|e| matches!(
                                    e,
                                    ChatEntry::Tool { name: n, .. } if *n == name
                                )),
                                Some(ChatEntry::Tool { status: ToolStatus::Started, .. })
                            );
                            if can_merge {
                                if let Some(ChatEntry::Tool { status: st, detail: d, .. }) =
                                    s.entries.iter_mut().rev().find(|e| matches!(
                                        e,
                                        ChatEntry::Tool {
                                            name: n,
                                            status: ToolStatus::Started,
                                            ..
                                        } if *n == name
                                    ))
                                {
                                    *st = status;
                                    if !detail.is_empty() {
                                        *d = detail;
                                    }
                                }
                            } else {
                                s.entries.push(ChatEntry::Tool { name, status, detail });
                            }
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
                            if r.session_id != s.session_id { continue; }
                            s.permission = Some(PromptState {
                                text: r.path.clone(),
                                at: std::time::Instant::now(),
                                kind: PromptKind::Server {
                                    request_id: r.request_id,
                                    session_id: r.session_id,
                                },
                            });
                            s.status = "permission? (y/N)".into();
                        }
                        ServerMessage::PermissionResult(_) => {
                            s.permission = None;
                            s.status = "ready".into();
                        }
                        ServerMessage::ModeChanged { session_id, mode } => {
                            if session_id != s.session_id { continue; }
                            s.mode = mode;
                            s.entries.push(ChatEntry::System(format!("mode → {}", mode_tag(mode))));
                        }
                        ServerMessage::TodoUpdate { session_id, items } => {
                            if session_id != s.session_id { continue; }
                            if items.is_empty() {
                                // List cleared — drop the panel entirely.
                                s.entries.retain(|e| !matches!(e, ChatEntry::Todo { .. }));
                                continue;
                            }
                            // Replace the most recent todo panel in place so
                            // the list doesn't spam one block per update.
                            if let Some(ChatEntry::Todo { items: existing }) =
                                s.entries.iter_mut().rev().find(|e| matches!(e, ChatEntry::Todo { .. }))
                            {
                                *existing = items;
                            } else {
                                s.entries.push(ChatEntry::Todo { items });
                            }
                        }
                        ServerMessage::Shutdown => {
                            s.working = false;
                            s.work_started = None;
                            s.entries.push(ChatEntry::System("server shutdown".into()));
                            s.running = false;
                        }
                        ServerMessage::ChatCancelled { .. } => {
                            s.working = false;
                            s.work_started = None;
                            s.entries.push(ChatEntry::System("(cancelled by user)".into()));
                        }
                        ServerMessage::ChatReset { session_id, .. } => {
                            if session_id != s.session_id { continue; }
                            // Server is regenerating a message that failed
                            // mid-stream — drop the partial assistant entry
                            // and anything after it (e.g. interleaved tool
                            // lines from the failed attempt).
                            if let Some(pos) = s.entries.iter().rposition(
                                |e| matches!(e, ChatEntry::Assistant { .. })
                            ) {
                                s.entries.truncate(pos);
                            }
                        }
                        ServerMessage::SessionList { sessions } => {
                            if do_continue {
                                do_continue = false;
                                // OpenSession already created a fresh (empty)
                                // session which tops the recency list — skip
                                // it and resume the most recent real one.
                                match sessions.iter().find(|m| m.id != s.session_id) {
                                    Some(prev) => {
                                        let id = prev.id;
                                        let target = prev.ssh_target.as_deref()
                                            .unwrap_or("quick").to_string();
                                        let title = prev.title.as_deref()
                                            .unwrap_or("(untitled)").to_string();
                                        s.entries.push(ChatEntry::System(format!(
                                            "resuming most recent: {target} · {title}"
                                        )));
                                        let _ = tx_session.send(ClientMessage::ResumeSession {
                                            session_id: id,
                                        });
                                    }
                                    None => {
                                        s.entries.push(ChatEntry::System(
                                            "no previous sessions — fresh start".into(),
                                        ));
                                    }
                                }
                            } else if sessions.is_empty() {
                                s.entries.push(ChatEntry::System(
                                    "no sessions — /new [target] to start one".into(),
                                ));
                            } else {
                                s.entries.push(ChatEntry::System(format!(
                                    "sessions ({}):", sessions.len()
                                )));
                                // Aligned columns: id8 · target · title —
                                // target width is fixed so titles line up.
                                let target_w = sessions.iter()
                                    .map(|m| m.ssh_target.as_deref().unwrap_or("quick").chars().count())
                                    .max()
                                    .unwrap_or(5)
                                    .min(20);
                                for (i, m) in sessions.iter().enumerate() {
                                    let id8: String = m.id.simple().to_string()
                                        .chars().take(8).collect();
                                    let target = m.ssh_target.as_deref().unwrap_or("quick");
                                    let title = m.title.as_deref().unwrap_or("(untitled)");
                                    let target_padded = format!(
                                        "{target:<width$}",
                                        width = target_w
                                    );
                                    s.entries.push(ChatEntry::System(format!(
                                        "  [{i}] {id8} · {target_padded} · {title}"
                                    )));
                                }
                                s.entries.push(ChatEntry::System(
                                    "  resume: /session <n>".into(),
                                ));
                            }
                            s.last_sessions = sessions;
                        }
                        ServerMessage::SessionResumed { session_id, meta, transcript } => {
                            s.session_id = session_id;
                            s.working = false;
                            s.work_started = None;
                            s.mode = match meta.mode.as_str() {
                                "ask" => PermissionMode::Ask,
                                "lockdown" => PermissionMode::Lockdown,
                                "yolo" => PermissionMode::Yolo,
                                _ => PermissionMode::Default,
                            };
                            // Restore input history so ↑ recalls prompts
                            // typed in the resumed session.
                            s.input_history = transcript.iter()
                                .filter(|(role, _)| role == "user")
                                .map(|(_, content)| content.clone())
                                .collect();
                            s.history_pos = None;
                            let target = meta.ssh_target.as_deref().unwrap_or("quick");
                            let title = meta.title.as_deref().unwrap_or("(untitled)");
                            s.entries.clear();
                            s.last_total = 0;
                            s.last_lines.clear();
                            s.selection = None;
                            s.sel_anchor = None;
                            s.entries.push(ChatEntry::System(format!(
                                "📋 session {target} · {title}"
                            )));
                            for (role, content) in transcript {
                                if role == "user" {
                                    s.entries.push(ChatEntry::User(content));
                                } else {
                                    s.entries.push(ChatEntry::Assistant {
                                        content,
                                        stats: None,
                                    });
                                }
                            }
                            s.scroll = 0;
                        }
                        ServerMessage::SessionDeleted { session_id: sid, .. } => {
                            let id8: String = sid.simple().to_string()
                                .chars().take(8).collect();
                            s.entries.push(ChatEntry::System(format!("deleted session {id8}")));
                            // Deleting the ACTIVE session leaves the client
                            // pointing at a dead id — every subsequent message
                            // would 400 with "session not found". Drop back to
                            // a fresh server-side session instead.
                            if sid == s.session_id {
                                s.entries.clear();
                                s.last_total = 0;
                                s.last_lines.clear();
                                s.selection = None;
                                s.sel_anchor = None;
                                s.mode = PermissionMode::Default;
                                s.entries.push(ChatEntry::System(
                                    "(that was the active session — starting fresh)".into(),
                                ));
                                let _ = tx_session.send(ClientMessage::NewSession {
                                    ssh_target: None,
                                    start_dir: None,
                                });
                            }
                        }
                        ServerMessage::Pong | ServerMessage::SessionClosed { .. } | ServerMessage::ToolResult { .. } | ServerMessage::ToolRequest { .. } => {}
                    }
                }

                // Connection dropped (server died / socket closed) — clean
                // up so the UI doesn't show a spinner forever.
                {
                    let mut s = s.lock().unwrap();
                    s.working = false;
                    s.work_started = None;
                    s.permission = None;
                    if s.running {
                        s.entries.push(ChatEntry::System("disconnected from server".into()));
                        s.status = "disconnected".into();
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

            // Internal command executor: runs local tools requested by
            // the smol UI thread, and sends results back via responder.
            tokio::spawn(async move {
                while let Some(cmd) = internal_rx.recv().await {
                    match cmd {
                        InternalCmd::ExecuteLocalTool {
                            name,
                            args_json,
                            cwd,
                            responder,
                        } => {
                            let result = execute_local_tool(&name, &args_json, &cwd).await;
                            let _ = responder.send(result).await;
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

/// Extract selected text from visible lines. Selection columns are
/// display columns — convert to char indices per line.
fn extract_selection(lines: &[String], sel: ((usize, usize), (usize, usize))) -> String {
    let ((sl, sc), (el, ec)) = sel;
    if sl >= lines.len() {
        return String::new();
    }
    let el = el.min(lines.len() - 1);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(el + 1).skip(sl) {
        let chars: Vec<char> = line.chars().collect();
        let from = if i == sl {
            display_col_to_char_idx(line, sc).min(chars.len())
        } else {
            0
        };
        let to = if i == el {
            display_col_to_char_idx(line, ec).min(chars.len())
        } else {
            chars.len()
        };
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

/// Strip terminal-hostile content from streamed text: ANSI escape
/// sequences (tool output is often colorized — iocraft would strip them
/// at draw but they'd corrupt our width math first), tabs become spaces,
/// C0/C1 control chars (except \n) are dropped. Raw control bytes desync
/// iocraft's canvas model from the real terminal (row corruption).
/// Normalize a tool path against the session cwd and reject ones that
/// escape the working directory. For new files (write_file), walks up
/// until an existing parent dir is found, canonicalizes that, then
/// re-joins the tail — the resolved path is always under cwd or /tmp.
fn resolve_tool_path(path: &str, cwd: &str) -> Result<std::path::PathBuf, String> {
    resolve_tool_path_for(path, cwd, false)
}

/// `for_write` tightens the sandbox: /tmp is allowed for reads but never
/// for writes — world-writable shared dirs are too easy to abuse (clobber
/// another process's socket, drop a file a cron job executes).
fn resolve_tool_path_for(
    path: &str,
    cwd: &str,
    for_write: bool,
) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::path::Path::new(cwd).join(p)
    };

    // Walk up until we find an existing component — handles writing to
    // new files in new directories.
    let mut candidate = abs.clone();
    let mut suffix = std::path::PathBuf::new();
    loop {
        if candidate.exists() {
            break;
        }
        match candidate.parent().and_then(|parent| {
            if parent.as_os_str().is_empty() {
                None
            } else {
                Some(parent)
            }
        }) {
            Some(parent) => {
                suffix = candidate
                    .file_name()
                    .map(|n| std::path::PathBuf::from(n).join(&suffix))
                    .unwrap_or(suffix);
                candidate = parent.to_path_buf();
            }
            None => break,
        }
    }

    let canonical_base =
        std::fs::canonicalize(&candidate).map_err(|e| format!("cannot resolve {abs:?}: {e}"))?;
    let resolved = canonical_base.join(&suffix);
    let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| std::path::PathBuf::from(cwd));

    if resolved.starts_with(&cwd_canon) || (!for_write && resolved.starts_with("/tmp")) {
        Ok(resolved)
    } else {
        Err(format!(
            "{path:?} escapes working directory ({})",
            cwd_canon.display()
        ))
    }
}

/// Max bytes read from a file (protects the client from OOM on huge
/// files — the server truncates further anyway).
const MAX_READ_BYTES: usize = 1 << 20; // 1 MiB
/// Max bytes captured per stream from a command.
const MAX_CMD_OUTPUT: usize = 256 << 10; // 256 KiB
/// Client-side command timeout — a hung command must never wedge the
/// reader loop. The server caps at 120s for SSH; match that here.
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Execute a server-requested tool against the local filesystem, confined
/// to `cwd`. Consent for risky operations is handled by the caller (the
/// ToolRequest handler) — this function assumes the operation is allowed.
async fn execute_local_tool(name: &str, args_json: &str, cwd: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
    match name {
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            match resolve_tool_path(path, cwd) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => match tokio::fs::File::open(&resolved).await {
                    Err(e) => format!("Error: {e}"),
                    Ok(f) => {
                        use tokio::io::AsyncReadExt;
                        let mut buf = Vec::with_capacity(8192);
                        let mut capped = f.take(MAX_READ_BYTES as u64 + 1);
                        match capped.read_to_end(&mut buf).await {
                            Err(e) => format!("Error: {e}"),
                            Ok(_) => {
                                let truncated = buf.len() as u64 > MAX_READ_BYTES as u64;
                                buf.truncate(MAX_READ_BYTES);
                                let mut out = String::from_utf8_lossy(&buf).to_string();
                                if truncated {
                                    out.push_str("\n[truncated at 1 MiB]");
                                }
                                out.push_str(&format!("\n[read {path}]"));
                                out
                            }
                        }
                    }
                },
            }
        }
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            match resolve_tool_path_for(path, cwd, true) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => {
                    if let Some(parent) = resolved.parent() {
                        tokio::fs::create_dir_all(parent).await.ok();
                    }
                    match tokio::fs::write(&resolved, content).await {
                        Ok(()) => format!("wrote {} ({} bytes)", resolved.display(), content.len()),
                        Err(e) => format!("Error: {e}"),
                    }
                }
            }
        }
        "list_dir" => {
            let path = args["path"].as_str().unwrap_or(".");
            match resolve_tool_path(path, cwd) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => match tokio::fs::read_dir(&resolved).await {
                    Err(e) => format!("Error: {e}"),
                    Ok(mut entries) => {
                        let mut out = Vec::new();
                        while let Ok(Some(e)) = entries.next_entry().await {
                            let name = e.file_name().to_string_lossy().to_string();
                            let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                            out.push(format!("{}{}", name, if is_dir { "/" } else { "" }));
                        }
                        out.join("\n")
                    }
                },
            }
        }
        "execute_command" => {
            let command = args["command"].as_str().unwrap_or("");
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(CMD_TIMEOUT, child).await {
                Err(_) => format!("Error: command timed out after {}s", CMD_TIMEOUT.as_secs()),
                Ok(Err(e)) => format!("Error: {e}"),
                Ok(Ok(output)) => {
                    let cap = |bytes: &[u8]| -> String {
                        if bytes.len() > MAX_CMD_OUTPUT {
                            let mut s =
                                String::from_utf8_lossy(&bytes[..MAX_CMD_OUTPUT]).to_string();
                            s.push_str("\n[truncated]");
                            s
                        } else {
                            String::from_utf8_lossy(bytes).to_string()
                        }
                    };
                    let stdout = cap(&output.stdout);
                    let stderr = cap(&output.stderr);
                    let mut out = String::new();
                    if !stdout.is_empty() {
                        out.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        out.push_str(&format!("\n[stderr]\n{stderr}"));
                    }
                    if !output.status.success() {
                        out.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
                    }
                    if out.is_empty() {
                        out.push_str("(no output)");
                    }
                    out
                }
            }
        }
        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_str = args["old_str"].as_str().unwrap_or("");
            let new_str = args["new_str"].as_str().unwrap_or("");
            match resolve_tool_path_for(path, cwd, true) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => match tokio::fs::read_to_string(&resolved).await {
                    Err(e) => format!("Error: {e}"),
                    Ok(content) => {
                        if !content.contains(old_str) {
                            format!("Error: old_str not found in {}", resolved.display())
                        } else {
                            let count = content.matches(old_str).count();
                            if count > 1 {
                                format!("Error: old_str found {count} times — must be unique")
                            } else {
                                let mut n = content.replacen(old_str, new_str, 1);
                                if content.ends_with('\n') && !n.ends_with('\n') {
                                    n.push('\n');
                                }
                                match tokio::fs::write(&resolved, &n).await {
                                    Err(e) => format!("Error: {e}"),
                                    Ok(()) => format!(
                                        "edited {} (replaced 1 occurrence)",
                                        resolved.display()
                                    ),
                                }
                            }
                        }
                    }
                },
            }
        }
        _ => format!("Error: unknown tool {name}"),
    }
}

fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => out.push_str("    "),
            '\x1b' => match chars.peek() {
                // CSI: ESC [ params... final-byte (0x40–0x7E)
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... terminated by BEL or ESC \
                Some(']') => {
                    chars.next();
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if c == '\x07' || (prev == '\x1b' && c == '\\') {
                            break;
                        }
                        prev = c;
                    }
                }
                // Two-byte sequence: ESC x
                _ => {
                    chars.next();
                }
            },
            c if c.is_control() && c != '\n' => {}
            c => out.push(c),
        }
    }
    out
}

/// Map a terminal display column to a char index (wide chars occupy
/// two columns, so column ≠ char index on lines containing them).
fn display_col_to_char_idx(line: &str, col: usize) -> usize {
    let mut w = 0usize;
    for (i, ch) in line.chars().enumerate() {
        if w >= col {
            return i;
        }
        w += ch.width().unwrap_or(0);
    }
    line.chars().count()
}

/// Truncate to at most `max` display columns (wide chars count double).
fn truncate_to_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

struct StyledLine {
    text: String,
    kind: LineKind,
}

#[derive(PartialEq, Clone, Copy)]
enum LineKind {
    Normal,
    Code,
    Dim,
    System,
    Error,
    ToolStart,
    ToolDone,
    ToolFail,
    ToolDetail,
    Separator,
    UserHeader,
    AgentHeader,
    Stats,
}

/// Short text tag for the permission mode, used in the status bar and
/// /mode feedback. Matches the Signal bot's preamble tag.
fn mode_tag(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::Ask => "ask",
        PermissionMode::Lockdown => "lockdown",
        PermissionMode::Yolo => "YOLO",
    }
}

/// Cycle order for Shift+Tab.
fn next_mode(mode: PermissionMode) -> PermissionMode {
    match mode {
        PermissionMode::Default => PermissionMode::Ask,
        PermissionMode::Ask => PermissionMode::Lockdown,
        PermissionMode::Lockdown => PermissionMode::Yolo,
        PermissionMode::Yolo => PermissionMode::Default,
    }
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
            let display = sanitize(&format!("  {raw_line}"));
            out.push(StyledLine {
                text: truncate_to_width(&display, width),
                kind: LineKind::Code,
            });
        } else {
            // normal text: word-wrap
            for chunk in wrap_text(raw_line, width.max(10)) {
                out.push(StyledLine {
                    text: chunk,
                    kind: LineKind::Normal,
                });
            }
        }
    }

    if out.is_empty() {
        out.push(StyledLine {
            text: String::new(),
            kind: LineKind::Normal,
        });
    }
    out
}

/// Insert a character at a given char index in a String.
fn insert_char_at(s: &mut String, idx: usize, c: char) {
    let byte_idx = s.char_indices()
        .nth(idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    s.insert(byte_idx, c);
}

/// Remove the character at a given char index from a String.
fn remove_char_at(s: &mut String, idx: usize) {
    if let Some((byte_idx, ch)) = s.char_indices().nth(idx) {
        s.remove(byte_idx);
        let _ = ch;
    }
}

/// Wrap text to `width` display columns. Handles embedded newlines and
/// strips control characters — every display path funnels through here.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for piece in text.split('\n') {
        out.extend(wrap_piece(&sanitize(piece), width));
    }
    out
}

/// Word-wrap a single sanitized line by display width (not char count —
/// wide chars like emoji/CJK must match iocraft's own width math).
fn wrap_piece(line: &str, width: usize) -> Vec<String> {
    if line.width() <= width {
        return vec![line.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in line.split(' ') {
        let wlen = word.width();
        let need = if current.is_empty() { wlen } else { wlen + 1 };
        if current_w + need > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_w += 1;
        }
        // hard-split very long words by display columns
        if wlen > width {
            let mut chunk_w = 0usize;
            for ch in word.chars() {
                let cw = ch.width().unwrap_or(0);
                if chunk_w + cw > width {
                    lines.push(std::mem::take(&mut current));
                    chunk_w = 0;
                }
                current.push(ch);
                chunk_w += cw;
            }
            current_w = chunk_w;
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
    internal_tx: tokio::sync::mpsc::Sender<InternalCmd>,
}

impl Default for TempleProps {
    fn default() -> Self {
        let (tx, _) = mpsc::channel::<ClientMessage>();
        let (int_tx, _) = tokio::sync::mpsc::channel(32);
        Self {
            state: Arc::new(Mutex::new(AppState::default())),
            cmd_tx: tx,
            internal_tx: int_tx,
        }
    }
}

// ── Main component ──

#[component]
fn Temple(props: &TempleProps, mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let (width, height) = hooks.use_terminal_size();
    let mut system = hooks.use_context_mut::<SystemContext>();

    // Periodic re-render for streaming updates
    let tick = hooks.use_state(|| 0u64);
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
    let internal_tx = props.internal_tx.clone();

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
    let int_h = internal_tx.clone();
    hooks.use_terminal_events(move |event| {
        use KeyCode::*;
        match event {
            TerminalEvent::FullscreenMouse(m) => {
                let mut s = state_h.lock().unwrap();
                let chat_top = 1usize + if s.entries.len() < 3 { TEMPLE_ART.lines().count() + 1 } else { 0 };
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
            }
            TerminalEvent::Key(k) => {
                if k.kind == KeyEventKind::Release { return; }
                let mut s = state_h.lock().unwrap();

            // Permission prompt
            if s.permission.is_some() {
                let prompt = s.permission.clone().unwrap();
                match k.code {
                    Char('y') | Char('Y') => {
                        s.permission = None;
                        s.status = "ready".into();
                        match prompt.kind {
                            PromptKind::Server { request_id, session_id } => {
                                cmd_h.send(ClientMessage::PermissionReply {
                                    request_id, session_id, granted: true,
                                }).ok();
                            }
                            PromptKind::Local { request_id, session_id, name, args_json } => {
                                // Consent granted — delegate tool execution
                                // to the Tokio thread, then return result.
                                let cwd = s.cwd.clone();
                                let (responder_tx, mut responder_rx) =
                                    tokio::sync::mpsc::channel::<String>(1);
                                let _ = int_h.try_send(
                                    InternalCmd::ExecuteLocalTool {
                                        name,
                                        args_json,
                                        cwd,
                                        responder: responder_tx,
                                    },
                                );
                                let result = smol::block_on(responder_rx.recv())
                                    .unwrap_or_else(|| {
                                        "Error: tool execution failed".into()
                                    });
                                let _ = cmd_h.send(ClientMessage::ToolResult {
                                    request_id, session_id, result,
                                });
                            }
                        }
                    }
                    Enter | Char('n') | Char('N') | Esc => {
                        s.permission = None;
                        s.status = "ready".into();
                        match prompt.kind {
                            PromptKind::Server { request_id, session_id } => {
                                cmd_h.send(ClientMessage::PermissionReply {
                                    request_id, session_id, granted: false,
                                }).ok();
                            }
                            PromptKind::Local { request_id, session_id, name, .. } => {
                                cmd_h.send(ClientMessage::ToolResult {
                                    request_id, session_id,
                                    result: format!("Error: {name} denied by user"),
                                }).ok();
                            }
                        }
                    }
                    Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Deny + cancel the whole loop
                        s.permission = None;
                        s.status = "ready".into();
                        let cancel_sid = match prompt.kind {
                            PromptKind::Server { request_id, session_id } => {
                                cmd_h.send(ClientMessage::PermissionReply {
                                    request_id, session_id, granted: false,
                                }).ok();
                                session_id
                            }
                            PromptKind::Local { request_id, session_id, name, .. } => {
                                cmd_h.send(ClientMessage::ToolResult {
                                    request_id, session_id,
                                    result: format!("Error: {name} denied by user"),
                                }).ok();
                                session_id
                            }
                        };
                        cmd_h.send(ClientMessage::CancelChat {
                            session_id: cancel_sid,
                        }).ok();
                        s.working = false;
                        s.work_started = None;
                    }
                    _ => {}
                }
                return;
            }

            match k.code {
                Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Cancel in-progress agent loop (server replies with
                    // ChatCancelled, which adds the scrollback entry)
                    cmd_h.send(ClientMessage::CancelChat {
                        session_id: s.session_id,
                    }).ok();
                    s.working = false;
                    s.work_started = None;
                }
                Char('g') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.editor_pending = true;
                }
                Char('l') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.entries.clear();
                    s.scroll = 0;
                    s.last_total = 0;
                    s.last_lines.clear();
                    s.selection = None;
                    s.sel_anchor = None;
                }
                Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.scroll = s.scroll.saturating_add(10);
                }
                Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.scroll = s.scroll.saturating_sub(10);
                }
                BackTab => {
                    // Shift+Tab: cycle permission mode. The server confirms
                    // with ModeChanged, which updates the status bar tag.
                    let next = next_mode(s.mode);
                    cmd_h.send(ClientMessage::SetPermissionMode {
                        session_id: s.session_id,
                        mode: next,
                    }).ok();
                }
                Up => {
                    // Recall previous prompt (input history)
                    if s.input_history.is_empty() { return; }
                    match s.history_pos {
                        None => {
                            s.history_stash = s.prompt.clone();
                            s.history_pos = Some(s.input_history.len() - 1);
                        }
                        Some(0) => {}
                        Some(p) => { s.history_pos = Some(p - 1); }
                    }
                    if let Some(p) = s.history_pos {
                        s.prompt = s.input_history[p].clone();
                        s.prompt_cursor = s.prompt.chars().count();
                    }
                }
                Down => {
                    // Cycle forward in input history / restore stash
                    match s.history_pos {
                        None => {}
                        Some(p) if p + 1 < s.input_history.len() => {
                            s.history_pos = Some(p + 1);
                            s.prompt = s.input_history[p + 1].clone();
                            s.prompt_cursor = s.prompt.chars().count();
                        }
                        Some(_) => {
                            s.history_pos = None;
                            s.prompt = std::mem::take(&mut s.history_stash);
                            s.prompt_cursor = s.prompt.chars().count();
                        }
                    }
                }
                PageUp => { s.scroll = s.scroll.saturating_add(10); }
                PageDown => { s.scroll = s.scroll.saturating_sub(10); }
                Enter => {
                    let content = std::mem::take(&mut s.prompt);
                    s.scroll = 0;
                    s.history_pos = None;
                    if content.trim().is_empty() { return; }
                    if s.input_history.last() != Some(&content) {
                        s.input_history.push(content.clone());
                    }
                    match content.as_str() {
                        ":q" | ":quit" => { s.running = false; }
                        "/models" => { cmd_h.send(ClientMessage::ListModels).ok(); return; }
                        "/quit" | "/exit" => { s.running = false; }
                        "/stop" => {
                            cmd_h.send(ClientMessage::CancelChat {
                                session_id: s.session_id,
                            }).ok();
                            s.working = false;
                            s.work_started = None;
                            return;
                        }
                        "/sessions" => {
                            cmd_h.send(ClientMessage::ListSessions).ok();
                            return;
                        }
                        "/new" => {
                            cmd_h.send(ClientMessage::NewSession {
                                ssh_target: None,
                                start_dir: None,
                            }).ok();
                            return;
                        }
                        "/help" => {
                            s.entries.push(ChatEntry::System(
                                "/sessions · /session N · /new [target] [dir] · /delete N · /stop · /clear <user|account> · /models · /model X · /model auto · /mode X · /help · Shift+Tab cycle mode · ↑↓ input history · Ctrl+C cancel · Ctrl+G editor · Ctrl+U/D scroll · :q quit".into(),
                            ));
                            return;
                        }
                        _ => {
                            if let Some(m) = content.strip_prefix("/model ") {
                                let model = m.trim();
                                if model == "auto" {
                                    cmd_h.send(ClientMessage::SetModel {
                                        session_id: s.session_id,
                                        model: "auto".into(),
                                    }).ok();
                                } else {
                                    cmd_h.send(ClientMessage::SetModel {
                                        session_id: s.session_id, model: model.into(),
                                    }).ok();
                                }
                                return;
                            }
                            if let Some(arg) = content.strip_prefix("/session ") {
                                let arg = arg.trim();
                                // Accept an index from the last /sessions listing
                                // or a raw id prefix
                                let sid = if let Ok(n) = arg.parse::<usize>() {
                                    s.last_sessions.get(n).map(|m| m.id)
                                } else {
                                    s.last_sessions.iter()
                                        .find(|m| m.id.simple().to_string().starts_with(arg))
                                        .map(|m| m.id)
                                };
                                match sid {
                                    Some(id) => {
                                        cmd_h.send(ClientMessage::ResumeSession {
                                            session_id: id,
                                        }).ok();
                                    }
                                    None => {
                                        s.entries.push(ChatEntry::System(
                                            "no matching session — run /sessions first".into(),
                                        ));
                                    }
                                }
                                return;
                            }
                            if content == "/clear" {
                                s.entries.push(ChatEntry::System(
                                    "usage: /clear <user|account|target> — e.g. /clear e-play, /clear e-work@e-desktop, /clear ethan".into(),
                                ));
                                return;
                            }
                            if let Some(account) = content.strip_prefix("/clear ") {
                                cmd_h.send(ClientMessage::ClearSessions {
                                    account: account.trim().to_string(),
                                }).ok();
                                return;
                            }
                            if let Some(arg) = content.strip_prefix("/delete ") {
                                let arg = arg.trim();
                                let sid = if let Ok(n) = arg.parse::<usize>() {
                                    s.last_sessions.get(n).map(|m| m.id)
                                } else {
                                    s.last_sessions.iter()
                                        .find(|m| m.id.simple().to_string().starts_with(arg))
                                        .map(|m| m.id)
                                };
                                match sid {
                                    Some(id) => {
                                        cmd_h.send(ClientMessage::DeleteSession {
                                            session_id: id,
                                        }).ok();
                                        s.entries.push(ChatEntry::System(
                                            "session delete requested".into(),
                                        ));
                                    }
                                    None => {
                                        s.entries.push(ChatEntry::System(
                                            "no matching session — run /sessions first".into(),
                                        ));
                                    }
                                }
                                return;
                            }
                            if let Some(rest) = content.strip_prefix("/new ") {
                                // "/new target dir" — target is the first
                                // word, dir is the remainder (may contain
                                // spaces if quoted, but we keep it simple).
                                let mut parts = rest.trim().splitn(2, ' ');
                                let target = parts.next().unwrap_or("").to_string();
                                let dir = parts.next().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
                                cmd_h.send(ClientMessage::NewSession {
                                    ssh_target: Some(target),
                                    start_dir: dir,
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
                    s.working = true;
                    s.work_started = Some(std::time::Instant::now());
                    cmd_h.send(ClientMessage::ChatInput {
                        session_id: s.session_id, content,
                    }).ok();
                }
                Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if !c.is_control() {
                        let i = s.prompt_cursor.min(s.prompt.chars().count());
                        insert_char_at(&mut s.prompt, i, c);
                        s.prompt_cursor += 1;
                    }
                }
                Backspace => {
                    let cur = s.prompt_cursor;
                    if cur > 0 {
                        remove_char_at(&mut s.prompt, cur - 1);
                        s.prompt_cursor = cur - 1;
                    }
                }
                Delete => {
                    let len = s.prompt.chars().count();
                    let i = s.prompt_cursor.min(len);
                    if i < len {
                        remove_char_at(&mut s.prompt, i);
                    }
                }
                Left => {
                    s.prompt_cursor = s.prompt_cursor.saturating_sub(1);
                }
                Right => {
                    let len = s.prompt.chars().count();
                    s.prompt_cursor = (s.prompt_cursor + 1).min(len);
                }
                Home => {
                    s.prompt_cursor = 0;
                }
                End => {
                    s.prompt_cursor = s.prompt.chars().count();
                }
                Esc => {
                    s.prompt.clear();
                    s.prompt_cursor = 0;
                }
                _ => {}
            }
            }
        _ => {}
        }
    });

    // ── Render ──
    let tick_val = tick.get();
    let mut s = state.lock().unwrap();

    // Permission prompts time out server-side after 300s — mirror that
    // locally so the keyboard doesn't wedge if PermissionResult never
    // arrives (server restart, dropped connection mid-prompt).
    if let Some(pstate) = &s.permission {
        if pstate.at.elapsed() > Duration::from_secs(300) {
            let kind = pstate.kind.clone();
            s.permission = None;
            s.status = "ready".into();
            s.entries
                .push(ChatEntry::System("permission timed out — denied".into()));
            match kind {
                PromptKind::Server {
                    request_id,
                    session_id,
                } => {
                    cmd_tx
                        .send(ClientMessage::PermissionReply {
                            request_id,
                            session_id,
                            granted: false,
                        })
                        .ok();
                }
                PromptKind::Local {
                    request_id,
                    session_id,
                    name,
                    ..
                } => {
                    cmd_tx
                        .send(ClientMessage::ToolResult {
                            request_id,
                            session_id,
                            result: format!("Error: {name} timed out — denied"),
                        })
                        .ok();
                }
            }
        }
    }

    let w = width.max(20) as usize;
    let h = height.max(5) as usize;

    let show_art = s.entries.len() < 3;
    let art_lines = if show_art {
        TEMPLE_ART.lines().count()
    } else {
        0
    };

    // Build all display lines with visual separation
    let mut lines: Vec<StyledLine> = Vec::new();
    let content_width = w.saturating_sub(4);
    let sep = "─".repeat(w.saturating_sub(2));

    for (idx, entry) in s.entries.iter().enumerate() {
        // Separator before each message (except the very first)
        if idx > 0 {
            lines.push(StyledLine {
                text: sep.clone(),
                kind: LineKind::Separator,
            });
        }

        match entry {
            ChatEntry::User(text) => {
                lines.push(StyledLine {
                    text: " you".into(),
                    kind: LineKind::UserHeader,
                });
                for l in render_markdown_lite(text, content_width.saturating_sub(2)) {
                    lines.push(StyledLine {
                        text: format!(" {}", l.text),
                        kind: l.kind,
                    });
                }
            }
            ChatEntry::Assistant { content, stats } => {
                let model_tag = s.model.as_str();
                if model_tag.is_empty() {
                    lines.push(StyledLine {
                        text: " renco".into(),
                        kind: LineKind::AgentHeader,
                    });
                } else {
                    lines.push(StyledLine {
                        text: format!(" renco · {model_tag}"),
                        kind: LineKind::AgentHeader,
                    });
                }
                let body = render_markdown_lite(content, content_width.saturating_sub(2));
                for l in body.iter() {
                    lines.push(StyledLine {
                        text: format!(" {}", l.text),
                        kind: l.kind,
                    });
                }
                if let Some(st) = stats {
                    lines.push(StyledLine {
                        text: format!(" ⏱ {st}"),
                        kind: LineKind::Stats,
                    });
                }
            }
            ChatEntry::System(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(StyledLine {
                        text: format!(" {l}"),
                        kind: LineKind::System,
                    });
                }
            }
            ChatEntry::Error(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(StyledLine {
                        text: format!(" {l}"),
                        kind: LineKind::Error,
                    });
                }
            }
            ChatEntry::Tool {
                name,
                status,
                detail,
            } => {
                let (icon, kind) = match status {
                    ToolStatus::Started => ("⟳", LineKind::ToolStart),
                    ToolStatus::Finished => ("✓", LineKind::ToolDone),
                    ToolStatus::Failed => ("✗", LineKind::ToolFail),
                };
                lines.push(StyledLine {
                    text: format!("   {icon} {name}"),
                    kind,
                });
                if !detail.is_empty() {
                    // Smart truncation: single-line details show inline;
                    // multi-line output shows the first line plus a size
                    // hint, so a 5,000-line build log doesn't flood the
                    // scrollback with a 200-char slab of its beginning.
                    let d = if detail.contains('\n') && detail.len() > 120 {
                        let first = detail.lines().next().unwrap_or("");
                        let first: String = first.chars().take(100).collect();
                        format!("{first} … ({} chars)", detail.len())
                    } else {
                        detail.chars().take(200).collect()
                    };
                    for l in wrap_text(&d, content_width.saturating_sub(6)) {
                        lines.push(StyledLine {
                            text: format!("   │ {l}"),
                            kind: LineKind::ToolDetail,
                        });
                    }
                }
            }
            ChatEntry::Todo { items } => {
                let done = items
                    .iter()
                    .filter(|i| i.status == temple_protocol::TodoStatus::Done)
                    .count();
                lines.push(StyledLine {
                    text: format!("   tasks ({done}/{})", items.len()),
                    kind: LineKind::AgentHeader,
                });
                for item in items {
                    let (icon, kind) = match item.status {
                        temple_protocol::TodoStatus::Pending => ("▫", LineKind::Dim),
                        temple_protocol::TodoStatus::InProgress => ("▸", LineKind::ToolStart),
                        temple_protocol::TodoStatus::Done => ("✓", LineKind::ToolDone),
                    };
                    for (i, l) in wrap_text(&item.content, content_width.saturating_sub(6))
                        .iter()
                        .enumerate()
                    {
                        let prefix = if i == 0 {
                            format!("   {icon} ")
                        } else {
                            "     ".into()
                        };
                        lines.push(StyledLine {
                            text: format!("{prefix}{l}"),
                            kind,
                        });
                    }
                }
            }
        }
    }

    // Prompt box content — wrapped so the box can expand as you type.
    // Inner width: borders (2) + "│ " prefix (2).
    let prompt_inner_w = w.saturating_sub(4).max(1);
    let prompt_lines: Vec<String> = if let Some(ref pstate) = s.permission {
        wrap_text(
            &format!("Allow {}? (y/N)", sanitize(&pstate.text)),
            prompt_inner_w,
        )
    } else if s.prompt.is_empty() {
        vec![String::new()]
    } else {
        wrap_text(&sanitize(&s.prompt), prompt_inner_w)
    };
    // Grow up to 4 content rows, then scroll — the tail (cursor end)
    // stays visible.
    const MAX_PROMPT_ROWS: usize = 4;
    let shown_rows = prompt_lines.len().min(MAX_PROMPT_ROWS);
    let prompt_h = shown_rows + 2; // + border rows
    let mut prompt_window: Vec<String> =
        prompt_lines[prompt_lines.len() - shown_rows..].to_vec();

    // Solid cursor at the caret position — iocraft draws no terminal
    // cursor, so we render one ourselves. Hidden while a permission
    // prompt owns the box (the y/N answer is single-key, not typed).
    if s.permission.is_none() {
        // Map cursor char index to position in sanitized, wrapped text.
        // Sanitize strips control chars, so count visible chars up to
        // prompt_cursor to find the effective index.
        let cursor_vis: usize = s.prompt
            .chars()
            .take(s.prompt_cursor.min(s.prompt.chars().count()))
            .filter(|c| !c.is_control())
            .count();
        // Find which wrapped line and column the cursor falls on.
        let mut char_count = 0usize;
        let (cursor_line, cursor_col) = prompt_window.iter().enumerate().find_map(|(i, line)| {
            let line_len = line.chars().count();
            if char_count + line_len > cursor_vis {
                Some((i, cursor_vis - char_count))
            } else {
                char_count += line_len;
                None
            }
        }).unwrap_or_else(|| {
            // Cursor at the very end
            (prompt_window.len().max(1) - 1,
             prompt_window.last().map(|l| l.chars().count()).unwrap_or(0))
        });
        // Ensure the prompt window shows the line containing the cursor.
        let shown_rows = prompt_lines.len().min(MAX_PROMPT_ROWS);
        let cursor_from_end = prompt_lines.len().saturating_sub(cursor_line + 1);
        let start = if cursor_from_end < shown_rows {
            0
        } else {
            cursor_from_end - shown_rows + 1
        };
        prompt_window = prompt_lines[start..start + shown_rows].to_vec();
        // Place cursor in the correct line.
        let adj_line = cursor_line - start;
        if let Some(line) = prompt_window.get_mut(adj_line) {
            let col = cursor_col.min(line.chars().count());
            let byte_idx = line.char_indices().nth(col).map(|(i, _)| i).unwrap_or(line.len());
            line.insert_str(byte_idx, "▌");
        }
    }
    // Scroll window
    let status_h = 1usize;
    let view_h = h.saturating_sub(prompt_h + status_h + art_lines);
    let total = lines.len();
    let max_scroll = total.saturating_sub(view_h);
    // While scrolled up reading, pin the view to the same content: lines
    // arriving below bump the offset, lines vanishing (ChatReset) shrink it.
    let mut scroll = s.scroll.min(max_scroll);
    if scroll > 0 && total > s.last_total {
        scroll = (scroll + (total - s.last_total)).min(max_scroll);
    } else if scroll > 0 && total < s.last_total {
        scroll = scroll.saturating_sub(s.last_total - total).min(max_scroll);
    }
    s.scroll = scroll;
    s.last_total = total;
    let start = max_scroll.saturating_sub(scroll);
    let visible: Vec<StyledLine> = lines.into_iter().skip(start).take(view_h).collect();

    // Store plain text of visible lines for mouse hit-testing.
    s.last_lines = visible.iter().map(|l| l.text.clone()).collect();
    let selection = s.selection;
    let s_model = s.model.clone();
    let s_status = s.status.clone();
    let s_permission = s.permission.clone();
    let s_working = s.working;
    let s_work_started = s.work_started;
    let s_prompt_len = s.prompt.len();
    let s_mode = s.mode;
    let banner = match &s.banner {
        Some((text, at)) if at.elapsed() < std::time::Duration::from_secs(3) => Some(text.clone()),
        _ => None,
    };
    drop(s);

    let mut children: Vec<AnyElement<'static>> = Vec::new();

    if show_art {
        for line in TEMPLE_ART.lines() {
            children.push(
                element! {
                    View(height: 1u16, overflow: Overflow::Hidden) {
                        Text(content: line.to_string())
                    }
                }
                .into(),
            );
        }
        // Key hints under the art — the discoverability gap between a new
        // user staring at an empty chat and /help.
        children.push(element! {
            View(height: 1u16, overflow: Overflow::Hidden) {
                Text(content: "  /help · Shift+Tab mode · Ctrl+G editor · Ctrl+L clear".to_string(), color: Color::DarkGrey)
            }
        }.into());
    }

    // Status line (banner takes over briefly)
    let status_text = if let Some(b) = banner {
        format!(" {b}")
    } else if s_permission.is_some() {
        " permission? (y/N)".to_string()
    } else {
        let model_info = if s_model.is_empty() {
            String::new()
        } else {
            format!(" · {s_model}")
        };
        let mode_info = format!(" [{}]", mode_tag(s_mode));
        let scroll_info = if scroll > 0 {
            format!(" · ↑{scroll}")
        } else {
            String::new()
        };
        let char_info = if s_prompt_len > 0 && !s_working {
            format!(" · ch:{s_prompt_len}")
        } else {
            String::new()
        };
        if s_working {
            let frame = SPINNER[(tick_val as usize / 2) % SPINNER.len()];
            let secs = s_work_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            format!(" {frame} working… {secs}s{model_info}{mode_info}{scroll_info}")
        } else {
            format!(" {s_status}{model_info}{mode_info}{scroll_info}{char_info}")
        }
    };
    children.push(
        element! {
            View(height: 1u16, overflow: Overflow::Hidden) {
                Text(content: status_text)
            }
        }
        .into(),
    );

    // Chat lines (highlight selection with a subtle background)
    for (i, l) in visible.iter().enumerate() {
        let in_sel = selection
            .map(|((sl, _), (el, _))| i >= sl && i <= el)
            .unwrap_or(false);
        let color = match l.kind {
            // Everything here is a standard ANSI theme color — these follow
            // the terminal's base16 scheme, so the palette always matches
            // the rest of the desktop.
            LineKind::Normal => None,
            LineKind::Code => Some(Color::Grey),
            LineKind::Dim => Some(Color::DarkGrey),
            LineKind::System => Some(Color::Cyan),
            LineKind::Error => Some(Color::Red),
            LineKind::ToolStart => Some(Color::Yellow),
            LineKind::ToolDone => Some(Color::Green),
            LineKind::ToolFail => Some(Color::Red),
            LineKind::ToolDetail => Some(Color::DarkGrey),
            LineKind::Separator => Some(Color::DarkGrey),
            LineKind::UserHeader => Some(Color::Blue),
            LineKind::AgentHeader => Some(Color::Green),
            LineKind::Stats => Some(Color::Magenta),
        };
        let elem = match (color, in_sel) {
            (Some(_), true) => element! {
                View(key: i as u64, height: 1u16, overflow: Overflow::Hidden, background_color: Color::DarkGrey) {
                    Text(content: l.text.clone(), color: Color::Black)
                }
            },
            (Some(c), false) => element! {
                View(key: i as u64, height: 1u16, overflow: Overflow::Hidden) {
                    Text(content: l.text.clone(), color: c)
                }
            },
            (None, true) => element! {
                View(key: i as u64, height: 1u16, overflow: Overflow::Hidden, background_color: Color::DarkGrey) {
                    Text(content: l.text.clone(), color: Color::Black)
                }
            },
            (None, false) => element! {
                View(key: i as u64, height: 1u16, overflow: Overflow::Hidden) {
                    Text(content: l.text.clone())
                }
            },
        };
        children.push(elem.into());
    }

    // Prompt box — bordered, expands up to MAX_PROMPT_ROWS then scrolls.
    // Border color carries state: red while a permission prompt is up,
    // yellow while the agent works, dim otherwise. No forced background —
    // the box follows the terminal theme.
    let border_color = if s_permission.is_some() {
        Color::Red
    } else if s_working {
        Color::Yellow
    } else {
        Color::DarkGrey
    };
    children.push(
        element! {
            View(
                height: prompt_h as u16,
                overflow: Overflow::Hidden,
                border_style: BorderStyle::Round,
                border_color: border_color,
            ) {
                #(prompt_window.iter().enumerate().map(|(i, l)| element! {
                    View(key: i as u64, height: 1u16, overflow: Overflow::Hidden) {
                        Text(content: format!("│ {}", l))
                    }
                }.into()).collect::<Vec<AnyElement<'static>>>().into_iter())
            }
        }
        .into(),
    );

    element! {
        View(
            // Clamped like the wrap math — a 0/1-wide terminal otherwise
            // underflows iocraft's border-width arithmetic and panics.
            width: width.max(20),
            height: height.max(5),
            flex_direction: FlexDirection::Column,
        ) {
            #(children.into_iter())
        }
    }
}

fn main() {
    // Restore the terminal on panic — iocraft leaves the tty in raw mode +
    // alternate screen + mouse capture if anything unwinds, bricking the
    // user's shell until `reset`.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show
        );
        default_hook(info);
    }));

    let cli = Cli::parse();
    cli.print_completions();

    let cwd =
        std::fs::canonicalize(&cli.cwd).unwrap_or_else(|_| std::path::PathBuf::from(&cli.cwd));
    let cwd_str = cwd.to_string_lossy().to_string();
    let client_id = cli.client_id.unwrap_or_else(|| whoami::fallible::hostname().unwrap_or_else(|_| "unknown".into()));

    let state = Arc::new(Mutex::new(AppState {
        session_id: Uuid::nil(),
        entries: Vec::new(),
        status: "connecting…".into(),
        model: String::new(),
        prompt: String::new(),
        prompt_cursor: 0,
        permission: None,
        running: true,
        editor_pending: false,
        cwd: cwd_str.clone(),
        scroll: 0,
        sel_anchor: None,
        selection: None,
        last_lines: Vec::new(),
        banner: None,
        input_history: Vec::new(),
        history_pos: None,
        history_stash: String::new(),
        last_sessions: Vec::new(),
        working: false,
        work_started: None,
        last_total: 0,
        mode: PermissionMode::Default,
    }));

    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>();
    let (internal_tx, internal_rx) = tokio::sync::mpsc::channel::<InternalCmd>(32);
    spawn_ws(
        cli.server,
        cwd_str,
        client_id,
        cli.token,
        cli.r#continue,
        cli.tls,
        state.clone(),
        cmd_rx,
        internal_rx,
    );

    smol::block_on(async {
        loop {
            {
                let s = state.lock().unwrap();
                if !s.running {
                    break;
                }
                if s.editor_pending {
                    let prompt = s.prompt.clone();
                    drop(s);

                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
                    // Unique per-instance tempfile with owner-only perms —
                    // a fixed shared path is a symlink attack and two
                    // concurrent temple instances clobber each other.
                    let tmp = std::env::temp_dir().join(format!(
                        "temple_prompt_{}_{}.md",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    ));
                    {
                        use std::io::Write;
                        let mut opts = std::fs::OpenOptions::new();
                        opts.create_new(true).write(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            opts.mode(0o600);
                        }
                        match opts.open(&tmp) {
                            Ok(mut f) => {
                                f.write_all(prompt.as_bytes()).ok();
                            }
                            Err(_) => {
                                let mut s = state.lock().unwrap();
                                s.editor_pending = false;
                                continue;
                            }
                        }
                    }
                    std::process::Command::new(&editor).arg(&tmp).status().ok();
                    let edited = std::fs::read_to_string(&tmp)
                        .unwrap_or_default()
                        .trim_end()
                        .to_string();
                    std::fs::remove_file(&tmp).ok();

                    let mut s = state.lock().unwrap();
                    s.prompt = edited;
                    s.prompt_cursor = s.prompt.chars().count();
                    s.editor_pending = false;
                    continue;
                }
            }

            let mut app = element! {
                Temple(
                    state: state.clone(),
                    cmd_tx: cmd_tx.clone(),
                    internal_tx: internal_tx.clone(),
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
