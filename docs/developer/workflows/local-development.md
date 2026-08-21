# Local Development

This guide covers running Bosun locally for development.

## Control plane

```bash
cargo run -p bosun -- serve --config cmd/bosun/settings/serve.toml
```

The control plane reads its config from `--config`. See [config.md](../config.md) for the fields.

## Node

```bash
cargo run -p bosun -- node --config cmd/bosun/settings/node.toml
```

The node reads its config from `--config`. See [config.md](../config.md) for the fields.

A node must have `opencode` (and its Node or Bun runtime) on `PATH`, with the
provider key configured in its own opencode config. Bosun does not install or
configure opencode — that is node provisioning.

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

Clone a session, then connect the opencode client through the control-plane proxy:

```bash
cargo run -p bosun -- clone --node node-1 file:///path/to/repo
# cloned session <id> on node node-1 (status running)
# opencode attach http://127.0.0.1:<proxy-port>

cargo run -p bosun -- open <id>
# opencode attach http://127.0.0.1:<proxy-port>
```

Run the printed `opencode` command to drive the session. The client reaches the session through the control-plane proxy port, the node forwarder, and the opencode server on the node's loopback.

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
