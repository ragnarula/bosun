# Sprint 012 — A password on the control plane

Every route on the control plane is open. The router's only middleware adds a version header, `/poll` trusts the node name in the request body, and the tunnel route accepts any caller, so anyone who can reach the port can start sessions, read and drive any transcript by id, browse a node's directories, register a node, or take over another node's queued commands. This sprint puts one shared password in front of the whole HTTP surface — the pane, the session API and the node routes — configured in `serve.toml` or the environment, presented by the CLI, the terminal client and every node, and required at boot. It is a lock on a single-owner control plane: no users, no per-session ownership, no authorization.

Status: **complete**. All five stories are implemented and tested.

> The secret's shape, the surface it guards and the refused alternatives are recorded in `../adrs/2026-09-20-shared-password-on-the-control-plane.md`.

## Confirmed decisions

- **One password guards the whole surface, and nothing finer.** The pane, the session API, `/poll` and the tunnel upgrade require the same secret, presented by people (browser, CLI, terminal client) and by nodes alike. There is one secret per deployment. No users table, no ownership on sessions, no authorization: this is a lock, not a user system.
- **HTTP Basic, and nothing else.** A browser shows its own prompt, caches the credential for the origin, and attaches it to every same-origin request — including the pane's `EventSource` stream, which cannot carry a custom header. The username is ignored; the password is the whole secret. A refusal is `401` with `WWW-Authenticate: Basic realm="bosun"`.
- **The comparison is constant time**, so a refusal leaks nothing about how much of the password matched.
- **The control plane refuses to start without a password.** Boot fails naming the field, beside the persona validation that already runs at boot.
- **A secret is a literal or an `env:VAR` reference**, resolved at boot by the same rule as `github_token` and a model `api_key`. A reference that resolves to nothing is a boot failure, not an open control plane.
- **The node refuses to start without a password, and refuses cleartext unless told.** A `cp_url` with the `http` scheme whose host is neither a loopback address nor `localhost` fails at boot unless `--insecure` is given; `https` or a loopback host needs no flag. The control plane does not refuse a non-loopback bind without TLS: it warns and starts, because the party that decides is the one handing over the credential, and a browser cannot be gated at all.
- **A 401 after a successful start is not fatal to the node.** The node keeps polling and warns, so its sessions stall rather than die, and it heals when both sides agree again.
- **No rate limiting, no delay, no lockout.** A credential that does not match logs at `warn`; a request with no credential logs at `debug`, because that is a browser's normal first request. A delay on the auth path lets an attacker park a task per request, and a lockout lets anyone who can reach the port lock the operator out.
- **The version header survives a refusal**, so an outdated client still learns its version from a 401.
- **No secret is ever logged or returned.** `bosun config get` reports whether a password is set, never its value, and no route returns it.

## CLI surface

- `bosun config set password <value|env:VAR>`, `bosun config get` (reports only whether one is set), `bosun config unset password`. The CLI resolves the password from the stored value, then `BOSUN_PASSWORD`.
- `--insecure` on the node and on every CLI command that reaches the control plane, beside that command's `--cp-url`. It permits `http` to a host that is neither loopback nor `localhost`; without it such a `cp_url` is refused with a message naming the flag.
- A 401 from any command is reported as the control plane requiring a password, not as a bare status code.

## User stories in implementation order

- [x] **S1 — One module builds and verifies the credential**

As a developer, I want one module that builds and checks the credential, so the control plane, the node and the CLI cannot drift apart.

- `crates/bosun-common/src/auth.rs` holds `authorization(password) -> String` (the `Authorization` header value: `Basic ` and the base64 of `bosun:<password>`), `authorized(header: Option<&str>, password: &str) -> bool`, and `client_headers(password: Option<&str>) -> reqwest::header::HeaderMap` for the reqwest clients.
- The module carries a private base64 encoder and decoder and a constant-time byte comparison.
- The username is fixed by the client and ignored by the verifier; the password is compared as bytes, so a password holding `:` or non-ASCII text works.
- Tests: a built header verifies; a wrong password, a missing header, another scheme, malformed base64 and a decoded value with no colon all fail; a password containing `:` and a non-ASCII password round trip; the codecs match known vectors and reject bad input; `client_headers(None)` is empty, so an unconfigured client sends no header.

- [x] **S2 — Every control-plane request is checked**

As an operator, I want the control plane to refuse to start unguarded and to check every request, so an exposed port cannot be driven by anyone who can reach it.

