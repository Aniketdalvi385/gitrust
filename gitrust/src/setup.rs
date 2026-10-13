use anyhow::{anyhow, Context};
use std::time::Instant;

use crate::config::Config;

/// The Kev commit GiTrust was evaluated against (docs/classifier_eval). Pinned
/// so a setup today serves the same code the numbers were measured on.
const KEV_REPO: &str = "https://github.com/jaredpalmer/kev.git";
const KEV_COMMIT: &str = "85ccf56a0afb952323c621055b02876e8498d60c";

/// Entry point for `gitrust setup`.
/// uv on PATH → Kev checkout (cloned + synced if missing) → server running →
/// smoke test. Saves `kev_dir` to ~/.gitrust.toml so later queries can start
/// the server in the background on demand.
pub async fn run_setup() -> anyhow::Result<()> {
    let mut config = crate::config::load().unwrap_or_default();

    println!("GiTrust setup\n");

    // Step 1: uv (runs Kev's Python environment)
    if which::which("uv").is_err() {
        return Err(anyhow!(
            "`uv` is required to run the local Kev model.\n  \
             Install it: brew install uv   (or https://docs.astral.sh/uv/)"
        ));
    }
    println!("✓ uv installed");

    // Step 2: a Kev checkout with its serve environment
    let default_dir = crate::config::gitrust_dir()?.join("kev");
    let dir = config
        .kev_dir
        .clone()
        .unwrap_or_else(|| default_dir.display().to_string());
    if !std::path::Path::new(&dir)
        .join("kev")
        .join("serve.py")
        .exists()
    {
        println!("  No Kev checkout at {}.", dir);
        if !confirm(&format!(
            "Clone Kev ({}) and install its environment (~1 GB)?",
            &KEV_COMMIT[..7]
        ))? {
            return Err(anyhow!(
                "Kev is required. Clone {} and set kev_dir in ~/.gitrust.toml.",
                KEV_REPO
            ));
        }
        run_visible("git", &["clone", KEV_REPO, &dir])?;
        run_visible("git", &["-C", &dir, "checkout", "-q", KEV_COMMIT])?;
        run_in(&dir, "uv", &["sync", "--extra", "serve"])?;
    }
    println!("✓ Kev checkout at {}", dir);
    if config.kev_dir.as_deref() != Some(dir.as_str()) {
        config.kev_dir = Some(dir.clone());
        crate::config::save(&config)?;
        println!("✓ Saved kev_dir to ~/.gitrust.toml");
    }

    // Step 3: server reachable (starts it in the background if needed; the
    // first start downloads the weights, ~1.5 GB, then ~25 s per cold start)
    if !crate::kev::is_reachable(&config).await {
        println!("  Starting Kev ({}) in the background…", config.kev_run);
    }
    crate::kev::ensure_running(&Config {
        kev_autostart: true,
        ..config.clone()
    })
    .await?;
    println!("✓ Kev running at {}", config.kev_url);

    // Step 4: smoke test — one routing question, timed.
    print!("  Running smoke test… ");
    let started = Instant::now();
    let ranked = crate::kev::ask_choice(
        &config,
        serde_json::json!({ "user_request": "show me the commit history", "repo_state": "clean repo, on branch main" }),
        crate::kev::instructions(),
        crate::kev::ops(),
    )
    .await
    .context("Smoke test failed — Kev responded with an error")?;
    let (op, p) = ranked.first().cloned().unwrap_or_default();
    println!(
        "passed ('show me the commit history' → {} p={:.2}, {:.1}s)",
        op,
        p,
        started.elapsed().as_secs_f32()
    );

    println!("\n✓ Setup complete. Try: gitrust \"show status\"");
    println!("  The Kev server stays resident (~3 GB). Stop it any time: pkill -f kev.serve");
    Ok(())
}

/// Run a command in `dir` with inherited stdout/stderr.
fn run_in(dir: &str, cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .status()
        .with_context(|| format!("Failed to run `{}` in {}", cmd, dir))?;
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
