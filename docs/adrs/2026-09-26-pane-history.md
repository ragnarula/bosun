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
- Back must not leave the pane. A back press from a session has to land on the list whenever the pane can decide what sits under the session.
- A session has to survive a reload and be shareable as a link.
- No new server route. The pane answers at two fixed paths, and `RESERVED_PATHS` in `crates/bosun-control/src/api.rs` keeps every path the router serves away from the OAuth callback's redirect URI.
- Bounded history. A session opened from a session must not stack entries, or one back press moves to another session instead of leaving.

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

**7. Leave an entry the pane did not write where it is, and repair history at load only. (rejected)**

A fragment pasted into the address bar while a session is open creates an entry the pane did not write, above whatever the browser held — often another session's entry. With no repair, the `‹` control and the browser's back button move from that session to the session underneath it, so leaving a session reaches another session instead of the list. The same two calls that repair a link loaded from another page repair this one.

## Decision

An open session is addressed as `#s=<session id>`, with the id percent-encoded — otherwise the pane is served at `/` and `/ui` exactly as before, and a reload or a shared link reopens the session.

The pane owns two history entries: the session list, and the one open session above it. Every entry it writes carries a state object with a `pane` key: `null` for the list, and the session id for a session. An entry is the pane's own for a session only when the state names that same session, because the browser copies the state it is on onto a fragment navigation, such as a pasted address. `paneEntry`, `isOwnEntry`, `sessionLink` and `sessionFromLink` in the pane are the whole of that bookkeeping.

`openSession(id)` is the one way into a session: it writes the entry and the fragment, then hands the id to the view. When the session already on screen owns the entry, the new session replaces it in place, so a session opened from a session — a child link in a transcript, or the owner behind a watch-only child — leaves the list as the entry directly under the session and adds no entry. Everywhere else, including an entry the pane did not write, the current entry becomes the list entry and the session is pushed above it.

A load starts on the session its fragment names, or on the list. When the loaded entry is the pane's own, a reload keeps it and adds no entry. Otherwise the pane writes the list entry onto it and pushes the session above it, so the browser's back button reaches the list instead of the page the link came from, and forward reopens the session.

`popstate` runs the same path on every back and forward: the fragment decides the screen, an entry naming no session shows the list, and a session entry the pane does not own gets the list entry written under it before the view opens. An entry already on screen is left alone, so a traversal does not rebuild the transcript or reopen the event stream for nothing.

The header's `‹` calls `history.back()`, so the control and the browser's button take the same step and a session has one way out.

A session stops being the entries' subject as soon as the pane knows it is gone: Stop, a poll that no longer lists the session, and a detail fetch that answers 404 each rewrite the current entry to the list entry, with the fragment off, and close the view. Each of those completions arrives after an await, so each checks that the session on screen is still the one it belongs to: a late detail reply, a late 404 and a late stop reply write nothing, close nothing, and rewrite no entry of the session the user moved to. An entry the pane is not on cannot be rewritten; visiting one whose session has since ended lands on the list for that reason.

Sheets stay out of history. The ask sheet and the ⋯ view sheet keep the close controls they had. `closeSession()` also hides the ⋯ sheet, which is a sibling of `#session-view` rather than a child of it, and clears the header, the sheet's identity fields and the footer's watch-only shape through `clearHeader`, so a session the pane cannot read yet shows no other session's node, directory, id, input row or sheet controls.

## Consequences

- The browser's back and forward buttons and the pane's `‹` are one path, so they cannot drift. Back from a session reaches the list. Before that list entry, the stack is the browser's: outside the pane when the pane wrote the list entry at its load, and otherwise the page, or the session, the user came through.
- The pane writes two entries where the browser had one: a link loaded from another page, or a fragment pasted into the address bar, leaves a list entry that the user never visited, so back returns to the list before it returns to that page.
- Entering a session from the list writes the list entry onto the current entry and then pushes the session, which is two history writes for one tap. A session opened from a session replaces one entry instead.
- An entry for a session that ends while the pane is elsewhere keeps its `#s=` fragment until the pane visits it. The pane cannot rewrite an entry it is not on, and the visit lands on the list once the detail fetch finds no session.
- The id in the fragment is not sent to the control plane, so it is never logged there. It is also not available to a proxy, which is why the pane reads `location.hash` itself rather than asking the API.
- The pane's state is now split between the DOM and the history entry. A later view that writes entries must carry the `pane` state key, or the pane will treat its entries as another page's and write a list entry under them.
- The open session lives in the address bar, so the pane reconnects its event stream and re-fetches the transcript on a reload. Nothing about the stream or the transcript changed.
- `crates/bosun-control/src/ui.rs` checks the pane by matching source text; it has no browser. The checks pin the fragment, the ownership rule, the single close path, the load path, the fragment clearing, the cleared identity and the late reply, and cannot see the rendered behaviour.

## Revisit When

- A second view needs a shareable address, which needs a fragment scheme with more than the one `s` key rather than a second mechanism beside it.
- The pane gains a router, a service worker or a build step that owns navigation.
- A session needs more address state than its id, such as a message anchor or a panel.
