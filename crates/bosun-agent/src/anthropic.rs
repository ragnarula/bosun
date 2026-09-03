//! Anthropic Messages API adapter: serializes the canonical call and parses
//! the streamed response back into [`StreamEvent`]s.

use std::collections::HashMap;

use futures_util::stream::BoxStream;
use serde_json::Value;
use serde_json::json;

use crate::provider::Provider;
use crate::provider::ProviderAdapter;
use crate::provider::ProviderCall;
use crate::provider::ProviderError;
use crate::provider::StreamEvent;
use crate::provider::messages_url;
use crate::serialize::anthropic_messages;
use crate::serialize::anthropic_tools;
use crate::sse::SseEvent;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic adapter. `base_url` is the provider root; the adapter appends
/// `/v1/messages`.
pub struct Anthropic {
    inner: ProviderAdapter,
}

impl Anthropic {
    pub fn new(model: &str, api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            inner: ProviderAdapter::new(model, api_key, base_url, DEFAULT_BASE_URL),
        }
    }

    fn request_body(&self, call: &ProviderCall<'_>) -> Value {
        let mut body = anthropic_messages(call.system, &call.messages, call.ask_recipient);
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
        &self.inner.model
    }

    fn chat_stream<'a>(
        &'a self,
        call: ProviderCall<'a>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let request = self
            .inner
            .client
            .post(messages_url(&self.inner.base_url, "messages"))
            .header("x-api-key", &self.inner.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.request_body(&call));
        crate::provider::chat_stream(request, AnthropicParser::default(), parse_event)
    }
}

/// Token counts and in-flight tool call identities carried between SSE
/// events.
#[derive(Default)]
struct AnthropicParser {
    input_tokens: u64,
    output_tokens: u64,
    tool_starts: HashMap<usize, (Option<String>, Option<String>)>,
}

/// Turn one SSE event into [`StreamEvent`]s. Unknown event kinds (ping,
/// content_block_stop) and empty deltas are skipped.
fn parse_event(
    event: &SseEvent,
    parser: &mut AnthropicParser,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let data: Value = serde_json::from_str(&event.data).map_err(|error| ProviderError::Parse {
        detail: format!("anthropic event data is not JSON: {error}"),
    })?;
    let Some(kind) = data["type"].as_str() else {
        return Ok(Vec::new());
    };
    match kind {
        "message_start" => {
            parser.input_tokens = data["message"]["usage"]["input_tokens"]
                .as_u64()
                .unwrap_or(0);
            Ok(Vec::new())
        }
        "content_block_start" => {
            let block = &data["content_block"];
            if block["type"] == "tool_use" {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let id = block["id"].as_str().map(str::to_string);
                let name = block["name"].as_str().map(str::to_string);
                parser.tool_starts.insert(index, (id, name));
            }
            Ok(Vec::new())
        }
        "content_block_delta" => {
            let index = data["index"].as_u64().unwrap_or(0) as usize;
            let delta = &data["delta"];
            match delta["type"].as_str() {
                Some("text_delta") => {
                    let text = delta["text"].as_str().unwrap_or_default();
                    if text.is_empty() {
                        Ok(Vec::new())
                    } else {
                        Ok(vec![StreamEvent::TextDelta(text.to_string())])
                    }
                }
                Some("input_json_delta") => {
                    let (id, name) = parser.tool_starts.remove(&index).unwrap_or((None, None));
                    let args_delta = delta["partial_json"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    if id.is_none() && name.is_none() && args_delta.is_empty() {
                        Ok(Vec::new())
                    } else {
                        Ok(vec![StreamEvent::ToolCallDelta {
                            index,
                            id,
                            name,
                            args_delta,
                        }])
                    }
                }
                _ => Ok(Vec::new()),
            }
        }
        "message_delta" => {
            parser.output_tokens = data["usage"]["output_tokens"].as_u64().unwrap_or(0);
            Ok(Vec::new())
        }
        "message_stop" => Ok(vec![StreamEvent::Stop {
            input_tokens: parser.input_tokens,
            output_tokens: parser.output_tokens,
        }]),
        "error" => {
            let message = data["error"]["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| "unknown error".to_string());
            Err(ProviderError::Parse { detail: message })
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use futures_util::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::test_support::FakeProvider;
    use crate::test_support::assert_non_200_is_an_error_item;
    use crate::test_support::collect_stream;
    use crate::test_support::provider_call;
    use crate::test_support::sse_response;

    fn sse(name: &'static str, data: Value) -> SseEvent {
        SseEvent {
            event: Some(name.into()),
            data: data.to_string(),
        }
    }

    #[tokio::test]
    async fn request_headers_and_body_match_the_provider_shape() {
        let server = FakeProvider::start(|_| sse_response(&[])).await;
        let provider = Anthropic::new("claude-test", "sk-test", Some(&server.url()));
        let events = collect_stream(&provider, provider_call("claude-test")).await;
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

        let events = collect_stream(&provider, provider_call("claude-test")).await;
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

        assert_non_200_is_an_error_item(&provider, provider_call("claude-test")).await;
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

        let mut stream = provider.chat_stream(provider_call("claude-test")).unwrap();
        let item = stream.next().await.unwrap();
        assert!(matches!(item, Err(ProviderError::Parse { detail }) if detail == "Overloaded"));
    }
}
