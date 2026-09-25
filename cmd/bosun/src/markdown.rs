//! Minimal markdown rendering for the terminal client: turns a model's
//! markdown text into styled, width-wrapped ratatui lines. Covers the subset
//! models actually emit — headings, emphasis, inline code, code fences,
//! links, lists, blockquotes, rules and tables — and leaves anything else
//! plain. A ```mermaid fence renders as a diagram; anything the diagram
//! renderer cannot lay out keeps its source.

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

/// One line of prose: its inline styling, wrapped to `width`.
fn prose_rows(line: &str, width: usize) -> Vec<Vec<Span<'static>>> {
    wrap_tokens(&parse_inline(line, base()), width)
}

/// The rows one line draws as when it opens no multi-line block: a rule, a
/// heading, a blockquote, a list item, or plain prose. The fence and table
/// branches stay out of it, so a line they consumed cannot open them again.
fn block_rows(line: &str, width: usize) -> Vec<Vec<Span<'static>>> {
    if is_rule(line) {
        return vec![vec![Span::styled("─".repeat(width), rule())]];
    }
    if let Some(level) = heading_level(line) {
        let body = line.trim_start_matches('#').trim_start();
        return wrap_tokens(&parse_inline(body, heading(level)), width);
    }
    if let Some(rest) = line.strip_prefix('>') {
        let inner = width.saturating_sub(2).max(1);
        let wrapped = wrap_tokens(&parse_inline(rest.trim_start(), quote()), inner);
        let mut rows = Vec::new();
        for (i, row) in wrapped.iter().enumerate() {
            let gutter = if i == 0 { "│ " } else { "  " };
            let mut spans = vec![Span::styled(gutter, quote())];
            spans.extend(row.iter().cloned());
            rows.push(spans);
        }
        return rows;
    }
    let marker_len = list_marker(line);
    if marker_len > 0 {
        let (marker_text, body) = line.split_at(marker_len);
        let inner = width.saturating_sub(marker_len).max(1);
        let wrapped = wrap_tokens(&parse_inline(body, base()), inner);
        let mut rows = Vec::new();
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
        return rows;
    }
    prose_rows(line, width)
}

/// Where a column's cells sit inside the column's width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Align {
    Left,
    Right,
    Centre,
}

/// The cells of one table row: the text between its pipes, trimmed. A pipe at
/// either end is the table's edge, not a cell.
fn table_cells(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(str::trim).collect()
}

/// Whether `line` opens a table: a trimmed line starting with `|` and holding
/// at least two of them. A lone `|` in prose is not one.
fn is_table_start(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.matches('|').count() >= 2
}

/// The alignment each cell of a delimiter row asks for — `---:` right,
/// `:---:` centre, anything else left — or None when the line is not a
/// delimiter row: only pipes, dashes, colons and spaces, with a dash.
fn delimiter_aligns(line: &str) -> Option<Vec<Align>> {
    let trimmed = line.trim();
    if !trimmed.contains('-') {
        return None;
    }
    if !trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
        return None;
    }
    Some(
        table_cells(trimmed)
            .into_iter()
            .map(|cell| {
                if cell.starts_with(':') && cell.ends_with(':') {
                    Align::Centre
                } else if cell.ends_with(':') {
                    Align::Right
                } else {
                    Align::Left
                }
            })
            .collect(),
    )
}

/// The display width of one cell: its styled spans laid end to end, so a wide
/// glyph counts two columns.
fn cell_width(tokens: &[(String, Style)]) -> usize {
    tokens
        .iter()
        .map(|(text, _)| Span::raw(text.as_str()).width())
        .sum()
}

/// One row of the table: each cell padded to its column's width, one space
/// either side of it, and a `│` between the columns. A right or centre column
/// takes its alignment padding inside the width, and the last column carries
/// no space on its right, so no row ends in whitespace.
fn table_row(
    cells: &[Vec<(String, Style)>],
    widths: &[usize],
    aligns: &[Align],
    style: Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, width) in widths.iter().enumerate() {
        let pad = width.saturating_sub(cell_width(&cells[i]));
        let (left, right) = match aligns[i] {
            Align::Left => (0, pad),
            Align::Right => (pad, 0),
            Align::Centre => (pad / 2, pad - pad / 2),
        };
        let last = i + 1 == widths.len();
        if i > 0 {
            spans.push(Span::styled("│", rule()));
        }
        spans.push(Span::styled(" ".repeat(left + 1), style));
        spans.extend(
            cells[i]
                .iter()
                .map(|(text, style)| Span::styled(text.clone(), *style)),
        );
        let trailing = if last { 0 } else { right + 1 };
        if trailing > 0 {
            spans.push(Span::styled(" ".repeat(trailing), style));
        }
    }
    spans
}

