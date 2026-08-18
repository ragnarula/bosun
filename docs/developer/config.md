# Configuration

Each role reads one TOML file passed with `--config`:

| Role | Command | Fields |
|---|---|---|
| Control plane | `bosun serve --config serve.toml` | `ControlConfig` |
| Node | `bosun node --config node.toml` | `NodeConfig` |
| CLI | reads `BOSUN_CP_URL`, else a default | `CliConfig` |

Every field has a default, so a config file can be sparse or empty. Deserialization fills missing fields from the struct's `Default` implementation. See `crates/bosun-common/src/config.rs` for the current fields and defaults.

## Control plane

| Field | Default | Meaning |
|---|---|---|
| `listen_addr` | `127.0.0.1:8090` | Address the control-plane HTTP server listens on |
| `template_path` | `opencode.json` | Path to the opencode config template injected into each spawned session |
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

## CLI

The CLI reads `BOSUN_CP_URL`, defaulting to `http://127.0.0.1:8090`.
