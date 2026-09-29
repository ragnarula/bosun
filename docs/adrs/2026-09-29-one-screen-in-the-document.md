# ADR: The pane shows one screen at a time in the document, sized by the browser, with a small keyboard rule for iOS Safari

**Date:** 2026-09-29
**Author:** Raghav

## Context

The web pane (`crates/bosun-control/src/ui/`) has five screens: the home column with the session list (`#home`), an open session (`#session-view`), and the machines, skills and MCP tabs (`#machines-tab`, `#skills-tab`, `#mcp-tab`). It also has sheets: the new-session sheet, the session's ⋯ sheet (`#view-sheet`), the ask sheet inside the composer, and the child panel, which covers the session on a phone.

The session screen is a column: header, transcript, composer. The composer is its last row. On a phone the keyboard must not cover the composer, and the header and the transcript must stay on screen while the reader types.

The browsers handle the keyboard differently:

- Android Chrome can shrink the layout for the keyboard. The viewport tag's `interactive-widget=resizes-content` asks for that, and `100dvh` then shrinks with it.
- iOS Safari ignores `interactive-widget`. The layout viewport and `100dvh` keep their height, the keyboard is drawn over the bottom of the page, and Safari scrolls the page to bring the focused field into view. `visualViewport` reports the part above the keyboard: its `height`, and its `offsetTop` inside the layout viewport. The page can also be scrolled by the keyboard's height, which shows as `window.scrollY`, even when the document fits the screen.

No iOS device or WebKit build is available where the pane is developed and tested. The browser checks in `crates/bosun-control/tests/browser/pane.py` run in Chromium with a stubbed visual viewport, so they check the pane against a model of Safari, not against Safari.

## Decision Drivers

- The composer stays just above the keyboard, and the header and the transcript stay on screen, on iOS Safari and on Android Chrome.
- The browser does as much of the sizing as it can. The script that remains is small enough to state the Safari behaviour it depends on.
- Nothing from another screen can show in a gap the keyboard leaves.
- A phone's real numbers can be read without a device at the developer's desk.
- History behaviour does not change (see the pane history ADR).

## Options Considered

**1. A fixed-height shell with the screens as overlays, and a view moved with the visual viewport. (rejected)**

The session view is an overlay (`position: absolute; inset: 0`) over the home column, and the tabs are fixed overlays. While a field has the focus, the view is sized to `visualViewport.height` and placed at `offsetTop + scrollY`. This depends on Safari's numbers being correct at every frame of the keyboard's movement. It has to tell a real push from a stale value that Safari leaves behind, and it has to guess which of the two ways Safari used to reveal the field. A wrong guess shows the home column in the gap, or moves the transcript off the top. The view was also an overlay over another screen, so any wrong geometry showed that screen.

**2. One screen in the document, sized by `100dvh` and `interactive-widget`, with a small iOS rule. (chosen)**

Android needs no script. On iOS, the script sets one number, the body's height, and returns the document to its top. It does not place anything. No other screen is in the page, so a wrong size shows the body's own background and not another screen.

**3. Let the document scroll, with the composer `position: sticky` at the bottom. (rejected)**

The browser would then scroll the page for the keyboard itself. But the transcript's follow flag, the bottom control, and the read-back of older pages all read and set the transcript's own scroll box. They would all have to move to the document's scroll. The header would scroll away with the transcript. And on iOS a sticky or fixed bottom row sits at the layout viewport's bottom, which is under the keyboard, so the composer would still be covered.

**4. `100dvh` and `interactive-widget` only, with no script. (rejected)**

This fixes Android. On iOS the body keeps the full height, the composer sits under the keyboard, and Safari's reveal scroll moves the header and the top of the transcript off the screen. That is the fault this decision is for.

**5. The VirtualKeyboard API (`navigator.virtualKeyboard.overlaysContent` and `env(keyboard-inset-height)`). (rejected)**

Only Chromium browsers support it. Safari does not.

## Decision

**Screens.** `#home`, `#session-view`, `#machines-tab`, `#skills-tab` and `#mcp-tab` are children of `body`. `showScreen` in `screens.js` shows one and hides the others with the `hidden` attribute, so exactly one screen is in the page at a time. The shown screen is in the body's normal flow (`position: static`), a flex item with `flex: 1; min-height: 0`, and it scrolls inside itself: the home column in its `main`, a tab in its list, the session in its transcript. The composer (`#input-row`) is the session screen's last row. `showScreen` keeps the session list's scroll while another screen shows, because WebKit drops the scroll of a box that leaves the layout. A session opened while a tab shows (by the browser's forward button) hides the tab.

**Sheets.** The new-session sheet, the ⋯ sheet, the phone child panel and the toast are `position: absolute` in the body, which is `position: relative`. They follow the body's size, and they do not depend on the screen under them. At 900 px and wider, the ⋯ sheet is a fixed popup under its button, as before.

**Size.** `body` is `height: 100vh; height: 100dvh` with `overflow: hidden`, and `html` has `overflow: hidden`. The viewport tag is `width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content`. The safe-area insets on the bottom rows are unchanged.

**The iOS rule** is `syncVisualViewport` in `viewport.js`:

