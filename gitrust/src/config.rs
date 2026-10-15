use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// GiTrust configuration loaded from ~/.gitrust.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default = "default_max_iters")]
    pub max_iters: u8,
    #[serde(default = "default_log_enabled")]
    pub log_enabled: bool,
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
    /// Base URL of the local Kev decision-model server (`python -m kev.serve`).
    /// Binds 127.0.0.1 by default — nothing leaves the machine.
    #[serde(default = "default_kev_url")]
    pub kev_url: String,
    /// Kev checkpoint to serve when GiTrust starts the server itself: a Hub id
    /// (`jaredpalmer/kev-0.8b@v1.0`) or a local run directory (a fine-tune).
    #[serde(default = "default_kev_run")]
    pub kev_run: String,
    /// Path to a Kev checkout (github.com/jaredpalmer/kev, `uv sync --extra serve`
    /// done). Needed only for `kev_autostart`.
    #[serde(default)]
    pub kev_dir: Option<String>,
    /// Start the Kev server in the background on the first query that needs it
    /// (~25 s cold start on the 8 GB M2; it then stays resident, ~2.9 GB).
    #[serde(default = "default_kev_autostart")]
    pub kev_autostart: bool,
    /// Minimum Kev probability to route a NON-destructive op without asking.
    /// Below it, the user picks from Kev's top candidates. Calibrated on the
    /// 44-case eval: at 0.5 stock Kev-0.8B routed 9/44, 8 correct, none
    /// order-unstable (docs/classifier_eval/run_kev_eval.py).
    #[serde(default = "default_kev_min_confidence")]
    pub kev_min_confidence: f32,
    /// Higher bar for ops that can rewrite or discard work (undo, cleanup,
    /// discard_local, force_push, squash, amend, revert). Even when cleared,
    /// those ops still hit their ChoiceGate / type-to-confirm downstream.
    #[serde(default = "default_kev_destructive_min_confidence")]
    pub kev_destructive_min_confidence: f32,
    /// Minimum Kev probability to refuse a request outright as "not a Git
    /// request". Below it (but still Kev's top answer) the user gets a menu of
    /// Kev's best Git ops plus "none of these" instead. Fine-tuned eval: true
    /// out-of-scope requests scored 0.55–0.99; the one wrong refusal ("where am
    /// i right now") scored 0.66 — a wrong refusal is cheaper than a wrong
    /// action, but a menu is cheaper still.
    #[serde(default = "default_kev_reject_min_confidence")]
    pub kev_reject_min_confidence: f32,
    /// Stop a Kev server that GiTrust started after this many idle minutes, to
    /// give its ~3 GB back (the next query pays the ~25 s cold start again).
    /// 0 = keep it resident. A server you started yourself is never stopped.
    #[serde(default = "default_kev_idle_minutes")]
    pub kev_idle_minutes: u64,
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
fn default_hooks() -> String {
    "auto".to_string()
}
fn default_trash_retention_days() -> u64 {
    7
}
fn default_kev_url() -> String {
    "http://127.0.0.1:8009".to_string()
}
fn default_kev_run() -> String {
    "AniketDalvi385/gitrust-kev-0.8b@v2".to_string()
}
fn default_kev_autostart() -> bool {
    true
}
fn default_kev_min_confidence() -> f32 {
    0.5
}
fn default_kev_destructive_min_confidence() -> f32 {
    0.6
}
fn default_kev_reject_min_confidence() -> f32 {
    0.9
}
fn default_kev_idle_minutes() -> u64 {
    15
}

impl Default for Config {
    fn default() -> Self {
        Self {
            remote: default_remote(),
            max_iters: default_max_iters(),
            log_enabled: default_log_enabled(),
            hooks: default_hooks(),
            trash_retention_days: default_trash_retention_days(),
            kev_url: default_kev_url(),
            kev_run: default_kev_run(),
            kev_dir: None,
            kev_autostart: default_kev_autostart(),
            kev_min_confidence: default_kev_min_confidence(),
            kev_destructive_min_confidence: default_kev_destructive_min_confidence(),
            kev_reject_min_confidence: default_kev_reject_min_confidence(),
            kev_idle_minutes: default_kev_idle_minutes(),
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
        assert_eq!(config.remote, "origin");
        assert_eq!(config.max_iters, 5);
        assert!(config.log_enabled);
        assert_eq!(config.hooks, "auto");
        assert_eq!(config.trash_retention_days, 7);
        assert_eq!(config.kev_url, "http://127.0.0.1:8009");
        assert_eq!(config.kev_run, "AniketDalvi385/gitrust-kev-0.8b@v2");
        assert!(config.kev_dir.is_none());
        assert!(config.kev_autostart);
        assert_eq!(config.kev_min_confidence, 0.5);
        assert_eq!(config.kev_destructive_min_confidence, 0.6);
        assert_eq!(config.kev_reject_min_confidence, 0.9);
        assert_eq!(config.kev_idle_minutes, 15);
    }

    #[test]
    fn test_config_deserialize_partial() {
        let toml_str = r#"kev_url = "http://127.0.0.1:9000""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.kev_url, "http://127.0.0.1:9000");
        assert_eq!(config.remote, "origin"); // default
        assert_eq!(config.max_iters, 5); // default
    }

    #[test]
    fn test_legacy_ollama_config_still_loads() {
        // A pre-Kev ~/.gitrust.toml must not break the binary: the retired
        // keys are ignored and everything else keeps its value.
        let toml_str = r#"
model = "qwen2.5:3b"
ollama_url = "http://localhost:11434"
router_enabled = true
router_min_margin = 0.015
remote = "upstream"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.remote, "upstream");
        assert_eq!(config.kev_url, "http://127.0.0.1:8009");
    }

    #[test]
    fn test_config_serialize_roundtrip() {
        let config = Config::default();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config.kev_url, deserialized.kev_url);
        assert_eq!(config.remote, deserialized.remote);
    }
}
