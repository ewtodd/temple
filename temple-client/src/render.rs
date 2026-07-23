use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A single styled display line — text plus a semantic kind that
/// determines colour.
#[derive(Clone)]
pub struct StyledLine {
    pub text: String,
    pub kind: LineKind,
}

/// Semantic kind for colour-coding display lines.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum LineKind {
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

/// Render markdown-lite: fenced code blocks are rendered as indented
/// dim lines, normal text is word-wrapped. Returns StyledLine vec.
pub fn render_markdown_lite(content: &str, width: usize) -> Vec<StyledLine> {
    let w = if width < 2 { 40 } else { width };
    let mut out = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if in_block {
            for l in wrap_text(line, w.saturating_sub(2)) {
                out.push(StyledLine {
                    text: format!("  {l}"),
                    kind: LineKind::Code,
                });
            }
        } else {
            for l in wrap_text(line, w) {
                out.push(StyledLine {
                    text: l,
                    kind: LineKind::Normal,
                });
            }
        }
    }
    out
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
    fn test_render_markdown_lite_plain() {
        let result = render_markdown_lite("hello world", 40);
        assert!(!result.is_empty());
        assert_eq!(result[0].text, "hello world");
    }

    #[test]
    fn test_render_markdown_lite_code_block() {
        let input = "```\ncode line\n```";
        let result = render_markdown_lite(input, 40);
        assert!(result.iter().any(|l| l.text.contains("code line")));
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
