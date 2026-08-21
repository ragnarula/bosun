# ADR: Nodes dial the control plane only

**Date:** 2026-08-21
**Author:** Raghav

## Context

Nodes today run an HTTP control listener and a per-session TCP forwarder on their advertised address. The control plane reaches both by dialing the node: it posts clone, dev, dirs, and stop to the control listener, and it connects a socket to each session's forwarder to route opencode client traffic. Every node therefore accepts inbound connections, and when the advertised address is not loopback, those connections are reachable from the network.

That does not work for nodes that sit on machines whose networks the user does not control. Opening ports on nodes and making them reachable from the control plane is a security risk, and it fails behind NAT. Nodes must only ever make outbound connections, and those connections go to the control plane.

## Decision Drivers

- A node must never accept an inbound connection from the network; all its connections are outbound to the control plane.
- The control plane stays the only endpoint the opencode client needs to know, and it still exposes exactly one port.
- `opencode serve` stays on loopback on the node.
- The control plane must not understand the opencode protocol; it routes by path prefix and copies bytes, per `2026-08-20-single-port-path-routing.md`.
- The opencode client keeps several connections open to one session at once: health checks, an SSE event stream, and a websocket PTY. The transport must carry concurrent connections per session.
- Interactive directory browsing must not pay a full poll interval per step.

## Options Considered

- **Control plane dials the node (current).** The node exposes a control HTTP listener and a per-session forwarder on its advertised address. Rejected: it requires opening ports on nodes and making them reachable from the control plane, which is the security risk this ADR removes.
- **Long-polling for commands (chosen).** The node holds one outbound request open; the control plane answers it when a command is queued, or after a hold timeout. The node immediately sends the next poll. A queued command reaches the node in one network hop, not one poll interval.
- **Timer polling for commands.** The node polls every `heartbeat_interval_secs`. Rejected: every command pays up to the poll interval, and directory browsing would stall per step.
- **Merged heartbeat and poll (chosen).** The poll request carries the heartbeat payload: node identity, status, and sessions. Node liveness is the cadence of poll arrivals. One outbound mechanism replaces both heartbeats and command delivery.
- **Separate heartbeat and poll.** Rejected: two outbound mechanisms where one suffices.
- **Multiplexed frame tunnel for session traffic (chosen).** Each session keeps one persistent outbound connection from node to control plane, established by an HTTP upgrade on the control plane's single port. A length-prefixed frame protocol carries any number of concurrent logical connections. The control-plane gateway opens a logical stream per client connection and keeps its byte-bridging code.
- **Per-connection reconnecting tunnels.** The node keeps spare outbound connections, each carrying one client connection, and reconnects one when it closes. Rejected: it breaks when the opencode client opens more concurrent connections than the pool holds — an SSE stream, a websocket, and health checks at once — and it is racy while a spare is being refilled.
- **HTTP/2 server-initiated streams.** Rejected: clean in principle, but hyper cannot initiate a stream on an existing server connection, so the control plane could not open streams on the node's outbound connection without a different HTTP stack.
- **Broker transport (NATS JetStream).** Rejected in `2026-08-18-http-polling-node-transport.md`; it adds a process to install and run.

## Decision

Node-to-control-plane communication is outbound only. The node binds nothing on a non-loopback address.

Commands flow by polling. The control plane keeps an in-memory queue of commands per node: clone, dev, dirs, stop. The node runs one poll loop: it POSTs `/poll` to the control plane with its heartbeat payload plus the result of the previous command, and the control plane either answers with the next queued command or holds the request for `node_timeout_secs / 2` and answers empty. The node executes at most one command at a time and reports the result in the next poll. Node liveness is the cadence of poll arrivals; the `/heartbeat` endpoint is gone.

Session traffic flows over a tunnel. When a session starts, the node opens one persistent connection to the control plane with `GET /tunnel/session/<id>` and an HTTP upgrade, on the control plane's single port. The upgraded stream carries little-endian length-prefixed frames: a type byte, a 64-bit connection id, a 32-bit length, and a payload. The control plane sends `OPEN` when an opencode client connects; both sides send `DATA`; either side sends `CLOSE`. The node relays every opened connection to `127.0.0.1:<opencode_port>` on its own machine.

The gateway routes by `/session/<id>` as before, but instead of dialing the node's forwarder it opens a logical stream on the session's tunnel. Its path rewriting, `101` relay, and byte bridging are unchanged.

Wire and config follow: session records stop carrying `forwarder_addr`, heartbeats stop carrying `control_addr`, and `advertise_addr`, `listen_port`, and `heartbeat_interval_secs` leave the node config. The node's `state.json` keeps `pid` and the loopback `opencode_port`: the node needs the port to dial `opencode serve` itself.

## Consequences

- No node port is reachable from the network. Nodes work behind NAT and need no inbound firewall rules.
- The control plane is the single trust point: nodes run whatever commands it sends. This is the same trust as before, since the control plane dialed the node then.
- There is no authentication yet: any caller who can reach the control plane can enqueue commands, connect to sessions, or register a tunnel. A shared token can later be required on `/poll` and on tunnel establishment.
- The control plane now queues commands and holds polls, so it keeps transient in-memory state per command. A control-plane restart drops it, and a command enqueued but not yet polled fails as if the node were unreachable — the same outcome as today, when the registry is lost on restart.
- A dropped tunnel fails opencode client connections until the node reconnects and the client retries; the session itself keeps running on the node.
- The frame codec and the logical-stream plumbing are new code in `bosun-common`, `bosun-node`, and `bosun-control`. Gateway tests move from dialing a stub backend to feeding an in-memory logical stream.

## Revisit When

- The control plane must push to a node that is not polling — for example to broadcast a command with no poll waiting to carry it. The tunnel then carries control frames too.
- Real multi-user isolation or untrusted nodes require authentication and authorization on the control plane.
- Node count or message volume makes the in-memory command queue and per-session tunnels too heavy; a broker then earns its process.
