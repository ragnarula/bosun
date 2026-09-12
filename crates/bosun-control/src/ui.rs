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

#[cfg(test)]
mod tests {
    const PANE: &str = include_str!("ui/index.html");

    // The pane ships as one embedded HTML file with no browser test harness,
    // so these are presence checks, not behaviour tests.
    #[test]
    fn the_pane_routes_activity_frames_to_the_console() {
        assert!(
            PANE.contains("case 'activity':") && PANE.contains("activities.push(event)"),
            "the pane must store activity frames for the console"
        );
    }

    #[test]
    fn the_pane_has_a_model_call_line_handler() {
        assert!(
            PANE.contains("case 'model_call':")
                && PANE.contains("appendLine('mono', modelCallLine(event))")
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
}
