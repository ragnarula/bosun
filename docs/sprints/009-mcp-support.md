# Sprint 009 — MCP support

Sessions connect to external Model Context Protocol servers and the model can call their tools. The operator configures and authenticates servers in the web pane, and picks which servers each session gets when the session starts. A server's tools are discovered once, advertised to the model beside the canonical tools, and called through a shared connection owned by the control plane.

Status: **complete**. All seven stories are implemented and tested.

## Confirmed decisions

- **Bosun is an MCP client, HTTP only.** The session connects to external MCP servers and calls their tools. Serving Bosun to other MCP clients is out of scope, and the stdio transport is out of scope: every server is a streamable HTTP URL.
- **The latest revision, dual-era.** Bosun targets the current spec revision (2026-07-28) and interoperates with legacy servers: at connect it probes with `server/discover`, and on a legacy error it falls back to the `initialize` handshake and speaks the legacy revision the server selects, including the legacy HTTP+SSE transport for pre-2025-06-18 servers.
- **Tools only.** No prompts, resources, completions, logging, ping, sampling, elicitation, or roots. Client capabilities are sent as `{}`, so a server that needs an unsupported capability gets the spec's `-32021` error.
- **One shared connection per server**, owned by a control-plane manager; every session borrowing the server shares it. The 2026-07-28 protocol is stateless, so sharing a connection does not leak session state.
- **The registry is the store.** One `mcp_servers` table — name, URL, enabled flag, auth kind, inline secrets, last error, timestamps — joins the `SCHEMA` string like `skill_repos`. The web pane is the only management surface; no config block declares servers.
- **Secrets are plaintext in the store**, the same trust model as the rest of the single-user MVP. Each field may be `env:VAR`, resolved at connect like model `api_key`s. List and read endpoints never return secret values.
- **OAuth is the auth-code flow with PKCE**, per the 2026-07-28 authorization spec, plus a static bearer token fallback for servers that skip OAuth. No token exchange. Both discovery mechanisms are supported: RFC 8414 authorization-server metadata and OpenID Connect Discovery. Scope selection and the step-up flow follow the spec; `iss` is validated per RFC 9207; refresh tokens are stored and refreshed.
- **Client registration is pre-registered plus Client ID Metadata Documents.** The operator may paste a client ID and secret, or Bosun may host a metadata document whose URL is the client ID. Dynamic client registration is out of scope.
- **The OAuth redirect URI is configured** as `oauth_redirect_uri` in `serve.toml`, with no default. A wrong value fails the flow with a clear error.
- **Per-session selection.** The session row gains the server list, chosen at creation and stored like `allowed_tools`. The CLI gains a repeatable `--mcp <name>`; the web form gains a multi-select. Default is none.
- **`allowed_tools` keeps governing canonical tools only.** The per-session server selection is the whole MCP grant, and MCP tools are not bound by read-only/read-write permission today. A permission overhaul is a later sprint.
- **Connection health.** A failed connect retries automatically with backoff, plus a manual retry on the server row. A session whose chosen server is down starts without that server's tools and shows a warning.
- **Tool names.** A tool whose name is unambiguous and valid is exposed bare; otherwise it is exposed as `<server>-<tool>`, with both parts sanitised to `[a-zA-Z0-9_-]`. Canonical tools stay bare and never collide, because they contain no hyphen. On overflow the tool part is truncated so the whole name fits 64 characters; a server name longer than 63 characters cannot keep its whole prefix inside that cap, so it is truncated as well. On collision the later tool is dropped deterministically with a logged warning.
- **Descriptions.** A bare-named tool's model-facing description is prefixed with `[server]` so it stays attributable; a namespaced tool keeps the server in the name and is not prefixed. Canonical tools are untouched.
- **Results are text.** Text blocks are concatenated, non-text blocks are described briefly, `structuredContent` is serialised as compact JSON text, `isError` becomes a normal tool error, and oversized results are truncated with a marker. No new spill path.
- **Caching and progress are honoured.** The tool list is reused within its TTL, and `notifications/progress` deltas stream to the live session view.
- **List changes.** The manager opens a `subscriptions/listen` stream with `toolsListChanged: true`; on `notifications/tools/list_changed` it re-lists, and sessions pick up the new list at their next turn.

## CLI surface

`bosun clone` and `bosun dev` gain a repeatable `--mcp <name>` flag selecting the session's MCP servers. No other CLI change. `serve.toml` gains the optional `oauth_redirect_uri` field.

## User stories in implementation order

- [x] **S1 — The MCP server table**

As a developer, I want the store to hold configured MCP servers and their secrets, so the manager, the API, and the loop share one source of truth.

- `mcp_servers` joins the `SCHEMA` string with: `name TEXT PRIMARY KEY`, `url TEXT NOT NULL`, `enabled INTEGER NOT NULL DEFAULT 1`, `auth TEXT NOT NULL` (`none` | `bearer` | `oauth`), `bearer_token TEXT`, `oauth_client_id TEXT`, `oauth_client_secret TEXT`, `oauth_access_token TEXT`, `oauth_refresh_token TEXT`, `oauth_expires_at_secs INTEGER`, `oauth_scope TEXT`, `added_at_secs INTEGER NOT NULL`, `updated_at_secs INTEGER`, `last_error TEXT`.
- Store methods: list servers (secrets omitted), load one server with its secrets, insert, update, set enabled, delete, and the OAuth token rows read and write side.
- Tests exercise the write/read split: a list never returns secret columns, and an update that fails leaves the old row untouched.

