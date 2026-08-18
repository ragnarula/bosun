# Writing Tests

## Target risk at the cheapest tier that can find the fault

Test effort follows risk, not line count. Business logic — session state, spawn ordering, port allocation — earns thorough tests. Plumbing and trivial code earn few or none. A test that restates the implementation costs maintenance and finds nothing.

Then split by what you are checking:

- **Edge cases, variants, boundaries and error paths → unit tests.** They are cheap, so be exhaustive.
- **Happy paths → tests that run against real things.** Mocks cannot tell you whether this works wired to a real dependency or assembled into the binary we ship, and it is not worth asking twenty times over.

**Assert on outcomes, not on wording.** Check `Ok` or `Err` and the error variant, never a substring of the message. Message text is presentation: it changes for readability and the test fails for no reason, or it stops matching a real regression and passes for no reason.

## The Tiers

Cheapest first. Cost tracks how much must be standing before the test can run.

| Tier | Answers | Location | Needs |
|---|---|---|---|
| Unit | Is this logic correct across all its cases? | Inline `#[cfg(test)]` | Nothing |
| Integration | Does this Rust API work against a real external dependency (git, opencode)? | `crates/bosun-integration-tests` | One dependency |
| Smoke | Is this API present in the binary and responding as expected? | `tests/smoke` | Built binaries, running services |

Edge cases belong in unit tests. Integration and smoke cover happy and common paths only.

## Polling for Async State

Poll for a condition instead of sleeping. Fixed sleeps are flaky on slow CI, wasteful on fast machines, and opaque — a failure does not say whether the timeout was wrong or the code is broken.

```rust
// BAD
sleep(Duration::from_millis(500)).await;
assert_eq!(manager.session_count(), 2);

// GOOD
wait_for_condition(
    || async { manager.session_count() == 2 },
    Duration::from_secs(5),
)
.await
.expect("sessions should appear within 5 seconds");
```

Fixed sleeps are acceptable only for time-based behaviour such as TTL expiry, or pauses under 20ms to let a spawned task start.
