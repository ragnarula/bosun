//! OpenAI Chat Completions adapter: serializes the canonical call and parses
//! the streamed response back into [`StreamEvent`]s.

use std::collections::VecDeque;

use futures_util::StreamExt;
use futures_util::stream;
use futures_util::stream::BoxStream;
use serde_json::Value;
use serde_json::json;

use crate::provider::Provider;
use crate::provider::ProviderCall;
use crate::provider::ProviderError;
use crate::provider::StreamEvent;
use crate::serialize::openai_messages;
use crate::serialize::openai_tools;
use crate::sse::SseError;
use crate::sse::SseEvent;
use crate::sse::sse_stream;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// OpenAI adapter. `base_url` is the provider root; the adapter appends
/// `/v1/chat/completions`.
pub struct OpenAi {
    client: reqwest::Client,
    model: String,
    api_key: String,
    base_url: String,
}

impl OpenAi {
    pub fn new(model: &str, api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).to_string(),
        }
    }

    fn request_body(&self, call: &ProviderCall<'_>) -> Value {
        json!({
            "model": call.model,
            "max_tokens": call.max_tokens,
            "stream": true,
            "messages": openai_messages(call.system, &call.messages),
            "tools": openai_tools(&call.tools),
            "stream_options": { "include_usage": true },
        })
    }
}

impl Provider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn chat_stream<'a>(
        &'a self,
        call: ProviderCall<'a>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let request = self
            .client
            .post(messages_url(&self.base_url))
            .bearer_auth(&self.api_key)
            .json(&self.request_body(&call));
        let events = stream::unfold(
            StreamPhase::Requesting(Box::new(request)),
            |mut phase| async move {
                loop {
                    match phase {
                        StreamPhase::Done => return None,
                        StreamPhase::Requesting(request) => match request.send().await {
                            Ok(response) if response.status().is_success() => {
                                phase = StreamPhase::Streaming {
                                    events: Box::pin(sse_stream(response.bytes_stream())),
                                    parser: OpenAiParser::default(),
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
                            mut pending,
                        } => {
                            if pending.is_empty() {
                                match events.next().await {
                                    Some(Ok(event)) => {
                                        let parsed = parse_event(&event, &mut parser);
                                        match parsed {
                                            Ok(stream_events) => pending = stream_events.into(),
                                            Err(error) => {
                                                return Some((Err(error), StreamPhase::Done));
                                            }
                                        }
                                    }
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
                                    pending,
                                };
                                return Some((Ok(stream_event), phase));
                            }
                            phase = StreamPhase::Streaming {
                                events,
                                parser,
                                pending,
                            };
                        }
                    }
                }
            },
        );
        Ok(Box::pin(events))
    }
}

/// Build the chat completions URL from a provider root. A trailing slash is
/// trimmed and an already-`/v1` root is not duplicated.
fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

enum StreamPhase {
    Requesting(Box<reqwest::RequestBuilder>),
    Streaming {
        events: BoxStream<'static, Result<SseEvent, SseError>>,
        parser: OpenAiParser,
        pending: VecDeque<StreamEvent>,
    },
    Done,
}

/// Token counts and the stop guard shared by the usage chunk and the
/// `[DONE]` marker, so a completion emits exactly one [`StreamEvent::Stop`].
#[derive(Default)]
struct OpenAiParser {
    input_tokens: u64,
    output_tokens: u64,
    stopped: bool,
}

impl OpenAiParser {
    fn stop(&mut self) -> Option<StreamEvent> {
        if self.stopped {
            return None;
        }
        self.stopped = true;
        Some(StreamEvent::Stop {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        })
    }
}

