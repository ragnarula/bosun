use axum::http::header;
use axum::response::IntoResponse;

/// The web pane: a self-contained page listing nodes and sessions, with a
/// live session view driven by the session API and the SSE event stream. The
/// page is data, embedded at compile time; no build step serves it.
pub async fn pane() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // A phone that kept the old page keeps the bug it fixed: an open tab
            // and a history restore both serve from the browser's store, so the
            // pane is never stored.
            (header::CACHE_CONTROL, "no-store"),
        ],
        pane_html(),
    )
}

/// The pane's page, with the version that served it stamped in. The page stays
/// a static asset with one placeholder in it, so the only thing the server
/// writes is the version — which a phone can then be read for at a glance.
fn pane_html() -> String {
    include_str!("ui/index.html").replace("{{BOSUN_VERSION}}", bosun_common::version::VERSION)
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

    #[tokio::test]
    async fn the_pane_is_served_uncached_and_says_which_build_it_is() {
        use axum::response::IntoResponse as _;
        let response = super::pane().await.into_response();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .unwrap(),
            "no-store",
            "a phone that kept the old page keeps the bug it fixed, and a history restore serves from the browser's store"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains(&format!(
                "<meta name=\"bosun-version\" content=\"{}\">",
                bosun_common::version::VERSION
            )),
            "the page names the build that served it, so a phone's version can be read at a glance"
        );
        assert!(
            !body.contains("{{BOSUN_VERSION}}"),
            "and the placeholder it replaces is not served"
        );
    }

    /// The selector prefix the transcript's block rules carry: the session's
    /// transcript and the subagent panel's draw the same blocks, so one rule set
    /// names both containers. A check that reads a block rule uses this prefix
    /// rather than repeating it.
    const BLOCKS: &str = ":is(#transcript, #child-transcript) ";

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

    /// The statements the block after `start` holds, up to the brace that
    /// closes it, with the braces dropped. `start` is the text before that
    /// brace, so a check reads what a branch does wherever the pane's braces
    /// and line breaks fall.
    fn block(source: &str, start: &str) -> String {
        let after = source
            .split_once(start)
            .unwrap_or_else(|| panic!("the pane must contain {start}"))
            .1;
        let mut depth = 0usize;
        let mut opened = false;
        let mut body = String::new();
        for ch in after.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => {
                    depth -= 1;
                    if opened && depth == 0 {
                        return body;
                    }
                }
                _ if opened => body.push(ch),
                _ => {}
            }
        }
        panic!("the block after {start} must close");
    }

    /// Where `needle` starts in the body of the block after `start`, at the
    /// block's own level, so a check reads a statement the function always
    /// reaches rather than one a branch may skip. `None` means the statement is
    /// absent or stands inside a brace.
    fn top_level(source: &str, start: &str, needle: &str) -> Option<usize> {
        let after = source
            .split_once(start)
            .unwrap_or_else(|| panic!("the pane must contain {start}"))
            .1;
        let mut depth = 0i32;
        let mut opened = false;
        for (at, ch) in after.char_indices() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => {
                    depth -= 1;
                    if opened && depth == 0 {
                        return None;
                    }
                }
                _ => {}
            }
            if opened && depth == 1 && after[at..].starts_with(needle) {
                return Some(at);
            }
        }
        None
    }

    /// The pane's script with its comments removed, so a count reads code: a
    /// comment that names a function, writes an assignment or holds a
    /// `return;` is prose about the script, not a second copy of it. The
    /// quotes keep a `//` inside a string out of the stripper's way.
    fn code() -> String {
        let body = PANE
            .split_once("<script>\n")
            .expect("the pane must carry the inline script")
            .1
            .split_once("\n</script>")
            .expect("the inline script must close")
            .0;
        let mut out = String::with_capacity(body.len());
        let mut chars = body.chars().peekable();
        let mut quote = None;
        while let Some(ch) = chars.next() {
            if let Some(end) = quote {
                out.push(ch);
                if ch == '\\' {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else if ch == end {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' | '`' => {
                    quote = Some(ch);
                    out.push(ch);
                }
                '/' if chars.peek() == Some(&'/') => {
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push(next);
                            break;
                        }
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => out.push(ch),
            }
        }
        // Every character dropped must belong to a comment: a string or a
        // regex the stripper mistook for one would corrupt the copy that every
        // count reads, and this check fails a test instead.
        let mut rest = body.chars().peekable();
        let mut run = String::new();
        for kept in out.chars() {
            for dropped in rest.by_ref() {
                if dropped == kept {
                    break;
                }
                run.push(dropped);
            }
            if !run.is_empty() {
                assert!(
                    run.starts_with("//") || run.starts_with("/*"),
                    "the stripper dropped {run:?}, which is not a comment"
                );
                run.clear();
            }
        }
        for dropped in rest {
            run.push(dropped);
        }
        assert!(
            run.is_empty() || run.starts_with("//") || run.starts_with("/*"),
            "the stripper dropped {run:?}, which is not a comment"
        );
        out
    }

    /// The pane's stylesheet with its comments removed, so a check reads
    /// selectors and declarations rather than the prose beside them.
    fn styles() -> String {
        let css = PANE
            .split_once("<style>")
            .expect("the pane must carry its styles")
            .1
            .split_once("</style>")
            .expect("the stylesheet must close")
            .0;
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        loop {
            match rest.split_once("/*") {
                None => {
                    out.push_str(rest);
                    break;
                }
                Some((before, after)) => {
                    out.push_str(before);
                    match after.split_once("*/") {
                        None => break,
                        Some((_, tail)) => rest = tail,
                    }
                }
            }
        }
        out
    }

    /// `code()` with every run of whitespace outside a string literal removed,
    /// and every double quote written as a single one, so a check on a name or
    /// a spelling reads the same text however the pane respaces a call or
    /// requotes a string. Whitespace inside a literal stays: there it is part
    /// of a name the pane looks up, so an edit to one must read as a change.
    fn flattened() -> String {
        let body = code();
        let mut out = String::with_capacity(body.len());
        let mut chars = body.chars().peekable();
        let mut quote = None;
        while let Some(ch) = chars.next() {
            if let Some(end) = quote {
                out.push(ch);
                if ch == '\\' {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else if ch == end {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' | '`' => {
                    quote = Some(ch);
                    out.push(ch);
                }
                _ if ch.is_whitespace() => {}
                _ => out.push(ch),
            }
        }
        out.replace('"', "'")
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

    // These two pin the link scheme check and the branch that reads it.

    #[test]
    fn the_pane_follows_only_the_http_https_and_mailto_link_schemes() {
        let check = squeezed(segment(PANE, "function isSafeLinkTarget(", "\n}"));
        for token in [
            "const scheme = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(target);",
            "return ['http', 'https', 'mailto'].includes(scheme[1].toLowerCase());",
        ] {
            assert!(
                check.contains(&squeezed(token)),
                "the check must read the ASCII scheme before the first colon, allow only http, https and mailto, and compare without regard to case — so {token} stays"
            );
        }
    }

    #[test]
    fn the_pane_builds_an_anchor_only_for_a_target_the_check_allows() {
        let inline = squeezed(segment(PANE, "function appendInline(", "\n}\n"));
        assert!(
            inline.contains(&squeezed("if (token.url && isSafeLinkTarget(token.url)) {")),
            "the anchor branch must be guarded by the scheme check"
        );
        assert!(
            inline.contains(&squeezed("link.href = token.url;"))
                && inline.contains(&squeezed("link.textContent = token.text;")),
            "an allowed target is still the href, and the anchor still shows the link's text"
        );
        assert!(
            inline.contains(&squeezed("parent.appendChild(link); continue;")),
            "the anchor branch must leave by continue, so a token the check refuses reaches the text path below it rather than an anchor"
        );
        // One place in the pane builds an anchor and one assigns an href, and
        // both sit behind the guard. A check over the source can only see
        // these spellings of the two calls.
        assert_eq!(
            PANE.matches("document.createElement('a')").count(),
            1,
            "the pane builds an anchor in one place, and that spelling sits behind the guard"
        );
        assert_eq!(
            PANE.matches("link.href = ").count(),
            1,
            "the pane assigns an href in one place, and that spelling sits behind the guard"
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
            "appendToolStrip(block.name, args, atMs, block.id)",
            "appendToolResult(block.name, payload, block.is_error, atMs, block.id, content)",
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
        let row = segment(PANE, &format!("{BLOCKS}.stamp-row {{"), "}");
        assert!(
            row.contains("display: flex") && row.contains("align-items: baseline"),
            "the stamp row must set the time beside the entry, on the entry's first line of text"
        );
        let entry = segment(PANE, &format!("{BLOCKS}.stamp-row > :last-child {{"), "}");
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
        let holder = segment(PANE, &format!("{BLOCKS}.md-table-wrap {{"), "}");
        assert!(
            holder.contains("overflow-x: auto"),
            "a wide table scrolls inside its holder, never sideways across the page"
        );
        let cell = segment(
            PANE,
            &format!("{BLOCKS}.md-table th,\n  {BLOCKS}.md-table td {{"),
            "}",
        );
        assert!(
            cell.contains("border: 1px solid var(--border)") && cell.contains("color: var(--text)"),
            "a cell's border and text come from the palette"
        );
        let header = segment(PANE, &format!("{BLOCKS}.md-table th {{"), "}");
        assert!(
            header.contains("background: var(--panel-2)") && header.contains("color: var(--muted)"),
            "the header row is a panel row, not a brighter one"
        );
        // The classes the builder writes are the classes the stylesheet sets.
        assert!(
            PANE.contains(&format!(
                "{BLOCKS}.md-table .md-align-right {{ text-align: right; }}"
            )) && PANE.contains(&format!(
                "{BLOCKS}.md-table .md-align-center {{ text-align: center; }}"
            )),
            "both alignments a delimiter row can ask for must be styled"
        );
    }

    // The session's history: one entry for the open session above the list,
    // addressed by `#s=`, so the header's ‹ and the browser's back take the
    // same way out.

    #[test]
    fn the_pane_addresses_an_open_session_by_the_fragment_it_writes() {
        let link = squeezed(segment(PANE, "function sessionLink(", "\n}\n"));
        assert!(
            link.contains(&squeezed("return '#s=' + encodeURIComponent(id);")),
            "an open session is addressed as `#s=<session id>`"
        );
        let reader = segment(PANE, "function sessionFromLink(", "\n}\n");
        let from_link = squeezed(reader);
        assert!(
            from_link.contains(&squeezed("const match = /^#s=(.+)$/.exec(location.hash);"))
                && from_link.contains(&squeezed("if (!match) return null;")),
            "the reader must match the fragment the writer writes, against the address bar, and a fragment that is not one names no session"
        );
        assert!(
            squeezed(&block(reader, "try "))
                .contains(&squeezed("return decodeURIComponent(match[1]);")),
            "the id is decoded the way it was encoded, inside a try: a fragment the pane did not write must not throw out of the reader"
        );
        assert!(
            squeezed(&block(reader, "catch (")).contains(&squeezed("return null;")),
            "a fragment whose escape does not decode names no session"
        );
    }

    #[test]
    fn the_pane_tells_an_entry_of_its_own_by_the_state_it_writes() {
        let entry = squeezed(segment(PANE, "function paneEntry(", "\n}\n"));
        assert!(
            entry.contains(&squeezed("return { pane: id };")),
            "a pane entry carries the session it shows, and null while it shows the list"
        );
        let own = squeezed(segment(PANE, "function isOwnEntry(", "\n}\n"));
        assert!(
            own.contains(&squeezed(
                "return !!state && Object.prototype.hasOwnProperty.call(state, 'pane') && state.pane === id;"
            )),
            "one expression decides ownership: a state that is null or carries no `pane` key is not the pane's, and the key's value has to be the session's id, because the browser copies the state it is on onto a pasted fragment"
        );
    }

    #[test]
    fn the_pane_writes_the_list_entry_under_every_session_it_pushes() {
        let push = squeezed(segment(PANE, "function pushSession(", "\n}\n"));
        let mark = push
            .find("markListEntry();")
            .expect("a pushed session needs the list entry under it");
        let pushed = push
            .find("history.pushState(")
            .expect("a pushed session writes its own entry");
        assert!(
            mark < pushed,
            "the list entry must be written before the session is pushed, or the entry under the session is whatever the browser had"
        );
        assert!(
            push.contains(&squeezed(
                "history.pushState(paneEntry(id), '', sessionLink(id));"
            )),
            "the pushed entry must carry the fragment, or a reload or a shared copy of it names no session"
        );
        assert_eq!(
            PANE.matches("history.pushState").count(),
            1,
            "one place pushes a session entry"
        );
        assert_eq!(
            PANE.matches("history.replaceState").count(),
            2,
            "one place writes the list entry, and the session that takes over the one open writes its own entry in place"
        );
    }

    #[test]
    fn the_pane_opens_every_session_through_the_history_path() {
        let open = squeezed(segment(PANE, "function openSession(", "\n}\n"));
        assert!(
            open.contains(&squeezed(
                "if (current && isOwnEntry(history.state, current)) {"
            )) && open.contains(&squeezed(
                "history.replaceState(paneEntry(id), '', sessionLink(id));"
            )) && open.contains(&squeezed("pushSession(id);")),
            "the session already open owns the entry on screen and the new session takes it; from anywhere else the list entry goes under a pushed session, because the entry there may be one the pane did not write"
        );
        assert!(
            open.contains(&squeezed("showSession(id);")),
            "the entry and the view are one act, so no caller can open a session without one"
        );
        assert_eq!(
            PANE.split_once("function openSession(")
                .expect("the pane must have one way into a session")
                .1
                .split_once(") {")
                .expect("openSession must take the session id alone")
                .0,
            "id",
            "openSession takes the session id alone: the pane's position decides push or replace, so no call site can choose"
        );
    }

    #[test]
    fn the_pane_leaves_a_session_the_way_the_browser_does() {
        assert_eq!(
            PANE.matches("history.back()").count(),
            1,
            "one call leaves a session: the header's ‹ and the browser's back button must be the same one"
        );
        assert!(
            PANE.contains("btnBack.addEventListener('click', () => history.back())"),
            "the ‹ control must take the history path"
        );
        assert!(
            !PANE.contains("btnBack.addEventListener('click', closeSession)"),
            "a second way out of a session would drift from the browser's back button"
        );
        let raw = segment(PANE, "function followHistory(", "\n}\n");
        let follow = squeezed(raw);
        for token in [
            "const id = sessionFromLink();",
            "if (id === current) return;",
            "if (!isOwnEntry(history.state, id)) pushSession(id);",
            "showSession(id);",
        ] {
            assert!(
                follow.contains(&squeezed(token)),
                "the fragment decides the screen — the session it names, or the list — and an entry the pane did not write gets the list entry under the session first: {token} stays"
            );
        }
        let list_branch = squeezed(&block(raw, "if (!id)"));
        let closes = list_branch
            .find(&squeezed("closeSession();"))
            .expect("an entry naming no session must close the view");
        let stops = list_branch
            .find(&squeezed("return;"))
            .expect("an entry naming no session must stop the branch there");
        assert!(
            closes < stops,
            "the close must stand before the branch's return, or the code after the branch runs against a missing id"
        );
        assert!(
            !list_branch.contains(&squeezed("showSession(")),
            "an entry naming no session must not open one"
        );
        assert!(
            PANE.contains("window.addEventListener('popstate', followHistory)"),
            "back and forward must run the pane's own history path"
        );
    }

    #[test]
    fn the_pane_starts_on_the_entry_the_address_bar_names() {
        let raw = segment(PANE, "function startFromLink(", "\n}\n");
        let start = squeezed(raw);
        assert!(
            start.contains(&squeezed("const id = sessionFromLink();"))
                && start.contains(&squeezed("if (isOwnEntry(history.state, id)) {")),
            "a load starts on the session the fragment names, told from a fresh link by the state the pane wrote"
        );
        let list_branch = squeezed(&block(raw, "if (!id)"));
        assert!(
            list_branch.contains(&squeezed("markListEntry();"))
                && !list_branch.contains(&squeezed("showSession(")),
            "a load naming no session marks the entry it starts on and opens nothing"
        );
        let reload = squeezed(&block(raw, "if (isOwnEntry(history.state, id))"));
        let opens = reload
            .find(&squeezed("if (id) showSession(id);"))
            .expect("the pane's own entry reopens its session");
        let stops = reload
            .find(&squeezed("return;"))
            .expect("the pane's own entry writes nothing");
        assert!(
            opens < stops,
            "the reopened session must come before the branch's return, or the reload falls through to the code that writes an entry"
        );
        assert!(
            !reload.contains(&squeezed("pushSession(")),
            "a reload adds no entry, so the list stays the entry under the session"
        );
        assert!(
            PANE.contains("startFromLink();"),
            "the pane must run the load path when it starts"
        );
    }

    #[test]
    fn the_pane_clears_the_fragment_when_the_session_is_gone() {
        let mark = squeezed(segment(PANE, "function markListEntry(", "\n}\n"));
        assert!(
            mark.contains(&squeezed(
                "history.replaceState(paneEntry(null), '', location.pathname + location.search);"
            )),
            "the list entry keeps the path the pane was served at, so it needs no route of its own"
        );
        let stop = segment(PANE, "btnStop.addEventListener('click'", "\n});");
        let stopped = squeezed(stop);
        let captured = stopped
            .find(&squeezed("const id = current;"))
            .expect("the stop must take the id of the session it was pressed for");
        let posted = stopped
            .find(&squeezed("await post('/stop'"))
            .expect("the stop must post that id");
        assert!(
            captured < posted,
            "the id must be taken before the post, or the reply is read against whatever session the pane shows by then"
        );
        let guarded = squeezed(&block(stop, "if (current === id)"));
        let rewrites = guarded
            .find(&squeezed("markListEntry();"))
            .expect("a stop rewrites the entry of the session it stopped");
        let closes = guarded
            .find(&squeezed("closeSession();"))
            .expect("a stop closes the view of the session it stopped");
        assert!(
            rewrites < closes,
            "inside the guard, the entry is rewritten before the view closes, and both stand inside it, or a stop that lands after the user moved on rewrites the entry of the session on screen now"
        );
        let poll = segment(PANE, "async function refreshSessions(", "\n}\n");
        let gone = squeezed(&block(poll, "if (current && !viewed)"));
        let says = gone
            .find(&squeezed("showStatus('session ' + current + ' ended');"))
            .expect("the poll must say which session ended");
        let marks = gone
            .find(&squeezed("markListEntry();"))
            .expect("the poll must clear the fragment of the session that ended");
        let closs = gone
            .find(&squeezed("closeSession();"))
            .expect("the poll must close the view of the session that ended");
        assert!(
            says < marks && marks < closs,
            "the poll's miss branch must name the session, then clear the fragment, then close the view"
        );
        let detail = segment(PANE, "async function fetchSession(", "\n}\n");
        let not_here = squeezed(&block(detail, "if (response.status === 404)"));
        let says = not_here
            .find(&squeezed("showStatus('session ' + id + ' ended');"))
            .expect("a missing session is named in the status line");
        let marks = not_here
            .find(&squeezed("markListEntry();"))
            .expect("a missing session's entry stops naming it");
        let closs = not_here
            .find(&squeezed("closeSession();"))
            .expect("a missing session's view closes");
        let stops = not_here
            .find(&squeezed("return;"))
            .expect("the branch must return before the reply's other cases");
        assert!(
            says < marks && marks < closs && closs < stops,
            "the 404 branch must name the session, clear the fragment, close the view and return, all from that branch"
        );
    }

    #[test]
    fn the_pane_ignores_a_reply_for_a_session_it_left() {
        let raw = segment(PANE, "async function fetchSession(", "\n}\n");
        let detail = squeezed(raw);
        let guard = squeezed("if (current !== id) return;");
        let (before, after) = detail
            .split_once(&squeezed("if (response.status === 404)"))
            .expect("the fetch must tell a session that is gone from one it cannot read");
        assert!(
            before.contains(&guard),
            "the session on screen must be checked before the 404 branch, or a late 404 closes the session the pane moved to"
        );
        let body = after
            .split_once(&squeezed("await response.json();"))
            .expect("the control plane's answer is read after the 404 branch");
        let (_, written) = body
            .1
            .split_once(&guard)
            .expect("the session on screen must be checked after the body read too, or a late body writes the header of a session the pane left");
        assert!(
            written.starts_with("updateHeader("),
            "the check must stand between the body read and the header write, the only two things a late reply could reach"
        );
        let caught = squeezed(&block(raw, "catch ("));
        let checked = caught
            .find(&guard)
            .expect("a failure for a session the pane has left must write nothing");
        let reported = caught
            .find(&squeezed("showStatus('session: ' + error.message);"))
            .expect("the failure the pane still owns must reach the status line");
        assert!(
            checked < reported,
            "the check must stand before the status write, or a failed fetch of a session the pane has left reports on the screen it left"
        );
    }

    #[test]
    fn the_pane_leaves_nothing_of_a_session_on_screen_when_it_closes_one() {
        let close = squeezed(segment(PANE, "function closeSession(", "\n}\n"));
        assert!(
            close.contains(&squeezed("askSheet.hidden = true;"))
                && close.contains(&squeezed("viewSheet.hidden = true;")),
            "the ⋯ sheet is a sibling of #session-view and the ask sheet keeps its own state, so closing a session must hide each one"
        );
        assert!(
            close.contains(&squeezed("clearHeader();")),
            "the header and the sheet carry the open session's identity, so closing a session must clear them"
        );
        assert!(
            !close.contains("history."),
            "the teardown writes no history; the handlers that leave a session own that"
        );
        let header = squeezed(segment(PANE, "function clearHeader(", "\n}\n"));
        for field in [
            "viewStateDot.className = 'dot'",
            "viewNode.textContent = ''",
            "viewDir.textContent = ''",
            "viewIdCopy.textContent = ''",
            "viewSheetMeta.textContent = ''",
            "viewWaiting.textContent = ''",
            "viewWaiting.hidden = true",
            "viewPermission.textContent = ''",
            "btnPermission.textContent = 'Switch to read-only'",
            "personaName.value = ''",
            "inputRow.hidden = false",
            "watchBanner.hidden = true",
            "rowPermission.hidden = false",
            "rowPersona.hidden = false",
            "rowInterrupt.hidden = false",
            "rowStop.hidden = false",
        ] {
            assert!(
                header.contains(&squeezed(field)),
                "clearHeader must clear {field}, or the session the pane left keeps naming itself on screen"
            );
        }
    }

    #[test]
    fn the_pane_returns_to_the_bottom_when_the_reader_types_in_the_composer() {
        let pane = flattened();
        assert_eq!(
            pane.matches(&squeezed("function jumpToBottom")).count(),
            1,
            "one `function jumpToBottom` runs: JavaScript runs the last declaration, and these checks read the first"
        );
        assert_eq!(
            pane.matches(&squeezed("input.addEventListener('input'"))
                .count(),
            1,
            "one `input.addEventListener('input'` runs; these checks read the first"
        );
        for declaration in [
            "const input = $('input');",
            "const transcript = $('transcript');",
        ] {
            assert!(
                pane.contains(&squeezed(declaration)),
                "`{declaration}` must stand: the checks below read that name for the element, and only a const binding keeps a later assignment from pointing it elsewhere"
            );
        }
        assert!(
            !pane.contains(&squeezed("jumpToBottom =")),
            "nothing assigns over `jumpToBottom`: an assignment runs in place of the declaration these checks read"
        );
        assert_eq!(
            block(&pane, &squeezed("input.addEventListener('input',")),
            squeezed("saveDraft(); jumpToBottom();"),
            "a keystroke must keep the chat draft and return the transcript to the bottom, or the message the reader is writing and the answer to it stay below the fold"
        );
        assert_eq!(
            block(&pane, &squeezed("function jumpToBottom")),
            squeezed(
                "stick = true; transcript.scrollTop = transcript.scrollHeight; syncBtnBottom();"
            ),
            "jumpToBottom must set auto-scroll, move the transcript itself rather than through the guarded follow, and hide the control the move makes redundant: a keystroke from a scrolled-up reader would otherwise leave them where they were, with a control still on screen"
        );
    }

    #[test]
    fn the_pane_returns_to_the_bottom_when_the_composer_sends() {
        let pane = flattened();
        assert_eq!(
            pane.matches(&squeezed("function send(")).count(),
            1,
            "one `function send(` runs: JavaScript runs the last declaration, so a second one leaves the button and the Enter key with a no-op"
        );
        assert_eq!(
            pane.matches(&squeezed("input.addEventListener('keydown'"))
                .count(),
            1,
            "one `input.addEventListener('keydown'` runs; this check reads the first"
        );
        assert_eq!(
            pane.matches(&squeezed("btnSend.addEventListener('click'"))
                .count(),
            1,
            "one `btnSend.addEventListener('click'` runs; this check reads the first"
        );
        assert!(
            !pane.contains(&squeezed("send =")),
            "nothing assigns over `send`: an assignment runs in place of the declaration these checks read"
        );
        assert!(
            pane.contains(&squeezed("const btnSend = $('btn-send');")),
            "`const btnSend = $('btn-send');` must stand: the checks below read that name for the button, and only a const binding keeps a later assignment from pointing it elsewhere"
        );
        let send = block(&pane, &squeezed("function send("));
        assert!(
            send.starts_with(&squeezed(
                "if (sending || !input.value.trim()) return; jumpToBottom();"
            )),
            "the return must be the first thing send does, so no statement ahead of it can leave the jump as dead code"
        );
        let posts = send
            .find(&squeezed("await post("))
            .expect("send must post the message");
        assert_eq!(
            send[..posts].matches("return").count(),
            1,
            "one `return` before the post, the guard's: a second one between the guard and the post skips the jump, strands `sending`, or sends nothing"
        );
        let jumps = send.find(&squeezed("jumpToBottom();")).expect(
            "a sent message must return the transcript to the bottom, or its answer lands below the fold",
        );
        assert!(
            jumps < posts,
            "the return must stand before the post, so the reader sees the bottom without waiting on the network"
        );
        assert_eq!(
            block(&pane, &squeezed("function jumpToBottom")),
            squeezed(
                "stick = true; transcript.scrollTop = transcript.scrollHeight; syncBtnBottom();"
            ),
            "jumpToBottom must set auto-scroll, move the transcript itself rather than through the guarded follow, and hide the control the move makes redundant, or the answer to the sent message does not land in view"
        );
        let wired = pane
            .split_once(&squeezed("btnSend.addEventListener('click', send);"))
            .expect("the button must keep its click wiring")
            .1;
        assert_eq!(
            wired.matches(&squeezed("btnSend")).count() + wired.matches("btn-send").count(),
            0,
            "nothing after `btnSend.addEventListener('click', send);` names the button again: a statement there undoes the wiring or disables the button, and a phone has no other way to send"
        );
        assert_eq!(
            block(&pane, &squeezed("input.addEventListener('keydown'")),
            squeezed(
                "if (event.key === 'Enter' && (event.ctrlKey || event.metaKey))
                 event.preventDefault();
                 send();"
            ),
            "Ctrl/Cmd+Enter must reach the same send, or the key the reader presses on a phone keyboard sends nothing"
        );
    }

    #[test]
    fn the_pane_keeps_following_while_the_end_of_the_transcript_is_in_view() {
        let pane = flattened();
        assert!(
            pane.contains(&squeezed("let stick = true;")),
            "`let stick = true;` must stand: the flag opens armed, and the scroll listener and jumpToBottom both assign it"
        );
        assert!(
            pane.contains(&squeezed("const transcript = $('transcript');")),
            "`const transcript = $('transcript');` must stand: the checks here read that name for the box that scrolls, and only a const binding keeps a later assignment from pointing it elsewhere"
        );
        assert_eq!(
            pane.matches(&squeezed("transcript.addEventListener('scroll'"))
                .count(),
            1,
            "one `transcript.addEventListener('scroll'` runs; this check reads the first"
        );
        assert!(
            PANE.contains("transcript.addEventListener('scroll', syncStick);"),
            "the scroll listener must read the flag and put the control away with it: a listener that clears `stick` on every scroll takes back the jump a keystroke or a send just made, and the sent message and its answer land below the fold again"
        );
        assert!(
            pane.contains(&squeezed(
                "function syncStick() { stick = transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 40; syncBtnBottom(); }"
            )),
            "and that read lives in one place, so the keyboard's resize can ask the same question"
        );
    }

    #[test]
    fn the_pane_renders_a_cleared_context_as_a_break() {
        let pane = flattened();
        assert!(
            pane.contains(&squeezed("case 'context_cleared':")),
            "a cleared context must render as itself, not fall through to the raw block"
        );
        let line = segment(PANE, "case 'context_cleared':", "break;");
        assert!(
            line.contains("block.reason"),
            "the marker line names the reason"
        );
        assert!(
            !line.contains("block.instructions"),
            "the line does not repeat the fresh instructions: they stand below it as the row the session continues from"
        );
        assert!(
            pane.contains(&squeezed("appendLine('cleared',")),
            "the marker appends as a line of its own"
        );
        assert!(
            PANE.contains(&format!("{BLOCKS}.cleared {{")),
            "the marker is styled as the break it draws"
        );
    }

    // The control that brings a reader who scrolled up back to the newest line.
    // It lives in the transcript's box, so the session view hides it, and its
    // two states are the two states of `stick`.

    #[test]
    fn the_pane_holds_the_bottom_control_with_the_transcript_it_moves() {
        let view_at = PANE
            .find("<div id=\"session-view\"")
            .expect("the pane must have the session view");
        let box_at = PANE
            .find("<div id=\"transcript-wrap\"")
            .expect("the pane must have the transcript's box");
        assert!(
            view_at < box_at,
            "the box, and the control inside it, must stand in the session view, or they show over the list the view hides them with"
        );
        let region = segment(
            PANE,
            "<div id=\"transcript-wrap\">",
            "<div id=\"watch-banner\"",
        );
        let closes = region
            .rfind("</div>")
            .expect("the box that holds the transcript must close before the watch banner");
        let between = region
            .split_once("id=\"transcript\"")
            .expect("the box must hold the transcript")
            .1
            .split_once("id=\"btn-bottom\"")
            .expect("the box must hold the control")
            .0;
        assert_eq!(
            squeezed(between),
            "></div><buttontype=\"button\"",
            "the control is the transcript's next sibling inside the box, never its child: the teardown empties the transcript, and a control the pane built into it goes with the lines"
        );
        let held = region
            .find("id=\"btn-bottom\"")
            .expect("the box must hold the control");
        assert!(
            held < closes,
            "the control must stand before the tag that closes the box, or its offset anchors to the session view instead, and it lands over the composer"
        );
        let tag = segment(PANE, "<button type=\"button\" id=\"btn-bottom\"", ">");
        assert_eq!(
            squeezed(tag),
            squeezed("hidden title=\"Back to the newest line\""),
            "the control opens hidden and its tag carries that attribute and a label, nothing else: an attribute that hides it another way, or one that turns the press off, leaves the reader with a control they cannot use"
        );
        assert_eq!(
            PANE.matches("id=\"btn-bottom\"").count(),
            1,
            "one element carries the control's id"
        );
    }

    #[test]
    fn the_pane_floats_the_bottom_control_above_the_composer_at_thumb_size() {
        for selector in [
            "#transcript-wrap {",
            "#btn-bottom {",
            "#btn-bottom[hidden] {",
        ] {
            assert_eq!(
                PANE.matches(selector).count(),
                1,
                "one rule for `{selector}`: a second rule with that selector wins over the one these checks read"
            );
        }
        let wrap = segment(PANE, "#transcript-wrap {", "}");
        assert_eq!(
            squeezed(wrap),
            squeezed("position: relative; flex: 1; min-height: 0; display: flex;"),
            "the box takes the transcript's place in the view, anchors the control, and holds nothing else: a plain box loses the flex and the transcript grows to its content and spills over the composer, a floor above zero stops the box shrinking to leave the composer on screen, and any further declaration here moves the box out of its place"
        );
        let control = segment(PANE, "#btn-bottom {", "}");
        assert_eq!(
            squeezed(control),
            squeezed(
                "position: absolute; right: 16px; bottom: 14px;
                 width: var(--touch-min-height); height: var(--touch-min-height);
                 padding: 0; border-radius: 50%; background: var(--panel-2);
                 color: var(--accent); font-size: 18px;
                 box-shadow: 0 4px 12px rgba(0, 0, 0, 0.32);"
            ),
            "the control floats in the box's lower corner at the one-handed size floor, and the rule holds nothing else: a display or visibility rule here would keep the control off screen at the bottom too, where the reader has no way back"
        );
        assert!(
            PANE.contains("#btn-bottom[hidden] { display: none; }"),
            "a hidden control must leave the screen, not sit over the transcript as an empty target"
        );
    }

    #[test]
    fn the_pane_shows_the_bottom_control_while_the_reader_is_off_the_newest_line() {
        let pane = flattened();
        assert_eq!(
            pane.matches(&squeezed("function syncBtnBottom")).count(),
            1,
            "one `function syncBtnBottom` runs: JavaScript runs the last declaration, and these checks read the first"
        );
        assert!(
            !pane.contains(&squeezed("syncBtnBottom =")),
            "nothing assigns over `syncBtnBottom`: an assignment runs in place of the declaration these checks read"
        );
        assert!(
            pane.contains(&squeezed("const btnBottom = $('btn-bottom');")),
            "`const btnBottom = $('btn-bottom');` must stand: the checks below read that name for the control, and only a const binding keeps a later assignment from pointing it elsewhere"
        );
        assert_eq!(
            pane.matches("btn-bottom").count(),
            1,
            "the script reaches the control by its id in the binding alone: a second lookup could write the control's state beside the one place that does"
        );
        assert_eq!(
            block(&pane, &squeezed("function syncBtnBottom")),
            squeezed("btnBottom.hidden = stick;"),
            "the control is hidden while the transcript is at its end, the state `stick` holds: any other rule shows it to a reader who is already at the bottom, or hides it from the reader who is not"
        );
        let sync = block(&pane, &squeezed("function syncStick"));
        let read = sync
            .find(&squeezed("stick = transcript.scrollTop"))
            .expect("the flag must be recomputed from where the transcript sits");
        let synced = sync
            .find(&squeezed("syncBtnBottom();"))
            .expect("which must sync the control, or a reader who scrolls up keeps a screen with no way back to the newest line");
        assert!(
            read < synced,
            "the sync must stand after the flag is recomputed, or the control shows the state the reader just left"
        );
        assert!(
            PANE.contains("transcript.addEventListener('scroll', syncStick);"),
            "and the scroll listener must ask it, so a scroll is still what changes the control"
        );
        assert_eq!(
            pane.matches(&squeezed("btnBottom.hidden")).count(),
            1,
            "one place writes the control's hidden state, so a scroll, a move and a close cannot disagree"
        );
    }

    #[test]
    fn the_pane_returns_to_the_bottom_when_the_bottom_control_is_pressed() {
        let pane = flattened();
        assert_eq!(
            pane.matches(&squeezed("btnBottom.addEventListener"))
                .count(),
            1,
            "the control has one press path"
        );
        assert!(
            pane.contains(&squeezed(
                "function syncBtnBottom() { btnBottom.hidden = stick; } btnBottom.addEventListener('click', jumpToBottom);"
            )),
            "one press must run the composer's own forced move rather than a second copy of it, and the wiring must stand at the top level beside the rule it settles: a call inside a branch, or an assignment in place of the declaration, leaves the reader the control exists for with no way back"
        );
    }

    #[test]
    fn the_pane_resets_the_bottom_control_when_it_closes_a_session() {
        let pane = flattened();
        for session in ["closeSession", "showSession"] {
            assert_eq!(
                pane.matches(&squeezed(&format!("function {session}")))
                    .count(),
                1,
                "one `function {session}` runs: JavaScript runs the last declaration, and these checks read the first"
            );
        }
        let teardown = squeezed("function closeSession(");
        let armed = top_level(&pane, &teardown, &squeezed("stick = true;"))
            .expect("closing a session must re-arm auto-follow at a statement the teardown always reaches, or a branch around it leaves the session the pane opens next following nothing");
        let synced = top_level(&pane, &teardown, &squeezed("syncBtnBottom();"))
            .expect("closing a session must put the control away at a statement the teardown always reaches, or a branch around it leaves the control of the session the reader left over the next one");
        assert!(
            armed < synced,
            "the teardown must arm auto-follow before it syncs the control, or the control is left showing the state of the session the reader just left"
        );
        let open = block(&pane, &squeezed("function showSession"));
        assert!(
            open.starts_with(&squeezed("closeSession();")),
            "every open runs the teardown first, so a reopened session starts on its newest line with the control hidden"
        );
    }

    // A text field is 16px at every width. A focused control under 16px makes
    // iOS Safari zoom the page, and a phone in landscape is wider than the
    // breakpoint that guard used to live in.

    #[test]
    fn the_pane_keeps_its_text_fields_at_sixteen_pixels() {
        // Every rule that names a text field, wherever it sits — the sheet, a
        // narrow-screen block, a later rule — must set at least 16px: one rule
        // that lowered it would put the zoom back, and a landscape phone is
        // wider than the breakpoint the guard used to live in.
        let css = styles();
        let mut rest = css.as_str();
        let mut read = 0;
        while let Some((before, after)) = rest.split_once('{') {
            let selector = squeezed(before.rsplit('}').next().unwrap_or(before));
            let Some((body, tail)) = after.split_once('}') else {
                break;
            };
            rest = tail;
            let names_a_field = [
                "#input",
                "#ask-input",
                ".chat-rowtextarea",
                ".ask-freetextarea",
                ".composerselect",
            ]
            .iter()
            .any(|field| selector.contains(field));
            if !names_a_field {
                continue;
            }
            read += 1;
            for (at, _) in body.match_indices("font-size:") {
                let size: u32 = body[at + "font-size:".len()..]
                    .trim_start()
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_default();
                assert!(
                    size >= 16,
                    "`{selector}` sets {size}px on a text field: under 16px a phone zooms the page when it is focused"
                );
            }
        }
        assert!(
            read >= 3,
            "the walk must have read the composer, the ask field and the picker's rules: read {read}"
        );
        for rule in [
            ".chat-row textarea {",
            ".ask-free textarea {",
            ".composer select {",
        ] {
            assert!(
                PANE.contains(rule),
                "`{rule}` must be in the sheet, not only inside a media query"
            );
        }
    }

    #[test]
    fn the_pane_takes_no_focus_without_a_gesture_on_a_coarse_pointer() {
        assert!(
            PANE.contains(
                "const coarsePointer = () => window.matchMedia('(pointer: coarse)').matches;"
            ),
            "the pane needs one place that knows a touch screen"
        );
        assert!(
            PANE.contains("if (!inputRow.hidden && !coarsePointer()) input.focus();"),
            "opening a session must not take the composer's focus on a touch screen: that focus is the one that sometimes does not land"
        );
        assert!(
            PANE.contains("if (!coarsePointer()) askInput.focus();"),
            "and the free-answer branch `renderAskComposer` opens for an arriving question must not take the ask field's focus either"
        );
        assert_eq!(
            PANE.matches("askInput.focus();").count(),
            2,
            "the ask field has two focuses: the arriving one, gated, and the one the `answer in your own words` button takes"
        );
        assert!(
            segment(PANE, "btnAskType.addEventListener('click', () => {", "});")
                .contains("askInput.focus();"),
            "the `answer in your own words` button still focuses the free-answer field, where the tap is the gesture"
        );
        assert!(
            segment(
                PANE,
                "function send(",
                "btnSend.addEventListener('click', send);"
            )
            .contains("input.focus();"),
            "and send still re-focuses the composer in its `finally`, where the tap is the gesture"
        );
    }

    // The one guard every toggled overlay needs. The UA's `[hidden] { display:
    // none }` loses to any author rule that sets `display` on the same element,
    // so an overlay the code believes is hidden can stay rendered — a
    // full-screen `#child-panel` at phone widths, above the session view,
    // eating taps meant for the composer.

    #[test]
    fn the_pane_guards_every_hidden_overlay_against_an_author_display() {
        assert!(
            PANE.contains("[hidden] { display: none !important; }"),
            "a global rule is what holds for any overlay a later rule could outrank"
        );
        assert_eq!(
            PANE.matches("!important").count(),
            1,
            "and it is the only `!important` in the sheet: any other would be a fight the reader loses"
        );
        for (guard, why) in [
            (
                "#child-panel[hidden]",
                "the panel is a full-screen layer at phone widths",
            ),
            ("#view-sheet[hidden]", "the actions sheet covers the view"),
            ("#ask-sheet[hidden]", "the ask sheet sits in the composer"),
            ("#activity-log[hidden]", "the console"),
            ("#watch-banner[hidden]", "the watch banner"),
            (".chat-row[hidden]", "the composer's row"),
            (".input-row[hidden]", "the composer"),
        ] {
            assert!(
                PANE.contains(&format!("{guard} {{ display: none; }}")),
                "`{guard}` keeps its own guard as well ({why}), so the element reads as hidden even where the global rule is not loaded"
            );
        }
    }

    // The composer rides the iOS keyboard: the session view follows the visual
    // viewport, which is the part of the page a reader can see, and the bottom
    // rows keep clear of the Home indicator's strip.

    #[test]
    fn the_pane_rides_the_visual_viewport_and_keeps_clear_of_the_home_indicator() {
        let pane = flattened();
        assert!(
            PANE.contains(
                "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1, viewport-fit=cover\">"
            ),
            "the page has to reach under the Home indicator before an inset can hold anything clear of it"
        );
        let handler = block(&pane, &squeezed("function syncVisualViewport("));
        assert!(
            handler.contains(&squeezed("const viewport = window.visualViewport;"))
                && handler.contains(&squeezed("view.style.height = viewport.height + 'px';"))
                && handler.contains(&squeezed("view.style.top = viewport.offsetTop + 'px';")),
            "the session view takes the visible height and offset, so the composer is not left where the keyboard is drawn"
        );
        assert!(
            handler.contains(&squeezed("syncStick();")),
            "and the keyboard's resize re-reads the follow flag: no scroll event arrives with it, which is the staleness #19 named"
        );
        assert!(
            pane.contains(&squeezed(
                "window.visualViewport.addEventListener('scroll', scheduleVisualViewportSync);"
            )),
            "and the scroll as well as the resize: iOS moves the page under the keyboard, which arrives as a scroll"
        );
        // Every rule that writes a bottom row's padding, the narrow-screen ones
        // included: a shorthand in a media query is what dropped the inset where
        // it matters most, which the browser harness caught on the composer and
        // this loop then found a rule later on the panel's transcript.
        for rule in [
            ".input-row {",
            "#child-transcript {",
            ".sheet-actions {",
            "#skills-list {",
            "#mcp-list {",
        ] {
            let mut rest = PANE;
            let mut read = 0;
            while let Some((_, after)) = rest.split_once(rule) {
                let Some((body, tail)) = after.split_once('}') else {
                    break;
                };
                rest = tail;
                read += 1;
                assert!(
                    body.contains("env(safe-area-inset-bottom"),
                    "`{rule}` must keep clear of the Home indicator in every rule that writes its padding"
                );
            }
            assert!(read > 0, "`{rule}` must be styled at all");
        }
        assert!(
            segment(PANE, "#view-sheet {", "}").contains("env(safe-area-inset-bottom"),
            "the sheet that anchors to the bottom keeps clear of the Home indicator; the desktop variant is a side sheet and needs no inset"
        );
        assert!(
            PANE.contains(
                "bottom: calc(20px + env(safe-area-inset-bottom, 0px) + var(--keyboard-inset, 0px));"
            ),
            "and the toast sits above the indicator rather than in it, and above the keyboard: it is fixed to the layout viewport's bottom, which is where the keyboard is drawn"
        );
        assert!(
            PANE.contains(
                "const covered = Math.max(0, window.innerHeight - viewport.height - viewport.offsetTop);"
            ) && PANE.contains(
                "document.documentElement.style.setProperty('--keyboard-inset', covered + 'px');"
            ),
            "which is the strip the visual viewport leaves uncovered, written once for every element fixed to that bottom"
        );
        assert!(
            PANE.contains("if (window.visualViewport) {")
                && PANE.contains(
                    "window.visualViewport.addEventListener('resize', scheduleVisualViewportSync);"
                ),
            "and the listeners attach only where a visual viewport exists: an old browser would throw on `window.visualViewport.addEventListener`, and the guard is what keeps the pane running there"
        );
        assert!(
            PANE.contains("let visualViewportFrame = null;")
                && PANE.contains("window.requestAnimationFrame(() => {"),
            "a resize arrives in bursts while the keyboard animates, so one sync per frame is the most this asks for"
        );
        assert!(
            PANE.contains("pointer-events: none;"),
            "a toast reports and never takes a tap: it used to cover the composer and swallow taps there"
        );
    }

    // The fork control: a session's conversation copies into a new session from
    // the actions sheet, and the pane opens the copy.

    #[test]
    fn the_pane_forks_from_the_actions_sheet_and_opens_the_fork() {
        assert!(
            PANE.contains(
                "<div class=\"view-sheet-row\" id=\"row-fork\">\n    <button type=\"button\" id=\"btn-fork\">Fork session</button>"
            ),
            "the fork control lives in the session actions sheet, with its own row"
        );
        let handler = flattened();
        assert!(
            handler.contains(&squeezed("const started = current;"))
                && handler.contains(&squeezed("encodeURIComponent(started) + '/fork'")),
            "the control posts to the session it was clicked in, not to whatever the pane shows when the clone answers"
        );
        assert!(
            handler.contains(&squeezed("btnFork.disabled = true;")),
            "the control is disabled while the clone runs, so one click makes one fork"
        );
        assert!(
            handler.contains(&squeezed("} finally { btnFork.disabled = false; }")),
            "and it comes back whether the fork was made or refused"
        );
        assert!(
            handler.contains(&squeezed("openSession(fork.id);"))
                && PANE.contains("if (current !== started) return;"),
            "the pane opens the fork only if the reader is still in the session it was forked from"
        );
        assert!(
            handler.contains(&squeezed("openSession(fork.id);")),
            "and the pane opens the fork, which is a session like any other"
        );
        assert!(
            PANE.contains(
                "if (current === started) viewFork.textContent = 'fork: ' + error.message;"
            ),
            "a refusal is shown where the control is, in the sheet, and only while the reader is still in that session"
        );
        assert!(
            PANE.contains("if (current !== started) return;"),
            "and the sheet closes over a session the reader has left only after they are back in it"
        );
        for token in ["rowFork.hidden = false;", "rowFork.hidden = watchOnly;"] {
            assert!(
                handler.contains(&squeezed(token)),
                "{token} keeps the control off a watch-only child, like the sheet's other controls"
            );
        }
    }

    // The panel's list rows are the panel's: their rules are scoped to it, so a
    // session-list row keeps the height it had.

    #[test]
    fn the_pane_scopes_the_child_rows_to_the_panel_list() {
        assert!(
            PANE.contains("#child-list .child-row {"),
            "the row's rule names the list it belongs to"
        );
        for rule in [
            "\n  .child-row {",
            "\n  .child-row:active {",
            "\n  .child-row.followed {",
            "\n  .child-row .child-row-name {",
            "\n  .child-row .child-row-id {",
            "\n  .child-list-note {",
        ] {
            assert!(
                !PANE.contains(rule),
                "the row rules must not be global: `{rule}` styled every row in the pane, including the session list's, whose minimum height was a narrow-screen rule"
            );
        }
    }

    // A child has a name: its own summary once its model wrote one, otherwise
    // the first line of the instructions it was spawned with.

    #[test]
    fn the_pane_names_a_child_by_its_summary_then_its_instructions() {
        let pane = flattened();
        let naming = block(&pane, &squeezed("function childName("));
        assert!(
            naming.contains(&squeezed(
                "if (child.summary) return clip(child.summary, CHILD_NAME_MAX);"
            )),
            "the child's own one-line summary comes first, cut to a row"
        );
        assert!(
            naming.contains(&squeezed("(child.prompt || '')"))
                && naming.contains(&squeezed(".map((line) => line.trim())"))
                && naming.contains(&squeezed(".find((line) => line);")),
            "and without one, the first line of the instructions that is not blank"
        );
        assert!(
            naming.contains(&squeezed(
                "return first ? clip(first, CHILD_NAME_MAX) : child.id;"
            )),
            "with the id as the last resort"
        );
        assert!(
            PANE.contains("function updateChildPanelDot(")
                && PANE.contains("if (child) {")
                && PANE.contains("childPanelTitle.textContent = childName(child);")
                && PANE.contains("childPanelTitle.title = child.id;"),
            "the panel's header shows the name and keeps the id in its tooltip, and a child the list has dropped leaves the last name standing"
        );
        let line = block(&pane, &squeezed("case 'child_event':"));
        assert!(
            line.contains(&squeezed(
                "link.textContent = childName(child) || block.child_id;"
            )) && PANE.contains(
                "link.title = block.child_id + ' (opens the child as the session view)';"
            ),
            "the child's line leads with the name and keeps the id in its tooltip"
        );
    }

    // The panel lists the open session's children, and every block that names a
    // child carries the control that follows it.

    #[test]
    fn the_pane_lists_the_children_and_watches_them_wherever_they_are_named() {
        let pane = flattened();
        let list = block(&pane, &squeezed("function renderChildList("));
        assert!(
            list.contains(&squeezed(
                "sessions.filter((session) => session.parent_id === current)"
            )),
            "the list is the open session's direct children, from the poll the pane already runs"
        );
        let body = segment(
            PANE,
            "function renderChildList(",
            "\nfunction updateChildPanelDot(",
        );
        assert!(
            body.contains("dot.className = 'dot ' + child.state;")
                && list.contains(&squeezed("childList.appendChild(row);"))
                && list.contains(&squeezed("name.textContent = childName(child);")),
            "each row the list appends carries the child's state and its name"
        );
        assert!(
            list.contains(&squeezed("const children = current ?"))
                && list.contains(&squeezed(": [];")),
            "and no session open means no children: every root would match a null parent"
        );
        assert!(
            PANE.contains("'child-row' + (child.id === childFollow ? ' followed' : '')"),
            "the followed child is marked"
        );
        assert!(
            list.contains(&squeezed("id.textContent = child.id.slice(0, 8);")),
            "and the row keeps the id in its detail column"
        );
        assert!(
            list.contains(&squeezed(
                "row.addEventListener('click', () => followChild(child.id));"
            )),
            "a row follows its child"
        );
        assert!(
            pane.matches(&squeezed("renderChildList();")).count() >= 3,
            "the list is refreshed when a child is followed, when the panel closes, and by the sessions poll"
        );

        // One control, in every place a child is named: the child's line, the
        // ask row, a `spawn` result and a `message_child` result.
        let control = block(&pane, &squeezed("function watchControl("));
        assert!(
            control.contains(&squeezed("watch.className = 'child-watch';"))
                && control.contains(&squeezed("followChild(childId)")),
            "the control is built once and follows the child it names"
        );
        assert!(
            pane.matches(&squeezed("watchControl(")).count() >= 4,
            "and it is used wherever a child is named"
        );
        let named = block(&pane, &squeezed("function namedChild("));
        assert!(
            PANE.contains("if (name === 'spawn') {")
                && PANE.contains(
                    "const made = content && typeof content === 'object' ? content.child_id : undefined;"
                )
                && PANE.contains("return typeof made === 'string' && made ? made : undefined;"),
            "a `spawn` result names its child from its content, which is an object before anything flattens it, and only a string counts"
        );
        assert!(
            named.contains(&squeezed(
                "if (name !== 'message_child' || !id) return undefined;"
            )) && named.contains(&squeezed("if (error) return undefined;")),
            "and only a `message_child` that succeeded is asked for the child its call named"
        );
        assert!(
            PANE.contains(
                "return args && typeof args.id === 'string' && args.id ? args.id : undefined;"
            ),
            "and only a string argument counts: an `id` of another shape names no child"
        );
        let result = block(&pane, &squeezed("function appendToolResult("));
        assert!(
            result.contains(&squeezed(
                "const named = namedChild(content, name, error, id);"
            )) && result.contains(&squeezed(
                "if (id && name === 'message_child') callArgs.delete(id);"
            )),
            "the result asks it about the content it was given, and drops the call's arguments once it has them"
        );
        assert!(
            block(&pane, &squeezed("case 'tool_result':")).contains(&squeezed(
                "appendToolResult(block.name, payload, block.is_error, atMs, block.id, content);"
            )),
            "the renderer hands it the raw content, not the flattened string the line shows"
        );
        let teardown = segment(PANE, "function closeSession() {", "\nfunction ");
        assert!(
            teardown.contains("callArgs.clear();"),
            "and the map belongs to the transcript: the session's teardown clears it, where the DOM it was filled for clears, and a collapse keeps it so an in-flight `message_child` still gets its control"
        );
        let strip = block(&pane, &squeezed("function appendToolStrip("));
        assert!(
            strip.contains(&squeezed(
                "if (id && name === 'message_child') callArgs.set(id, args);"
            )),
            "only the call whose result may need them keeps its arguments, so no other call's body is held for the life of the transcript"
        );
        let ask = block(&pane, &squeezed("function renderAskBox("));
        assert!(
            ask.contains(&squeezed(
                "if (block.child_id) box.appendChild(watchControl(block.child_id));"
            )),
            "a child's ask row carries it too"
        );
    }

    // The context-size note: the loop's own count of how full the context is,
    // drawn as a line of its own in both transcripts.

    #[test]
    fn the_pane_draws_the_context_size_note() {
        let case = segment(PANE, "case 'context_size':", "break;");
        assert!(
            case.contains("'context: ' + block.tokens + ' / ' + block.window")
                && case.contains("'%), compaction at ' + block.compact_at"),
            "the line states the tokens, the window, the percentage and the count that fires compaction"
        );
        assert!(
            case.contains("Math.floor((block.tokens * 100) / block.window)"),
            "and the percentage is the note's own warning, rounded the way the terminal rounds it"
        );
        assert!(
            case.contains("appendLine(\n        'context',"),
            "it is a line of its own, like a summary"
        );
        assert!(
            PANE.contains(":is(#transcript, #child-transcript) .context {"),
            "and it is styled for both transcripts, since a child's frames draw with the same renderers"
        );
    }

    // The subagent panel: a second reader of a child's own stream, beside the
    // session's transcript rather than in place of it.

    #[test]
    fn the_pane_holds_the_panel_beside_the_transcript_inside_the_session_view() {
        let view_at = PANE
            .find("<div id=\"session-view\"")
            .expect("the pane must have the session view");
        let row_at = PANE
            .find("<div id=\"conversation\"")
            .expect("the pane must have the row that holds the transcript and the panel");
        let wrap_at = PANE
            .find("<div id=\"transcript-wrap\"")
            .expect("the pane must have the transcript's box");
        let panel_at = PANE
            .find("<div id=\"child-panel\"")
            .expect("the pane must have the panel");
        let banner_at = PANE
            .find("<div id=\"watch-banner\"")
            .expect("the pane must have the watch banner");
        assert!(
            view_at < row_at && row_at < wrap_at && wrap_at < panel_at && panel_at < banner_at,
            "the panel stands beside the transcript inside the session view, so the view hides it and the composer stays below it"
        );
        assert!(
            PANE.contains("<div id=\"child-panel\" hidden>"),
            "the panel is collapsed until a child is followed"
        );
        let row = squeezed(segment(PANE, "#conversation {", "}"));
        for declaration in [
            "flex: 1;",
            "min-height: 0;",
            "display: flex;",
            "flex-direction: row;",
        ] {
            assert!(
                row.contains(&squeezed(declaration)),
                "the row takes the transcript's place in the view and lays the two side by side: {declaration} keeps it doing so"
            );
        }
        assert_eq!(
            PANE.matches("id=\"child-panel\"").count(),
            1,
            "one element carries the panel's id"
        );
    }

    #[test]
    fn the_pane_styles_the_panel_transcript_with_the_session_rules() {
        let css = styles();
        assert!(
            css.contains(BLOCKS),
            "the pane's stylesheet must carry the shared prefix these checks read: the session's transcript and the panel's draw the same blocks"
        );
        // Every selector that names a transcript is either that container itself
        // or the shared prefix. A selector that named one container and then
        // something under it — a class or an element — would draw a block in one
        // transcript and not the other.
        // `#transcript-wrap` names its own box, not the transcript: a container
        // is named when the id stands entire, followed by no name character.
        let names = |selector: &str, container: &str| {
            selector.match_indices(container).any(|(at, _)| {
                !selector[at + container.len()..]
                    .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
        };
        let mut read = 0;
        for chunk in css.split('{') {
            let selector = chunk.rsplit('}').next().unwrap_or(chunk);
            let flat = squeezed(selector);
            for container in ["#transcript", "#child-transcript"] {
                if !names(selector, container) {
                    continue;
                }
                read += 1;
                assert!(
                    flat == container || flat.starts_with(&squeezed(BLOCKS)),
                    "the selector `{flat}` names `{container}`: a block rule names both transcripts through `{BLOCKS}`, and a container rule names its container alone"
                );
            }
        }
        assert!(
            read > 0,
            "the check must have read the rules that name a transcript"
        );
    }

    #[test]
    fn the_pane_follows_one_child_at_a_time_on_the_childs_own_stream() {
        let pane = flattened();
        assert_eq!(
            pane.matches(&squeezed("new EventSource(")).count(),
            2,
            "two streams exist: the session's own, and the panel's child"
        );
        let follow = block(&pane, &squeezed("function followChild("));
        let closes = follow
            .find(&squeezed("closeChildPanel();"))
            .expect("following a child must close whatever was followed before it");
        let opens = follow
            .find(&squeezed("childEs = new EventSource("))
            .expect("following a child must open its own stream");
        assert!(
            closes < opens,
            "the previous child's stream closes before the new one opens, so one child is followed at a time and no stream is left running"
        );
        assert!(
            follow.contains(&squeezed(
                "childEs = new EventSource('/sessions/' + encodeURIComponent(id) + '/events')"
            )),
            "the panel is driven by the child's own event stream, which reconnects with Last-Event-ID like the session's"
        );
        for token in [
            "openSession(",
            "pushSession(",
            "history.pushState",
            "replaceState",
        ] {
            assert!(
                !follow.contains(&squeezed(token)),
                "following a child in the panel must not call {token}: it is not opening it as the session view, and it adds no history entry"
            );
        }
        let collapse = block(&pane, &squeezed("function closeChildPanel("));
        assert!(
            collapse.contains(&squeezed("childEs.close();"))
                && collapse.contains(&squeezed("childFollow = null;"))
                && collapse.contains(&squeezed("childPanel.hidden = true;")),
            "collapsing closes the child's stream, forgets the child and hides the panel"
        );
        assert!(
            !collapse.contains(&squeezed("openSession("))
                && !collapse.contains(&squeezed("closeSession("))
                && !collapse.contains(&squeezed("transcript.textContent")),
            "collapsing changes the panel alone, leaving the session's view and transcript as they were"
        );
        let close = block(&pane, &squeezed("function closeSession("));
        assert!(
            close.contains(&squeezed("closeChildPanel();")),
            "leaving the session takes the panel, its child and its stream with it"
        );
    }

    #[test]
    fn the_pane_keeps_a_childs_frames_out_of_the_sessions_state() {
        let pane = flattened();
        let handler = block(&pane, &squeezed("function handleChildFrame("));
        assert!(
            handler.contains(&squeezed(
                "if (!frame.event || frame.event.kind !== 'message') return;"
            )),
            "the panel draws a child's durable messages and nothing else"
        );
        assert!(
            handler.contains(&squeezed("out = childTranscript;"))
                && handler.contains(&squeezed("out = previousOut;"))
                && handler.contains(&squeezed("openAskBox = previousAsk;")),
            "a child's frame draws into the panel and puts the session's render target and ask record back"
        );
        for token in [
            "handleFrame(",
            "handleEvent(",
            "renderMessage(",
            "updateStatusLabel(",
            "activityLog.",
            "scheduleAskSync(",
            "viewStateDot",
        ] {
            assert!(
                !handler.contains(&squeezed(token)),
                "a child's frame must not touch the session's view through {token}: the dot, the status label, the activity console and the ask composer describe the session on screen"
            );
        }
    }

    // `renderAskBox` keeps the record of the box it last drew, so the panel has
    // to draw with its own: with the session's record saved and put back around
    // a frame, the record a child's own question built is dropped, and the frame
    // that answers the question draws a second box beside it.

    #[test]
    fn the_pane_keeps_the_panels_ask_record_with_the_panel() {
        let pane = flattened();
        let handler = block(&pane, &squeezed("function handleChildFrame("));
        assert!(
            handler.contains(&squeezed(
                "out = childTranscript; openAskBox = childAskBox;"
            )),
            "a child's frame draws into the panel and with the panel's ask record, or the record the child's own question built is dropped and the frame that answers it draws a second box"
        );
        assert!(
            handler.contains(&squeezed(
                "childAskBox = openAskBox; openAskBox = previousAsk;"
            )),
            "the record the frame left is the panel's for the next frame, and the session's record goes back with the render target"
        );
        for (owner, why) in [
            (
                "function followChild(",
                "a child followed next starts with no ask record of its own",
            ),
            (
                "function closeChildPanel(",
                "a collapsed panel drops the record with the lines it drew",
            ),
        ] {
            let body = block(&pane, &squeezed(owner));
            assert!(
                body.contains(&squeezed("childAskBox = null;")),
                "`{owner}` must clear `childAskBox = null;`, so {why}"
            );
        }
    }

    #[test]
    fn the_pane_follows_a_child_from_its_line_and_shrinks_the_panel_to_a_sheet() {
        assert!(
            PANE.contains("line.appendChild(watchControl(block.child_id));"),
            "the child's line carries the control that follows it in the panel, built the one way every place that names a child builds it"
        );
        assert!(
            PANE.contains("link.addEventListener('click', () => openSession(block.child_id))"),
            "the child's id still opens it as the session view, as it did before the panel"
        );
        let side = squeezed(segment(PANE, "#child-panel {", "}"));
        assert!(
            side.contains(&squeezed("flex: 0 0 auto; width: 420px; max-width: 45%;"))
                && side.contains(&squeezed("border-left: 1px solid var(--border);")),
            "on a wide screen the panel is a column beside the transcript"
        );
        for out_of_flow in ["position: absolute", "position: fixed", "position: sticky"] {
            assert!(
                !side.contains(&squeezed(out_of_flow)),
                "the column stays in the row and takes width from the transcript, rather than covering it: {out_of_flow} would overlay instead"
            );
        }
        let sheet = squeezed(segment(PANE, "    #child-panel {", "}"));
        for declaration in [
            "position: fixed;",
            "inset: 0;",
            "width: auto;",
            "max-width: none;",
            "z-index: 11;",
        ] {
            assert!(
                sheet.contains(&squeezed(declaration)),
                "on a phone the column does not fit, so the panel covers the view as a full-height sheet: {declaration} keeps it covering the view and keeps the header's controls above it"
            );
        }
        let pane = flattened();
        assert!(
            pane.contains(&squeezed("childTranscript.addEventListener('scroll'"))
                && pane.contains(&squeezed("childStick =")),
            "the panel's transcript carries its own follow flag, set by its own scroll"
        );
        assert!(
            pane.contains(&squeezed(
                "if (out === childTranscript) { if (childStick) childTranscript.scrollTop = childTranscript.scrollHeight; return; }"
            )),
            "a render follows the transcript it drew into, so a child's frame scrolls the panel and never the session"
        );
    }
}
