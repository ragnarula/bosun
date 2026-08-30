use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::stream;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Error)]
pub enum SseError {
    #[error("sse source error")]
    Source(#[from] Box<dyn std::error::Error + Send + Sync>),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

struct SseState<S> {
    source: S,
    buffer: Vec<u8>,
    done: bool,
}

/// Parse a byte stream of Server-Sent Events into blocks. Blocks are
/// delimited by a blank line; a final block without the delimiter is flushed
/// when the source ends.
pub fn sse_stream<S, E>(source: S) -> impl Stream<Item = Result<SseEvent, SseError>>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let state = SseState {
        source,
        buffer: Vec::new(),
        done: false,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if state.done {
                return None;
            }
            if let Some(block) = take_block(&mut state.buffer) {
                if let Some(event) = parse_block(&block) {
                    return Some((Ok(event), state));
                }
                continue;
            }
            match state.source.next().await {
                Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    state.done = true;
                    return Some((Err(SseError::Source(Box::new(error))), state));
                }
                None => {
                    state.done = true;
                    if !state.buffer.is_empty()
                        && let Some(event) = parse_block(&state.buffer)
                    {
                        return Some((Ok(event), state));
                    }
                    return None;
                }
            }
        }
    })
}

/// Cut one complete block (up to and including the blank line) off the
/// buffer. Accepts `\n\n` and `\r\n\r\n` separators.
fn take_block(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let end_nn = find_subslice(buffer, b"\n\n").map(|index| index + 2);
    let end_rnrn = find_subslice(buffer, b"\r\n\r\n").map(|index| index + 4);
    let end = match (end_nn, end_rnrn) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let block = buffer[..end].to_vec();
    buffer.drain(..end);
    Some(block)
}

/// Parse one block's `event:` and `data:` fields, joining repeated `data:`
/// lines with `\n`. Comment lines (`: ...`) and unknown fields are ignored;
/// a block without data is skipped.
fn parse_block(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut event = None;
    let mut data_lines = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }
    let data = data_lines.join("\n");
    if data.is_empty() {
        return None;
    }
    Some(SseEvent { event, data })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn source_of(
        parts: &[&str],
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static {
        let items: Vec<Result<Bytes, reqwest::Error>> = parts
            .iter()
            .map(|part| Ok(Bytes::from(part.to_string())))
            .collect();
        stream::iter(items)
    }

    async fn collect(
        source: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
    ) -> Vec<SseEvent> {
        let events: Vec<Result<SseEvent, SseError>> = sse_stream(source).collect().await;
        events.into_iter().map(|event| event.unwrap()).collect()
    }

    #[tokio::test]
    async fn parses_a_single_block() {
        let events = collect(source_of(&["data: hello\n\n"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "hello".into(),
            }]
        );
    }

    #[tokio::test]
    async fn parses_event_field_and_multiline_data() {
        let events = collect(source_of(&["event: message\ndata: a\ndata: b\n\n"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message".into()),
                data: "a\nb".into(),
            }]
        );
    }

    #[tokio::test]
    async fn joins_blocks_split_across_chunks() {
        let events = collect(source_of(&["data: one\n", "\ndata: two\n", "\n"])).await;
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "one".into(),
                },
                SseEvent {
                    event: None,
                    data: "two".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn handles_crlf_line_endings() {
        let events = collect(source_of(&["data: a\r\ndata: b\r\n\r\n"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "a\nb".into(),
            }]
        );
    }

    #[tokio::test]
    async fn skips_comments_and_unknown_fields() {
        let events = collect(source_of(&[": keep-alive\n\nx-foo: bar\ndata: ok\n\n"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "ok".into(),
            }]
        );
    }

    #[tokio::test]
    async fn skips_a_comment_only_heartbeat_and_continues() {
        let events = collect(source_of(&[": ping\n\n", "data: real\n\n"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "real".into(),
            }]
        );
    }

    #[tokio::test]
    async fn skips_blocks_with_empty_data() {
        let events = collect(source_of(&["data:\n\ndata: real\n\n"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "real".into(),
            }]
        );
    }

    #[tokio::test]
    async fn flushes_the_remaining_block_when_the_source_ends() {
        let events = collect(source_of(&["data: final"])).await;
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "final".into(),
            }]
        );
    }

    #[tokio::test]
    async fn maps_a_source_error_into_an_sse_error() {
        let source = stream::iter(vec![Err::<Bytes, std::io::Error>(std::io::Error::other(
            "boom",
        ))]);
        let events: Vec<Result<SseEvent, SseError>> = sse_stream(source).collect().await;
        assert!(matches!(&events[0], Err(SseError::Source(_))));
    }
}
