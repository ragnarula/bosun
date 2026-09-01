# ADR: The control plane distributes updates

**Date:** 2026-09-01
**Author:** Raghav

## Context

Bosun ships as one `bosun` binary hosting every role: control plane, node, CLI, executor. The workspace is versioned in lockstep and released through cargo-dist to GitHub Releases. Nodes dial the control plane only, on a long-poll command channel; they bind no inbound port. There is no update mechanism today: a node or CLI keeps whatever binary the operator installed until the operator replaces it by hand.

The fleet must converge on the control plane's version, and clients that cannot reach the release feed must still be updatable.

## Decision Drivers

- Nodes dial out only; the control plane must not need new inbound paths to a node.
- Clients (nodes and CLI) may have no access to GitHub Releases; the control plane is the single trusted party they already talk to.
- No extra update-check roundtrip: version and update availability ride the existing protocol.
- The poll handshake must stay stable across versions, because an outdated node can only be pulled forward if it can still receive the update instruction.
- A single-user system: keep the mechanism simple, no fleet orchestration beyond what is needed.

## Options Considered

- **Clients check a release feed directly.** Rejected: nodes and CLI users may not reach GitHub, and it spreads the feed's trust into every client.
- **Control plane relays feed versions and artifacts.** Rejected in favour of an even simpler model: the control plane distributes only its own version, and is itself updated out of band.
- **Automatic scheduled checks.** Rejected: updates apply on demand (nodes converge on connect; the CLI is told and the user runs `bosun update`). Nothing updates without being demanded.
- **Idle-gated updates on nodes.** Rejected: a demanded update applies immediately, then resumes sessions from `state.json`.

## Decision

The control plane is the single distributor of updates. It serves only its own version, for every target platform, from a local artifacts directory; it has no upstream. Nodes converge automatically; the CLI is told and updates on request; the control plane itself is updated by hand.

- **Version in the protocol.** The node sends its version and target triple in each poll request; the control plane puts its version on every response, as a header and in the poll body. No separate update-check endpoint is needed. Versions compare as semver.
- **Stable handshake.** The poll handshake is a deliberately minimal protocol that does not change. `Update` is a permanent primitive every node understands; the control plane version-gates other commands by the node's reported version.
- **Node auto-converge.** A node behind the control plane, with updates enabled, downloads the control plane's artifact for its platform, verifies it, swaps its binary, keeps the previous one as `.previous`, and restarts itself (`execve` on Unix, spawn-and-finalize on Windows). Sessions restore from `state.json`. Upgrades only; a node ahead of the control plane stays put.
- **CLI.** The control plane announces its version on any response; the CLI prints one notice when it is behind and applies the update with `bosun update`. The next invocation runs the new binary; the CLI does not restart itself.
- **Verification.** Client-side only: the client checks the sha256 from the control plane's manifest, then runs the new binary `--version` and requires a match. The control plane cannot validate binaries for other platforms.
- **No downgrades without `--force`.** `bosun update --force` for the CLI; `bosun update <node> --force` enqueues an `Update` command carrying the force flag.
- **Opt-out.** `[update] enabled = false` in the node config is a hard override; the node reports `disabled` and the control plane shows it as such.
- **Rollback is manual.** `bosun node --rollback` restores `.previous`. There is no automatic crash-loop detection.
- **Reporting.** The node reports its update status (up-to-date, updating, failed(reason), ahead, disabled, no-artifact) in each poll; the registry stores it and `bosun nodes` renders it.
- **Old nodes.** A node whose reported version is empty or unparsable predates the version handshake and cannot parse an `Update` command; the control plane refuses to enqueue one for it and the operator upgrades it out of band.

## Consequences

- Nodes and CLI need no release-feed access; they update over the same TLS relationship they already have with the control plane.
- The control plane's artifacts directory must be refreshed whenever the operator updates the control plane, or clients converge on a stale version.
- Nodes ahead of the control plane (e.g. after a control-plane rollback) stay ahead until explicitly forced down.
- A node behind a firewall that cannot even reach the control plane cannot auto-update; that was already true for every other command.
- Rollback re-triggers auto-converge on the next poll, so an operator who rolls back to escape a bad build must also disable updates or keep the control plane pinned.
- The update path adds binary download, verification, and self-restart machinery to the node and CLI; the rollback and Windows finalizer paths carry the platform-specific rename/copy handling.

## Revisit When

- The control plane must distribute a version other than its own — for example a fleet operating on a mixed-version rollout.
- Artifacts must be signed end to end rather than verified by sha256 plus a `--version` match.
- Multiple users or untrusted nodes require the control plane to authenticate update commands.
