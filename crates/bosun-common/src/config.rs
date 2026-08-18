use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    pub listen_addr: String,
    pub template_path: PathBuf,
    pub node_timeout_secs: u64,
    pub proxy_bind: String,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8090".into(),
            template_path: "opencode.json".into(),
            node_timeout_secs: 30,
            proxy_bind: "127.0.0.1".into(),
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
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            cp_url: "http://127.0.0.1:8090".into(),
            node_name: "node".into(),
            work_dir: "work".into(),
            advertise_addr: "127.0.0.1".into(),
            heartbeat_interval_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
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
}
