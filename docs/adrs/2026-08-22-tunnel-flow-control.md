# ADR: Flow-controlled logical connections in the session tunnel

**Date:** 2026-08-22
**Author:** Raghav

## Context

The session tunnel carries multiple logical connections over one outbound socket. Each logical connection gets a bounded queue on the reader side (32 frames, about 256 KiB). When a burst — for example two concurrent downloads over `curl --parallel` — fills a queue, the reader silently drops the connection and its queue sender, so the peer's stream hits EOF or hangs with no close frame. The writer side fails the same way: `poll_write` errors on a full shared queue instead of backing off. In production, the second of two concurrent streams stalled after roughly 800 KiB, the combined capacity of the writer queue, socket buffers, and the per-connection queue.

## Decision Drivers

- A slow consumer must pause the producer, never lose data or kill the connection.
- One slow connection (a paused terminal WebSocket, a throttled download) must not stall the other connections in the same tunnel.
- Memory per connection must stay bounded and independent of OS socket buffer sizes.

## Options Considered

- **Per-connection flow control (chosen).** Each side grants a byte window per connection; the sender puts at most that many bytes in flight, and the receiver sends credit as its application consumes. Mirrors HTTP/2 flow control, so the behaviour is well understood.
- **Enlarge the queues and block the writer on the shared queue.** Rejected: the reader still cannot apply backpressure to one sender selectively, and the safe queue size depends on TCP buffer tuning, so it is not deterministic.
- **Pause reading the whole tunnel socket when any connection's queue is full.** Rejected: one slow connection would head-of-line block every other connection in the tunnel, breaking fault isolation.
- **Drop frames silently but keep the connection open.** Rejected: it corrupts the stream while looking healthy.

## Decision

Each logical connection is flow-controlled in both directions. A sender may put at most 512 KiB (`INITIAL_WINDOW`) of unacknowledged data in flight; its `poll_write` parks when the window is exhausted and resumes when the receiver grants more credit. The receiver grants credit as the application consumes data, batching into a `WindowUpdate` frame every 256 KiB. The per-connection inbound queue is sized to hold a full window of frames. A queue overflow that survives flow control is now a protocol violation and tears the whole tunnel down loudly, instead of silently killing one stream.

## Consequences

- Concurrent streams over one tunnel complete; the producer backs up to `opencode serve` when a consumer is slow, and no data is lost.
- Memory per connection is bounded by the window (512 KiB in flight plus the same again buffered on the receiver).
- The tunnel wire protocol gains a `WindowUpdate` frame, so a node and control plane running different versions cannot share a tunnel. The two ship together and the node reconnects on failure, so this is acceptable at this stage.
- Throughput on one connection is paced by window/RTT: credit of 256 KiB per node-to-control round trip.
- A flow-control violation is fatal to the whole tunnel, so all connections in one session fail together rather than one stream hanging.

## Revisit When

- The node-to-control round trip grows (a distant node) and 256 KiB of credit per round trip becomes the throughput limit — a larger window or adaptive credit is then worth it.
- A session routinely runs many concurrent connections, and 512 KiB of buffer per connection adds up — a shared connection-level window is then worth it.
- The tunnel needs a rolling upgrade path between versions.
