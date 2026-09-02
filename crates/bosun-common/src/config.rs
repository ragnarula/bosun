use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::session::Permission;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    pub listen_addr: String,
    pub node_timeout_secs: u64,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    pub models: HashMap<String, ModelConfig>,
    pub subagents: HashMap<String, SubagentConfig>,
    pub default_model: Option<String>,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8090".into(),
            node_timeout_secs: 30,
            tls_cert: None,
            tls_key: None,
            data_dir: default_data_dir(),
            models: HashMap::new(),
            subagents: HashMap::new(),
            default_model: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub provider: String, // "anthropic" or "openai"
    pub name: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub api_key: String, // a literal key, or "env:VAR" to read from the environment
    #[serde(default)]
    pub price_input_per_mtok: f64,
    #[serde(default)]
    pub price_output_per_mtok: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentConfig {
    pub model: String,
    pub permission: Permission,
}

/// The node's auto-update settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NodeUpdateConfig {
    /// Whether the node applies the control plane's version on its own.
    pub enabled: bool,
    /// Base URL of the release feed the node fetches update archives from,
    /// overriding the `BOSUN_UPDATE_BASE_URL` mirror and the GitHub Releases
    /// default.
    pub base_url: Option<String>,
}

impl Default for NodeUpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub cp_url: String,
    pub node_name: String,
    pub work_dir: PathBuf,
    pub browse_roots: Vec<PathBuf>,
    pub ca_cert: Option<PathBuf>,
    pub update: NodeUpdateConfig,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            cp_url: "http://127.0.0.1:8090".into(),
            node_name: "node".into(),
            work_dir: "work".into(),
            browse_roots: Vec::new(),
            ca_cert: None,
            update: NodeUpdateConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CliConfig {
    pub cp_url: String,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            cp_url: "http://127.0.0.1:8090".into(),
        }
    }
}

pub fn load_config<T>(path: &Path) -> Result<T, anyhow::Error>
where
    T: Default + DeserializeOwned,
{
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    let config = toml::from_str(&text)
        .with_context(|| format!("failed to parse config from {}", path.display()))?;
    Ok(config)
}

/// Load a config file, returning `T::default()` when the file does not exist.
pub fn load_config_if_exists<T>(path: &Path) -> Result<T, anyhow::Error>
where
    T: Default + DeserializeOwned,
{
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(T::default());
    };
    toml::from_str(&text).with_context(|| format!("failed to parse config from {}", path.display()))
}

/// Write a config file, creating the parent directory.
pub fn save_config<T>(path: &Path, config: &T) -> Result<(), anyhow::Error>
where
    T: Serialize,
{
    let text = toml::to_string(config).context("failed to serialize config")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    std::fs::write(path, text)
        .with_context(|| format!("failed to write config to {}", path.display()))
}

