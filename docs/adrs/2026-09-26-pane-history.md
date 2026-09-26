# ADR: The open session is one browser history entry addressed by a `#s=` fragment

**Date:** 2026-09-26
**Author:** Raghav

## Context

The web pane (`crates/bosun-control/src/ui/index.html`) shows the session list and, over it, one open session. The pane is one embedded HTML file: `crates/bosun-control/src/ui.rs` serves it with `include_str!` at the two fixed paths `crates/bosun-control/src/api.rs` registers, `/` and `/ui`, with no build step and no client-side router. The session API is JSON under `/sessions`, with the id in the path.

The pane never touched `history`. Opening a session changed the DOM only: no `pushState`, no `replaceState`, no `popstate`, and no session id in the URL. The browser's back button therefore left the pane for whatever page came before it, while the pane's own `‹` control (`btnBack`) called `closeSession()`. On a phone, a back press that leaves the pane loses the list, and the forward entry it leaves behind reopens the same list.

Constraints fixed going in:

- The two ways out of a session — the `‹` control and the browser's back button — are one path.
- History entries belong to the open session only. The machines, skills and MCP views and the new-session sheet keep their current behaviour and gain none.
- Sheets are not history entries and keep their own close paths.
- The API does not change.

## Decision Drivers

- One way out. A header control that closes a session itself drifts from what the browser's back button does.
- Back must not leave the pane. A back press with the pane's own list behind it has to land on the list.
- A session has to survive a reload and be shareable as a link.
- No new server route. The pane answers at two fixed paths, and `RESERVED_PATHS` in `crates/bosun-control/src/api.rs` keeps every path the router serves away from the OAuth callback's redirect URI.
- Bounded history. A session opened from a session must not stack entries, or back walks a chain of sessions instead of leaving.

## Options Considered

**1. A `#s=<session id>` fragment, written with `pushState` and `replaceState`. (chosen)**

A fragment never reaches the control plane: the pane keeps its two routes, the API keeps its shape, and the id stays out of the request log. The pane reads `location.hash` and writes it as it opens and leaves sessions, and the browser's own back and forward buttons move between the two entries the pane writes.

**2. The session id in the path, such as `/s/<id>`. (rejected)**

It needs a new route in `router`, a decision about where that route sits relative to `RESERVED_PATHS` and the OAuth callback check, and a way to keep the mermaid bundle's absolute path served. The control plane would also receive the session id on every page load. The pane gains nothing over a fragment, which the browser resolves on its own.

**3. The session id in a query string, such as `/ui?s=<id>`. (rejected)**

The existing route serves it, so the routing cost is lower than option 2. The id is then part of every request the browser makes for the pane, so it lands in the access log of the control plane and of any proxy in front of it, and it is re-parsed by a route handler that ignores it. A fragment is read only by the pane.

**4. A hash router over every view: list, machines, skills, MCP, sheets. (rejected)**

One scheme for everything reads as tidier, but the other views are overlays with their own close controls, and an entry per overlay means a back press must decide between closing an overlay and leaving a session. The constraint is that history belongs to the open session alone.

**5. Close in place when no pane entry sits behind the session, and write no history at load. (rejected)**

This leaves a shared link, or a link opened from another page, with a foreign entry directly behind the session. The browser's back button then traverses to that page and the pane is gone before any handler runs; only the pane's own `‹` could be caught. Writing the list entry under the session at load keeps both back buttons inside the pane.

**6. The header's `‹` keeps calling `closeSession()`, with `popstate` handled separately. (rejected)**

Two paths out of a session: the control tears the view down, the browser button moves an entry. They differ as soon as the URL or the entry changes, which is the state this decision introduces.

## Decision

An open session is addressed as `#s=<session id>`, with the id percent-encoded — otherwise the pane is served at `/` and `/ui` exactly as before, and a reload or a shared link reopens the session.

The pane owns two history entries: the session list, and the one open session above it. Every entry it writes carries a state object with a `pane` key: `{ pane: null }` for the list and `{ pane: "<id>" }` for a session. A state without that key belongs to another page, so the pane never treats such an entry as its own.

`openSession(id)` is the one way into a session: it writes the entry and the fragment, then hands the id to the view. Opened from the list it pushes, so back reaches the list; opened from a session — a child line, a child link, or the owner behind a watch-only child — it replaces that session's entry, so the list stays the entry directly under the session and no session stacks on another.

A load on a `#s=` link opens the session the fragment names. When the loaded entry is not the pane's own — a link opened from another page, a pasted address, a restored session that lost its state — the pane writes the list entry onto that entry and then pushes the session above it, so the browser's back button reaches the list instead of the previous page, and forward reopens the session. A reload of the pane's own session entry writes nothing, because the list entry is still below it.

`popstate` reads the fragment: the entry it names decides the screen, and an entry that names no session shows the list. The header's `‹` calls `history.back()`, so the control and the browser's button take the same path, and forward reopens the session both leave.

Paths that leave a session without going back — Stop, and a session that ended on the node — rewrite the current entry to the list entry, with the fragment off, so the address bar never names a session the pane does not show. Sheets stay out of history: the ask sheet and the ⋯ view sheet keep the close controls they had, and `closeSession()` also hides the ⋯ sheet, which hangs outside `#session-view` and would otherwise sit over the list.

## Consequences

- The browser's back and forward buttons and the pane's `‹` are one path, so they cannot drift.
- A back press from a session always lands on the list. A second back press then leaves the pane, as a browser's back button does when the pane's own entries run out.
- A link opened from another page has the list entry written under it before the session shows: a tab opened on such a link holds one entry the user never visited, and the session's address bar is unchanged.
- The id in the fragment is not sent to the control plane, so it is never logged there. It is also not available to a proxy, which is why the pane reads `location.hash` itself rather than asking the API.
- Two entries mean the pane's state is now spread across the DOM and the history entry. A future view that writes entries must keep the `pane` state key, or the load path will write a list entry under it.
- The open session lives in the address bar, so the pane reconnects its event stream and re-fetches the transcript on a reload. Nothing about the stream or the transcript changed.
- `crates/bosun-control/src/ui.rs` checks the pane by matching source text; it has no browser. The new checks pin the fragment, the single close path, the load path and the fallback, and cannot see the rendered behaviour.

## Revisit When

- A second view needs a shareable address, which needs a fragment scheme with more than the one `s` key rather than a second mechanism beside it.
- The pane gains a router, a service worker or a build step that owns navigation.
- A session needs more address state than its id, such as a message anchor or a panel.
