# ADR: Remote skill packages from GitHub

**Date:** 2026-09-06
**Author:** Raghav

## Context

Today the operator feeds the control plane's injected skills by dropping `<name>/SKILL.md` directories into `data_dir/skills` before starting `bosun serve`, and the loop reads them through `injected_skills_dir` in `AgentRegistry`. The `skill` tool's advertisement names the working copy and the control plane as the two discovery sources. Sprint 006's in-process executors keep that arrangement.

The next feature replaces manual drops with GitHub repositories: the operator references a repo that contains skills, bosun fetches it, indexes the packages, and sessions use them. The runtime shape of those packages is decided in `2026-09-06-skill-package-standard.md`. This ADR decides where the source list lives, how fetching and indexing work, and how the store holds the result.

## Decision Drivers

- Repos are managed from the web pane: add, remove, and update happen in the browser. The store is the source of truth for the repo list; no config block declares it.
- Nothing fetched is written to disk beyond the SQLite store: no clones, no tarball leftovers, no per-repo directories, no token in any file.
- Secrets stay out of the database and out of the API: one optional GitHub token, set as `github_token = "env:VAR"` in `serve.toml`, resolved at boot like the model `api_key`s.
- Fetching must not depend on a git binary. The control plane already ships a reqwest client (provider calls, release downloads).
- An update is atomic per repo and only moves forward: re-resolve the tracked ref to a SHA, and when it changed, replace that repo's packages in one transaction.
- The control-plane local-injected source is deleted: remote packages and the working copy are the two sources, and the standard's short-name precedence (working copy wins) applies between them.

## Options Considered

- **Config-declared repos with a boot fetch and a periodic refresh timer.** Rejected: the operator wanted repos added from the web pane, and a boot fetch turns an unreachable GitHub into a boot failure mode that a store-managed list does not have.
- **`git clone` on the control plane.** Rejected: it adds a git dependency, leaves a working tree and `.git` directory to manage, and needs token plumbing into a clone URL. Fetching over the already-present reqwest client keeps the control plane free of repo state.
- **Read skill files live from GitHub on every session.** Rejected at review: it puts GitHub on the session's hot path, trips API rate limits for directory enumeration, and offers nothing a pulled-and-stored package does not.
- **Per-repo tokens entered in the web pane.** Rejected: secrets in the store and masking rules through the API and UI for one operator; a single env-backed token covers public and private repos.
- **Keep `data_dir/skills` as a third source.** Rejected: the operator sees no use for it; deleting it leaves exactly two sources and drops `injected_skills_dir`, the boot `skills` directory creation, and the disk read path in the loop.
- **Multi-SHA retention with rollback.** Rejected in the standard ADR: one version per package, atomic replace; the recorded SHA is provenance.

## Decision

**Repositories are added, listed, removed, and updated through the control-plane API and the web pane. The store holds the repo list and every indexed package.**

**Schema.** Three tables are added to the store's `SCHEMA` string, so existing `store.db` files gain them at next open; foreign keys stay off, and the multi-row writes use the store's `conn.transaction()` pattern:

- `skill_repos`: `repo` (`owner/repo`, primary key), `host` (default `github.com`), `ref` (tracked branch, tag, or SHA; NULL means the default branch), `sha` (the commit currently indexed), `enabled` (default 1), `added_at_secs`, `updated_at_secs`, `last_error`.
- `skill_packages`: `address` (the package address, primary key), `repo`, `name` (the leaf short name, indexed), `description`, `when_to_use`, `license`, `author`, `instructions`, `sha`, `updated_at_secs`.
- `skill_references`: `package` (→ `skill_packages.address`), `path`, `content`, `PRIMARY KEY (package, path)` — one row per reference chunk, so a chunk read is one page.

**Fetch and index.** Adding a repo with ref `NULL` first resolves the default branch from `GET /repos/{owner}/{repo}`. Fetching resolves the ref to a commit SHA (`GET /repos/{owner}/{repo}/commits/{ref}`), lists the repository tree (`GET /repos/{owner}/{repo}/git/trees/{sha}?recursive=1`, failing when the response is truncated), and then fetches only the files it keeps — the raw endpoint (`raw.githubusercontent.com/{owner}/{repo}/{sha}/{path}`) without a token, the contents endpoint with `Accept: application/vnd.github.raw` and the bearer token when one is set. No archive is downloaded; the workspace has no gzip dependency, and the task picks the files to keep by path without pulling the rest of the repo. When the resolved SHA equals the repo's stored SHA, the fetch is a no-op that clears any recorded error. Package roots are the `**/skills/**/<name>` directories that hold `SKILL.md`, found anywhere in the tree: a directory named `skills` and, any depth below it, a package directory. Each root maps to a package: frontmatter to metadata, the body to `instructions`, and the directory's other text files to `skill_references` rows, subject to the indexer's per-file size cap and binary skip. `github_token` is sent as the `Authorization` header for all calls when set.

**Management.** `GET /skills/repos` lists repos with their SHA, timestamps, and package counts; `POST /skills/repos` adds a repo (fetching it before it returns); `DELETE /skills/repos/{repo}` removes it and its packages; `POST /skills/repos/{repo}/update` re-resolves the ref and, when the SHA changed, atomically replaces the repo's packages; `POST /skills/repos/{repo}/enabled` toggles whether its packages are advertised. The web pane gets a skills section: an add form, one row per repo with its SHA and status, and update, toggle, and remove controls. A public repo needs no token; a failed fetch records `last_error` and no packages.

**Loop wiring.** The loop stops reading `injected_skills_dir` and queries remote packages through its `Store` handle, cached once per session exactly like the working-copy list is today. The system-prompt advertisement and the `skill` tool implement the standard's `{ name, reference? }` surface, merging working-copy skills ahead of remote packages on short names.

**Deletions.** `AgentRegistry.skills_dir`, `LoopDeps.injected_skills_dir`, the boot-time creation of `data_dir/skills`, `injected_skills()`, and `read_injected_skill()` are removed.

## Consequences

- The repo list and all skill content are ordinary rows in the same SQLite file as sessions, so a later distributed deployment can move the store as one unit; nothing on disk depends on the control plane's host.
- Sessions need GitHub reachable only at the moment a repo is added or updated; at runtime the store serves every read, so a flaky network never stalls a turn.
- One token, env-backed, covers private repos; its value never appears in the store, the API, or the web pane.
- Two adoption costs: existing `data_dir/skills` content must move into a repo (it stops being advertised), and skills must match the standard's fields (the indexer's layout recognition is the conversion).
- Updating is forward-only; there is no way back to the previous SHA except re-fetching it, and old packages are gone when a newer index replaces them.
- The whole feature trusts the operator's choice of source: fetched content is third-party prose served into the session prompt, and the package address in each advertisement is the only marker of where it came from.

## Revisit When

- Repo management needs to happen outside the web pane (a CLI, a config block, or automation).
- A periodic refresh, a marketplace, binary assets, or multi-SHA retention is worth its machinery.
- Sources other than github.com become first-class.