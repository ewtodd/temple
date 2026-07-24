use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use std::time::Instant;
use temple_protocol::*;

use crate::render::insert_char_at;
use crate::state::{AppState, InternalCmd, PromptKind, SharedState};
use crate::tools::{next_mode, osc52_copy};

/// Braille spinner frames for the working indicator.
pub const SPINNER: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Handle a single crossterm input event. Returns `true` if the app
/// should continue running, `false` if it should exit.
pub fn handle_event(
    event: Event,
    state: &SharedState,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    internal_tx: &tokio::sync::mpsc::Sender<InternalCmd>,
    chat_top: usize,
) -> bool {
    match event {
        Event::Paste(text) => {
            let mut s = state.lock().unwrap();
            if s.permission.is_some() {
                return true;
            }
            for c in text.chars() {
                let i = s.prompt_cursor.min(s.prompt.chars().count());
                insert_char_at(&mut s.prompt, i, c);
                s.prompt_cursor += 1;
            }
        }
        Event::Key(key) => {
            if key.kind == KeyEventKind::Release {
                return true;
            }
            if !handle_key_event(key, state, cmd_tx, internal_tx) {
                return false;
            }
        }
        Event::Mouse(mouse) => {
            handle_mouse_event(mouse, state, chat_top);
        }
        Event::Resize(_, _) => {
            // Ratatui handles resize automatically via terminal.draw()
        }
        _ => {}
    }
    true
}

fn handle_mouse_event(mouse: crossterm::event::MouseEvent, state: &SharedState, chat_top: usize) {
    use crate::render::{extract_selection, norm_sel};
    let mut s = state.lock().unwrap();
    let line_idx = (mouse.row as usize).saturating_sub(chat_top);

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if line_idx < s.last_lines.len() {
                s.sel_anchor = Some((line_idx, mouse.column as usize));
                s.selection = None;
                s.banner = None;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(anchor) = s.sel_anchor {
                if line_idx < s.last_lines.len() {
                    let cur = (line_idx, mouse.column as usize);
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
                        format!("\u{29C9} copied {n} chars to clipboard"),
                        Instant::now(),
                    ));
                }
            }
            s.sel_anchor = None;
        }
        MouseEventKind::ScrollUp => {
            s.scroll = s.scroll.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            s.scroll = s.scroll.saturating_sub(3);
        }
        _ => {}
    }
}

