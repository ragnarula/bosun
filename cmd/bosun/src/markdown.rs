//! Minimal markdown rendering for the terminal client: turns a model's
//! markdown text into styled, width-wrapped ratatui lines. Covers the subset
//! models actually emit — headings, emphasis, inline code, code fences,
//! links, lists, blockquotes, and rules — and leaves anything else plain.

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Span;

fn base() -> Style {
    Style::default().fg(Color::White)
}

fn heading(level: usize) -> Style {
    let fg = if level <= 1 {
        Color::Yellow
    } else {
        Color::Cyan
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

fn link() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED)
}

fn rule() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn quote() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn code_block() -> Style {
    Style::default().fg(Color::White).bg(Color::DarkGray)
}

/// Parses one line of markdown into styled tokens. A token is a run of text
/// with a single style; the run may contain spaces. Underscores stay literal
/// so names like `file_name` are not split into italics.
fn parse_inline(text: &str, base: Style) -> Vec<(String, Style)> {
    let mut tokens = Vec::new();
    let mut run = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut code_span = false;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    // The current style is rebuilt from the flags on flush: ratatui's
    // Style::remove_modifier records a sub_modifier override, so comparing
    // styles for equality after a toggle does not work.
    let flush = |run: &mut String,
                 bold: bool,
                 italic: bool,
                 code_span: bool,
                 tokens: &mut Vec<(String, Style)>| {
        if !run.is_empty() {
            let mut style = base;
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if code_span {
                style = style.fg(Color::Yellow);
            }
            tokens.push((std::mem::take(run), style));
        }
    };

    while i < len {
        match chars[i] {
            '`' => {
                flush(&mut run, bold, italic, code_span, &mut tokens);
                code_span = !code_span;
                i += 1;
            }
            '*' => {
                let mut n = 0;
                while i + n < len && chars[i + n] == '*' {
                    n += 1;
                }
                flush(&mut run, bold, italic, code_span, &mut tokens);
                match n {
                    3 => {
                        bold = !bold;
                        italic = !italic;
                    }
                    2 => bold = !bold,
                    _ => italic = !italic,
                }
                i += n;
            }
            '[' => {
                let rel = chars[i + 1..].iter().position(|&ch| ch == ']');
                if let Some(rel) = rel {
                    let text = chars[i + 1..i + 1 + rel].iter().collect::<String>();
                    let after = i + 1 + rel + 1;
                    if after < len && chars[after] == '(' {
                        let paren = chars[after + 1..].iter().position(|&ch| ch == ')');
                        if let Some(paren) = paren {
                            flush(&mut run, bold, italic, code_span, &mut tokens);
                            tokens.push((text, link()));
                            i = after + 1 + paren + 1;
                            continue;
                        }
                    }
                }
                run.push('[');
                i += 1;
            }
            c => {
                run.push(c);
                i += 1;
            }
        }
    }
    flush(&mut run, bold, italic, code_span, &mut tokens);
    tokens
}

