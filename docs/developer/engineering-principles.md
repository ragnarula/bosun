# Engineering Principles

These shape every change. They follow from where Bosun runs: on machines you own, often left unattended, with nobody to restart a process that has stopped.

## Engineer for performance

Avoid unnecessary buffer copies, serialization and allocations on the hot path.

## Engineer for correctness by construction

Choose data structures and process ordering that make correctness easy to reason about, rather than correctness you have to check for.

## Engineer for simplicity

Avoid object-oriented and trait-heavy patterns in business logic. Prefer C-style functions plus POD data. "Shaping business logic" below gives the concrete rules.

## Engineer for determinism

Resource costs should be known and bounded for every process. A client must not be able to grow resource use without limit — memory above all.

## Engineer for fault isolation

One key, file, store or peer failing must fail locally and cleanly. Nothing a single client does may take down the control plane or another session. A machine left alone has nobody to restart it.

## Engineer for observability

A problem is diagnosed from telemetry that was already emitted — you cannot go back and add a log line. See [logging.md](./logging.md).

---

# Shaping business logic

## Prefer linear flows over service objects

If steps are sequential — A must finish before B starts — write them sequentially in the calling function. Wrapping them in a service object with `start()`/`shutdown()` hides the execution order and adds coordination machinery (`Notify`, `AtomicBool`, `OnceCell`) that would not exist if the code were written top to bottom.

```
Bad:  spawn task A, spawn task B, have B wait_for_a_complete().await
Good: do_a().await; do_b().await;
```

Introduce concurrency only where there is genuine parallelism: two things that run at the same time, neither waiting for the other.

## Pass data, not services

If a component needs a UUID, a key, or a config value from another service, pass that value. Do not pass `Arc<WholeService>` so it can call `.node_name()`. Passing the value reduces coupling and makes the dependency visible in the signature.

## Add lifecycle management only when there is a lifecycle

A struct created once at startup and dropped at shutdown does not need `start()`/`shutdown()` methods. A `CancellationToken` and a `JoinHandle` in the calling function are enough. Reserve lifecycle wrappers for components created and destroyed dynamically, or with genuinely complex teardown.

## Leave no dead code behind

Dead code is acceptable only while the intermediate phases of one piece of work are in flight, marked `#[allow(dead_code)]`. When the work is complete, no dead code from it should remain and every `#[allow(dead_code)]` it added should be gone.
