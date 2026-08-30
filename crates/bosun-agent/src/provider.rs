use bosun_common::session::Message;
use bosun_common::tool::ToolSpec;
use futures_util::stream::BoxStream;
use thiserror::Error;

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
    fn chat_stream<'a>(
        &'a self,
        call: ProviderCall<'a>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}
