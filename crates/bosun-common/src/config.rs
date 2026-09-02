use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::session::Permission;
use crate::tool::ALL_TOOLS;

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
    pub personas: HashMap<String, PersonaConfig>,
    pub default_persona: Option<String>,
    pub update: UpdateConfig,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

impl ControlConfig {
    /// The directory holding one update binary per platform, named
    /// `bosun.<target-triple>`. Defaults to `<data dir>/artifacts`.
    pub fn artifacts_dir(&self) -> PathBuf {
        self.update
            .artifacts_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("artifacts"))
    }

    /// Boot validation of the persona catalog. A persona's `model` must name
    /// a configured model, its `allowed_tools` must be `"*"` or canonical
    /// tool names, and a set `default_persona` must name a persona. The error
    /// carries one message per problem found, so a boot failure reports them
    /// all at once.
    pub fn validate_personas(&self) -> Result<(), PersonaConfigError> {
        let mut problems = Vec::new();
        let mut names: Vec<&String> = self.personas.keys().collect();
        names.sort();
        for name in names {
            let persona = &self.personas[name];
            if !self.models.contains_key(&persona.model) {
                problems.push(format!(
                    "persona {name} references model {} which is not configured",
                    persona.model
                ));
            }
            if let Err(error) = crate::tool::parse_allowed_tools(&persona.allowed_tools) {
                problems.push(format!(
                    "persona {name} allows unknown tool(s): {}",
                    error.unknown.join(", ")
                ));
            }
        }
        if let Some(default) = &self.default_persona
            && !self.personas.contains_key(default)
        {
            problems.push(format!(
                "default_persona {default} is not a configured persona"
            ));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(PersonaConfigError { problems })
        }
    }

    /// Reads each persona's prompt body from `<data dir>/personas/<name>.md`
    /// when the file exists. A persona without a prompt file keeps no system
    /// prompt, so its sessions fall back to the loop's default system text.
    pub fn load_persona_prompts(&mut self) -> Result<(), anyhow::Error> {
        let dir = self.data_dir.join("personas");
        let mut names: Vec<String> = self.personas.keys().cloned().collect();
        names.sort();
        for name in names {
            let path = dir.join(format!("{name}.md"));
            let persona = self
                .personas
                .get_mut(&name)
                .expect("cloned from the keys above");
            persona.system_prompt = match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read persona prompt from {}", path.display())
                    });
                }
            };
        }
        Ok(())
    }
}

/// The persona catalog failed validation. `problems` holds one sentence per
/// problem, so the control plane can report them all at boot.
#[derive(Debug, Error)]
#[error("invalid persona configuration:\n{}", self.problems.join("\n"))]
pub struct PersonaConfigError {
    pub problems: Vec<String>,
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
            personas: HashMap::new(),
            default_persona: None,
            update: UpdateConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Where per-platform update binaries live, when not the default of
    /// `<data dir>/artifacts`.
    pub artifacts_dir: Option<PathBuf>,
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
#[serde(deny_unknown_fields)]
pub struct PersonaConfig {
    /// The model entry this persona runs sessions on.
    pub model: String,
    /// The session's permission mode, enforced by its executor.
    pub permission: Permission,
    /// Which canonical tools the session may use: `"*"` for every tool, or a
    /// comma/space-separated list of canonical tool names.
    #[serde(default = "default_allowed_tools")]
    pub allowed_tools: String,
    /// What the persona is for, shown to the user when the persona is picked.
    #[serde(default)]
    pub description: String,
    /// The persona's role/behaviour prompt, read from
    /// `<data dir>/personas/<name>.md` at boot when the file exists; not a
    /// TOML key. `None` leaves the session on the loop's default system text.
    #[serde(skip)]
    pub system_prompt: Option<String>,
}

fn default_allowed_tools() -> String {
    ALL_TOOLS.into()
}

/// The node's auto-update settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NodeUpdateConfig {
    /// Whether the node applies the control plane's version on its own.
    pub enabled: bool,
}

