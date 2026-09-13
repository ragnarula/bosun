# ADR: MCP client support

**Date:** 2026-09-06
**Author:** Raghav

## Context

Bosun's model calls the tools the control plane advertises: the canonical surface in `crates/bosun-common/src/tool.rs`, plus the remote skill packages the `skill` tool resolves. An operator who runs an external Model Context Protocol server has no way to give a session its tools.

This decision adds MCP support: Bosun acts as an MCP client for HTTP servers, and the model calls their tools beside the canonical ones. The server list is managed in the web pane, a per-session selection decides which servers a session may use, and one control-plane manager owns a shared connection per server.

## Decision Drivers

- The model should see an MCP server's tools with no change to the provider adapters; the loop builds one tool list and the adapters serialize it.
- Sessions must not each hold their own connection to the same server: the connection carries the protocol's per-request metadata but no per-session state, so sharing it costs nothing.
- The store is the source of truth everywhere else in Bosun (sessions, skill repos, skill packages), so the server registry belongs there too, and the web pane stays the only management surface.
- One operator runs the control plane today, with no security model. Secrets in the store follow the same trust model as the rest of the single-user MVP, and may be `env:VAR` like a model `api_key`.
- The tools a session may call must be decided by the operator at session start, not inferred at call time.

## Options Considered

- **Serve Bosun to other MCP clients as an MCP server.** Rejected: it is a different surface with its own protocol obligations, and nothing in this sprint needs it.
- **Support the stdio transport.** Rejected: it needs a subprocess manager per server, with lifecycle, environment, and stderr handling. HTTP-only keeps the transport on the reqwest client the control plane already ships. An operator with a stdio-only server must wait for that manager.
- **Speak only the 2025-06-18-era `initialize` handshake.** Rejected: the current revision (`2026-07-28`) is stateless and carries version, identity, and capabilities in per-request `_meta`; a dual-era client connects to both kinds of server. A modern-only client would fail against every legacy server, and a legacy-only client would miss the current revision's behaviour.
- **Declare servers in `serve.toml`.** Rejected: repos and packages are managed in the web pane, and a config block would put server definitions in a second place the operator must edit.
- **A connection per session.** Rejected: the 2026-07-28 protocol is stateless, so sharing leaks no session state, and one connection per server keeps connection count independent of session count.
- **Prompt, resource, completion, logging, ping, sampling, elicitation, and root support.** Rejected: the model needs tools. Client capabilities are sent as `{}`, so a server that needs an unsupported capability receives the spec's `-32021` error rather than a silent wrong answer.
- **Dynamic client registration (RFC 7591).** Rejected: the spec deprecates it in favour of Client ID Metadata Documents, and an operator can paste a client ID instead.
- **Token exchange (RFC 8693) and enterprise-managed authorization.** Rejected: no server in scope needs them, and they add a second token flow.
- **Bind MCP tools to the session's read-only/read-write permission.** Rejected for now: the permission is enforced by the executor on canonical tools, and MCP calls happen on the control plane. Binding them needs the permissions overhaul, which is its own sprint.
- **Pass image, audio, and embedded-resource blocks to the model.** Rejected: the provider adapters carry no such blocks today. Non-text blocks are described briefly instead.
- **Spill oversized MCP results to a file like a tool result.** Rejected: spilling needs the executor's working copy, which an MCP result never touched. Oversized results are truncated with a marker instead.
- **Require the operator to pick scopes.** Rejected: the spec's scope-selection strategy (the 401 challenge's `scope`, else the resource metadata's `scopes_supported`) is well-defined and least-privilege, and step-up handles a later `insufficient_scope` challenge.
- **Host the OAuth callback at a fixed path.** Rejected: the callback URL must match a value registered with the authorization server, so the operator configures it as `oauth_redirect_uri`.

## Decision

**Bosun is an MCP client over HTTP. The `mcp_servers` table in the store is the registry, the web pane is the only management surface, and one control-plane manager holds a shared connection per enabled server.**

**Transport and era.** Every server is a streamable HTTP URL. At connect the client probes with `server/discover`; a server that answers with `UnsupportedProtocolVersionError` is retried at a version it lists. Any other non-modern answer falls back to the `initialize` handshake and the legacy revision the server selects, and a server that refuses the modern endpoint falls back further to the 2025-03-26-era streamable HTTP session and to the deprecated HTTP+SSE transport (`endpoint` event, then POSTs). The era is a property of the server and is cached for the connection's lifetime.

