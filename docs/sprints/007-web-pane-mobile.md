# Sprint 007 — Mobile-first web pane

The web pane is redesigned from the ground up for a phone, one-handed, and for the principles in `../developer/web-ui-principles.md`: low-friction input with no text entry outside the chat box, one-handed use, shortcuts for repetitive steps, and conciseness. The single self-contained page at `crates/bosun-control/src/ui/index.html` becomes a five-screen app and stays data-only, embedded at compile time with no build step.

Status: **planned**.

## Confirmed decisions

- **One page, five screens.** The pane has a bottom tab bar (Sessions / New / Machines) and a session screen. The sessions tab is the home screen; the new-session sheet slides up from the bottom; the machines and session-action surfaces are sheets and screens of the same page. `include_str!` keeps serving the single embedded `index.html`.
- **No text entry outside the chat box.** The create form's `#form-prompt` textarea is removed: the first instruction is typed into the session's chat box after Start, the one text surface the standard allows. `#form-dir` becomes a plain span, not an `<input type="text">`; the directory is chosen by browsing. Personas and nodes are choices, never typed. Ask options become tap-to-answer chips.
- **The composer persists until a session actually starts.** Close or cancel does not reset the picks. The recent-settings list (`bosun.recent-sessions` in localStorage) is re-validated against the live node list on open: a recent entry whose node is gone is offered against a live node instead of silently restoring a broken choice. The chat draft survives closing the session and returning.
- **One-tap recents.** A recent entry fills the composer; Start is one tap below it. The design notes this is two taps from home to a running session — deliberately not one, so a fat-finger on "recent" cannot launch a session the user did not want.
- **Transcript lines that restate the header are gone.** `handleEvent` stops printing `* <state>`, `* persona: <name>`, and `— <model> —` as transcript lines; the current state, persona, and permission live once each in the session header. Ask, message, tool, summary, and child-event history is unchanged.
- **Ask options are choices.** When an `ask` block with options is on screen, the input area offers the options as thumb-width chips that post the answer; a small "or type an answer" field remains available under the chips.
- **Header actions go behind a `⋯` sheet.** The session header shows back, state dot + node/dir, and `⋯`. The sheet holds permission toggle, persona switch, copy session id, interrupt, and a separated red Stop with a confirm. Watch-only children render as a full-width "Open to act" banner where the input row is, not as five disabled controls.
- **Machines is a tab, not a panel.** Node health shows as a one-line strip on the home screen (`2 up · 1 down`, tap to open the Machines tab) and as a dot on each session row's node name. The separate nodes panel on home is gone.
- **Touch-first CSS.** Every interactive element is ≥44px with `:active` feedback and `-webkit-tap-highlight-color: transparent`; form fonts are 16px so iOS does not zoom on focus; nothing scrolls sideways; nothing requires hover. The desktop layout keeps a full session screen without sheets, but the same sheet structure underlies it.

## User stories in implementation order

- [ ] **S1 — Home screen: sessions as a tappable list**

As a user, I want one list of my sessions where each row is a big tappable target, so I can open what I care about with my thumb.

- The nodes panel is removed from home; a one-line strip `2 up · 1 down` replaces it, tapping into the Machines tab.
- Each session row (≥44px, `:active` feedback) shows a state dot, `node / dir`, a muted persona tag, and a muted relative time. Running sessions come first, then recently finished.
- Child sessions render as an expandable tag on the parent row (`2 children`) that opens in place with one banner line per child, watch-only, with their state dots. Rows never indent deeper than one level.
- The full session id lives on the session screen; the home row shows the short id, and the row's hover `title` is removed so no information is touch-invisible.

- [ ] **S2 — New-session sheet: choices only**

As a user, I want to start a session without typing a node, path, or prompt, so the composer works one-handed.

- The New tab opens a bottom sheet with the recent list on top and the composer below; Start is a pinned full-width button under the thumb.
- Node pick is chips of up nodes (down nodes greyed), one pre-selected when only one node is up; a select appears only when more than ~4 nodes are configured.
- The directory is a span showing the chosen dir plus a Browse button that opens the directory picker inline in the sheet; `#form-dir` is no longer an `<input>`.
- Persona pick is chips — "default" plus the named personas from `/personas` — with the last-used persona pre-selected.
- The `#form-prompt` textarea is deleted. The first instruction goes into the session's chat box after Start.

- [ ] **S3 — Directory picker: whole-row taps**

As a user, I want to pick a directory by tapping rows, not small Open/Use buttons, so browsing works with one hand.

