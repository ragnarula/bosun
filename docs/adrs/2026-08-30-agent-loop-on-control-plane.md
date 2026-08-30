# ADR: The agent loop runs on the control plane

**Date:** 2026-08-30
**Author:** Raghav

## Context

The MVP delegated all agent behaviour to `opencode serve`, which ran per session on a node. The control plane only proxied client traffic. Sprint 002 makes Bosun run its own agent loop so the opencode dependency can be removed: the loop drives a provider API directly, tool calls execute on the node, and a terminal client and web pane are thin clients of one protocol.

## Decision Drivers

- Sessions must keep working when the node that hosts their working copy reboots; the loop must be independent of node liveness.
- The control plane owns the provider credentials and model choice, so keys never travel to nodes and `bosun clone` does not need per-node provider setup.
- A terminal client and a web pane must show the same live transcript, so the transcript must live where both can reach it.

## Options Considered

- **Agent loop on the node (per session), control plane proxies.** Rejected: keys and model config would have to reach every node, session state would fragment across machines, and a node reboot would kill the agent even when its machine was idle.
- **Agent loop on the control plane (chosen).** One loop process per session on the control plane. The node only executes tools. The loop reads and writes the session store, calls the provider, and dispatches tool calls over the session tunnel.

## Decision

The agent loop runs on the control plane, one `tokio` task per session. It is driven by a per-session message channel: a user message, the initial prompt, or a resume request starts a turn. A turn builds one request from the system prompt, skill advertisements, tool schemas, and the transcript window; streams the completion; dispatches each tool call to the node through the session tunnel; and appends every message and model call to the SQLite store. The `ask` tool ends the turn in `waiting_for_input`; the client answers through the session API.

## Consequences

- A node reboot loses only the working copy's process: the executor restarts on the node and the loop's next tool call finds it again.
- The control plane holds all provider keys; a node needs none.
- The store must be on the control plane and the loop must survive control-plane restarts (see `2026-08-30-sqlite-session-store.md`).
- Interrupting or crashing the loop kills only the in-flight turn, never the loop task.

## Revisit When

Bosun must keep running a session while its control plane is down, or must run the loop on a machine closer to the working copy for latency reasons.
