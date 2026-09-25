use axum::http::header;
use axum::response::IntoResponse;

/// The web pane: a self-contained page listing nodes and sessions, with a
/// live session view driven by the session API and the SSE event stream. The
/// page is data, embedded at compile time; no build step serves it.
pub async fn pane() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("ui/index.html"),
    )
}

/// The mermaid bundle the pane renders diagram fences with: mermaid 12.0.0,
/// vendored from the npm tarball and never edited. Embedded as data like the
/// pane, so the control plane needs no build step and no asset directory.
pub async fn mermaid_bundle() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            // 5.5 MB on every phone reload, and the bundle only changes with the control plane.
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("ui/mermaid.min.js"),
    )
}

#[cfg(test)]
mod tests {
    const PANE: &str = include_str!("ui/index.html");

    // The pane ships as one embedded HTML file with no browser test harness,
    // so these are presence checks, not behaviour tests.

    /// The pane's source from `start` to the next `end`, so a check reads one
    /// function or one case instead of the indentation around a single line.
    fn segment<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        source
            .split_once(start)
            .unwrap_or_else(|| panic!("the pane must contain {start}"))
            .1
            .split_once(end)
            .unwrap_or_else(|| panic!("the pane must contain {end} after {start}"))
            .0
    }

    /// The same source with every run of whitespace removed, so a check reads
    /// the tokens of a call and not the line breaks a formatter chose.
    fn squeezed(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn the_pane_remembers_the_children_groups_the_user_opened() {
        let declaration = PANE
            .find("const openGroups = new Set();")
            .expect("an open children group must outlive the render that showed it");
        let render = PANE
            .find("function renderSessions() {")
            .expect("the pane must have a session-list render");
        assert!(
            declaration < render,
            "the set must be declared before renderSessions, or a rebuild drops its state"
        );
        assert_eq!(
            PANE.matches("openGroups = new Set()").count(),
            1,
            "one set holds the open groups; a second assignment would drop them mid-render"
        );
    }

    #[test]
    fn the_pane_restores_an_open_children_group_when_it_rerenders_the_list() {
        let list = squeezed(segment(PANE, "function renderSessions() {", "\n}\n"));
        for token in [
            "const expanded = openGroups.has(root.id);",
            "toggle.className = expanded ? 'children-toggle open' : 'children-toggle';",
            "toggle.textContent = expanded ? 'hide children' : label;",
            "line.hidden = !expanded;",
        ] {
            assert!(
                list.contains(&squeezed(token)),
                "the rebuilt list must read the remembered state for {token}"
            );
        }
    }

    #[test]
    fn the_pane_records_the_children_group_the_toggle_opens_or_closes() {
        let list = squeezed(segment(PANE, "function renderSessions() {", "\n}\n"));
        assert!(
            list.contains(&squeezed(
                "if (open) openGroups.add(root.id); else openGroups.delete(root.id);"
            )),
            "the toggle must record its own state, so the next render restores it"
        );
    }

    #[test]
    fn the_pane_loads_the_mermaid_bundle_from_an_absolute_path() {
        assert!(
            PANE.contains(
                "<script src=\"/ui/mermaid.min.js\" id=\"mermaid-bundle\" defer></script>"
            ),
            "the pane is served at both / and /ui, so the bundle loads by absolute path"
        );
    }

    #[test]
    fn the_pane_renders_a_mermaid_fence_through_the_xml_parser() {
        let diagram = squeezed(segment(PANE, "async function renderMermaid(", "\n}"));
        for token in [
            "startOnLoad: false",
            "securityLevel: 'strict'",
            "htmlLabels: false",
            "theme: 'dark'",
            "flowchart: { useMaxWidth: false }",
            "new DOMParser().parseFromString(svg, 'image/svg+xml')",
            "parsed.querySelector('parsererror')",
            "document.importNode(parsed.documentElement, true)",
            // A failed render leaves this scratch element in the body.
            "document.getElementById('d' + id)",
            "holder.replaceWith(mdPre(source))",
        ] {
            assert!(
                diagram.contains(&squeezed(token)),
                "the diagram path must contain {token}"
            );
        }
        let markdown = squeezed(PANE);
        assert!(
            markdown.contains(&squeezed("fenceLanguage = fenceInfo(line.slice(3))"))
                && markdown.contains(&squeezed("if (closed && language === 'mermaid')"))
                && markdown.contains(&squeezed("renderMermaid(container, source)"))
                && markdown.contains(&squeezed("codeOut(true)"))
                && markdown.contains(&squeezed("codeOut(false)")),
            "only a closed fence whose language is mermaid may leave the <pre> path"
        );
    }

    #[test]
    fn the_pane_inserts_no_text_as_html() {
        assert!(
            !PANE.contains("innerHTML")
                && !PANE.contains("insertAdjacentHTML")
                && !PANE.contains("document.write"),
            "model text must reach the DOM as nodes, never as parsed HTML"
        );
    }

    #[test]
    fn the_pane_routes_activity_frames_to_the_console() {
        assert!(
            PANE.contains("case 'activity':") && PANE.contains("activities.push(event)"),
            "the pane must store activity frames for the console"
        );
    }

    #[test]
    fn the_pane_leads_a_session_row_with_its_summary() {
        let row = squeezed(segment(PANE, "function appendSessionRow(", "\n}"));
        assert!(
            row.contains(&squeezed("summary.textContent = session.summary"))
                && row.contains(&squeezed(
                    "nodeDir.className = session.summary ? 'row-meta' : 'row-node'"
                )),
            "a summarized session must lead its row with that line and drop node/dir to the meta line"
        );
    }

    #[test]
    fn the_pane_has_a_model_call_line_handler() {
        assert!(
            PANE.contains("case 'model_call':")
                && PANE.contains("appendLine('mono', modelCallLine(event), event.at_ms)")
                && PANE.contains("event.call_kind")
                && PANE.contains("event.cost.toFixed(4)"),
            "the pane must render a model_call as one monospace transcript line"
        );
    }

    #[test]
    fn the_pane_has_an_activity_console_toggled_from_the_status_line() {
        assert!(
            PANE.contains("id=\"activity-log\"")
                && PANE.contains("function phaseDetail(")
                && PANE.contains("function renderActivityConsole(")
                && PANE.contains("viewTitle.addEventListener('click'"),
            "the pane must toggle the activity console from the status line"
        );
    }

    #[test]
    fn the_pane_wires_the_running_status_to_the_newest_activity() {
        assert!(
            PANE.contains("function phaseLabel(")
                && PANE.contains("'awaiting model'")
                && PANE.contains("'running tool '")
                && PANE.contains("newest.received")
                && PANE.contains("window.setInterval(refreshStatusLabel, 1000)"),
            "the pane must label the running status from the newest activity"
        );
    }

    #[test]
    fn the_pane_renders_a_stamp_in_the_readers_local_time() {
        let clock = segment(PANE, "function clockTime(", "\n}");
        assert!(
            clock.contains("new Date(atMs)")
                && clock.contains("toLocaleTimeString")
                && clock.contains("hourCycle")
                && clock.contains("h23"),
            "the pane must convert the stamp to the reader's local 00-23 time"
        );
    }

    #[test]
    fn the_pane_stamps_a_durable_entry_and_skips_one_without_a_stamp() {
        let stamp = segment(PANE, "function stampRow(", "\n}");
        assert!(
            stamp.contains("== null") && stamp.contains("clockTime(") && stamp.contains("'ts'"),
            "the stamp helper must time the entry, and add nothing when the stamp is missing"
        );
    }

    #[test]
    fn the_pane_threads_the_event_stamp_into_every_durable_entry() {
        let source = squeezed(PANE);
        for call in [
            "renderMessage(event.message, event.at_ms)",
            "appendMsg(message.role === 'user' ? 'user' : 'assistant', block.text, atMs)",
            "appendAssistant(block.text, atMs)",
            "appendToolStrip(block.name, args, atMs)",
            "appendToolResult(block.name, payload, block.is_error, atMs)",
            "appendReasoningPanel(block.text, atMs)",
            "appendLine('summary', block.text, atMs)",
            "appendLine('unknown', JSON.stringify(block), atMs)",
            "renderAskBox(block, atMs);",
        ] {
            assert!(
                source.contains(&squeezed(call)),
                "the pane must pass the stamp to {call}"
            );
        }
        // The child report and the durable block that replaces the live
        // paragraph build their element inline, so each of those two stamps is
        // read from the case or the function it belongs to.
        let child_report = segment(PANE, "case 'child_event':", "default:");
        assert!(
            squeezed(child_report).contains(&squeezed("stampRow(line, atMs)")),
            "a child report line must carry the stamp"
        );
        let message = segment(PANE, "function renderMessage(", "\n}");
        assert!(
            squeezed(message).contains(&squeezed("liveEl.replaceWith(stampRow(container, atMs))")),
            "the durable block that replaces the live paragraph must carry the stamp"
        );
    }

    #[test]
    fn the_pane_stamps_the_entry_that_every_durable_append_helper_writes() {
        for helper in [
            "appendLine",
            "appendToolStrip",
            "appendReasoningPanel",
            "appendToolResult",
            "appendMsg",
            "appendAssistant",
            "renderAskBox",
        ] {
            let body = squeezed(segment(PANE, &format!("function {helper}("), "\n}"));
            assert!(
                body.contains(&squeezed("stampRow(")),
                "{helper} must put the event's stamp on the entry it appends"
            );
        }
    }

    #[test]
    fn the_pane_lays_the_stamp_out_as_a_gutter_column() {
        let row = segment(PANE, "#transcript .stamp-row {", "}");
        assert!(
            row.contains("display: flex") && row.contains("align-items: baseline"),
            "the stamp row must set the time beside the entry, on the entry's first line of text"
        );
        let entry = segment(PANE, "#transcript .stamp-row > :last-child {", "}");
        assert!(
            entry.contains("flex: 1") && entry.contains("min-width: 0"),
            "the entry must take the row's remaining width and still shrink on a phone"
        );
    }

    #[test]
    fn the_pane_leaves_the_live_delta_unstamped() {
        let delta = segment(PANE, "function appendDelta(", "\n}");
        assert!(
            !delta.contains("stampRow"),
            "a live delta is not durable and must carry no time"
        );
    }

    #[test]
    fn the_pane_renders_a_markdown_table_from_the_pane_branch() {
        let table = squeezed(segment(PANE, "function tableNode(", "\n}\n"));
        for token in [
            "holder.className = 'md-table-wrap'",
            "table.className = 'md-table'",
            "th.className = alignClass(align)",
            "td.className = alignClass(align)",
            "appendInline(th, header[index])",
            "appendInline(td, row[index] === undefined ? '' : row[index])",
        ] {
            assert!(
                table.contains(&squeezed(token)),
                "the table builder must contain {token}"
            );
        }
        let start = squeezed(segment(PANE, "function isTableStart(", "\n}\n"));
        assert!(
            start.contains(&squeezed(
                "text.startsWith('|') && (text.match(/\\|/g) || []).length >= 2"
            )),
            "a lone pipe in prose is not a table row"
        );
        // Left is a cell's default, so the builder writes no class for it and
        // the stylesheet has none.
        assert!(
            !PANE.contains("md-align-left"),
            "a left column needs no alignment class"
        );
        assert!(
            squeezed(segment(PANE, "function alignClass(", "\n}\n"))
                .contains(&squeezed("align === 'left' ? '' : 'md-align-' + align")),
            "only a right or centre column carries a class"
        );
        let markdown = squeezed(PANE);
        assert!(
            markdown.contains(&squeezed("if (isTableStart(line))"))
                && markdown.contains(&squeezed("const aligns = tableAligns(lines[i + 1] || '')"))
                && markdown.contains(&squeezed("if (aligns && aligns.length === header.length)"))
                && markdown.contains(&squeezed("if (!text.includes('-')) return null;"))
                && markdown.contains(&squeezed("body.push(tableCells(lines[i]))"))
                && markdown.contains(&squeezed(
                    "container.appendChild(tableNode(header, body, aligns))"
                )),
            "only a pipe row whose next line is a delimiter row of the same width may leave the prose path"
        );
        assert!(
            markdown.contains(&squeezed("i += 2;")),
            "the delimiter row draws as the header's rule, never as a row of its own"
        );
        assert!(
            squeezed(segment(PANE, "function tableAligns(", "\n}\n")).contains(&squeezed(
                "cell.startsWith(':') && cell.endsWith(':') ? 'center' : cell.endsWith(':') ? 'right' : 'left'"
            )),
            "a delimiter cell decides its column: :---: centre, ---: right, anything else left"
        );
    }

    #[test]
    fn the_pane_scrolls_a_wide_table_in_its_own_holder() {
        let holder = segment(PANE, "#transcript .md-table-wrap {", "}");
        assert!(
            holder.contains("overflow-x: auto"),
            "a wide table scrolls inside its holder, never sideways across the page"
        );
        let cell = segment(
            PANE,
            "#transcript .md-table th,\n  #transcript .md-table td {",
            "}",
        );
        assert!(
            cell.contains("border: 1px solid var(--border)") && cell.contains("color: var(--text)"),
            "a cell's border and text come from the palette"
        );
        let header = segment(PANE, "#transcript .md-table th {", "}");
        assert!(
            header.contains("background: var(--panel-2)") && header.contains("color: var(--muted)"),
            "the header row is a panel row, not a brighter one"
        );
        // The classes the builder writes are the classes the stylesheet sets.
        assert!(
            PANE.contains("#transcript .md-table .md-align-right { text-align: right; }")
                && PANE.contains("#transcript .md-table .md-align-center { text-align: center; }"),
            "both alignments a delimiter row can ask for must be styled"
        );
    }
}
