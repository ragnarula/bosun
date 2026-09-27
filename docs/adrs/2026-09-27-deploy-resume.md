# ADR: A deployment resumes the sessions it interrupted

**Date:** 2026-09-27
**Author:** Raghav

## Context

Bosun is deployed by restarting the control plane and updating the nodes. Until now, every session that was mid-turn when that happened was stopped: `recover` marked each stored `running` or `creating` session `interrupted` with cause `crash`, started its loop, and started no turn. `2026-08-30-session-states.md` states the rule that follows: "An interrupted turn is never replayed: resuming always starts from a user message." A user who noticed could prod the session, and the loop answered the in-flight call with a synthetic result saying the effect was unknown, so the window was valid — but the turn's partial output was gone, and the session waited until someone acted.

A session's in-flight work is durable. A tool call is written to the thread before it is dispatched, so a thread that ends on a call with no result names exactly what was running. The loop's own turn structure already re-sends a turn from that thread: the window is the store's active messages, and a completion follows it. What was missing was permission and a wake.

On the node, a restart rebuilt executor states from `state.json`, and shells that were in flight were neither signalled nor recorded: they outlived the node with nothing left to report their exit to.

The operator's answers to the four open questions fixed the shape (issue #22, 2026-09-27): the control plane re-issues an interrupted call and a node waits for a re-issue rather than running anything itself; a child's interruption is its parent's to decide; there are no orphans, so in-flight shells are cleaned up when a node exits; and there is one attempt, no more. And, on marking: a deployment should not mark anything — sessions are either running or waiting for input, and a running session's work is simply re-run.

## Decision Drivers

- A deployment must not stop a session that was working. The user should notice nothing but the clients reconnecting.
- The control plane decides and records; a node executes what arrives. Nothing re-runs on the node's own initiative.
- The session's state must stay truthful: `running` means work is in flight or about to be, `waiting_for_input` means it is waiting for someone. A deployment is not a fact about a session, so it gets no state, no cause and no marker.
- A child's work is its parent's to decide, as it already is for a crash: the tree is what supervises, and the parent's model can weigh a half-finished review.
- A restart must not leave a command running that nothing can report on.
- The bound must be simple: one wake per session per boot.

## Options Considered

**1. Boot wakes each running session once, and the loop re-runs the work its thread names. (chosen)**

`recover` marks nothing for a session that was running. It starts the loops as it always did, sends one internal resume wake per running root session, and leaves `waiting_for_input` alone. On that wake the loop runs the calls the thread holds without results — re-dispatching them through the same tool path as any call, recording their results — and then asks the model, which reads the results; with nothing unanswered it asks the model straight away, which is the turn re-sent. A running child is parked `waiting_for_input` and reports the restart to its parent.

**2. Keep the crash marking and give the deployment its own interrupt cause. (rejected)**

The research note proposed a `deploy` cause so boot could tell a deliberate replacement from a crash. The operator rejected marking: a cause is a statement about the session's turn, and nothing about a deployment belongs on the session row. A session that was running is still running from the session's own point of view — it has work whose result is missing — and the state says that truthfully without a second vocabulary for "interrupted, but meant".

**3. Answer the in-flight call with the synthetic unknown-effect result, and let the model decide whether to run it again. (rejected)**

That is what a prod does today, and it is safe: the window stays valid and the model is told the effect is unknown. But it makes the model re-decide something the machine knows — the call is unanswered, and the session wants its result — and the operator chose the re-issue. The synthetic path stays for a genuine crash, where no boot is coming to fix the thread.

**4. Re-issue the call on the node, when its tunnel reconnects. (rejected)**

The node knows which shells it had, but it does not know whether the control plane recorded their results, whether the turn is still wanted, or what the session's model is doing. It would also have to write into the transcript. The control plane owns the thread and the decision; the node executes a command when one arrives.

**5. Bound the resume with an attempt marker in the store. (rejected)**

A marker (one attempt per session per deployment) would need a store column, a reset rule for the next deployment, and agreement about what counts as an attempt. The wake itself is the bound: one per session per boot, sent from the boot scan, which runs once. The residual is named below: a session that crashes the control plane is resumed again by the next boot.

