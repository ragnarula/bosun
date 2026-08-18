# Developer Setup

This guide covers setting up your local development environment for Bosun.

## Prerequisites

- Rust stable (edition 2024)
- Rust nightly, for formatting

## Install Rust Toolchains

```bash
# Install stable and nightly toolchains
rustup install stable nightly

# Ensure nightly rustfmt is available (required for format checks)
rustup component add rustfmt --toolchain nightly
rustup component add clippy
```

## Pre-commit Hooks

We use [pre-commit](https://pre-commit.com/) to run validation before each commit. This catches issues locally before they reach CI.

### Install pre-commit

```bash
# Using uv (recommended if you have it)
uv tool install pre-commit

# Or using brew on macOS
brew install pre-commit
```

### Enable hooks

```bash
# Install the git hooks
pre-commit install
```

### What the hooks check

Every hook runs on commit. There is no pre-push stage.

| Hook | Description |
|---|---|
| `trailing-whitespace` | Remove trailing whitespace |
| `end-of-file-fixer` | Ensure files end with newline |
| `check-yaml` | Validate YAML syntax |
| `check-toml` | Validate TOML syntax |
| `check-merge-conflict` | Detect merge conflict markers |
| `check-added-large-files` | Prevent committing files >500KB |
| `cargo-fmt` | Format Rust code (nightly) |
| `cargo-clippy` | Lint Rust code |

`.pre-commit-config.yaml` is the source of truth.

### Skipping hooks (when necessary)

```bash
# Skip all hooks for a single commit
git commit --no-verify -m "WIP: work in progress"

# Skip specific hooks
SKIP=cargo-clippy git commit -m "Quick fix"
```

## Building

```bash
# Debug build
cargo build --workspace

# Release build
cargo build --workspace --release
```

## Running Tests

See [running-tests.md](./running-tests.md). For which kind of test to write, see [writing-tests.md](../writing-tests.md).

## Running Bosun Locally

See [local-development.md](./local-development.md).
