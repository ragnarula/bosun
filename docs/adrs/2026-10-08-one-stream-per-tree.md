# ADR: A client follows a whole session tree on one stream

**Date:** 2026-10-08
**Author:** Pete

## Context

A session tree is a root session and the children it spawned, all carrying the root's id in `sessions.owner_id`. Each session has its own durable event stream: `GET /sessions/{id}/events` replays the session's rows of the `events` table from a cursor, polls the store every 500 ms for newer rows, and adds the session loop's live text deltas. A client that wants to show what a whole crew is doing — a group chat of the lead and its children, a task list, the files they changed — would need one connection per member, opened and closed as children come and go.

`events.seq` is one `AUTOINCREMENT` column across every session, so the rows of several sessions already share one order.

## Decision Drivers

- One connection for a crew, however many members join while it is open.
- The resume behaviour of the per-session stream: `after=`, `Last-Event-ID`, and `tail=` with a `history` frame and a read-back route.
- No change to the per-session stream that the terminal client and the pane's Log view read.
- A bounded cost per poll, the same as one session's.

## Options Considered

**1. A tree-scoped stream beside the per-session one, sharing its code. (chosen)**

`GET /sessions/{id}/tree-events` serves every session whose `owner_id` is the tree root of `id`. Each durable frame adds the `session_id` that wrote it. `GET /sessions/{id}/tree-history` pages back the same way `history` does. The store reads either scope through one `EventScope`, so the replay, the tail page and the poll are one implementation.

**2. One per-session stream per member, opened by the client. (rejected)**

The client must learn about each new child from the session list, open its stream, merge frames by seq across connections, and close streams for removed children. A crew of five costs five connections and five pollers, and a browser limits concurrent connections per host.

**3. A message sender field and a stream of the root alone. (rejected)**

Copying each child's messages into the root's thread would double the transcript the root's model reads and pays for, and still leave the children's tool calls, activity and task changes out.

## Decision

- `Store::scoped_events_after` and `Store::scoped_events_page` read events for an `EventScope`: `Session(id)` or `Tree(root_id)`. The per-session `events_after` and `events_page` call them with `Session`.
- `GET /sessions/{id}/tree-events` accepts `after`, `tail` and `Last-Event-ID` exactly as `/events` does, resolves `id` to its tree root, and frames each durable event as `{seq, session_id, event}` with the seq as the SSE id. A tail counts the crew's messages, whoever wrote them.
- The root's live deltas arrive as `{delta, session_id}`. Children's deltas are not streamed: their messages arrive when the store writes them.
- `GET /sessions/{id}/tree-history?before=&messages=` pages back with the same frame shape.
- `/events` frames are unchanged.

## Consequences

- A crew view needs one connection, and a child spawned after it opened appears on the next poll with no work from the client.
- A poll of a tree runs one query with a sub-select over `sessions.owner_id`, which has no index. Trees are small and the table holds live sessions only.
- Children's text appears a message at a time, not token by token.
- A stopped tree's rows are removed with its sessions, so the stream of a stopped tree ends with nothing to replay.

## Revisit When

- Live deltas from children are wanted in the chat.
- The sessions table grows large enough that the owner sub-select shows in a poll's time: add an index on `owner_id`.