**6. Children resume like roots. (rejected)**

A child's work is half-done and its parent is the one that decides what to do with the remainder: resume it, abandon it, or hand it to another persona. A child that resumed on its own would also re-run a call its parent may be about to cancel, and the parent would learn about it only from the manifest. The parent's re-decision already exists for crashes; a restart now feeds it a report instead of a failure.

**7. Let the node's shells die with the process, or sweep them on the next command. (rejected)**

A shell whose leader is killed by the OS exit leaves its children running in their own session, reparented and unreported: the operator's "no orphans". Sweeping them later cannot recover their exit codes either, because the process that reaps them is gone. Killing them on the way down is the only point where the node still knows what it started.

## Decision

**Boot marks nothing for a running session.** `recover` reads the stored sessions once and:

- A session in `running` whose model is configured, and which has no parent, is left `running`. After the loops start, it is sent one resume wake. Nothing else is written about it.
- A session in `running` whose model is configured and which has a parent is set `waiting_for_input`, and then authors a report to its parent: `RESTART_REPORT_TEXT`, "the control plane was replaced while I was running, and I stopped; resume me or abandon me". It is not woken. Its parent's manifest carries the report, the parent's loop wakes, and the parent's normal re-decision applies: `message_child` resumes the child, or it stays parked.
- A session in `waiting_for_input`, `interrupted` or `stopped` is untouched.
- A session in `creating` ran nothing, and a session in `running` whose model is no longer configured has no loop to run: both keep the crash rule — marked `interrupted` with cause `crash`, and a child among them authors the failure event it does today.

**One resume wake per session per boot.** `LoopEvent::Resume` is a wake of its own; the loop's gate lets it through on the session it was sent for and drops it if the session has stopped or the user interrupted it first, because a stop the user asked for outranks a restart. Nothing bounds the resume beyond that: a session that crashes the control plane is resumed again by the next boot.

**The first turn of a resume wake runs the thread's unanswered calls.** `unanswered_calls` reads the window and returns every `ToolCall` no `ToolResult` answers, in thread order, except one whose `ask` became a question — that question is live, not interrupted, and re-issuing it would ask it twice. The session's tool surface and the route of every MCP tool it can reach are built before this, because the dispatch reads them: a re-issued MCP call must find its server, and a resumed turn reaches the dispatch without a completion in front of it. Each of those calls is then dispatched through the ordinary tool path with its call already in the transcript, so the turn records only the result, and the turn ends as a tool-call turn. The turn after it is an ordinary one: the model is asked with the results in its window. When nothing is unanswered, the resume turn does nothing and the model turn follows at once, which is the turn re-sent from the durable thread.

**A re-issued MCP call whose server is gone falls to the node.** The routes are built from the servers this boot can reach, so a server that was up when the call was first dispatched and is down at resume has no route: the dispatch falls through to the node path, which does not know the tool, and the model reads an unknown-tool failure for that call. The session's own warning about the unavailable server is in the transcript beside it, so a reader sees why.

**A question that was asked but never recorded is lost.** The ask tool's call is written before it is dispatched and its question follows as the Ask block; a control plane that stops between those two writes leaves a call no result and no question answers. `unanswered_calls` skips it, because the live question it usually marks is not there, and the readme the session's turn sees is a dangling ask call the request filter drops. The question never reaches the user, and the model is not told it asked. Carrying the call id in the Ask block would close it, at the price of a durable block shape used by every stored transcript since asks existed; the window is two writes wide, and this ADR names it instead.

**A node-only restart does not re-run anything.** The rebuilt executors restore from `state.json` and wait. The call whose node was down, or was stopping, is recorded by the control plane as a tool failure — the node never answered it — and that failure stands: only a control plane boot that resumes the session runs the call again. A node coming back inside a running control plane leaves that failure in the thread for the model to read, and it is the model that decides whether to call the tool again.