/// Handle a keyboard event. Returns `false` if the app should exit.
fn handle_key_event(
    key: KeyEvent,
    state: &SharedState,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    internal_tx: &tokio::sync::mpsc::Sender<InternalCmd>,
) -> bool {
    let mut s = state.lock().unwrap();

    if s.permission.is_some() {
        let prompt = s.permission.clone().unwrap();
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                s.permission = None;
                s.status = "ready".into();
                match prompt.kind {
                    PromptKind::Server {
                        request_id,
                        session_id,
                    } => {
                        cmd_tx
                            .send(ClientMessage::PermissionReply {
                                request_id,
                                session_id,
                                granted: true,
                            })
                            .ok();
                    }
                    PromptKind::Local {
                        request_id,
                        session_id,
                        name,
                        args_json,
                    } => {
                        let cwd = s.cwd.clone();
                        let (responder_tx, responder_rx) =
                            std::sync::mpsc::sync_channel::<String>(1);
                        let _ = internal_tx.try_send(InternalCmd::ExecuteLocalTool {
                            name,
                            args_json,
                            cwd,
                            responder: responder_tx,
                        });
                        let result = responder_rx
                            .recv()
                            .unwrap_or_else(|_| "Error: tool execution failed".into());
                        let _ = cmd_tx.send(ClientMessage::ToolResult {
                            request_id,
                            session_id,
                            result,
                        });
                    }
                }
                return true;
            }
            KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                s.permission = None;
                s.status = "ready".into();
                match prompt.kind {
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
                                result: format!("Error: {name} denied by user"),
                            })
                            .ok();
                    }
                }
                return true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.permission = None;
                s.status = "ready".into();
                let cancel_sid = match prompt.kind {
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
                        session_id
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
                                result: format!("Error: {name} denied by user"),
                            })
                            .ok();
                        session_id
                    }
                };
                cmd_tx
                    .send(ClientMessage::CancelChat {
                        session_id: cancel_sid,
                    })
                    .ok();
                s.working = false;
                s.work_started = None;
                return true;
            }
            _ => return true,
        }
    }

    // ── Session search overlay (Ctrl+F) ──
    if s.session_search.is_some() {
        match key.code {
            KeyCode::Esc => {
                s.session_search = None;
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.session_search = None;
            }
            KeyCode::Enter => {
                if let Some(ref search) = s.session_search {
                    let search = search.clone();
                    let sid = s
                        .last_sessions
                        .iter()
                        .find(|m| {
                            let label = format!(
                                "{} — {}",
                                m.ssh_target.as_deref().unwrap_or("local"),
                                m.title.as_deref().unwrap_or("(untitled)")
                            );
                            label.to_lowercase().contains(&search.to_lowercase())
                        })
                        .map(|m| m.id);
                    s.session_search = None;
                    if let Some(id) = sid {
                        cmd_tx
                            .send(ClientMessage::ResumeSession { session_id: id })
                            .ok();
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(ref mut s) = s.session_search {
                    s.pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut s) = s.session_search {
                    s.push(c);
                }
            }
            _ => {}
        }
        return true;
    }

    match key.code {
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Fetch sessions if none loaded, then open search
            if s.last_sessions.is_empty() {
                cmd_tx.send(ClientMessage::ListSessions).ok();
            }
            s.session_search = Some(String::new());
            return true;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            cmd_tx
                .send(ClientMessage::CancelChat {
                    session_id: s.session_id,
                })
                .ok();
            s.working = false;
            s.work_started = None;
        }
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.running = false;
        }
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.editor_pending = true;
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.entries.clear();
            s.scroll = 0;
            s.last_total = 0;
            s.last_lines.clear();
            s.selection = None;
            s.sel_anchor = None;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Bash convention: Ctrl+U clears the prompt
            s.prompt.clear();
            s.prompt_cursor = 0;
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.scroll = s.scroll.saturating_sub(10);
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.scroll = s.scroll.saturating_add(10);
        }
        KeyCode::BackTab => {
            let next = next_mode(s.mode);
            cmd_tx
                .send(ClientMessage::SetPermissionMode {
                    session_id: s.session_id,
                    mode: next,
                })
                .ok();
        }
        KeyCode::Up => {
            if s.input_history.is_empty() {
                return true;
            }
            match s.history_pos {
                None => {
                    s.history_stash = s.prompt.clone();
                    s.history_pos = Some(s.input_history.len() - 1);
                }
                Some(0) => {}
                Some(p) => {
                    s.history_pos = Some(p - 1);
                }
            }
            if let Some(p) = s.history_pos {
                s.prompt = s.input_history[p].clone();
                s.prompt_cursor = s.prompt.chars().count();
            }
        }
        KeyCode::Down => match s.history_pos {
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
        },
        KeyCode::PageUp => {
            s.scroll = s.scroll.saturating_add(10);
        }
        KeyCode::PageDown => {
            s.scroll = s.scroll.saturating_sub(10);
        }
        KeyCode::Enter => {
            let content = std::mem::take(&mut s.prompt);
            s.prompt_cursor = 0;
            s.scroll = 0;
            s.history_pos = None;
            s.history_stash.clear();

            if content.trim().is_empty() {
                return true;
            }

            // Slash commands
            // Push to input history regardless of type (chat or command)
            if s.input_history.is_empty()
                || s.input_history
                    .last()
                    .map(|h| h != &content)
                    .unwrap_or(true)
            {
                s.input_history.push(content.clone());
            }

            let handled = handle_slash_command(&mut s, &content, cmd_tx);
            if handled {
                return true;
            }

            // Normal chat
            s.entries
                .push(crate::state::ChatEntry::User(content.clone()));
            s.working = true;
            s.work_started = Some(Instant::now());
            cmd_tx
                .send(ClientMessage::ChatInput {
                    session_id: s.session_id,
                    content,
                })
                .ok();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if c.is_control() && c != '\n' {
                return true;
            }
            let i = s.prompt_cursor.min(s.prompt.chars().count());
            insert_char_at(&mut s.prompt, i, c);
            s.prompt_cursor += 1;
        }
        KeyCode::Backspace => {
            let cur = s.prompt_cursor;
            if cur > 0 {
                crate::render::remove_char_at(&mut s.prompt, cur - 1);
                s.prompt_cursor = cur - 1;
            }
        }
        KeyCode::Delete => {
            let len = s.prompt.chars().count();
            let i = s.prompt_cursor.min(len);
            if i < len {
                crate::render::remove_char_at(&mut s.prompt, i);
            }
        }
        KeyCode::Left => {
            s.prompt_cursor = s.prompt_cursor.saturating_sub(1);
        }
        KeyCode::Right => {
            let len = s.prompt.chars().count();
            s.prompt_cursor = (s.prompt_cursor + 1).min(len);
        }
        KeyCode::Home => {
            s.prompt_cursor = 0;
        }
        KeyCode::End => {
            s.prompt_cursor = s.prompt.chars().count();
        }
        KeyCode::Esc => {
            s.prompt.clear();
            s.prompt_cursor = 0;
        }
        KeyCode::Tab => {
            // Tab on /model<space><prefix> cycles through models
            // Tab on /model (no space) cycles through commands like any other /
            if s.prompt.starts_with("/model ") {
                let models = s.available_models.clone();
                if models.is_empty() {
                    return true;
                }
                if let Some(prefix) = s.prompt.strip_prefix("/model ") {
                    let lower = prefix.to_lowercase();
                    // Exact match: cycle full list from that position
                    if let Some(pos) = models.iter().position(|m| m.as_str() == prefix) {
                        let next = (pos + 1) % models.len();
                        s.prompt = format!("/model {}", models[next]);
                        s.prompt_cursor = s.prompt.chars().count();
                    } else if lower.is_empty() {
                        // /model<space> (empty): pick first model
                        s.prompt = format!("/model {}", models[0]);
                        s.prompt_cursor = s.prompt.chars().count();
                    } else {
                        // Partial prefix: filter and show first match
                        if let Some(m) =
                            models.iter().find(|m| m.to_lowercase().starts_with(&lower))
                        {
                            s.prompt = format!("/model {}", m);
                            s.prompt_cursor = s.prompt.chars().count();
                        }
                    }
                }
                return true;
            }
            // Tab on / cycles through available commands alphabetically
            if s.prompt.starts_with('/') {
                let commands = [
                    "/clear",
                    "/delete ",
                    "/help",
                    "/mode ",
                    "/new ",
                    "/q",
                    "/quit",
                    "/session ",
                    "/sessions",
                ];
                let prefix = s.prompt.as_str();
                let current = commands.iter().position(|c| c == &prefix);
                let next = current.map(|i| (i + 1) % commands.len()).unwrap_or(0);
                s.prompt = commands[next].to_string();
                s.prompt_cursor = s.prompt.chars().count();
            }
        }
        _ => {}
    }
    true
}

