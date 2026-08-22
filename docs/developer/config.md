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
| `node_timeout_secs` | `30` | Polls older than this mark a node down |
| `tls_cert` | none | PEM certificate chain. When set with `tls_key`, the control plane serves HTTPS |
| `tls_key` | none | PEM private key. When set with `tls_cert`, the control plane serves HTTPS |

For session subdomains to work over HTTPS, the certificate must cover both the
control-plane host and its wildcard (`bosun.on.21cs.biz` and
`*.bosun.on.21cs.biz`), and DNS must resolve every session subdomain to the
control plane. See `docs/adrs/2026-08-22-session-subdomains.md`.

## Node

| Field | Default | Meaning |
|---|---|---|
| `cp_url` | `http://127.0.0.1:8090` | Control-plane base URL, `http` or `https` |
| `node_name` | `node` | Name this node registers under |
| `work_dir` | `work` | Directory session clones are created in |
| `browse_roots` | none | Directories `bosun dev` may browse and spawn into. Empty disables `bosun dev` on this node |
| `ca_cert` | none | PEM certificate the node trusts in addition to the system roots, for a control plane behind a private CA |

The node opens no inbound ports. It polls the control plane and holds one
outbound tunnel per session, per `docs/adrs/2026-08-21-nodes-dial-out-only.md`.

## CLI

The CLI reads its control-plane URL from `~/.config/bosun/config.toml` (or
`$XDG_CONFIG_HOME/bosun/config.toml` when set). Store it once with:

```bash
bosun config set cp-url http://10.0.0.5:8090
bosun config get      # shows the stored URL and the file path
bosun config unset    # resets the stored URL to the default
```

Every CLI command resolves the URL from, in order: `--cp-url`, `BOSUN_CP_URL`,
the stored config file, then the default `http://127.0.0.1:8090`. To reach a
control plane behind a private CA, set `BOSUN_CA_CERT` to a PEM file the CLI
should trust.
