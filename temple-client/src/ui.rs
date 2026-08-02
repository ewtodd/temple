use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::input::SPINNER;
use crate::state::{AppState, ChatEntry, PromptState};
use crate::tools::mode_tag;

/// Temple ASCII art banner.
pub const TEMPLE_ART: &str = include_str!("../../assets/temple.asc");

/// Build a Vec<Line> from ChatEntry items for the chat area.
fn build_chat_lines(s: &AppState, width: usize) -> Vec<Line<'static>> {
    use crate::render::{render_markdown, wrap_text};

    let mut lines: Vec<Line<'static>> = Vec::new();
    let content_width = width.saturating_sub(4);
    let sep = "\u{2500}".repeat(width.saturating_sub(2));

    for (idx, entry) in s.entries.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(Span::styled(
                sep.clone(),
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(String::new(), Style::default())));
        }

        match entry {
            ChatEntry::User(text) => {
                lines.push(
                    Line::from(vec![Span::styled(
                        "you",
                        Style::default()
                            .fg(Color::LightMagenta)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )])
                    .centered(),
                );
                for l in render_markdown(text, content_width.saturating_sub(2)) {
                    lines.push(l);
                }
            }
            ChatEntry::Assistant {
                content,
                reasoning,
                stats,
            } => {
                let model_tag = s.model.as_str();
                let header = if model_tag.is_empty() {
                    "renco".to_string()
                } else {
                    format!("renco \u{b7} {model_tag}")
                };
                lines.push(
                    Line::from(vec![Span::styled(
                        header,
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )])
                    .centered(),
                );
                // Reasoning/thinking content rendered dimmed and italic.
                // Wrap it so the pre-computed line count matches what the
                // chat Paragraph actually renders (keeps scroll & mouse in
                // sync — an unwrapped long line would break both).
                if let Some(ref r) = reasoning {
                    if !r.trim().is_empty() {
                        let wrapped = wrap_text(r, content_width.saturating_sub(2));
                        for (i, l) in wrapped.iter().enumerate() {
                            let prefix = if i == 0 { "\u{2026} " } else { "   " };
                            lines.push(Line::from(Span::styled(
                                format!("{prefix}{l}"),
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(ratatui::style::Modifier::ITALIC),
                            )));
                        }
                    }
                }
                let body = crate::render::render_markdown(content, content_width.saturating_sub(2));
                for l in body {
                    lines.push(l);
                }
                if let Some(st) = stats {
                    for l in wrap_text(&format!("\u{23F1} {st}"), content_width.saturating_sub(2)) {
                        lines.push(Line::from(Span::styled(
                            l,
                            Style::default().fg(Color::Magenta),
                        )));
                    }
                }
            }
            ChatEntry::System(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(Color::Cyan),
                    )));
                }
            }
            ChatEntry::Error(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
            ChatEntry::Tool {
                name,
                status,
                detail,
            } => {
                use temple_protocol::ToolStatus;
                let (icon, color) = match status {
                    ToolStatus::Started => ("\u{27F3}", Color::Yellow),
                    ToolStatus::Finished => ("\u{2713}", Color::Green),
                    ToolStatus::Failed => ("\u{2717}", Color::Red),
                };
                let wrapped =
                    wrap_text(&format!(" {icon} {name}"), content_width.saturating_sub(2));
                for (i, l) in wrapped.iter().enumerate() {
                    let prefix = if i == 0 { "" } else { "   " };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{l}"),
                        Style::default().fg(color),
                    )));
                }
                if !detail.is_empty() {
                    let d = if detail.contains('\n') && detail.len() > 120 {
                        let first = detail.lines().next().unwrap_or("");
                        let first: String = first.chars().take(100).collect();
                        format!("{first} \u{2026} ({} chars)", detail.len())
                    } else {
                        detail.chars().take(200).collect()
                    };
                    for l in wrap_text(&d, content_width.saturating_sub(4)) {
                        lines.push(Line::from(Span::styled(
                            format!(" \u{2502} {l}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            }
            ChatEntry::Todo { items } => {
                let done = items
                    .iter()
                    .filter(|i| i.status == temple_protocol::TodoStatus::Done)
                    .count();
                lines.push(Line::from(Span::styled(
                    format!(" tasks ({done}/{})", items.len()),
                    Style::default().fg(Color::Green),
                )));
                for item in items {
                    let (icon, color) = match item.status {
                        temple_protocol::TodoStatus::Pending => ("\u{25AB}", Color::DarkGray),
                        temple_protocol::TodoStatus::InProgress => ("\u{25B8}", Color::Yellow),
                        temple_protocol::TodoStatus::Done => ("\u{2713}", Color::Green),
                    };
                    for (i, l) in wrap_text(&item.content, content_width.saturating_sub(4))
                        .iter()
                        .enumerate()
                    {
                        let prefix = if i == 0 {
                            format!(" {icon} ")
                        } else {
                            "   ".into()
                        };
                        lines.push(Line::from(Span::styled(
                            format!("{prefix}{l}"),
                            Style::default().fg(color),
                        )));
                    }
                }
            }
        }
    }
    lines
}

/// Map the prompt cursor to a (line, col) pair within the wrapped display
/// lines of the sanitized prompt. Mirrors the render path exactly: newlines
/// split lines, tabs expand to 4 spaces, long words wrap at the width, and
/// spaces dropped at word-wrap boundaries belong to the previous line.
fn cursor_in_prompt_lines(prompt: &str, prompt_cursor: usize, width: usize) -> (usize, usize) {
    use crate::render::sanitize;
    use unicode_width::UnicodeWidthChar;

    let sanitized = sanitize(prompt);
    let width = width.max(1);

    // Map the raw cursor index to a char index within `sanitized` (tabs
    // expand to 4 chars, control chars except '\n' vanish).
    let mut sidx = 0usize;
    for c in prompt.chars().take(prompt_cursor) {
        match c {
            '\t' => sidx += 4,
            c if c.is_control() && c != '\n' => {}
            _ => sidx += 1,
        }
    }
    sidx = sidx.min(sanitized.chars().count());

    // Replicate wrap_text's segmentation (newline pieces + word wrapping),
    // recording the sanitized char range [start, end) of each display line.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0usize;
    for piece in sanitized.split('\n') {
        let piece_len = piece.chars().count();
        if piece_len == 0 {
            ranges.push((offset, offset));
        } else {
            let mut line_start = 0usize;
            let mut current_w = 0usize;
            let mut word_start = 0usize;
            for word in piece.split(' ') {
                let wlen = word.chars().count();
                let word_end = word_start + wlen;
                let need = if current_w == 0 { wlen } else { wlen + 1 };
                if current_w > 0 && current_w + need > width {
                    // The word starts a new line; the previous line ends at
                    // word_start - 1 (the space in between is dropped).
                    ranges.push((offset + line_start, offset + word_start - 1));
                    current_w = 0;
                    line_start = word_start;
                }
                if current_w > 0 {
                    current_w += 1;
                }
                if wlen > width {
                    // Words wider than the display are chunked across
                    // multiple lines, mirroring wrap_piece.
                    let mut chunk_w = 0usize;
                    for (chunk_start, ch) in (word_start..).zip(word.chars()) {
                        let cw = ch.width().unwrap_or(0);
                        if chunk_w + cw > width {
                            ranges.push((offset + line_start, offset + chunk_start));
                            line_start = chunk_start;
                            chunk_w = 0;
                        }
                        chunk_w += cw;
                    }
                    current_w = chunk_w;
                } else {
                    current_w += wlen;
                }
                word_start = word_end + 1;
            }
            ranges.push((offset + line_start, offset + word_start - 1));
        }
        offset += piece_len + 1;
    }
    if ranges.is_empty() {
        ranges.push((0, 0));
    }

    // Find the line that contains sidx. A cursor exactly on a line boundary
    // belongs to the end of the previous line (chunked long words), while a
    // cursor inside a gap (dropped space or '\n') sits at the end of the
    // line before the gap.
    let mut line = 0usize;
    for (i, &(start, _)) in ranges.iter().enumerate() {
        if start < sidx {
            line = i;
        } else if start == sidx {
            let prev_end = if i > 0 { ranges[i - 1].1 } else { 0 };
            if prev_end != sidx {
                line = i;
            }
        } else {
            break;
        }
    }
    let (start, end) = ranges[line];
    (
        line,
        sidx.saturating_sub(start).min(end.saturating_sub(start)),
    )
}

/// Return (cursor_x, cursor_y) in terminal coordinates for the prompt
/// input position, or None if the cursor should be hidden (permission
/// prompt active).
pub fn cursor_position(s: &AppState, prompt_area: Rect, width: usize) -> Option<(u16, u16)> {
    if s.permission.is_some() {
        return None;
    }
    if s.prompt.is_empty() {
        // Cursor at the start of the empty prompt box
        let inner_x = prompt_area.x + 2; // border + left padding
        let inner_y = prompt_area.y + 1; // top border
        return Some((inner_x, inner_y));
    }

    // Build the prompt lines exactly as they would be rendered
    let prompt_inner_w = width.saturating_sub(3).max(1);
    let sanitized = crate::render::sanitize(&s.prompt);
    let prompt_lines = crate::render::wrap_text(&sanitized, prompt_inner_w);

    // Find the display position of the cursor character index
    let (cursor_line, cursor_col) =
        cursor_in_prompt_lines(&s.prompt, s.prompt_cursor, prompt_inner_w);

    // Window into prompt_lines (max 8 rows)
    const MAX_PROMPT_ROWS: usize = 8;
    let shown_rows = prompt_lines.len().min(MAX_PROMPT_ROWS);
    let window_start = (cursor_line + 1)
        .saturating_sub(shown_rows)
        .min(prompt_lines.len().saturating_sub(shown_rows));
    let adj_line = cursor_line - window_start;

    // Terminal coords: border(1) + left padding(1) = 2
    let inner_x = prompt_area.x + 2;
    let inner_y = prompt_area.y + 1;

    // Column: count Unicode display width of chars before cursor
    let win_line = prompt_lines
        .get(window_start + adj_line)
        .cloned()
        .unwrap_or_default();
    let prefix: String = win_line
        .chars()
        .take(cursor_col.min(win_line.chars().count()))
        .collect();
    let col_offset = prefix.width() as u16;

    Some((inner_x + col_offset, inner_y + adj_line as u16))
}

/// Draw the entire UI. Returns (prompt_area, visible_chat_text) where
/// visible_chat_text is plain-text lines matching the rendered output
/// 1:1, used for mouse hit-testing and selection.
pub fn draw(f: &mut Frame, s: &mut AppState, tick_count: u64) -> (Rect, Vec<String>) {
    let area = f.area();
    let w = area.width as usize;
    let h = area.height as usize;

    // Split vertically: art (if showing), chat, prompt, status
    let show_art = s.entries.len() < 3;
    let art_lines = if show_art {
        TEMPLE_ART.lines().count()
    } else {
        0
    };
    let art_extra = if show_art && art_lines > 0 {
        art_lines + 1
    } else {
        0
    }; // +1 for key hints

    // Chat lines
    let all_chat_lines = build_chat_lines(s, w);

    // Scroll anchoring: when new content arrives at the bottom while the
    // user is scrolled up, adjust the scroll offset so the viewport stays
    // pinned to the same content instead of drifting down each frame.
    let total = all_chat_lines.len();
    if total > s.last_total && s.scroll > 0 {
        s.scroll = s.scroll.saturating_add(total - s.last_total);
    }
    s.last_total = total;

    let prompt_h = prompt_box_height(s, w);
    let status_h = 1usize;
    let chat_avail = h.saturating_sub(prompt_h + status_h + art_extra);

    // Clamp scroll to the valid range. Scrolling up further than the top of
    // the conversation must clamp at the top (max_scroll), never wrap around
    // to the bottom — otherwise the user can't reach the first message.
    let max_scroll = total.saturating_sub(chat_avail);
    if s.scroll > max_scroll {
        s.scroll = max_scroll;
    }
    let start = max_scroll.saturating_sub(s.scroll);
    let visible_chat: Vec<Line> = all_chat_lines
        .into_iter()
        .skip(start)
        .take(chat_avail.max(1))
        .collect();

    // Extract plain text for mouse selection BEFORE consuming with styles
    let visible_text: Vec<String> = visible_chat
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<Vec<&str>>()
                .concat()
        })
        .collect();

    // Apply selection highlighting
    let visible_chat: Vec<Line> = if let Some(((sl, _), (el, _))) = s.selection {
        visible_chat
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let (top, bot) = if sl <= el { (sl, el) } else { (el, sl) };
                if i >= top && i <= bot {
                    line.patch_style(Style::default().bg(Color::DarkGray).fg(Color::Black))
                } else {
                    line
                }
            })
            .collect()
    } else {
        visible_chat
    };

    // Store visible line plain-text for mouse selection
    // (done externally after draw — we store in a way the caller can access)
    // We return the prompt_area so caller can compute cursor position

    // Layout
    let mut constraints = Vec::new();
    if art_extra > 0 {
        constraints.push(Constraint::Length(art_extra as u16));
    }
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(prompt_h as u16));
    constraints.push(Constraint::Length(1));
    let layout = Layout::vertical(constraints).split(area);

    let art_idx_offset = if art_extra > 0 { 1 } else { 0 };
    let chat_area = layout[art_idx_offset];
    let prompt_area = layout[art_idx_offset + 1];
    let status_area = layout[art_idx_offset + 2];

    // Art
    if art_extra > 0 {
        let art_area = layout[0];

        // Compute the max displayed width of all banner lines, then create
        // a centered sub-Rect so the whole block is centered as one unit.
        let raw_lines: Vec<&str> = TEMPLE_ART.trim_end_matches('\n').lines().collect();
        let banner_art_lines = raw_lines.len();
        let max_banner_width = raw_lines.iter().map(|l| l.width()).max().unwrap_or(0);
        let banner_width = (max_banner_width as u16).min(art_area.width);
        let banner_left_pad = (art_area.width.saturating_sub(banner_width)) / 2;
        let banner_rect = Rect {
            x: art_area.x + banner_left_pad,
            y: art_area.y,
            width: banner_width,
            height: (banner_art_lines + 1) as u16, // +1 for hint line
        };

        // Render banner lines left-aligned inside the centered rect
        let art_text: Vec<Line> = raw_lines
            .iter()
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ))
            })
            .collect();
        f.render_widget(Paragraph::new(art_text), banner_rect);

        // Hint line centered across the full art area
        let hint_area = Rect {
            x: art_area.x,
            y: art_area.y + banner_art_lines as u16,
            width: art_area.width,
            height: 1,
        };
        let hint = Line::from(Span::styled(
            "/help \u{b7} Shift+Tab mode \u{b7} Ctrl+G editor \u{b7} Ctrl+L clear".to_string(),
            Style::default().fg(Color::DarkGray),
        ))
        .centered();
        f.render_widget(Paragraph::new(hint), hint_area);
    }
    // Chat
    let chat_para = Paragraph::new(visible_chat).wrap(Wrap { trim: false });
    f.render_widget(chat_para, chat_area);

    // Prompt
    draw_prompt(f, s, prompt_area, w);

    // Status
    draw_status(f, s, status_area, tick_count);

    // Session search overlay (Ctrl+F)
    if let Some(ref search) = s.session_search {
        let sessions = &s.last_sessions;
        let filtered: Vec<&temple_protocol::SessionMeta> = if search.is_empty() {
            sessions.iter().collect()
        } else {
            sessions
                .iter()
                .filter(|m| {
                    let label = format!(
                        "{} — {}",
                        m.username,
                        m.title.as_deref().unwrap_or("(untitled)")
                    );
                    label.to_lowercase().contains(&search.to_lowercase())
                })
                .collect()
        };

        let max_h = (filtered.len() + 3).min(16);
        let popup_w = (f.area().width as usize / 2).clamp(30, 60);
        let popup_h = max_h as u16;
        let popup_x = (f.area().width.saturating_sub(popup_w as u16)) / 2;
        let popup_y = (f.area().height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect {
            x: popup_x,
            y: popup_y,
            width: popup_w as u16,
            height: popup_h,
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" sessions — Ctrl+F to close ".to_string())
            .border_style(Style::default().fg(Color::Cyan));
        f.render_widget(block.clone(), popup_area);

        let inner = block.inner(popup_area);
        let lines: Vec<Line> = if filtered.is_empty() {
            vec![Line::from(Span::styled(
                " no matching sessions",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            let idx = s.session_search_idx.min(filtered.len().saturating_sub(1));
            // Window the list around the selection so the highlighted row
            // stays visible when there are more sessions than the popup rows.
            let view_h = inner.height as usize;
            let start = if filtered.len() > view_h {
                idx.saturating_sub(view_h / 2).min(filtered.len() - view_h)
            } else {
                0
            };
            filtered
                .iter()
                .enumerate()
                .skip(start)
                .take(view_h)
                .map(|(i, m)| {
                    let title = m.title.as_deref().unwrap_or("(untitled)");
                    let id8: String = m.id.simple().to_string().chars().take(8).collect();
                    let hl = i == idx;
                    let sid_style = if hl {
                        Style::default().bg(Color::DarkGray).fg(Color::White)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let user_style = if hl {
                        Style::default().bg(Color::DarkGray).fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    let title_style = if hl {
                        Style::default().bg(Color::DarkGray).fg(Color::White)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    Line::from(vec![
                        Span::styled(format!(" {id8}  "), sid_style),
                        Span::styled(m.username.clone(), user_style),
                        Span::styled(format!(" — {title}"), title_style),
                    ])
                })
                .collect()
        };
        f.render_widget(Paragraph::new(lines), inner);

        // Clear the fg color for subsequent rendering
        f.buffer_mut().set_style(
            popup_area,
            Style::default().bg(Color::Reset).fg(Color::Reset),
        );
    }

    (prompt_area, visible_text)
}

fn prompt_box_height(s: &AppState, width: usize) -> usize {
    if s.prompt.is_empty() && s.permission.is_none() {
        return 3; // border + empty line + border
    }
    if let Some(ref pstate) = s.permission {
        let lines =
            crate::render::wrap_text(&permission_text(pstate), width.saturating_sub(3).max(1));
        return lines.len().min(MAX_PROMPT_ROWS) + 2; // +2 for borders
    }
    let prompt_inner_w = width.saturating_sub(3).max(1);
    let prompt_lines =
        crate::render::wrap_text(&crate::render::sanitize(&s.prompt), prompt_inner_w);
    const MAX_PROMPT_ROWS: usize = 8;
    let shown = prompt_lines.len().min(MAX_PROMPT_ROWS);
    shown + 2 // +2 for borders
}

/// Format the permission prompt line, truncating very long payloads (e.g.
/// huge shell commands) so the prompt box can never grow taller than the
/// terminal and squeeze the chat area out.
fn permission_text(pstate: &PromptState) -> String {
    let raw = crate::render::sanitize(&pstate.text);
    let body = if raw.chars().count() > 200 {
        format!(
            "{} \u{2026} ({} chars)",
            raw.chars().take(180).collect::<String>(),
            raw.chars().count()
        )
    } else {
        raw
    };
    format!("Allow {body}? (y/N)")
}

fn draw_prompt(f: &mut Frame, s: &AppState, area: Rect, width: usize) {
    let border_color = if s.permission.is_some() {
        Color::Red
    } else if s.working {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .padding(ratatui::widgets::Padding::new(1, 0, 0, 0));
    let inner_area = block.inner(area);
    f.render_widget(block, area);

    let prompt_inner_w = width.saturating_sub(3).max(1);

    if let Some(ref pstate) = s.permission {
        let lines = crate::render::wrap_text(&permission_text(pstate), prompt_inner_w);
        let display: Vec<Line> = lines
            .into_iter()
            .take(MAX_PROMPT_ROWS)
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::White))))
            .collect();
        f.render_widget(Paragraph::new(display), inner_area);
        return;
    }

    let prompt_lines: Vec<String> = if s.prompt.is_empty() {
        vec![String::new()]
    } else {
        crate::render::wrap_text(&crate::render::sanitize(&s.prompt), prompt_inner_w)
    };

    const MAX_PROMPT_ROWS: usize = 8;
    let shown_rows = prompt_lines.len().min(MAX_PROMPT_ROWS);

    // Find cursor position in the full prompt_lines (multi-line aware)
    let (cursor_line, _) = cursor_in_prompt_lines(&s.prompt, s.prompt_cursor, prompt_inner_w);

    // Ensure the visible window contains the cursor line
    let window_start = (cursor_line + 1)
        .saturating_sub(shown_rows)
        .min(prompt_lines.len().saturating_sub(shown_rows));
    let window: Vec<&str> = prompt_lines[window_start..window_start + shown_rows]
        .iter()
        .map(|s| s.as_str())
        .collect();

    let cmd_mode = s.prompt.starts_with('/');
    let prompt_color = if cmd_mode { Color::Cyan } else { Color::White };
    let display: Vec<Line> = window
        .iter()
        .map(|line| Line::from(Span::styled(*line, Style::default().fg(prompt_color))))
        .collect();
    f.render_widget(Paragraph::new(display), inner_area);
}

fn draw_status(f: &mut Frame, s: &AppState, area: Rect, tick_count: u64) {
    let status_text = if let Some((ref text, at)) = s.banner {
        if at.elapsed() < std::time::Duration::from_secs(3) {
            format!(" {text}")
        } else {
            String::new()
        }
    } else if s.permission.is_some() {
        " permission? (y/N)".to_string()
    } else {
        // Connection/session state (e.g. "connecting…", "disconnected") —
        // never show the trivial "ready" state.
        let state_str = if s.status.is_empty() || s.status == "ready" {
            String::new()
        } else {
            format!(" {}", s.status)
        };
        let model_info = if s.model.is_empty() {
            String::new()
        } else {
            format!(" | {}", s.model)
        };
        let mode = mode_tag(s.mode);
        let mode_str = if s.mode == temple_protocol::PermissionMode::Default {
            String::new()
        } else {
            format!(" ({mode})")
        };
        let spinner = if s.working {
            let idx = tick_count as usize % SPINNER.len();
            format!(" {}", SPINNER[idx])
        } else {
            String::new()
        };
        let elapsed = if let (true, Some(started)) = (s.working, s.work_started) {
            let secs = started.elapsed().as_secs();
            format!(" {:02}:{:02}", secs / 60, secs % 60)
        } else {
            String::new()
        };
        let scroll_indicator = if s.scroll > 0 {
            format!(" up:{}", s.scroll)
        } else {
            String::new()
        };
        format!("{state_str}{spinner}{elapsed}{model_info}{mode_str}{scroll_indicator}")
    };

    let style = if s.working {
        Style::default().fg(Color::Yellow)
    } else if s.permission.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let line = Line::from(Span::styled(status_text, style));
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_single_line() {
        assert_eq!(cursor_in_prompt_lines("hello", 0, 40), (0, 0));
        assert_eq!(cursor_in_prompt_lines("hello", 3, 40), (0, 3));
        assert_eq!(cursor_in_prompt_lines("hello", 5, 40), (0, 5));
    }

    #[test]
    fn test_cursor_wrapped_line() {
        assert_eq!(cursor_in_prompt_lines("hello world foo", 0, 11), (0, 0));
        assert_eq!(cursor_in_prompt_lines("hello world foo", 6, 11), (0, 6));
        // "world" ends the first display line; the space at idx 11 belongs
        // to the end of the first line
        assert_eq!(cursor_in_prompt_lines("hello world foo", 11, 11), (0, 11));
        // after the space, cursor is on the second display line
        assert_eq!(cursor_in_prompt_lines("hello world foo", 12, 11), (1, 0));
        assert_eq!(cursor_in_prompt_lines("hello world foo", 15, 11), (1, 3));
    }

    #[test]
    fn test_cursor_before_newline_belongs_to_prev_line() {
        let prompt = "ab\ncd";
        // cursor right before the '\n' → end of the first line
        assert_eq!(cursor_in_prompt_lines(prompt, 2, 40), (0, 2));
        // cursor right after the '\n' → start of the second line
        assert_eq!(cursor_in_prompt_lines(prompt, 3, 40), (1, 0));
        // cursor at the very end → end of the last line
        assert_eq!(cursor_in_prompt_lines(prompt, 5, 40), (1, 2));
    }

    #[test]
    fn test_cursor_between_double_newlines() {
        let prompt = "ab\n\ncd";
        // in the empty middle line
        assert_eq!(cursor_in_prompt_lines(prompt, 3, 40), (1, 0));
    }

    #[test]
    fn test_cursor_with_tab_expansion() {
        let prompt = "a\tb";
        // tab → 4 spaces: cursor after the tab sits after the expansion
        assert_eq!(cursor_in_prompt_lines(prompt, 2, 40), (0, 5));
        // cursor before the tab sits before the expansion
        assert_eq!(cursor_in_prompt_lines(prompt, 1, 40), (0, 1));
        // cursor at the very end
        assert_eq!(cursor_in_prompt_lines(prompt, 3, 40), (0, 6));
    }
}

#[test]
fn test_cursor_long_word_chunked() {
    // "abcdefghij" wraps into chunks of 5
    assert_eq!(cursor_in_prompt_lines("abcdefghij", 0, 5), (0, 0));
    assert_eq!(cursor_in_prompt_lines("abcdefghij", 3, 5), (0, 3));
    // idx 5 = between 'e' and 'f' → end of the first chunk
    assert_eq!(cursor_in_prompt_lines("abcdefghij", 5, 5), (0, 5));
    // idx 6 = between 'f' and 'g' → col 1 of the second chunk
    assert_eq!(cursor_in_prompt_lines("abcdefghij", 6, 5), (1, 1));
    // idx 9 = between 'i' and 'j' → col 4 of the second chunk
    assert_eq!(cursor_in_prompt_lines("abcdefghij", 9, 5), (1, 4));
    assert_eq!(cursor_in_prompt_lines("abcdefghij", 10, 5), (1, 5));
}