/// The rule under the header row: `─` across every column and `┼` at the
/// joins, so it meets the `│` between the columns above and below it. The
/// last column's run stops where a row's trailing padding is trimmed.
fn table_rule(widths: &[usize]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, width) in widths.iter().enumerate() {
        let last = i + 1 == widths.len();
        if i > 0 {
            spans.push(Span::styled("┼", rule()));
        }
        spans.push(Span::styled(
            "─".repeat(width + if last { 1 } else { 2 }),
            rule(),
        ));
    }
    spans
}

/// The rows a table draws as, or None when it is wider than `width`. A row is
/// one line of the output and its cells cannot wrap, so a table that does not
/// fit keeps its source. `rows` is the header line followed by the body rows;
/// the delimiter row is not one of them.
fn table_rows(rows: &[&str], aligns: &[Align], width: usize) -> Option<Vec<Vec<Span<'static>>>> {
    let columns = aligns.len();
    let header_style = base().add_modifier(Modifier::BOLD);
    let mut cells = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        // The header reads bold, and its padding carries the same style.
        let style = if i == 0 { header_style } else { base() };
        let row_cells = table_cells(row);
        cells.push(
            (0..columns)
                .map(|c| parse_inline(row_cells.get(c).copied().unwrap_or(""), style))
                .collect::<Vec<_>>(),
        );
    }
    let mut widths = vec![0usize; columns];
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell_width(cell));
        }
    }
    // The width the whole table needs: every column's padded field and a bar
    // between the columns. The last column's field is one space narrower, as
    // no row ends in whitespace.
    let total: usize = widths
        .iter()
        .enumerate()
        .map(|(i, w)| w + if i + 1 == columns { 1 } else { 2 })
        .sum::<usize>()
        + columns
        - 1;
    if total > width {
        return None;
    }
    let mut laid = Vec::with_capacity(cells.len() + 1);
    laid.push(table_row(&cells[0], &widths, aligns, header_style));
    laid.push(table_rule(&widths));
    laid.extend(
        cells[1..]
            .iter()
            .map(|row| table_row(row, &widths, aligns, base())),
    );
    Some(laid)
}

