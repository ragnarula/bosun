# Configuration

Each role reads one TOML file passed with `--config`:

| Role | Command | Fields |
|---|---|---|
| Control plane | `bosun serve --config serve.toml` | `ControlConfig` |
| Node | `bosun node --config node.toml` | `NodeConfig` |
| CLI | stored config file, then `BOSUN_CP_URL`, then a default | `CliConfig` |

Every field has a default, so a config file can be sparse or empty. Deserialization fills missing fields from the struct's `Default` implementation. See `crates/bosun-common/src/config.rs` for the current fields and defaults.

## Control plane

| Field | Default | Meaning |
|---|---|---|
| `listen_addr` | `127.0.0.1:8090` | Address the control-plane HTTP server listens on |
| `node_timeout_secs` | `30` | Heartbeats older than this mark a node down |
| `proxy_bind` | `127.0.0.1` | Address the per-session proxy ports bind on |

## Node

| Field | Default | Meaning |
|---|---|---|
| `cp_url` | `http://127.0.0.1:8090` | Control-plane base URL |
| `node_name` | `node` | Name this node registers under |
| `work_dir` | `work` | Directory session clones are created in |
| `advertise_addr` | `127.0.0.1` | Address the control plane reaches this node at |
| `heartbeat_interval_secs` | `5` | Seconds between heartbeats |
| `listen_port` | `8091` | Port the node's HTTP server binds on |
| `browse_roots` | none | Directories `bosun dev` may browse and spawn into. Empty disables `bosun dev` on this node |

## CLI

The CLI reads its control-plane URL from `~/.config/bosun/config.toml` (or
`$XDG_CONFIG_HOME/bosun/config.toml` when set). Store it once with:

```bash
bosun config set cp-url http://10.0.0.5:8090
bosun config get      # shows the stored URL and the file path
bosun config unset    # resets the stored URL to the default
```

Every CLI command resolves the URL from, in order: `--cp-url`, `BOSUN_CP_URL`,
the stored config file, then the default `http://127.0.0.1:8090`.
