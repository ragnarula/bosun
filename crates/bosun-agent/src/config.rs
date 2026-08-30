use std::collections::HashMap;

use bosun_common::config::ModelConfig;

use crate::provider::ProviderError;

#[derive(Debug)]
pub struct ResolvedModel {
    pub config: ModelConfig,
    pub api_key: String,
}

/// Pick the model to call: the requested name, else the `default` entry,
/// else the first entry in sorted order. `api_key` values of the form
/// `env:VAR` are read from the environment; anything else is used literally.
pub fn resolve_model(
    models: &HashMap<String, ModelConfig>,
    requested: Option<&str>,
) -> Result<ResolvedModel, ProviderError> {
    let config = if let Some(requested) = requested {
        models.get(requested).ok_or_else(|| ProviderError::Parse {
            detail: format!("model {requested} is not configured"),
        })?
    } else if let Some(config) = models.get("default") {
        config
    } else {
        let mut names: Vec<&String> = models.keys().collect();
        names.sort();
        let name = names
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse {
                detail: "no models configured".into(),
            })?;
        &models[name.as_str()]
    };
    let api_key = resolve_api_key(&config.api_key)?;
    Ok(ResolvedModel {
        config: config.clone(),
        api_key,
    })
}

fn resolve_api_key(api_key: &str) -> Result<String, ProviderError> {
    match api_key.strip_prefix("env:") {
        Some(var) => match std::env::var(var) {
            Ok(value) if !value.is_empty() => Ok(value),
            _ => Err(ProviderError::MissingEnvVar {
                var: var.to_string(),
            }),
        },
        None => Ok(api_key.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(api_key: &str) -> ModelConfig {
        ModelConfig {
            provider: "anthropic".into(),
            name: "claude-sonnet-4-5".into(),
            base_url: None,
            api_key: api_key.into(),
            price_input_per_mtok: 0.0,
            price_output_per_mtok: 0.0,
        }
    }

    #[test]
    fn requested_model_wins() {
        let models = HashMap::from([
            ("main".to_string(), model("sk-main")),
            ("cheap".to_string(), model("sk-cheap")),
        ]);
        let resolved = resolve_model(&models, Some("cheap")).unwrap();
        assert_eq!(resolved.config.name, "claude-sonnet-4-5");
        assert_eq!(resolved.api_key, "sk-cheap");
    }

    #[test]
    fn unknown_requested_model_is_an_error() {
        let models = HashMap::from([("main".to_string(), model("sk-test"))]);
        let err = resolve_model(&models, Some("missing")).unwrap_err();
        assert!(matches!(err, ProviderError::Parse { .. }));
    }

    #[test]
    fn default_entry_is_used_when_nothing_is_requested() {
        let models = HashMap::from([
            ("a".to_string(), model("sk-a")),
            ("default".to_string(), model("sk-default")),
        ]);
        let resolved = resolve_model(&models, None).unwrap();
        assert_eq!(resolved.api_key, "sk-default");
    }

    #[test]
    fn first_entry_in_sorted_order_is_used() {
        let models = HashMap::from([
            ("zeta".to_string(), model("sk-zeta")),
            ("alpha".to_string(), model("sk-alpha")),
        ]);
        let resolved = resolve_model(&models, None).unwrap();
        assert_eq!(resolved.api_key, "sk-alpha");
    }

    #[test]
    fn no_models_is_an_error() {
        let models = HashMap::new();
        let err = resolve_model(&models, None).unwrap_err();
        assert!(matches!(err, ProviderError::Parse { .. }));
    }

    #[test]
    fn env_key_is_read_from_the_environment() {
        let var = "BOSUN_TEST_PROVIDER_KEY";
        unsafe {
            std::env::set_var(var, "sk-env-test");
        }
        let models = HashMap::from([("main".to_string(), model(&format!("env:{var}")))]);
        let resolved = resolve_model(&models, Some("main")).unwrap();
        assert_eq!(resolved.api_key, "sk-env-test");
    }

    #[test]
    fn missing_env_key_is_an_error() {
        let var = "BOSUN_TEST_MISSING_PROVIDER_KEY";
        unsafe {
            std::env::remove_var(var);
        }
        let models = HashMap::from([("main".to_string(), model(&format!("env:{var}")))]);
        let err = resolve_model(&models, Some("main")).unwrap_err();
        assert!(
            matches!(err, ProviderError::MissingEnvVar { var } if var == "BOSUN_TEST_MISSING_PROVIDER_KEY")
        );
    }

    #[test]
    fn empty_env_value_is_an_error() {
        let var = "BOSUN_TEST_EMPTY_PROVIDER_KEY";
        unsafe {
            std::env::set_var(var, "");
        }
        let models = HashMap::from([("main".to_string(), model(&format!("env:{var}")))]);
        let err = resolve_model(&models, Some("main")).unwrap_err();
        assert!(matches!(err, ProviderError::MissingEnvVar { .. }));
    }

    #[test]
    fn literal_key_is_used_as_is() {
        let models = HashMap::from([("main".to_string(), model("sk-literal"))]);
        let resolved = resolve_model(&models, Some("main")).unwrap();
        assert_eq!(resolved.api_key, "sk-literal");
    }
}
