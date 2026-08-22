# ADR: Single-port path routing for sessions

**Date:** 2026-08-20
**Author:** Raghav

> Updated by `2026-08-21-nodes-dial-out-only.md`: the gateway's map points at each session's outbound tunnel instead of the node's forwarder address. The single-port, path-prefix routing decision itself stands.
>
> Updated by `2026-08-22-session-subdomains.md`: the gateway also routes by the `Host` header, addressing each session at `<session-id>.<control-plane-host>`. The path-prefix route remains for IP and localhost setups.

## Context

`2026-08-18-byte-level-proxy-chain.md` decided the control plane opens one proxy port per session (`proxy_bind:<ephemeral>`), and the client connects with `opencode attach http://<cp>:<proxy_port>`. This makes hosting the control plane in Docker impossible: the person must publish one host port per session, and Docker exposes one container port per `-p` flag.

The opencode client builds request URLs by string concatenation: it appends each API path to the attach URL. So `opencode attach http://<cp>:8090/session/<id>` produces requests to `/session/<id>/global/health`, `/session/<id>/session`, `/session/<id>/event`, and so on. A path prefix in the attach URL survives on every request.

## Decision Drivers

- The control plane exposes exactly one port, so one Docker `-p` publishes it.
- The control plane stays the only endpoint the client needs to know.
- The control plane must not understand the opencode REST/SSE protocol; routing is by the HTTP path prefix, which is generic HTTP, not opencode-specific.
- Each session is isolated behind its own path prefix.
- The opencode terminal (PTY) feature upgrades a connection to WebSocket; the route must carry upgrades, not just ordinary HTTP.

## Options Considered

- **One proxy port per session (2026-08-18).** Rejected: cannot be published from one Docker container.
- **Host-header routing.** The client connects to `http://<session-id>.bosun.example` and the control plane routes by the `Host` header, forwarding bytes untouched. Rejected: needs a wildcard DNS record per deployment, and still requires reading the request line and headers to learn the host.
- **Protocol-aware routing by a query parameter or header.** Rejected: the SDK concatenates paths onto the URL, so a query string swallows the path; a custom header would need client changes.
- **External reverse proxy (nginx, Caddy) in front of per-session ports.** Rejected: adds a process to install and run for one user.
- **Path-prefix routing on the control plane's own port (chosen).** The `Gateway` in `crates/bosun-control/src/gateway.rs` strips the `/session/<id>` prefix, rewrites the request to origin form, and forwards the rest as bytes.

## Decision

The control plane's `listen_addr` serves the session routes on the same port as the control API. The `Gateway` keeps a map from session id to the node's `forwarder_addr`, rebuilt from heartbeats. A request whose path starts with `/session/<id>` is routed to that forwarder with the prefix stripped; the `Host` header is set to the forwarder address, so the request line is origin form. Requests and responses stream as bytes. A `Connection: upgrade` / `Upgrade: websocket` request is forwarded, the forwarder's `101` headers are relayed, and the two upgraded streams are bridged byte for byte.

`bosun spawn` and `bosun open` print `opencode attach http://<cp>/session/<id>`. The `proxy_bind` config is gone.

## Consequences

- One port serves the control API and every session route; Docker publishes `listen_addr` once.
- The control plane now inspects the HTTP request line and headers enough to route and rewrite the path. It still does not parse the opencode API, so swapping the agent server later stays cheap.
- The byte-level guarantee is narrowed to the hops after routing: the control plane rewrites the request target once, then copies bytes.
- A session whose forwarder lags or drops the connection yields `502` at the gateway until the next heartbeat re-points the target.

## Revisit When

The control plane needs to understand sessions — to replay events or enforce policies. Protocol awareness then moves into the gateway deliberately.
