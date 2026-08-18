# Error Handling

## Core Rules

1. **`thiserror` enums.** Every error type is an enum deriving `thiserror::Error`.
2. **A variant only for what a caller can handle.** Everything else goes in `Internal`.
3. **Add context, do not wrap.** Use `.context()` / `.with_context()` while propagating.
4. **Log where the error is handled**, never where it is raised, using `display_chain()`.
5. **A spawned task logs its own errors.** Nobody else can.

## Defining an Error Type

`Internal` comes last and absorbs everything a caller cannot act on. Its `#[from] anyhow::Error` is what lets `.context()?` convert automatically.

```rust
#[derive(Debug, Error)]
pub enum MyError {
    #[error("resource was not found")]
    NotFound,

    #[error("input exceeded maximum size of {max} bytes")]
    TooLarge { max: usize },

    #[error("network error")]
    Network(#[from] NetworkError),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}
```

Give a variant to an error the caller can recover from, one that maps to a specific HTTP status, or one that needs handling distinct from its neighbours. Give no variant to I/O, serialization, or database-connection failures — they all mean "something went wrong internally", and a caller cannot do anything different with each.

## Adding Context

Add context when crossing an abstraction boundary, or when a runtime value makes the failure diagnosable. Skip it when the original error already says everything.

```rust
let config = read_config(&path)
    .with_context(|| format!("failed to read config from {}", path.display()))?;

let n: i32 = s.parse().context("failed to parse string as integer")?;  // pointless
```

## Logging Errors

`display_chain()` from `bosun_common::error::ErrorExt` prints the whole chain:

```
operation failed: failed to store chunk
    caused by: failed to write to disk
    caused by: No space left on device (os error 28)
```

Log at the site that handles the error. An inner function that logs and then returns the error produces the same failure twice in the log, at two different levels of detail.

```rust
// Good: the caller decided what to do, so the caller logs
if let Err(e) = inner() {
    error!("inner failed: {}", e.display_chain());
}
```

For REST endpoints the handling site is `IntoResponse`. Log the full chain there, and keep `Internal` details out of the response body:

```rust
impl IntoResponse for MyError {
    fn into_response(self) -> Response {
        let (status, text) = match &self {
            MyError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
            MyError::NotFound => (StatusCode::NOT_FOUND, Some(self.to_string())),
            e => (StatusCode::BAD_REQUEST, Some(e.to_string())),
        };

        tracing::error!("error: {}", self.display_chain());

        match text {
            Some(text) => (status, text).into_response(),
            None => status.into_response(),
        }
    }
}
```

## Spawned Tasks

A fire-and-forget task is the last place its errors can be seen. Put the fallible work in an inner block so one `?` chain covers it, then log the result:

```rust
tokio::spawn(async move {
    let result = async {
        let data = fetch_data().await.context("failed to fetch data")?;
        process_data(data).await.context("failed to process data")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        error!("background task failed: {}", e.display_chain());
    }
});
```
