# Bosun

A persistent, distributed harness for AI coding jobs.

Bosun runs coding jobs on any infrastructure. A job arrives from an issue
tracker, from a change waiting for review, from a schedule, or from a person
typing a request. Bosun runs it on one of your machines, gives the agent the
standards you have set, and reports what happened. The job runs in the
background on that machine, so closing your terminal does not stop it, and each
job gets its own agent, working copy and permission, so one job does not disturb
another.

Capacity grows with the machines you add: each one runs its own node and dials
out to the control plane, so no machine needs an open inbound port.

Bosun is written in Rust. Sessions run on your control plane, tool calls
execute on the node the session works on, and you drive sessions from a
terminal client or the web pane.

## Status

Single-user MVP. One control plane holds every session: there is no security
model, no scheduler, and no automatic placement yet, so run it on a network you
trust and name the node each job runs on.

Shipped so far:

- **Sessions that outlive the client.** A session runs on the control plane and
  its node, so closing the terminal or the browser does not stop it, and
  `bosun open` reattaches to a live one.
- **Sessions in a tree.** A session spawns children and names the node and
  directory each one runs on. A child reports to its parent by ending its turn,
  and the pane watches a child in a panel beside the session, on its own event
  stream.
- **Forks.** The pane's actions sheet forks a session's conversation into a new
  root session. The fork keeps the same model, persona, permission, MCP servers
  and thread, and gets its own clone of the same repository; the original is
  left as it was.
- **Context control.** The transcript states the session's context size once a
  turn, from half the window up, so the session can see how close it is to
  compaction. A session can also clear its own context, giving a reason, and
  continue from a fresh prompt; the reader's transcript keeps everything.
- **Remote skill packages.** Skills come from GitHub repositories the operator
  manages in the web pane, and reach sessions through the `skill` tool.
- **MCP support.** The control plane connects to external MCP servers over
  HTTP. The operator manages the server list and each session's selection in
  the web pane, and the chosen servers' tools reach the model beside the
  canonical tools.
- **Session summaries.** Each session's own model writes a one-line description
  of what the session is for and what it is doing now. The session's loop
  refreshes it once the session goes idle, and the web pane leads the session
  row with it.
- **A mobile-first web pane.** The pane lists nodes and sessions, starts a
  session, follows a live transcript, and manages skill repositories and MCP
  servers. The open session is one browser history entry addressed by a `#s=`
  fragment, so a phone's back gesture returns to the list, and a mermaid fence
  renders as a diagram in both clients.
- **Updates.** Nodes converge on the control plane's version, and a deployment
  re-issues the call each working session had in flight, once. The CLI
  self-updates from GitHub Releases.

The current sprint and the planned roadmap are tracked in
[docs/sprints](docs/sprints/).

## How it works

- **Control plane** (`bosun serve`) runs one agent loop per session, keeps the
  session store, manages skill repositories and MCP servers in the web pane,
  and holds one shared connection per enabled MCP server.
- **Nodes** (`bosun node`) dial out to the control plane, one node per machine.
  Each session runs its tools in-process on the node, scoped to the session's
  working copy and permission.
- **Clients** are one binary: `bosun clone` starts a session from a repository,
  `bosun dev` starts one in an existing directory on a node, `bosun list` shows
  sessions, and `bosun open` attaches to a live session.
- **Sessions** hold their own transcript, store, and model calls. Tool output
  streams back to the client live; assistant text renders as markdown.
- **Skills** are packages fetched from GitHub repositories the operator adds in
  the web pane, stored in the session store, and loaded on demand through the
  `skill` tool beside the working copy's own skills. Versions are commit SHAs;
  an update re-resolves the tracked ref and replaces the packages.

## Requirements

- [Rust](https://rustup.rs/) stable, with edition 2024 support, to build from
  source.
- Rust nightly, for formatting. The options in `rustfmt.toml` are nightly-only,
  so the pre-commit hook runs `cargo +nightly fmt`.

## Install

Each release publishes one archive per platform, with a `.sha256` file beside
it:

- Linux: `x86_64` and `aarch64`, statically linked against musl
- macOS: `x86_64` and `aarch64`
- Windows: `x86_64`

Download the archive for your platform from the
[latest release](https://github.com/ragnarula/bosun/releases/latest), unpack
it, and put `bosun` on your `PATH`. `bosun update` then updates the binary to
the control plane's version. To run from source instead, see
[Build and test](#build-and-test).

## Run

Three pieces make a working setup: a control plane, at least one node, and a
client. Config files are TOML; commented templates live in
[`cmd/bosun/settings/`](cmd/bosun/settings/). A control plane also needs at
least one model and one persona, as
[`cmd/bosun/settings/serve.toml`](cmd/bosun/settings/serve.toml) describes.

```sh
# 1. Control plane. Models and their API keys are configured here.
bosun serve --config cmd/bosun/settings/serve.toml

# 2. A node on the machine that does the work.
bosun node --config cmd/bosun/settings/node.toml

# 3. Client commands, from anywhere that can reach the control plane.
bosun nodes
bosun dev --node node-1          # pick a directory interactively
bosun clone --node node-1 https://github.com/you/repo.git   # clone a repo
bosun list
bosun open <session-id>          # attach to a live session
bosun stop <session-id>
```

The control-plane URL defaults to `http://127.0.0.1:8090` and can be set per
command with `--cp-url`, stored with `bosun config set cp-url`, or exported as
`BOSUN_CP_URL`.

The web pane is served at the control-plane root (`/` or `/ui`). Open it in a
browser to see the node list, start a session, follow its live transcript, and
manage skill repositories and MCP servers.

## Repository layout

| Path | Purpose |
|---|---|
| `cmd/bosun` | The `bosun` binary: control plane, node daemon, and client |
| `crates/bosun-control` | Control plane: agent loops, session API, web pane |
| `crates/bosun-node` | Node daemon: session and executor management, polling |
| `crates/bosun-agent` | Agent loop and model-provider streaming |
| `crates/bosun-executor` | Per-session executor and its tool set |
| `crates/bosun-common` | Shared types, config, and protocol framing |
| `crates/bosun-store` | SQLite session store |

## Build and test

```sh
cargo build --release
cargo test
cargo clippy --workspace
cargo +nightly fmt --check
```

The binary lands at `target/release/bosun`. The repository also ships
pre-commit hooks that run formatting, clippy, and `cargo deny`; see
[docs/developer/workflows/setup.md](docs/developer/workflows/setup.md).

## Documentation

- [Developer standards](docs/developer/README.md): the rules to follow while
  writing code here, and the procedures around it.
- [Architecture decisions](docs/adrs/): the ADRs behind the current design.
- [Sprint notes](docs/sprints/): how the project got here and where it is
  going.

## Contributing

Set up a development environment with
[docs/developer/workflows/setup.md](docs/developer/workflows/setup.md), then
read [docs/developer/README.md](docs/developer/README.md) before you change
code.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.