**Surface.** Tools only. Requests carry `io.modelcontextprotocol/protocolVersion`, `clientInfo`, and `clientCapabilities: {}` in `_meta`. Tool results map to text: text blocks concatenate, non-text blocks are described, `structuredContent` becomes compact JSON, and `isError` becomes a tool error. An oversized result is truncated with a marker. A tool that annotates a parameter with `x-mcp-header` has that value mirrored into an `Mcp-Param-{name}` header; a tool whose annotation violates the spec's constraints is excluded from `tools/list` and logged.

**Registry.** The `mcp_servers` table holds `name`, `url`, `enabled`, `auth` (`none` | `bearer` | `oauth`), the inline secret columns, the OAuth token columns (`oauth_access_token`, `oauth_refresh_token`, `oauth_expires_at_secs`, `oauth_scope`), `added_at_secs`, `updated_at_secs`, and `last_error`. List and read endpoints return the row without secret values. Every field may be `env:VAR`, resolved at connect like a model `api_key`. Secrets are plaintext in the store, matching the single-user MVP's trust model.

**OAuth.** `oauth_redirect_uri` in `serve.toml` names the callback URL; a wrong value fails boot with a clear error, and an unset value fails the flow with a clear error. The flow is the auth-code flow with PKCE (`S256`) and the `resource` parameter set to the server's canonical URL. Authorization-server discovery tries RFC 8414 metadata and OpenID Connect Discovery in the spec's order, and refuses a server that does not advertise `S256`. The callback validates `state` and, per RFC 9207, `iss`. Tokens store with their expiry; a refresh uses the stored refresh token and replaces it when the server rotates it; an `insufficient_scope` challenge re-authorises with the union of the requested scopes. Client registration is a pasted client ID (with an optional secret sent as HTTP Basic), or an HTTPS client-id URL sent as a Client ID Metadata Document. A static bearer token is the fallback for a server that needs no OAuth.

**Selection.** A `sessions.mcp_servers` column holds the selected names, chosen at creation through `POST /sessions`, `/clone`, and `/dev`, the CLI's repeatable `--mcp <name>`, or the web form's multi-select. The default is none, an unknown or disabled name fails creation, and `allowed_tools` continues to govern canonical tools only.

**Naming.** A tool whose name is valid and unambiguous is exposed bare with its description prefixed `[server]`; otherwise it is exposed as `<server>-<tool>`, both parts sanitised to `[a-zA-Z0-9_-]`, the tool part truncated so the whole name fits 64 characters and the server prefix kept intact while it fits and truncated when it cannot. A collision drops the later tool deterministically with a warning. Canonical tools stay bare; they contain no hyphen, so they cannot collide with a namespaced name.

**Health.** A failed connect retries with backoff, and the server row has a manual retry. `last_error` records the latest failure. A session whose chosen server is down starts without that server's tools and records a visible warning. The manager opens a `subscriptions/listen` stream with `toolsListChanged: true`, re-lists on the notification, and reuses the tool list within its `ttlMs`. Progress notifications stream to the live session view.

## Consequences

- A session's tool list is the canonical surface plus its selected servers' tools, so an operator can add a capability without a Bosun release.
- The store moves as one unit, as with sessions and skill packages; nothing on disk depends on the control plane's host.
- Secrets are plaintext in the store and in the backup of it. The single-user MVP already accepts this for provider keys, and this decision widens it to third-party server credentials. The API never returns them, but the file is readable by anyone who can read the store.
- One connection per server means one failing server affects every session that selected it. Sessions degrade rather than fail: the tools are absent and a warning shows.
- Sharing one connection means a tool list change is refreshed once for all borrowers, but it also means the manager's health is the feature's health.
- The dual-era fallback is the largest code surface here. It exists because an operator cannot tell from a URL which era a server speaks.
- Tool-name rewriting means the model calls a name the server never sees, and the transcript records the exposed name. A namespaced name is stable only while the server's tool names and the sanitisation rules are.
- OAuth requires the operator to register the callback URL with the authorization server before the flow can complete. A wrong `oauth_redirect_uri` is a boot failure, not a runtime surprise.
- Non-text MCP content never reaches the model, so an image-only tool result reads as `[image]`.

## Revisit When

- The permissions overhaul makes it possible to bind an MCP tool to read-only or read-write, or to grant a tool rather than a whole server.
- An operator needs a stdio-only server, which needs a subprocess manager on the control plane.
- Multi-user support arrives and the plaintext-secrets model stops holding.
- A server needs prompts, resources, sampling, elicitation, or roots; each needs its capability declared and its result mapped.
- The model needs image or audio MCP content, which needs provider adapters that carry those blocks.
- Serving Bosun to other MCP clients becomes useful, which is its own surface.
