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
        let from_link = squeezed(segment(PANE, "function sessionFromLink(", "\n}\n"));
        assert!(
            from_link.contains(&squeezed("/^#s=(.+)$/.exec(location.hash)"))
                && from_link.contains(&squeezed("decodeURIComponent(match[1])")),
            "a load reads back the fragment the pane writes, decoded the way it was encoded"
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
                "Object.prototype.hasOwnProperty.call(state, 'pane')"
            )) && own.contains(&squeezed("&& state.pane === id")),
            "the key tells the pane's entries from another page's, and the id tells the pane's own entry for a session from one the browser put a copied state on, such as a fragment pasted into the address bar"
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
        let follow = squeezed(segment(PANE, "function followHistory(", "\n}\n"));
        for token in [
            "const id = sessionFromLink();",
            "if (id === current) return;",
            "closeSession();",
            "if (!isOwnEntry(history.state, id)) pushSession(id);",
            "showSession(id);",
        ] {
            assert!(
                follow.contains(&squeezed(token)),
                "the fragment decides the screen — the session it names, or the list — and an entry the pane did not write gets the list entry under the session first: {token} stays"
            );
        }
        assert!(
            PANE.contains("window.addEventListener('popstate', followHistory)"),
            "back and forward must run the pane's own history path"
        );
    }

    #[test]
    fn the_pane_starts_on_the_entry_the_address_bar_names() {
        let start = squeezed(segment(PANE, "function startFromLink(", "\n}\n"));
        for token in [
            "const id = sessionFromLink();",
            "if (isOwnEntry(history.state, id)) {",
            "if (!id) { markListEntry(); return; }",
            "pushSession(id);",
            "showSession(id);",
        ] {
            assert!(
                start.contains(&squeezed(token)),
                "a load starts on the session the fragment names, with the list entry written under it, or on the list: {token} stays"
            );
        }
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
        let stop = squeezed(segment(PANE, "btnStop.addEventListener('click'", "\n});"));
        assert!(
            stop.contains(&squeezed("markListEntry(); closeSession();")),
            "a session stopped here must leave no `#s=` link behind"
        );
        let refresh = squeezed(segment(PANE, "async function refreshSessions(", "\n}\n"));
        assert!(
            refresh.contains(&squeezed("showStatus('session ' + current + ' ended');"))
                && refresh.contains(&squeezed("markListEntry(); closeSession();")),
            "a session that ended on the node must leave no `#s=` link behind"
        );
        let detail = squeezed(segment(PANE, "async function fetchSession(", "\n}\n"));
        assert!(
            detail.contains(&squeezed("if (response.status === 404) {"))
                && detail.contains(&squeezed("markListEntry(); closeSession();")),
            "an entry naming a session the control plane does not have must stop naming it, so the session screen and a reload do not outlive the session"
        );
    }

    #[test]
    fn the_pane_ignores_a_reply_for_a_session_it_left() {
        let detail = squeezed(segment(PANE, "async function fetchSession(", "\n}\n"));
        assert_eq!(
            detail
                .matches(&squeezed("if (current !== id) return;"))
                .count(),
            2,
            "a reply that arrives after the pane left the session must write nothing: one check guards the header and the close, and the body read can outlive the session too"
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
            "viewPermission.textContent = ''",
            "personaName.value = ''",
        ] {
            assert!(
                header.contains(&squeezed(field)),
                "clearHeader must clear {field}, or the session the pane left keeps naming itself on screen"
            );
        }
    }
}
