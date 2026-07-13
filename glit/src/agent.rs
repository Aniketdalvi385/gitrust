use anyhow::Context;
use inquire::Text;

use crate::commands::{
    CommitMessage, FeaturePipelineOp, GateResponse, GateType, IntentType, PipelineStep,
    PreCheckResult, ReadOnlyOp, ResetMode, ResolutionOutcome, ScopedWriteOp,
};
use crate::config::Config;
use crate::confirm::{AutoConfirmGate, ConfirmationGate, InquireGate};
use crate::context;
use crate::executor::{DefaultExecutor, StepExecutor};
use crate::git_ops::{Git2Operations, GitOperations};
use crate::intent::{IntentClassifier, OllamaClassifier};
use crate::manifest;
use crate::recovery::{FileRecoveryManager, RecoveryManager};
use crate::resolver::{FileResolver, TwoPassResolver};
use crate::validation::{capture_pre_step_state, DefaultValidator, PostCheckResult, Validator};

/// Print the one-glyph engine indicator showing which path classified the
/// query — fast canonical (⚡) vs LLM (◉). Latency is shown for the LLM only
/// since the fast-path is sub-millisecond. ASCII fallback when color is off.
fn print_engine_marker(engine: crate::commands::Engine, llm_elapsed: Option<std::time::Duration>) {
    let use_color = crate::danger::use_color();
    let (glyph, suffix, ansi) = match engine {
        crate::commands::Engine::Fast => {
            let g = if use_color {
                crate::copy::ENGINE_FAST_GLYPH
            } else {
                crate::copy::ENGINE_FAST_ASCII
            };
            (g, "(instant)".to_string(), "\x1b[2;36m") // dim cyan
        }
        crate::commands::Engine::Llm => {
            let g = if use_color {
                crate::copy::ENGINE_LLM_GLYPH
            } else {
                crate::copy::ENGINE_LLM_ASCII
            };
            let suffix = match llm_elapsed {
                Some(d) if d.as_millis() >= 100 => format!("(llm · ~{:.1}s)", d.as_secs_f32()),
                _ => "(llm)".to_string(),
            };
            (g, suffix, "\x1b[2;35m") // dim magenta
        }
    };
    if use_color {
        println!("{}{}\x1b[0m  {}", ansi, glyph, suffix);
    } else {
        println!("{}  {}", glyph, suffix);
    }
}

/// Run the full agent pipeline for one query.
/// Layers: 0 (context) → 1 (intent) → 2 (resolve) → 3 (validate/gate) → 4 (execute)
pub async fn run_agent(
    query: &str,
    config: &Config,
    auto_confirm: bool,
    show_all: bool,
) -> anyhow::Result<()> {
    let gate: Box<dyn ConfirmationGate> = if auto_confirm {
        Box::new(AutoConfirmGate)
    } else {
        Box::new(InquireGate)
    };

    // If not inside a git repo, check if the intent is "init" before erroring out.
    if find_repo_root().is_err() {
        let empty_ctx = crate::context::RepoContext::empty();
        let classifier = OllamaClassifier;
        let started = std::time::Instant::now();
        let intent_result =
            clarification_loop(&classifier, &empty_ctx, query, config, &*gate, config.max_iters)
                .await?;
        let elapsed = if intent_result.engine == crate::commands::Engine::Llm {
            Some(started.elapsed())
        } else {
            None
        };
        print_engine_marker(intent_result.engine, elapsed);
        return match intent_result.intent {
            IntentType::ScopedWrite(ScopedWriteOp::Init) => init_repo(&*gate),
            _ => Err(anyhow::anyhow!(
                "Not inside a git repository. Run 'glit \"initialize a repository\"' or 'git init' first."
            )),
        };
    }

    let repo_path = find_repo_root()?;
    let mut repo = git2::Repository::open(&repo_path)
        .map_err(|_| anyhow::anyhow!("Not a git repository: {}", repo_path.display()))?;

    // Check for interrupted pipeline from a previous crash
    let recovery_mgr = FileRecoveryManager;
    if let Ok(Some(snapshot)) = recovery_mgr.check_for_interrupted() {
        println!(
            "⚠  An interrupted pipeline was found (captured at {}).",
            snapshot.captured_at
        );
        println!(
            "   Run 'glit --recover' to restore the repository to its pre-pipeline state."
        );
        println!("   Or delete ~/.glit/snapshots/ to ignore it.\n");
    }

    // Pre-flight: make sure git identity is configured. First-time users
    // hit a deep-in-the-pipeline failure on first commit without this.
    ensure_git_identity(&repo, &*gate)?;

    // Layer 0: build repo context
    let context = context::build_context(&repo)
        .unwrap_or_else(|_| crate::context::RepoContext::empty());

    // Layer 1: classify intent (with clarification loop)
    let classifier = OllamaClassifier;
    let classify_started = std::time::Instant::now();
    let intent_result =
        clarification_loop(&classifier, &context, query, config, &*gate, config.max_iters).await?;
    let llm_elapsed = if intent_result.engine == crate::commands::Engine::Llm {
        Some(classify_started.elapsed())
    } else {
        None
    };
    print_engine_marker(intent_result.engine, llm_elapsed);

    // Layer 3 (early): the situation gate. An intent that is invalid for the
    // current RepoSituation is stopped here, BEFORE resolution — the single
    // source of state-based blocking (subsumes Phase 9's detached-HEAD
    // pre-check and the execute_pipeline rescue routing).
    match crate::validation::situation_gate(&context.situation, &intent_result.intent) {
        crate::validation::SituationVerdict::Allowed => {}
        crate::validation::SituationVerdict::Blocked { message } => {
            // TODO(Phase 12): migrate onto SituationVerdict::Redirect
            if matches!(
                context.situation,
                crate::commands::RepoSituation::DetachedHead { .. }
            ) && crate::validation::is_write_intent(&intent_result.intent)
            {
                // Preserve the Phase 9 interactive way-out (branch-here / switch-back).
                return handle_detached_head_rescue(&mut repo, &*gate).await;
            }
            eprintln!("✗  {}", message);
            return Ok(());
        }
        crate::validation::SituationVerdict::Redirect { .. } => { /* Phase 12+ */ }
    }

    // Dispatch based on intent
    match intent_result.intent {
        // MergePreview needs &mut repo so it can offer a follow-up resolution action.
        IntentType::ReadOnly(ReadOnlyOp::MergePreview { remote }) => {
            handle_merge_preview(&mut repo, &remote, config, &*gate, auto_confirm).await
        }
        IntentType::ReadOnly(op) => handle_read_only(&repo, op, &context),
        IntentType::ScopedWrite(op) => {
            handle_scoped_write(&mut repo, op, config, &*gate, auto_confirm, show_all).await
        }
        IntentType::FeaturePipeline(op) => {
            handle_feature_pipeline(&mut repo, op, config, &*gate, auto_confirm, show_all).await
        }
        IntentType::NeedsClarification(question) => {
            println!("? {}", question);
            println!("  (Max clarification attempts reached. Try rephrasing your query.)");
            Ok(())
        }
    }
}

/// Run a clarification loop: keep re-classifying until confident or max_iters hit.
async fn clarification_loop(
    classifier: &OllamaClassifier,
    context: &crate::context::RepoContext,
    initial_query: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
    max_iters: u8,
) -> anyhow::Result<crate::commands::IntentResult> {
    let mut query = initial_query.to_string();

    // Fast path: exact-match canonical phrases bypass the LLM entirely.
    if let Some(intent) = crate::intent::try_canonical(&query) {
        return Ok(crate::commands::IntentResult {
            intent,
            confidence: 1.0,
            raw_query: query,
            engine: crate::commands::Engine::Fast,
        });
    }

    for iter in 0..max_iters {
        let result = match classifier.classify(context, &query, config).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("⚠  LLM classification failed: {}", e);
                eprintln!("   Falling back to manual mode.\n");
                return fallback_interactive_intent();
            }
        };

        match &result.intent {
            IntentType::NeedsClarification(question) => {
                if iter + 1 >= max_iters {
                    return Ok(result);
                }
                println!("? {}", question);

                // Ask user for clarification
                let clarified = match gate.prompt(&GateType::SimpleConfirm {
                    message: "Would you like to provide more context? (no = abort)".to_string(),
                }) {
                    Ok(GateResponse::Confirmed) => {
                        // Prompt for clarification text inline
                        match Text::new("Clarification:").prompt() {
                            Ok(t) => t,
                            Err(_) => return Err(anyhow::anyhow!("Clarification aborted.")),
                        }
                    }
                    _ => return Err(anyhow::anyhow!("Aborted.")),
                };

                query = format!("{} (clarification: {})", query, clarified);
            }
            _ => {
                // Mid-confidence: offer a deterministic disambiguation menu.
                if result.confidence < 0.75 && result.confidence >= 0.4 {
                    if let Some(chosen) =
                        maybe_disambiguate(&result.intent, context, gate)?
                    {
                        return Ok(crate::commands::IntentResult {
                            intent: chosen,
                            confidence: 1.0,
                            raw_query: result.raw_query,
                            engine: result.engine,
                        });
                    }
                }
                return Ok(result);
            }
        }
    }

    Err(anyhow::anyhow!(
        "Max clarification iterations ({}) reached.",
        max_iters
    ))
}

// =============================================================================
// Read-only handler
// =============================================================================

