# ADR: The web pane is a page, a stylesheet and native ES modules, embedded and served uncached

**Date:** 2026-09-29
**Author:** Raghav

## Context

The web pane is the control plane's browser client. `crates/bosun-control/src/ui.rs` embeds it in the binary with `include_str!` and serves it at `/` and `/ui`, which `crates/bosun-control/src/api.rs` registers. The page is served `no-store`, with the build's version written into a `bosun-version` meta tag. The vendored mermaid bundle is a separate embedded file at `/ui/mermaid.min.js`, cached for a day.

The pane's code was one HTML file: a stylesheet, the markup, and one classic script of about 3,850 lines. All of the script's names shared one global scope, so any function could read or write any top-level variable, and nothing stated which part of the pane used which.

Constraints fixed going in:

- No build step. The repository has no JavaScript toolchain, and the control plane is built by `cargo` alone.
- The binary is the whole deployment. The control plane reads no asset directory at run time.
- `RESERVED_PATHS` and `RESERVED_PATH_PREFIXES` in `api.rs` keep every path the router serves away from the OAuth callback.
- The pane inserts no HTML: no `innerHTML` anywhere (see the mermaid ADR).

## Decision Drivers

- Each part of the pane lives in its own file, and a file states what it takes from the others.
- A browser runs code from two builds together only when a new build starts between the page fetch and its module fetches, and a reload recovers from it.
- The router serves the pane's files and nothing else under `/ui/`.
- The browser checks in `crates/bosun-control/tests/browser/pane.py` drive the files the browser runs, and the source checks in `ui.rs` read the files as written.

## Options Considered

**1. Native ES modules and one stylesheet, embedded through an explicit table and served `no-store`. (chosen)**

Browsers load `<script type="module">` without a bundler, and `import` and `export` state each file's dependencies. The table is one line per file, so the served names are written in the code.

**2. Keep one HTML file. (rejected)**

It needs no serving work, but every name stays global. A change to one part of the pane can reach any other part, and a reader has to search the whole file to learn what a function touches.

**3. A bundler, such as esbuild, that builds one file from the modules. (rejected)**

It keeps a single served file and allows long-lived caching, but it adds a JavaScript toolchain to every build and to CI, and the file the browser runs is no longer the source a reader edits or the source checks read.

**4. Several classic `<script src>` files sharing the global scope. (rejected)**

It splits the text but not the scope. The order of the script tags becomes the dependency graph, and nothing in a file says what it uses from the others.

**5. The `include_dir` crate to embed the directory. (rejected)**

It embeds new files with no edit to `ui.rs`, but it adds a crate and a procedural macro for what a table of about twenty lines does. With the table, the served names are explicit, and a unit test keeps the table equal to the directory.

**6. Versioned URLs, such as `/ui/<version>/main.js`, with long-lived caching. (rejected)**

The modules would be cached across loads. It needs a second placeholder in the page, a route with a version segment, and a rule for a request that names another build. The saving is small: the page, the stylesheet and the modules are about 210 KB together, against 199 KB for the single `no-store` page they replace.

## Decision

The pane is `crates/bosun-control/src/ui/`: `index.html` holds the markup, `pane.css` the stylesheet, and one module per responsibility holds the code. `main.js` is the entry module and the only one the page runs. The other modules come in through its imports. The page names every other module in a `<link rel="modulepreload">`, so the browser fetches them all at once instead of learning each level of the import graph from the level above it: without the links, the chain `main.js` → `history.js` → `session-view.js` → `transcript.js` → `markdown.js` → `diagram.js` is six levels, one round trip each. A unit test checks that every module in `ASSETS` except `main.js` has its link. There is no build step. The browser loads the files as written.

`ASSETS` in `ui.rs` lists each stylesheet and module with its content type (`text/css; charset=utf-8` or `text/javascript; charset=utf-8`) and its body from `include_str!`. `ui::asset` serves an entry at `/ui/{asset}` with `Cache-Control: no-store`, and returns 404 for any name not in the table, so a request can reach no other file. The OAuth callback check rejects a path of one segment below `/ui/`, which is what `/ui/{asset}` matches, and accepts a deeper path such as `/ui/oauth/callback`. `/`, `/ui` and `/ui/mermaid.min.js` keep their handlers and headers. A unit test checks that `ASSETS` lists every `.js` and `.css` file in the directory except the mermaid bundle.

The page loads `/ui/main.js` before the deferred mermaid bundle. Deferred scripts and module scripts run in document order, and the diagram module listens for the bundle's `load` event, so it must run first.

Everything the pane holds for the open session is one object, `opened` in `session-view.js`: its state, its event stream and timers, the transcript's follow flag, live paragraph, last message and ask record, its activity, its read-back position and its subagent panel. `showSession` creates it and `closeSession` drops it. Other modules read it through a live binding and write its fields; only `session-view.js` replaces it, because an imported binding is read-only. A reply that lands after an await compares the object it started with against `opened`, so a reply for a session the pane has left does nothing. App-wide state, such as the session list, the nodes, the personas and the open children groups, stays in the module that fetches or owns it, and only that module writes it.

The block renderers in `transcript.js` return the element they build. The caller places it, in the session's transcript, the panel's or a page read back off screen, and decides whether that transcript follows. Each caller passes the record the renderers read and write: its ask box and the arguments of its `message_child` calls.

The pane's behaviour is held by the browser checks in `crates/bosun-control/tests/browser/pane.py`, which `tests/browser.rs` runs on demand against the real router. The source checks in `ui.rs` keep only what a browser cannot check well: that `ASSETS` matches the directory, the preload links, the order of the entry module and the mermaid tag, that no text is inserted as HTML, the link schemes, mermaid's settings and the Home-indicator insets. They read `PANE`, which joins `index.html`, `pane.css` in a `<style>` block and the modules in one `<script>` block, in `ASSETS` order.

## Consequences

- Each module states its imports and exports, and a name a module does not export cannot be reached from another module.
- Every load fetches the page and about twenty small files, none of them cached. That is twenty more requests than one page, and about 5% more bytes, most of it the import and export lists.
- `no-store` narrows the chance of mixing builds but does not remove it. The page and its modules are separate requests, so a control plane that restarts on a new build between the page's fetch and the module fetches serves the old page with new modules. A reload fixes it.
- A new stylesheet or module needs an `ASSETS` entry, an `include_str!` line in `PANE`, and, for a module, a `modulepreload` link. The unit tests fail until each exists.
- The order of the entry module and the mermaid tag in `index.html` matters. A source check in `ui.rs` holds it, and a browser check in `tests/browser/pane.py` draws a diagram, which fails in the other order.
- Module scripts are deferred and strict. The inline script was strict too, and ran after the markup it reads, so neither change alters what the code sees.
- Closing a session drops its whole object, so no piece of it can outlive the session. The browser checks fail when a session opened after another shows the first one's scroll position, read-back, activity, question, streamed paragraph or unanswered `message_child` calls, or when a late reply for the first one writes to the second. Nothing stops a module from writing a field the object does not declare: `newSessionState` in `session-view.js` is where the fields are listed.

## Revisit When

- The pane gains a build step, for example to type-check or to minify its code.
- The pane's own files grow large enough that loading them uncached costs a phone noticeable time.
- The pane needs a file that is not a stylesheet or a module, such as an image or a font.
