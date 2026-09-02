# ADR: Clients fetch updates from the release feed

**Date:** 2026-09-02
**Author:** Raghav

> Supersedes `2026-09-01-control-plane-distributes-updates.md` for the distribution mechanism. The control plane stops serving binaries; clients download the announced version from GitHub Releases or a configured mirror. The other decisions in the superseded ADR stand: version in the protocol, the stable handshake, node auto-converge, verification by `--version`, no downgrades without `--force`, the `[update] enabled` opt-out, manual rollback, and result reporting.

## Context

Bosun shipped updates through the control plane in `2026-09-01-control-plane-distributes-updates.md`: the control plane served only its own version, for every target platform, from a local artifacts directory that the operator refreshes by hand whenever the control plane is updated.

Operating the artifacts directory is a recurring footgun. Every control-plane update carries an out-of-band duty to drop matching binaries into the directory; miss it and the control plane advertises a version it does not serve, so clients fail to converge with version-mismatch errors until an operator notices. The clients actually have outbound access to the release feed, which the earlier ADR assumed they might not.

## Decision Drivers

- Remove the artifacts-directory operational duty. Clients should fetch exactly the released binary for the announced version, verified against the publisher's checksums.
- No extra roundtrips: the version already rides the poll response and the `X-Bosun-Version` header, so discovery stays on the control plane.
- The control plane should only ever run released versions; an untagged build has no release to converge on.
- Keep an air-gapped path: a configurable feed base lets a mirror serve the same release layout.

## Options Considered

- **Control plane serves its own version from a local artifacts directory (2026-09-01).** The decision being revisited. Rejected: it adds a per-update manual duty whose failure mode is silent non-convergence.
- **Clients check the feed themselves for the latest version.** Rejected: it costs a roundtrip and hits feed rate limits from every client; discovery already rides the protocol.
- **Clients fetch the control-plane-announced version from the feed (chosen).** The control plane announces, the feed delivers.

## Decision

The control plane announces its version and serves nothing else. A client that is behind fetches the released archive for that version and its own platform from the release feed, verifies the release's per-asset `.sha256`, extracts the binary, and applies it exactly as before: verify the staged binary's `--version` against the announced version, swap it in keeping the previous one as `.previous`, and restart (nodes) or let the next invocation run it (CLI).

- The feed base resolves as node `[update] base_url`, then `BOSUN_UPDATE_BASE_URL`, then GitHub Releases for this repository. A mirror must serve `v{version}/bosun-{target}.tar.xz` (`.zip` on Windows) and the matching `.sha256` files.
- Availability is discovered at fetch time: a missing release is a `no-release` outcome reported through the poll result channel, not a fact the control plane announces.
- The control-plane artifacts machinery is removed: the artifacts directory and its config, the manifest and artifact endpoints, the artifact-availability poll field, and the node's target triple in the poll request. The update status label `no-artifact` becomes `no-release`.
- Clients released under the superseded ADR (0.6.0) still fetch from the removed control-plane endpoints; they need one out-of-band update to a build that fetches from the feed.

## Consequences

- Updating the control plane no longer requires touching a binaries directory; clients converge on the release the operator runs.
- A client can only reach a released version. Running an untagged or locally patched control plane leaves clients where they are.
- The feed is now in the client's trust path: a compromised mirror can serve a wrong binary, bounded by the sha256 the mirror itself publishes and the `--version` check against the announced version. Operators who need more than this must sign artifacts.
- GitHub Releases download URLs are not API-rate-limited, so per-client downloads do not exhaust the feed's API quota; discovery stays on the control plane and costs nothing.

## Revisit When

- A fleet must run a version the feed does not publish — for example an internal patched build — or must be pinned to a version older than the control plane's.
- Artifacts must be signed end to end rather than verified by the release's sha256 plus a `--version` match.
- Air-gapped nodes need a first-class mirror story rather than a manually replicated release layout.
