use std::collections::VecDeque;

use bosun_common::session::Message;
use bosun_common::tool::ToolSpec;
use futures_util::StreamExt;
use futures_util::stream;
use futures_util::stream::BoxStream;
use thiserror::Error;

use crate::sse::SseError;
use crate::sse::SseEvent;
use crate::sse::sse_stream;

/// The output-token budget a completion gets unless the model config raises
/// it. Reasoning models consume this budget on thinking, so the default is
/// generous enough that thinking and the reply both fit.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32768;

/// One canonical provider request. The provider adapter serializes it and
/// parses the streamed response back into [`StreamEvent`].
pub struct ProviderCall<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a str,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        args_delta: String,
    },
    Stop {
        input_tokens: u64,
        output_tokens: u64,
    },
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unsupported provider: {name}")]
    UnsupportedProvider { name: String },
    #[error("environment variable {var} is not set")]
    MissingEnvVar { var: String },
    #[error("provider returned {status}: {body}")]
    Non200 { status: String, body: String },
    #[error("provider request failed")]
    Request(#[from] reqwest::Error),
    #[error("invalid provider response")]
    Parse { detail: String },
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    /// The output-token budget this model's completions may consume.
    fn max_output_tokens(&self) -> u32 {
        DEFAULT_MAX_OUTPUT_TOKENS
    }
    /// The thinking budget a reasoning model may spend, when it exposes one.
    fn thinking_budget(&self) -> Option<u32> {
        None
    }
    fn chat_stream<'a>(
        &'a self,
        call: ProviderCall<'a>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

/// Provider state shared by the adapters: HTTP client, model, API key and
/// provider root URL.
pub(crate) struct ProviderAdapter {
    pub(crate) client: reqwest::Client,
    pub(crate) model: String,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
}

impl ProviderAdapter {
    pub(crate) fn new(
        model: &str,
        api_key: &str,
        base_url: Option<&str>,
        default_base_url: &str,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            base_url: base_url.unwrap_or(default_base_url).to_string(),
        }
    }
}

/// Build the provider API endpoint URL from a provider root. A trailing slash
/// is trimmed and an already-`/v1` root is not duplicated.
pub(crate) fn messages_url(base_url: &str, endpoint: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/{endpoint}")
    } else {
        format!("{base}/v1/{endpoint}")
    }
}

/// Request/stream state machine shared by the provider adapters.
enum StreamPhase<P> {
    Requesting(Box<reqwest::RequestBuilder>),
    Streaming {
        events: BoxStream<'static, Result<SseEvent, SseError>>,
        parser: P,
        parse: fn(&SseEvent, &mut P) -> Result<Vec<StreamEvent>, ProviderError>,
        pending: VecDeque<StreamEvent>,
    },
    Done,
}

/// Send the request and parse the SSE response through `parse`, yielding the
/// resulting [`StreamEvent`]s.
pub(crate) fn chat_stream<P>(
    request: reqwest::RequestBuilder,
    parser: P,
    parse: fn(&SseEvent, &mut P) -> Result<Vec<StreamEvent>, ProviderError>,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>
where
    P: Send + 'static,
{
    // `stream::unfold` needs a FnMut closure, so the initial parser rides in a
    // one-shot slot and moves into the stream state on the first request.
    let mut parser_slot = Some(parser);
    let events = stream::unfold(
        StreamPhase::Requesting(Box::new(request)),
        move |mut phase| {
            let mut parser = parser_slot.take();
            async move {
                loop {
                    match phase {
                        StreamPhase::Done => return None,
                        StreamPhase::Requesting(request) => match request.send().await {
                            Ok(response) if response.status().is_success() => {
                                phase = StreamPhase::Streaming {
                                    events: Box::pin(sse_stream(response.bytes_stream())),
                                    parser: parser
                                        .take()
                                        .expect("the first request supplies the parser"),
                                    parse,
                                    pending: VecDeque::new(),
                                };
                            }
                            Ok(response) => {
                                let status = response.status().to_string();
                                let body = response.text().await.unwrap_or_default();
                                return Some((
                                    Err(ProviderError::Non200 { status, body }),
                                    StreamPhase::Done,
                                ));
                            }
                            Err(error) => {
                                return Some((
                                    Err(ProviderError::Request(error)),
                                    StreamPhase::Done,
                                ));
                            }
                        },
                        StreamPhase::Streaming {
                            mut events,
                            mut parser,
                            parse,
                            mut pending,
                        } => {
                            if pending.is_empty() {
                                match events.next().await {
                                    Some(Ok(event)) => match parse(&event, &mut parser) {
                                        Ok(stream_events) => pending = stream_events.into(),
                                        Err(error) => {
                                            return Some((Err(error), StreamPhase::Done));
                                        }
                                    },
                                    Some(Err(error)) => {
                                        return Some((
                                            Err(ProviderError::Internal(error.into())),
                                            StreamPhase::Done,
                                        ));
                                    }
                                    None => return None,
                                }
                            }
                            if let Some(stream_event) = pending.pop_front() {
                                phase = StreamPhase::Streaming {
                                    events,
                                    parser,
                                    parse,
                                    pending,
                                };
                                return Some((Ok(stream_event), phase));
                            }
                            phase = StreamPhase::Streaming {
                                events,
                                parser,
                                parse,
                                pending,
                            };
                        }
                    }
                }
            }
        },
    );
    Ok(Box::pin(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_normalizes_the_base_url() {
        assert_eq!(
            messages_url("https://api.openai.com", "chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://api.openai.com/", "chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://openrouter.ai/api/v1", "chat/completions"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://openrouter.ai/api/v1/", "chat/completions"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com", "messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/", "messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://example.com/v1", "messages"),
            "https://example.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://example.com/v1/", "messages"),
            "https://example.com/v1/messages"
        );
    }
}
