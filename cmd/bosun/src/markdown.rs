//! Minimal markdown rendering for the terminal client: turns a model's
//! markdown text into styled, width-wrapped ratatui lines. Covers the subset
//! models actually emit — headings, emphasis, inline code, code fences,
//! links, lists, blockquotes, and rules — and leaves anything else plain. A
//! ```mermaid fence renders as a diagram; anything the diagram renderer
//! cannot lay out keeps its source.

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

/// The most entries the cache keeps, so its per-lookup scan stays cheap, and
/// the most bytes it may hold in total. A redraw renders the whole transcript,
/// not just the rows on screen, so the bound cannot be a count of visible
/// diagrams.
const DIAGRAM_CACHE_ENTRIES: usize = 64;
const DIAGRAM_CACHE_BYTES: usize = 1024 * 1024;

/// Rendered diagrams, kept between redraws of the transcript. Every redraw
/// renders every line again, and one diagram costs 7 ms at ten nodes and 250
/// ms at two hundred, so re-rendering an unchanged one would make scrolling
/// and typing slow. Keyed by source and width, because the layout depends on
/// both, and bounded in entries and in bytes so a long session cannot grow it
/// without limit.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiagramCache {
    entries: Vec<((String, usize), Option<String>)>,
}

impl DiagramCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The diagram for `source` laid out within `width`, or None when the
    /// crate cannot render one that fits. A miss is kept too: a source that
    /// does not fit on this redraw will not fit on the next one either.
    fn diagram_text(&mut self, source: &str, width: usize) -> Option<String> {
        if let Some((_, rendered)) = self
            .entries
            .iter()
            .find(|(key, _)| key.0 == source && key.1 == width)
        {
            return rendered.clone();
        }
        let rendered = render_diagram(source, width);
        // An entry over the whole budget is not worth keeping: it would evict
        // every older entry on the way in, and then itself, leaving the cache
        // empty and every one of those diagrams to render again.
        if entry_bytes(source, rendered.as_deref()) > DIAGRAM_CACHE_BYTES {
            return rendered;
        }
        self.entries
            .push(((source.to_string(), width), rendered.clone()));
        // Drop the oldest until both bounds hold. A transcript can hold more
        // diagrams than the bound, and a redraw walks them in order, so an
        // entry evicted before the walk reaches it is one this pass renders
        // again. Keeping the newest is what makes scrolling stable.
        while self.entries.len() > DIAGRAM_CACHE_ENTRIES || self.held_bytes() > DIAGRAM_CACHE_BYTES
        {
            self.entries.remove(0);
        }
        rendered
    }

    /// The bytes the entries hold together.
    fn held_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(key, rendered)| entry_bytes(&key.0, rendered.as_deref()))
            .sum()
    }
}

/// Whether every line of `rendered` fits `width`. The output pane wraps by
/// display columns, so a wide glyph counts two.
fn fits_columns(rendered: &str, width: usize) -> bool {
    rendered
        .lines()
        .all(|line| Span::raw(line).width() <= width)
}

/// The bytes one entry holds: its source key and its rendered diagram, which
/// are the only allocations it owns.
fn entry_bytes(source: &str, rendered: Option<&str>) -> usize {
    source.len() + rendered.map_or(0, str::len)
}

/// Renders `source` with the crate, or None when it errors or when even its
/// most compact layout is wider than `width`. The crate returns that compact
/// layout rather than an error when nothing fits, so the widest line decides.
fn render_diagram(source: &str, width: usize) -> Option<String> {
    let rendered = mermaid_text::render_with_width(source, Some(width)).ok()?;
    if !fits_columns(&rendered, width) {
        return None;
    }
    Some(rendered)
}

/// The fence body as a code block: its lines hard-wrapped to `width`. A fence
/// renders this way when it is not a diagram, its diagram does not fit, or
/// the crate cannot render it.
fn code_block_rows(lines: &[&str], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = Vec::new();
    for line in lines {
        for chunk in wrap_chars(line, width) {
            rows.push(vec![Span::styled(chunk, code_block())]);
        }
    }
    rows
}

