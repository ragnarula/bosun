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

Spawn a session, then connect the opencode client through the control-plane proxy:

```bash
cargo run -p bosun -- spawn --node node-1 file:///path/to/repo
# spawned session <id> on node node-1 (status running)
# opencode --hostname 127.0.0.1 --port <proxy-port>

cargo run -p bosun -- open <id>
# opencode --hostname 127.0.0.1 --port <proxy-port>
```

Run the printed `opencode` command to drive the session. The client reaches the session through the control-plane proxy port, the node forwarder, and the opencode server on the node's loopback.
