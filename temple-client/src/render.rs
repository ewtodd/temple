use pulldown_cmark::{Event, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Strip ANSI escape sequences and control characters (except \n).
/// Tabs become 4 spaces. Raw control bytes corrupt terminal state
/// and must never reach the canvas.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => out.push_str("    "),
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
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

/// Wrap text to `width` display columns. Handles embedded newlines and
/// strips control characters — every display path funnels through here.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for piece in text.split('\n') {
        out.extend(wrap_piece(&sanitize(piece), width));
    }
    out
}

/// Word-wrap a single sanitized line by display width (not char count —
/// wide chars like emoji/CJK must match ratatui's own width math).
pub fn wrap_piece(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
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

/// Render markdown to styled ratatui Lines. Handles bold, italic,
/// inline code, fenced code blocks, headers, and bullet lists. Text
/// is word-wrapped at `width` display columns.
pub fn render_markdown(content: &str, width: usize) -> Vec<Line<'static>> {
    let w = if width < 2 { 40 } else { width };
    let mut out: Vec<Line<'static>> = Vec::new();
    let parser = pulldown_cmark::Parser::new(content);
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut base_style = Style::default();
    let mut in_code_block = false;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                flush_line(&mut current, &mut out, w);
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                flush_line(&mut current, &mut out, w);
                in_code_block = false;
            }
            Event::Start(Tag::Heading { .. }) => {
                flush_line(&mut current, &mut out, w);
                base_style = Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_line(&mut current, &mut out, w);
                base_style = Style::default();
            }
            Event::Start(Tag::Emphasis) => {
                base_style = base_style.add_modifier(Modifier::ITALIC);
            }
            Event::End(TagEnd::Emphasis) => {
                base_style = base_style.remove_modifier(Modifier::ITALIC);
            }
            Event::Start(Tag::Strong) => {
                base_style = base_style.add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Strong) => {
                base_style = base_style.remove_modifier(Modifier::BOLD);
            }
            Event::Start(Tag::Item) => {
                flush_line(&mut current, &mut out, w);
            }
            Event::End(TagEnd::Item) => {
                flush_line(&mut current, &mut out, w);
            }
            Event::Start(Tag::List(_)) | Event::End(TagEnd::List(_)) => {}
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) => {}
            Event::Start(Tag::BlockQuote(_)) => {
                base_style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush_line(&mut current, &mut out, w);
                base_style = Style::default();
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current, &mut out, w);
            }
            Event::Text(text) => {
                let style = if in_code_block {
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC)
                } else {
                    base_style
                };
                for ch in text.chars() {
                    if ch == '\n' {
                        flush_line(&mut current, &mut out, w);
                    } else {
                        current.push(Span::styled(ch.to_string(), style));
                    }
                }
            }
            Event::Code(text) => {
                let code_style = Style::default().fg(Color::Yellow);
                for ch in text.chars() {
                    if ch == '\n' {
                        flush_line(&mut current, &mut out, w);
                    } else {
                        current.push(Span::styled(ch.to_string(), code_style));
                    }
                }
            }
            _ => {}
        }
    }
    flush_line(&mut current, &mut out, w);

    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// Flush accumulated spans into wrapped lines. An empty current vec
/// produces nothing (no empty lines for repeated breaks).
fn flush_line(current: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>, width: usize) {
    if current.is_empty() {
        return;
    }
    let spans = std::mem::take(current);
    let line_text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    if line_text.width() <= width {
        out.push(Line::from(spans));
    } else {
        for wrapped in wrap_spans(&spans, width) {
            out.push(wrapped);
        }
    }
}

/// Wrap a sequence of styled spans at display width, preserving styles
/// across line breaks.
fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_w = 0usize;

    for span in spans {
        let mut remaining = span.content.as_ref();
        let mut consumed: usize = 0;
        while !remaining.is_empty() {
            let space = width.saturating_sub(current_w);
            if space == 0 {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_w = 0;
                continue;
            }
            let (chunk, rest) = split_at_width(remaining, space);
            let chunk_style = span.style;
            if !chunk.is_empty() {
                current.push(Span::styled(chunk.to_string(), chunk_style));
                current_w += chunk.width();
            }
            if rest.is_empty() {
                break;
            }
            remaining = rest;
            if current_w >= width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_w = 0;
            }
            consumed += chunk.len();
            if consumed < span.content.len() {
                remaining = &span.content[consumed..];
            }
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// Split a string at `max_width` display columns. Returns (first part, remainder).
fn split_at_width(s: &str, max_width: usize) -> (&str, &str) {
    if max_width == 0 {
        return ("", s);
    }
    let mut w = 0usize;
    for (bi, ch) in s.char_indices() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max_width {
            return (&s[..bi], &s[bi..]);
        }
        w += cw;
    }
    (s, "")
}

/// Insert a character at a given char index in a String.
pub fn insert_char_at(s: &mut String, idx: usize, c: char) {
    let byte_idx = s.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(s.len());
    s.insert(byte_idx, c);
}

/// Remove the character at a given char index from a String.
pub fn remove_char_at(s: &mut String, idx: usize) {
    if let Some((byte_idx, _ch)) = s.char_indices().nth(idx) {
        s.remove(byte_idx);
    }
}