/// Path to the CLI config file: `$XDG_CONFIG_HOME` or `$HOME/.config`, then
/// `bosun/config.toml`.
pub fn cli_config_path() -> PathBuf {
    cli_config_path_with(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn cli_config_path_with(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    let base = xdg_config_home
        .or_else(|| home.map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("bosun").join("config.toml")
}

pub fn load_cli_config() -> Result<CliConfig, anyhow::Error> {
    load_config_if_exists(&cli_config_path())
}

pub fn save_cli_config(config: &CliConfig) -> Result<(), anyhow::Error> {
    save_config(&cli_config_path(), config)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let config: ControlConfig = toml::from_str("").unwrap();
        assert_eq!(config.listen_addr, "127.0.0.1:8090");
        assert_eq!(config.node_timeout_secs, 30);
    }

    #[test]
    fn empty_config_defaults_data_dir_and_empty_maps() {
        let config: ControlConfig = toml::from_str("").unwrap();
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert!(config.models.is_empty());
        assert!(config.subagents.is_empty());
        assert_eq!(config.default_model, None);
    }

    #[test]
    fn sparse_config_keeps_the_data_dir_default() {
        let config: ControlConfig = toml::from_str("listen_addr = \"0.0.0.0:9000\"").unwrap();
        assert_eq!(config.data_dir, PathBuf::from("data"));
    }

    #[test]
    fn config_parses_models_and_subagents() {
        let config: ControlConfig = toml::from_str(
            r#"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "env:ANTHROPIC_API_KEY"
            base_url = "https://api.anthropic.com"

            [models.cheap]
            provider = "openai"
            name = "gpt-4o-mini"
            api_key = "sk-test"

            [subagents.coder]
            model = "main"
            permission = "read_write"
            "#,
        )
        .unwrap();
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert_eq!(config.models.len(), 2);
        assert_eq!(config.subagents.len(), 1);
        assert_eq!(config.default_model, None);

        let main = &config.models["main"];
        assert_eq!(main.provider, "anthropic");
        assert_eq!(main.name, "claude-sonnet-4-5");
        assert_eq!(main.api_key, "env:ANTHROPIC_API_KEY");
        assert_eq!(main.base_url.as_deref(), Some("https://api.anthropic.com"));

        let cheap = &config.models["cheap"];
        assert_eq!(cheap.provider, "openai");
        assert_eq!(cheap.name, "gpt-4o-mini");
        assert_eq!(cheap.base_url, None);

        let coder = &config.subagents["coder"];
        assert_eq!(coder.model, "main");
        assert_eq!(coder.permission, Permission::ReadWrite);
    }

    #[test]
    fn model_without_prices_defaults_to_zero() {
        let config: ControlConfig = toml::from_str(
            r#"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "env:ANTHROPIC_API_KEY"
            "#,
        )
        .unwrap();
        let main = &config.models["main"];
        assert_eq!(main.price_input_per_mtok, 0.0);
        assert_eq!(main.price_output_per_mtok, 0.0);
    }

    #[test]
    fn partial_config_overrides_defaults() {
        let config: ControlConfig = toml::from_str("listen_addr = \"0.0.0.0:9000\"").unwrap();
        assert_eq!(config.listen_addr, "0.0.0.0:9000");
        assert_eq!(config.node_timeout_secs, 30);
    }

    #[test]
    fn load_missing_file_is_err() {
        assert!(load_config::<ControlConfig>(Path::new("/nonexistent/bosun.toml")).is_err());
    }

    #[test]
    fn node_config_defaults_to_no_browse_roots() {
        let config: NodeConfig = toml::from_str("").unwrap();
        assert!(config.browse_roots.is_empty());
    }

    #[test]
    fn node_update_defaults_to_enabled() {
        let config: NodeConfig = toml::from_str("").unwrap();
        assert!(config.update.enabled);
    }

    #[test]
    fn node_update_can_be_disabled() {
        let config: NodeConfig = toml::from_str("[update]\nenabled = false").unwrap();
        assert!(!config.update.enabled);
    }

    #[test]
    fn node_update_defaults_base_url_to_none() {
        let config: NodeConfig = toml::from_str("").unwrap();
        assert_eq!(config.update.base_url, None);
    }

    #[test]
    fn node_update_parses_a_base_url_override() {
        let config: NodeConfig = toml::from_str(
            r#"
            [update]
            enabled = true
            base_url = "https://mirror.example/bosun"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.update.base_url.as_deref(),
            Some("https://mirror.example/bosun")
        );
    }

    #[test]
    fn cli_config_path_uses_xdg_config_home_when_set() {
        assert_eq!(
            cli_config_path_with(Some(PathBuf::from("/cfg")), Some(PathBuf::from("/home"))),
            PathBuf::from("/cfg/bosun/config.toml")
        );
    }

    #[test]
    fn cli_config_path_falls_back_to_home_config_dir() {
        assert_eq!(
            cli_config_path_with(None, Some(PathBuf::from("/home"))),
            PathBuf::from("/home/.config/bosun/config.toml")
        );
    }

    #[test]
    fn load_config_if_exists_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let config: CliConfig = load_config_if_exists(&dir.path().join("missing.toml")).unwrap();
        assert_eq!(config.cp_url, "http://127.0.0.1:8090");
    }

    #[test]
    fn load_config_if_exists_malformed_file_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not = = valid").unwrap();
        assert!(load_config_if_exists::<CliConfig>(&path).is_err());
    }

    #[test]
    fn save_and_load_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        let config = CliConfig {
            cp_url: "http://10.0.0.5:8090".into(),
        };
        save_config(&path, &config).unwrap();
        let loaded: CliConfig = load_config_if_exists(&path).unwrap();
        assert_eq!(loaded.cp_url, "http://10.0.0.5:8090");
    }
}
