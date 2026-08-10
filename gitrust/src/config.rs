use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// GiTrust configuration loaded from ~/.gitrust.toml.
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
    /// How many days GiTrust keeps trash-net backups (~/.gitrust/trash/<ts>/) before
    /// pruning them on startup. The trash-net copies content that would be
    /// deleted by a hard reset / discard-local / clean (Phase 14, S30/S62).
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u64,
    /// Enable the embedding router (Layer 1.25): between the exact fast-path and
    /// the LLM, route paraphrases by nearest-exemplar over the loaded model's
    /// `/api/embed`. Robust to unseen phrasings and ~30× faster than the LLM,
    /// but only ACTS on a confident, non-destructive match — otherwise it
    /// abstains and the LLM path runs unchanged. Set false to disable.
    #[serde(default = "default_router_enabled")]
    pub router_enabled: bool,
    /// Minimum top1−top2 cosine margin for the embedding router to trust a match.
    /// Below this the router abstains → LLM. Calibrated in the eval harness
    /// (docs/classifier_eval); correct routes cluster well above incorrect ones,
    /// but the buckets overlap, so this trades coverage for precision.
    #[serde(default = "default_router_min_margin")]
    pub router_min_margin: f32,
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
fn default_trash_retention_days() -> u64 {
    7
}
fn default_router_enabled() -> bool {
    true
}
fn default_router_min_margin() -> f32 {
    // Calibrated against the REAL binary's embeddings (qwen2.5:3b, non-centered)
    // over the router.rs exemplar set — see docs/classifier_eval and the
    // margin-probe notes. The v0.3.0 target wins land at margin ≥ ~0.016
    // (scoped_push 0.027–0.040, squash 0.016–0.026, pull/branches far higher),
    // while the noise abstains (a status↔fetch confusion at 0.006, "what's the
    // weather" at 0.012, a reword↔squash tie at 0.001). 0.015 sits in that gap.
    // Deliberately conservative: a wrong direct route is worse than a
    // slow-but-correct LLM route, so when in doubt we abstain.
    0.015
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
            trash_retention_days: default_trash_retention_days(),
            router_enabled: default_router_enabled(),
            router_min_margin: default_router_min_margin(),
        }
    }
}

/// Get the path to the GiTrust config directory (~/.gitrust/).
pub fn gitrust_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".gitrust"))
}

/// Get the path to the config file (~/.gitrust.toml).
fn config_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".gitrust.toml"))
}

/// Load configuration from ~/.gitrust.toml, falling back to defaults.
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

/// Save configuration to ~/.gitrust.toml.
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
        assert_eq!(config.trash_retention_days, 7);
        assert!(config.router_enabled);
        assert_eq!(config.router_min_margin, 0.015);
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
