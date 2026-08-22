# ADR: Session subdomains for the web UI

**Date:** 2026-08-22
**Author:** Raghav

## Context

The opencode web UI is a single-page app served by `opencode serve` at the origin root. Its HTML loads assets from root-absolute paths (`/assets/index-*.js`) and its JavaScript calls the API at `location.origin` (`/api/...`, `/global/...`). Behind `2026-08-20-single-port-path-routing.md`, a session is only reachable at the control plane under a path prefix (`/session/<id>`), so the browser's origin is the control-plane root and every web UI request misses the prefix. The terminal API works because the opencode client appends its own paths to the attach URL; the web UI cannot.

`2026-08-20-single-port-path-routing.md` rejected host-header routing because it needs a wildcard DNS record. The deployment now has wildcard DNS and a wildcard certificate (`*.bosun.on.21cs.biz`), so that obstacle is gone.

## Decision Drivers

- The web UI must be served at the origin root of its own host.
- The control plane stays one port and one endpoint to operate.
- The terminal API keeps working, and it can use the same subdomain.
- The control plane stays protocol-agnostic: it routes by `Host` and path prefix, never parsing opencode's API.

## Options Considered

- **Session subdomain per session (chosen).** Each session is addressed at `<session-id>.<control-plane-host>`. The wildcard certificate covers the subdomains. The gateway routes a request whose `Host` starts with `<session-id>.` to that session's tunnel, passing the path through unchanged and preserving the `Host` header. The web UI loads at the subdomain root, so its root-absolute assets and origin-relative API calls resolve against the subdomain origin. The terminal client attaches to the subdomain root and appends its API paths there. The path-prefix route `/session/<id>` is kept for clients that reach the control plane by IP or localhost, where no wildcard DNS exists.
- **Path-prefix routing only (status quo).** Rejected: the web UI's root-absolute assets and origin-root API calls cannot survive a path prefix without rewriting the served HTML or JavaScript, which couples the control plane to opencode's web build.
- **Cookie-selected active session.** Rejected: a browser cookie selects one session per browser, so two sessions cannot be open at once, and root-absolute asset URLs would route by cookie state, which is fragile.

## Decision

The control-plane gateway routes a request by host first and by path prefix second. A request whose `Host` header begins with `<session-id>.` opens a connection on that session's tunnel and forwards the request unchanged, preserving the `Host` header, so the node's opencode server sees the session's subdomain as its origin. A request whose path begins with `/session/<id>` is routed to that session's tunnel with the prefix stripped, as before.

`bosun clone` and `bosun open` print the attach command for the subdomain when the control-plane URL has a DNS host (`opencode attach https://<session-id>.bosun.on.21cs.biz`), and keep the path form for IP addresses and `localhost`.

## Consequences

- The web UI works without rewriting opencode's assets or API calls.
- The terminal client and the web UI share the same session subdomain.
- The deployment needs wildcard DNS and a wildcard certificate for the control-plane host, and the certificate must also cover the apex host, because the node's tunnel and polls connect to the control-plane host itself.
- A hostname that begins with a session id routes to that session, so guessing or knowing a session id is enough to reach the session — the same trust as the path form.
- The path-prefix route remains for local and IP-based setups.

## Revisit When

- A single control-plane host must serve more than sessions — for example a control-plane web UI of its own. Host routing then needs a configured domain to tell session subdomains apart from control-plane hosts.
- The web UI is served by a host that is not a subdomain of the control plane.
