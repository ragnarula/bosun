//! Fake provider server for adapter tests: captures the request and answers
//! with a canned response, so tests run against a real HTTP round trip.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use bosun_common::session::Block;
use bosun_common::session::Message;
use bosun_common::session::Role;
use bosun_common::tool::ToolSpec;
use futures_util::StreamExt;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;

use crate::provider::AskRecipient;
use crate::provider::Provider;
use crate::provider::ProviderCall;
use crate::provider::ProviderError;
use crate::provider::StreamEvent;
use crate::sse::SseEvent;

#[derive(Clone)]
pub struct CapturedRequest {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

pub struct FakeProvider {
    pub addr: SocketAddr,
    captured: Arc<Mutex<Option<CapturedRequest>>>,
}

#[derive(Clone)]
struct FakeState {
    captured: Arc<Mutex<Option<CapturedRequest>>>,
    respond: Arc<dyn Fn(CapturedRequest) -> Response + Send + Sync>,
}

impl FakeProvider {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn captured(&self) -> CapturedRequest {
        self.captured
            .lock()
            .unwrap()
            .clone()
            .expect("no request captured")
    }

    /// Boot a fake provider on a loopback port. Every request is captured,
    /// then handed to `respond` for the canned answer.
    pub async fn start(
        respond: impl Fn(CapturedRequest) -> Response + Send + Sync + 'static,
    ) -> FakeProvider {
        let captured = Arc::new(Mutex::new(None));
        let state = FakeState {
            captured: captured.clone(),
            respond: Arc::new(respond),
        };
        let app = Router::new().fallback(handle).with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server failed");
        });
        FakeProvider { addr, captured }
    }
}

/// An SSE response body built from [`SseEvent`]s.
pub fn sse_response(events: &[SseEvent]) -> Response {
    let events = events.to_vec();
    let stream = futures_util::stream::iter(events.into_iter().map(|event| {
        let mut builder = axum::response::sse::Event::default().data(event.data);
        if let Some(name) = event.event {
            builder = builder.event(name);
        }
        Ok::<_, std::convert::Infallible>(builder)
    }));
    axum::response::sse::Sse::new(stream).into_response()
}

async fn handle(State(state): State<FakeState>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .expect("request body fits in 1 MiB");
    let body: Value = serde_json::from_slice(&body).expect("request body is JSON");
    let captured = CapturedRequest {
        path,
        headers,
        body,
    };
    *state.captured.lock().unwrap() = Some(captured.clone());
    (state.respond)(captured)
}

/// A canonical provider call fixture with the given model name.
pub fn provider_call(model: &'static str) -> ProviderCall<'static> {
    ProviderCall {
        model,
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
        ask_recipient: AskRecipient::User,
    }
}

/// Collect a provider's whole stream into its emitted events.
pub async fn collect_stream(provider: &impl Provider, call: ProviderCall<'_>) -> Vec<StreamEvent> {
    let mut stream = provider.chat_stream(call).unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }
    events
}

/// Assert the provider's first stream item is a non-200 error.
pub async fn assert_non_200_is_an_error_item(provider: &impl Provider, call: ProviderCall<'_>) {
    let mut stream = provider.chat_stream(call).unwrap();
    let item = stream.next().await.unwrap();
    assert!(matches!(item, Err(ProviderError::Non200 { .. })));
}
