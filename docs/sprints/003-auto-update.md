# Sprint 003 — Auto-update

The control plane distributes updates to nodes and the CLI. Nodes converge to the control plane's version automatically; the CLI is told an update exists and the user applies it. The control plane itself is updated by hand.

Status: **planned**.

## Confirmed decisions

- The control plane is the single distributor of updates. It serves only its own version, for every target platform, from a local artifacts directory. Nodes and the CLI never touch a release feed.
- The control plane itself is updated manually, out of band. The operator also refreshes the artifacts directory.
- Version rides the existing protocol, so there is no separate update-check roundtrip. The node sends its version in the poll request; the control plane puts its version in every response, as a header and in the poll body.
- The poll handshake is a deliberately stable, minimal protocol. `Update` is a permanent primitive every node understands; the control plane version-gates other commands by the node's reported version.
- Nodes auto-converge: a node behind the control plane, with updates enabled, updates itself. Upgrades only — a node ahead of the control plane stays put unless forced.
- Apply is immediate, then resume: download, verify, swap, self-restart, and sessions restore from `state.json`. Self-restart is `execve` on Unix and spawn-then-exit on Windows; no supervisor is required.
- Verification is client-side: the client checks the sha256 from the control plane's manifest, then runs the new binary `--version` and requires it to match. The control plane cannot validate binaries for other platforms.
- No downgrades without an explicit `--force`.
- Per-node opt-out: `[update] enabled = bool`, default true. It is a hard override the control plane respects and reports.
- Rollback is manual only: a `.previous` backup plus `bosun node --rollback`. No automatic crash-loop detection.
- The CLI is told, not updated for: the control plane announces its version, the CLI prints one notice, and the user runs `bosun update`.
- This extends `2026-08-21-nodes-dial-out-only.md`: updates ride the existing outbound poll and command channel; nothing inbound is added. A new ADR records the update model.

## CLI surface

`update`, `nodes` gains version and status.

- `bosun update` — update the local binary from the connected control plane. `--force` allows a downgrade.
- `bosun update <node>... [--force]` — demand a node update. Ordinary upgrades auto-converge; this exists to force a downgrade of a node ahead of the control plane.
- `bosun node --rollback` — swap back to the previous binary.
- `bosun nodes` — shows each node's version and update status.

## User stories in implementation order

- [x] **S1 — Version handshake in the protocol**

As a developer, I want version and update availability to ride the existing protocol, so a client learns it is outdated with zero extra roundtrips.

- The node sends its version in the `PollRequest` payload; the control plane stores it on the node's registry entry.
- The control plane puts its version in every response: the `X-Bosun-Version` header and the poll response body.
- The poll response also carries whether an artifact exists for the node's platform, so a node with no matching binary does not spin.
- Versions compare as semver; equal versions mean nothing to do.

- [x] **S2 — Artifacts directory and manifest on the control plane**

As a developer, I want the control plane to serve pre-built binaries from a local directory, so clients can be updated without any access to a release feed.

- `update.artifacts_dir` in the control-plane config (default `<cp data dir>/artifacts/`) holds one file per platform: `bosun.<target-triple>`.
- A manifest endpoint returns `{ version, artifacts: { target: { sha256, size } } }`, hashed lazily per file and cached by mtime.
- Targets missing from the directory are absent from the manifest; the control plane never downloads anything.

- [x] **S3 — Node auto-converge**

As a node operator, I want a node to update itself to the control plane's version, so the fleet stays in step without commands.

- On each poll, a node that is behind the control plane and has updates enabled fetches the manifest and its platform's artifact.
- The node verifies the sha256, runs the new binary `--version`, and requires a match with the control plane's reported version before touching the running binary.
- The node swaps the binary, keeps the previous one as `.previous`, self-restarts (`execve` on Unix, spawn-then-exit on Windows), and resumes active sessions from `state.json`.

- [x] **S4 — Node opt-out and result reporting**

As a node operator, I want the node's update decisions and outcomes visible on the control plane, so I can see why a node is not in step.

- `[update] enabled = false` in the node config is a hard opt-out; the node reports `update disabled` and the control plane shows the node as paused rather than demanding updates.
- Failures — download error, checksum mismatch, missing artifact, version mismatch — are reported back through the poll result channel.
- The registry records `version` and `update_status` (up-to-date / updating / failed(reason) / ahead / disabled / no-artifact); `bosun nodes` renders them.

- [x] **S5 — Manual rollback**

As a node operator, I want to revert a bad update by hand, so I can recover without re-installing.

- `bosun node --rollback` swaps the `.previous` binary back into place and self-restarts.
- Rollback is manual only; there is no automatic crash-loop detection.

- [x] **S6 — CLI update**

As a CLI user, I want to be told when my binary is behind the control plane and to update it with one command, so I always run a compatible client.

- When a command reaches a control plane whose version is newer, the CLI prints one notice to stderr — `bosun <version> available, run "bosun update"` — and never for `serve`, `node`, or `update` itself.
- `bosun update` fetches the control plane's artifact for the CLI's platform, verifies the sha256 and the `--version` match, and swaps the binary; the next invocation runs the new version.
- `bosun update --force` allows a CLI ahead of the control plane to downgrade.

- [x] **S7 — Forced node downgrade**

As an operator, I want to bring a node ahead of the control plane back in step, so the fleet converges on the control plane's version.

- `bosun update <node> [--force]` enqueues an `Update` command carrying the force flag.
- Without `--force`, a node ahead of the control plane refuses and reports `ahead`; with it, the node applies the downgrade.

## Out of scope

No release-feed integration or upstream dispatch — the control plane has no upstream. No automatic crash-loop rollback. No update scheduling or quiet hours. No channels (stable/beta). No artifact signatures beyond the sha256. No auto-update of the control plane itself. No update of the executor beyond what the node's own binary provides.
