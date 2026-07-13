mod agent;
mod commands;
mod config;
mod confirm;
mod context;
mod copy;
mod danger;
mod error;
mod executor;
mod explain;
mod format;
mod git_ops;
mod intent;
mod log;
mod manifest;
mod prompts;
mod recovery;
mod resolver;
mod setup;
mod validation;

use anyhow::Context;
use clap::{Parser, Subcommand};

/// GLit — Agentic Git CLI.
/// Natural language interface for Git. Powered by a local LLM (Ollama).
///
/// Examples:
///   glit setup
///   glit "push login changes"
///   glit "commit and push with message feat: add auth"
///   glit "show status"
///   glit --recover
///   glit              # opens interactive REPL
#[derive(Parser, Debug)]
// `disable_help_subcommand`: we define an explicit `Help` subcommand (canonical
// phrases), which collides with clap's auto-generated `help` and trips a
// debug-only assertion ("command name 'help' is duplicated") that panics the
// debug binary at startup. Disabling clap's generated one leaves ours.
#[command(
    name = "glit",
    about = "Agentic Git CLI",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Query to execute (omit for interactive REPL mode).
    query: Option<String>,

    /// Skip all confirmation prompts (auto-confirm). Does NOT bypass type-to-confirm safety gates.
    #[arg(short, long)]
    yes: bool,

    /// Recover from an interrupted pipeline (restores HEAD and index to pre-pipeline state).
    #[arg(long)]
    recover: bool,

    /// Override the Ollama model for this run.
    #[arg(long)]
    model: Option<String>,

    /// Show all modified files in file selection (including unrelated).
    #[arg(long)]
    show_all: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Install Ollama (if missing), start service, and pull the LLM model.
    Setup,
    /// Show the list of canonical phrases GLit understands.
    Help,
    /// Manage recovery snapshots (preserved when a pipeline fails).
    Recover {
        /// List all preserved snapshots without modifying them.
        #[arg(long)]
        list: bool,
        /// Interactively pick snapshots to delete.
        #[arg(long)]
        clean: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("✗  {}", e);
        let mut source = e.source();
        while let Some(cause) = source {
            eprintln!("   caused by: {}", cause);
            source = cause.source();
        }
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Setup subcommand — runs before loading config
    if let Some(Command::Setup) = cli.command {
        return setup::run_setup().await;
    }

    // Help is also config-independent.
    if let Some(Command::Help) = cli.command {
        print_help();
        return Ok(());
    }

    // Recover subcommand — handle --list / --clean before loading config.
    if let Some(Command::Recover { list, clean }) = cli.command {
        return run_recover_subcommand(list, clean);
    }

    // Pre-flight: check Ollama is installed before doing anything else
    check_ollama_or_guide();

    // Load config, apply CLI overrides
    let mut config = config::load()?;
    if let Some(model) = cli.model {
        config.model = model;
    }

    // Recovery mode
    if cli.recover {
        return agent::execute_recovery();
    }

    match cli.query {
        // One-shot mode: glit "push login changes"
        Some(query) => {
            agent::run_agent(&query, &config, cli.yes, cli.show_all).await?;
        }

        // Interactive REPL mode: glit (no args)
        None => {
            run_repl(&config, cli.yes, cli.show_all).await?;
        }
    }

    Ok(())
}

fn print_help() {
    println!("GLit canonical phrases (always work, never need the LLM):\n");
    println!("  Inspect:    status · log · show · diff · branches · remotes · config");
    println!("  Track:      fetch · safe to pull · merge preview · check remote");
    println!("  Stage:      commit · commit all my files · commit <file>");
    println!("  Branch:     create branch <name> · switch to <name> · current branch");
    println!("  Sync:       push · publish · pull · rebase");
    println!("  Recover:    undo · stash · stash pop · force push · discard local");
    println!("  Setup:      init · add remote <url> · delete repository");
    println!("  Tooling:    glit setup · glit help · glit recover --list / --clean");
    println!();
    println!("Anything else: phrase it naturally — the AI will figure it out.");
    println!("Use --yes to auto-confirm non-destructive prompts.");
}

fn run_recover_subcommand(list: bool, clean: bool) -> anyhow::Result<()> {
    use recovery::{FileRecoveryManager, RecoveryManager};
    let mgr = FileRecoveryManager;
    let snapshots = mgr.list_snapshots()?;

    if list || (!list && !clean) {
        if snapshots.is_empty() {
            println!("No preserved snapshots.");
            return Ok(());
        }
        println!("Preserved snapshots (run `glit recover --clean` to remove):\n");
        for s in &snapshots {
            println!("  {}  captured {}", s.pipeline_id, s.captured_at);
            if let Some(head) = &s.head_oid {
                println!("    pre-HEAD: {}", &head[..head.len().min(7)]);
            }
        }
        if !clean {
            return Ok(());
        }
        println!();
    }

    if clean {
        if snapshots.is_empty() {
            println!("Nothing to clean.");
            return Ok(());
        }
        let labels: Vec<String> = snapshots
            .iter()
            .map(|s| format!("{}  ({})", s.pipeline_id, s.captured_at))
            .collect();
        let to_delete = inquire::MultiSelect::new("Pick snapshots to delete:", labels)
            .prompt()
            .map_err(|e| anyhow::anyhow!("Aborted: {}", e))?;
        if to_delete.is_empty() {
            println!("  Nothing selected.");
            return Ok(());
        }
        for label in to_delete {
            // First field of the label is the id.
            let id = label.split_whitespace().next().unwrap_or("");
            if let Err(e) = mgr.delete_snapshot_by_id(id) {
                eprintln!("  Could not delete {}: {}", id, e);
            } else {
                println!("  ✓ Deleted {}", id);
            }
        }
    }
    Ok(())
}

/// Print a friendly setup guide and exit if Ollama is not installed.
fn check_ollama_or_guide() {
    if which::which("ollama").is_ok() {
        return;
    }

    eprintln!("GLit needs Ollama to run the local LLM.\n");
    eprintln!("  Install Ollama:");

    if cfg!(target_os = "macos") {
        eprintln!("    brew install ollama");
    } else if cfg!(target_os = "linux") {
        eprintln!("    curl -fsSL https://ollama.com/install.sh | sh");
    } else {
        eprintln!("    https://ollama.com/download");
    }

    eprintln!("\n  Then run the one-time setup:");
    eprintln!("    glit setup\n");

    std::process::exit(1);
}

/// REPL loop — read queries from the user interactively.
async fn run_repl(
    config: &config::Config,
    auto_confirm: bool,
    show_all: bool,
) -> anyhow::Result<()> {
    use inquire::Text;

    println!("GLit — Agentic Git CLI  (Ctrl-C or empty input to exit)");
    println!("Model: {}  |  Remote: {}", config.model, config.remote);
    println!();

    loop {
        let query = match Text::new("glit>").prompt() {
            Ok(q) => q,
            Err(inquire::InquireError::OperationCanceled)
            | Err(inquire::InquireError::OperationInterrupted) => {
                println!("Bye.");
                break;
            }
            Err(e) => return Err(e).context("REPL prompt failed"),
        };

        let trimmed = query.trim();
        if trimmed.is_empty() {
            println!("Bye.");
            break;
        }

        if let Err(e) = agent::run_agent(trimmed, config, auto_confirm, show_all).await {
            eprintln!("✗  {}", e);
        }

        println!();
    }

    Ok(())
}
