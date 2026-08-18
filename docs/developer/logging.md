# Logging

We use the `tracing` crate for structured logging with span-based context.

## Log Levels

| Level | When to Use |
|-------|-------------|
| `error!` | Operation failed and requires attention |
| `warn!` | Unexpected condition that was handled |
| `info!` | High-level operational events (startup, connections, requests) |
| `debug!` | Detailed flow useful for debugging |
| `trace!` | Very detailed diagnostics (byte-level, per-iteration) |

## Structured Fields

Use `key = value` rather than interpolating into the message. Interpolated values cannot be queried.

```rust
// Good
debug!(msg = "handling request", request_id = %id, session_id = %session, action = "spawn");

// Avoid
debug!("handling request {} for session {} action spawn", id, session);
```

Prefix a field with `%` for `Display` and `?` for `Debug`.

Never log secrets, tokens, or content bytes. Log identifiers instead — a node name and session id, not the API key they address.

## Spans

Put `#[instrument]` on **boundaries**:

- Incoming request and connection handlers (HTTP, proxy listeners)
- Wrappers around outgoing calls (control-plane calls, node commands, spawned processes)
- Background task entry points

Do not put it on internal helpers already running inside an instrumented span, on trivial utilities, or on getters and formatters.

Skip arguments that are large or sensitive, since every field appears in the span:

```rust
#[instrument(skip(template))]
async fn spawn_session(node: &str, repo: &str, template: &ConfigTemplate) -> Result<Session, Error> { }

#[instrument(skip_all)]
async fn forward_bytes(stream: TcpStream, target: SocketAddr) -> Result<(), Error> { }
```

### Spawned Tasks

A spawned task does not inherit the parent span. Choose by whether its work belongs to the current operation:

| The task | Do this |
|---|---|
| Works on behalf of the current request, and is short-lived | `.in_current_span()` on the future |
| Handles its own events, or outlives the parent | Nothing — put `#[instrument]` on the function it calls |
| Needs its own named span with fields | `.instrument(info_span!("name", field))` |

## Initialization

Call `bosun_common::telemetry::setup_logging` once at startup:

```rust
setup_logging(cli.log_filter.as_deref())?;
```

Three sources set the filter, and the first one present wins: the `--log-filter` value passed above, then `RUST_LOG`, then `info`. A filter replaces the configured default rather than extending it.

To set a filter when running a binary, see [workflows/local-development.md](./workflows/local-development.md).
