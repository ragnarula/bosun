# ADR: One tunnel per node, addressed by session id

**Date:** 2026-09-03
**Author:** Raghav

> Superseded in part by `2026-09-05-in-process-executors.md`: the relay no longer dials a session's executor port on loopback. Executors run in the node process, and the relay dispatches each logical connection's typed tool call to the session's in-process executor. The tunnel transport, registry, session-addressed Opens, and flow control survive unchanged.

## Context

A node hosts a tree of sessions, and every session runs its own executor on the node. Until now each session kept its own outbound tunnel to the control plane, opened when its executor started and torn down when the session stopped. A session tree therefore multiplies connections: a root with several live children holds several tunnels from one node. Session teardown and node restarts manage a tunnel per session, and a control-plane restart that drops the registry is repaired only by whatever sessions still have tunnel tasks running.

## Decision Drivers

- A node holds one outbound connection to the control plane, no matter how many sessions run on it.
- A tool call must still reach the executor of the session that issued it.
- A session's teardown, and a node gaining or losing sessions, must not disturb the node's other sessions' traffic.
- A dropped tunnel must restore every session on the node at once, and the node must re-establish it without any per-session nudge.
- Flow control stays per logical connection (`2026-08-22-tunnel-flow-control.md`), and a protocol violation still tears the whole tunnel down.

## Options Considered

- **One tunnel per node, session id on the wire (chosen).** The node opens one tunnel at boot and keeps it for life. Every logical connection the control plane opens names the session it serves; the node relay dials that session's executor port on loopback and bridges bytes, as `2026-08-30-tool-protocol-over-tunnel.md` already does for one session. This is the sprint decision "transport is one tunnel per node".
- **Keep one tunnel per session.** Rejected: a session tree multiplies connections, and session teardown and control-plane restarts each manage one tunnel per session. A tunnel a node reconnects at boot, independent of sessions, is strictly less machinery.
- **Carry the session id in a new frame type instead of extending Open.** Rejected: an Open that carries no session id cannot be relayed, so an id-less Open is a protocol error; a separate frame type would duplicate the Open semantics for no gain.
- **Reuse the tunnel's connection id as the session id.** Rejected: one session runs many concurrent tool calls, so the connection id must stay a per-tunnel multiplexing counter.

## Decision

Each node holds one outbound tunnel to the control plane. The node starts it at boot, before any session exists, and it reconnects on its own until the node exits; sessions never start or stop it.

The Open frame's payload, empty until now, carries the session id as UTF-8 bytes. The control plane keys its tunnel registry by node name; a tool call resolves the session's node from the session row and opens a logical connection on that node's tunnel, addressed with the session id. The node's relay looks up the session's executor port in its session state, dials `127.0.0.1:<port>`, and bridges, per `2026-08-30-tool-protocol-over-tunnel.md`. A session this node does not run, or an executor that does not answer, closes the connection instead of attaching, so the tool call fails rather than hangs.

The registry's unregister is identity-checked: a closing tunnel removes itself only while it is still the node's registered tunnel, so a stale close from a replaced connection cannot drop a newer tunnel for the same node.

Sessions reconnect together: executors are node processes independent of the tunnel, and the relay resolves the executor port afresh for each opened connection, so a dropped or violated tunnel costs only the in-flight tool calls; the node reconnects and every session's next call works again. Flow control stays per logical connection and a protocol violation still marks the whole tunnel dead.

This supersedes the tunnel sections of `2026-08-21-nodes-dial-out-only.md` (one connection per session, `GET /tunnel/session/<id>`) and `2026-08-30-tool-protocol-over-tunnel.md` (a connection opened on "the session's tunnel"): the upgrade path is now `GET /tunnel/node/<name>`, and the logical connection carries the session id instead of the tunnel being keyed by session. The frame codec, per-connection flow control, and relay survive unchanged in shape.

## Consequences

- A tree of sessions holds one connection per node instead of one per session; session teardown removes no registry entry.
- A control-plane restart needs no per-session repair: the node's tunnel task predates and outlives every session and reconnects on its own.
- The control plane resolves the session's node from the store per tool call, so it needs no separate session-to-node map to keep consistent.
- Node and control plane must ship together again: the Open frame payload is a wire change, like the `WindowUpdate` frame before it.
- A session whose node is down fails a tool call with the same "session has no live tunnel" error as before, because the registry lookup is by the session's node.

## Revisit When

- Sessions move between nodes at runtime, or the control plane must reach a node without a session row.
- A tool call must survive the tunnel reconnecting mid-request, instead of failing and being retried by the model.