/// Renders a markdown string as width-wrapped rows of styled spans. Blank
/// lines are preserved so paragraphs keep their spacing. A ```mermaid fence
/// renders as a diagram, drawn from `cache` so a redraw does not pay for it
/// again; every other fence shows its source. A table draws as one row per
/// table row, in the columns its delimiter row asks for.
pub fn markdown_rows(
    text: &str,
    width: usize,
    cache: &mut DiagramCache,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut lines = text.split('\n').peekable();
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
        // A table starts at a pipe row whose next line is a delimiter row of
        // the same width, and runs on through the lines that start with a
        // pipe. Its lines are consumed either way: a table too wide for the
        // pane draws each of them as it would have drawn on its own.
        if is_table_start(line)
            && let Some(aligns) = lines.peek().and_then(|next| delimiter_aligns(next))
            && aligns.len() == table_cells(line).len()
        {
            // The delimiter row is drawn as the rule under the header, never
            // as a row of its own.
            let delimiter = lines.next().expect("peeked the delimiter row").trim_end();
            let mut body = Vec::new();
            while let Some(next) = lines.peek() {
                if !next.trim_start().starts_with('|') {
                    break;
                }
                body.push(lines.next().expect("peeked a body row").trim_end());
            }
            let mut table_source = vec![line];
            table_source.extend(body.iter().copied());
            match table_rows(&table_source, &aligns, width) {
                Some(table) => rows.extend(table),
                None => {
                    rows.extend(block_rows(line, width));
                    rows.extend(block_rows(delimiter, width));
                    for row in &body {
                        rows.extend(block_rows(row, width));
                    }
                }
            }
            continue;
        }
        rows.extend(block_rows(line, width));
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

    /// The row's width in display columns, as the output pane measures it.
    fn row_width(row: &[Span<'_>]) -> usize {
        row.iter().map(|s| s.width()).sum()
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
    fn a_table_renders_its_columns_at_their_widest_cell() {
        let rows = rendered("| name | age |\n|---|---|\n| alice | 30 |", 40);
        assert_eq!(
            rows_text(&rows),
            vec![" name  │ age", "───────┼────", " alice │ 30"]
        );
        // The rule meets every `│`, so the columns stay aligned down the table.
        assert_eq!(row_width(&rows[0]), row_width(&rows[1]));
    }

    #[test]
    fn a_table_header_is_bold_and_its_rule_is_dim() {
        let rows = rendered("| name |\n|---|\n| alice |", 40);
        // The header row's second span holds the cell's text; the first is
        // the space before it, and the bold reaches both.
        assert_eq!(rows[0][1].content.as_ref(), "name");
        assert_eq!(rows[0][1].style, base().add_modifier(Modifier::BOLD));
        assert_eq!(rows[0][0].style, base().add_modifier(Modifier::BOLD));
        assert_eq!(rows[1][0].style, rule(), "the rule is dim");
        assert_eq!(rows[2][1].style, base(), "a body cell is plain");
    }

    #[test]
    fn a_table_takes_each_column_alignment_from_its_delimiter_cell() {
        // The last column is right, the middle one centred, the first left.
        let rows = rendered(
            "| item | qty | note |\n|:-----|:---:|-----:|\n| a | 5 | ok |",
            40,
        );
        assert_eq!(
            rows_text(&rows),
            vec![
                " item │ qty │ note",
                "──────┼─────┼─────",
                " a    │  5  │   ok"
            ]
        );
    }

    #[test]
    fn inline_styling_inside_a_table_cell_survives() {
        let rows = rendered("| one | two |\n|---|---|\n| **bold** | `code` |", 40);
        let row = &rows[2];
        let at = |needle: &str| {
            row.iter()
                .position(|s| s.content.as_ref() == needle)
                .expect(needle)
        };
        assert!(row[at("bold")].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(fg(row, at("code")), Some(Color::Yellow));
    }

    #[test]
    fn a_table_wider_than_the_width_keeps_its_source() {
        let source = "| name | age |\n|---|---|\n| alice | 30 |";
        // The table needs twelve columns with its padding; at eleven every
        // line of it shows as the prose it would be anyway, the delimiter row
        // included.
        assert_eq!(rows_text(&rendered(source, 12)).len(), 3);
        assert_eq!(
            rows_text(&rendered(source, 11)),
            vec!["| name |", "age |", "|---|---|", "| alice |", "30 |"]
        );
    }

    #[test]
    fn the_delimiter_row_draws_as_the_rule_not_a_row() {
        let rows = rendered("| a | b |\n| :--- | ---: |\n| c | d |", 40);
        assert_eq!(rows.len(), 3);
        assert!(
            !rows_text(&rows).iter().any(|row| row.contains("---")),
            "{:?}",
            rows_text(&rows)
        );
    }

    #[test]
    fn a_pipe_row_without_a_delimiter_row_stays_prose() {
        // The first line opens no table: its next line is not a delimiter
        // row. The last line holds one pipe, not two.
        let rows = rendered("| one | two |\nnot a delimiter\n\n| lone", 40);
        assert_eq!(
            rows_text(&rows),
            vec!["| one | two |", "not a delimiter", "", "| lone"]
        );
    }

    #[test]
    fn a_pipe_row_whose_next_line_is_a_shorter_delimiter_stays_prose() {
        // A delimiter row must have the header's cell count: one cell against
        // two is no table.
        let rows = rendered("| a | b |\n|---|\n| c | d |", 40);
        assert_eq!(rows_text(&rows), vec!["| a | b |", "|---|", "| c | d |"]);
    }

    #[test]
    fn a_lone_pipe_in_prose_opens_no_table() {
        // One pipe is not a table row, even with a delimiter row under it: a
        // guard that asked only for a leading pipe would draw a one-column
        // table here.
        let rows = rows_text(&rendered("| lone\n---", 40));
        assert_eq!(rows[0], "| lone");
        assert_eq!(rows[1], "─".repeat(40));
    }

    #[test]
    fn a_cell_pads_by_its_rendered_text_not_its_source() {
        // **bold** renders as the four columns of "bold", not the eight
        // characters of its source, so the column and the rule under it are
        // seven columns wide.
        let rows = rendered("| **bold** | x |\n|---|---|\n| y | z |", 40);
        assert_eq!(
            rows_text(&rows),
            vec![" bold │ x", "──────┼──", " y    │ z"]
        );
    }

    #[test]
    fn a_table_wider_than_the_width_draws_its_lines_as_they_would_draw_alone() {
        // One column holding a one-column cell needs two columns, so at one
        // the table falls back. Its delimiter row is only dashes, which draws
        // as a rule outside a table, and the fallback draws it that way.
        let rows = rendered("| a |\n---\n| b |", 1);
        assert_eq!(rows_text(&rows), vec!["|", "a", "|", "─", "|", "b", "|"]);
    }

    #[test]
    fn a_wide_glyph_in_a_table_cell_counts_two_columns() {
        // Six characters, twelve columns: counting characters would make the
        // column six wide and draw the rule five columns short of the cell.
        let rows = rendered("| name |\n|---|\n| 使用者的問題 |", 40);
        assert_eq!(row_width(&rows[2]), 13);
        assert_eq!(row_width(&rows[1]), 13);
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
