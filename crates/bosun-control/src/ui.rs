use axum::Json;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Deserialize;
use tracing::info;
use tracing::instrument;

/// The web pane: a page listing nodes and sessions, with a live session view
/// driven by the session API and the SSE event stream. The page, its
/// stylesheet and its modules are data, embedded at compile time; no build
/// step serves them.
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

const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

/// The pane's stylesheet and modules: the name each is served at below `/ui/`,
/// its content type, and its body. A name outside this table is not served, so
/// a request cannot reach any other file.
pub(crate) const ASSETS: &[(&str, &str, &str)] = &[
    ("pane.css", CSS, include_str!("ui/pane.css")),
    ("dom.js", JS, include_str!("ui/dom.js")),
    ("common.js", JS, include_str!("ui/common.js")),
    ("machines.js", JS, include_str!("ui/machines.js")),
    ("skills.js", JS, include_str!("ui/skills.js")),
    ("mcp.js", JS, include_str!("ui/mcp.js")),
    ("new-session.js", JS, include_str!("ui/new-session.js")),
    ("activity.js", JS, include_str!("ui/activity.js")),
    ("session-list.js", JS, include_str!("ui/session-list.js")),
    ("history.js", JS, include_str!("ui/history.js")),
    ("session-view.js", JS, include_str!("ui/session-view.js")),
    ("composer.js", JS, include_str!("ui/composer.js")),
    ("scroll.js", JS, include_str!("ui/scroll.js")),
    ("screens.js", JS, include_str!("ui/screens.js")),
    ("viewport.js", JS, include_str!("ui/viewport.js")),
    (
        "viewport-report.js",
        JS,
        include_str!("ui/viewport-report.js"),
    ),
    ("transcript.js", JS, include_str!("ui/transcript.js")),
    ("diagram.js", JS, include_str!("ui/diagram.js")),
    ("markdown.js", JS, include_str!("ui/markdown.js")),
    ("subagents.js", JS, include_str!("ui/subagents.js")),
    ("earlier.js", JS, include_str!("ui/earlier.js")),
    ("main.js", JS, include_str!("ui/main.js")),
];

