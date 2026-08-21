use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    pub listen_addr: String,
    pub node_timeout_secs: u64,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8090".into(),
            node_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub cp_url: String,
    pub node_name: String,
    pub work_dir: PathBuf,
    pub advertise_addr: String,
    pub heartbeat_interval_secs: u64,
    pub listen_port: u16,
    pub browse_roots: Vec<PathBuf>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            cp_url: "http://127.0.0.1:8090".into(),
            node_name: "node".into(),
            work_dir: "work".into(),
            advertise_addr: "127.0.0.1".into(),
            heartbeat_interval_secs: 5,
            listen_port: 8091,
            browse_roots: Vec::new(),
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
    fn node_config_defaults_listen_port() {
        let config: NodeConfig = toml::from_str("").unwrap();
        assert_eq!(config.listen_port, 8091);
    }

    #[test]
    fn node_config_defaults_to_no_browse_roots() {
        let config: NodeConfig = toml::from_str("").unwrap();
        assert!(config.browse_roots.is_empty());
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
