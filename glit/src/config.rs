use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// GLit configuration loaded from ~/.glit.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default = "default_max_iters")]
    pub max_iters: u8,
    #[serde(default = "default_log_enabled")]
    pub log_enabled: bool,
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    /// Hook execution routing: "auto" | "git2" | "system".
    ///
    /// - "auto" (default): use system `git` for commit when executable hooks
    ///   are present (so husky/pre-commit/etc. run), else fast git2.
    /// - "system": always route commit through system `git`.
    /// - "git2": always use git2 (hooks are NOT run — opt-in bypass).
    ///
    /// Push already shells out unconditionally, so this knob governs commit.
    #[serde(default = "default_hooks")]
    pub hooks: String,
}

fn default_model() -> String {
    "qwen2.5:3b".to_string()
}
fn default_remote() -> String {
    "origin".to_string()
}
fn default_max_iters() -> u8 {
    5
}
fn default_log_enabled() -> bool {
    true
}
fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_hooks() -> String {
    "auto".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: default_model(),
            remote: default_remote(),
            max_iters: default_max_iters(),
            log_enabled: default_log_enabled(),
            ollama_url: default_ollama_url(),
            hooks: default_hooks(),
        }
    }
}

/// Get the path to the GLit config directory (~/.glit/).
pub fn glit_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".glit"))
}

/// Get the path to the config file (~/.glit.toml).
fn config_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".glit.toml"))
}

/// Load configuration from ~/.glit.toml, falling back to defaults.
pub fn load() -> anyhow::Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config at {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config at {}", path.display()))?;
    Ok(config)
}

/// Save configuration to ~/.glit.toml.
pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = config_path()?;
    let content = toml::to_string_pretty(config).context("Failed to serialize config")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.model, "qwen2.5:3b");
        assert_eq!(config.remote, "origin");
        assert_eq!(config.max_iters, 5);
        assert!(config.log_enabled);
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.hooks, "auto");
    }

    #[test]
    fn test_config_deserialize_partial() {
        let toml_str = r#"model = "custom-model""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.remote, "origin"); // default
        assert_eq!(config.max_iters, 5); // default
    }

    #[test]
    fn test_config_serialize_roundtrip() {
        let config = Config::default();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config.model, deserialized.model);
        assert_eq!(config.remote, deserialized.remote);
    }
}