impl Default for NodeUpdateConfig {
    fn default() -> Self {
        Self { enabled: true }
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
        assert!(config.personas.is_empty());
        assert_eq!(config.default_persona, None);
    }

    #[test]
    fn update_artifacts_dir_defaults_to_data_dir_artifacts() {
        let config: ControlConfig = toml::from_str("").unwrap();
        assert_eq!(config.artifacts_dir(), PathBuf::from("data/artifacts"));
    }

    #[test]
    fn update_artifacts_dir_follows_data_dir_by_default() {
        let config: ControlConfig = toml::from_str("data_dir = \"/var/bosun\"").unwrap();
        assert_eq!(
            config.artifacts_dir(),
            PathBuf::from("/var/bosun/artifacts")
        );
    }

    #[test]
    fn empty_update_table_defaults_artifacts_dir() {
        let config: ControlConfig = toml::from_str("[update]").unwrap();
        assert_eq!(config.artifacts_dir(), PathBuf::from("data/artifacts"));
    }

    #[test]
    fn update_artifacts_dir_overrides_the_default() {
        let config: ControlConfig = toml::from_str(
            r#"
            [update]
            artifacts_dir = "/opt/bosun/artifacts"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.artifacts_dir(),
            PathBuf::from("/opt/bosun/artifacts")
        );
    }

    #[test]
    fn sparse_config_keeps_the_data_dir_default() {
        let config: ControlConfig = toml::from_str("listen_addr = \"0.0.0.0:9000\"").unwrap();
        assert_eq!(config.data_dir, PathBuf::from("data"));
    }

    #[test]
    fn config_parses_models_and_personas() {
        let config: ControlConfig = toml::from_str(
            r#"
            default_persona = "coder"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "env:ANTHROPIC_API_KEY"
            base_url = "https://api.anthropic.com"

            [models.cheap]
            provider = "openai"
            name = "gpt-4o-mini"
            api_key = "sk-test"

            [personas.coder]
            model = "main"
            permission = "read_write"
            allowed_tools = "*"
            description = "Makes changes directly"

            [personas.reviewer]
            model = "cheap"
            permission = "read_only"

            "#,
        )
        .unwrap();
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert_eq!(config.models.len(), 2);
        assert_eq!(config.personas.len(), 2);
        assert_eq!(config.default_persona.as_deref(), Some("coder"));

        let main = &config.models["main"];
        assert_eq!(main.provider, "anthropic");
        assert_eq!(main.name, "claude-sonnet-4-5");
        assert_eq!(main.api_key, "env:ANTHROPIC_API_KEY");
        assert_eq!(main.base_url.as_deref(), Some("https://api.anthropic.com"));

        let cheap = &config.models["cheap"];
        assert_eq!(cheap.provider, "openai");
        assert_eq!(cheap.name, "gpt-4o-mini");
        assert_eq!(cheap.base_url, None);

        let coder = &config.personas["coder"];
        assert_eq!(coder.model, "main");
        assert_eq!(coder.permission, Permission::ReadWrite);
        assert_eq!(coder.allowed_tools, "*");
        assert_eq!(coder.description, "Makes changes directly");

        let reviewer = &config.personas["reviewer"];
        assert_eq!(reviewer.model, "cheap");
        assert_eq!(reviewer.permission, Permission::ReadOnly);
        assert_eq!(reviewer.allowed_tools, "*");
        assert_eq!(reviewer.description, "");
        assert_eq!(
            reviewer.system_prompt, None,
            "prompts come from files, not TOML"
        );
    }

    #[test]
    fn persona_allowed_tools_defaults_to_star() {
        let config: ControlConfig = toml::from_str(
            r#"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "x"

            [personas.coder]
            model = "main"
            permission = "read_write"
            "#,
        )
        .unwrap();
        assert_eq!(config.personas["coder"].allowed_tools, "*");
    }