fn handle_read_only(
    repo: &git2::Repository,
    op: ReadOnlyOp,
    ctx: &crate::context::RepoContext,
) -> anyhow::Result<()> {
    let git_ops = Git2Operations;
    match op {
        ReadOnlyOp::Status => {
            // Rich header: branch · upstream · ahead/behind
            println!("{}", ctx.render_status_header());
            if !ctx.last_commit.is_empty() && ctx.last_commit != "(no commits yet)" {
                println!("Last commit: {}", ctx.last_commit);
            }
            println!();
            print!("{}", ctx.status); // Display impl ends each line with \n
            if let Some(s) = ctx.suggestion() {
                println!("\n→ {}", s);
            }
            // Honesty: ahead/behind comes from the last fetch.
            // Show this whenever an upstream exists — counts could be wrong
            // regardless of whether they're currently zero.
            if ctx.upstream.is_some() {
                println!("\n(Remote state from last fetch — run `glit \"fetch\"` to refresh.)");
            }
        }
        ReadOnlyOp::Log { count } => {
            let entries = git_ops.log(repo, count)?;
            if entries.is_empty() {
                println!("No commits yet.");
                return Ok(());
            }
            println!("{}", ctx.render_status_header());
            println!();
            // First entry is HEAD — mark it.
            for (i, entry) in entries.iter().enumerate() {
                if i == 0 {
                    println!("{}  ← HEAD", entry);
                } else {
                    println!("{}", entry);
                }
            }
        }
        ReadOnlyOp::Diff { path } => {
            let p = path.as_deref().unwrap_or(".");
            let snippet = git_ops.get_diff_snippet(repo, p, 200)?;
            if snippet.is_empty() {
                println!("No changes vs HEAD.");
                if ctx.last_commit != "(no commits yet)" && ctx.last_commit != "(unavailable)" {
                    println!("Last commit: {}", ctx.last_commit);
                }
                println!("  → Try: glit \"log\" for history, or glit \"show\" for full commit details.");
            } else {
                println!("{}", snippet);
            }
        }
        ReadOnlyOp::Remotes => {
            let remotes = git_ops.list_remotes(repo)?;
            if remotes.is_empty() {
                println!("No remotes configured.");
                println!("  → Add one: glit \"add remote origin <url>\"");
            } else {
                println!("Remotes:");
                for (name, url) in &remotes {
                    println!("  {}  →  {}", name, url);
                }
            }
        }
        ReadOnlyOp::Branches => {
            // Header tells the user where they are right now.
            println!("{}", ctx.render_status_header());
            println!();
            print_branches_enriched(repo)?;
        }
        ReadOnlyOp::Config => {
            let (name, email) = git_ops.get_user_config(repo)?;
            println!("Git identity:");
            println!("  user.name   = {}", name);
            println!("  user.email  = {}", email);
            if name == "(not set)" || email == "(not set)" {
                println!("\n  → Set with: git config --global user.name \"Your Name\"");
                println!("              git config --global user.email \"you@example.com\"");
            }
        }
        ReadOnlyOp::Show { target } => {
            let target_ref = target.as_deref().unwrap_or("HEAD");
            let details = git_ops.get_commit_details(repo, target_ref)?;
            print_commit_details(&details, target_ref);
        }
        ReadOnlyOp::MergePreview { remote: _ } => {
            // Routed to handle_merge_preview in run_agent — this arm is unreachable
            // but kept for exhaustiveness.
            unreachable!("MergePreview is dispatched in run_agent, not handle_read_only");
        }
        ReadOnlyOp::FetchStatus { remote } => {
            // Honest "am I up to date" — fetch first, then re-read context.
            if !crate::context::remote_exists(repo, &remote) {
                println!("No remote '{}' configured. Try: glit \"add remote origin <url>\"", remote);
                return Ok(());
            }
            print!("Fetching {}… ", remote);
            std::io::Write::flush(&mut std::io::stdout()).ok();
            match git_ops.fetch_remote(repo, &remote) {
                Ok(()) => println!("✓"),
                Err(e) => {
                    println!("✗");
                    eprintln!("  Could not fetch: {}", e);
                    println!("\n(Showing status based on the last known remote state.)");
                    println!("{}", ctx.render_status_header());
                    return Ok(());
                }
            }
            // Rebuild context so ahead/behind reflects what was just fetched.
            let fresh = crate::context::build_context(repo).unwrap_or_else(|_| ctx.clone());
            println!();
            println!("{}", fresh.render_status_header());
            if !fresh.last_commit.is_empty()
                && fresh.last_commit != "(no commits yet)"
                && fresh.last_commit != "(unavailable)"
            {
                println!("Last commit: {}", fresh.last_commit);
            }
            if fresh.behind > 0 {
                println!("\n→ {} commit(s) on remote you don't have. Run: glit \"pull\"", fresh.behind);
            } else if fresh.ahead > 0 {
                println!("\n→ {} unpushed commit(s). Run: glit \"push\"", fresh.ahead);
            } else if fresh.upstream.is_some() {
                println!("\n→ Local and remote are in sync.");
            }
        }
    }
    Ok(())
}

/// Render the result of a merge-conflict prediction.
fn print_merge_preview(
    preview: &crate::git_ops::MergePreview,
    dirty: &crate::git_ops::DirtyOverlap,
) {
    use crate::git_ops::MergePreview as P;
    println!();
    match preview {
        P::UpToDate => {
            println!("✓ Up to date — nothing to pull.");
        }
        P::LocalAhead { local_commits } => {
            println!(
                "✓ Local is {} commit(s) ahead of remote. Pull is a no-op.",
                local_commits
            );
            println!("  → To share: glit \"push\"");
        }
        P::FastForward { remote_commits } => {
            println!(
                "✓ Pull will fast-forward {} commit(s). No conflicts possible.",
                remote_commits
            );
            print_dirty_overlap_block(dirty, /*conflicts_present=*/ false);
            if dirty.overlapping_files.is_empty() {
                println!("  → Safe to: glit \"pull\"");
            }
        }
        P::CleanMerge {
            local_commits,
            remote_commits,
        } => {
            println!(
                "✓ Clean 3-way merge predicted ({} local, {} remote commit(s) to merge).",
                local_commits, remote_commits
            );
            print_dirty_overlap_block(dirty, /*conflicts_present=*/ false);
            if dirty.overlapping_files.is_empty() {
                println!("  → Safe to: glit \"pull\"");
            }
        }
        P::Conflicts {
            local_commits: _,
            remote_commits: _,
            files,
        } => {
            // Confident header — once fetched, we KNOW (handoff principle #2).
            let warn = crate::danger::colorize(
                crate::danger::DangerLevel::Caution,
                "⚠ Pull will conflict",
            );
            println!("{}", warn);
            for f in files {
                println!("  · {}", f);
            }
            print_dirty_overlap_block(dirty, /*conflicts_present=*/ true);
            // Resolution options follow as a picker (handle_merge_preview).
            // The picker labels carry the recovery promise inline.
        }
        P::NoUpstream => {
            println!("No upstream configured for this branch.");
            println!("  → Set one: glit \"publish\"");
        }
        P::NoRemoteTracking => {
            println!("No remote-tracking ref found (have you fetched recently?).");
        }
    }
}

fn print_dirty_overlap_block(dirty: &crate::git_ops::DirtyOverlap, conflicts_present: bool) {
    if dirty.overlapping_files.is_empty() {
        return;
    }
    if conflicts_present {
        println!("\nAlso: you have local changes to files the merge will touch:");
    } else {
        let warn = crate::danger::colorize(
            crate::danger::DangerLevel::Caution,
            "⚠ You have local changes to files the merge will touch:",
        );
        println!("\n{}", warn);
    }
    for f in &dirty.overlapping_files {
        println!("  · {}", f);
    }
    println!("  → Pull will block until these are committed or stashed.");
    println!("    Try: glit \"commit all my files\" — or — glit \"stash\"");
}

/// Print a `git show`-style block for a single commit.
fn print_commit_details(d: &crate::git_ops::CommitDetails, requested_ref: &str) {
    let head_marker = if requested_ref == "HEAD" { " (HEAD)" } else { "" };
    println!("commit {}{}", d.sha, head_marker);
    if !d.email.is_empty() {
        println!("Author: {} <{}>", d.author, d.email);
    } else {
        println!("Author: {}", d.author);
    }
    let rel = crate::format::format_relative(d.time_epoch);
    let abs = chrono::DateTime::from_timestamp(d.time_epoch, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    println!("Date:   {} ({})", abs, rel);
    println!();
    for line in d.message.lines() {
        println!("    {}", line);
    }
    if d.parent_sha.is_some() && !d.file_changes.is_empty() {
        println!("\nFiles changed:");
        let max_path = d
            .file_changes
            .iter()
            .map(|f| f.path.len())
            .max()
            .unwrap_or(0);
        for f in &d.file_changes {
            println!(
                "  {:<width$}  +{}  -{}",
                f.path,
                f.additions,
                f.deletions,
                width = max_path
            );
        }
    } else if d.parent_sha.is_none() {
        println!("\n(initial commit — no parent to diff against)");
    }
}

/// Print each branch with its upstream and ahead/behind, current marked with *.
fn print_branches_enriched(repo: &git2::Repository) -> anyhow::Result<()> {
    let current = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from));

    let mut any = false;
    for branch_result in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch_result?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };
        any = true;
        let marker = if current.as_deref() == Some(name.as_str()) { "*" } else { " " };

        // Upstream / ahead-behind info
        let detail = match branch.upstream() {
            Ok(up) => {
                let up_name = up.name().ok().flatten().unwrap_or("(unknown)").to_string();
                let (ahead, behind) = match (branch.get().target(), up.get().target()) {
                    (Some(local), Some(remote)) => {
                        repo.graph_ahead_behind(local, remote).unwrap_or((0, 0))
                    }
                    _ => (0, 0),
                };
                if ahead == 0 && behind == 0 {
                    format!("  →  {} (up to date)", up_name)
                } else {
                    format!("  →  {} (↑{} ↓{})", up_name, ahead, behind)
                }
            }
            Err(_) => "  (no upstream)".to_string(),
        };
        println!("{} {}{}", marker, name, detail);
    }

    if !any {
        println!("No branches yet (make your first commit to create one).");
    }
    Ok(())
}

// =============================================================================
// Scoped write handler (single-step ops)
// =============================================================================

