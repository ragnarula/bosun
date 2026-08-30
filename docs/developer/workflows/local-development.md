# Local Development

This guide covers running Bosun locally for development.

## Control plane

```bash
cargo run -p bosun -- serve --config cmd/bosun/settings/serve.toml
```

The control plane reads its config from `--config`. See [config.md](../config.md) for the fields. A control plane needs at least one configured model; set `[models.default]` in the config and export the key it references, for example:

```toml
[models.default]
provider = "anthropic"
name = "claude-sonnet-4-5"
api_key = "env:ANTHROPIC_API_KEY"
```

## Node

```bash
cargo run -p bosun -- node --config cmd/bosun/settings/node.toml
```

The node reads its config from `--config`. See [config.md](../config.md) for the fields. The node runs one `bosun executor` process per session from the same binary; no other runtime or provider key is needed on the node.

## Controlling Log Output

The filter comes from the first of these that is set: the `--log-filter` flag, `RUST_LOG`, then `info`.

```bash
# All logs at debug level
RUST_LOG=debug cargo run -p bosun -- serve --config cmd/bosun/settings/serve.toml

# Specific crate at trace, others at info
RUST_LOG=info,bosun=trace cargo run -p bosun -- node --config cmd/bosun/settings/node.toml
```

The `--log-filter` flag overrides `RUST_LOG`.

## Driving a session

Clone a session and attach to it with the terminal client:

```bash
cargo run -p bosun -- clone --node node-1 file:///path/to/repo
# cloned session <id> on node node-1 (state waiting_for_input)
# open with: bosun open <id>

cargo run -p bosun -- open <id>
```

`bosun open` attaches interactively: it renders the live transcript, sends
messages from an input line, interrupts the current turn with ctrl-c, toggles
permission with ctrl-p, and reconnects after a disconnection. With no session id
it lists sessions to pick from. The web pane lives at the control-plane root
(`http://127.0.0.1:8090/`).

With no message, a new session idles at `waiting_for_input`; with
`--message <prompt>` the first turn starts immediately. The control plane runs
one agent loop per session; tool calls travel to the node's executor over the
session tunnel.

## Dev session in an existing directory

`bosun dev` browses a node's directories interactively and starts a session in
the one you pick, without cloning. The node must have `browse_roots` set in its
config, or `bosun dev` reports that browsing is disabled:

```bash
# node.toml
browse_roots = ["/home/me/code"]

cargo run -p bosun -- dev --node node-1
```

The directory is left in place when the session is stopped. Anything uncommitted
in it stays.