- [x] **S2 — Manage servers from the web pane**

As a user, I want to add, list, edit, enable, disable, and delete MCP servers in the browser, so the web pane is the only surface I need.

- Control-plane routes for the server registry, mirroring the skill-repo routes: list, create, update, delete, and a togglable enabled.
- The web pane's MCP section lists each server with name, URL, enabled, connection state, last error, and OAuth status (authorized, token expiry, or not required), plus an add form and per-server edit, enable/disable, delete, and retry controls.
- Add and edit take a name, URL, and auth kind. Bearer takes a token or custom header value; OAuth starts the authorization flow. Create validates the URL and fails on a duplicate name before writing.

- [x] **S3 — OAuth for HTTP servers**

As an operator, I want to authorise an MCP server through the browser, so servers that require OAuth work without me handling tokens by hand.

- `oauth_redirect_uri` joins `ControlConfig`; the control plane serves the callback at that path.
- For an OAuth server, the web pane starts the flow: discovery via RFC 8414 metadata or OpenID Connect Discovery, client registration by pasted client ID/secret or a hosted Client ID Metadata Document, then the auth-code request with PKCE and the `resource` parameter carrying the server's canonical URL.
- The callback validates `state` and `iss` (RFC 9207), exchanges the code, and stores the access and refresh tokens with the expiry. A later 401 or expiry refreshes the token; a step-up `insufficient_scope` challenge re-authorises with the union of scopes.
- The web pane shows the OAuth status and a re-authorise control. Tests cover a full flow against a stub authorization server, a token refresh, and an `iss` mismatch rejection.

- [x] **S4 — The connection manager and MCP client**

As a developer, I want one shared connection per enabled server that discovers tools and calls them, so sessions borrow a live, healthy server instead of each speaking the protocol.

- A control-plane manager connects each enabled server at boot with `server/discover`, then opens `subscriptions/listen` with `toolsListChanged: true`. A modern server serves per-request `_meta`; a legacy server gets the `initialize` fallback and the legacy transport.
- The manager lists tools once, caches them with their TTL, and re-lists on `notifications/tools/list_changed`. A failed connect retries with backoff; the server row's retry button forces an immediate attempt. `last_error` records the latest failure.
- The client speaks `tools/list` and `tools/call` with pagination, `server/discover`, `subscriptions/listen`, and cancellation by closing the SSE stream. Progress notifications stream to the live session view.
- Tests: modern handshake, legacy fallback, tool-list caching within TTL, list-changed re-list, a down server reporting `last_error` and retrying.

- [x] **S5 — Advertise and call MCP tools in the loop**

As a model, I want the chosen servers' tools beside the canonical tools, so I can call them like any other tool.

- The loop builds its per-turn tool list from canonical tools plus the selected servers' discovered tools, translated through the naming rules: bare when unambiguous and valid, else `<server>-<tool>`, sanitised and truncated, collisions dropped with a log.
- A bare-named tool's description is prefixed with `[server]`. The provider adapters receive the finished list unchanged.
- A call routes by exposed name back to its server; the result is mapped text-faithfully and truncated with a marker at the existing limits. A server error surfaces as a normal tool error.
- A session whose chosen server is down at start omits that server's tools and records a visible warning. Tests: naming, sanitising, truncation, collision drop, routing, and result mapping.

- [x] **S6 — Per-session selection**

As a user, I want to choose which servers a session gets when I start it, so a session only ever sees the servers it was given.

- The session row gains the selected server list; `POST /sessions`, `/clone`, and `/dev` accept it and default to none. `bosun clone` and `bosun dev` gain a repeatable `--mcp <name>` flag.
- The web form's session creation gains a multi-select over enabled servers.
- Tests: a session with servers advertises only those tools; a session with none advertises only canonical tools; a disabled server is not selectable and an unknown server name fails creation.

- [x] **S7 — Config, docs, and the decision on record**

As a developer, I want the feature coherently configured and documented, so operators can adopt it.

- `config.md` documents `oauth_redirect_uri` and the web-pane management model.
- `../adrs/2026-09-06-mcp-support.md` records the decisions: HTTP-only, dual-era latest revision, tools-only capabilities, one shared connection per server, the store registry with inline secrets, the OAuth scope, and the naming rules.
- `CLAUDE.md`'s current-state line moves to this sprint, and the README's status paragraph mentions MCP support.

## Out of scope

- **The stdio transport.** Dropped: stdio-only servers are unusable until a later sprint adds a subprocess manager.
- **Bosun as an MCP server.** Other clients driving Bosun is a different surface and its own sprint.
- **Prompts, resources, completions, logging, ping, sampling, elicitation, and roots.** A server needing an unsupported capability receives the spec's `-32021` error.
- **Token exchange (RFC 8693)** and the enterprise-managed-authorization extension.
- **Dynamic client registration (RFC 7591).**
- **Read-only/read-write binding for MCP tools**, pending the later permissions overhaul.
- **Image, audio, and embedded-resource passthrough to the model**; non-text blocks are described, not sent.
- **A spill path for oversized MCP results**; they are truncated with a marker.
- **Non-HTTP transports**, including custom byte-stream transports.
