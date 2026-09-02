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
| `data_dir` | `data` | Directory for the SQLite store, injected skills, and persona prompt files |
| `models` | none | Named model entries (see `ModelConfig` below). Sessions never name one directly; a persona's `model` does |
| `personas` | none | Named personas (see `PersonaConfig` below) |
| `default_persona` | none | Persona sessions use when the request does not name one |

At boot the control plane validates the persona catalog: every persona's
`model` must name a configured model entry, its `allowed_tools` must be `"*"`
or canonical tool names, and a set `default_persona` must name a persona.

A model entry is the provider binding a persona's `model` names:

| Field | Meaning |
|---|---|
| `provider` | `anthropic` or `openai` |
| `name` | The provider's model name |
| `base_url` | Provider API root. Defaults to the provider's public API |
| `api_key` | A literal key, or `env:VAR` read from the environment at boot |
| `price_input_per_mtok` | Cost per million input tokens, used by metering |
| `price_output_per_mtok` | Cost per million output tokens, used by metering |

A persona pairs a model entry with the session's effective surface:

| Field | Default | Meaning |
|---|---|---|
| `model` | required | Names a configured model entry |
| `permission` | required | `read_only` or `read_write`, enforced by the session's executor |
| `allowed_tools` | `"*"` | `"*"` for every canonical tool, or a comma/space-separated list of canonical tool names |
| `description` | `""` | What the persona is for |

A persona's role/behaviour prompt lives outside the TOML: when
`<data dir>/personas/<name>.md` exists, its text is read at boot and becomes
the persona's system prompt for sessions under it. Without a file the session
runs on the built-in default system text. The personas directory is created at
boot like the skills directory; the prompt files themselves are optional.

`bosun clone` and `bosun dev` take `--persona <name>`; the persona's model,
permission, and allowed-tool set are resolved onto the session at creation
(the persona name is stored on the session), and a session without `--persona`
uses `default_persona`. Tool calls outside the allowed set are refused
control-plane-side; the executor enforces the permission. An `allowed_tools`
value that no longer parses fails the session's turn closed instead of
widening the tool set. The old `subagents`/`default_model` surface and
`--model` / `--permission` flags are replaced by personas.

### Switching a session's persona live

A session's persona can be switched mid-session with
`POST /sessions/{id}/persona` and a body of `{ "persona": "<name>" }`, from
the terminal client with `/persona <name>` in `bosun open`, or from the web
pane's session view. The switch replaces the stored session's persona, model,
permission, and allowed-tool spec in one transaction and records a `persona`
event (plus a `permission` event when the permission differs) on the session's
event stream. It applies from the next turn — an in-flight turn finishes under
the persona it started with. When the new persona's permission differs, the
executor's permission toggles live through the same `/permission` mechanism a
manual permission change uses; the executor toggle is best-effort, and the
stored session permission is authoritative. An unknown persona is refused with
`persona <name> is not configured` and nothing changes. This is the root
persona; the tree-wide child rules arrive with the tree itself.

See `crates/bosun-common/src/config.rs` for the current fields and defaults.

## Node

| Field | Default | Meaning |
|---|---|---|
| `cp_url` | `http://127.0.0.1:8090` | Control-plane base URL, `http` or `https` |
| `node_name` | `node` | Name this node registers under |
| `work_dir` | `work` | Directory session clones are created in |
| `browse_roots` | none | Directories `bosun dev` may browse and spawn into. Empty disables `bosun dev` on this node |
| `ca_cert` | none | PEM certificate the node trusts in addition to the system roots, for a control plane behind a private CA |
| `update.enabled` | `true` | Whether the node fetches a released binary for the control plane's announced version and auto-updates to it |
| `update.base_url` | none | Release feed the node fetches update archives from. Overrides `BOSUN_UPDATE_BASE_URL`, then GitHub Releases for this repository |

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
