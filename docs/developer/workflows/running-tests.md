# Running Tests

How to run the test suites locally. For which kind of test to write, see [writing-tests.md](../writing-tests.md).

## Run All Tests

```bash
# Rust unit tests (fast, no dependencies needed)
cargo test
```

## Formatting and Linting

```bash
# Format check (nightly rustfmt)
cargo +nightly fmt --check

# Lint with clippy
cargo clippy --locked --all-targets --all-features -- -D warnings
```

## E2E Test

The end-to-end tests boot the control plane and a node over HTTPS with a
self-signed certificate, spawn a session on a local repo, drive it through the
session API, and stop it. They need `git` on PATH (no model key: the tests use a
model configured against an unreachable provider address), so they are
`#[ignore]`d and run on demand:

```bash
cargo test -p bosun --test e2e -- --ignored --nocapture
```

The test uses temp directories and cleans up after itself; it does not touch a
real deployment.

## Debugging a Failing Test

```bash
# Stop on first failure, show test output
cargo test test_name -- --nocapture
```

To get log output from a Rust test, install a subscriber in the test and set `RUST_LOG`:

```rust
use tracing_subscriber::EnvFilter;

tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::from_default_env())
    .init();
```

```bash
RUST_LOG=debug cargo test test_name -- --nocapture
```
