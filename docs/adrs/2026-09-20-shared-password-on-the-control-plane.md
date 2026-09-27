# ADR: One shared password guards the control plane

**Date:** 2026-09-20
**Author:** Raghav

## Context

The control plane serves one HTTP surface on one port: the web pane at `/` and `/ui`, the session API, `POST /poll`, and `GET /tunnel/node/{node}`. Its router carries one middleware, `add_version_header`, and no check of the caller. Anyone who can reach the port can start a session, read and drive any transcript by id, browse a node's directories, register a node name through `/poll`, or take over another node's queued commands.

The deployment shape is fixed going in. One control plane serves one operator, who owns every node. Nodes dial out only, per `2026-08-21-nodes-dial-out-only.md`, so the control plane has no inbound port on a node to protect. TLS stays the operator's choice through `tls_cert` and `tls_key` on the control plane and `ca_cert` or `BOSUN_CA_CERT` on the clients.

Config secrets already have a rule: `github_token` and a model `api_key` are a literal or an `env:VAR` reference, resolved at boot, never serialized back to TOML, and never returned through the API. A control-plane password follows it.

The pane is a browser page. Its live transcript arrives over `new EventSource('/sessions/<id>/events')` in `crates/bosun-control/src/ui/index.html`, and an `EventSource` cannot carry a custom header.

## Decision Drivers

- Every request is checked, on every route, including the fallback route and the MCP OAuth callback route. A route that answers without the credential is a hole in the lock.
- People and nodes present the same credential, and daily use gains no step: no login page to visit, no token to paste per command.
- One module builds and verifies the credential, so the control plane, the CLI and the node cannot drift apart.
- The change is middleware and config. It adds no table, no endpoint, and no change to any existing route's shape.
- Boot refuses a configuration that would serve unguarded, rather than starting an open control plane and hoping the operator notices.
- No secret appears in a log line, a response body, or `bosun config get`.
- The party that decides whether a credential may cross the network in cleartext is the party handing it over, so the cleartext check lives where the credential leaves the machine.

## Options Considered

- **One shared password presented as HTTP Basic by every client (chosen).** `bosun_common::auth` builds the header, `require_password` checks it on every request, and the password is configured once per deployment.
- **Per-user accounts, with each session owned by its creator and every route authorizing against that owner.** Rejected: the deployment has one operator, so a second identity buys nothing today, and the whole cost of accounts is paid now against a requirement that does not exist — a users table, a login surface, an owner column on every session row, an authorization check on every route, and a rule for who may see a node's directories. The lock is one secret for one person; identity and per-session authorization arrive when a second person needs the same control plane.
- **A bearer header, a login page, and a session cookie.** Rejected: the pane's `EventSource` cannot set an `Authorization` header, so the cookie would be the real transport, and that pulls in a login page, cookie lifetime, logout, CSRF handling and server-side session state to replace a browser prompt the browser already implements and caches per origin.
- **Separate secrets for people and nodes.** Rejected: the two holders reach the same routes, so separate secrets do not separate privileges — a node's secret opens the pane and a person's secret takes a node's queued commands. It doubles the configuration surface and hides the fact that one secret covers the whole surface, which is the cost this ADR states instead.
- **Refusing a non-loopback bind without TLS.** Rejected: the control plane cannot gate a browser, which sends the credential the moment it is prompted for it, and it cannot see whether a client's path to it is encrypted. The decision belongs to whoever holds the credential: the node and the CLI each refuse an `http` URL whose host is neither a loopback address nor `localhost`, and each takes `--insecure` to override it. The control plane warns on a non-loopback bind and starts.
- **Guarding the human surface only, and leaving `/poll` and the tunnel route open.** Rejected: `/poll` carries the per-node command queue — clone, dev, dirs, stop — so an open `/poll` lets any caller claim a node name and take that node's commands, and an open tunnel route lets any caller attach to a session. Guarding only what a person reads leaves the driving of nodes and sessions open.
- **Rate limiting, a fixed delay, or a lockout on failed attempts.** Rejected: a delay holds a task per request, so an attacker who can reach the port parks the control plane's own request handling, and a lockout lets anyone who can reach the port lock the operator out of their own control plane. A failed credential logs at `warn` and the connection closes; the strength of the secret is the protection.
- **Per-node identity, so a node proves which node it is.** Rejected: it adds a per-node secret, a provisioning step for every node, a table to hold them, and a rotation problem for each one, and it still leaves the node name in `/poll`'s body as the claim of identity. It becomes worth that cost when a node is not the operator's own machine, which is not the case here.

## Decision

`ControlConfig.password` holds the secret: a literal, or an `env:VAR` reference resolved at boot by the same rule as `github_token`. `bosun serve` refuses to start when it is unset, empty or unresolvable, with a message naming the field and an example. A non-loopback `listen_addr` with neither `tls_cert` nor `tls_key` warns at boot and the control plane starts.

