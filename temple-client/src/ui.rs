use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::input::SPINNER;
use crate::render::LineKind;
use crate::state::{AppState, ChatEntry};
use crate::tools::mode_tag;

/// Temple ASCII art banner.
pub const TEMPLE_ART: &str = include_str!("../../assets/temple.asc");

/// Build a Vec<Line> from ChatEntry items for the chat area.
fn build_chat_lines(s: &AppState, width: usize) -> Vec<Line<'static>> {
    use crate::render::{render_markdown_lite, wrap_text};

    let mut lines: Vec<Line<'static>> = Vec::new();
    let content_width = width.saturating_sub(4);
    let sep = "\u{2500}".repeat(width.saturating_sub(2));

    for (idx, entry) in s.entries.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(Span::styled(
                sep.clone(),
                Style::default().fg(Color::DarkGray),
            )));
        }

        match entry {
            ChatEntry::User(text) => {
                lines.push(Line::from(Span::styled(
                    " you",
                    Style::default().fg(Color::Blue),
                )));
                for l in render_markdown_lite(text, content_width.saturating_sub(2)) {
                    let color = match l.kind {
                        LineKind::Code => Color::Gray,
                        _ => Color::Reset,
                    };
                    lines.push(Line::from(Span::styled(
                        format!(" {}", l.text),
                        Style::default().fg(color),
                    )));
                }
            }
            ChatEntry::Assistant { content, stats } => {
                let model_tag = s.model.as_str();
                let header = if model_tag.is_empty() {
                    " renco".to_string()
                } else {
                    format!(" renco \u{b7} {model_tag}")
                };
                lines.push(Line::from(Span::styled(
                    header,
                    Style::default().fg(Color::Green),
                )));
                let body = render_markdown_lite(content, content_width.saturating_sub(2));
                for l in body.iter() {
                    let color = match l.kind {
                        LineKind::Code => Color::Gray,
                        _ => Color::Reset,
                    };
                    lines.push(Line::from(Span::styled(
                        format!(" {}", l.text),
                        Style::default().fg(color),
                    )));
                }
                if let Some(st) = stats {
                    lines.push(Line::from(Span::styled(
                        format!(" \u{23F1} {st}"),
                        Style::default().fg(Color::Magenta),
                    )));
                }
            }
            ChatEntry::System(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(Line::from(Span::styled(
                        format!(" {l}"),
                        Style::default().fg(Color::Cyan),
                    )));
                }
            }
            ChatEntry::Error(text) => {
                for l in wrap_text(text, content_width.saturating_sub(2)) {
                    lines.push(Line::from(Span::styled(
                        format!(" {l}"),
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
                lines.push(Line::from(Span::styled(
                    format!("   {icon} {name}"),
                    Style::default().fg(color),
                )));
                if !detail.is_empty() {
                    let d = if detail.contains('\n') && detail.len() > 120 {
                        let first = detail.lines().next().unwrap_or("");
                        let first: String = first.chars().take(100).collect();
                        format!("{first} \u{2026} ({} chars)", detail.len())
                    } else {
                        detail.chars().take(200).collect()
                    };
                    for l in wrap_text(&d, content_width.saturating_sub(6)) {
                        lines.push(Line::from(Span::styled(
                            format!("   \u{2502} {l}"),
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
                    format!("   tasks ({done}/{})", items.len()),
                    Style::default().fg(Color::Green),
                )));
                for item in items {
                    let (icon, color) = match item.status {
                        temple_protocol::TodoStatus::Pending => ("\u{25AB}", Color::DarkGray),
                        temple_protocol::TodoStatus::InProgress => ("\u{25B8}", Color::Yellow),
                        temple_protocol::TodoStatus::Done => ("\u{2713}", Color::Green),
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
    let cursor_vis: usize = s
        .prompt
        .chars()
        .take(s.prompt_cursor.min(s.prompt.chars().count()))
        .filter(|c| !c.is_control())
        .count();

    let mut char_count = 0usize;
    let (cursor_line, cursor_col) = prompt_lines
        .iter()
        .enumerate()
        .find_map(|(i, line)| {
            let line_len = line.chars().count();
            if char_count + line_len > cursor_vis {
                Some((i, cursor_vis - char_count))
            } else {
                char_count += line_len;
                None
            }
        })
        .unwrap_or_else(|| {
            (
                prompt_lines.len().max(1) - 1,
                prompt_lines.last().map(|l| l.chars().count()).unwrap_or(0),
            )
        });

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
pub fn draw(f: &mut Frame, s: &AppState, tick_count: u64) -> (Rect, Vec<String>) {
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
    let prompt_h = prompt_box_height(s, w);
    let status_h = 1usize;
    let chat_avail = h.saturating_sub(prompt_h + status_h + art_extra);

    // Scroll handling
    let total = all_chat_lines.len();
    let max_scroll = total.saturating_sub(chat_avail);
    let scroll = s.scroll.min(max_scroll);
    let start = max_scroll.saturating_sub(scroll);
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
                let abs_idx = start + i;
                let (top, bot) = if sl <= el { (sl, el) } else { (el, sl) };
                if abs_idx >= top && abs_idx <= bot {
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
        let art_text: Vec<Line> = TEMPLE_ART
            .lines()
            .map(|l| Line::from(Span::raw(l.to_string())))
            .collect();
        f.render_widget(Paragraph::new(art_text), art_area);
        let hint_area = Rect {
            x: art_area.x,
            y: art_area.y + art_lines as u16,
            width: art_area.width,
            height: 1,
        };
        let hint = Line::from(Span::styled(
            "  /help \u{b7} Shift+Tab mode \u{b7} Ctrl+G editor \u{b7} Ctrl+L clear".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
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
            filtered
                .iter()
                .enumerate()
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
        let text = format!("Allow {}? (y/N)", crate::render::sanitize(&pstate.text));
        let lines = crate::render::wrap_text(&text, width.saturating_sub(3).max(1));
        return lines.len() + 2; // +2 for borders
    }
    let prompt_inner_w = width.saturating_sub(3).max(1);
    let prompt_lines =
        crate::render::wrap_text(&crate::render::sanitize(&s.prompt), prompt_inner_w);
    const MAX_PROMPT_ROWS: usize = 8;
    let shown = prompt_lines.len().min(MAX_PROMPT_ROWS);
    shown + 2 // +2 for borders
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
        let text = format!("Allow {}? (y/N)", crate::render::sanitize(&pstate.text));
        let lines = crate::render::wrap_text(&text, prompt_inner_w);
        let display: Vec<Line> = lines
            .into_iter()
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

    // Find cursor position in the full prompt_lines
    let cursor_vis: usize = s
        .prompt
        .chars()
        .take(s.prompt_cursor.min(s.prompt.chars().count()))
        .filter(|c| !c.is_control())
        .count();
    let mut char_count = 0usize;
    let cursor_line = prompt_lines
        .iter()
        .enumerate()
        .find_map(|(i, line)| {
            let line_len = line.chars().count();
            if char_count + line_len > cursor_vis {
                Some(i)
            } else {
                char_count += line_len;
                None
            }
        })
        .unwrap_or(prompt_lines.len().saturating_sub(1));

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
        format!("{spinner}{elapsed}{model_info}{mode_str}{scroll_indicator}")
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
