# ADR: Both clients render a mermaid fence as a diagram

**Date:** 2026-09-21
**Author:** Raghav

## Context

A model answers in markdown, and the transcript carries that text to every client: the store and the event stream hold text, and each client renders it itself. Neither client renders anything but markdown. The terminal client draws into a ratatui frame (`cmd/bosun/src/markdown.rs`) and has no rasteriser, no image protocol and no terminal-capability detection. The web pane is one embedded HTML file (`crates/bosun-control/src/ui/index.html`) with no build step, no asset directory, no `Content-Security-Policy`, and no `innerHTML` anywhere: model text reaches the DOM as `textContent` and `createTextNode` only.

Models write mermaid when they explain a flow or a structure. Both clients already receive such a fence and both show its source.

## Decision Drivers

- One source of truth. The diagram belongs to the model's text. The store holds text, sessions already running must gain the rendering, and no side channel may be needed for a diagram to exist.
- Two clients with different abilities. A cell grid draws box-drawing text; a browser draws SVG.
- Bounded cost. A redraw re-renders every transcript line, not just the visible ones, and diagram layout costs 7 ms at ten nodes and 250 ms at two hundred.
- No new machinery in the pane. No build step, no asset directory, and model text keeps reaching the DOM as nodes rather than as parsed HTML.
- Few dependencies. `deny.toml` allows permissive licences only, bans duplicate crate versions, and admits crates.io only.

## Options Considered

**1. Each client detects a `mermaid` fence and renders it. (chosen)**

Both clients already split a message into fences and both discard the info string today, so the detection is a branch in each renderer, and the transcript, the store, the API and the event stream stay unchanged.

**2. A tool the model calls to produce a diagram. (rejected)**

A tool result is plain text in the transcript: the terminal flattens it to `key: value` lines and clips it at 1000 characters (`MAX_INLINE_CHARS` in `cmd/bosun/src/attach.rs`), and the pane previews 400 with tap-to-expand (`TOOL_RESULT_PREVIEW`). Text art returned by a tool would be truncated and rewrapped, and both clients would still need a rule that renders one tool's result unlike every other — the same client work as option 1, plus a model round trip and its tokens for each diagram. No tool result renders specially today. A tool that validates a model's mermaid before the model authors it remains available on top of this decision.

**3. The terminal draws a rasterised image through a terminal graphics protocol. (rejected)**

Kitty, sixel and iTerm2 inline images can show a diagram at full fidelity, but only on terminals that implement them, and the repository has no rasteriser, no protocol code and no way to ask a terminal what it supports. The client draws into a ratatui frame; text is the only output every terminal shows.

**4. The terminal renders text with a multi-format crate (mmdflux). (rejected)**

mmdflux renders text, ASCII, SVG and JSON from one crate, which would let a single crate serve both clients. It brings 30 packages against mermaid-text's 8, and its text output takes no column budget, which the terminal needs because its pane width varies per client and per resize. With the web pane rendering through mermaid itself, its second output format buys nothing.

**5. The pane asks the control plane for SVG, rendered by a Rust crate. (rejected)**

This keeps one renderer for both clients and adds no JavaScript, but it needs a new endpoint and a render in the control plane per diagram, and it draws mermaid through a third-party reimplementation of it rather than through mermaid. The user chose mermaid's own renderer for fidelity.

**6. The pane loads mermaid's ESM entry and its lazy chunks. (rejected)**

The ESM entry is 30 KB, but it imports 104 chunk files with content-hashed names, 5.5 MB in total. Vendoring and serving that directory is more machinery than one file, and every name changes on upgrade.

**7. The pane shows the fence's source and renders nothing. (rejected)**

The pane is where a reader has screen space and colour. Shipping the terminal's text art there would make the mobile pane the worst place to read a diagram.

## Decision

A fenced block whose info string is `mermaid` — the first whitespace-separated token, case-insensitive — is a diagram in both clients. Every other fence keeps its source, exactly as before. A fence renders only once it is closed, so a message still streaming shows its source.

The terminal client renders the source with the `mermaid-text` crate at the width left for the transcript, as a code block. It shows the source instead when the crate errors, when the body is empty, or when the layout is wider than the width in display columns. Rendered diagrams live in `DiagramCache` on `ClientState`, keyed by source and width, bounded by 64 entries and 1 MiB, oldest first out.

The web pane renders the source with mermaid 12.0.0, vendored as `crates/bosun-control/src/ui/mermaid.min.js` beside its MIT licence `mermaid.LICENSE`. The control plane serves it at `/ui/mermaid.min.js` with `Cache-Control: public, max-age=86400`, as an embedded asset like the pane itself. The pane initialises mermaid once with `securityLevel: 'strict'`, a top-level `htmlLabels: false` and `theme: 'dark'`; `htmlLabels` works only at the top level, and under `flowchart` it leaves HTML labels in `<foreignObject>`. The pane parses the returned SVG with `DOMParser`, rejects a `parsererror` or a root that is not `svg`, and inserts a copy with `importNode`, so the pane still inserts no HTML. Every failure — a bundle that never loads, invalid source, an SVG the parser rejects — leaves the fence's source in a `<pre>`. A render that throws leaves a scratch element in `document.body`, which the pane removes.

`.pre-commit-config.yaml` excludes the vendored bundle from `trailing-whitespace`, `end-of-file-fixer` and `check-added-large-files`. Nothing edits a vendored file, and it is 5.5 MB.

## Consequences

- The terminal pays one layout per diagram per width, and the cache means a redraw does not pay again. A transcript holding more diagrams than the cache holds re-renders the excess on every redraw.
- The terminal's diagram is box-drawing text: monospaced, readable, and an approximation of mermaid's layout. Labels wider than one column are counted in display columns, so a diagram that only just fits falls back to its source.
- The vendored bundle adds 5.5 MB (1.6 MB gzipped) to the control-plane binary and to the pane's first load, which is a real cost on a phone. A newer bundle stays served for up to a day.
- The pane now runs third-party JavaScript that renders model-supplied text. `securityLevel: 'strict'` with `htmlLabels: false` keeps label text out of HTML, and the insert path stays out of `innerHTML`, but the page does host output it did not build.
- The terminal client gains 8 packages (mermaid-text, ascii-dag, chrono, iana-time-zone, num-traits, unicode-width and their dependencies). No duplicate crate version is introduced.
- Nothing instructs the model to emit mermaid. An operator who wants diagrams writes that instruction into a persona's role text, which is one layer of the system prompt; no code carries it.
- A fence with four or more backticks is not a diagram, matching the three-backtick fence rule both clients already had.

## Revisit When

- mermaid ships a bundle small enough that it is not the pane's largest response, or the pane gains a build step and a static-asset route.
- A terminal in daily use implements kitty or sixel and the repository gains a rasteriser, making fidelity worth more than text.
- Models stop writing mermaid, or write it under a different fence language.
- The cache bounds stop covering a realistic transcript, or a diagram exists that neither renderer lays out legibly.
