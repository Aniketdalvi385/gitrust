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
mod kev;
mod log;
mod manifest;
mod recovery;
mod resolver;
mod setup;
mod trash;
mod ux;
mod validation;

use anyhow::Context;
use clap::{Parser, Subcommand};

/// GiTrust — say what you want to do in Git, in plain words.
/// A small local model (Kev) picks from a fixed list of Git operations — it
/// never invents a command — and GiTrust asks you whenever it isn't sure.
///
/// Examples:
///   gitrust "status"
///   gitrust "commit and push"
///   gitrust "push only the login changes"
///   gitrust "undo"
///   gitrust                    # interactive mode
///   gitrust help               # phrases that work instantly
///   gitrust setup              # install the local model
#[derive(Parser, Debug)]
// `disable_help_subcommand`: we define an explicit `Help` subcommand (canonical
// phrases), which collides with clap's auto-generated `help` and trips a
// debug-only assertion ("command name 'help' is duplicated") that panics the
// debug binary at startup. Disabling clap's generated one leaves ours.
#[command(
    name = "gitrust",
    about = "Say what you want to do in Git, in plain words.",
    version,
    disable_help_subcommand = true,
    verbatim_doc_comment
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

    /// Kev checkpoint to serve if GiTrust has to start the server (Hub id or
    /// local run directory, e.g. a fine-tune). Ignored if a server is already up.
    #[arg(long)]
    model: Option<String>,

    /// Show all modified files in file selection (including unrelated).
    #[arg(long)]
    show_all: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Set up the local Kev model (clone, install, start, smoke test).
    Setup,
    /// Show the list of canonical phrases GiTrust understands.
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
    /// Internal: owns a background Kev server and stops it when idle.
    #[command(name = "kev-supervise", hide = true)]
    KevSupervise {
        #[arg(long)]
        dir: String,
        #[arg(long)]
        run: String,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        idle_minutes: u64,
    },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        if ux::is_canceled(&e) {
            // A backed-out run: calm, no error chain — but still exit 1, because
            // nothing was done (see ux.rs "Cancel semantics").
            eprintln!("{}  {}", copy::CANCELED_GLYPH, copy::CANCELED);
            std::process::exit(1);
        }
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

    if let Some(Command::KevSupervise {
        dir,
        run,
        port,
        idle_minutes,
    }) = &cli.command
    {
        return kev::supervise(dir, run, *port, *idle_minutes);
    }

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

    // Load config, apply CLI overrides
    let mut config = config::load()?;
    if let Some(model) = cli.model {
        config.kev_run = model;
    }

    // Recovery mode
    if cli.recover {
        return agent::execute_recovery();
    }

    match cli.query {
        // One-shot mode: gitrust "push login changes"
        Some(query) => {
            agent::run_agent(&query, &config, cli.yes, cli.show_all).await?;
        }

        // Interactive REPL mode: gitrust (no args)
        None => {
            run_repl(&config, cli.yes, cli.show_all).await?;
        }
    }

    Ok(())
}

fn print_help() {
    println!("GiTrust — say what you want to do in Git, in plain words.\n");
    println!("These always work instantly (no model needed):\n");
    for (section, phrases) in copy::HELP_SECTIONS {
        println!("  {:<13}{}", format!("{}:", section), phrases.join(" · "));
    }
    println!();
    println!("Anything else, just say it:  gitrust \"push only the login changes\"");
    println!("                             gitrust \"what did I do yesterday\"");
    println!("{}", copy::HELP_TRUST);
    println!();
    println!("Keys: Enter chooses or runs · Esc cancels any prompt (nothing is changed).");
    println!("--yes skips prompts, but never picks an option for you and never skips");
    println!("a type-to-confirm.  GITRUST_DEBUG=1 shows the model's scores.");
    println!();
    println!("More: gitrust setup · gitrust recover --list · gitrust --help");
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
        println!("Preserved snapshots (run `gitrust recover --clean` to remove):\n");
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
        let to_delete = ux::prompt_result(
            inquire::MultiSelect::new("Pick snapshots to delete:", labels).prompt(),
            "Snapshot picker",
        )?;
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

/// REPL loop — read queries from the user interactively.
async fn run_repl(
    config: &config::Config,
    auto_confirm: bool,
    show_all: bool,
) -> anyhow::Result<()> {
    use inquire::Text;

    println!("GiTrust — say what you want to do in Git.  help lists instant phrases · exit leaves");
    println!(
        "Local model: {} · remote: {}",
        config.kev_run, config.remote
    );
    println!();

    loop {
        let query = match Text::new("gitrust>").prompt() {
            Ok(q) => q,
            Err(inquire::InquireError::OperationCanceled)
            | Err(inquire::InquireError::OperationInterrupted) => {
                println!("Bye.");
                break;
            }
            Err(e) => return Err(e).context("REPL prompt failed"),
        };

        let trimmed = query.trim();
        if trimmed.is_empty() || ux::is_repl_exit(trimmed) {
            println!("Bye.");
            break;
        }
        if matches!(trimmed.to_lowercase().as_str(), "help" | "?" | "h") {
            print_help();
            println!();
            continue;
        }

        if let Err(e) = agent::run_agent(trimmed, config, auto_confirm, show_all).await {
            if ux::is_canceled(&e) {
                eprintln!("{}  {}", copy::CANCELED_GLYPH, copy::CANCELED);
            } else {
                eprintln!("✗  {}", e);
            }
        }

        println!();
    }

    Ok(())
}