/// Lays styled tokens out into rows of at most `width` columns, wrapping on
/// spaces and hard-breaking words longer than a row.
fn wrap_tokens(tokens: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut first = true;
    for (text, style) in tokens {
        for word in text.split(' ') {
            if word.is_empty() {
                continue;
            }
            let word_width = word.chars().count();
            if !first && used + 1 + word_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
                first = true;
            }
            if !first {
                row.push(Span::styled(" ", *style));
                used += 1;
            }
            if word_width > width {
                let mut chunk = String::new();
                for ch in word.chars() {
                    chunk.push(ch);
                    if chunk.chars().count() == width {
                        row.push(Span::styled(std::mem::take(&mut chunk), *style));
                        rows.push(std::mem::take(&mut row));
                        used = 0;
                        first = true;
                    }
                }
                if !chunk.is_empty() {
                    let chunk_width = chunk.chars().count();
                    row.push(Span::styled(chunk, *style));
                    used = chunk_width;
                    first = false;
                }
            } else {
                row.push(Span::styled(word.to_string(), *style));
                used += word_width;
                first = false;
            }
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    if text.chars().count() <= width {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn is_rule(line: &str) -> bool {
    let trimmed: Vec<char> = line.trim().chars().collect();
    trimmed.len() >= 3 && trimmed.iter().all(|&c| c == '-' || c == '*' || c == '_')
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line.chars().nth(hashes);
    if matches!(rest, Some(' ') | Some('\t')) {
        Some(hashes)
    } else {
        None
    }
}

/// The width of a leading list marker (`- `, `1. `, ...), in characters.
fn list_marker(line: &str) -> usize {
    let trimmed = line.trim_start();
    let lead = line.len() - trimmed.len();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return lead + 2;
    }
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits >= 1 {
        let rest = &trimmed[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            return lead + digits + 2;
        }
    }
    0
}

/// Renders a markdown string as width-wrapped rows of styled spans. Blank
/// lines are preserved so paragraphs keep their spacing.
pub fn markdown_rows(text: &str, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if in_code {
            if trimmed.starts_with("```") {
                in_code = false;
            } else {
                for chunk in wrap_chars(raw, width) {
                    rows.push(vec![Span::styled(chunk, code_block())]);
                }
            }
            continue;
        }
        if trimmed.starts_with("```") {
            in_code = true;
            continue;
        }
        let line = raw.trim_end();
        if line.trim().is_empty() {
            rows.push(Vec::new());
            continue;
        }
        if is_rule(line) {
            rows.push(vec![Span::styled("─".repeat(width), rule())]);
            continue;
        }
        if let Some(level) = heading_level(line) {
            let body = line.trim_start_matches('#').trim_start();
            rows.extend(wrap_tokens(&parse_inline(body, heading(level)), width));
            continue;
        }
        if let Some(rest) = line.strip_prefix('>') {
            let inner = width.saturating_sub(2).max(1);
            let wrapped = wrap_tokens(&parse_inline(rest.trim_start(), quote()), inner);
            for (i, row) in wrapped.iter().enumerate() {
                let gutter = if i == 0 { "│ " } else { "  " };
                let mut spans = vec![Span::styled(gutter, quote())];
                spans.extend(row.iter().cloned());
                rows.push(spans);
            }
            continue;
        }
        let marker_len = list_marker(line);
        if marker_len > 0 {
            let (marker_text, body) = line.split_at(marker_len);
            let inner = width.saturating_sub(marker_len).max(1);
            let wrapped = wrap_tokens(&parse_inline(body, base()), inner);
            for (i, row) in wrapped.iter().enumerate() {
                let mut spans = vec![Span::styled(
                    if i == 0 {
                        marker_text.to_string()
                    } else {
                        " ".repeat(marker_len)
                    },
                    Style::default().fg(Color::White),
                )];
                spans.extend(row.iter().cloned());
                rows.push(spans);
            }
            continue;
        }
        rows.extend(wrap_tokens(&parse_inline(line, base()), width));
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(row: &[Span<'_>]) -> String {
        row.iter().map(|s| s.content.as_ref()).collect()
    }

    fn rows_text(rows: &[Vec<Span<'static>>]) -> Vec<String> {
        rows.iter().map(|r| text(r)).collect()
    }

    fn fg(row: &[Span<'_>], index: usize) -> Option<Color> {
        row[index].style.fg
    }

    #[test]
    fn headings_are_bold_and_colored() {
        let rows = markdown_rows("# Title\n\n## Section", 40);
        assert_eq!(rows_text(&rows), vec!["Title", "", "Section"]);
        assert_eq!(fg(&rows[0], 0), Some(Color::Yellow));
        assert!(rows[0][0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(fg(&rows[2], 0), Some(Color::Cyan));
    }

    #[test]
    fn emphasis_and_inline_code_are_styled() {
        let rows = markdown_rows("a **bold** b *it* c `code` d", 40);
        assert_eq!(rows_text(&rows), vec!["a bold b it c code d"]);
        let row = &rows[0];
        let at = |needle: &str| {
            row.iter()
                .position(|s| s.content.as_ref() == needle)
                .expect(needle)
        };
        assert!(row[at("bold")].style.add_modifier.contains(Modifier::BOLD));
        assert!(row[at("it")].style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(fg(row, at("code")), Some(Color::Yellow));
    }

    #[test]
    fn underscores_inside_words_stay_literal() {
        let rows = markdown_rows("file_name.txt is fine", 40);
        assert_eq!(rows_text(&rows), vec!["file_name.txt is fine"]);
    }

    #[test]
    fn links_render_their_text() {
        let rows = markdown_rows("see [docs](https://x) now", 40);
        assert_eq!(rows_text(&rows), vec!["see docs now"]);
        let row = &rows[0];
        let bold = row
            .iter()
            .position(|s| s.content.as_ref() == "docs")
            .unwrap();
        assert_eq!(fg(row, bold), Some(Color::Blue));
        assert!(row[bold].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn code_fences_are_not_parsed() {
        let rows = markdown_rows("```\n**raw**\n# not a heading\n```", 40);
        assert_eq!(rows_text(&rows), vec!["**raw**", "# not a heading"]);
        assert_eq!(fg(&rows[0], 0), Some(Color::White));
        assert!(rows[0][0].style.bg.is_some());
    }

    #[test]
    fn blockquotes_and_lists_keep_their_markers() {
        let rows = markdown_rows("> quoted\n- one\n1. two", 40);
        assert_eq!(rows_text(&rows), vec!["│ quoted", "- one", "1. two"]);
    }

    #[test]
    fn rules_render_as_a_full_line() {
        let rows = markdown_rows("---", 10);
        assert_eq!(rows_text(&rows), vec!["──────────"]);
    }

    #[test]
    fn wrapping_respects_width_and_styles() {
        let rows = markdown_rows("one **two three four** five", 12);
        assert!(rows.len() >= 2);
        let joined: String = rows_text(&rows).join(" ");
        assert_eq!(joined, "one two three four five");
    }

    #[test]
    fn blank_lines_preserve_paragraph_spacing() {
        let rows = markdown_rows("a\n\nb", 40);
        assert_eq!(rows_text(&rows), vec!["a", "", "b"]);
    }
}
