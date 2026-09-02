# Sprint 004 — Updates fetched from releases

The control plane stops distributing binaries. It announces its version in the protocol, and nodes and the CLI fetch the released artifact for that version directly from GitHub Releases, or from a configurable mirror. The artifacts directory on the control plane is gone.

Status: **complete**. All four stories are implemented and tested.

## Confirmed decisions

- The control plane announces its version only: the node's poll response and the `X-Bosun-Version` header on every control-plane response. It no longer serves binaries, and it has no update endpoints.
- A behind client downloads the released archive for the announced version and its platform: `bosun-{target}.tar.xz` (`.zip` on Windows) under `v{version}`, verifies the per-asset `.sha256` the release publishes, extracts the `bosun` binary, and applies it as before (verify `--version`, swap, restart).
- The release feed is configurable. Precedence: node `[update] base_url` config, then `BOSUN_UPDATE_BASE_URL`, then GitHub Releases for this repository. A mirror must serve the same `v{version}/bosun-{target}.tar.xz` layout.
- Clients can only converge on released versions. A control plane running an untagged build cannot update clients; the control plane should run released versions.
- Availability is discovered at fetch time, not announced: a missing release surfaces as a fetch failure (`no-release`), reported through the poll result channel.
- The update status vocabulary follows: `no-artifact` becomes `no-release`.
- This sprint supersedes the distribution mechanism in `2026-09-01-control-plane-distributes-updates.md`. The rest of that ADR's decisions stand: version in the protocol, the stable handshake, node auto-converge, verification, no downgrades without `--force`, the node opt-out, manual rollback, and result reporting.
- The transition from 0.6.0: clients released with 0.6.0 still fetch from the control plane's removed artifact endpoints, so they need one out-of-band update to a build that fetches from releases. After that, auto-update works from the feed.

## User stories in implementation order

- [x] **S1 — Fetch the released artifact**

As a developer, I want a shared fetch path that downloads and verifies a released binary, so nodes and the CLI update from the same code.

- `fetch_release_artifact` downloads `{base}/v{version}/bosun-{target}.tar.xz` (`.zip` on Windows), verifies it against the release's per-asset `.sha256` file, extracts the `bosun` binary into the staging directory, and makes it executable on Unix.
- The base resolves as config, then `BOSUN_UPDATE_BASE_URL`, then GitHub Releases; whitespace-only values are treated as unset.
- Failures are distinct: no release for the version, missing or malformed checksum file, checksum mismatch, extraction failure. Extraction runs off the async executor.

- [x] **S2 — The node updates from the release feed**

As a node operator, I want a node to fetch the announced version from the release feed, so updates no longer depend on what the control plane has on disk.

- The node's auto-converge and forced-downgrade paths fetch `fetch_release_artifact` for the control-plane-announced version and the node's own target.
- `update.base_url` in the node config selects the feed; the node stops reading the poll response's artifact availability.
- A node that would converge on an unreleased version reports `no-release` through the poll result channel.

- [x] **S3 — The CLI updates from the release feed**

As a CLI user, I want `bosun update` to fetch the announced version from the release feed, so my binary matches the control plane without the control plane serving it.

- `bosun update` reads the control plane's version from the `X-Bosun-Version` header on an existing route, then fetches that version from the release feed and swaps the binary.
- The downgrade gate, `--force`, the `.previous` backup, and the Windows finalizer are unchanged.

- [x] **S4 — Remove the control plane's artifacts machinery**

As a developer, I want the artifacts directory and its endpoints gone, so the control plane no longer carries binaries.

- Removed: the artifacts directory and its config, the manifest and artifact endpoints, the artifact availability poll field, the node's target triple in the poll request, and the control-plane update wire types.
- The node `[update]` section keeps `enabled` and gains `base_url`; the control-plane `[update]` section is gone.

## Out of scope

No artifact signing beyond the release's sha256 plus the `--version` match. No automatic crash-loop rollback. No update scheduling or channels. No auto-update of the control plane itself. A mirror must replicate the GitHub Releases layout; nothing serves it for you.
