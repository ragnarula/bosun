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

## Browser Tests

The browser test boots the real control-plane router on a temporary store
seeded with sessions, then runs `crates/bosun-control/tests/browser/pane.py`,
which drives the web pane with Playwright in a phone-sized Chromium. It needs
`python3` with the `playwright` package and Playwright's Chromium, so it is
`#[ignore]`d and runs on demand:

```bash
# Once per machine
python3 -m pip install --user playwright
python3 -m playwright install chromium

cargo test -p bosun-control --test browser -- --ignored --nocapture
```

Each check prints `PASS` or `FAIL`, and the test fails when any check fails.
When Chromium cannot be started, most often because Playwright or its Chromium
is missing, the script prints the error and the install commands above, and
the test fails saying so. The script expects the sessions
`tests/browser.rs` seeds, so it runs only against the server that test starts.
Run it after any change to the pane.

Chromium differs from iOS Safari in three ways the pane handles itself, and the
script makes Chromium behave like Safari so the checks can see the pane's own
handling. It turns scroll anchoring off, so inserting older messages would
move the lines on screen. It takes the hidden session list out of the document
and puts it back, so the list loses its scroll while another screen shows. It
replaces the visual viewport with a stub that reports a keyboard, a viewport
offset and a document scroll, so the checks can see how the pane fits the
screen to the keyboard. The stub is a model, not Safari. It does not model a
second reveal scroll, events that arrive only at the end of the keyboard's
movement, or a scale other than 1, and the checks do not cover the pane's zoom
guard or its `100dvh` height. The viewport report in
`docs/adrs/2026-09-29-one-screen-in-the-document.md` is how a phone's own
numbers are read.

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