/// Returns `true` if `content` was a slash command that was handled.
fn handle_slash_command(
    s: &mut AppState,
    content: &str,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<ClientMessage>,
) -> bool {
    if content == "/clear" {
        s.entries.clear();
        s.scroll = 0;
        s.last_total = 0;
        s.last_lines.clear();
        s.selection = None;
        s.sel_anchor = None;
        return true;
    }
    if content == "/help" || content == "/?" {
        let help_text = "\
Commands:                     Keys:
  /clear           clear chat    Ctrl+G   edit in $EDITOR
  /delete <n>      permanently   Ctrl+L   clear (same)
  /help            this help     Ctrl+C   cancel agent
  /mode <m>        permission    Ctrl+U   clear prompt
  /model <id>      switch model  Ctrl+J/K scroll by 10
  /new [target]    start new     PgUp/Dn  scroll by 10
  /q, /quit        exit          Shift+Tab cycle mode
  /session <n>     resume        Esc      clear prompt
  /sessions        list          Tab      cycle commands";
        s.entries
            .push(crate::state::ChatEntry::System(help_text.into()));
        return true;
    }
    if content == "/sessions" {
        cmd_tx.send(ClientMessage::ListSessions).ok();
        return true;
    }
    if let Some(n_str) = content.strip_prefix("/session ") {
        if let Ok(idx) = n_str.trim().parse::<usize>() {
            if let Some(meta) = s.last_sessions.get(idx) {
                let sid = meta.id;
                cmd_tx
                    .send(ClientMessage::ResumeSession { session_id: sid })
                    .ok();
                s.entries.push(crate::state::ChatEntry::System(format!(
                    "resuming session {}",
                    sid.simple().to_string().chars().take(8).collect::<String>()
                )));
            } else {
                s.entries.push(crate::state::ChatEntry::System(format!(
                    "no session at index {idx}"
                )));
            }
        }
        return true;
    }
    if let Some(n_str) = content.strip_prefix("/delete ") {
        if let Ok(idx) = n_str.trim().parse::<usize>() {
            if let Some(meta) = s.last_sessions.get(idx) {
                let sid = meta.id;
                cmd_tx
                    .send(ClientMessage::DeleteSession { session_id: sid })
                    .ok();
            } else {
                s.entries.push(crate::state::ChatEntry::System(format!(
                    "no session at index {idx}"
                )));
            }
        }
        return true;
    }
    if let Some(target) = content.strip_prefix("/new ") {
        let mut parts = target.splitn(2, ' ');
        let ssh_target = parts
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let dir = parts
            .next()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        cmd_tx
            .send(ClientMessage::NewSession {
                ssh_target,
                start_dir: dir,
            })
            .ok();
        return true;
    }
    if let Some(m) = content.strip_prefix("/mode ") {
        let mode = match m.trim() {
            "default" => PermissionMode::Default,
            "ask" => PermissionMode::Ask,
            "lockdown" => PermissionMode::Lockdown,
            "yolo" => PermissionMode::Yolo,
            _ => {
                s.entries
                    .push(crate::state::ChatEntry::System(format!("bad mode: {m}")));
                return true;
            }
        };
        cmd_tx
            .send(ClientMessage::SetPermissionMode {
                session_id: s.session_id,
                mode,
            })
            .ok();
        return true;
    }
    if let Some(model) = content.strip_prefix("/model ") {
        let m = model.trim();
        if m.is_empty() {
            cmd_tx.send(ClientMessage::ListModels).ok();
        } else {
            cmd_tx
                .send(ClientMessage::SetModel {
                    session_id: s.session_id,
                    model: m.into(),
                })
                .ok();
        }
        return true;
    }
    if content == "/q" || content == "/quit" || content == ":q" {
        s.running = false;
        return true;
    }
    if content.starts_with('/') {
        s.entries.push(crate::state::ChatEntry::System(format!(
            "unknown: {content}"
        )));
        return true;
    }
    false
}