/// Turn one SSE chunk into [`StreamEvent`]s. The `[DONE]` marker and the
/// final usage chunk both stop the completion; empty deltas are skipped.
/// A chunk may carry several tool call fragments, one per index.
fn parse_event(
    event: &SseEvent,
    parser: &mut OpenAiParser,
) -> Result<Vec<StreamEvent>, ProviderError> {
    if event.data == "[DONE]" {
        return Ok(parser.stop().into_iter().collect());
    }
    let chunk: Value = serde_json::from_str(&event.data).map_err(|error| ProviderError::Parse {
        detail: format!("openai event data is not JSON: {error}"),
    })?;
    if let Some(usage) = chunk.get("usage")
        && !usage.is_null()
    {
        parser.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
        parser.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        return Ok(parser.stop().into_iter().collect());
    }
    let Some(choices) = chunk["choices"].as_array() else {
        return Ok(Vec::new());
    };
    for choice in choices {
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            return Ok(vec![StreamEvent::TextDelta(text.to_string())]);
        }
        let Some(calls) = delta["tool_calls"].as_array() else {
            continue;
        };
        let mut tool_events = Vec::new();
        for call in calls {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            let id = call["id"].as_str().map(str::to_string);
            let name = call["function"]["name"].as_str().map(str::to_string);
            let args_delta = call["function"]["arguments"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if id.is_none() && name.is_none() && args_delta.is_empty() {
                continue;
            }
            tool_events.push(StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            });
        }
        if !tool_events.is_empty() {
            return Ok(tool_events);
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use bosun_common::session::Block;
    use bosun_common::session::Message;
    use bosun_common::session::Role;
    use bosun_common::tool::ToolSpec;
    use serde_json::json;

    use super::*;
    use crate::test_support::FakeProvider;
    use crate::test_support::sse_response;

    fn call() -> ProviderCall<'static> {
        ProviderCall {
            model: "gpt-test",
            max_tokens: 100,
            system: "You are Bosun.",
            messages: vec![Message {
                role: Role::User,
                block: Block::Text {
                    text: "hello".into(),
                },
            }],
            tools: vec![ToolSpec {
                name: "shell".into(),
                description: "Run a command.".into(),
                schema: json!({"type": "object"}),
            }],
        }
    }

    fn sse(data: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    async fn collect(provider: &OpenAi, call: ProviderCall<'_>) -> Vec<StreamEvent> {
        let mut stream = provider.chat_stream(call).unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    }

    #[tokio::test]
    async fn request_headers_and_body_match_the_provider_shape() {
        let server = FakeProvider::start(|_| sse_response(&[])).await;
        let provider = OpenAi::new("gpt-test", "sk-test", Some(&server.url()));

        let events = collect(&provider, call()).await;
        assert!(events.is_empty());

        let captured = server.captured();
        assert_eq!(captured.path, "/v1/chat/completions");
        assert_eq!(captured.headers["authorization"], "Bearer sk-test");
        let body = captured.body;
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({ "include_usage": true }));
        assert_eq!(
            body["messages"],
            json!([
                { "role": "system", "content": "You are Bosun." },
                { "role": "user", "content": "hello" },
            ])
        );
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[tokio::test]
    async fn streams_text_and_tool_call_deltas_to_a_single_stop() {
        let server_events = vec![
            sse(json!({
                "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" } }],
            })),
            sse(json!({
                "choices": [{ "index": 0, "delta": { "content": "Hel" } }],
            })),
            sse(json!({
                "choices": [{ "index": 0, "delta": { "content": "lo" } }],
            })),
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "shell", "arguments": "" },
                        }],
                    },
                }],
            })),
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": { "arguments": "{\"command\":" },
                        }],
                    },
                }],
            })),
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": { "arguments": "\"ls\"}" },
                        }],
                    },
                }],
            })),
            sse(json!({
                "choices": [],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
            })),
            sse(json!("[DONE]")),
        ];
        let server = FakeProvider::start(move |_| sse_response(&server_events)).await;
        let provider = OpenAi::new("gpt-test", "sk-test", Some(&server.url()));

        let events = collect(&provider, call()).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hel".into()),
                StreamEvent::TextDelta("lo".into()),
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("shell".into()),
                    args_delta: "".into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    args_delta: "{\"command\":".into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    args_delta: "\"ls\"}".into(),
                },
                StreamEvent::Stop {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            ]
        );
    }

    #[tokio::test]
    async fn non_200_becomes_an_error_item() {
        let server = FakeProvider::start(|_| {
            (axum::http::StatusCode::BAD_REQUEST, "bad request").into_response()
        })
        .await;
        let provider = OpenAi::new("gpt-test", "sk-test", Some(&server.url()));

        let mut stream = provider.chat_stream(call()).unwrap();
        let item = stream.next().await.unwrap();
        assert!(matches!(item, Err(ProviderError::Non200 { .. })));
    }

    #[tokio::test]
    async fn a_delta_with_two_tool_calls_emits_a_delta_for_each() {
        let server_events = vec![
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "a",
                                "type": "function",
                                "function": { "name": "shell", "arguments": "{\"c" },
                            },
                            {
                                "index": 1,
                                "id": "b",
                                "type": "function",
                                "function": { "name": "file/read", "arguments": "{\"p" },
                            },
                        ],
                    },
                }],
            })),
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": { "arguments": "ommand\": \"ls\"}" },
                        }],
                    },
                }],
            })),
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 1,
                            "function": { "arguments": "ath\": \"/tmp/\"}" },
                        }],
                    },
                }],
            })),
            sse(json!({
                "choices": [],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 },
            })),
            sse(json!("[DONE]")),
        ];
        let server = FakeProvider::start(move |_| sse_response(&server_events)).await;
        let provider = OpenAi::new("gpt-test", "sk-test", Some(&server.url()));

        let events = collect(&provider, call()).await;
        let deltas: Vec<StreamEvent> = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ToolCallDelta { .. }))
            .cloned()
            .collect();
        assert_eq!(
            deltas,
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("a".into()),
                    name: Some("shell".into()),
                    args_delta: "{\"c".into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("b".into()),
                    name: Some("file/read".into()),
                    args_delta: "{\"p".into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    args_delta: "ommand\": \"ls\"}".into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: None,
                    name: None,
                    args_delta: "ath\": \"/tmp/\"}".into(),
                },
            ]
        );

        let mut joined: Vec<(usize, String)> = Vec::new();
        for delta in deltas {
            if let StreamEvent::ToolCallDelta {
                index, args_delta, ..
            } = delta
            {
                match joined.iter_mut().find(|(i, _)| *i == index) {
                    Some((_, args)) => args.push_str(&args_delta),
                    None => joined.push((index, args_delta)),
                }
            }
        }
        assert_eq!(
            joined,
            vec![
                (0, "{\"command\": \"ls\"}".to_string()),
                (1, "{\"path\": \"/tmp/\"}".to_string())
            ]
        );
        assert!(matches!(events.last(), Some(StreamEvent::Stop { .. })));
    }

    #[test]
    fn messages_url_normalizes_the_base_url() {
        assert_eq!(
            messages_url("https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://api.openai.com/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            messages_url("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
    }
}
