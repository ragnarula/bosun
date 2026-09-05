# ADR: Tool calls ride the session tunnel as HTTP/1.1

**Date:** 2026-08-30
**Author:** Raghav

> Superseded by `2026-09-05-in-process-executors.md`: tool calls ride the tunnel as typed frames, not HTTP/1.1 over a loopback-dialed executor port. The relay and the executor no longer exist as described here.
>
> Superseded in part by `2026-09-03-one-tunnel-per-node.md`: the tool protocol above survives unchanged; a logical connection is now opened on the node's tunnel, addressed by session id, instead of on the session's tunnel.

## Context

Nodes dial the control plane only (see `2026-08-21-nodes-dial-out-only.md`), so the control plane cannot reach the executor directly. The MVP's byte-level proxy spoke the opencode wire protocol through the tunnel. Sprint 002 replaces opencode with the executor, so the tunnel's payload protocol is now ours to choose.

## Decision Drivers

- Nodes must keep dialing out only; no inbound ports.
- One tunnel per session must carry every tool call without interleaving them.
- Tool calls are request/response with streaming bodies, and must support cancellation mid-stream.

## Options Considered

- **Carve out a custom multiplexed tool channel.** A new frame type per tool call duplicates what HTTP already does (method, path, status, headers, streaming body, error codes) and invents a second protocol to debug.
- **HTTP/1.1 over a logical tunnel connection (chosen).** The existing tunnel already multiplexes byte streams. The agent loop opens a logical connection on the session's tunnel and speaks HTTP/1.1 over it; the node relay dials the executor on loopback and bridges bytes. The executor is a plain HTTP server, so it is testable with any HTTP client and needs no tunnel code of its own.

## Decision

Every tool call is one HTTP/1.1 request carried over a fresh logical connection of the session's tunnel. The control plane opens the connection, sends `POST /tool/<name>`, and streams the response body. Shell output streams back over the connection as SSE events. Cancelling a running tool is `POST /tool/{id}/cancel`, sent on a second logical connection; the executor kills the underlying process, which ends the first stream. The node relay is unchanged from the opencode era: it copies bytes between the executor port and each logical connection.

## Consequences

- The executor is an ordinary HTTP server with no knowledge of the tunnel.
- The control plane's tunnel registry keeps serving only one role: opening connections.
- Each tool call pays the cost of an HTTP handshake, which is negligible next to a shell or a model call.

## Revisit When

A tool call must survive the control plane reconnecting its tunnel mid-request, or per-tool QoS is needed.