/// A rendered diagram as one code block. The crate has already laid every
/// line out within the width it was given, so no line wraps, and its trailing
/// newline is not a row of its own.
fn diagram_rows(rendered: &str) -> Vec<Vec<Span<'static>>> {
    rendered
        .lines()
        .map(|line| vec![Span::styled(line.to_string(), code_block())])
        .collect()
}

/// Renders a markdown string as width-wrapped rows of styled spans. Blank
/// lines are preserved so paragraphs keep their spacing. A ```mermaid fence
/// renders as a diagram, drawn from `cache` so a redraw does not pay for it
/// again; every other fence shows its source.
pub fn markdown_rows(
    text: &str,
    width: usize,
    cache: &mut DiagramCache,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut lines = text.split('\n');
    while let Some(raw) = lines.next() {
        let trimmed = raw.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            let language = info.split_whitespace().next().unwrap_or("");
            let mut body = Vec::new();
            let mut closed = false;
            for line in lines.by_ref() {
                if line.trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                body.push(line);
            }
            // An open fence has no complete body to draw: while a message
            // streams, the diagram it will become is still arriving.
            let diagram = if closed && language.eq_ignore_ascii_case("mermaid") && !body.is_empty()
            {
                cache.diagram_text(&body.join("\n"), width)
            } else {
                None
            };
            match diagram {
                Some(rendered) => rows.extend(diagram_rows(&rendered)),
                None => rows.extend(code_block_rows(&body, width)),
            }
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

    /// Renders through a fresh cache, for the tests that are not about the
    /// cache itself.
    fn rendered(text: &str, width: usize) -> Vec<Vec<Span<'static>>> {
        markdown_rows(text, width, &mut DiagramCache::new())
    }

    #[test]
    fn headings_are_bold_and_colored() {
        let rows = rendered("# Title\n\n## Section", 40);
        assert_eq!(rows_text(&rows), vec!["Title", "", "Section"]);
        assert_eq!(fg(&rows[0], 0), Some(Color::Yellow));
        assert!(rows[0][0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(fg(&rows[2], 0), Some(Color::Cyan));
    }

    #[test]
    fn emphasis_and_inline_code_are_styled() {
        let rows = rendered("a **bold** b *it* c `code` d", 40);
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
        let rows = rendered("file_name.txt is fine", 40);
        assert_eq!(rows_text(&rows), vec!["file_name.txt is fine"]);
    }

    #[test]
    fn links_render_their_text() {
        let rows = rendered("see [docs](https://x) now", 40);
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
        let rows = rendered("```\n**raw**\n# not a heading\n```", 40);
        assert_eq!(rows_text(&rows), vec!["**raw**", "# not a heading"]);
        assert_eq!(fg(&rows[0], 0), Some(Color::White));
        assert!(rows[0][0].style.bg.is_some());
    }

    #[test]
    fn blockquotes_and_lists_keep_their_markers() {
        let rows = rendered("> quoted\n- one\n1. two", 40);
        assert_eq!(rows_text(&rows), vec!["│ quoted", "- one", "1. two"]);
    }

    #[test]
    fn rules_render_as_a_full_line() {
        let rows = rendered("---", 10);
        assert_eq!(rows_text(&rows), vec!["──────────"]);
    }

    #[test]
    fn wrapping_respects_width_and_styles() {
        let rows = rendered("one **two three four** five", 12);
        assert!(rows.len() >= 2);
        let joined: String = rows_text(&rows).join(" ");
        assert_eq!(joined, "one two three four five");
    }

    #[test]
    fn blank_lines_preserve_paragraph_spacing() {
        let rows = rendered("a\n\nb", 40);
        assert_eq!(rows_text(&rows), vec!["a", "", "b"]);
    }

    #[test]
    fn a_mermaid_fence_renders_as_a_diagram() {
        let rows = rendered("```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```", 80);
        let rows = rows_text(&rows);
        assert!(rows.iter().any(|row| row.contains("Start")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("End")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains('┌')), "{rows:?}");
        // The source is drawn, not shown.
        assert!(
            !rows.iter().any(|row| row.contains("flowchart")),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|row| row.contains("-->")), "{rows:?}");
    }

    #[test]
    fn a_mermaid_fence_reads_its_language_from_the_first_word_ignoring_case() {
        let plain = rendered("```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```", 80);
        let labelled = rendered(
            "```MERMAID flowchart\nflowchart TD\n  A[Start] --> B[End]\n```",
            80,
        );
        assert_eq!(rows_text(&plain), rows_text(&labelled));
    }

    #[test]
    fn a_diagram_reads_as_one_code_block() {
        let rows = rendered("```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```", 80);
        assert!(rows.len() > 1);
        for row in &rows {
            assert_eq!(row.len(), 1);
            assert_eq!(row[0].style, code_block());
        }
    }

    #[test]
    fn a_mermaid_source_the_crate_cannot_render_keeps_its_source() {
        // The info string is mermaid; it is the body the crate rejects, so the
        // fence stays the code block it has always been.
        let rows = rendered("```mermaid\nthis is not a diagram\n```", 80);
        assert_eq!(rows_text(&rows), vec!["this is not a diagram"]);
        assert_eq!(rows[0][0].style, code_block());
    }

    #[test]
    fn an_open_mermaid_fence_keeps_its_source() {
        // While a message streams, the fence has not closed yet and its
        // diagram is still arriving.
        let rows = rendered("```mermaid\nflowchart TD\n  A[Start] --> B[End]", 80);
        assert_eq!(
            rows_text(&rows),
            vec!["flowchart TD", "  A[Start] --> B[End]"]
        );
    }

    #[test]
    fn an_empty_mermaid_body_contributes_no_row() {
        let rows = rendered("```mermaid\n```\nafter", 40);
        assert_eq!(rows_text(&rows), vec!["after"]);
    }

    #[test]
    fn a_fence_that_is_not_mermaid_keeps_its_source() {
        let rows = rendered("```rust\nlet x = 1;\n```", 40);
        assert_eq!(rows_text(&rows), vec!["let x = 1;"]);
        assert_eq!(rows[0][0].style, code_block());
    }

    #[test]
    fn a_diagram_wider_than_the_width_keeps_its_source() {
        // The most compact layout the crate can produce for this diagram is
        // twelve columns wide, so it cannot fit ten.
        let source = "```mermaid\ngraph LR\n  A --> B\n```";
        assert_eq!(
            rows_text(&rendered(source, 10)),
            vec!["graph LR", "  A --> B"]
        );
        // The same diagram fits fifteen.
        assert!(
            rows_text(&rendered(source, 15))
                .iter()
                .any(|r| r.contains('┌'))
        );
    }

    #[test]
    fn a_diagram_wider_than_the_width_in_columns_keeps_its_source() {
        // The labels are wide glyphs, so this layout is twelve characters and
        // fifteen columns: counting characters would call it a fit, and the
        // output pane would wrap the box art.
        let body = "flowchart TD\n  A[使用者的問題] --> B[使用者的答案]";
        let drawn = mermaid_text::render_with_width(body, Some(12)).unwrap();
        assert!(drawn.lines().all(|line| line.chars().count() <= 12));
        assert!(drawn.lines().any(|line| Span::raw(line).width() > 12));

        let rows = rendered(&format!("```mermaid\n{body}\n```"), 12);
        let text = rows_text(&rows);
        assert!(!text.iter().any(|row| row.contains('┌')), "{text:?}");
        assert!(text.iter().all(|row| row.chars().count() <= 12), "{text:?}");
    }

    #[test]
    fn a_cached_diagram_is_reused_and_the_cache_keeps_its_entry_bound() {
        let text = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```";
        let mut cache = DiagramCache::new();
        let first = markdown_rows(text, 80, &mut cache);
        let second = markdown_rows(text, 80, &mut cache);
        assert_eq!(rows_text(&first), rows_text(&second));
        assert_eq!(cache.entries.len(), 1);

        // A later render draws what the cache holds, rather than the source.
        cache.entries[0].1 = Some("from the cache".to_string());
        assert_eq!(
            rows_text(&markdown_rows(text, 80, &mut cache)),
            vec!["from the cache"]
        );

        // The oldest entry makes room once the cache is full. A redraw walks
        // the transcript in order, so it is the newest entries a pass keeps.
        for i in 0..DIAGRAM_CACHE_ENTRIES {
            let body = format!("flowchart TD\n  N{i} --> M{i}");
            markdown_rows(&format!("```mermaid\n{body}\n```"), 80 + i, &mut cache);
        }
        assert_eq!(cache.entries.len(), DIAGRAM_CACHE_ENTRIES);
        assert!(
            cache
                .entries
                .iter()
                .all(|(key, _)| key.0 != "flowchart TD\n  A[Start] --> B[End]"),
            "the oldest entry makes room"
        );
    }

    #[test]
    fn the_width_keys_a_cached_diagram_as_much_as_the_source() {
        let text = "```mermaid\ngraph LR\n  A --> B\n```";
        let mut cache = DiagramCache::new();
        markdown_rows(text, 80, &mut cache);
        // What one width kept is not what another width is given: at ten
        // columns the diagram cannot fit, and the fence keeps its source.
        cache.entries[0].1 = Some("eighty".to_string());
        assert_eq!(
            rows_text(&markdown_rows(text, 10, &mut cache)),
            vec!["graph LR", "  A --> B"]
        );
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn a_cached_failure_is_not_rendered_again() {
        let text = "```mermaid\ngraph LR\n  A --> B\n```";
        let mut cache = DiagramCache::new();
        markdown_rows(text, 80, &mut cache);
        // This source renders, but the cache holds a failure for it, and the
        // cache decides: the fence keeps its source instead of being drawn.
        cache.entries[0].1 = None;
        assert_eq!(
            rows_text(&markdown_rows(text, 80, &mut cache)),
            vec!["graph LR", "  A --> B"]
        );
        assert_eq!(cache.entries.len(), 1, "the failure is not replaced");
    }

    #[test]
    fn the_cache_keeps_itself_within_the_byte_bound() {
        let mut cache = DiagramCache::new();
        // No diagram renders to a megabyte, so the oversized entry is written
        // straight in: what is under test is the bound, not the crate.
        cache
            .entries
            .push((("x".repeat(DIAGRAM_CACHE_BYTES + 1), 1), None));
        assert!(cache.held_bytes() > DIAGRAM_CACHE_BYTES);

        let rows = markdown_rows("```mermaid\ngraph LR\n  A --> B\n```", 80, &mut cache);
        assert!(rows_text(&rows).iter().any(|row| row.contains('┌')));
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.held_bytes() <= DIAGRAM_CACHE_BYTES);
    }

    #[test]
    fn a_push_over_the_byte_bound_evicts_the_oldest_entry() {
        let mut cache = DiagramCache::new();
        // Two sources of 600 KiB. No diagram renders that large, so the crate
        // rejects both and each entry is its source text.
        let first = "x".repeat(3 * DIAGRAM_CACHE_BYTES / 5);
        let second = "y".repeat(3 * DIAGRAM_CACHE_BYTES / 5);

        assert!(cache.diagram_text(&first, 80).is_none());
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.held_bytes() <= DIAGRAM_CACHE_BYTES);

        // The second brings the total over the bound, so the first goes.
        assert!(cache.diagram_text(&second, 80).is_none());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].0.0, second, "the oldest entry goes");
        assert!(cache.held_bytes() <= DIAGRAM_CACHE_BYTES);
    }

    #[test]
    fn a_render_larger_than_the_byte_bound_is_returned_but_not_kept() {
        let mut cache = DiagramCache::new();
        markdown_rows("```mermaid\ngraph LR\n  A --> B\n```", 80, &mut cache);
        assert_eq!(cache.entries.len(), 1);

        // An entry over the whole budget would evict every older entry on the
        // way in and then itself, so it is not kept. It is still rendered.
        let huge = "x".repeat(DIAGRAM_CACHE_BYTES + 1);
        assert!(cache.diagram_text(&huge, 80).is_none());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].0.0, "graph LR\n  A --> B");
        assert!(cache.held_bytes() <= DIAGRAM_CACHE_BYTES);
    }

    #[test]
    fn fits_columns_counts_columns_not_characters() {
        // Six characters, twelve columns: a count of characters would call
        // this a fit at eleven.
        assert_eq!("使用者的問題".chars().count(), 6);
        assert!(fits_columns("使用者的問題", 12));
        assert!(!fits_columns("使用者的問題", 11));
    }
}
