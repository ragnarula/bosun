//! Adapter factory: pick the provider adapter for a resolved model.

use crate::anthropic::Anthropic;
use crate::config::ResolvedModel;
use crate::openai::OpenAi;
use crate::provider::Provider;
use crate::provider::ProviderError;

/// Build the adapter for the resolved model's provider.
pub fn provider_for(model: &ResolvedModel) -> Result<Box<dyn Provider>, ProviderError> {
    match model.config.provider.as_str() {
        "anthropic" => Ok(Box::new(Anthropic::new(
            &model.config.name,
            &model.api_key,
            model.config.base_url.as_deref(),
        ))),
        "openai" => Ok(Box::new(OpenAi::new(
            &model.config.name,
            &model.api_key,
            model.config.base_url.as_deref(),
        ))),
        other => Err(ProviderError::UnsupportedProvider {
            name: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use bosun_common::config::ModelConfig;

    use super::*;

    fn resolved(provider: &str, name: &str) -> ResolvedModel {
        ResolvedModel {
            config: ModelConfig {
                provider: provider.into(),
                name: name.into(),
                base_url: None,
                api_key: "sk-test".into(),
                price_input_per_mtok: 0.0,
                price_output_per_mtok: 0.0,
            },
            api_key: "sk-test".into(),
        }
    }

    #[test]
    fn anthropic_model_builds_the_anthropic_adapter() {
        let provider = provider_for(&resolved("anthropic", "claude-test")).unwrap();
        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.model(), "claude-test");
    }

    #[test]
    fn openai_model_builds_the_openai_adapter() {
        let provider = provider_for(&resolved("openai", "gpt-test")).unwrap();
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model(), "gpt-test");
    }

    #[test]
    fn unknown_provider_is_an_error() {
        let err = match provider_for(&resolved("gemini", "gemini-test")) {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        };
        assert!(matches!(err, ProviderError::UnsupportedProvider { .. }));
    }
}
