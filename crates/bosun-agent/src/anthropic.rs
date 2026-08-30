//! Anthropic Messages API adapter: serializes the canonical call and parses
//! the streamed response back into [`StreamEvent`]s.

use std::collections::HashMap;

use futures_util::StreamExt;
use futures_util::stream;
use futures_util::stream::BoxStream;
use serde_json::Value;
use serde_json::json;

use crate::provider::Provider;
use crate::provider::ProviderCall;
use crate::provider::ProviderError;
use crate::provider::StreamEvent;
use crate::serialize::anthropic_messages;
use crate::serialize::anthropic_tools;
use crate::sse::SseError;
use crate::sse::SseEvent;
use crate::sse::sse_stream;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic adapter. `base_url` is the provider root; the adapter appends
/// `/v1/messages`.
pub struct Anthropic {
    client: reqwest::Client,
    model: String,
    api_key: String,
    base_url: String,
}

impl Anthropic {
    pub fn new(model: &str, api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).to_string(),
        }
    }

    fn request_body(&self, call: &ProviderCall<'_>) -> Value {
        let mut body = anthropic_messages(call.system, &call.messages);
        body["model"] = json!(call.model);
        body["max_tokens"] = json!(call.max_tokens);
        body["stream"] = json!(true);
        body["tools"] = anthropic_tools(&call.tools);
        body
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
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
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.request_body(&call));
        let events = stream::unfold(StreamPhase::Requesting(request), |mut phase| async move {
            loop {
                match phase {
                    StreamPhase::Done => return None,
                    StreamPhase::Requesting(request) => match request.send().await {
                        Ok(response) if response.status().is_success() => {
                            phase = StreamPhase::Streaming {
                                events: Box::pin(sse_stream(response.bytes_stream())),
                                parser: AnthropicParser::default(),
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
                            return Some((Err(ProviderError::Request(error)), StreamPhase::Done));
                        }
                    },
                    StreamPhase::Streaming {
                        mut events,
                        mut parser,
                    } => match events.next().await {
                        Some(Ok(event)) => {
                            let parsed = parse_event(&event, &mut parser);
                            phase = StreamPhase::Streaming { events, parser };
                            match parsed {
                                Ok(Some(stream_event)) => {
                                    return Some((Ok(stream_event), phase));
                                }
                                Ok(None) => {}
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
                    },
                }
            }
        });
        Ok(Box::pin(events))
    }
}

/// Build the messages URL from a provider root. A trailing slash is trimmed
/// and an already-`/v1` root is not duplicated.
fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

enum StreamPhase {
    Requesting(reqwest::RequestBuilder),
    Streaming {
        events: BoxStream<'static, Result<SseEvent, SseError>>,
        parser: AnthropicParser,
    },
    Done,
}

/// Token counts and in-flight tool call identities carried between SSE
/// events.
#[derive(Default)]
struct AnthropicParser {
    input_tokens: u64,
    output_tokens: u64,
    tool_starts: HashMap<usize, (Option<String>, Option<String>)>,
}

/// Turn one SSE event into a [`StreamEvent`]. Unknown event kinds (ping,
/// content_block_stop) and empty deltas are skipped.
fn parse_event(
    event: &SseEvent,
    parser: &mut AnthropicParser,
) -> Result<Option<StreamEvent>, ProviderError> {
    let data: Value = serde_json::from_str(&event.data).map_err(|error| ProviderError::Parse {
        detail: format!("anthropic event data is not JSON: {error}"),
    })?;
    let Some(kind) = data["type"].as_str() else {
        return Ok(None);
    };
    match kind {
        "message_start" => {
            parser.input_tokens = data["message"]["usage"]["input_tokens"]
                .as_u64()
                .unwrap_or(0);
            Ok(None)
        }
        "content_block_start" => {
            let block = &data["content_block"];
            if block["type"] == "tool_use" {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let id = block["id"].as_str().map(str::to_string);
                let name = block["name"].as_str().map(str::to_string);
                parser.tool_starts.insert(index, (id, name));
            }
            Ok(None)
        }
        "content_block_delta" => {
            let index = data["index"].as_u64().unwrap_or(0) as usize;
            let delta = &data["delta"];
            match delta["type"].as_str() {
                Some("text_delta") => {
                    let text = delta["text"].as_str().unwrap_or_default();
                    if text.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(StreamEvent::TextDelta(text.to_string())))
                    }
                }
                Some("input_json_delta") => {
                    let (id, name) = parser.tool_starts.remove(&index).unwrap_or((None, None));
                    let args_delta = delta["partial_json"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    if id.is_none() && name.is_none() && args_delta.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(StreamEvent::ToolCallDelta {
                            index,
                            id,
                            name,
                            args_delta,
                        }))
                    }
                }
                _ => Ok(None),
            }
        }
        "message_delta" => {
            parser.output_tokens = data["usage"]["output_tokens"].as_u64().unwrap_or(0);
            Ok(None)
        }
        "message_stop" => Ok(Some(StreamEvent::Stop {
            input_tokens: parser.input_tokens,
            output_tokens: parser.output_tokens,
        })),
        "error" => {
            let message = data["error"]["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| "unknown error".to_string());
            Err(ProviderError::Parse { detail: message })
        }
        _ => Ok(None),
    }
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
            model: "claude-test",
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

    fn sse(name: &'static str, data: Value) -> SseEvent {
        SseEvent {
            event: Some(name.into()),
            data: data.to_string(),
        }
    }

    async fn collect(provider: &Anthropic, call: ProviderCall<'_>) -> Vec<StreamEvent> {
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
        let provider = Anthropic::new("claude-test", "sk-test", Some(&server.url()));
        let events = collect(&provider, call()).await;
        assert!(events.is_empty());

        let captured = server.captured();
        assert_eq!(captured.path, "/v1/messages");
        assert_eq!(captured.headers["x-api-key"], "sk-test");
        assert_eq!(captured.headers["anthropic-version"], "2023-06-01");
        let body = captured.body;
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "You are Bosun.");
        assert_eq!(
            body["messages"],
            json!([{ "type": "text", "text": "hello" }])
        );
        assert_eq!(body["tools"][0]["name"], "shell");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[tokio::test]
    async fn streams_text_and_tool_call_deltas_to_stop() {
        let server_events = vec![
            sse("ping", json!({ "type": "ping" })),
            sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": { "usage": { "input_tokens": 25, "output_tokens": 1 } },
                }),
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" },
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "Hel" },
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "lo" },
                }),
            ),
            sse(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }),
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "shell",
                        "input": {},
                    },
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"command\":" },
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": "\"ls\"}" },
                }),
            ),
            sse(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": { "output_tokens": 30 },
                }),
            ),
            sse("message_stop", json!({ "type": "message_stop" })),
        ];
        let server = FakeProvider::start(move |_| sse_response(&server_events)).await;
        let provider = Anthropic::new("claude-test", "sk-test", Some(&server.url()));

        let events = collect(&provider, call()).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hel".into()),
                StreamEvent::TextDelta("lo".into()),
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("toolu_1".into()),
                    name: Some("shell".into()),
                    args_delta: "{\"command\":".into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: None,
                    name: None,
                    args_delta: "\"ls\"}".into(),
                },
                StreamEvent::Stop {
                    input_tokens: 25,
                    output_tokens: 30,
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
        let provider = Anthropic::new("claude-test", "sk-test", Some(&server.url()));

        let mut stream = provider.chat_stream(call()).unwrap();
        let item = stream.next().await.unwrap();
        assert!(matches!(item, Err(ProviderError::Non200 { .. })));
    }

    #[tokio::test]
    async fn mid_stream_error_becomes_a_parse_error() {
        let server_events = vec![
            sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": { "usage": { "input_tokens": 3, "output_tokens": 0 } },
                }),
            ),
            sse(
                "error",
                json!({
                    "type": "error",
                    "error": { "type": "overloaded_error", "message": "Overloaded" },
                }),
            ),
        ];
        let server = FakeProvider::start(move |_| sse_response(&server_events)).await;
        let provider = Anthropic::new("claude-test", "sk-test", Some(&server.url()));

        let mut stream = provider.chat_stream(call()).unwrap();
        let item = stream.next().await.unwrap();
        assert!(matches!(item, Err(ProviderError::Parse { detail }) if detail == "Overloaded"));
    }

    #[test]
    fn messages_url_normalizes_the_base_url() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://example.com/v1"),
            "https://example.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://example.com/v1/"),
            "https://example.com/v1/messages"
        );
    }
}
