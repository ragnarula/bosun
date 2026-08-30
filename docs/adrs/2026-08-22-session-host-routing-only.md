# ADR: Session host routing only

**Date:** 2026-08-22
**Author:** Raghav

> Superseded by `2026-08-30-tool-protocol-over-tunnel.md` and the removal of the opencode client: the gateway no longer exists. Sessions are driven through the session API and the terminal client; tool calls ride the tunnel as HTTP/1.1.

> Supersedes the path-prefix routing in `2026-08-20-single-port-path-routing.md` and the path-prefix fallback kept by `2026-08-22-session-subdomains.md`.

## Context

The gateway routed sessions two ways: by `Host` header (`<session-id>.<control-plane-host>`, per `2026-08-22-session-subdomains.md`) and by path prefix (`/session/<id>`, with the prefix stripped). The path prefix is ambiguous with opencode's own API: `opencode serve` exposes `/session` and `/session/<id>` paths, so any such request to a session's subdomain was hijacked by the prefix router and forwarded to the wrong session with a mangled path. The path prefix only served control planes reachable at a non-loopback IP, where no wildcard DNS exists.

## Decision Drivers

- opencode's own `/session` and `/session/<id>` API paths must reach the node's opencode server unchanged.
- The control plane stays protocol-agnostic: routing is by the generic `Host` header, never by opencode's API paths.
- One session is addressed one way, so there is no ambiguous fallback to maintain.

## Options Considered

- **Host routing only (chosen).** The gateway routes purely on the `Host` header and forwards the path unchanged. Loopback control planes use `<session-id>.localhost`, which every browser and operating system resolves without DNS. A non-loopback IP control plane has no wildcard DNS, so its sessions have no address; the CLI says so instead of printing a path form that no longer routes.
- **Path-prefix routing only.** Rejected: it breaks the web UI, which uses root-absolute asset and API paths, and it cannot reach opencode's `/session` API without ambiguity.
- **Both, with path routing removed only under a subdomain host.** Rejected: the ambiguity is in the path itself; keeping the prefix under any host keeps the collision.

## Decision

The control-plane gateway routes a request purely by `Host`. A request whose `Host` header begins with `<session-id>.` opens a connection on that session's tunnel and forwards the request unchanged, preserving the `Host` header, so the node's opencode server sees the session's subdomain as its origin. Any other host returns `404`. The path-prefix route `/session/<id>` and its prefix stripping are removed, so `/session` and `/session/<id>` are opencode's own API paths and reach the tunnel unchanged.

`bosun clone` and `bosun open` print the attach command for the subdomain, and use `<session-id>.localhost` for loopback control planes. A control plane reachable only at a non-loopback IP prints a message explaining that its sessions have no address.

## Consequences

- opencode's `/session` and `/session/<id>` API paths work through the tunnel.
- The web UI works without rewriting opencode's assets or API calls, as before.
- The deployment still needs wildcard DNS and a wildcard certificate for the control-plane host.
- Sessions are unreachable from a control plane addressed by non-loopback IP: an IP host cannot carry a session subdomain.
- The terminal client and the web UI share the same session subdomain.

## Revisit When

- A single control-plane host must serve more than sessions — for example a control-plane web UI of its own. Host routing then needs a configured domain to tell session subdomains apart from control-plane hosts.
- Sessions must be reachable from a control plane addressed by IP. A wildcard DNS service such as `sslip.io`, or explicit per-session routing, is then needed.