/// One of the pane's `ASSETS`. It is served `no-store` like the page: the
/// modules import each other by fixed names, so a browser that kept one module
/// from an older build would run it against the newer modules beside it.
pub async fn asset(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|(served, _, _)| *served == name) {
        Some((_, content_type, body)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                (header::CACHE_CONTROL, "no-store"),
            ],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The mermaid bundle the pane renders diagram fences with: mermaid 12.0.0,
/// vendored from the npm tarball and never edited. Embedded as data like the
/// pane, so the control plane needs no build step.
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

/// The largest viewport report body the control plane reads, in bytes. The
/// pane sends at most 16 samples a batch, about 500 bytes each.
pub(crate) const VIEWPORT_REPORT_MAX_BYTES: usize = 16 * 1024;

/// A batch of viewport samples from one page load, sent when the page is
/// loaded with `?viewport-report`.
#[derive(Debug, Deserialize)]
pub struct ViewportReport {
    /// Tells one page load's samples from another's.
    page: String,
    /// Samples the pane dropped because its queue was full.
    #[serde(default)]
    dropped: u32,
    /// Samples the pane could not take because a number could not be read.
    #[serde(default)]
    failed: u32,
    samples: Vec<ViewportSample>,
}

/// What the pane saw of the viewport as one event arrived, before the pane
/// answered it. Sizes and places are CSS pixels.
#[derive(Debug, Deserialize)]
struct ViewportSample {
    /// The page's `performance.now()` when the event arrived, in ms.
    at: f64,
    /// The event that was sampled.
    event: String,
    inner_width: f64,
    inner_height: f64,
    /// Absent in a browser with no visual viewport API.
    visual: Option<VisualViewport>,
    scroll_y: f64,
    scroll_height: f64,
    /// Absent while the element is not shown.
    view: Option<Edges>,
    composer: Option<Edges>,
    /// The focused element's tag and id, absent when nothing has the focus.
    focused: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct VisualViewport {
    width: Option<f64>,
    height: Option<f64>,
    offset_top: Option<f64>,
    page_top: Option<f64>,
    scale: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Edges {
    top: f64,
    bottom: f64,
    height: f64,
}

/// Logs a batch of viewport samples, one line each. The keyboard on a phone
/// cannot be reproduced off the phone, so this log is how its numbers are
/// read.
#[instrument(skip_all)]
pub async fn viewport_report(headers: HeaderMap, Json(report): Json<ViewportReport>) -> StatusCode {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    if report.dropped > 0 || report.failed > 0 {
        info!(
            page = ?report.page,
            dropped = report.dropped,
            failed = report.failed,
            "viewport samples lost"
        );
    }
    for sample in report.samples {
        let visual = sample.visual.unwrap_or_default();
        let view = sample.view.as_ref();
        let composer = sample.composer.as_ref();
        info!(
            page = ?report.page,
            at = sample.at,
            event = ?sample.event,
            inner_width = sample.inner_width,
            inner_height = sample.inner_height,
            visual_width = visual.width,
            visual_height = visual.height,
            visual_offset_top = visual.offset_top,
            visual_page_top = visual.page_top,
            visual_scale = visual.scale,
            scroll_y = sample.scroll_y,
            scroll_height = sample.scroll_height,
            view_top = view.map(|edges| edges.top),
            view_bottom = view.map(|edges| edges.bottom),
            view_height = view.map(|edges| edges.height),
            composer_top = composer.map(|edges| edges.top),
            composer_bottom = composer.map(|edges| edges.bottom),
            composer_height = composer.map(|edges| edges.height),
            focused = ?sample.focused,
            user_agent = ?user_agent,
            "viewport report"
        );
    }
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    /// The pane's sources as one text: the markup, then the stylesheet in a
    /// `<style>` block, then the modules in one `<script>` block in `ASSETS`
    /// order, which is the shape `code()` and `styles()` read.
    const PANE: &str = concat!(
        include_str!("ui/index.html"),
        "<style>\n",
        include_str!("ui/pane.css"),
        "</style>\n<script>\n",
        include_str!("ui/dom.js"),
        include_str!("ui/common.js"),
        include_str!("ui/machines.js"),
        include_str!("ui/skills.js"),
        include_str!("ui/mcp.js"),
        include_str!("ui/new-session.js"),
        include_str!("ui/activity.js"),
        include_str!("ui/session-list.js"),
        include_str!("ui/history.js"),
        include_str!("ui/session-view.js"),
        include_str!("ui/composer.js"),
        include_str!("ui/scroll.js"),
        include_str!("ui/screens.js"),
        include_str!("ui/viewport.js"),
        include_str!("ui/viewport-report.js"),
        include_str!("ui/transcript.js"),
        include_str!("ui/diagram.js"),
        include_str!("ui/markdown.js"),
        include_str!("ui/subagents.js"),
        include_str!("ui/earlier.js"),
        include_str!("ui/main.js"),
        "\n</script>\n",
    );

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

    /// A writer the log subscriber shares with the test that reads it.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_viewport_sample_is_logged_at_info_with_its_numbers_as_fields() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let report: super::ViewportReport = serde_json::from_value(serde_json::json!({
            "page": "k3j9x2a1",
            "dropped": 2,
            "samples": [{
            "at": 1234.5,
            "event": "visual resize",
            "inner_width": 390,
            "inner_height": 844,
            "visual": { "width": 390, "height": 500, "offset_top": 12, "page_top": 12, "scale": 1 },
            "scroll_y": 0,
            "scroll_height": 844,
            "view": { "top": 0, "bottom": 500, "height": 500 },
            "composer": null,
            "focused": "textarea#input",
            }],
        }))
        .unwrap();
        let status = super::viewport_report(axum::http::HeaderMap::new(), axum::Json(report)).await;
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains(" INFO "), "{log}");
        assert!(log.contains("dropped=2"), "lost samples are counted: {log}");
        for field in [
            "page=\"k3j9x2a1\"",
            "at=1234.5",
            "event=\"visual resize\"",
            "inner_height=844.0",
            "visual_height=500.0",
            "visual_offset_top=12.0",
            "view_bottom=500.0",
            "focused=Some(\"textarea#input\")",
        ] {
            assert!(
                log.contains(field),
                "the log line must carry {field}: {log}"
            );
        }
        assert!(
            !log.contains("composer_top"),
            "a composer not shown carries no numbers: {log}"
        );
    }

    #[test]
    fn every_stylesheet_and_module_in_the_ui_directory_is_served_and_checked() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| {
                (name.ends_with(".js") || name.ends_with(".css")) && name != "mermaid.min.js"
            })
            .collect();
        on_disk.sort();
        let mut served: Vec<String> = super::ASSETS
            .iter()
            .map(|(name, _, _)| name.to_string())
            .collect();
        served.sort();
        assert_eq!(
            served, on_disk,
            "a file the pane loads must be in `ASSETS`, or the browser gets a 404 for it"
        );
        for (name, _, body) in super::ASSETS {
            assert!(
                PANE.contains(body),
                "`{name}` must be in `PANE`, or the source checks do not read it"
            );
        }
    }

    // The tests below read the pane's source text. They hold what a browser
    // check cannot see well: the files the page loads and their order, that no
    // text reaches the DOM as HTML, the link schemes the markdown follows,
    // mermaid's settings, and the insets that keep the bottom rows clear of a
    // phone's Home indicator, which a desktop browser reports as zero. The
    // pane's behaviour is checked in a browser: `tests/browser.rs`.

    /// The pane's source from `start` to the next `end`, so a check reads one
    /// function or one rule instead of the whole pane.
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

    // The browser learns a module's imports only once it has that module, so
    // without these links it fetches the import graph one level at a time.

    #[test]
    fn the_page_preloads_every_module_the_entry_module_imports() {
        for (name, _, _) in super::ASSETS {
            if !name.ends_with(".js") || *name == "main.js" {
                continue;
            }
            assert!(
                PANE.contains(&format!("<link rel=\"modulepreload\" href=\"/ui/{name}\">")),
                "the page must preload `{name}`, or the browser waits for the module that imports it"
            );
        }
        for link in PANE
            .split("<link rel=\"modulepreload\" href=\"/ui/")
            .skip(1)
        {
            let name = link
                .split_once('"')
                .expect("a preload link closes its href")
                .0;
            assert!(
                super::ASSETS.iter().any(|(served, _, _)| *served == name),
                "the page preloads `{name}`, which `ASSETS` does not serve"
            );
        }
    }

    // The diagram module waits for the bundle's `load` event. Deferred scripts
    // and module scripts run in document order, so a module placed after the
    // bundle would listen once the event had fired, and every diagram would
    // wait forever.

    #[test]
    fn the_pane_runs_its_modules_before_the_mermaid_bundle() {
        let module = PANE
            .find("<script type=\"module\" src=\"/ui/main.js\"></script>")
            .expect("the page loads its entry module by absolute path");
        let bundle = PANE
            .find("<script src=\"/ui/mermaid.min.js\"")
            .expect("the page loads the mermaid bundle");
        assert!(
            module < bundle,
            "the entry module must come before the bundle, or the diagram module misses its load event"
        );
    }

    #[test]
    fn the_pane_renders_a_mermaid_fence_through_the_xml_parser() {
        let diagram = squeezed(segment(PANE, "async function fillMermaid(", "\n}"));
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
                && markdown.contains(&squeezed("nodes.appendChild(renderMermaid(source))"))
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

    /// The content of the page's viewport tag.
    fn viewport_meta() -> &'static str {
        segment(PANE, "<meta name=\"viewport\" content=\"", "\"")
    }

    // Chromium on Android shrinks the layout for the keyboard only when the
    // viewport tag asks for it; the browser checks cannot raise a keyboard.

    #[test]
    fn the_page_asks_android_to_resize_its_layout_for_the_keyboard() {
        assert!(
            viewport_meta().contains("interactive-widget=resizes-content"),
            "without interactive-widget=resizes-content Android Chrome draws the keyboard over the composer"
        );
    }

    // The page reaches under the Home indicator, and every row that sits at the
    // bottom of the screen keeps its padding clear of it. `env()` insets are
    // zero in a desktop browser, so this reads the stylesheet.

    #[test]
    fn the_pane_keeps_its_bottom_rows_clear_of_the_home_indicator() {
        assert!(
            viewport_meta().contains("viewport-fit=cover"),
            "the page has to reach under the Home indicator before an inset can hold anything clear of it"
        );
        // Every rule whose selector names one of these rows, wherever it sits and
        // however it is written: the walk reads the stylesheet's own rules, so a
        // grouped selector, a class-qualified one, and a rule inside a media
        // query are all read. A shorthand in a media query is what dropped the
        // inset where it matters most.
        let css = styles();
        let mut rest = css.as_str();
        let mut read = 0;
        while let Some((before, after)) = rest.split_once('{') {
            // The raw selector keeps the descendant spaces that tell a container
            // from a row inside it; the squeezed form is only for the message.
            let raw = before.rsplit('}').next().unwrap_or(before);
            let selector = squeezed(raw);
            let Some((body, tail)) = after.split_once('}') else {
                break;
            };
            rest = tail;
            // Each comma-separated group's subject, its last compound, is what
            // the rule styles. `#skills-list .repo-row` is a row inside the
            // container, not the container, and the container's own padding is
            // what keeps the last of them clear.
            let names_row = raw.split(',').any(|group| {
                let subject = group.split_whitespace().last().unwrap_or_default();
                [
                    ".input-row",
                    "#child-transcript",
                    ".sheet-actions",
                    "#skills-list",
                    "#mcp-list",
                    "#machines-list",
                ]
                .iter()
                .any(|row| subject.contains(row))
            });
            if !names_row || !body.contains("padding") {
                continue;
            }
            read += 1;
            assert!(
                body.contains("env(safe-area-inset-bottom"),
                "`{selector}` writes a bottom row's padding without the Home indicator's inset"
            );
        }
        assert!(
            read >= 7,
            "the walk must read every rule that pads one of the six rows, the narrow-screen ones included: read {read}"
        );
        assert!(
            segment(PANE, "#view-sheet {", "}").contains("env(safe-area-inset-bottom"),
            "the sheet that anchors to the bottom keeps clear of the Home indicator"
        );
        assert!(
            segment(PANE, "#toast {", "}").contains("env(safe-area-inset-bottom, 0px)"),
            "the toast sits above the Home indicator rather than in it"
        );
    }

    // The clear control is a row of the actions sheet. What the row does when
    // it is pressed is checked in a browser (`tests/browser.rs`); this reads the
    // source a browser check cannot: the markup row, the URL the handler posts
    // to, and the watch-only token that keeps the row off a child.

    #[test]
    fn the_pane_clears_the_context_from_the_actions_sheet() {
        assert!(
            PANE.contains(
                "<div class=\"view-sheet-row\" id=\"row-clear\">\n    <button type=\"button\" id=\"btn-clear\">Clear context</button>\n    <span class=\"view-sheet-note\" id=\"view-clear\"></span>"
            ),
            "the clear control lives in the session actions sheet, with its own row and its own note"
        );
        let handler = squeezed(segment(PANE, "btnClear.addEventListener('click'", "\n});"));
        assert!(
            handler.contains(&squeezed("encodeURIComponent(started.id) + '/clear'")),
            "the control posts to the clear route for the session it was clicked in"
        );
        assert!(
            PANE.contains("rowClear.hidden = watchOnly;"),
            "the row is hidden for a watch-only child, like the sheet's other controls"
        );
    }
}
