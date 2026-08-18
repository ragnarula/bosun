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