    #[test]
    fn validate_personas_accepts_a_valid_catalog() {
        let config: ControlConfig = toml::from_str(
            r#"
            default_persona = "coder"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "x"

            [personas.coder]
            model = "main"
            permission = "read_write"
            allowed_tools = "shell, file/read, git"

            [personas.looker]
            model = "main"
            permission = "read_only"

            "#,
        )
        .unwrap();
        assert!(config.validate_personas().is_ok());
    }

    #[test]
    fn validate_personas_rejects_an_unknown_model() {
        let config: ControlConfig = toml::from_str(
            r#"
            [personas.coder]
            model = "ghost"
            permission = "read_write"
            "#,
        )
        .unwrap();
        let err = config.validate_personas().unwrap_err();
        assert_eq!(
            err.problems,
            ["persona coder references model ghost which is not configured"]
        );
    }

    #[test]
    fn validate_personas_rejects_unknown_allowed_tools() {
        let config: ControlConfig = toml::from_str(
            r#"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "x"

            [personas.coder]
            model = "main"
            permission = "read_write"
            allowed_tools = "shell, websurf"
            "#,
        )
        .unwrap();
        let err = config.validate_personas().unwrap_err();
        assert_eq!(
            err.problems,
            ["persona coder allows unknown tool(s): websurf"]
        );
    }

    #[test]
    fn validate_personas_rejects_an_unknown_default_persona() {
        let config: ControlConfig = toml::from_str(
            r#"
            default_persona = "ghost"
            [models.main]
            provider = "anthropic"
            name = "claude-sonnet-4-5"
            api_key = "x"

            [personas.coder]
            model = "main"
            permission = "read_write"

            "#,
        )
        .unwrap();
        let err = config.validate_personas().unwrap_err();
        assert_eq!(
            err.problems,
            ["default_persona ghost is not a configured persona"]
        );
    }

    #[test]
    fn validate_personas_reports_every_problem() {
        let config: ControlConfig = toml::from_str(
            r#"
            default_persona = "missing"
            [personas.coder]
            model = "ghost"
            permission = "read_write"
            allowed_tools = "websurf"

            "#,
        )
        .unwrap();
        let err = config.validate_personas().unwrap_err();
        assert_eq!(err.problems.len(), 3);
    }

    #[test]
    fn validate_personas_accepts_no_personas() {
        let config: ControlConfig = toml::from_str("").unwrap();
        assert!(config.validate_personas().is_ok());
    }

    #[test]
    fn persona_prompts_are_loaded_from_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let personas_dir = dir.path().join("personas");
        std::fs::create_dir_all(&personas_dir).unwrap();
        std::fs::write(personas_dir.join("coder.md"), "You are the coder.").unwrap();
        std::fs::write(personas_dir.join("reviewer.md"), "You review.").unwrap();

        let mut config: ControlConfig = toml::from_str(&format!(
            r#"
            data_dir = "{}"

            [personas.coder]
            model = "main"
            permission = "read_write"

            [personas.reviewer]
            model = "main"
            permission = "read_only"

            [personas.bare]
            model = "main"
            permission = "read_write"
            "#,
            dir.path().display()
        ))
        .unwrap();
        assert_eq!(config.personas["coder"].system_prompt, None);

        config.load_persona_prompts().unwrap();

        assert_eq!(
            config.personas["coder"].system_prompt.as_deref(),
            Some("You are the coder.")
        );
        assert_eq!(
            config.personas["reviewer"].system_prompt.as_deref(),
            Some("You review.")
        );
        assert_eq!(
            config.personas["bare"].system_prompt, None,
            "a persona without a prompt file keeps no system prompt"
        );
    }

    #[test]
    fn persona_prompts_without_a_personas_dir_stay_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut config: ControlConfig = toml::from_str(&format!(
            r#"
            data_dir = "{}"

            [personas.coder]
            model = "main"
            permission = "read_write"
            "#,
            dir.path().display()
        ))
        .unwrap();
        config.load_persona_prompts().unwrap();
        assert_eq!(config.personas["coder"].system_prompt, None);
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
