# ADR: The Bosun skill package standard

**Date:** 2026-09-06
**Author:** Raghav

## Context

Skills reach a session from two places today. The working copy's `.agents/skills/<name>/SKILL.md` is discovered and read through the session's executor, because the working copy lives on the node; the control plane's own `data_dir/skills/<name>/SKILL.md` is read locally by the loop. Both parse to `Skill { name, description }` in `bosun-common/src/skills.rs`, are merged in `agent_loop.rs` (`merge_skills`, working copy shadowing injected), advertised in the system prompt, and loaded on demand through the `skill` tool.

The next feature adds skills that come from GitHub repositories. The conventions of local harnesses — Claude Code, opencode, and the Anthropic agent-skills spec — define a skill as a directory of files: a `SKILL.md` plus scripts, references, and templates, placed beside the agent's file and shell tools. Their runtime depends on that co-location: living command execution inlines output at load time, bundled scripts run through the shell, and reference files are read on demand. Bosun has no shared disk. The agent loop reasons on the control plane; the file and shell tools execute on a node. In that split, "a skill is a folder beside the tools" has no meaning.

This ADR decides what a skill is at runtime. The companion `2026-09-06-remote-skill-packages.md` decides where skills come from and how the store holds them.

## Decision Drivers

- A skill must be fully described by data the loop can hold, and served to the loop without writing files on any machine.
- Identity must be unambiguous and carry provenance, so two sources that ship the same skill name never shadow each other silently.
- One version of each package exists at runtime; the store is a state cache, not an archive.
- Progressive disclosure: the advertisement carries name and description, the instructions load on demand, and each reference loads on demand.
- Enforcement stays where it is: a persona's `allowed_tools` and the executor's `read_only` permission. No per-skill privilege, tool surface, or capability declaration is added.
- Existing repo conventions are conversion inputs at ingest time, never runtime constraints.

## Options Considered

- **Adopt a local convention verbatim.** Rejected: a skill "directory" only means something where files exist. The loop cannot read the node's filesystem, and syncing skill trees into the working copy pollutes the user's checkout and — with in-process executors sharing a node — the disk that sessions share.
- **Materialize skill content on nodes**, as a content-addressed store or as per-session copies under the working copy. Rejected at review twice: the working copy is the user's checkout, and any node-side store is shared state the feature would have to own, fence, and clean. A distributable harness keeps content as data, not disk.
- **Instructions-only skills**, bundled resources ignored. Rejected: the reference-heavy skills (document skills whose `SKILL.md` points at `FORMS.md` and `REFERENCE.md`, rule sets like `vercel-labs/agent-skills`) are gutted, and progressive disclosure dies with them.
- **A code-execution unit in the standard.** Rejected: the agent already has a shell and file tools on the node. An executable chunk would have to be written out before it could run, an indirection that buys nothing, and the deterministic behaviour it holds is better described in the instructions.
- **Tool grants, live command injection, and a `needs` capability declaration.** Rejected: grants would raise a skill above the persona's permission, live injection executes on load and needs node-side machinery the loop should not own, and `needs` duplicates the executor's gate.
- **Multi-version packages with rollback.** Rejected for the single-user world: one version per package keeps identity short, and an update is an atomic replace. The stored SHA is provenance, not a version to return to.
- **Leaf-name addressing with collision-order resolution.** Rejected: silently resolves which of two same-named skills loads. A Go-style package address makes identity global and unambiguous.
- **References stored as a JSON map in the package row.** Rejected at review: reading one chunk would deserialize every chunk. One row per file reads a single page.

## Decision

**A skill is a Bosun Skill Package: metadata, instructions, and reference chunks, identified by an immutable package address.**

**Identity.** A package is addressed `github.com/<owner>/<repo>/<path...>/skills/<name>` — the repo-relative path to its `skills/` directory plus the skill's directory name. The address never carries a SHA; one stored version of a package exists at runtime, and the `@sha` only appears as provenance in the store and the web pane.

**Contents.** A package carries exactly this:

- Metadata parsed from the `SKILL.md` frontmatter: `name` (the directory name when absent), `description`, optional `when_to_use`, `license`, `author`. The `skill` advertisement truncates `description` to about 500 characters.
- `instructions`: the `SKILL.md` body, served by the `skill` tool on load.
- `references`: the package directory's other text files, each stored as a single chunk addressed `package#<relative-path>`. Binary files and files over the indexer's size cap are skipped at ingest, so a reference row never holds bytes and no path that is not a stored key resolves.

**Runtime flow.** The system prompt lists each advertised skill as `name (github.com/owner/repo/<path...>/skills/name)` and the truncated description. The `skill` tool takes `{ name, reference? }`: `name` is the short leaf when it resolves to one package, or the full package address; ambiguity among short names is an error listing the candidates. A load returns the instructions and an index of the reference paths the instructions actually reference, so the model knows what is pullable without guessing. `reference` returns the chunk's content.

**Enforcement.** No per-skill grant exists. A persona's `allowed_tools` gates the `skill` tool itself, and the executor's `read_only`/`read_write` permission continues to gate shell, `file/write`, and `edit` regardless of what a skill's instructions say.

**Precedence.** A working-copy skill whose short name collides with a remote package wins the short name in the advertisement; the full package address always resolves unambiguously.

## Consequences

- The loop can serve everything a session ever needs from the control plane's store: no file is materialized on a node, no path confinement changes, and the executor is untouched by remote skills.
- Reference chunks live one per row and a package update replaces rows in one transaction, so load and reference reads are single-row reads.
- Skills that exist only as bundled executable code or binary assets do not work; the model reads their instructions or adapted text and implements with its own tools. This is a stated boundary, not a gap.
- A skill authored for another harness needs conversion to this standard's fields. The indexer's layout recognition at ingest (`**/skills/**` package roots, `SKILL.md` frontmatter mapping, plugin-pack paths) is that conversion for repo sources; a standalone converter for local skill directories is future work.
- Third-party skill text is third-party input to the model. The package address in every advertisement is the provenance label; the trust gate is the operator adding the source repo.

## Revisit When

- A skill must ship executable or binary content, and the cost of an ephemeral execution sandbox for it is accepted.
- Sessions must hold a previous version of a package to roll back to.
- A non-GitHub source, a package registry, or a marketplace becomes a distribution path.