/// Run MergePreview with an interactive follow-up: when conflicts are predicted,
/// offer the user a menu of resolution strategies and dispatch the chosen one
/// through the normal scoped-write pipeline (with its own confirms + recovery).
async fn handle_merge_preview(
    repo: &mut git2::Repository,
    remote: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
    auto_confirm: bool,
) -> anyhow::Result<()> {
    let git_ops = Git2Operations;
    if !crate::context::remote_exists(repo, remote) {
        println!("No remote '{}' configured.", remote);
        return Ok(());
    }
    print!("Fetching {}… ", remote);
    std::io::Write::flush(&mut std::io::stdout()).ok();
    if let Err(e) = git_ops.fetch_remote(repo, remote) {
        println!("✗");
        eprintln!("  Could not fetch: {}", e);
        return Ok(());
    }
    println!("✓");

    let preview = git_ops.predict_pull(repo, remote)?;
    let dirty = git_ops.dirty_overlap_for_pull(repo, remote).unwrap_or_default();
    print_merge_preview(&preview, &dirty);

    // Open the picker in two cases:
    //   1. Real commit-vs-commit conflicts (Conflicts variant).
    //   2. Fast-forward / clean-merge BUT dirty workdir overlaps with the
    //      files the merge will touch — git will refuse with "would be
    //      overwritten by merge". The user must commit or stash first.
    let has_dirty = !dirty.overlapping_files.is_empty();
    let (local_commits, _remote_commits) = match (&preview, has_dirty) {
        (crate::git_ops::MergePreview::Conflicts {
            local_commits,
            remote_commits,
            ..
        }, _) => (*local_commits, *remote_commits),
        (crate::git_ops::MergePreview::FastForward { remote_commits }, true) => {
            (0usize, *remote_commits)
        }
        (crate::git_ops::MergePreview::CleanMerge {
            local_commits,
            remote_commits,
        }, true) => (*local_commits, *remote_commits),
        _ => return Ok(()),
    };

    let mut labels: Vec<String> = Vec::new();
    let mut actions: Vec<ScopedWriteOp> = Vec::new();

    let dim = |s: &str| -> String {
        if crate::danger::use_color() {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };
    let dest_marker = crate::danger::colorize(
        crate::danger::DangerLevel::Destructive,
        "⚠⚠",
    );

    if has_dirty {
        labels.push(format!(
            "Merge      {}",
            dim("glit \"commit all\" → glit \"pull\"  (commit your edits first)")
        ));
        actions.push(ScopedWriteOp::AddAndCommit {
            message: crate::commands::CommitMessage::Draft("update".to_string()),
            files: crate::commands::FileSelection::All,
        });
        labels.push(format!(
            "Stash      {}",
            dim("glit \"stash\" → glit \"pull\" → glit \"stash pop\"")
        ));
        actions.push(ScopedWriteOp::Stash);
    } else {
        labels.push(format!(
            "Merge      {}",
            dim("glit \"pull\"          merge commit · resolve markers, then commit")
        ));
        actions.push(ScopedWriteOp::Pull {
            remote: remote.to_string(),
        });
        labels.push(format!(
            "Rebase     {}",
            dim(&format!(
                "glit \"rebase\"        replay your {} commit(s) · resolve per commit",
                local_commits
            ))
        ));
        actions.push(ScopedWriteOp::PullRebase {
            remote: remote.to_string(),
        });
        labels.push(format!(
            "Force-push {}  {}",
            dim("glit \"force push\""),
            format!(
                "{}  {}",
                dest_marker,
                dim("rewrites remote · glit recover restores for 30d")
            )
        ));
        actions.push(ScopedWriteOp::ForcePush {
            remote: remote.to_string(),
        });
        labels.push(format!(
            "Discard    {}  {}",
            dim("glit \"discard local\""),
            format!(
                "{}  {}",
                dest_marker,
                dim(&format!(
                    "drops {} commit(s) · glit recover restores for 30d",
                    local_commits
                ))
            )
        ));
        actions.push(ScopedWriteOp::DiscardLocal {
            remote: remote.to_string(),
        });
    }
    labels.push("Cancel".to_string());

    println!();
    let resp = gate.prompt(&GateType::Choose {
        message: "Pick a resolution strategy:".to_string(),
        options: labels,
    })?;

    let chosen_idx = match resp {
        GateResponse::Chose(i) => i,
        _ => {
            println!("  Aborted.");
            return Ok(());
        }
    };

    if chosen_idx >= actions.len() {
        // "Do nothing" or out-of-range — just exit.
        return Ok(());
    }
    let op = actions.remove(chosen_idx);
    handle_scoped_write(repo, op, config, gate, auto_confirm, false).await
}

async fn handle_scoped_write(
    repo: &mut git2::Repository,
    op: ScopedWriteOp,
    config: &Config,
    gate: &dyn ConfirmationGate,
    _auto_confirm: bool,
    show_all: bool,
) -> anyhow::Result<()> {
    let steps = build_scoped_steps(repo, op, config, gate, show_all).await?;
    if steps.is_empty() {
        println!("  Nothing to do.");
        return Ok(());
    }

    execute_pipeline(repo, steps, config, gate).await
}

/// Build pipeline steps for a scoped write op, applying gates as needed.
async fn build_scoped_steps(
    repo: &git2::Repository,
    op: ScopedWriteOp,
    config: &Config,
    gate: &dyn ConfirmationGate,
    show_all: bool,
) -> anyhow::Result<Vec<PipelineStep>> {
    match op {
        ScopedWriteOp::Add { files } => {
            use crate::commands::FileSelection;
            let resolved = match files {
                FileSelection::All => {
                    // Stage all tracked+modified files
                    vec![]  // empty = add_all in git_ops
                }
                FileSelection::Explicit(fs) => fs,
                FileSelection::Pattern(pat) => {
                    // Use manifest to find matching files
                    let manifests = manifest::build_manifest(repo, 25)
                        .context("Failed to build manifest for pattern resolution")?;
                    let classification_result =
                        TwoPassResolver.resolve(&manifests, &pat, config).await?;
                    match classification_result {
                        ResolutionOutcome::Found(cr) => {
                            let gate_response = gate.prompt(&GateType::FileMultiSelect {
                                classification: cr,
                                show_all,
                            })?;
                            match gate_response {
                                GateResponse::FilesSelected(fs) => fs,
                                _ => return Ok(vec![]),
                            }
                        }
                        ResolutionOutcome::NothingMatched { hint, suggestion } => {
                            println!("  No files matched '{}'.", hint);
                            if let Some(s) = suggestion {
                                println!("  Hint: {}", s);
                            }
                            return Ok(vec![]);
                        }
                        ResolutionOutcome::NothingModified => {
                            println!("  No modified files in the repository.");
                            return Ok(vec![]);
                        }
                        ResolutionOutcome::TooManyFiles { count } => {
                            println!(
                                "  Too many modified files ({}). Narrow the feature hint.",
                                count
                            );
                            return Ok(vec![]);
                        }
                        ResolutionOutcome::AmbiguousFeature { options } => {
                            println!("  Ambiguous feature. Did you mean one of: {}", options.join(", "));
                            return Ok(vec![]);
                        }
                    }
                }
            };
            Ok(vec![PipelineStep::Add { files: resolved }])
        }

        ScopedWriteOp::Commit { message } => {
            // Plain commit = only staged → generate message from index-vs-HEAD diff.
            let final_msg = resolve_or_generate_commit_message(
                repo,
                message,
                /*include_workdir=*/ false,
                config,
                gate,
            )
            .await?;
            if final_msg.is_empty() {
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::Commit { message: final_msg }])
        }

        ScopedWriteOp::Branch { name } => {
            if name.trim().is_empty() {
                // No name given — list branches instead of creating an unnamed one.
                let git_ops = Git2Operations;
                let branches = git_ops.list_branches(repo)?;
                if branches.is_empty() {
                    println!("No branches yet (make your first commit to create one).");
                } else {
                    for b in &branches {
                        println!("{}", b);
                    }
                }
                return Ok(vec![]);
            }
            // Validate the name. If invalid, offer a sanitized suggestion and ask.
            let final_name = match validate_branch_name(&name) {
                Ok(()) => name,
                Err(reason) => {
                    let suggestion = sanitize_branch_name(&name);
                    println!(
                        "  Branch name '{}' is not valid: {}",
                        name, reason
                    );
                    let resp = gate.prompt(&GateType::Choose {
                        message: "Choose what to do:".to_string(),
                        options: vec![
                            format!("Use suggested name: '{}'", suggestion),
                            "Enter a different name".to_string(),
                            "Cancel".to_string(),
                        ],
                    })?;
                    match resp {
                        GateResponse::Chose(0) => suggestion,
                        GateResponse::Chose(1) => {
                            let new = Text::new("Branch name:")
                                .prompt()
                                .map_err(|e| anyhow::anyhow!("Aborted: {}", e))?;
                            let new = new.trim().to_string();
                            if let Err(r) = validate_branch_name(&new) {
                                return Err(anyhow::anyhow!("Still invalid: {}", r));
                            }
                            new
                        }
                        _ => {
                            println!("  Aborted.");
                            return Ok(vec![]);
                        }
                    }
                }
            };
            Ok(vec![PipelineStep::Branch { name: final_name }])
        }
        ScopedWriteOp::Checkout { target } => {
            if target.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "No target branch specified. Try: glit \"checkout <branch-name>\""
                ));
            }
            Ok(vec![PipelineStep::Checkout { target }])
        }
        ScopedWriteOp::Stash => Ok(vec![PipelineStep::Stash]),
        ScopedWriteOp::StashPop => Ok(vec![PipelineStep::StashPop]),
        ScopedWriteOp::Pull { remote } => Ok(vec![PipelineStep::Pull { remote }]),

        ScopedWriteOp::Undo => build_undo_step(repo, gate),

        ScopedWriteOp::PullRebase { remote } => {
            let branch = current_branch_name(repo);
            if branch == "HEAD" {
                return Err(anyhow::anyhow!(
                    "No branch to rebase — repository has no commits or HEAD is detached."
                ));
            }
            let resp = gate.prompt(&GateType::SimpleConfirm {
                message: format!(
                    "Rebase your local commits onto {}/{}? (your changes replay on top of remote)",
                    remote, branch
                ),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::PullRebase { remote, branch }])
        }

        ScopedWriteOp::ForcePush { remote } => {
            let branch = current_branch_name(repo);
            if branch == "HEAD" {
                return Err(anyhow::anyhow!("No branch to force-push."));
            }
            // Type-to-confirm — destructive to anyone else watching the remote.
            let op_for_explain = ScopedWriteOp::ForcePush { remote: remote.clone() };
            let ctx_for_danger = crate::context::build_context(repo)
                .unwrap_or_else(|_| crate::context::RepoContext::empty());
            let resp = gate.prompt(&GateType::TypeToConfirm {
                expected: branch.clone(),
                message: format!("Type '{}' to confirm force-push to {}/{}:", branch, remote, branch),
                explanation: Some(crate::explain::explain(&op_for_explain)),
                danger: crate::danger::intent_danger(&op_for_explain, &ctx_for_danger),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::ForcePush { remote, branch }])
        }

        ScopedWriteOp::DiscardLocal { remote } => {
            let branch = current_branch_name(repo);
            if branch == "HEAD" {
                return Err(anyhow::anyhow!("No branch to reset."));
            }
            let ahead_count = {
                use crate::context;
                context::build_context(repo).map(|c| c.ahead).unwrap_or(0)
            };
            let op_for_explain = ScopedWriteOp::DiscardLocal { remote: remote.clone() };
            let ctx_for_danger = crate::context::build_context(repo)
                .unwrap_or_else(|_| crate::context::RepoContext::empty());
            let resp = gate.prompt(&GateType::TypeToConfirm {
                expected: branch.clone(),
                message: format!(
                    "Type '{}' to DISCARD {} local commit(s) and reset to {}/{}:",
                    branch, ahead_count, remote, branch
                ),
                explanation: Some(crate::explain::explain(&op_for_explain)),
                danger: crate::danger::intent_danger(&op_for_explain, &ctx_for_danger),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::DiscardLocal { remote, branch }])
        }

        ScopedWriteOp::Init => {
            // Already inside a repo — re-init is harmless but confirm first
            init_repo(gate)?;
            Ok(vec![])
        }

        ScopedWriteOp::AddRemote { name, url } => {
            if url.is_empty() {
                return Err(anyhow::anyhow!("No remote URL provided. Try: glit 'add remote origin https://github.com/user/repo.git'"));
            }
            let resp = gate.prompt(&GateType::SimpleConfirm {
                message: format!("Add remote '{}' → {}?", name, url),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::AddRemote { name, url }])
        }

        ScopedWriteOp::AddAndCommit { message, files } => {
            use crate::commands::FileSelection;
            let add_files: Vec<String> = match files {
                FileSelection::All => vec![],   // empty = add_all
                FileSelection::Explicit(fs) => fs,
                FileSelection::Pattern(_) => vec![],   // fall back to add_all
            };
            // add_commit = stage then commit → generate message from full HEAD→workdir diff.
            let final_msg = resolve_or_generate_commit_message(
                repo,
                message,
                /*include_workdir=*/ true,
                config,
                gate,
            )
            .await?;
            if final_msg.is_empty() {
                return Ok(vec![]);
            }
            Ok(vec![
                PipelineStep::Add { files: add_files },
                PipelineStep::Commit { message: final_msg },
            ])
        }

        ScopedWriteOp::RemoveRemote { name } => {
            let resp = gate.prompt(&GateType::SimpleConfirm {
                message: format!("Remove remote '{}'?", name),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::RemoveRemote { name }])
        }

        ScopedWriteOp::DeleteRepo => {
            delete_repo(repo, gate)?;
            Ok(vec![])  // .git is gone; pipeline executor won't run
        }

        ScopedWriteOp::Push { remote } => {
            let branch = current_branch_name(repo);
            if branch == "HEAD" {
                return Err(anyhow::anyhow!(
                    "No branch to push — repository has no commits or HEAD is detached."
                ));
            }
            // Decide whether to set upstream: only if the local branch has no upstream yet.
            let has_upstream = repo
                .find_branch(&branch, git2::BranchType::Local)
                .ok()
                .and_then(|b| b.upstream().ok())
                .is_some();
            let set_upstream = !has_upstream;

            let label = if set_upstream {
                format!("Publish '{}' to {} (set upstream tracking)?", branch, remote)
            } else {
                format!("Push '{}' to {}?", branch, remote)
            };
            let resp = gate.prompt(&GateType::SimpleConfirm { message: label })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(vec![]);
            }
            Ok(vec![PipelineStep::Push { remote, branch, set_upstream }])
        }

        ScopedWriteOp::Reset { mode } => {
            // Always preview what reset will affect — different details per mode.
            print_reset_preview(repo, mode);

            // Hard reset is the only mode that requires type-to-confirm.
            if mode == ResetMode::Hard {
                let branch = current_branch_name(repo);
                let op_for_explain = ScopedWriteOp::Reset { mode };
                let ctx_for_danger = crate::context::build_context(repo)
                    .unwrap_or_else(|_| crate::context::RepoContext::empty());
                let response = gate.prompt(&GateType::TypeToConfirm {
                    expected: branch.clone(),
                    message: format!("Type '{}' to confirm reset --hard:", branch),
                    explanation: Some(crate::explain::explain(&op_for_explain)),
                    danger: crate::danger::intent_danger(&op_for_explain, &ctx_for_danger),
                })?;
                if !matches!(response, GateResponse::Confirmed) {
                    println!("  Aborted.");
                    return Ok(vec![]);
                }
            }
            Ok(vec![PipelineStep::Reset { mode }])
        }
    }
}