**A node kills its shells when it is asked to stop.** `run_node` watches SIGTERM as well as Ctrl-C — a service manager stops a unit with SIGTERM, and a process that watched only for Ctrl-C would be killed outright with its shells left running — and after the loop ends it calls `NodeManager::kill_all_shells`, which signals every session's in-flight shells and waits, bounded, for their runs to end: signalling alone would leave the process groups alive if the runtime stopped before the owner tasks ran. It also refuses any new call once the shutdown has started, so the tunnel cannot start a shell in the window. Each shell's owner task kills its process group and reaps it. A restarted node rebuilds its executors from `state.json` and does nothing else: it does not re-run the call its predecessor was holding, and the failure the control plane recorded for that call stands, as above.

The synthetic unknown-effect answer stays for the case it was written for: a session a crash interrupted, which the user then prods. Nothing about that path changed. It is also what a user who types before the resume wake sees: their message runs an ordinary turn, which answers the dangling call as interrupted, and the resume then runs after that turn and re-issues the call anyway. Such a call can therefore be attempted twice, and the model reads the synthetic answer once before the real one.

## Consequences

- A deployment no longer stops the work it interrupted. Every session that was running keeps running, its in-flight call runs again, and its model reads the result in the same turn. The user sees the clients reconnect and nothing else.
- A re-issued call can run twice. The first run may have completed — a shell command, a file edit, a spawn — and the control plane cannot know. This is the operator's decision, and it is the same risk a model takes when it retries a call itself; what the resume adds is that the retry happens without the model, in the transcript, so the result the model reads is the second run's.
- The session that re-runs is the one whose work was interrupted, so a call with an effect on the outside world (a deploy, a push) can act twice. Nothing here distinguishes the tools; a per-tool policy for resume would be its own decision.
- There is no cap beyond one wake per boot. A session that reliably crashes the control plane is resumed on every boot, and re-runs its call each time; the damage is bounded by the boot, not by the session.
- A child that was running is parked, not resumed: its parent learns from a report and decides. A parent that never wakes — a stopped root, a tree nobody is looking at — leaves the child waiting, which is what a crash does today as well.
- The resume turn is a tool-only turn: it can append a result without a completion. A reader looking at the activity console sees the tool traffic without a model call around it, which is exactly what happened.
- The node's exit is now a signal away from being graceful: SIGTERM ends the poll loop, the tunnel and the shells, where before it killed the process with its shells still running. The control plane gains the same handler, and that is not a pure gain: its drain waits for the connections a stop leaves open, and a pane's event stream never completes on its own, so the drain is bounded at five seconds and the process then stops with those streams open. Both serve paths share that drain — `serve_until_stopped` owns the signal and the bound, and the plain path hands the trigger to axum's graceful shutdown while the TLS path stops accepting and joins the connections it opened. The TLS path is the one a deployment runs, and before this it had no drain at all: a stop closed the listener and exited with the connections cut. A stop with a pane connected therefore takes up to that bound, and the clients reconnect from their cursors. Without the bound, a service manager waiting for the process would run out its own timeout and kill it, which is exactly what the handler was added to avoid.
- Nothing in this path is exactly-once. The re-issue can run a call twice (above), a `spawn` whose first attempt landed before the restart starts a second child, a `message_child` whose first delivery was in flight is delivered again, and a node receives a call again when the control plane did not see its answer. The protocol carries no dedup key and none of these layers adds one: a tool whose effect matters has to be idempotent, or the model has to check what it changed before it relies on the result.
- `running` no longer implies "a turn is in flight right now" for the moment between boot and the resume wake. It means what it always meant at the store: the session has work, and something about it is unfinished.
- The session-states rule "an interrupted turn is never replayed" now holds only for the causes it was written about: a user interrupt, a turn failure, and a crash with no boot to resume it. A deployment replays the turn by design.

## Revisit When

- A call must not be re-issued — a deployment, a push, a message with an effect — which needs a per-tool answer the tool list does not carry today.
- Resumes need a bound across boots: a session that crashes the control plane, or a call that reliably fails it, is currently resumed once per boot forever.
- A resume should be visible to the user: a transcript line, or a state the list can show, rather than an unchanged `running`.
- A node's shells need a durable record of their exit, so a restart can tell a killed shell from one that finished.
- A child needs to decide for itself: a long-running child whose parent is gone has no one to re-decide it.
