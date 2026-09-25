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
| `data_dir` | `data` | Directory for the SQLite store and persona prompt files |
| `nudge` | `true` | Whether the agent loop appends its `[harness nudge]` message when a turn ends with prose and no tool call. `false` takes the pre-nudge path for every session on the control plane: no message is appended and no announcement is checked, so a prose-ending wake ends there, with a root waiting for the operator and a child reporting to its parent and stopping |
| `github_token` | none | Optional GitHub token, a literal or `env:VAR` read from the environment at boot. Sent as the `Authorization` header when the control plane fetches or updates skill repositories; private repos need it, public repos do not. Never stored or exposed; see skill repositories below |
| `oauth_redirect_uri` | none | The control-plane URL the MCP OAuth callback is served at. No default. A path the control plane already serves fails boot with a clear error, and an unset value fails an OAuth flow with a clear error; see MCP servers below |
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

A persona's role/behaviour prompt lives outside the TOML. When
`<data dir>/personas/<name>.md` exists, Bosun reads its text at boot and appends
it to the session's system prompt as the persona's role layer. The harness
contract always comes first, and the role layer cannot override it. Without a
file the session has no role layer; it still gets the contract and the session's
live context, such as the repo-standards notice and the skill list. The personas
directory is created at boot; the prompt files themselves are optional.

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

### Skill repositories

Skills come from GitHub repositories that the operator manages in the web pane:
add, remove, and update happen there, and the store holds the repo list. The
web pane lists each repo with the ref it tracks, the commit SHA it was
indexed at, the number of packages, and the last error. Adding a repo fetches
it immediately; Update re-resolves the tracked ref to a commit and, when the
SHA changed, atomically replaces that repo's packages. A disabled repo stops
advertising its packages but keeps them stored.

A skill is a Bosun Skill Package with an identity like
`github.com/owner/repo/<path...>/skills/<name>`, served to sessions from the
store. The `skill` tool advertises these beside the working copy's own skills
and loads a package's instructions or one of its reference chunks on demand.
See `docs/adrs/2026-09-06-skill-package-standard.md` for the standard and
`docs/adrs/2026-09-06-remote-skill-packages.md` for the acquisition and storage
model.

### MCP servers

Bosun is an MCP client for HTTP servers. The operator manages the server list
in the web pane's MCP section: add, edit, enable or disable, retry, and remove
happen there, and the store's `mcp_servers` table holds the list. Each row
carries the name, the URL, the auth kind (`none`, `bearer`, or `oauth`), the
inline secrets, the OAuth tokens with their expiry and the scopes they were
granted for, and the last error. List and read endpoints never return secret
values.

A server's secret is a literal or `env:VAR`, resolved at connect like a model
`api_key`. Secrets are plaintext in the store, matching the single-user MVP's
trust model; see `docs/adrs/2026-09-06-mcp-support.md`.

An OAuth server needs `oauth_redirect_uri` set to the callback URL registered
with its authorization server. The pane starts the flow; the authorization
server redirects back to that URL. A server that needs no OAuth takes a static
bearer token instead. A later 401 or an expired token refreshes from the stored
refresh token. If a server refuses a call because the token's scopes are too
small, the control plane asks for the union of the scopes already granted and
the challenge's scope; the row's OAuth status becomes re-authorization
required, and the Re-authorize control opens that authorization URL.

A session gets the servers chosen when it was created: `bosun clone` and
`bosun dev` take a repeatable `--mcp <name>`, `POST /sessions`, `/clone`, and
`/dev` accept the list, and the web pane's new-session form has a multi-select
over the enabled servers. The default is none, and an unknown or disabled name
fails creation. The selection is the whole MCP grant: a persona's
`allowed_tools` governs the canonical tools only, and MCP tools are not bound
by the read-only/read-write permission today.

The control plane connects each enabled server at boot and gives every session
that selected it the same connection. A tool whose name is valid and
unambiguous is exposed bare with its description prefixed `[server]`;
otherwise it is exposed as `<server>-<tool>`. A failed connect retries with
backoff, the row's retry control forces an immediate attempt, and `last_error`
records the latest failure. A session whose chosen server is down starts
without that server's tools and shows a warning.

## Node

| Field | Default | Meaning |
|---|---|---|
| `cp_url` | `http://127.0.0.1:8090` | Control-plane base URL, `http` or `https` |
| `node_name` | `node` | Name this node registers under |
| `work_dir` | `work` | Directory session clones are created in |
| `browse_roots` | none | Directories the interactive `bosun dev` picker may browse and spawned child-session executors may run in. Empty disables `bosun dev` and child-session spawning on this node |
| `ca_cert` | none | PEM certificate the node trusts in addition to the system roots, for a control plane behind a private CA |
| `update.enabled` | `true` | Whether the node fetches a released binary for the control plane's announced version and auto-updates to it |
| `update.base_url` | none | Release feed the node fetches update archives from. Overrides `BOSUN_UPDATE_BASE_URL`, then GitHub Releases for this repository |

`browse_roots` confines both the interactive `bosun dev` picker and spawned
child-session executors: the control plane starts each child's executor on
its parent's working copy, and that directory must lie within a root, exactly
like a picked `dev` directory. Clone sessions live under `work_dir/<id>`, so
to let children of clone sessions run, `browse_roots` must include the node's
`work_dir`.

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