- While a text field (`input`, `textarea`, `select`) has the focus, the body's height is `visualViewport.height`. A report that is empty, or taller than `innerHeight`, is not a keyboard, and the body keeps the height it had.
- The document is always kept at its top: when `scrollY` or `visualViewport.offsetTop` is not zero, the script calls `window.scrollTo(0, 0)`.
- With no field focused, the body's inline height is cleared, and the stylesheet's `100dvh` applies again. Leaving a field clears it at once. Moving the focus from one field to another does not clear it.
- A zoomed page (`visualViewport.scale` not 1) is left alone: the body keeps the stylesheet's height, and the script does not scroll.
- The sync runs once per frame on `visualViewport` resize and scroll, window resize and scroll, and focus changes. It runs once more 300 ms after the last event.

The rule relies on these Safari behaviours:

1. While the keyboard is up, `visualViewport.height` is the height of the page above the keyboard and its accessory bar, and Safari sends `visualViewport` resize events while it changes.
2. Safari's reveal scroll shows as `window.scrollY`, `visualViewport.offsetTop`, or both. `window.scrollTo(0, 0)` moves the visible part back to the top of the document and sets both to zero.
3. Once the body fits the visible part, the focused field is inside it, so Safari makes no further reveal scroll. If Safari scrolls again, the scroll event runs the rule again.
4. A tap on a field gives it the focus and raises the keyboard, and the keyboard closes when the field loses the focus.

The rule does not handle:

- A visual viewport offset that `scrollTo(0, 0)` does not clear. The body would then sit above the visible part by that offset.
- A pinch-zoomed page while a field has the focus.
- A floating or split keyboard on an iPad, which covers the middle of the page, not its bottom.
- A keyboard raised with no text field focused. The pane has no other editable element.
- The frame between Safari's reveal scroll and the script's undo, which can show as a short jump.
- A browser without `dvh` (iOS Safari before 15.4). `100vh` there includes the area under Safari's toolbar.

**The viewport report.** When the address carries `?viewport-report`, for example `https://host/ui?viewport-report#s=<session id>`, `viewport-report.js` takes a sample of the viewport numbers as each event arrives: page load, `visualViewport` resize and scroll, window resize and scroll, `focusin` and `focusout`, and one more 500 ms after the last event, named `settled after <event>`. `main.js` imports it before `viewport.js`, and its listeners use the capture phase, so a sample reads the numbers before the iOS rule answers the event: a reveal scroll that the rule undoes still shows in `scroll_y`. A sample holds `at` (the page's `performance.now()`), `event`, `inner_width`, `inner_height`, `visual` (`width`, `height`, `offset_top`, `page_top`, `scale`), `scroll_y`, `scroll_height` (the document's), `view` and `composer` (`top`, `bottom`, `height` of `#session-view` and `#input`, or null when not shown), and `focused` (the focused element's tag and id). The samples are queued (at most 64, older ones dropped and counted) and sent to `POST /viewport-report` in batches of at most 16, at most one batch per animation frame and one per 250 ms. A batch carries a random id for the page load, and the counts of dropped samples and of samples that could not be taken. Without the query, the pane sends nothing.

`viewport_report` in `crates/bosun-control/src/ui.rs` reads a JSON body of at most `VIEWPORT_REPORT_MAX_BYTES` (16 KiB), logs each sample at info level with the message `viewport report` and one structured field per number (`page`, `at`, `inner_height`, `visual_height`, `visual_offset_top`, `scroll_y`, `view_bottom`, `composer_bottom` and the rest, plus `event`, `focused` and `user_agent`), logs lost samples as `viewport samples lost`, and returns 204. A larger body gets 413, and a body that is not a report gets a 4xx. `/viewport-report` is in `RESERVED_PATHS`, so the OAuth callback cannot be registered on it.

To read a phone: open the pane on the phone with `?viewport-report`, open a session, tap the composer, type, and leave the field. Then read the control plane's log lines with the message `viewport report`. Sort the lines of one `page` by `at`. The phone confirms the rule when, after each `settled after` sample with the composer focused, `scroll_y` and `visual_offset_top` are 0, `view_height` equals `visual_height`, and `composer_bottom` is at or just above `visual_height`, less the row's padding. With no field focused, `view_height` must equal `inner_height`.

## Consequences

- Android Chrome sizes the pane for the keyboard with no script.
- On iOS the script sets one height and one scroll. It no longer places the view, so there is no stale offset to tell apart from a real one.
- A wrong height on iOS shows the body's background in the gap, never another screen.
- The rule depends on the Safari behaviours listed above, and the browser checks cannot confirm them. The checks' viewport stub models behaviour 2: its `window.scrollTo` clears both the stub's scroll and its offset. The stub does not model a second reveal scroll after the undo, events that arrive only at the end of the keyboard's movement, or a scale other than 1. The zoom guard and the `100dvh` height are not covered by the browser checks: the stub's scale is always 1, and Chromium's `100dvh` equals `100vh`, so removing either passes. The viewport report is the only way to see what Safari does.
- A hidden screen leaves the layout. The session list's scroll is kept by `showScreen`. A tab's list is re-rendered when the tab opens, so its scroll starts at the top.
- A tab hidden because a session opened over it keeps its add form's state, because its own close path did not run.
- The report route is open to any client that can reach the control plane, like every other route in this single-user build. The body limit bounds each request, and each report is one log line.

## Revisit When

- The viewport report from a phone shows an offset or a document scroll that `scrollTo(0, 0)` does not clear, or a `visual_height` that does not match the space above the keyboard.
- Safari supports `interactive-widget=resizes-content` or the VirtualKeyboard API. The iOS rule can then be removed.
- The pane needs two screens side by side, for example on a tablet.
