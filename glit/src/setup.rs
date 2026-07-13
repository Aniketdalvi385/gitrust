use anyhow::{anyhow, Context};
use std::time::{Duration, Instant};

use crate::config::Config;

/// Entry point for `glit setup`.
/// Checks Ollama install → service running → model pulled → smoke test.
pub async fn run_setup() -> anyhow::Result<()> {
    let config = Config::default();

    println!("GLit setup\n");

    // Step 1: Ollama binary on PATH
    if which::which("ollama").is_err() {
        println!("  Ollama not found on PATH.");
        if confirm("Install Ollama now?")? {
            install_ollama()?;
        } else {
            return Err(anyhow!(
                "Ollama is required. Install it from https://ollama.com/download"
            ));
        }
    } else {
        println!("✓ Ollama installed");
    }

    // Step 2: Service reachable
    if !is_ollama_reachable(&config).await {
        println!("  Ollama service is not running.");
        if confirm("Start Ollama now?")? {
            start_ollama()?;
            wait_until_reachable(&config, Duration::from_secs(20)).await?;
        } else {
            return Err(anyhow!(
                "Ollama must be running. Start it with: ollama serve"
            ));
        }
    } else {
        println!("✓ Ollama running");
    }

    // Step 3: Model pulled
    let model = &config.model;
    if !is_model_pulled(&config, model).await? {
        println!("  Model {} not found locally.", model);
        if confirm(&format!("Pull model {} (~2GB)?", model))? {
            ollama_pull(model)?;
        } else {
            return Err(anyhow!("Model {} is required.", model));
        }
    } else {
        println!("✓ Model {} ready", model);
    }

    // Step 4: Smoke test
    print!("  Running smoke test... ");
    smoke_test(&config).await?;
    println!("passed");

    println!("\n✓ Setup complete. Try: glit \"show status\"");
    Ok(())
}

// =============================================================================
// Detection helpers
// =============================================================================

pub async fn is_ollama_reachable(config: &Config) -> bool {
    let url = format!("{}/api/tags", config.ollama_url.trim_end_matches('/'));
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()
        .map(|c| {
            // Use blocking check via a short-lived runtime isn't possible here —
            // we're already in async, so just try the request.
            let _ = c;
            true // handled below via await
        });

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    client
        .get(&url)
        .send()
        .await
        .map_or(false, |r| r.status().is_success())
}

async fn is_model_pulled(config: &Config, model: &str) -> anyhow::Result<bool> {
    let url = format!("{}/api/tags", config.ollama_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to reach Ollama /api/tags")?;

    let json: serde_json::Value = resp.json().await.context("Invalid JSON from /api/tags")?;

    let models = json["models"].as_array().cloned().unwrap_or_default();
    let found = models.iter().any(|m| {
        m["name"]
            .as_str()
            .map(|n| {
                n == model
                    || n.starts_with(&format!("{}:", model.split(':').next().unwrap_or(model)))
            })
            .unwrap_or(false)
    });
    Ok(found)
}

// =============================================================================
// Install / start helpers
// =============================================================================

fn install_ollama() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        if which::which("brew").is_ok() {
            println!("  Running: brew install ollama");
            run_visible("brew", &["install", "ollama"])
        } else {
            println!("  Running: curl -fsSL https://ollama.com/install.sh | sh");
            run_visible(
                "sh",
                &["-c", "curl -fsSL https://ollama.com/install.sh | sh"],
            )
        }
    } else if cfg!(target_os = "linux") {
        println!("  Running: curl -fsSL https://ollama.com/install.sh | sh");
        run_visible(
            "sh",
            &["-c", "curl -fsSL https://ollama.com/install.sh | sh"],
        )
    } else {
        Err(anyhow!(
            "Auto-install not supported on this OS. \
             Install Ollama manually from https://ollama.com/download"
        ))
    }
}

fn start_ollama() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") && which::which("brew").is_ok() {
        println!("  Running: brew services start ollama");
        // Non-fatal: if brew services fails we still try spawning directly
        let _ = run_visible("brew", &["services", "start", "ollama"]);
    } else {
        // Spawn `ollama serve` in background, detached from this process
        println!("  Starting: ollama serve (background)");
        std::process::Command::new("ollama")
            .arg("serve")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn `ollama serve`")?;
    }
    Ok(())
}

async fn wait_until_reachable(config: &Config, timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if is_ollama_reachable(config).await {
            println!("✓ Ollama running");
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(anyhow!(
                "Ollama did not become reachable within {}s. \
                 Try running `ollama serve` manually.",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn ollama_pull(model: &str) -> anyhow::Result<()> {
    println!("  Running: ollama pull {}", model);
    run_visible("ollama", &["pull", model])
}

async fn smoke_test(config: &Config) -> anyhow::Result<()> {
    crate::intent::call_ollama(
        "Reply with the single word: ok",
        "You are a test. Reply with the single word: ok",
        config,
        0.0,
    )
    .await
    .context("Smoke test failed — Ollama responded with an error")?;
    Ok(())
}

// =============================================================================
// Shared utilities
// =============================================================================

fn confirm(question: &str) -> anyhow::Result<bool> {
    inquire::Confirm::new(question)
        .with_default(true)
        .prompt()
        .map_err(|e| anyhow!("Prompt error: {}", e))
}

/// Run a command with inherited stdout/stderr so the user sees output.
fn run_visible(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("Failed to run `{}`", cmd))?;

    if !status.success() {
        return Err(anyhow!(
            "`{} {}` exited with status {}",
            cmd,
            args.join(" "),
            status
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_confirm_helper_compiles() {
        // confirm() is interactive; just verify it compiles and the function exists
        let _: fn(&str) -> anyhow::Result<bool> = confirm;
    }

    #[test]
    fn test_run_visible_success() {
        // Use `true` (always-exit-0 command) on Unix
        #[cfg(unix)]
        run_visible("true", &[]).unwrap();
    }

    #[test]
    fn test_run_visible_failure() {
        #[cfg(unix)]
        let result = run_visible("false", &[]);
        #[cfg(unix)]
        assert!(result.is_err());
    }
}