- Each directory entry is one full-width (≥44px) row that descends into it; `:active` replaces the hover background.
- The current path is a fixed head inside the sheet; `Use this directory` is one pinned button at the bottom, and `Up` is a second, always in reach.
- The per-entry Open and Use buttons are gone.

- [ ] **S4 — Session screen: thin header, `⋯` sheet**

As a user, I want the session screen to show the conversation and keep session controls one tap away, so the view never scrolls sideways and the actions are never off-screen.

- The session header collapses to back + `state dot / node·dir` + a `⋯` button; the current state is shown here, once. The header's id, node, model, persona, permission, and five action buttons are replaced by the `⋯` sheet.
- The `⋯` sheet holds full-width (≥44px) rows: permission toggle (labeled from live state), persona switch (a select, choices not text), copy session id, interrupt, and Stop.
- Stop sits in a separated red danger zone with a confirm; it is never next to the send button.
- The full session id is visible only in the sheet, on the copy row.
- Watch-only children: the input row is replaced by one banner — `watching child session — Open to act` — with a full-width Open button instead of disabled controls.

- [ ] **S5 — Ask options are tap-to-answer**

As a user, I want the model's question to offer me the answer as buttons, so a question does not force me to type.

- While an `ask` block with options is the live question, the input area renders the question and its options as thumb-width chips that POST the answer (`{ content, redirect: false }`).
- A muted `or type an answer` field stays under the chips, and the chat box placeholder no longer explains Ctrl+Enter (send is a button; Enter sends while an ask is active).
- The plain-text `<ul>` of options in the transcript is replaced by the chip row; an answered ask keeps a one-line `answered: <answer>` record.

- [ ] **S6 — Composer state and recent reuse survive everything**

As a user, I want my picks and my draft to survive cancel and close, so I never rebuild a session I was composing.

- Close and cancel hide the sheet without resetting the composer; only a successful Start writes the recent-settings entry and clears it.
- On open, the recent list is checked against the live node list: an entry whose node is gone is re-offered against a live node (or the person and prompt, with the dir cleared) instead of falling through a dead select value.
- The chat draft is kept when a session closes and restored when the same or another session opens.
- The recents order, five-entry limit, and dedup semantics in localStorage are unchanged.

- [ ] **S7 — Machines tab**

As a user, I want each machine's health at a glance and the details only where they matter, so the home screen does not repeat node state.

- The Machines tab lists nodes: `dot + name`, down nodes first and red, each with `down · 3m ago`; up nodes show no last-seen since the dot already says up.
- Tapping a node row shows nothing more today; the tab is a read-only surface until a later sprint needs node actions.

- [ ] **S8 — Transcript: deduplicated and touch-visible**

As a user, I want a transcript that says each thing once and exposes every tap on it, so nothing is repeated and nothing depends on hover.

- `handleEvent` no longer appends `* <state>`, `* persona: …`, or `— <model> —` transcript lines; state, persona, and permission live in the header and the `⋯` sheet.
- Tool strips and clipped tool results get ≥40px hit areas with an `▸` glyph and `:active` feedback; the clipped "click to expand" cue is drawn explicitly (accent colour and a `tap to expand` label), never a hover colour or `title` alone.
- The model-call separator lines are removed; the model for a session is shown in the `⋯` sheet instead.
- Everything else in the transcript — messages, tool calls and results, asks, summaries, child events — renders as today.

- [ ] **S9 — Touch-first CSS and no sideways scroll**

As a developer, I want the pane's CSS to enforce the mobile floor, so one-handed targets cannot regress while the sheet work lands.

- A shared `--touch-min-height: 44px` token and `:active`-state rules for every interactive class; the 640px and 560px media-query blocks are reworked so no action strip scrolls horizontally and header actions are never off-screen.
- Form and chat textareas are 16px so iOS does not zoom on focus; `-webkit-tap-highlight-color: transparent` is set pane-wide.
- The desktop layout at ≥900px keeps the current two-panel home and full session view, but the sheet and chip elements are the same components as mobile.

## Out of scope

No backend or API changes — the pane is a thin client of the existing REST and SSE surface, and the `ask` blocks already carry `options`. No push notifications, background refresh of the home screen beyond the current polling, node actions from the Machines tab, multi-window or offline support, or a build step for the pane (`include_str!` stays). No keyboard shortcut tier (the pane is touch-first); the terminal client keeps Ctrl+Enter semantics of its own.
