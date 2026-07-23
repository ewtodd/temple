use crossterm::{
    cursor,
    event::{self, poll, read},
    execute, terminal,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use temple_protocol::*;

use crate::client;
use crate::input;
use crate::state::{AppState, ChatEntry, InternalCmd, PromptKind, PromptState, SharedState};
use crate::tools::{execute_local_tool, mode_tag};
use crate::ui;

/// Try to read the user's default SSH public key for daemon-auth.
/// Returns None if no key is found (TUI/legacy clients fall back to token auth).
fn discover_pubkey() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.ssh/id_ed25519.pub"),
        format!("{home}/.ssh/id_rsa.pub"),
    ];
    for path in &candidates {
        if let Ok(key) = std::fs::read_to_string(path) {
            let trimmed = key.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

pub struct App {
    pub state: SharedState,
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    internal_tx: tokio::sync::mpsc::Sender<InternalCmd>,
    tick_count: u64,
}

impl App {
    pub fn new(
        cwd: String,
        client_id: String,
        token: Option<String>,
        do_continue: bool,
        force_tls: bool,
        server: String,
    ) -> Self {
        let state = Arc::new(Mutex::new(AppState::new(cwd.clone())));
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::channel::<InternalCmd>(32);

        // Spawn the WebSocket background thread
        let s = state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let pubkey = discover_pubkey();
                let sess = SessionOpen {
                    client_id: client_id.clone(),
                    cwd: std::fs::canonicalize(&cwd)
                        .unwrap_or_else(|_| std::path::PathBuf::from(&cwd))
                        .to_string_lossy()
                        .into(),
                    hostname: whoami::fallible::hostname()
                        .unwrap_or_else(|_| "unknown".into()),
                    username: whoami::username(),
                    daemon_pubkey: pubkey,
                };
                let (mut conn, _) =
                    match client::connect(&server, &token, sess, force_tls).await {
                        Ok(c) => c,
                        Err(e) => {
                            let mut s = s.lock().unwrap();
                            s.entries.push(ChatEntry::System(format!(
                                "Connection failed: {e}"
                            )));
                            s.status = "connection failed".into();
                            return;
                        }
                    };
                let tx = conn.write;
                let tx_ping = tx.clone();
                let tx_session = tx.clone();

                // Reader: server → shared state
                let s_clone = s.clone();
                let tx_task = tx_session.clone();
                tokio::spawn(async move {
                    let mut do_continue = do_continue;
                    while let Some(msg) = conn.incoming.recv().await {
                        // Handle ToolRequest outside the lock
                        if let ServerMessage::ToolRequest {
                            request_id,
                            session_id,
                            ref name,
                            ref args_json,
                        } = msg
                        {
                            let (cwd_str, active_sid, mode) = {
                                let g = s_clone.lock().unwrap();
                                (g.cwd.clone(), g.session_id, g.mode)
                            };
                            if session_id != active_sid {
                                let _ = tx_task.send(ClientMessage::ToolResult {
                                    request_id,
                                    session_id,
                                    result: "Error: tool request for inactive session"
                                        .into(),
                                });
                                continue;
                            }

                            if mode == PermissionMode::Yolo {
                                let result =
                                    execute_local_tool(name, args_json, &cwd_str).await;
                                let _ = tx_task.send(ClientMessage::ToolResult {
                                    request_id,
                                    session_id,
                                    result,
                                });
                                continue;
                            }

                            let args: serde_json::Value =
                                serde_json::from_str(args_json).unwrap_or_default();
                            let needs_consent = match name.as_str() {
                                "write_file" | "edit_file" => {
                                    let path =
                                        args["path"].as_str().unwrap_or("");
                                    match crate::tools::resolve_tool_path(path, &cwd_str)
                                    {
                                        Ok(resolved) => {
                                            !resolved.starts_with(&cwd_str)
                                        }
                                        Err(_) => true,
                                    }
                                }
                                "execute_command" => {
                                    let command =
                                        args["command"].as_str().unwrap_or("");
                                    !temple_protocol::command::is_safe_command(
                                        command,
                                    )
                                }
                                _ => false,
                            };

                            if needs_consent {
                                let text = match name.as_str() {
                                    "execute_command" => format!(
                                        "run: {}",
                                        args["command"].as_str().unwrap_or("")
                                    ),
                                    _ => format!(
                                        "{}: {}",
                                        name,
                                        args["path"].as_str().unwrap_or("")
                                    ),
                                };
                                let mut g = s_clone.lock().unwrap();
                                if g.permission.is_some() {
                                    let _ = tx_task.send(ClientMessage::ToolResult {
                                        request_id, session_id,
                                        result: format!("Error: {name} denied — another prompt is pending"),
                                    });
                                    continue;
                                }
                                g.permission = Some(PromptState {
                                    text,
                                    at: Instant::now(),
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

                            let result =
                                execute_local_tool(name, args_json, &cwd_str).await;
                            let _ = tx_task.send(ClientMessage::ToolResult {
                                request_id,
                                session_id,
                                result,
                            });
                            continue;
                        }

                        let mut s = s_clone.lock().unwrap();
                        match msg {
                            ServerMessage::SessionOpened {
                                session_id,
                                info: _,
                            } => {
                                s.session_id = session_id;
                                let cwd = s.cwd.clone();
                                s.entries.push(ChatEntry::System(format!(
                                    "{} \u{b7} {}@{}",
                                    cwd,
                                    whoami::username(),
                                    whoami::fallible::hostname()
                                        .unwrap_or_else(|_| "?".into()),
                                )));
                                s.status = "ready".into();
                            }
                            ServerMessage::ChatDelta {
                                session_id,
                                delta,
                                done,
                                ..
                            } => {
                                // Stale message for a session we switched away
                                // from — ignore rather than corrupt the view.
                                if session_id != s.session_id {
                                    continue;
                                }
                                if done {
                                    // stream finished; stats come separately
                                } else if let Some(ChatEntry::Assistant {
                                    content: ref mut c,
                                    ..
                                }) = s.entries.last_mut()
                                {
                                    c.push_str(&delta);
                                } else if !delta.trim().is_empty() {
                                    // Don't open an assistant bubble for
                                    // whitespace-only deltas — tool-call-only
                                    // rounds emit stray newlines.
                                    s.entries.push(ChatEntry::Assistant {
                                        content: delta,
                                        stats: None,
                                    });
                                }
                            }
                            ServerMessage::ChatError { error, .. } => {
                                s.working = false;
                                s.work_started = None;
                                s.entries.push(ChatEntry::Error(error));
                            }
                            ServerMessage::ChatStats {
                                model,
                                prompt_tokens,
                                completion_tokens,
                                duration_ms,
                                tokens_per_second,
                                ..
                            } => {
                                s.working = false;
                                s.work_started = None;
                                if !model.is_empty() && model != "auto" {
                                    s.model = model;
                                }
                                let stats = format!(
                                    "{prompt_tokens}+{completion_tokens} tk \
                                     {duration_ms}ms \
                                     {tokens_per_second:.0} t/s",
                                );
                                if let Some(ChatEntry::Assistant {
                                    stats: ref mut st,
                                    ..
                                }) = s
                                    .entries
                                    .iter_mut()
                                    .rev()
                                    .find(|e| {
                                        matches!(e, ChatEntry::Assistant { .. })
                                    })
                                {
                                    *st = Some(stats);
                                }
                            }
                            ServerMessage::ToolEvent {
                                name,
                                status,
                                detail,
                                ..
                            } => {
                                s.entries.push(ChatEntry::Tool {
                                    name,
                                    status,
                                    detail,
                                });
                            }
                            ServerMessage::ModelList { models } => {
                                s.available_models =
                                    models.iter().map(|m| m.id.clone()).collect();
                                s.entries.push(ChatEntry::System(format!(
                                    "models ({}): type /model <name> or use Tab to complete",
                                    models.len()
                                )));
                            }
                            ServerMessage::ModelChanged { model, .. } => {
                                s.model = model.clone();
                                s.entries.push(ChatEntry::System(format!(
                                    "model \u{2192} {model}"
                                )));
                            }
                            ServerMessage::PermissionRequired(r) => {
                                if r.session_id != s.session_id {
                                    continue;
                                }
                                s.permission = Some(PromptState {
                                    text: r.path.clone(),
                                    at: Instant::now(),
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
                                if session_id != s.session_id {
                                    continue;
                                }
                                s.mode = mode;
                                s.entries.push(ChatEntry::System(format!(
                                    "mode \u{2192} {}",
                                    mode_tag(mode)
                                )));
                            }
                            ServerMessage::TodoUpdate {
                                session_id,
                                items,
                            } => {
                                if session_id != s.session_id {
                                    continue;
                                }
                                if items.is_empty() {
                                    s.entries.retain(|e| {
                                        !matches!(e, ChatEntry::Todo { .. })
                                    });
                                    continue;
                                }
                                if let Some(ChatEntry::Todo {
                                    items: existing,
                                }) = s
                                    .entries
                                    .iter_mut()
                                    .rev()
                                    .find(|e| matches!(e, ChatEntry::Todo { .. }))
                                {
                                    *existing = items;
                                } else {
                                    s.entries.push(ChatEntry::Todo { items });
                                }
                            }
                            ServerMessage::Shutdown => {
                                s.working = false;
                                s.work_started = None;
                                s.entries
                                    .push(ChatEntry::System("server shutdown".into()));
                                s.running = false;
                            }
                            ServerMessage::ChatCancelled { .. } => {
                                s.working = false;
                                s.work_started = None;
                                s.entries.push(ChatEntry::System(
                                    "(cancelled by user)".into(),
                                ));
                            }
                            ServerMessage::ChatReset { session_id, .. } => {
                                if session_id != s.session_id {
                                    continue;
                                }
                                if let Some(pos) = s.entries.iter().rposition(|e| {
                                    matches!(e, ChatEntry::Assistant { .. })
                                }) {
                                    s.entries.truncate(pos);
                                }
                            }
                            ServerMessage::SessionList { sessions } => {
                                if do_continue {
                                    do_continue = false;
                                    // Resume the most recent session in the
                                    // same working directory — not globally.
                                    let cwd = s.cwd.clone();
                                    match sessions
                                        .iter()
                                        .filter(|m| m.cwd == cwd)
                                        .find(|m| m.id != s.session_id)
                                    {
                                        Some(prev) => {
                                            let id = prev.id;
                                            let target = prev
                                                .ssh_target
                                                .as_deref()
                                                .unwrap_or("quick")
                                                .to_string();
                                            let title = prev
                                                .title
                                                .as_deref()
                                                .unwrap_or("(untitled)")
                                                .to_string();
                                            s.entries.push(ChatEntry::System(
                                                format!(
                                                    "resuming most recent: {target} \u{b7} {title}"
                                                ),
                                            ));
                                            let _ = tx_task
                                                .send(ClientMessage::ResumeSession {
                                                    session_id: id,
                                                });
                                        }
                                        None => {
                                            s.entries.push(ChatEntry::System(
                                                "no previous sessions \u{2014} fresh start"
                                                    .into(),
                                            ));
                                        }
                                    }
                                } else if sessions.is_empty() {
                                    s.entries.push(ChatEntry::System(
                                        "no sessions \u{2014} /new [target] to start one"
                                            .into(),
                                    ));
                                } else {
                                    s.entries.push(ChatEntry::System(format!(
                                        "sessions ({}):",
                                        sessions.len()
                                    )));
                                    let target_w = sessions
                                        .iter()
                                        .map(|m| {
                                            m.ssh_target
                                                .as_deref()
                                                .unwrap_or("quick")
                                                .chars()
                                                .count()
                                        })
                                        .max()
                                        .unwrap_or(5)
                                        .min(20);
                                    for (i, m) in sessions.iter().enumerate() {
                                        let id8: String = m
                                            .id
                                            .simple()
                                            .to_string()
                                            .chars()
                                            .take(8)
                                            .collect();
                                        let target = m
                                            .ssh_target
                                            .as_deref()
                                            .unwrap_or("quick");
                                        let title = m
                                            .title
                                            .as_deref()
                                            .unwrap_or("(untitled)");
                                        let target_padded =
                                            format!("{target:<width$}", width = target_w);
                                        s.entries.push(ChatEntry::System(format!(
                                            "  [{i}] {id8} \u{b7} {target_padded} \u{b7} {title}"
                                        )));
                                    }
                                    s.entries.push(ChatEntry::System(
                                        "  resume: /session <n>".into(),
                                    ));
                                }
                                s.last_sessions = sessions;
                            }
                            ServerMessage::SessionResumed {
                                session_id,
                                meta,
                                transcript,
                            } => {
                                s.session_id = session_id;
                                s.working = false;
                                s.work_started = None;
                                s.mode = match meta.mode.as_str() {
                                    "ask" => PermissionMode::Ask,
                                    "lockdown" => PermissionMode::Lockdown,
                                    "yolo" => PermissionMode::Yolo,
                                    _ => PermissionMode::Default,
                                };
                                s.input_history = transcript
                                    .iter()
                                    .filter(|(role, _)| role == "user")
                                    .map(|(_, content)| content.clone())
                                    .collect();
                                s.history_pos = None;
                                let target =
                                    meta.ssh_target.as_deref().unwrap_or("quick");
                                let title =
                                    meta.title.as_deref().unwrap_or("(untitled)");
                                s.entries.clear();
                                s.last_total = 0;
                                s.last_lines.clear();
                                s.selection = None;
                                s.sel_anchor = None;
                                s.entries.push(ChatEntry::System(format!(
                                    "\u{1F4CB} session {target} \u{b7} {title}"
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
                            ServerMessage::SessionDeleted {
                                session_id: sid, ..
                            } => {
                                let id8: String = sid
                                    .simple()
                                    .to_string()
                                    .chars()
                                    .take(8)
                                    .collect();
                                s.entries.push(ChatEntry::System(format!(
                                    "deleted session {id8}"
                                )));
                                if sid == s.session_id {
                                    s.entries.clear();
                                    s.last_total = 0;
                                    s.last_lines.clear();
                                    s.selection = None;
                                    s.sel_anchor = None;
                                    s.mode = PermissionMode::Default;
                                    s.entries.push(ChatEntry::System(
                                        "(that was the active session \u{2014} starting fresh)"
                                            .into(),
                                    ));
                                    let _ = tx_task.send(ClientMessage::NewSession {
                                        ssh_target: None,
                                        start_dir: None,
                                    });
                                }
                            }
                            ServerMessage::Pong
                            | ServerMessage::SessionClosed { .. }
                            | ServerMessage::ToolResult { .. }
                            | ServerMessage::ToolRequest { .. } => {}
                        }
                    }
                });

                // Request model list for tab-completion on connect.
                let _ = tx_session.send(ClientMessage::ListModels);

                // Request session list on connect for --continue: resume the
                // most recent session in the same directory, not across cwds.
                if do_continue {
                    let _ = tx_session.send(ClientMessage::ListSessions);
                }

                // Writer: UI channel → socket, + pings
                loop {
                    tokio::select! {
                        cmd = cmd_rx.recv() => match cmd {
                            Some(msg) => {
                                if tx_ping.send(msg).is_err() {
                                    break;
                                }
                            }
                            None => break,
                        },
                        internal = internal_rx.recv() => match internal {
                            Some(InternalCmd::ExecuteLocalTool { name, args_json, cwd, responder }) => {
                                let result = execute_local_tool(&name, &args_json, &cwd).await;
                                let _ = responder.send(result);
                            }
                            None => break,
                        },
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            if tx_ping.send(ClientMessage::Ping).is_err() {
                                break;
                            }
                        },
                    }
                }
            });
        });

        App {
            state,
            cmd_tx,
            internal_tx,
            tick_count: 0,
        }
    }

    /// Run the main event loop. Returns Ok if the app exited normally.
    pub fn run(&mut self) -> io::Result<()> {
        // Terminal setup
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        // Ensure terminal is restored even on panic
        let _restore = RestoreGuard;

        terminal.clear()?;

        // Enter raw mode, alternate screen, enable mouse + bracketed paste
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            event::EnableMouseCapture,
            event::EnableBracketedPaste,
            cursor::Hide,
        )?;

        let tick_dur = Duration::from_millis(40);

        loop {
            // Check for editor_pending before blocking
            {
                let s = self.state.lock().unwrap();
                if !s.running {
                    break;
                }
                if s.editor_pending {
                    let prompt = s.prompt.clone();
                    drop(s);
                    self.handle_editor(prompt);
                    continue;
                }
            }

            // Time out stale permission prompts (300s)
            self.check_permission_timeout();

            // Poll for events with render-tick timeout
            if poll(tick_dur)? {
                let event = read()?;
                let chat_top = self.compute_chat_top();
                if !input::handle_event(
                    event,
                    &self.state,
                    &self.cmd_tx,
                    &self.internal_tx,
                    chat_top,
                ) {
                    break;
                }
            }

            // Check running after event handling
            {
                let s = self.state.lock().unwrap();
                if !s.running {
                    break;
                }
            }

            // Tick counter
            self.tick_count = self.tick_count.wrapping_add(1);

            // Render
            let mut visible_text = Vec::new();
            let _ = terminal.draw(|f| {
                let s = self.state.lock().unwrap();
                let (prompt_area, text) = ui::draw(f, &s, self.tick_count);
                visible_text = text;

                // Position cursor at prompt input position
                if let Some((cx, cy)) =
                    ui::cursor_position(&s, prompt_area, f.area().width as usize)
                {
                    f.set_cursor_position((cx, cy));
                }
            });

            // Store visible lines for mouse hit-testing
            {
                let mut s = self.state.lock().unwrap();
                s.last_lines = visible_text;
            }
        }

        // Send close-session before cleanup
        let sid = self.state.lock().unwrap().session_id;
        let _ = self
            .cmd_tx
            .send(ClientMessage::CloseSession { session_id: sid });

        // Cleanup
        terminal::disable_raw_mode()?;
        execute!(
            io::stdout(),
            terminal::LeaveAlternateScreen,
            event::DisableMouseCapture,
            event::DisableBracketedPaste,
            cursor::Show,
        )?;

        Ok(())
    }

    fn check_permission_timeout(&self) {
        let mut s = self.state.lock().unwrap();
        if let Some(ref pstate) = s.permission {
            if pstate.at.elapsed() > Duration::from_secs(300) {
                let kind = pstate.kind.clone();
                s.permission = None;
                s.status = "ready".into();
                s.entries.push(ChatEntry::System(
                    "permission timed out \u{2014} denied".into(),
                ));
                match kind {
                    PromptKind::Server {
                        request_id,
                        session_id,
                    } => {
                        self.cmd_tx
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
                        self.cmd_tx
                            .send(ClientMessage::ToolResult {
                                request_id,
                                session_id,
                                result: format!("Error: {name} timed out \u{2014} denied"),
                            })
                            .ok();
                    }
                }
            }
        }
    }

    fn compute_chat_top(&self) -> usize {
        let s = self.state.lock().unwrap();
        let show_art = s.entries.len() < 3;
        if show_art {
            let art_lines = crate::ui::TEMPLE_ART.lines().count();
            art_lines + 1 // +1 for key hints line
        } else {
            0
        }
    }

    fn handle_editor(&self, prompt: String) {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
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
                    let mut s = self.state.lock().unwrap();
                    s.editor_pending = false;
                    return;
                }
            }
        }
        std::process::Command::new(&editor).arg(&tmp).status().ok();
        let edited = std::fs::read_to_string(&tmp)
            .unwrap_or_default()
            .trim_end()
            .to_string();
        std::fs::remove_file(&tmp).ok();

        let mut s = self.state.lock().unwrap();
        s.prompt = edited;
        s.prompt_cursor = s.prompt.chars().count();
        s.editor_pending = false;
    }
}

/// RAII guard that restores the terminal on drop (even on panic).
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            terminal::LeaveAlternateScreen,
            event::DisableMouseCapture,
            event::DisableBracketedPaste,
            cursor::Show,
        );
    }
}
