# ADR: Nodes are the source of truth for sessions

**Date:** 2026-08-18
**Author:** Raghav

## Context

The architecture says the control plane is the only component that stores state. In the MVP, sessions run on nodes: a node cloned a repo, started `opencode serve`, and opened a forwarder. A control plane that restarts must not lose track of those sessions, and a node that restarts must not lose the work in its checkout.

## Decision Drivers

- A control-plane restart must not orphan running sessions.
- A node already knows its own sessions — which directories exist and which processes it started.
- No database to operate for a single user.

## Options Considered

- **SQLite on the control plane.** Persists sessions and the proxy-port mapping. Rejected: the state can drift from what actually runs on nodes; after a reboot the mapping may point at sessions that no longer exist, or miss ones that do.
- **In-memory registry rebuilt from heartbeats (chosen).** Nodes report their sessions in every heartbeat. The control plane replaces its whole view of a node on each report.
- **Sessions die with the node.** Rejected: a machine reboot would lose a checkout mid-work.

## Decision

The node is authoritative for which sessions exist and their states. The control plane keeps an in-memory view rebuilt from heartbeats. Each node persists its own session list to a state file next to its work, and restarts its `opencode serve` processes from that file when it boots. The control plane needs no durable state.

## Consequences

- A heartbeat that lags shows stale sessions for up to one interval.
- Control-plane proxy ports are reallocated on restart, so the person re-reads the connect URL from `bosun list`.
- A node reboot is handled by the node itself: it restarts its sessions and re-advertises them on the next heartbeat.

## Revisit When

The control plane must answer queries while no node is connected, or session records must survive without their node.
