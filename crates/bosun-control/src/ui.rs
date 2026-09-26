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

    /// `code()` with every run of whitespace removed and every double quote
    /// written as a single one, so a check on a name or a spelling reads the
    /// same text however the pane respaces a call or requotes a string.
    fn flattened() -> String {
        code()
            .split_whitespace()
            .collect::<String>()
            .replace('"', "'")
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
            squeezed("stick = true; transcript.scrollTop = transcript.scrollHeight;"),
            "jumpToBottom must set auto-scroll and move the transcript itself, not through the guarded follow: a keystroke from a scrolled-up reader would otherwise leave them where they were"
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
            squeezed("stick = true; transcript.scrollTop = transcript.scrollHeight;"),
            "jumpToBottom must set auto-scroll and move the transcript itself, not through the guarded follow, or the answer to the sent message does not land in view"
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
        assert_eq!(
            block(&pane, &squeezed("transcript.addEventListener('scroll'")),
            squeezed(
                "stick = transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 40;"
            ),
            "the listener must hold auto-scroll while the end of the transcript is in view: a listener that clears `stick` on every scroll takes back the jump a keystroke or a send just made, and the sent message and its answer land below the fold again"
        );
    }
}
