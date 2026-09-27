# ADR: A spawn names the node and the directory a child runs on

**Date:** 2026-09-27
**Author:** Raghav

## Context

Sessions form a tree of full sessions: a parent's `spawn(persona, instructions)` starts a child that runs its own loop and executor, holds its own transcript, and reports to its parent by authoring an event. That is recorded in `2026-09-03-agent-tree.md`. The child always started on its parent's node, in the parent's working copy, because the node command that starts a child's executor (`NodeCommand::Start`) took the parent's directory and nothing else. Only the user could put work on another machine, through `bosun clone`, `bosun dev`, or the pane's new-session sheet.

Nodes exist to be used: the operator registers them, `bosun nodes` and the pane show which are up, and each one hosts its own executors and its own filesystem. An agent with a large piece of work could not fan it out across those machines.

Constraints fixed going in:

- A node's `browse_roots` live in that node's own config file and are never reported to the control plane; the node enforces them when it resolves a directory, and it chooses a clone session's directory itself (`work_dir/<session id>`). Nothing in the control plane knows a node's filesystem layout or its work directory.
- A child stays a child: it reports through the live-children manifest, answers `message_child`, and is visible to `session_status`, wherever it runs.
- A spawn with no placement keeps today's behaviour exactly: the parent's node, the parent's working copy, no new decisions for a caller that names nothing.
- The tree has no cap and no tree-wide permission bound; a session's surface comes from its persona, and read-only sessions drop the mutating tools.

## Decision Drivers

- An agent must be able to put parallel work on the machines the operator has registered, not only on its own.
- The control plane must not learn a node's directories to place a child there.
- A caller-named directory has to be checked by the node that owns the roots, and refused there, because that node is the only place the answer exists.
- A caller that names nothing must get exactly what it gets today.
- The work has to compose with the tree: the child's relation to its parent cannot depend on where it runs.

## Options Considered

**1. Optional `node` and `dir` on `spawn`, the target node choosing the directory when none is named. (chosen)**

The tool gains two optional strings. `node` names any registered node that is up; `dir` names an existing directory on that node, which that node confines to its own browse roots. A command with no directory asks the node to make one under its own work directory, the same way a clone's directory is the node's to name.

**2. Operator-set allow-lists of nodes and directories for agents. (rejected)**

A list of nodes an agent may use, or of directories it may place children in, would bound the blast radius of a wrong decision. Rejected for now: the system is single-user with no authorization model, and the tree already has no creation-time cap — an agent may spawn without limit on its own node today. A node list would be the first half of a permission model and would be the wrong half to build alone; it belongs with the caps and approvals that no part of this system has.

**3. Nodes advertise their browse roots to the control plane. (rejected)**

The control plane could then choose a plausible directory itself, and answer "no" before queueing a command. Rejected because it publishes every node's filesystem layout, which the whole design keeps node-side, and because it is a much larger change for one cancellation: the node's own refusal already reaches the caller as a tool error, and the caller can correct the directory on the next call. It also ages badly: the roots a node advertises would be a snapshot of a config file the operator edits.

**4. "Pick a node with capacity" instead of naming a node. (rejected)**

The registry holds liveness, version, and update status only — no cores, no disk, no running-session count. A control-plane choice would therefore be health-based guesswork dressed as scheduling. If capacity-based placement is wanted, nodes must report capacity first, which is its own decision.

**5. A second tool beside `spawn` for cross-node work. (rejected)**

One concept, not two: a child on another node is an ordinary child session. A second tool would need its own placement rules, its own refusals, and its own place in the persona tool lists, and would drift from `spawn` at the first change to either.

**6. A read-only session may still spawn a read-write child. (rejected)**

The agent-tree decision allowed it, on the grounds that a read-only session's child's surface is its own persona's. With a placement, a spawn is no longer a local act: it puts work on a machine, in a directory, that the caller itself may not write to. A read-only session therefore drops `spawn` with the writing tools.

## Decision

`spawn(persona, instructions, node?, dir?)` starts a child session, always a child of the caller: `parent_id` names the caller, `owner_id` its tree root, and the child reports through the same manifest and answers `message_child` and `session_status` as any child does. Only placement differs.

With no `node`, the child keeps today's place: the parent's node, in the parent's working copy. A tool call with `dir` but no `node` is refused — `dir needs node: without a node the child starts in the parent's directory` — rather than silently starting somewhere else.

With `node`, the node must be in the registry and up. A name the registry has never seen is refused as `unknown node <name>`; a name it has seen and that has stopped reporting is refused as `node <name> is not up`. Both are answered before any command is queued, so a bad name costs no work on any machine.

With `dir`, the target node resolves the directory against its own `browse_roots`, exactly as `bosun dev` and the parent's-copy case already do: a directory outside every root, a missing one, and a file are refused by the node and reach the caller as the tool error `node <name> rejected the request: <the node's reason>`.

With no `dir` (and a named node), the node creates `work_dir/<child id>` and starts the child there. It is the node's own directory: `reapable`, so `stop` removes it, and restored on boot from the node's session state exactly as a clone's is. The control plane never learns the path — it takes the directory from the node's reply, as it already does for clone and dev.

A child that ends up in the parent's own directory keeps the parent's `repo_url` and `git_ref`; a child placed anywhere else keeps neither, so a row never claims a repository the directory does not hold.

A read-only session is not offered the tool and a call is refused by the loop with `spawn is not available in a read-only session`, matching how the root-only `todowrite` is refused. Advertisement already drops it: `canonical_tools` removes `spawn` from a read-only session's surface.

No cap is added. The tree has none, and placement does not change what one session may claim.

## Consequences

- An agent can now start work on every machine the operator has registered, in a directory that machine accepts, without the control plane ever learning a node's paths. This is what nodes were for.
- There is no allow-list and no cap, so a root that decides badly spends the operator's machines as well as its own node. The single-user scope accepts this; it is the first thing an authorization model should bound.
- A child on another node shares no working copy with its parent. A named directory holds whatever that node had there; a fresh one is empty, so a child that needs a repository must clone it itself or be handed the work in its assignment. The parent's edits are invisible to it.
- A placement the node refuses costs one round trip and reaches the model as a tool error, which is recoverable. The control plane still cannot tell in advance which directories a node will accept, and nothing here changes that; the refusal is the answer.
- An empty `browse_roots` no longer means "no child sessions on this node". It means "no caller-named child directory": a spawn with no directory still lands under the node's work directory. `docs/developer/config.md` states this.
- The node gains a second way to create a session directory (the work directory it names itself, alongside the clone path), with the same lifecycle: `reapable`, removed on stop, restored on boot.
- `NodeCommand::Start` now carries `Option<PathBuf>`, so a control plane and a node from different releases disagree about a directory-less start: an older node cannot parse it and refuses the command. Releases ship both together and nodes auto-update, so the skew is the same one every new field carries, but a fleet mid-update can see it.
- A child's placement is fixed when it starts. Nothing moves a running session between nodes, so a parent that later needs its child's files on its own machine has to ask for them.

## Revisit When

- An authorization model appears: an allow-list of nodes, a cap on children per tree, or an approval channel. The rejected option 2 becomes the natural shape for the first of those.
- Placement should be chosen rather than named — for example, "run this where there is capacity". That needs nodes to report capacity first.
- A node must advertise its browse roots or work directory to the control plane, which a directory picker in the pane would need.
- A child needs the parent's working copy across machines, which needs a shared volume, a copy step, or a repository both can fetch.
- A session must move between nodes, or a child must be re-placed after it starts.
