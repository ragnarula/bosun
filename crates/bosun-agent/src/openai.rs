//! OpenAI Chat Completions adapter: serializes the canonical call and parses
//! the streamed response back into [`StreamEvent`]s.

use futures_util::stream::BoxStream;
use serde_json::Value;
use serde_json::json;

use crate::provider::Provider;
use crate::provider::ProviderAdapter;
use crate::provider::ProviderCall;
use crate::provider::ProviderError;
use crate::provider::StopReason;
use crate::provider::StreamEvent;
use crate::provider::messages_url;
use crate::serialize::openai_messages;
use crate::serialize::openai_tools;
use crate::sse::SseEvent;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// OpenAI adapter. `base_url` is the provider root; the adapter appends
/// `/v1/chat/completions`.
pub struct OpenAi {
    inner: ProviderAdapter,
    max_output_tokens: u32,
}

impl OpenAi {
    pub fn new(model: &str, api_key: &str, base_url: Option<&str>, max_output_tokens: u32) -> Self {
        Self {
            inner: ProviderAdapter::new(model, api_key, base_url, DEFAULT_BASE_URL),
            max_output_tokens,
        }
    }

    fn request_body(&self, call: &ProviderCall<'_>) -> Value {
        json!({
            "model": call.model,
            "max_tokens": call.max_tokens,
            "stream": true,
            "messages": openai_messages(call.system, &call.messages, call.ask_recipient),
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
        &self.inner.model
    }

    fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    fn chat_stream<'a>(
        &'a self,
        call: ProviderCall<'a>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let request = self
            .inner
            .client
            .post(messages_url(&self.inner.base_url, "chat/completions"))
            .bearer_auth(&self.inner.api_key)
            .json(&self.request_body(&call));
        crate::provider::chat_stream(request, OpenAiParser::default(), parse_event)
    }
}

/// Token counts, the terminal reason and the stop guard shared by the usage
/// chunk and the `[DONE]` marker, so a completion emits exactly one
/// [`StreamEvent::Stop`].
#[derive(Default)]
struct OpenAiParser {
    input_tokens: u64,
    output_tokens: u64,
    finish_reason: Option<String>,
    stopped: bool,
}

impl OpenAiParser {
    fn stop(&mut self) -> Option<StreamEvent> {
        if self.stopped {
            return None;
        }
        self.stopped = true;
        let stop_reason = match self.finish_reason.as_deref() {
            Some("stop") => StopReason::StopResponse,
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };
        Some(StreamEvent::Stop {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            stop_reason,
        })
    }
}

/// Turn one SSE chunk into [`StreamEvent`]s. The `[DONE]` marker and the
/// final usage chunk both stop the completion; empty deltas are skipped.
/// A chunk may carry text, tool call fragments, one per index, or both.
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
    if let Some(first) = choices.first()
        && let Some(reason) = first["finish_reason"].as_str()
    {
        parser.finish_reason = Some(reason.to_string());
    }
    for choice in choices {
        let delta = &choice["delta"];
        let mut events = Vec::new();
        // The field name differs by host: DeepSeek's own API streams
        // `reasoning_content`, the Vercel AI Gateway streams `reasoning`.
        // Requests carry it back as `reasoning_content` either way.
        if let Some(thinking) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            && !thinking.is_empty()
        {
            events.push(StreamEvent::ReasoningDelta(thinking.to_string()));
        }
        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            events.push(StreamEvent::TextDelta(text.to_string()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
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
                events.push(StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                });
            }
        }
        if !events.is_empty() {
            return Ok(events);
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use serde_json::json;

    use super::*;
    use crate::test_support::FakeProvider;
    use crate::test_support::assert_non_200_is_an_error_item;
    use crate::test_support::collect_stream;
    use crate::test_support::provider_call;
    use crate::test_support::sse_response;

    fn sse(data: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    #[tokio::test]
    async fn request_headers_and_body_match_the_provider_shape() {
        let server = FakeProvider::start(|_| sse_response(&[])).await;
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        let events = collect_stream(&provider, provider_call("gpt-test")).await;
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
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        let events = collect_stream(&provider, provider_call("gpt-test")).await;
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
                    stop_reason: StopReason::Other,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_finish_reason_of_stop_reports_a_clean_finish() {
        let server_events = vec![
            sse(json!({
                "choices": [{ "index": 0, "delta": { "content": "Hi" } }],
            })),
            sse(json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            })),
            sse(json!({
                "choices": [],
                "usage": { "prompt_tokens": 4, "completion_tokens": 2 },
            })),
            sse(json!("[DONE]")),
        ];
        let server = FakeProvider::start(move |_| sse_response(&server_events)).await;
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        let events = collect_stream(&provider, provider_call("gpt-test")).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hi".into()),
                StreamEvent::Stop {
                    input_tokens: 4,
                    output_tokens: 2,
                    stop_reason: StopReason::StopResponse,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_finish_reason_of_length_reports_a_truncation() {
        let server_events = vec![
            sse(json!({
                "choices": [{ "index": 0, "delta": { "content": "Bye" } }],
            })),
            sse(json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }],
            })),
            sse(json!({
                "choices": [],
                "usage": { "prompt_tokens": 4, "completion_tokens": 2 },
            })),
            sse(json!("[DONE]")),
        ];
        let server = FakeProvider::start(move |_| sse_response(&server_events)).await;
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        let events = collect_stream(&provider, provider_call("gpt-test")).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Bye".into()),
                StreamEvent::Stop {
                    input_tokens: 4,
                    output_tokens: 2,
                    stop_reason: StopReason::MaxTokens,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_chunk_with_content_and_a_tool_call_emits_both() {
        let server_events = vec![
            sse(json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": "Hel",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "shell", "arguments": "{\"c" },
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
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        let events = collect_stream(&provider, provider_call("gpt-test")).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hel".into()),
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("shell".into()),
                    args_delta: "{\"c".into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                    stop_reason: StopReason::Other,
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
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        assert_non_200_is_an_error_item(&provider, provider_call("gpt-test")).await;
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
                                "function": { "name": "file_read", "arguments": "{\"p" },
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
        let provider = OpenAi::new(
            "gpt-test",
            "sk-test",
            Some(&server.url()),
            crate::provider::DEFAULT_MAX_OUTPUT_TOKENS,
        );

        let events = collect_stream(&provider, provider_call("gpt-test")).await;
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
                    name: Some("file_read".into()),
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
    fn thinking_parses_under_either_field_name() {
        // DeepSeek's own API streams `reasoning_content`; the Vercel AI
        // Gateway streams `reasoning` for the same model.
        let mut parser = OpenAiParser::default();
        assert_eq!(
            parse_event(
                &sse(json!({ "choices": [{ "delta": { "reasoning": "we" } }] })),
                &mut parser,
            )
            .expect("a reasoning delta parses"),
            vec![StreamEvent::ReasoningDelta("we".into())],
        );
        assert_eq!(
            parse_event(
                &sse(json!({ "choices": [{ "delta": { "reasoning_content": "think" } }] })),
                &mut parser,
            )
            .expect("a reasoning delta parses"),
            vec![StreamEvent::ReasoningDelta("think".into())],
        );
    }

    #[test]
    fn a_chunk_carrying_thinking_and_text_yields_both_in_order() {
        let mut parser = OpenAiParser::default();
        assert_eq!(
            parse_event(
                &sse(json!({ "choices": [{ "delta": {
                    "reasoning": "why",
                    "content": "because",
                } }] })),
                &mut parser,
            )
            .expect("both deltas parse"),
            vec![
                StreamEvent::ReasoningDelta("why".into()),
                StreamEvent::TextDelta("because".into()),
            ],
        );
    }

    #[test]
    fn an_empty_thinking_delta_yields_nothing() {
        let mut parser = OpenAiParser::default();
        assert!(
            parse_event(
                &sse(json!({ "choices": [{ "delta": { "reasoning": "" } }] })),
                &mut parser,
            )
            .expect("an empty delta parses")
            .is_empty(),
        );
    }
}