- `ControlConfig.password: Option<String>` joins `github_token`: a literal or an `env:VAR` reference, never serialized back and never returned.
- Boot resolves it and fails with a message naming the field and an example when it is unset, empty, or unresolvable.
- `AppState.password` holds the resolved value and one middleware checks every request, including the fallback route and the OAuth callback route.
- The middleware sits inside the version-header layer, so a 401 carries `X-Bosun-Version`.
- A request with no credential logs at `debug`; a credential that does not match logs at `warn` with the path, never the value.
- Tests: boot without a password fails and with one starts; `/`, `/ui`, `/sessions`, `/sessions/{id}`, `/sessions/{id}/events`, `/poll` and `/tunnel/node/{name}` each answer 401 with no credential, 401 with a wrong password, and pass with the right one; a 401 carries the challenge header and the version header; no response body or log line carries the password.

- [x] **S3 — The CLI and the terminal client present it**

As a user, I want my CLI and terminal client to authenticate without new flags, so daily use is unchanged.

- `CliConfig.password` carries the secret; `bosun config set|get|unset password` manages it; `BOSUN_PASSWORD` works without touching the file.
- `cp_client()` attaches the credential as a default header, so every existing call site authenticates with no edit, and `bosun open` uses the same client.
- A 401 is reported as the control plane requiring a password.
- Tests: resolution order; `bosun config get` prints no secret; a stub control plane that refuses produces the clear error.

- [x] **S4 — The node authenticates and refuses cleartext unless told**

As an operator, I want the node to present the password and to refuse handing it over an unencrypted link, so a misconfiguration is loud rather than silent.

- `NodeConfig.password`; `bosun node` refuses to start when it is unset or unresolvable, naming the field.
- The poll client carries the credential as a default header, and the tunnel handshake sends the same header on its hyper request.
- `--insecure` decides the cleartext case: a `cp_url` with the `http` scheme whose host is neither a loopback address nor `localhost` fails at boot without it. `node.toml` gains no field for this; the flag is the one way.
- A 401 from `/poll` logs a warning naming the likely cause and keeps polling.
- Tests: the poll request and the tunnel request each carry the header; the cleartext check accepts loopback and `https`, refuses a non-loopback `http`, and accepts it with the flag; a 401 logs and the loop keeps polling.

- [x] **S5 — The docs and the samples that own the current state**

As a developer, I want the change stated where the project keeps its current state, so a reader meets it from the entry point.

- `docs/developer/config.md` gains the control-plane row, the node row and the CLI paragraph.
- `cmd/bosun/settings/serve.toml` and `cmd/bosun/settings/node.toml` show the field, so the documented quickstart works.
- The README's status paragraph and run block carry the password, and its "no security model" claim is replaced.
- `CLAUDE.md`'s current-state sentence names the shared password and points at this sprint.
- `../adrs/2026-09-20-shared-password-on-the-control-plane.md` records the decision, the refused alternatives with the reason each lost, and the costs.

## Acceptance measures

| Measure | Before | Target |
|---|---|---|
| A control plane with no password | starts and serves every route | refuses to start, naming the field |
| A request with no credential | served | 401 with the Basic challenge |
| A request with a wrong credential | served | 401, logged at `warn` |
| Browser access to the pane and its live stream | open | one native prompt, then unchanged, with no edit to the pane's HTML or JS |
| A node with no password | connects | refuses to start |
| A node sending a password over `http` to a non-loopback host | possible | refused unless `--insecure` |
| Secrets in logs, responses, or `bosun config get` | — | none |
| New tables, endpoints, users, or per-session ownership | — | none |

## Out of scope

- No per-user identity: no users table, no ownership on sessions, no authorization on any route. One password is one lock, and every holder can drive every session and every node.
- No per-node or per-client secrets: a node's config holds the same secret as the pane, so a node's config file is as sensitive as the control plane's.
- No node identity: `/poll` still trusts the node name in the body, so any holder of the password can claim any node name and take its queued commands.
- No rate limiting, delay, or lockout on failed attempts.
- No logout, no session cookie, no login page: the browser's prompt and its credential cache are the whole mechanism.
- No password rotation protocol: changing it is editing the config and restarting both sides.
- No TLS enforcement: a non-loopback bind without TLS warns, and TLS stays the operator's choice through `tls_cert`/`tls_key` and the clients' `ca_cert`.
- No encryption at rest: the secret sits in the config file or the environment, as every other secret in the project does.
