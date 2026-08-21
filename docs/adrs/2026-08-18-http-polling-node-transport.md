# ADR: Plain HTTP and polling between node and control plane

**Date:** 2026-08-18
**Author:** Raghav

> Superseded by `2026-08-21-nodes-dial-out-only.md`: the node's control listener is gone, and commands flow node to control plane by long polling.

## Context

The architecture brief names NATS JetStream as the transport: a task queue for nodes to pull from and ordered event streams with sequence numbers for replay. The MVP has one control plane and one or two nodes.

## Decision Drivers

- Fewest running processes. A broker is a third thing to install, start, and debug.
- The node must lose nothing when a heartbeat is missed; state must come from what actually runs.
- The MVP has no event-replay requirement, because the opencode client talks to the session directly.

## Options Considered

- **NATS JetStream (architecture).** Ordered event log and queue for free, ready for a fleet. Rejected for the MVP: it adds a broker process, and the MVP has no event stream to store.
- **Plain HTTP with polling (chosen).** The node polls the control plane on a timer; each poll is a heartbeat that carries registration and session reports. The control plane calls node endpoints for spawn and stop.

## Decision

Node-to-control-plane communication is HTTP. Heartbeats flow node to control plane on a timer. Commands flow control plane to node over HTTP against the node's control listener. There is no broker.

## Consequences

- There is no durable ordered event log. Sessions the client missed are not replayed by Bosun; the opencode client reconnects itself.
- Each node runs one small HTTP server for commands, in addition to its heartbeat loop.

## Revisit When

The MVP needs durable ordered event delivery, or the node count grows beyond what one registry holds.
