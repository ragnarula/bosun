# Sprint 001 — Bosun MVP setup

Single-user Bosun in Rust. No security, no scalability. Trusted network only.

Status: **complete**. All nine stories are implemented, tested, and committed.

## Confirmed decisions

- Multi-machine from day one. One Rust codebase: `bosun serve` (control plane), `bosun node` (daemon), and CLI subcommands.
- Node polls the CP every heartbeat; each poll upserts the node and reports its sessions. CP registry is in memory, rebuilt from node reports. Node persists its own session list and restarts its sessions after a node reboot.
- `bosun spawn --node <name> <git-url> [ref]` — node full-clones into `work_dir/<id>` using its own git credentials, writes the CP's `opencode.json` template into the clone dir, starts `opencode serve` on `127.0.0.1:<n>`, opens a forwarder on its advertised address.
- CP allocates a proxy port per session, reverse-proxies to the node forwarder, prints: `opencode --hostname <cp> --port <proxy-port>`. You create sessions inside the client yourself.
- `bosun stop <session>` kills opencode, closes the forwarder, deletes the clone, drops the proxy.

Connection path: `client -> CP proxy port -> node forwarder -> opencode serve on 127.0.0.1`.

## CLI surface

`serve`, `node`, `spawn`, `list`, `stop`, `open`, `nodes`

## User stories in implementation order

- [x] **S1 — Project scaffolding**

As a developer, I want a Rust workspace that builds and tests cleanly, so I can add features incrementally.

- Cargo workspace with binaries `bosun serve`, `bosun node`, and the CLI, plus a shared crate for types and config.
- Config structs loaded from a `bosun.toml` per role (CP: listen address, `opencode.json` template path; node: `cp_url`, `node_name`, `work_dir`, `advertise_addr`, heartbeat interval; CLI: `cp_url`).
- Structured logging wired up; `bosun --help` and `--version` work on every subcommand.
- `cargo test`, `cargo clippy`, and `cargo fmt --check` pass with no warnings.

- [x] **S2 — Node self-registration**

As a user, I want nodes to appear on the control plane without manual entry, so I can add a machine by just starting Bosun on it.

- Node starts a heartbeat loop: every interval it POSTs `{node_name, status}` to the CP and gets back nothing it needs to act on.
- CP upserts the node into an in-memory registry keyed by name; any node that has not heartbeated within a configurable timeout is marked down.
- `bosun nodes` lists nodes with status (up/down) and last-seen.
- Spawning onto a down node fails with a clear error.

- [x] **S3 — Spawn clones the repo**

As a user, I want `bosun spawn --node <name> <git-url> [ref]` to fetch the repo on that node, so a session has a fresh checkout to work in.

- CP validates the node is up, generates a session id, and POSTs the spawn request `{session_id, repo_url, ref, opencode_config}` to the node.
- Node clones `repo_url` into `work_dir/<session_id>` using its own git credentials; `ref` defaults to the remote default branch; clone failure reports the git error.
- On success the session appears in `bosun list` with id, repo, ref, node, and status; the CLI prints the session id.

- [x] **S4 — Inject config and start the agent server**

As a user, I want the spawned session to run a working opencode server with my provider config, so the agent can actually think and act.

- Node writes the CP's `opencode.json` template into the clone dir (the API key travels CP to node at spawn time).
- Node starts `opencode serve --hostname 127.0.0.1 --port <n>` in the clone dir with a free local port, and polls `/global/health` until healthy or a timeout; a failure to start reports an error and leaves the session failed.
- A failed spawn cleans up: kill the process, delete the clone dir, remove the session from node state.

- [x] **S5 — Node-side forwarder**

As a user, I want the node to expose the loopback server to the control plane, so the CP can reach it without opencode listening on the network.

- Node opens a forwarder listener on `advertise_addr:<port>` per session, forwarding bytes to `127.0.0.1:<opencode_port>`.
- The node's heartbeat includes each running session's forwarder address; `bosun list` reflects the session as ready.

- [x] **S6 — Control-plane proxy and end-to-end drive**

As a user, I want one URL per session on the control plane, so I can drive the agent with the opencode client from anywhere on the network.

- CP allocates a proxy port per session and reverse-proxies bytes `cp:<proxy_port> -> node:forwarder`.
- `bosun spawn` (and `bosun open <session>`) print the connect command: `opencode --hostname <cp> --port <proxy_port>`.
- End-to-end check: connect the opencode client, create a session, give it a task, watch it edit files in the cloned repo.

- [x] **S7 — Explicit stop**

As a user, I want `bosun stop <session>` to end a session completely, so nothing runs or costs money when I'm done.

- CP POSTs stop to the node; node kills opencode serve, closes the forwarder, deletes the clone dir, and removes the session from its state.
- CP closes the proxy port and removes the session; `bosun list` no longer shows it.
- Stopping an already-gone session reports success (idempotent).

- [x] **S8 — Node restart recovery**

As a user, I want sessions to survive a node reboot, so I don't lose running work when a machine restarts.

- Node persists its session list to a local state file; on startup it restarts opencode serve and the forwarder for each session, then resumes heartbeating.
- The CP registry is repopulated from heartbeats; `bosun list` shows the sessions again and `bosun open` yields a working URL.

- [x] **S9 — Control-plane restart resilience**

As a user, I want sessions to stay reachable after the control plane restarts, so a CP reboot doesn't orphan my running agents.

- On restart the CP rebuilds its session registry from node heartbeats, reallocates proxy ports, and accepts the new URLs. Sessions continue running on the nodes throughout.
- Note: proxy ports may change after a CP restart; `bosun list` always shows the current ones.

## Out of scope

No auth, no multi-user, no task queue or state machine, no done-criteria or budgets, no events API, no mobile or web clients, no node scheduling.
