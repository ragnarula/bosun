# 🚀 Bosun

Send software work to AI agents and run them on machines **you** own. 🤖

Work arrives from an issue tracker 🎫, from a change waiting for review 👀,
from a schedule ⏰, or from a person typing a request ⌨️. Bosun finds a
machine 🖥️, starts an agent there, gives it the standards you have set 📋,
and reports what happened 📊. Each agent works on its own machine, so no
agent affects another 🛡️.

Bosun is written in Rust 🦀. Sessions run on your control plane 🎛️, tool
calls execute on the node the session works on 🧰, and you drive sessions
from a terminal client 💻 or the web pane 🌐.

## 🧭 Status

Single-user MVP. There is no security model and no scalability story yet:
run it on a network you trust 🔒. The current sprint and the planned roadmap
are tracked in [docs/sprints](docs/sprints/).

## ⚙️ How it works

- **🎛️ Control plane** (`bosun serve`) runs one agent loop per session and
  keeps the session store. It serves a web pane listing nodes and sessions.
- **🖥️ Nodes** (`bosun node`) dial out to the control plane. No open inbound
  ports are needed on a node. Each session has its own executor process on
  the node, which runs the tools.
- **💻 Clients** are one binary: `bosun clone` starts a session from a
  repository, `bosun dev` starts one in an existing directory on a node,
  `bosun list` shows sessions, and `bosun open` attaches to a live session.
- **🧠 Sessions** hold their own transcript, store, and model calls. Tool
  output streams back to the client live; assistant text renders as markdown.
- **🔄 Updates** flow from the control plane to nodes, and the CLI
  self-updates from GitHub Releases.

## 📁 Repository layout

| Path | Purpose |
|---|---|
| `cmd/bosun` | The `bosun` binary: control plane, node daemon, and client 🎛️ |
| `crates/bosun-control` | Control plane: agent loops, session API, web pane 🎛️ |
| `crates/bosun-node` | Node daemon: session and executor management, polling 🖥️ |
| `crates/bosun-agent` | Agent loop and model-provider streaming 🧠 |
| `crates/bosun-executor` | Per-session executor and its tool set 🧰 |
| `crates/bosun-common` | Shared types, config, and protocol framing 📦 |
| `crates/bosun-store` | SQLite session store 🗄️ |

## 🛠️ Build and test

```sh
cargo build --release
cargo test
cargo clippy --workspace
cargo fmt --check
```

## ▶️ Run

Three pieces make a working setup: a control plane 🎛️, at least one node 🖥️,
and a client 💻. Config files are TOML; commented templates live in
[`cmd/bosun/settings/`](cmd/bosun/settings/).

```sh
# 1. Control plane. Models and their API keys are configured here. 🔑
bosun serve --config cmd/bosun/settings/serve.toml

# 2. A node on the machine that does the work. 🛠️
bosun node --config cmd/bosun/settings/node.toml

# 3. Client commands, from anywhere that can reach the control plane. 📡
bosun nodes
bosun dev --node node-1          # pick a directory interactively 🧭
bosun clone --node node-1 https://github.com/you/repo.git   # clone a repo 📥
bosun list
bosun open <session-id>          # attach to a live session 🖥️
bosun stop <session-id>
```

The control-plane URL defaults to `http://127.0.0.1:8090` and can be set per
command with `--cp-url`, stored with `bosun config set`, or exported as
`BOSUN_CP_URL` 🌐.

The web pane is served at the control-plane root (`/` or `/ui`). Open it in a
browser to see the node list, start a session, and follow its live
transcript 🍿.

## 📚 Documentation

- [Developer standards](docs/developer/README.md): conventions for anyone
  writing code here ✍️.
- [Architecture decisions](docs/adrs/): the ADRs behind the current design 🏗️.
- [Sprint notes](docs/sprints/): how the project got here and where it is
  going 🗺️.

## 📜 License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.
