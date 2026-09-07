# Sprint 007 — Remote skill packages

Sessions get skills from GitHub repositories. The operator adds a repo in the web pane, the control plane fetches and indexes its skills into the store, and the loop advertises and serves them through the `skill` tool. A skill is a Bosun Skill Package — metadata, instructions, and reference chunks in SQLite — rather than a directory of files, because the loop reasons on the control plane and the tools execute on a node. See the decisions in `../adrs/2026-09-06-skill-package-standard.md` and `../adrs/2026-09-06-remote-skill-packages.md`.

Status: **planned**.

## Confirmed decisions

- **A skill is a package with an immutable address.** Identity is `github.com/<owner>/<repo>/<path...>/skills/<name>`, never carrying a SHA. One stored version per package exists at runtime; updates replace in place and record the SHA as provenance.
- **Contents are metadata, instructions, and reference chunks.** Frontmatter maps to `description`/`when_to_use`/`license`/`author`; the body is `instructions`; the package directory's other text files are `references`, one row per file, addressed `package#<path>`. No code unit, tool grant, capability declaration, or live-command injection exists.
- **The store is the source of truth.** Three tables — `skill_repos`, `skill_packages`, `skill_references` — join the existing `SCHEMA` string; repos are added, removed, and updated through the web pane; no config block declares them.
- **Fetch is reqwest, not git.** Resolve the tracked `ref` to a commit SHA, list the tree, fetch only the files that become packages, index them, discard bytes. No archive and no gzip dependency. One optional `github_token = "env:VAR"` in `serve.toml` covers private repos and is never stored or exposed.
- **No files are materialized anywhere.** Not the working copy, not a node store, not the control plane's disk: content lives only in the store, and every read is a store query on the control plane.
- **The loop has two sources.** The working copy (via the executor) and remote packages (via the store); the control-plane `data_dir/skills` source is deleted. Working-copy skills win short names in the advertisement; package addresses are always unambiguous.
- **Enforcement is unchanged.** `allowed_tools` gates the `skill` tool; the executor's `read_only`/`read_write` gates shell and file writes regardless of skill content.

## CLI surface

No CLI changes. `serve.toml` gains the optional `github_token` field (`"env:VAR"` or a literal, resolved at boot like model `api_key`s). The web pane gains the skills management section.

## User stories in implementation order

- [ ] **S1 — Skill tables in the store**

As a developer, I want the store to hold repos, packages, and references as rows, so the loop, the API, and the indexer share one source of truth.

- `skill_repos`, `skill_packages`, and `skill_references` join the `SCHEMA` string with the columns in the remote-packages ADR, so existing `store.db` files gain them at next open; `created_at_secs`/`updated_at_secs` follow the sessions-table conventions.
- Store methods: list repos; resolve a short name to packages; load a package's instructions by address; list a package's reference paths; read one reference row; and the write side — `replace_skill_repo` replaces a repo's packages and reference rows and refreshes the repo row inside one transaction, and `remove_skill_repo` deletes the same rows.
- Tests exercise the transaction shape: a failed package insert leaves the repo's old rows and SHA untouched.

- [ ] **S2 — Fetch and index a repo**

As a user, I want to give bosun a GitHub repo and have its skills become packages, so I never edit files by hand.

- The GitHub client resolves a ref to a SHA (`GET /repos/{o}/{r}/commits/{ref}`, default branch when `ref` is NULL), lists the tree (`GET /repos/{o}/{r}/git/trees/{sha}?recursive=1`, failing on truncation), and fetches only the files it keeps via the shared reqwest client — the raw endpoint without a token, the contents endpoint with the `Authorization` header when `github_token` is set. No archive is downloaded and no gzip dependency exists.
- The indexer finds package roots — directories named `skills` whose children hold `SKILL.md`, anywhere in the tree, so Claude Code, opencode, `.agents`, plugin-pack, and collection layouts all convert to the standard.
- Each root maps to one package: frontmatter to metadata, body to `instructions`, sibling text files to `skill_references` rows (binary and over-cap files skipped), shipped to the store in one transaction with the resolved SHA.
- A repo with no package roots indexes zero packages without error; a failed fetch records `last_error` and changes nothing.

- [ ] **S3 — Manage repos from the web pane**

As a user, I want to add, list, update, and remove skill repositories in the browser, so the web pane is the only surface I need.

- Control-plane routes: `GET /skills/repos`, `POST /skills/repos` (`{ repo, ref? }`, fetch before returning), `DELETE /skills/repos/{repo}`, `POST /skills/repos/{repo}/update`, and a togglable `enabled`.
- The web pane's skills section lists each repo with its ref, SHA, `updated_at`, `last_error`, and package count, plus an add form and per-repo update and remove controls.
- Disabled repos stop advertising their packages but keep them stored; re-enabling restores the advertisement without a fetch.

- [ ] **S4 — Advertise remote packages in the loop**

As a developer, I want sessions to advertise remote packages beside working-copy skills, so the model sees the union.

- The loop drops `injected_skills_dir` and reads remote packages through its `Store` handle; the fetch is cached once per session exactly as the working-copy list is cached today, and a store failure degrades to working-copy-only with a warning.
- The advertisement renders each skill as `name (github.com/owner/repo/<path...>/skills/name)` plus the description, truncated to the standard's cap; `allowed_tools` gating of the `skill` tool is unchanged.
- `AgentRegistry.skills_dir`, `LoopDeps.injected_skills_dir`, and the boot creation of `data_dir/skills` are removed with `injected_skills()` and `read_injected_skill()`.

- [ ] **S5 — The `skill` tool serves packages**

As a model, I want to load a package's instructions and pull its reference chunks by name, so skills work end to end.

- `skill { name, reference? }`: `name` resolves a short name when it matches one package, or a full package address always; an ambiguous short name is an error listing the candidates; a working-copy skill wins the short name.
- A load returns the instructions and the index of reference paths the body references; `reference` returns that row's chunk. The existing working-copy read path (`read_working_skill` via the executor) is unchanged.

- [ ] **S6 — Config, docs, and deletions**

As a developer, I want the feature coherently configured and documented, so operators can adopt it.

- `github_token` joins `ControlConfig` with `env:VAR` resolution, applied at boot and never serialized back to TOML or exposed through the API.
- `config.md` documents the field and the web-pane management model; the sprints and ADRs cross-reference the standard and the remote-packages decisions; `CLAUDE.md`'s current-state line moves to this sprint.

## Out of scope

- No code execution unit, binary chunks, tool grants, live-command injection, or `needs` declarations in the standard.
- No periodic refresh, marketplace, package registry, or non-GitHub host.
- No multi-SHA retention or rollback.
- No per-package enable/disable (repos enable and disable as a unit).
- No standalone converter for local skill directories; the indexer's layout recognition is the conversion path, and authoring guidance follows once the standard ships.
- No CLI or config-file management of repos; the web pane is the only surface.