// =============================================================================
// Feature pipeline handler (multi-step)
// =============================================================================

async fn handle_feature_pipeline(
    repo: &mut git2::Repository,
    op: FeaturePipelineOp,
    config: &Config,
    gate: &dyn ConfirmationGate,
    _auto_confirm: bool,
    show_all: bool,
) -> anyhow::Result<()> {
    match op {
        FeaturePipelineOp::AddCommitPush { feature_hint } => {
            add_commit_push_pipeline(repo, &feature_hint, config, gate, show_all).await
        }
        FeaturePipelineOp::CommitPush { message } => {
            commit_push_pipeline(repo, message, config, gate).await
        }
        FeaturePipelineOp::Sync { strategy } => {
            use crate::commands::SyncStrategy;
            let remote = config.remote.clone();
            let branch = current_branch_name(repo);

            // Confirm before pulling
            let resp = gate.prompt(&GateType::SimpleConfirm {
                message: format!("Sync: pull from {}/{} ({})?", remote, branch,
                    match strategy { SyncStrategy::Merge => "merge", SyncStrategy::Rebase => "rebase" }),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                println!("  Aborted.");
                return Ok(());
            }

            execute_pipeline(repo, vec![PipelineStep::Pull { remote }], config, gate).await
        }
    }
}

/// "Push X changes" pipeline: resolve files → add → commit (with message edit) → push.
async fn add_commit_push_pipeline(
    repo: &mut git2::Repository,
    feature_hint: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
    show_all: bool,
) -> anyhow::Result<()> {
    // Build manifest and resolve files
    let manifests = manifest::build_manifest(repo, 25)
        .context("Failed to build file manifest")?;

    if manifests.is_empty() {
        println!("  Nothing modified in the repository.");
        return Ok(());
    }

    let resolution = TwoPassResolver.resolve(&manifests, feature_hint, config).await?;

    let files = match resolution {
        ResolutionOutcome::Found(cr) => {
            let resp = gate.prompt(&GateType::FileMultiSelect {
                classification: cr,
                show_all,
            })?;
            match resp {
                GateResponse::FilesSelected(fs) => fs,
                GateResponse::Aborted => {
                    println!("  Aborted.");
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        ResolutionOutcome::NothingModified => {
            println!("  No modified files.");
            return Ok(());
        }
        ResolutionOutcome::NothingMatched { hint, suggestion } => {
            println!("  No files matched '{}'.", hint);
            if let Some(s) = suggestion {
                println!("  Hint: {}", s);
            }
            return Ok(());
        }
        ResolutionOutcome::TooManyFiles { count } => {
            println!("  {} modified files is too many. Narrow the hint.", count);
            return Ok(());
        }
        ResolutionOutcome::AmbiguousFeature { options } => {
            println!("  Ambiguous. Did you mean: {}", options.join(", "));
            return Ok(());
        }
    };

    if files.is_empty() {
        println!("  No files selected.");
        return Ok(());
    }

    // Generate a commit message from the actual diff (feature_hint becomes
    // a starting hint, but the LLM sees the real changes).
    let _draft_seed = format!("feat: {}", feature_hint);
    let final_msg = resolve_or_generate_commit_message(
        repo,
        CommitMessage::Draft(_draft_seed),
        /*include_workdir=*/ true,
        config,
        gate,
    )
    .await?;
    if final_msg.is_empty() {
        println!("  Aborted.");
        return Ok(());
    }

    let branch = current_branch_name(repo);
    let remote = config.remote.clone();

    let set_upstream = !branch_has_upstream(repo, &branch);
    let steps = vec![
        PipelineStep::Add { files },
        PipelineStep::Commit { message: final_msg },
        PipelineStep::Push {
            remote,
            branch,
            set_upstream,
        },
    ];

    execute_pipeline(repo, steps, config, gate).await
}

/// "Commit and push staged" pipeline.
async fn commit_push_pipeline(
    repo: &mut git2::Repository,
    message: CommitMessage,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    // commit_push only commits already-staged files → use index-vs-HEAD diff.
    let final_msg = resolve_or_generate_commit_message(
        repo,
        message,
        /*include_workdir=*/ false,
        config,
        gate,
    )
    .await?;
    if final_msg.is_empty() {
        println!("  Aborted.");
        return Ok(());
    }

    let branch = current_branch_name(repo);
    let remote = config.remote.clone();

    let set_upstream = !branch_has_upstream(repo, &branch);
    let steps = vec![
        PipelineStep::Commit { message: final_msg },
        PipelineStep::Push { remote, branch, set_upstream },
    ];

    execute_pipeline(repo, steps, config, gate).await
}

/// Handle the "bare push but nothing to push" case. Either:
///   - working tree clean → just tell the user (no plan, no snapshot, exit)
///   - working tree dirty → offer to convert into stage+commit+push, so the
///     user's intent ("push my changes") actually ships
async fn handle_nothing_to_push(
    repo: &mut git2::Repository,
    remote: String,
    has_uncommitted: bool,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let dim = |s: &str| -> String {
        if crate::danger::use_color() {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };

    if !has_uncommitted {
        println!();
        println!("  Nothing to push — your branch is up to date with the remote.");
        println!("  {}", dim("(no new local commits)"));
        return Ok(());
    }

    // Dirty workdir → the user almost certainly meant "ship my changes."
    // Offer the conversion right here so they don't have to remember the
    // add → commit → push sequence themselves.
    println!();
    println!("  Nothing committed yet — `push` alone won't ship your local edits.");
    println!("  {}", dim("(you have uncommitted changes; push needs a commit first)"));
    println!();

    let labels = vec![
        format!(
            "Stage all → commit → push     {}",
            dim("ship your edits in one go")
        ),
        format!(
            "Just commit (don't push)      {}",
            dim("stage + commit only")
        ),
        "Cancel".to_string(),
    ];

    let resp = gate.prompt(&crate::commands::GateType::Choose {
        message: "What did you want to do?".to_string(),
        options: labels,
    })?;

    let chosen = match resp {
        crate::commands::GateResponse::Chose(i) => i,
        _ => return Ok(()),
    };

    let steps = match chosen {
        0 => {
            // Stage all → commit → push. Use the empty Add{files:[]}
            // convention (= stage everything) and a "update" draft commit
            // message; the gate flow will let the user edit it.
            let branch = current_branch_name(repo);
            let set_upstream = !branch_has_upstream(repo, &branch);
            vec![
                PipelineStep::Add { files: vec![] },
                PipelineStep::Commit { message: "update".to_string() },
                PipelineStep::Push {
                    remote: remote.clone(),
                    branch,
                    set_upstream,
                },
            ]
        }
        1 => vec![
            PipelineStep::Add { files: vec![] },
            PipelineStep::Commit { message: "update".to_string() },
        ],
        _ => return Ok(()),
    };

    let fut = Box::pin(execute_pipeline(repo, steps, config, gate));
    fut.await
}

/// Render a class-aware recovery picker after a failed step. Each option is
/// one keystroke. Each option *does* the thing — no chained command recipes.
async fn handle_classified_recovery(
    repo: &mut git2::Repository,
    class: &crate::git_ops::FailureClass,
    failed_verb: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    use crate::git_ops::FailureClass as FC;

    let dim = |s: &str| -> String {
        if crate::danger::use_color() {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };

    println!();
    println!("  {}", dim(&format!("fix `{}` and continue:", failed_verb)));

    /// One picker option. `pipeline` is `None` for informational/cancel
    /// rows; `Some(...)` for fully composed pipelines we run end-to-end so
    /// the user never has to remember the next two steps themselves.
    struct RecoveryOption {
        label: String,
        pipeline: Option<Vec<PipelineStep>>,
        /// Inline informational handler — runs in place of a pipeline.
        info: Option<&'static str>,
    }

    let remote = config.remote.clone();
    let options: Vec<RecoveryOption> = match class {
        FC::DirtyTreeBlocksMerge { files } => {
            let pretty_files = match files.as_slice() {
                [] => "your edits".to_string(),
                [one] => one.clone(),
                many => format!("{} file(s)", many.len()),
            };
            vec![
                RecoveryOption {
                    label: format!(
                        "Stash → pull → restore     {}",
                        dim(&format!("stash {}, pull, then pop", pretty_files))
                    ),
                    pipeline: Some(vec![
                        PipelineStep::Stash,
                        PipelineStep::Pull { remote: remote.clone() },
                        PipelineStep::StashPop,
                    ]),
                    info: None,
                },
                RecoveryOption {
                    label: format!(
                        "Commit → pull              {}",
                        dim(&format!("commit {} then pull", pretty_files))
                    ),
                    pipeline: Some(vec![
                        PipelineStep::Add { files: vec![] },
                        PipelineStep::Commit { message: "update".to_string() },
                        PipelineStep::Pull { remote: remote.clone() },
                    ]),
                    info: None,
                },
                RecoveryOption {
                    label: "Cancel".to_string(),
                    pipeline: None,
                    info: None,
                },
            ]
        }
        FC::AuthFailed => vec![
            RecoveryOption {
                label: format!(
                    "Verify  {}",
                    dim("show what `git ls-remote` says about your auth")
                ),
                pipeline: None,
                info: Some("ls-remote"),
            },
            RecoveryOption {
                label: "Cancel".to_string(),
                pipeline: None,
                info: None,
            },
        ],
        FC::Network => vec![
            RecoveryOption {
                label: format!("Retry pull   {}", dim("try the same step again")),
                pipeline: Some(vec![PipelineStep::Pull { remote: remote.clone() }]),
                info: None,
            },
            RecoveryOption {
                label: "Cancel".to_string(),
                pipeline: None,
                info: None,
            },
        ],
        FC::LockHeld => vec![
            RecoveryOption {
                label: format!(
                    "Wait    {}",
                    dim("try again after the other git process finishes")
                ),
                pipeline: None,
                info: Some("wait"),
            },
            RecoveryOption {
                label: "Cancel".to_string(),
                pipeline: None,
                info: None,
            },
        ],
        FC::HookRejected { .. } => vec![
            RecoveryOption {
                label: format!("Show    {}", dim("re-print the hook output")),
                pipeline: None,
                info: Some("hook"),
            },
            RecoveryOption {
                label: "Cancel".to_string(),
                pipeline: None,
                info: None,
            },
        ],
        FC::NonFastForward => return Ok(()), // Handled by merge-preview path
        FC::Unknown(_) => vec![RecoveryOption {
            label: "Cancel".to_string(),
            pipeline: None,
            info: None,
        }],
    };

    let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
    let resp = gate.prompt(&crate::commands::GateType::Choose {
        message: "Pick one:".to_string(),
        options: labels,
    })?;

    let chosen_idx = match resp {
        crate::commands::GateResponse::Chose(i) => i,
        _ => return Ok(()),
    };
    if chosen_idx >= options.len() {
        return Ok(());
    }

    // Informational handler (no pipeline).
    if let Some(kind) = options[chosen_idx].info {
        match kind {
            "ls-remote" => {
                let output = std::process::Command::new("git")
                    .args(["ls-remote"])
                    .current_dir(repo.workdir().unwrap_or_else(|| std::path::Path::new(".")))
                    .output();
                match output {
                    Ok(o) if o.status.success() => {
                        println!("  ✓ `git ls-remote` works — your auth is fine.");
                        println!("    The earlier failure must be specific to that operation.");
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!("  ✗ `git ls-remote` failed:");
                        for line in stderr.lines().take(5) {
                            println!("    {}", line);
                        }
                    }
                    Err(e) => println!("  Couldn't run `git ls-remote`: {}", e),
                }
            }
            "wait" => {
                println!("  {}", dim("Retry the original command when ready."));
            }
            "hook" => {
                println!("  {}", dim("Hook output was printed above the failure block."));
            }
            _ => {}
        }
        return Ok(());
    }

    // Run the chained pipeline end-to-end.
    if let Some(steps) = options[chosen_idx].pipeline.clone() {
        // Box::pin to break async recursion (this fn → execute_pipeline →
        // handle_classified_recovery).
        let fut = Box::pin(execute_pipeline(repo, steps, config, gate));
        fut.await?;
    }

    Ok(())
}

// =============================================================================
// Core pipeline executor — validation + gate + snapshot + execute + post-check
// =============================================================================

async fn execute_pipeline(
    repo: &mut git2::Repository,
    steps: Vec<PipelineStep>,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let validator = DefaultValidator;

    // Pre-checks
    let check_results = validator.run_pre_checks(repo, &steps, &config.hooks)?;
    let warnings: Vec<String> = check_results
        .iter()
        .filter_map(|r| match r {
            PreCheckResult::Warn(w) => Some(w.clone()),
            _ => None,
        })
        .collect();

    let blocks: Vec<String> = check_results
        .iter()
        .filter_map(|r| match r {
            PreCheckResult::Block(b) => Some(b.clone()),
            _ => None,
        })
        .collect();

    if !blocks.is_empty() {
        // State-based blocking (detached HEAD, in-progress ops, …) now happens
        // in the situation gate before resolution, so anything reaching here is
        // an execution-layer pre-check (e.g. missing remote, empty commit).
        eprintln!("✗  Pre-check failed:");
        for b in &blocks {
            eprintln!("   · {}", b);
        }
        return Err(anyhow::anyhow!("Pre-checks blocked execution."));
    }

    // Catch the silent no-op-push case BEFORE snapshot/preview.
    //
    // Trigger: pipeline is a single bare Push and the local branch is not
    // ahead of upstream. Without this check the executor happily runs
    // `git push`, gets exit 0 (nothing to push), and reports success — but
    // the user's local changes (uncommitted or just no new commits) never
    // reach the remote. That's worse than failing because it looks fine.
    if steps.len() == 1 {
        if let PipelineStep::Push { remote, .. } = &steps[0] {
            let ctx = crate::context::build_context(repo)
                .unwrap_or_else(|_| crate::context::RepoContext::empty());
            if ctx.ahead == 0 {
                let has_uncommitted = ctx.has_uncommitted();
                return handle_nothing_to_push(repo, remote.clone(), has_uncommitted, config, gate)
                    .await;
            }
        }
    }

    // Capture snapshot BEFORE preview so the user sees the recovery point in
    // the header. The snapshot is on disk before any step runs.
    let recovery_mgr = FileRecoveryManager;
    let snapshot = recovery_mgr.capture_snapshot(repo)?;
    let sha7 = snapshot
        .head_oid
        .as_deref()
        .map(|s| &s[..s.len().min(7)])
        .unwrap_or("");

    // Show preview with the snapshot SHA in the header.
    let ctx_for_preview = crate::context::build_context(repo)
        .unwrap_or_else(|_| crate::context::RepoContext::empty());
    let preview = validator.build_preview(&steps, &warnings, &ctx_for_preview, sha7);
    println!("{}", preview);

    // Confirmation gate. Variant C from prototype.jsx specs Enter=run-all,
    // ↑↓=focus, e=edit, s=skip, q=cancel. First-pass implementation keeps the
    // SimpleConfirm gate; the keymap legend was printed inside the plan box.
    let proceed = gate.prompt(&GateType::SimpleConfirm {
        message: "Execute this plan? [Y/n]".to_string(),
    })?;
    if !matches!(proceed, GateResponse::Confirmed) {
        // The snapshot was captured before the gate — clean it up since
        // nothing changed.
        let _ = recovery_mgr.delete_snapshot(&snapshot);
        println!("  Aborted.");
        return Ok(());
    }

    let executor = DefaultExecutor {
        git_ops: Git2Operations,
        hooks_mode: config.hooks.clone(),
    };

    let mut last_error: Option<anyhow::Error> = None;
    let mut no_local_change = false;
    let mut failed_push: Option<(String, String)> = None; // (remote, branch) when a Push step fails
    // Classified network/merge failure captured during the step loop. When
    // set, the executor renders a class-aware picker instead of the generic
    // anyhow chain.
    let mut classified: Option<(crate::git_ops::FailureClass, String, String)> = None; // (class, raw_stderr, step_verb)
    let glit_debug = std::env::var("GLIT_DEBUG").is_ok();

    let use_color = crate::danger::use_color();
    let glyph_done = if use_color {
        crate::copy::TIMELINE_DONE
    } else {
        crate::copy::TIMELINE_DONE_ASCII
    };
    let glyph_fail = if use_color {
        crate::copy::TIMELINE_FAILED
    } else {
        crate::copy::TIMELINE_FAILED_ASCII
    };
    let dim = |s: &str| -> String {
        if use_color {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };
    let green = |s: &str| -> String {
        if use_color {
            format!("\x1b[32m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };
    let red = |s: &str| -> String {
        if use_color {
            format!("\x1b[31m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };

    // Track Commit successes for the Done footer ("N commit(s) on <branch>").
    let mut commits_made: usize = 0;
    let last_idx = steps.len().saturating_sub(1);

    for (i, step) in steps.iter().enumerate() {
        let pre_state = capture_pre_step_state(repo);
        let (verb, detail) = crate::validation::format_step_label(step);
        let level = crate::danger::step_danger(step, &ctx_for_preview);
        let marker = crate::danger::colored_marker(level);

        // "Running" placeholder — printed with carriage return so we can
        // overwrite it cleanly with the final state.
        print!("  {}  {:<10}{}", dim("●"), verb, detail);
        use std::io::Write;
        let _ = std::io::stdout().flush();

        match executor.execute(repo, step) {
            Ok(()) => {
                // Post-check
                let post_result = validator.run_post_check(repo, step, &pre_state);
                let (final_glyph, post_warning, post_failed) = match &post_result {
                    Ok(PostCheckResult::Verified) => (green(glyph_done), None, None),
                    Ok(PostCheckResult::Warning(w)) => (green(glyph_done), Some(w.clone()), None),
                    Ok(PostCheckResult::Failed(f)) => (red(glyph_fail), None, Some(f.clone())),
                    Err(e) => (green(glyph_done), Some(format!("post-check error: {}", e)), None),
                };

                // Rewrite the line with final glyph + marker at the end.
                let trailing = if marker.is_empty() {
                    format!("  {}", final_glyph)
                } else {
                    format!("  {}  {}", marker, final_glyph)
                };
                print!("\r  {}  {:<10}{}{}", final_glyph, verb, detail, trailing);
                println!();

                if let Some(w) = post_warning {
                    println!("  {}  {}", dim("│"), dim(&format!("⚠ {}", w)));
                }
                if let Some(f) = post_failed {
                    println!("  {}  {}", red("│"), red(&f));
                    last_error = Some(anyhow::anyhow!("Post-check failed: {}", f));
                    break;
                }
                if matches!(step, PipelineStep::Commit { .. }) {
                    commits_made += 1;
                }
                if let Some(line) = summarize_step(repo, step, &pre_state) {
                    // Indent under the step glyph; dim the secondary detail
                    // so it reads as supporting text, not another action.
                    println!("  {}  {}", dim("│"), dim(&line));
                }
                if i < last_idx {
                    println!("  {}", dim("│"));
                }
            }
            Err(e) => {
                // Rewrite the line in place with the failed glyph.
                print!(
                    "\r  {}  {:<10}{}  {}",
                    red(glyph_fail),
                    verb,
                    detail,
                    red(glyph_fail)
                );
                println!();

                // Did the operation return a typed `ClassifiedFailure`?
                // If so, capture it for the post-loop picker and suppress
                // the generic "Step failed:" / "Recovery: ..." prints —
                // the picker is the action surface.
                if let Some(failure) = e.downcast_ref::<crate::git_ops::ClassifiedFailure>() {
                    // One-line headline. Details live in the picker.
                    println!("  {}  {}", red("│"), failure.class.what());
                    if glit_debug {
                        eprintln!(
                            "  {}  {}",
                            dim("│"),
                            dim(&format!("(git: {})", failure.raw_stderr.trim().replace('\n', " · ")))
                        );
                    }
                    let (verb, _) = crate::validation::format_step_label(step);
                    classified = Some((
                        failure.class.clone(),
                        failure.raw_stderr.clone(),
                        verb.to_string(),
                    ));
                    // Non-fast-forward push → keep the existing merge-preview hand-off.
                    if matches!(failure.class, crate::git_ops::FailureClass::NonFastForward) {
                        if let PipelineStep::Push { remote, branch, .. } = step {
                            failed_push = Some((remote.clone(), branch.clone()));
                        }
                    }
                    let action = step.recovery_action(&snapshot);
                    no_local_change = matches!(
                        action,
                        crate::commands::RecoveryAction::NoLocalChange { .. }
                    ) && i == 0;
                    last_error = Some(e);
                    break;
                }

                // Untyped failure — fall back to the generic chain.
                eprintln!("  {}  {}", red("│"), e);
                if let PipelineStep::Push { remote, branch, .. } = step {
                    let msg = format!("{:?}", e);
                    let looks_non_ff = msg.contains("non-fast-forward")
                        || msg.contains("rejected")
                        || msg.contains("NonFastForward");
                    if looks_non_ff {
                        failed_push = Some((remote.clone(), branch.clone()));
                    }
                }
                let action = step.recovery_action(&snapshot);
                no_local_change = matches!(
                    action,
                    crate::commands::RecoveryAction::NoLocalChange { .. }
                ) && i == 0;
                if glit_debug {
                    eprintln!("  Recovery: {:?}", action);
                }
                last_error = Some(e);
                break;
            }
        }
    }

    // Delete snapshot on success, or when the first step failed without changing repo state.
    // Keep it on real failures (for `glit --recover`).
    if last_error.is_none() {
        let _ = recovery_mgr.delete_snapshot(&snapshot);
        println!();

        // Compose the Done line. Pull a fresh RepoContext so the ahead/behind
        // is post-execution (after a push, the ahead count drops to 0).
        let post_ctx = crate::context::build_context(repo)
            .unwrap_or_else(|_| crate::context::RepoContext::empty());
        let mut done_line = String::from("Done");
        if commits_made > 0 {
            done_line.push_str(&format!(
                " · {} commit{} on {}",
                commits_made,
                if commits_made == 1 { "" } else { "s" },
                post_ctx.branch
            ));
        } else if !post_ctx.branch.is_empty() && post_ctx.branch != "(no branch)" {
            done_line.push_str(&format!(" · on {}", post_ctx.branch));
        }
        if post_ctx.upstream.is_some() {
            done_line.push_str(&format!(
                ", ↑{} ↓{}",
                post_ctx.ahead, post_ctx.behind
            ));
        }
        println!("  {} {}", green("✓"), green(&done_line));
        println!("  {}", dim(crate::copy::PLAN_NEXT_HINT));
    } else if no_local_change || failed_push.is_some() {
        // Push that fails non-FF leaves the local repo in a fine state — the
        // earlier commit is intact, only the network step refused. No snapshot
        // recovery is needed.
        let _ = recovery_mgr.delete_snapshot(&snapshot);
    } else {
        eprintln!(
            "\n  Snapshot preserved at ~/.glit/snapshots/{}.json",
            snapshot.pipeline_id
        );
        eprintln!("  Run 'glit --recover' to restore to pre-pipeline state.");
    }

    // Class-aware recovery picker. Renders right at failure time so the
    // user has a one-keystroke path forward instead of a recipe to memorize.
    if let Some((class, _raw, verb)) = classified.clone() {
        // NonFastForward keeps using the merge-preview hand-off below.
        if !matches!(class, crate::git_ops::FailureClass::NonFastForward) {
            if let Err(e) = handle_classified_recovery(repo, &class, &verb, config, gate).await {
                eprintln!("  {}", e);
            }
            return Ok(());
        }
    }

    // When the failure was a non-fast-forward push, offer the same conflict-
    // resolution menu the user gets from `glit "safe to pull"`. The local
    // commit is already safe; the user just needs to decide how to reconcile.
    if let Some((remote, _branch)) = failed_push {
        eprintln!(
            "\nPush was rejected because the remote has commits you don't have."
        );
        eprintln!("Let's figure out how to reconcile…\n");
        // Recursive across async fn boundary — box the future so the
        // compiler doesn't need to figure out the indefinite size.
        let fut = Box::pin(handle_merge_preview(
            repo, &remote, config, gate, /*auto_confirm=*/ false,
        ));
        if let Err(e) = fut.await {
            eprintln!("  Could not run merge preview: {}", e);
        }
        // Swallow the push error so the user-visible exit is the picker.
        return Ok(());
    }

    match last_error {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

// =============================================================================
// Recovery
// =============================================================================

/// Execute recovery from a persisted snapshot.
pub fn execute_recovery() -> anyhow::Result<()> {
    let recovery_mgr = FileRecoveryManager;
    let snapshot = recovery_mgr
        .check_for_interrupted()?
        .ok_or_else(|| anyhow::anyhow!("No interrupted pipeline found in ~/.glit/snapshots/"))?;

    println!(
        "Found interrupted pipeline from {}.",
        snapshot.captured_at
    );
    println!("Restoring repository to pre-pipeline state...\n");

    let repo_path = find_repo_root()?;
    let repo = git2::Repository::open(&repo_path)?;
    recovery_mgr.execute_recovery(&repo, &snapshot)?;

    Ok(())
}

// =============================================================================
// Fallback interactive mode
// =============================================================================

/// Fallback to manual interactive mode when LLM is unavailable or fails.
pub fn fallback_interactive_intent() -> anyhow::Result<crate::commands::IntentResult> {
    use crate::commands::IntentResult;

    println!("  LLM unavailable. Entering manual mode.");
    println!("  What would you like to do?");

    let options = vec![
        "git init",
        "git remote add",
        "git remote remove",
        "delete repository (.git)",
        "git status",
        "git remote -v",
        "git branch --list",
        "git config (user)",
        "git add (stage files)",
        "git commit",
        "git push",
        "git pull",
        "git log",
        "git diff",
        "git branch (create)",
        "git checkout",
        "git stash / stash pop",
        "git reset",
    ];

    let selection = inquire::Select::new("Select operation:", options)
        .with_starting_cursor(0)
        .prompt()
        .context("Operation selection failed")?;

    let intent = match selection {
        "git init" => IntentType::ScopedWrite(ScopedWriteOp::Init),
        "git status" => IntentType::ReadOnly(ReadOnlyOp::Status),
        "git remote -v" => IntentType::ReadOnly(ReadOnlyOp::Remotes),
        "git remote add" => {
            let name = Text::new("Remote name (e.g. origin):").prompt()?;
            let url = Text::new("Remote URL:").prompt()?;
            IntentType::ScopedWrite(ScopedWriteOp::AddRemote { name, url })
        }
        "git remote remove" => {
            let name = Text::new("Remote name to remove:").prompt()?;
            IntentType::ScopedWrite(ScopedWriteOp::RemoveRemote { name })
        }
        "delete repository (.git)" => IntentType::ScopedWrite(ScopedWriteOp::DeleteRepo),
        "git branch --list" => IntentType::ReadOnly(ReadOnlyOp::Branches),
        "git config (user)" => IntentType::ReadOnly(ReadOnlyOp::Config),
        "git log" => IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }),
        "git diff" => IntentType::ReadOnly(ReadOnlyOp::Diff { path: None }),
        "git add (stage files)" => IntentType::ScopedWrite(ScopedWriteOp::Add {
            files: crate::commands::FileSelection::All,
        }),
        "git commit" => {
            let msg = Text::new("Commit message:").prompt()?;
            IntentType::ScopedWrite(ScopedWriteOp::Commit {
                message: CommitMessage::Provided(msg),
            })
        }
        "git push" => IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
            message: CommitMessage::Draft("update".to_string()),
        }),
        "git pull" => IntentType::ScopedWrite(ScopedWriteOp::Pull {
            remote: "origin".to_string(),
        }),
        "git branch (create)" => {
            let name = Text::new("Branch name:").prompt()?;
            IntentType::ScopedWrite(ScopedWriteOp::Branch { name })
        }
        "git checkout" => {
            let target = Text::new("Branch/commit to checkout:").prompt()?;
            IntentType::ScopedWrite(ScopedWriteOp::Checkout { target })
        }
        "git stash / stash pop" => {
            let opts = vec!["stash", "stash pop"];
            let choice = inquire::Select::new("Stash or stash pop?", opts)
                .with_starting_cursor(0)
                .prompt()?;
            if choice == "stash pop" {
                IntentType::ScopedWrite(ScopedWriteOp::StashPop)
            } else {
                IntentType::ScopedWrite(ScopedWriteOp::Stash)
            }
        }
        "git reset" => {
            let modes = vec!["soft", "mixed", "hard"];
            let mode_str = inquire::Select::new("Reset mode:", modes)
                .with_starting_cursor(0)
                .prompt()?;
            let mode = match mode_str {
                "hard" => ResetMode::Hard,
                "mixed" => ResetMode::Mixed,
                _ => ResetMode::Soft,
            };
            IntentType::ScopedWrite(ScopedWriteOp::Reset { mode })
        }
        _ => IntentType::ReadOnly(ReadOnlyOp::Status),
    };

    Ok(IntentResult {
        intent,
        confidence: 1.0,
        raw_query: "[manual]".to_string(),
        engine: crate::commands::Engine::Fast,
    })
}

// =============================================================================
// Helpers
// =============================================================================

/// Delete the .git directory — only allowed when the repo has zero commits.
fn delete_repo(repo: &git2::Repository, gate: &dyn ConfirmationGate) -> anyhow::Result<()> {
    // Block if any commits exist
    if repo.head().is_ok() {
        return Err(anyhow::anyhow!(
            "Repository has commits — deletion refused.\n  \
             Only empty repositories (no commits) can be deleted this way.\n  \
             To remove all history, delete the .git directory manually."
        ));
    }

    let git_dir = repo.path().to_path_buf();
    let display = git_dir.display().to_string();

    let op_for_explain = ScopedWriteOp::DeleteRepo;
    let ctx_for_danger = crate::context::build_context(repo)
        .unwrap_or_else(|_| crate::context::RepoContext::empty());
    let resp = gate.prompt(&GateType::TypeToConfirm {
        expected: "delete".to_string(),
        message: format!("Type 'delete' to permanently remove {}:", display),
        explanation: Some(crate::explain::explain(&op_for_explain)),
        danger: crate::danger::intent_danger(&op_for_explain, &ctx_for_danger),
    })?;

    if !matches!(resp, GateResponse::Confirmed) {
        println!("  Aborted.");
        return Ok(());
    }

    print!("  Removing {} … ", display);
    std::fs::remove_dir_all(&git_dir)
        .with_context(|| format!("Failed to remove {}", display))?;
    println!("✓");
    println!("\nRepository deleted. Directory is no longer tracked by git.");
    Ok(())
}

/// Initialize a new git repository in the current directory.
fn init_repo(gate: &dyn ConfirmationGate) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let resp = gate.prompt(&GateType::SimpleConfirm {
        message: format!("Initialize a new git repository in '{}'?", cwd.display()),
    })?;
    if !matches!(resp, GateResponse::Confirmed) {
        println!("  Aborted.");
        return Ok(());
    }
    print!("  [1] git init … ");
    git2::Repository::init(&cwd)
        .map_err(|e| anyhow::anyhow!("Failed to initialize repository: {}", e))?;
    println!("✓");
    println!("\nInitialized empty git repository in {}/.git/", cwd.display());
    Ok(())
}

/// Find the root of the current git repository by walking up the directory tree.
fn find_repo_root() -> anyhow::Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let repo = git2::Repository::discover(&cwd)
        .map_err(|_| anyhow::anyhow!("Not inside a git repository. Run 'git init' first."))?;
    Ok(repo.workdir().unwrap_or_else(|| repo.path()).to_path_buf())
}

/// Build the single Undo step (soft-reset HEAD~1), capturing the parent OID
/// up-front so the executor doesn't re-resolve a moving reference.
fn build_undo_step(
    repo: &git2::Repository,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    if repo.head_detached().unwrap_or(false) {
        return Err(anyhow::anyhow!(
            "Cannot undo in detached HEAD state. Checkout a branch first."
        ));
    }
    let head = repo
        .head()
        .map_err(|_| anyhow::anyhow!("No commits to undo — repository is empty."))?;
    let head_commit = head
        .peel_to_commit()
        .map_err(|_| anyhow::anyhow!("HEAD does not point to a commit."))?;

    let parent = match head_commit.parent(0) {
        Ok(p) => p,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Cannot undo: this is the first commit (no parent to reset to).\n  To discard it entirely, use: glit \"reset --hard\" with care."
            ));
        }
    };

    let undone_sha = head_commit.id().to_string()[..7].to_string();
    let undone_summary = head_commit.summary().unwrap_or("(no message)").to_string();

    let resp = gate.prompt(&GateType::SimpleConfirm {
        message: format!(
            "Undo commit {} \"{}\"? Files will return to staged.",
            undone_sha, undone_summary
        ),
    })?;
    if !matches!(resp, GateResponse::Confirmed) {
        println!("  Aborted.");
        return Ok(vec![]);
    }

    Ok(vec![PipelineStep::Undo {
        to_oid: parent.id().to_string(),
        undone_summary,
        undone_sha,
    }])
}

/// When the pre-check found we're in detached HEAD and the user's intent
/// required writes, offer a path forward instead of just erroring.
async fn handle_detached_head_rescue(
    repo: &mut git2::Repository,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let current_sha = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .map(|o| o.to_string()[..7].to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!();
    println!("⚠  You are in DETACHED HEAD state.");
    println!("   Currently at commit {} — not on any branch.", current_sha);
    println!("   Any commit you make here will be orphaned (lose-able) unless you save it.");
    println!();

    let resp = gate.prompt(&GateType::Choose {
        message: "What would you like to do?".to_string(),
        options: vec![
            "Create a branch HERE to save work".to_string(),
            "Switch back to your previous branch (no changes made)".to_string(),
            "Cancel".to_string(),
        ],
    })?;

    match resp {
        GateResponse::Chose(0) => {
            let name = Text::new("New branch name:")
                .with_help_message("This will save your current commit on a real branch")
                .prompt()
                .map_err(|e| anyhow::anyhow!("Aborted: {}", e))?;
            let name = name.trim().to_string();
            if let Err(r) = validate_branch_name(&name) {
                let suggestion = sanitize_branch_name(&name);
                println!("  '{}' isn't a valid branch name ({}). Using '{}'.", name, r, suggestion);
                let git_ops = Git2Operations;
                git_ops.create_branch(repo, &suggestion)?;
                git_ops.checkout(repo, &suggestion)?;
                println!("  ✓ Created and switched to branch '{}'", suggestion);
            } else {
                let git_ops = Git2Operations;
                git_ops.create_branch(repo, &name)?;
                git_ops.checkout(repo, &name)?;
                println!("  ✓ Created and switched to branch '{}'", name);
            }
            Ok(())
        }
        GateResponse::Chose(1) => {
            // Try to switch to a sensible default branch.
            for fallback in &["main", "master"] {
                let git_ops = Git2Operations;
                if git_ops.checkout(repo, fallback).is_ok() {
                    println!("  ✓ Switched to '{}'", fallback);
                    return Ok(());
                }
            }
            Err(anyhow::anyhow!(
                "Could not find 'main' or 'master' to switch back to. Manually: glit \"checkout <branch>\""
            ))
        }
        _ => {
            println!("  Aborted.");
            Ok(())
        }
    }
}

/// Lightweight branch-name validator. Rejects names that git refuses (spaces,
/// double dots, leading/trailing dashes/dots, invalid chars).
/// Returns Err(human reason) on invalid names.
fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".to_string());
    }
    if name.contains(' ') {
        return Err("contains a space (git refnames can't have spaces)".to_string());
    }
    if name.contains("..") {
        return Err("contains '..' (git refnames can't have two consecutive dots)".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("starts or ends with '-'".to_string());
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err("starts or ends with '.'".to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("starts or ends with '/'".to_string());
    }
    if name.contains("//") {
        return Err("contains '//' (consecutive slashes)".to_string());
    }
    if name.contains("@{") {
        return Err("contains '@{' (reserved sequence)".to_string());
    }
    for ch in name.chars() {
        if matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\x7f')
            || (ch as u32) < 0x20
        {
            return Err(format!("contains invalid character '{}'", ch));
        }
    }
    Ok(())
}

/// Best-effort normalization: convert spaces and other invalid characters
/// into hyphens, collapse repeated separators, trim leading/trailing punctuation.
fn sanitize_branch_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '_' | '/' | '.');
        if ok {
            out.push(ch);
        } else if matches!(ch, ' ' | '-' | '\t') {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
        // Drop everything else
    }
    // Collapse "//", "..", trim leading/trailing '-', '.', '/'.
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let trimmed = out.trim_matches(|c| matches!(c, '-' | '.' | '/')).to_string();
    if trimmed.is_empty() {
        "branch".to_string()
    } else {
        trimmed
    }
}

/// Return the file paths affected by the most recent stash (stash@{0}).
fn stash_top_file_list(repo: &git2::Repository) -> anyhow::Result<Vec<String>> {
    let stash_ref = repo
        .find_reference("refs/stash")
        .map_err(|e| anyhow::anyhow!("No stash ref: {}", e))?;
    let stash_commit = stash_ref.peel_to_commit()?;
    let parent = stash_commit.parent(0)?;
    let new_tree = stash_commit.tree()?;
    let old_tree = parent.tree()?;
    let diff = repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)?;
    let mut paths: Vec<String> = Vec::new();
    for d in diff.deltas() {
        if let Some(p) = d.new_file().path().or_else(|| d.old_file().path()) {
            paths.push(p.to_string_lossy().into_owned());
        }
    }
    Ok(paths)
}

/// Print a beginner-friendly preview of what a reset will affect, BEFORE
/// the (optional) type-to-confirm. Shows the HEAD commit summary + counts
/// of staged/modified files that will be impacted, scoped to the mode.
fn print_reset_preview(repo: &git2::Repository, mode: ResetMode) {
    let head_summary = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| {
            let sha = c.id().to_string()[..7].to_string();
            let summary = c.summary().unwrap_or("(no message)").to_string();
            format!("{} \"{}\"", sha, summary)
        });
    let staged_count = repo
        .statuses(None)
        .ok()
        .map(|s| {
            s.iter()
                .filter(|e| {
                    e.status().intersects(
                        git2::Status::INDEX_NEW
                            | git2::Status::INDEX_MODIFIED
                            | git2::Status::INDEX_DELETED
                            | git2::Status::INDEX_RENAMED
                            | git2::Status::INDEX_TYPECHANGE,
                    )
                })
                .count()
        })
        .unwrap_or(0);
    let modified_count = repo
        .statuses(None)
        .ok()
        .map(|s| {
            s.iter()
                .filter(|e| {
                    e.status().intersects(
                        git2::Status::WT_MODIFIED
                            | git2::Status::WT_DELETED
                            | git2::Status::WT_TYPECHANGE
                            | git2::Status::WT_RENAMED,
                    )
                })
                .count()
        })
        .unwrap_or(0);

    println!();
    match mode {
        ResetMode::Soft => {
            println!("  Reset --soft preview:");
            if let Some(ref s) = head_summary {
                println!("    Will move HEAD back from: {}", s);
            }
            println!("    Files: untouched (kept staged).");
        }
        ResetMode::Mixed => {
            println!("  Reset --mixed preview:");
            if let Some(ref s) = head_summary {
                println!("    Will move HEAD back from: {}", s);
            }
            println!(
                "    {} file(s) currently staged will be unstaged.",
                staged_count
            );
            println!("    Working tree contents: untouched.");
        }
        ResetMode::Hard => {
            println!("  Reset --hard preview:");
            if let Some(ref s) = head_summary {
                println!("    Will move HEAD back from: {}", s);
            }
            println!(
                "    {} staged file(s) and {} modified file(s) WILL BE DISCARDED.",
                staged_count, modified_count
            );
            println!("    Recovery: only via `git reflog` if you remember the old SHA.");
        }
    }
    println!();
}

/// Show a disambiguation menu when classifier confidence is mid-range.
/// Returns `Some(chosen_intent)` when the user picks an alternate, `None` to keep the top.
fn maybe_disambiguate(
    top: &IntentType,
    ctx: &crate::context::RepoContext,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Option<IntentType>> {
    let alts = crate::intent::alternates_for(top, ctx);
    if alts.is_empty() {
        return Ok(None);
    }

    let mut all: Vec<IntentType> = vec![top.clone()];
    all.extend(alts);

    let mut labels: Vec<String> = all
        .iter()
        .enumerate()
        .map(|(i, intent)| {
            let prefix = if i == 0 { "(top guess) " } else { "(maybe)     " };
            format!("{}{}", prefix, crate::intent::intent_label(intent))
        })
        .collect();
    labels.push("Cancel & rephrase".to_string());

    println!("\n? I'm not fully sure what you meant. Pick one:");
    let response = gate.prompt(&GateType::Choose {
        message: "Choice:".to_string(),
        options: labels,
    })?;

    match response {
        GateResponse::Chose(idx) if idx < all.len() => Ok(Some(all[idx].clone())),
        _ => Err(anyhow::anyhow!("Aborted.")),
    }
}

/// Produce a one-line "what changed" summary after a successful pipeline step.
/// Returns None when no meaningful before/after delta exists.
fn summarize_step(
    repo: &git2::Repository,
    step: &PipelineStep,
    pre: &crate::validation::PreStepState,
) -> Option<String> {
    use crate::commands::PipelineStep as P;
    match step {
        P::Add { files } => {
            let n = files.len();
            if n == 0 {
                Some("→ Staged all changes".to_string())
            } else if n == 1 {
                Some(format!("→ Staged {}", files[0]))
            } else {
                Some(format!("→ Staged {} files", n))
            }
        }
        P::Commit { message } => {
            let new_oid = repo
                .head()
                .ok()
                .and_then(|h| h.target())
                .map(|o| o.to_string());
            let pre_short = pre.head_oid.as_deref().map(|s| &s[..s.len().min(7)]);
            let new_short = new_oid.as_deref().map(|s| &s[..s.len().min(7)]);
            let first_line = message.lines().next().unwrap_or("").to_string();
            match (pre_short, new_short) {
                (Some(a), Some(b)) => Some(format!("→ Committed: {} ({}→{})", first_line, a, b)),
                (None, Some(b)) => Some(format!("→ First commit: {} ({})", first_line, b)),
                _ => Some(format!("→ Committed: {}", first_line)),
            }
        }
        P::Push { remote, branch, set_upstream } => {
            let suffix = if *set_upstream {
                " (upstream set)"
            } else {
                ""
            };
            let mut line = if pre.ahead > 0 {
                format!(
                    "→ Pushed {} commit(s) to {}/{}{}",
                    pre.ahead, remote, branch, suffix
                )
            } else {
                format!("→ Pushed to {}/{}{}", remote, branch, suffix)
            };
            // Append a clickable web URL when the remote is GitHub/GitLab/Bitbucket.
            if let Some(web) = repo
                .find_remote(remote)
                .ok()
                .and_then(|r| r.url().map(String::from))
                .as_deref()
                .and_then(crate::format::remote_to_web_url)
            {
                line.push_str(&format!("\n       → View: {}/commits/{}", web, branch));
                if *set_upstream {
                    line.push_str(&format!(
                        "\n       → Open PR: {}/compare/{}?expand=1",
                        web, branch
                    ));
                }
            }
            Some(line)
        }
        P::Pull { remote } => {
            if pre.behind > 0 {
                Some(format!("→ Pulled {} commit(s) from {}", pre.behind, remote))
            } else {
                Some(format!("→ Already up to date with {}", remote))
            }
        }
        P::Checkout { target } => Some(format!("→ Switched to {}", target)),
        P::Branch { name } => Some(format!("→ Created branch {}", name)),
        P::AddRemote { name, url } => Some(format!("→ Added remote {} → {}", name, url)),
        P::RemoveRemote { name } => Some(format!("→ Removed remote {}", name)),
        P::Stash => {
            // After stashing, refs/stash points at the new stash commit.
            // The first parent is the index state (= what was staged), and the
            // stash commit's own tree is the WORKDIR snapshot. We diff stash
            // commit vs its parent (the original HEAD) to get the file list.
            let files = stash_top_file_list(repo).unwrap_or_default();
            if files.is_empty() {
                Some("→ Stashed changes → stash@{0}".to_string())
            } else if files.len() <= 5 {
                Some(format!(
                    "→ Stashed {} file(s): {} → stash@{{0}}",
                    files.len(),
                    files.join(", ")
                ))
            } else {
                let preview: Vec<&str> = files.iter().take(4).map(|s| s.as_str()).collect();
                Some(format!(
                    "→ Stashed {} file(s): {}, … → stash@{{0}}",
                    files.len(),
                    preview.join(", ")
                ))
            }
        }
        P::StashPop => Some("→ Restored stashed changes (stash entry removed)".to_string()),
        P::Reset { mode } => Some(format!("→ Reset ({:?})", mode).to_lowercase()),
        P::Undo { undone_summary, undone_sha, .. } => Some(format!(
            "→ Undid commit {} \"{}\" (files re-staged)",
            undone_sha, undone_summary
        )),
        P::PullRebase { remote, branch } => {
            Some(format!("→ Rebased onto {}/{}", remote, branch))
        }
        P::ForcePush { remote, branch } => {
            Some(format!("→ Force-pushed to {}/{} (with-lease)", remote, branch))
        }
        P::DiscardLocal { remote, branch } => Some(format!(
            "→ Reset local to {}/{} — any unpushed work is gone (recoverable via reflog)",
            remote, branch
        )),
        P::Status | P::Log { .. } | P::Diff { .. } => None,
    }
}

/// Ensure git's user.name and user.email are set. Prompts to fill missing
/// values and writes them globally. No-op when both are already configured.
fn ensure_git_identity(
    repo: &git2::Repository,
    _gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let config = repo
        .config()
        .map_err(|e| anyhow::anyhow!("Failed to read git config: {}", e))?;
    let name = config.get_string("user.name").ok();
    let email = config.get_string("user.email").ok();
    if name.is_some() && email.is_some() {
        return Ok(());
    }

    println!("Git identity is not fully configured.");
    println!(
        "  user.name  = {}\n  user.email = {}",
        name.as_deref().unwrap_or("(not set)"),
        email.as_deref().unwrap_or("(not set)")
    );
    println!("These are recorded on every commit. Let's set them now.\n");

    let global_config_result = git2::Config::open_default();
    let mut global_config = match global_config_result {
        Ok(c) => c,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Could not open global git config: {}. \
                 Set manually: git config --global user.name \"...\" && git config --global user.email \"...\"",
                e
            ));
        }
    };

    if name.is_none() {
        let n = Text::new("Your name:")
            .with_help_message("Will be saved to ~/.gitconfig globally")
            .prompt()
            .map_err(|e| anyhow::anyhow!("Aborted: {}", e))?;
        let n = n.trim();
        if n.is_empty() {
            return Err(anyhow::anyhow!("Name cannot be empty."));
        }
        global_config
            .set_str("user.name", n)
            .map_err(|e| anyhow::anyhow!("Failed to write user.name: {}", e))?;
        println!("  ✓ user.name = {}", n);
    }
    if email.is_none() {
        let e = Text::new("Your email:")
            .with_help_message("Will be saved to ~/.gitconfig globally")
            .prompt()
            .map_err(|err| anyhow::anyhow!("Aborted: {}", err))?;
        let e = e.trim();
        if e.is_empty() {
            return Err(anyhow::anyhow!("Email cannot be empty."));
        }
        global_config
            .set_str("user.email", e)
            .map_err(|err| anyhow::anyhow!("Failed to write user.email: {}", err))?;
        println!("  ✓ user.email = {}", e);
    }
    println!();
    Ok(())
}

/// True if the given local branch has an upstream configured.
fn branch_has_upstream(repo: &git2::Repository, branch: &str) -> bool {
    repo.find_branch(branch, git2::BranchType::Local)
        .ok()
        .and_then(|b| b.upstream().ok())
        .is_some()
}

/// Get the current branch name, falling back to "HEAD".
fn current_branch_name(repo: &git2::Repository) -> String {
    repo.head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from))
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Resolve a CommitMessage (Draft/Provided) through the edit gate.
/// Kept for tests and callers that don't have repo/config in scope.
fn resolve_commit_message(
    message: CommitMessage,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<String> {
    let initial = match message {
        CommitMessage::Provided(m) => m,
        CommitMessage::Draft(m) => m,
    };

    let response = gate.prompt(&GateType::EditableText {
        initial: initial.clone(),
        label: "Commit message (edit or accept):".to_string(),
    })?;

    match response {
        GateResponse::TextProvided(msg) if !msg.trim().is_empty() => Ok(msg),
        GateResponse::Confirmed => Ok(initial),
        GateResponse::Aborted => Ok(String::new()),
        _ => Ok(initial),
    }
}

/// Async variant: when message is a Draft, generate a real conventional-commit
/// message from the actual diff (via the LLM) instead of using the query-derived
/// heuristic. Provided messages are passed through as-is to the edit gate.
async fn resolve_or_generate_commit_message(
    repo: &git2::Repository,
    message: CommitMessage,
    include_workdir: bool,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<String> {
    let initial = match message {
        CommitMessage::Provided(m) => m,
        CommitMessage::Draft(_) => {
            // Mid-merge, git has already prepared a MERGE_MSG — prefer it so the
            // merge commit reads conventionally rather than an LLM re-draft (S17).
            let merge_msg = if repo.state() == git2::RepositoryState::Merge {
                std::fs::read_to_string(repo.path().join("MERGE_MSG"))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            };
            match merge_msg {
                Some(m) => m,
                None => {
                    // Always regenerate from diff — the Draft heuristic produces
                    // query-echo garbage like "All my changes with appropriate message".
                    print!("  Drafting commit message from diff… ");
                    std::io::Write::flush(&mut std::io::stdout()).ok();
                    let msg =
                        crate::intent::generate_commit_message(repo, include_workdir, config).await;
                    println!("✓");
                    msg
                }
            }
        }
    };

    let response = gate.prompt(&GateType::EditableText {
        initial: initial.clone(),
        label: "Commit message (edit or accept):".to_string(),
    })?;

    match response {
        GateResponse::TextProvided(msg) if !msg.trim().is_empty() => Ok(msg),
        GateResponse::Confirmed => Ok(initial),
        GateResponse::Aborted => Ok(String::new()),
        _ => Ok(initial),
    }
}

fn step_name(step: &PipelineStep) -> &'static str {
    match step {
        PipelineStep::Add { .. } => "git add",
        PipelineStep::Commit { .. } => "git commit",
        PipelineStep::Push { .. } => "git push",
        PipelineStep::Pull { .. } => "git pull",
        PipelineStep::Branch { .. } => "git branch",
        PipelineStep::Checkout { .. } => "git checkout",
        PipelineStep::Stash => "git stash",
        PipelineStep::StashPop => "git stash pop",
        PipelineStep::Reset { .. } => "git reset",
        PipelineStep::Log { .. } => "git log",
        PipelineStep::Status => "git status",
        PipelineStep::Diff { .. } => "git diff",
        PipelineStep::AddRemote { .. } => "git remote add",
        PipelineStep::RemoveRemote { .. } => "git remote remove",
        PipelineStep::Undo { .. } => "git reset --soft HEAD~1",
        PipelineStep::PullRebase { .. } => "git pull --rebase",
        PipelineStep::ForcePush { .. } => "git push --force-with-lease",
        PipelineStep::DiscardLocal { .. } => "git reset --hard",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{ClassificationResult, GateResponse, GateType};
    use crate::confirm::MockGate;

    #[test]
    fn test_validate_branch_name_accepts_normal_names() {
        assert!(validate_branch_name("feat/login").is_ok());
        assert!(validate_branch_name("hotfix").is_ok());
        assert!(validate_branch_name("v1.2.3-rc").is_ok());
        assert!(validate_branch_name("user/feature.flag").is_ok());
    }

    #[test]
    fn test_validate_branch_name_rejects_invalid() {
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("fix login").is_err()); // space
        assert!(validate_branch_name("foo..bar").is_err()); // double dot
        assert!(validate_branch_name("-leading").is_err());
        assert!(validate_branch_name("trailing-").is_err());
        assert!(validate_branch_name(".leading").is_err());
        assert!(validate_branch_name("trailing.").is_err());
        assert!(validate_branch_name("foo:bar").is_err()); // invalid char
        assert!(validate_branch_name("foo~bar").is_err());
    }

    #[test]
    fn test_sanitize_branch_name_replaces_spaces() {
        assert_eq!(sanitize_branch_name("fix login"), "fix-login");
        assert_eq!(sanitize_branch_name("Big Feature  Name"), "Big-Feature-Name");
        assert_eq!(sanitize_branch_name("foo..bar"), "foo.bar");
        assert_eq!(sanitize_branch_name("-leading-"), "leading");
        assert_eq!(sanitize_branch_name("feat/login bug"), "feat/login-bug");
        // Empty / all-junk inputs become "branch".
        assert_eq!(sanitize_branch_name(""), "branch");
        assert_eq!(sanitize_branch_name("---"), "branch");
    }

    #[test]
    fn test_step_name_coverage() {
        assert_eq!(
            step_name(&PipelineStep::Add {
                files: vec![]
            }),
            "git add"
        );
        assert_eq!(
            step_name(&PipelineStep::Commit {
                message: "test".to_string()
            }),
            "git commit"
        );
        assert_eq!(
            step_name(&PipelineStep::Push {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                set_upstream: false,
            }),
            "git push"
        );
        assert_eq!(step_name(&PipelineStep::Stash), "git stash");
        assert_eq!(step_name(&PipelineStep::Status), "git status");
    }

    #[test]
    fn test_resolve_commit_message_provided() {
        let gate = MockGate {
            response: GateResponse::TextProvided("feat: my feature".to_string()),
        };
        let result =
            resolve_commit_message(CommitMessage::Provided("feat: my feature".to_string()), &gate)
                .unwrap();
        assert_eq!(result, "feat: my feature");
    }

    #[test]
    fn test_resolve_commit_message_aborted() {
        let gate = MockGate {
            response: GateResponse::Aborted,
        };
        let result =
            resolve_commit_message(CommitMessage::Draft("draft".to_string()), &gate).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_commit_message_confirmed_uses_initial() {
        let gate = MockGate {
            response: GateResponse::Confirmed,
        };
        let result =
            resolve_commit_message(CommitMessage::Draft("auto message".to_string()), &gate)
                .unwrap();
        assert_eq!(result, "auto message");
    }

    #[test]
    fn test_auto_confirm_gate_pipeline_selects_files() {
        let gate = AutoConfirmGate;
        let classification = ClassificationResult {
            related: vec![crate::commands::ClassifiedFile {
                path: "src/login.rs".to_string(),
                reason: "login form".to_string(),
            }],
            uncertain: vec![],
            unrelated: vec![],
        };
        let result = gate
            .prompt(&GateType::FileMultiSelect { classification, show_all: false })
            .unwrap();
        match result {
            GateResponse::FilesSelected(files) => {
                assert_eq!(files, vec!["src/login.rs".to_string()]);
            }
            _ => panic!("Expected FilesSelected"),
        }
    }
}