`AppState.password` carries the resolved value, and `require_password` — an axum middleware in `crates/bosun-control/src/api.rs` — checks every request before any route runs. It covers `/`, `/ui`, `/sessions`, `/sessions/{id}`, `/sessions/{id}/events`, `/poll`, `GET /tunnel/node/{node}`, the fallback route and the MCP OAuth callback route. A refusal is `401` with `WWW-Authenticate: Basic realm="bosun"`, and the middleware sits inside the version-header layer, so a refusal still carries `X-Bosun-Version`. A request with no credential logs at `debug`, because that is a browser's normal first request; a credential that does not match logs at `warn` with the path. Neither logs the value.

HTTP Basic is the whole mechanism, and `bosun_common::auth` is the only place that builds or checks it. `authorization(password)` returns the header value, `authorized(header, password)` checks one, and `client_headers(password)` builds the default headers for the reqwest clients, so every call site presents the credential without an edit. The client sends the username `bosun` and the verifier ignores it, so the password is the whole secret. The comparison is constant time and over bytes, so a password holding `:` or non-ASCII text works, and a refusal says nothing about how much of the password matched.

The browser needs no change. A browser answers the challenge with its own prompt, caches the credential for the origin, and attaches it to every same-origin request, including the pane's `EventSource` stream, which cannot set a header of its own. The pane's HTML and JavaScript carry no authentication code.

`CliConfig.password` holds the CLI's copy. `bosun config set|get|unset password` manages it, and `bosun config get` reports whether one is set and never its value. The CLI resolves the password from the stored value, then from `BOSUN_PASSWORD`, and `cp_client()` attaches it as a default header, so `bosun open` and every other command authenticate unchanged. A `401` is reported as the control plane requiring a password.

`NodeConfig.password` holds the node's copy, resolved by the same rule. `bosun node` refuses to start when it is unset, empty or unresolvable, naming the field, and the poll client's default headers and the tunnel handshake both carry it. A `401` from `/poll` is not fatal: the node logs a warning and keeps polling, so its sessions stall and it heals when both sides agree again.

`--insecure` is the one way to permit cleartext, and it is a flag on `bosun node` and on every CLI command that reaches the control plane, beside that command's `--cp-url`. An `http` `cp_url` whose host is neither a loopback address nor `localhost` is refused without it, at boot on the node and when the command starts in the CLI, with a message naming the flag; an `https` URL and a loopback host need no flag. Neither `node.toml` nor the CLI config carries a field for it.

## Consequences

- One secret covers people and nodes, so a node's config file is as sensitive as the control plane's, and a leaked node config opens the pane.
- The secret is plaintext at rest, in the config file or the environment, like `github_token` and every model `api_key`.
- There is no brute-force protection: no rate limit, no delay, no lockout. An attacker who can reach the port tries passwords as fast as the connection allows, and only the secret's own strength resists it.
- A browser pointed at an `http` control plane on a non-loopback address sends the password in the clear. The control plane warns at boot and serves the request anyway; nothing stops it.
- `/poll` still trusts the node name in the request body, so any holder of the password can claim any node name and take that node's queued commands.
- There is no rotation protocol. Changing the password means editing the config on the control plane, on every node and on every client, and restarting them. A node whose password no longer matches stalls: it warns on each `401` and keeps polling rather than exiting.
- `bosun config unset` takes a key: `bosun config unset cp-url` resets the stored URL and `bosun config unset password` forgets the stored password.
- Every route is checked, including the fallback route and the OAuth callback route, so no endpoint answers an unauthenticated caller.
- A control plane that cannot resolve its password refuses to start, and a node that cannot resolve its password refuses to start, so a misconfiguration is a boot failure rather than an open port or a node that silently never polls.
- A node refuses to hand the password over an unencrypted link to a host that is not loopback unless the operator passes `--insecure`.
- No secret is logged or returned: `bosun config get` reports only whether one is set, the default header map marks the value sensitive, and no route carries it in a response.
- The pane keeps working with no code change, because the browser's prompt and its per-origin credential cache are the whole client side of the mechanism.

## Revisit When

- More than one person needs one control plane. The lock then becomes identity, with per-session ownership and authorization on the routes that read or drive a session.
- Nodes are not all the operator's. Per-node identity, so a node proves which node it is, then earns its per-node secret, provisioning step and revocation path.
- The control plane is reachable from an untrusted network. Brute-force protection, enforced TLS rather than a boot warning, and secrets encrypted at rest are then required rather than optional.
- The password must change without restarting the control plane and its nodes. A rotation protocol then replaces editing each config and restarting both sides.
