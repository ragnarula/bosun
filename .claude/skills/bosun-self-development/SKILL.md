---
name: bosun-self-development
description: Use when developing Bosun itself — pick the next issue from the tracker, rank it, see it through to a tagged release, then pick the next. Fires on "work on the next issue", "self-develop", "what should I build next", and after an issue or a release is finished, because that is when the loop repeats.
version: 0.1.0
---

# Bosun Self-Development

The loop that develops Bosun from its own issue tracker. Run it until the tracker holds nothing shippable, then report and stop.

Load `bosun-development` before designing anything: it points at the standards that govern the code you are about to change.

## The loop

Run these steps in order and repeat from step 1 after step 6.

### 1. Pick

`gh issue list --repo ragnarula/bosun --state open --limit 100`, read the candidates, rank them by the rules in **Ranking** below, and choose one.

Done when you can name the chosen issue and say in one line why it outranks the rest.

### 2. Size it

A **shippable** issue is a bug or a bounded feature: a change you can describe in a few sentences, with no question in the issue left open. Anything larger is a **research** issue.

- Shippable: go to step 3.
- Research: do the reading the issue asks for, then post one comment with what you found, what it costs, what it would break, and the questions only the operator can answer. Ping `@ragnarula`. Change no code, cut no release. Return to step 1 with this issue marked done-for-now.

Done when the issue is either in hand or carries that comment and the ping.

### 3. Build

Work on one issue at a time: a builder child to make the change, a reviewer child to find what is wrong with it, and a fix round until no important finding remains. Keep the ledger that workflow describes — task, what changed, what review found, what was fixed, what was not.

Branch off the default branch unless the operator named one. Commit as you go with messages in the repo's own style: a subject in the imperative and a body that says why.

Done when every requirement in the issue is implemented and the ledger's findings are fixed or named as unfixed.

### 4. Prove it

Tests are not proof. Run the suite (`cargo test --workspace --locked`), `cargo clippy --locked --all-targets --all-features -- -D warnings` and `cargo +nightly fmt --all -- --check`, and then drive the real thing:

- A terminal feature: the real binary against a real control plane and node, captured through `tmux capture-pane`.
- A pane feature: the real page in `/home/dev/.cache/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-linux64/chrome-headless-shell` (`--dump-dom` runs JS), driven over CDP when you need to click or hold a button.
- A provider path: a stub provider on `base_url`, shaped like `crates/bosun-agent/src/test_support.rs`'s `sse_response`.

`cargo deny check` fails on this repository before any change of yours: unmaintained `paste` and `rustls-pemfile`, a rustls advisory, banned `openssl-probe` and `security-framework`, duplicate crates and one licence. Compare against the base commit's error list; only a new line is yours.

Done when you have output from the real thing showing the issue's behaviour, pasted into the ledger, or a plain statement of what you could not run and why.

### 5. Release

Every issue ships as its own release — that is the rule, and it is what makes a deployment finite.

1. Bump `[workspace.package] version` in `Cargo.toml` by one patch.
2. `cargo metadata --format-version 1 > /dev/null` to refresh `Cargo.lock`.
3. Commit it: `Bump version to X.Y.Z`.
4. `git tag vX.Y.Z` (lightweight, matching the existing tags).
5. Push the branch, then the tag: `git push origin <branch> && git push origin vX.Y.Z`.

Done when the tag's workflow finished green: `curl -s "https://api.github.com/repos/ragnarula/bosun/actions/runs?per_page=3"` shows the run for your commit `completed success`, and the release at `releases/tags/vX.Y.Z` carries its assets.

### 6. Deploy, close, repeat

Follow the `bosun-deployment` skill for the release you just cut. Then comment on the issue with the tag and what shipped, close it, and return to step 1.

Done when the issue is closed with its release named, and the next issue is picked.

## Ranking

Sort every open issue into these tiers and take the highest that is not blocked.

1. **Bugs.** Something that used to work, or was meant to, and does not. A `bug` label, or a report that names a broken behaviour, is enough.
2. **Unblocking issues.** Anything that lets this loop run without a human in it: spawning sessions on other nodes, clearing context mid-session, seeing what another session is doing, watching a subagent, forking a conversation. These outrank ordinary features at their own tier because each one removes a reason to stop and wait.
3. **Small features with clear scope.** A bounded change with nothing left to decide.
4. **Research.** Everything else. Answer it with a comment and a ping, never with code.

Within a tier, take the issue that has waited longest, unless a newer one blocks more work.

## What stops the loop

- Every open issue is a research issue whose question is with the operator. Report the questions and stop.
- A shippable issue needs a decision the operator has not made. Ask on the issue, ping `@ragnarula`, and pick a different issue meanwhile.
- Two issues in a row fail on the same cause outside your control — the release workflow, the provider, the machine. Report and stop rather than grind.

## Keep the tracker honest

- Move a research issue forward only when you add something to it.
- If you learn an issue is wrong — already fixed, or asking for the impossible — say so on the issue and close it.
- Never close an issue whose release did not build.
