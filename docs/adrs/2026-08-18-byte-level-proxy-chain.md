# ADR: Byte-level proxy chain, one control-plane port per session

**Date:** 2026-08-18
**Author:** Raghav

> Superseded by `2026-08-20-single-port-path-routing.md` for the control-plane ports, and by `2026-08-21-nodes-dial-out-only.md` for the node forwarder. The byte-level chain through the control plane remains: the gateway copies opaque bytes.

## Context

`opencode serve` listens on `127.0.0.1` by default and the architecture keeps it there. The person's opencode client must reach a session on a node across the network, through the control plane, without ever addressing the node directly.

## Decision Drivers

- The control plane stays the only endpoint the client needs to know.
- The control plane and node must not have to understand the opencode REST/SSE protocol.
- Each session is isolated behind its own port.

## Options Considered

- **opencode on `0.0.0.0`, client connects directly.** Rejected: every node becomes a reachable endpoint, there is no single entry point, and the person chose proxying through the control plane.
- **Protocol-aware single-URL routing by session id.** Rejected: it forces the control plane to parse the opencode REST/SSE API, coupling Bosun to opencode's protocol.
- **Byte-level chain (chosen).** The node opens a forwarder on its advertised address per session, while opencode stays on loopback. The control plane opens one proxy port per session bound to `proxy_bind`. Both forwarders copy bytes without inspecting them.

## Decision

The connection path is: opencode client to control-plane proxy port, to node forwarder, to opencode on `127.0.0.1`. The control plane and node treat the streams as opaque bytes. There is one control-plane port per session.

## Consequences

- Each hop carries one TCP connection at a time; session listing through the proxy is whatever opencode itself offers.
- Control-plane proxy ports are reallocated when the control plane restarts, so connect URLs can change.
- No protocol work is needed to replace opencode with another agent server later.

## Revisit When

The control plane needs to understand sessions — to replay events or enforce policies. Protocol awareness then moves into the proxy deliberately.