/// Extract selected text from visible lines given a normalized selection.
pub fn extract_selection(lines: &[String], sel: ((usize, usize), (usize, usize))) -> String {
    let ((sl, sc), (el, ec)) = sel;
    if sl > lines.len() || el > lines.len() {
        return String::new();
    }
    if sl == el {
        let line = &lines[sl];
        let chars: Vec<char> = line.chars().collect();
        let start = sc.min(chars.len());
        let end = ec.min(chars.len());
        let (s, e) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        chars[s..e].iter().collect()
    } else {
        let (top, bot) = if sl <= el { (sl, el) } else { (el, sl) };
        let (tc, bc) = if sl <= el { (sc, ec) } else { (ec, sc) };
        let mut parts: Vec<String> = Vec::new();
        let top_line = &lines[top];
        let top_chars: Vec<char> = top_line.chars().collect();
        let tc_clamped = tc.min(top_chars.len());
        parts.push(top_chars[tc_clamped..].iter().collect::<String>());
        for line in lines[(top + 1)..bot].iter() {
            parts.push(line.clone());
        }
        let bot_line = &lines[bot];
        let bot_chars: Vec<char> = bot_line.chars().collect();
        let bc_clamped = bc.min(bot_chars.len());
        parts.push(bot_chars[..bc_clamped].iter().collect::<String>());
        parts.join("\n")
    }
}

/// Normalize a possibly-inverted selection pair.
pub fn norm_sel(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1) {
        (a, b)
    } else {
        (b, a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_strips_ansi() {
        assert_eq!(sanitize("\x1b[31mhello\x1b[0m"), "hello");
    }

    #[test]
    fn test_sanitize_strips_osc() {
        assert_eq!(sanitize("hello\x1b]0;title\x07world"), "helloworld");
    }

    #[test]
    fn test_sanitize_preserves_newlines() {
        assert_eq!(sanitize("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn test_sanitize_converts_tabs() {
        assert_eq!(sanitize("a\tb"), "a    b");
    }

    #[test]
    fn test_sanitize_strips_control_chars() {
        assert_eq!(sanitize("hello\x08world"), "helloworld");
    }

    #[test]
    fn test_sanitize_empty() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn test_wrap_text_simple() {
        let result = wrap_text("hello world", 20);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn test_wrap_text_short_width() {
        let result = wrap_text("hello world", 5);
        assert_eq!(result.len(), 2);
        assert_eq!(result, vec!["hello", "world"]);
    }

    #[test]
    fn test_wrap_text_newlines() {
        let result = wrap_text("hello\nworld", 20);
        assert_eq!(result, vec!["hello", "world"]);
    }

    #[test]
    fn test_wrap_text_empty() {
        let result = wrap_text("", 20);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn test_wrap_text_zero_width() {
        let result = wrap_text("hello", 0);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn test_wrap_piece_exact_width() {
        let result = wrap_piece("hello", 5);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn test_wrap_piece_long_word() {
        let result = wrap_piece("abcdefghij", 5);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_insert_char_at_start() {
        let mut s = "bc".to_string();
        insert_char_at(&mut s, 0, 'a');
        assert_eq!(s, "abc");
    }

    #[test]
    fn test_insert_char_at_end() {
        let mut s = "ab".to_string();
        insert_char_at(&mut s, 2, 'c');
        assert_eq!(s, "abc");
    }

    #[test]
    fn test_insert_char_at_middle() {
        let mut s = "ac".to_string();
        insert_char_at(&mut s, 1, 'b');
        assert_eq!(s, "abc");
    }

    #[test]
    fn test_insert_char_at_past_end() {
        let mut s = "ab".to_string();
        insert_char_at(&mut s, 10, 'c');
        assert_eq!(s, "abc");
    }

    #[test]
    fn test_remove_char_at() {
        let mut s = "abc".to_string();
        remove_char_at(&mut s, 1);
        assert_eq!(s, "ac");
    }

    #[test]
    fn test_remove_char_at_end() {
        let mut s = "abc".to_string();
        remove_char_at(&mut s, 2);
        assert_eq!(s, "ab");
    }

    #[test]
    fn test_remove_char_at_out_of_bounds() {
        let mut s = "ab".to_string();
        remove_char_at(&mut s, 10);
        assert_eq!(s, "ab");
    }

    #[test]
    fn test_render_markdown_plain() {
        let result = render_markdown("hello world", 40);
        assert!(!result.is_empty());
        let text: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_render_markdown_code_block() {
        let input = "```\ncode line\n```";
        let result = render_markdown(input, 40);
        let text: String = result
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("code line"));
    }

    #[test]
    fn test_render_markdown_bold() {
        let result = render_markdown("hello **world**", 40);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_extract_selection_single_line() {
        let lines = vec!["hello world".to_string()];
        let result = extract_selection(&lines, ((0, 0), (0, 5)));
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_extract_selection_multi_line() {
        let lines = vec!["hello".to_string(), "world".to_string()];
        let result = extract_selection(&lines, ((0, 3), (1, 2)));
        assert_eq!(result, "lo\nwo");
    }

    #[test]
    fn test_norm_sel_ordered() {
        assert_eq!(norm_sel((0, 0), (1, 1)), ((0, 0), (1, 1)));
    }

    #[test]
    fn test_norm_sel_reversed() {
        assert_eq!(norm_sel((1, 1), (0, 0)), ((0, 0), (1, 1)));
    }

    #[test]
    fn test_wrap_text_with_ansi() {
        let result = wrap_text("\x1b[31mhello\x1b[0m world", 40);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn test_wrap_piece_zero_width() {
        let result = wrap_piece("hello", 0);
        assert_eq!(result, vec![""]);
    }
}
