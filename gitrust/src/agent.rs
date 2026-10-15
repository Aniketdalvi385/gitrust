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
use crate::intent::{ArgKind, IntentClassifier, KevClassifier};
use crate::manifest;
use crate::recovery::{FileRecoveryManager, RecoveryManager};
use crate::resolver::{FileResolver, TwoPassResolver};
use crate::validation::{capture_pre_step_state, DefaultValidator, PostCheckResult, Validator};

/// Print what GiTrust understood, before it acts: the engine glyph (⚡ exact
/// phrase · ✦ the local Kev model · ◇ you picked from a menu) and the
/// intent's name — "⚡  Show status", "✦  Show history · 0.6s". The model's
/// probability shows only under GITRUST_DEBUG. A refusal prints nothing here;
/// its own message says it.
fn print_engine_marker(result: &crate::commands::IntentResult, elapsed: std::time::Duration) {
    let use_color = crate::danger::use_color();
    let debug = std::env::var("GITRUST_DEBUG").is_ok();
    let Some(line) = crate::ux::engine_line(result, elapsed, !use_color, debug) else {
        return;
    };
    if !use_color {
        println!("{}", line);
        return;
    }
    let ansi = match result.engine {
        crate::commands::Engine::Fast => "\x1b[36m", // cyan
        crate::commands::Engine::Kev => "\x1b[34m",  // blue
        crate::commands::Engine::User => "\x1b[35m", // magenta
    };
    // glyph (colored) · title (plain) · suffix (dim)
    let (glyph, rest) = line.split_once("  ").unwrap_or((&line, ""));
    let (title, suffix) = match rest.find(" · ").or_else(|| rest.find("  (p=")) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    println!(
        "{}{}\x1b[0m  {}\x1b[2m{}\x1b[0m",
        ansi, glyph, title, suffix
    );
}

/// Snapshot the set of trash-net backup-dir names that exist right now. Used to
/// detect the dir a destructive step writes, so the success note reflects a real
/// backup instead of an unconditional claim (M-16 Defect 1). Best-effort: an
/// unreadable/absent trash root yields an empty set.
fn trash_dir_snapshot() -> std::collections::HashSet<std::path::PathBuf> {
    let root = match crate::trash::trash_dir() {
        Ok(r) => r,
        Err(_) => return std::collections::HashSet::new(),
    };
    match std::fs::read_dir(&root) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => std::collections::HashSet::new(),
    }
}

/// Return the display path of the single new trash dir created since `before`,
/// or `None` if the destructive step backed nothing up (empty destroy-set).
fn new_trash_dir_since(before: &std::collections::HashSet<std::path::PathBuf>) -> Option<String> {
    let root = crate::trash::trash_dir().ok()?;
    let entries = std::fs::read_dir(&root).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .find(|p| !before.contains(p))
        .map(|p| p.display().to_string())
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
        let started = std::time::Instant::now();
        let intent_result =
            classify_query(&KevClassifier, &empty_ctx, query, config, &*gate).await?;
        print_engine_marker(&intent_result, started.elapsed());
        return match intent_result.intent {
            IntentType::ScopedWrite(ScopedWriteOp::Init) => init_repo(&*gate),
            _ => Err(anyhow::anyhow!(
                "Not inside a git repository. Run 'gitrust \"initialize a repository\"' or 'git init' first."
            )),
        };
    }

    let repo_path = find_repo_root()?;
    let mut repo = git2::Repository::open(&repo_path)
        .map_err(|_| anyhow::anyhow!("Not a git repository: {}", repo_path.display()))?;

    // Check for interrupted pipeline from a previous crash. Scoped to THIS
    // repo's workdir (S56) so a crash in another worktree/repo can't prompt
    // recovery here.
    let recovery_mgr = FileRecoveryManager;
    let this_workdir = repo.workdir().map(|p| p.to_string_lossy().into_owned());
    if let Ok(Some(snapshot)) = recovery_mgr.check_for_interrupted(this_workdir.as_deref()) {
        println!(
            "⚠  An interrupted pipeline was found (captured at {}).",
            snapshot.captured_at
        );
        println!("   Run 'gitrust --recover' to restore the repository to its pre-pipeline state.");
        println!("   Or delete ~/.gitrust/snapshots/ to ignore it.\n");
    }

    // Prune expired trash-net backups (Phase 14). Best-effort — never fail the
    // run over housekeeping.
    let _ = crate::trash::prune_trash(config.trash_retention_days);

    // Pre-flight: make sure git identity is configured. First-time users
    // hit a deep-in-the-pipeline failure on first commit without this.
    ensure_git_identity(&repo, &*gate)?;

    // Layer 0: build repo context. Fail CLOSED — if we can't read repo state we
    // cannot safely gate writes, so abort rather than fall back to empty(), whose
    // NotARepo situation would make situation_gate allow every write (the
    // fail-open bug). build_context only errors when the status/index read itself
    // fails; empty, detached, and mid-operation repos all build successfully.
    let context = context::build_context(&repo)?;

    // Layer 1: classify intent — exact fast-path, else the local Kev model.
    let classify_started = std::time::Instant::now();
    let intent_result = classify_query(&KevClassifier, &context, query, config, &*gate).await?;
    print_engine_marker(&intent_result, classify_started.elapsed());

    // Layer 3 (early): the situation gate. An intent that is invalid for the
    // current RepoSituation is stopped here, BEFORE resolution — the single
    // source of state-based blocking (subsumes Phase 9's detached-HEAD
    // pre-check and the execute_pipeline rescue routing).
    match crate::validation::situation_gate(&context.situation, &intent_result.intent) {
        crate::validation::SituationVerdict::Allowed => {}
        crate::validation::SituationVerdict::Blocked { message } => {
            // Return the refusal as an Err so it exits non-zero (main() renders
            // `✗ {e}` and exits 1; the REPL prints and continues), matching the
            // execution-layer block in run_pre_checks. Returning Ok exited 0 and
            // let scripts read a refused, never-executed op as success.
            return Err(anyhow::anyhow!("{}", message));
        }
        // The gate — not agent.rs — drives the way out (Phase 12; subsumes the
        // old detached-HEAD special-case that used to live in the Blocked arm).
        crate::validation::SituationVerdict::Redirect(redirect) => {
            use crate::validation::Redirect;
            match redirect {
                Redirect::DetachedHeadRescue => {
                    // After a branch-here rescue the repo is on a real branch and
                    // the situation is now Normal, so the ORIGINAL intent must be
                    // resumed — a user who typed "commit this" should end up with
                    // the commit made, not silently dropped on the new branch
                    // (M-13). Any other rescue outcome (switched away / cancelled)
                    // ends the run here.
                    match handle_detached_head_rescue(&mut repo, &*gate).await? {
                        RescueOutcome::Resume => {
                            // fall through to normal intent dispatch below
                        }
                        RescueOutcome::Done => return Ok(()),
                    }
                }
                Redirect::ReconcileDiverged { ahead, behind } => {
                    return reconcile_flow(
                        &mut repo,
                        ReconcileMode::SameBranch,
                        ahead,
                        behind,
                        config,
                        &*gate,
                    )
                    .await;
                }
            }
        }
    }

    // Dispatch based on intent. Keep the raw query for the few builders that need
    // it (e.g. UndoIntent's multi-commit count detection — parsing, not deciding).
    let raw_query = intent_result.raw_query.clone();
    match intent_result.intent {
        // MergePreview needs &mut repo so it can offer a follow-up resolution action.
        IntentType::ReadOnly(ReadOnlyOp::MergePreview { remote }) => {
            handle_merge_preview(&mut repo, &remote, config, &*gate, auto_confirm).await
        }
        IntentType::ReadOnly(op) => handle_read_only(&repo, op, &context),
        IntentType::ScopedWrite(op) => {
            handle_scoped_write(
                &mut repo,
                op,
                &raw_query,
                config,
                &*gate,
                auto_confirm,
                show_all,
            )
            .await
        }
        IntentType::FeaturePipeline(op) => {
            handle_feature_pipeline(&mut repo, op, config, &*gate, auto_confirm, show_all).await
        }
        IntentType::NeedsClarification(question) => unclear_request(&question),
    }
}

/// A request GiTrust won't act on (e.g. Kev is confident it isn't about Git).
/// Nothing was done, so it is an `Err`: main() renders `✗ …` and exits 1 (the
/// REPL prints and continues) — a refusal must never read as success.
fn unclear_request(question: &str) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("{}", question))
}

/// Layer 1: turn the query into an intent.
///
/// 1. ⚡ exact fast-path (`try_canonical`) — no model at all.
/// 2. ✦ Kev ranks every known op; `kev::decide` (deterministic) then:
///    * routes when the top op clears its confidence bar (a higher bar for
///      destructive ops, which still meet their own gates downstream);
///    * rejects a confident "not a Git request";
///    * otherwise asks the user to pick from Kev's top candidates — a
///      `ChoiceGate`, so `--yes` refuses instead of guessing.
/// 3. Arguments are captured from the query by `build_from_op`; a missing one
///    is asked for, never invented.
///
/// If Kev is unreachable, falls back to the manual operation menu.
async fn classify_query(
    classifier: &impl IntentClassifier,
    context: &crate::context::RepoContext,
    query: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<crate::commands::IntentResult> {
    use crate::commands::{Engine, IntentResult};
    use crate::kev::KevDecision;

    if let Some(intent) = crate::intent::try_canonical(query) {
        if std::env::var("GITRUST_DEBUG").is_ok() {
            eprintln!("  (fast-path: {:?})", intent);
        }
        return Ok(IntentResult {
            intent,
            confidence: 1.0,
            raw_query: query.to_string(),
            engine: Engine::Fast,
        });
    }

    let state = crate::intent::kev_state(context, query);

    // Mid-operation, "which files are conflicting?" is answered by status (it
    // lists them, plus the way forward/back) — deterministic, no model call.
    if crate::intent::in_progress_family(context).is_some()
        && crate::intent::is_conflict_question(query)
    {
        return Ok(IntentResult {
            intent: IntentType::ReadOnly(ReadOnlyOp::Status),
            confidence: 1.0,
            raw_query: query.to_string(),
            engine: Engine::Fast,
        });
    }

    // Mid-operation: first ask whether the request controls that operation
    // (continue / skip / abort). A confident control op routes (skip/abort need
    // the destructive bar); an unsure one whose best guess is still a control
    // op shows the family's menu; anything else falls through to the main
    // question. The situation gate still validates whatever comes out.
    if let Some(family) = crate::intent::in_progress_family(context) {
        if let Some((instr, opts)) = crate::kev::in_progress_question(family) {
            if let Ok(r) = classifier.rank(state.clone(), instr, &opts, config).await {
                if std::env::var("GITRUST_DEBUG").is_ok() {
                    let top: Vec<String> =
                        r.iter().map(|(o, p)| format!("{}={:.2}", o, p)).collect();
                    eprintln!("  (kev in-progress: {})", top.join(" "));
                }
                let mut d = crate::kev::decide(
                    &r,
                    config.kev_min_confidence,
                    config.kev_destructive_min_confidence,
                    config.kev_reject_min_confidence,
                );
                // skip / abort discard work: only act if the pick survives reordering
                // (an unstable one falls to the family menu below).
                if let KevDecision::Route { op, .. } = &d {
                    if crate::kev::DESTRUCTIVE_OPS.contains(&op.as_str())
                        && reversed_top(classifier, &state, instr, &opts, config)
                            .await
                            .as_ref()
                            != Some(op)
                    {
                        d = KevDecision::Ask { candidates: vec![] };
                    }
                }
                let decided = match d {
                    KevDecision::Route { op, p } => Some((op, p, Engine::Kev)),
                    _ if r
                        .first()
                        .and_then(|(o, _)| crate::intent::build_in_progress(o))
                        .is_some() =>
                    {
                        let other = opts.last().map(|(o, _)| o.clone()).unwrap_or_default();
                        // Menu over the family's ops in the model's order, "something
                        // else" last (it falls through to the main question).
                        let mut menu: Vec<(String, f32)> =
                            r.iter().filter(|(o, _)| *o != other).cloned().collect();
                        menu.push((other.clone(), 0.0));
                        let picked = pick_in_progress(&menu, &other, gate)?;
                        picked.map(|op| (op, 1.0, Engine::User))
                    }
                    _ => None,
                };
                if let Some((op, p, engine)) = decided {
                    if let Some(intent) = crate::intent::build_in_progress(&op) {
                        return Ok(IntentResult {
                            intent,
                            confidence: p,
                            raw_query: query.to_string(),
                            engine,
                        });
                    }
                }
            }
        }
    }

    let ranked = match classifier
        .rank(
            state.clone(),
            crate::kev::instructions(),
            crate::kev::ops(),
            config,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("⚠  {}", e);
            return manual_menu(context, query, gate);
        }
    };
    if std::env::var("GITRUST_DEBUG").is_ok() {
        let top: Vec<String> = ranked
            .iter()
            .take(4)
            .map(|(o, p)| format!("{}={:.2}", o, p))
            .collect();
        eprintln!("  (kev: {})", top.join(" "));
    }

    // When "not a Git request" was Kev's best (but unconfident) guess, say so —
    // the menu is then a "did you mean one of these?" escape hatch, not a claim
    // that the request was about Git.
    let mut question = if ranked.first().map(|(o, _)| o == crate::kev::REJECT) == Some(true) {
        crate::copy::KEV_MAYBE_NOT_GIT_QUESTION
    } else {
        crate::copy::KEV_UNSURE_QUESTION
    };
    let mut decision = crate::kev::decide(
        &ranked,
        config.kev_min_confidence,
        config.kev_destructive_min_confidence,
        config.kev_reject_min_confidence,
    );
    // Second opinions on a route — both deterministic, both can only turn "act"
    // into "ask", never pick a different op on the user's behalf:
    //  * the request's own words contradict the op (a named topic, plain push);
    //  * a destructive op that changes when the options are reordered.
    if let KevDecision::Route { op, p } = &decision {
        let p_of = |o: &str| ranked.iter().find(|(x, _)| x == o).map_or(0.0, |(_, p)| *p);
        if let Some(alt) = crate::intent::conflicting_op(op, query) {
            decision = KevDecision::Ask {
                candidates: vec![(alt.to_string(), p_of(alt)), (op.clone(), *p)],
            };
            question = match alt {
                "commit_push" => crate::copy::KEV_COMMIT_PUSH_CONFLICT_QUESTION,
                "add_commit" => crate::copy::KEV_NO_PUSH_CONFLICT_QUESTION,
                _ => crate::copy::KEV_CONFLICT_QUESTION,
            };
        } else if crate::kev::DESTRUCTIVE_OPS.contains(&op.as_str()) {
            let opts = crate::kev::ops();
            let second =
                reversed_top(classifier, &state, crate::kev::instructions(), opts, config).await;
            if second.as_ref() != Some(op) {
                let mut candidates: Vec<(String, f32)> = ranked
                    .iter()
                    .filter(|(o, _)| o != crate::kev::REJECT)
                    .take(3)
                    .cloned()
                    .collect();
                if let Some(s) = second
                    .filter(|s| s != crate::kev::REJECT && !candidates.iter().any(|(o, _)| o == s))
                {
                    let ps = p_of(&s);
                    candidates.insert(1, (s, ps));
                    candidates.truncate(3);
                }
                decision = KevDecision::Ask { candidates };
                question = crate::copy::KEV_UNSTABLE_QUESTION;
            }
        }
    }

    let (op, confidence, engine) = match decision {
        KevDecision::Route { op, p } => (op, p, Engine::Kev),
        KevDecision::Reject { p } => {
            return Ok(IntentResult {
                intent: IntentType::NeedsClarification(crate::copy::NOT_A_GIT_REQUEST.to_string()),
                confidence: p,
                raw_query: query.to_string(),
                engine: Engine::Kev,
            });
        }
        KevDecision::Ask { candidates } => {
            let op = pick_candidate(&candidates, question, gate)?;
            (op, 1.0, Engine::User)
        }
    };

    let mut ask = |kind: ArgKind| ask_argument(kind, gate);
    let intent = crate::intent::build_from_op(&op, query, context, &mut ask)?;
    Ok(IntentResult {
        intent,
        confidence,
        raw_query: query.to_string(),
        engine,
    })
}

/// Kev's top op when the same question is asked with the options reversed
/// (`kev::reversed_keep_last`). `None` if that second call fails — the caller
/// then treats the route as unstable and asks: a missing second opinion must
/// never become a silent destructive action.
async fn reversed_top(
    classifier: &impl IntentClassifier,
    state: &serde_json::Value,
    instructions: &str,
    opts: &[(String, String)],
    config: &Config,
) -> Option<String> {
    let reversed = crate::kev::reversed_keep_last(opts);
    let r = classifier
        .rank(state.clone(), instructions, &reversed, config)
        .await
        .ok()?;
    if std::env::var("GITRUST_DEBUG").is_ok() {
        let top: Vec<String> = r
            .iter()
            .take(3)
            .map(|(o, p)| format!("{}={:.2}", o, p))
            .collect();
        eprintln!("  (kev, options reversed: {})", top.join(" "));
    }
    r.into_iter().next().map(|(o, _)| o)
}

/// Kev wasn't sure: show its top candidates, each with what it would do, and
/// let the user choose. A `ChoiceGate`, so `--yes` refuses — it never picks an
/// operation the model itself wasn't confident about.
fn pick_candidate(
    candidates: &[(String, f32)],
    question: &str,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<String> {
    if candidates.is_empty() {
        anyhow::bail!("{}", crate::copy::NOT_A_GIT_REQUEST);
    }
    // Rows speak the user's language: a title + a short hint (copy::op_label),
    // never the model's option name or probability (that's GITRUST_DEBUG only).
    // Ops that can rewrite or discard work carry the ⚠ marker. Backing out is
    // Esc — not a "none of these" row (design handoff: never a list item).
    let debug = std::env::var("GITRUST_DEBUG").is_ok();
    let options: Vec<crate::commands::ChoiceOption> = candidates
        .iter()
        .map(|(op, p)| {
            let (title, hint) = crate::copy::op_label(op).unwrap_or((op.as_str(), ""));
            let mut label = title.to_string();
            if crate::kev::DESTRUCTIVE_OPS.contains(&op.as_str()) {
                label.push_str("  ");
                label.push_str(&crate::danger::colored_marker(
                    crate::danger::DangerLevel::Caution,
                ));
            }
            if debug {
                label.push_str(&format!("  ({} {:.0}%)", op, p * 100.0));
            }
            crate::commands::ChoiceOption {
                label,
                consequence: hint.to_string(),
            }
        })
        .collect();
    match gate.prompt(&GateType::ChoiceGate {
        question: question.to_string(),
        options,
    })? {
        GateResponse::Chose(i) if i < candidates.len() => Ok(candidates[i].0.clone()),
        _ => crate::ux::canceled(),
    }
}

/// The in-progress menu (continue / skip / abort / something else). Returns
/// `None` for "something else" so the caller falls through to the main
/// question. A `ChoiceGate`: `--yes` refuses rather than continue or abort an
/// operation on the model's unsure guess.
fn pick_in_progress(
    menu: &[(String, f32)],
    other: &str,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Option<String>> {
    let options: Vec<crate::commands::ChoiceOption> = menu
        .iter()
        .map(|(op, _)| {
            let (title, hint) = crate::copy::op_label(op).unwrap_or((op.as_str(), ""));
            crate::commands::ChoiceOption {
                label: title.to_string(),
                consequence: hint.to_string(),
            }
        })
        .collect();
    match gate.prompt(&GateType::ChoiceGate {
        question: crate::copy::KEV_IN_PROGRESS_QUESTION.to_string(),
        options,
    })? {
        GateResponse::Chose(i) if i < menu.len() => {
            Ok((menu[i].0 != other).then(|| menu[i].0.clone()))
        }
        _ => crate::ux::canceled(),
    }
}

/// Ask the user for an argument the query didn't contain. `--yes` answers an
/// EditableText with its (empty) initial text, which `build_from_op` rejects —
/// so a scripted run fails loudly instead of inventing a branch name or count.
fn ask_argument(kind: ArgKind, gate: &dyn ConfirmationGate) -> anyhow::Result<String> {
    match gate.prompt(&GateType::EditableText {
        initial: String::new(),
        label: kind.prompt().to_string(),
    })? {
        GateResponse::TextProvided(t) => Ok(t),
        _ => Ok(String::new()),
    }
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
            // Stopped mid-operation: say so first, with the way forward and back.
            if let Some(lines) = ctx.in_progress_lines() {
                for l in lines {
                    println!("{}", l);
                }
            }
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
                println!("\n(Remote state from last fetch — run `gitrust \"fetch\"` to refresh.)");
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
                println!(
                    "  → Try: gitrust \"log\" for history, or gitrust \"show\" for full commit details."
                );
            } else {
                println!("{}", snippet);
            }
        }
        ReadOnlyOp::Remotes => {
            let remotes = git_ops.list_remotes(repo)?;
            if remotes.is_empty() {
                println!("No remotes configured.");
                println!("  → Add one: gitrust \"add remote origin <url>\"");
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
                println!(
                    "No remote '{}' configured. Try: gitrust \"add remote origin <url>\"",
                    remote
                );
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
                println!(
                    "\n→ {} commit(s) on remote you don't have. Run: gitrust \"pull\"",
                    fresh.behind
                );
            } else if fresh.ahead > 0 {
                println!(
                    "\n→ {} unpushed commit(s). Run: gitrust \"push\"",
                    fresh.ahead
                );
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
            println!("  → To share: gitrust \"push\"");
        }
        P::FastForward { remote_commits } => {
            println!(
                "✓ Pull will fast-forward {} commit(s). No conflicts possible.",
                remote_commits
            );
            print_dirty_overlap_block(dirty, /*conflicts_present=*/ false);
            if dirty.overlapping_files.is_empty() {
                println!("  → Safe to: gitrust \"pull\"");
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
                println!("  → Safe to: gitrust \"pull\"");
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
            println!("  → Set one: gitrust \"publish\"");
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
    println!("    Try: gitrust \"commit all my files\" — or — gitrust \"stash\"");
}

/// Print a `git show`-style block for a single commit.
fn print_commit_details(d: &crate::git_ops::CommitDetails, requested_ref: &str) {
    let head_marker = if requested_ref == "HEAD" {
        " (HEAD)"
    } else {
        ""
    };
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
        let marker = if current.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };

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
    let dirty = git_ops
        .dirty_overlap_for_pull(repo, remote)
        .unwrap_or_default();
    print_merge_preview(&preview, &dirty);

    // Open the picker in two cases:
    //   1. Real commit-vs-commit conflicts (Conflicts variant).
    //   2. Fast-forward / clean-merge BUT dirty workdir overlaps with the
    //      files the merge will touch — git will refuse with "would be
    //      overwritten by merge". The user must commit or stash first.
    let has_dirty = !dirty.overlapping_files.is_empty();
    let (local_commits, _remote_commits) = match (&preview, has_dirty) {
        (
            crate::git_ops::MergePreview::Conflicts {
                local_commits,
                remote_commits,
                ..
            },
            _,
        ) => (*local_commits, *remote_commits),
        (crate::git_ops::MergePreview::FastForward { remote_commits }, true) => {
            (0usize, *remote_commits)
        }
        (
            crate::git_ops::MergePreview::CleanMerge {
                local_commits,
                remote_commits,
            },
            true,
        ) => (*local_commits, *remote_commits),
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
    let dest_marker = crate::danger::colorize(crate::danger::DangerLevel::Destructive, "⚠⚠");

    if has_dirty {
        labels.push(format!(
            "Merge      {}",
            dim("gitrust \"commit all\" → gitrust \"pull\"  (commit your edits first)")
        ));
        actions.push(ScopedWriteOp::AddAndCommit {
            message: crate::commands::CommitMessage::Draft("update".to_string()),
            files: crate::commands::FileSelection::All,
        });
        labels.push(format!(
            "Stash      {}",
            dim("gitrust \"stash\" → gitrust \"pull\" → gitrust \"stash pop\"")
        ));
        actions.push(ScopedWriteOp::Stash);
    } else {
        labels.push(format!(
            "Merge      {}",
            dim("gitrust \"pull\"          merge commit · resolve markers, then commit")
        ));
        actions.push(ScopedWriteOp::Pull {
            remote: remote.to_string(),
        });
        labels.push(format!(
            "Rebase     {}",
            dim(&format!(
                "gitrust \"rebase\"        replay your {} commit(s) · resolve per commit",
                local_commits
            ))
        ));
        actions.push(ScopedWriteOp::PullRebase {
            remote: remote.to_string(),
        });
        labels.push(format!(
            "Force-push {}  {}",
            dim("gitrust \"force push\""),
            format!(
                "{}  {}",
                dest_marker,
                dim("rewrites remote · gitrust recover restores for 30d")
            )
        ));
        actions.push(ScopedWriteOp::ForcePush {
            remote: remote.to_string(),
        });
        labels.push(format!(
            "Discard    {}  {}",
            dim("gitrust \"discard local\""),
            format!(
                "{}  {}",
                dest_marker,
                dim(&format!(
                    "drops {} commit(s) · gitrust recover restores for 30d",
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
            return crate::ux::canceled();
        }
    };

    if chosen_idx >= actions.len() {
        // "Do nothing" or out-of-range — just exit.
        return Ok(());
    }
    let op = actions.remove(chosen_idx);
    // These menu actions are concrete non-undo ops, so no raw query is needed
    // for count parsing.
    handle_scoped_write(repo, op, "", config, gate, auto_confirm, false).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_scoped_write(
    repo: &mut git2::Repository,
    op: ScopedWriteOp,
    raw_query: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
    _auto_confirm: bool,
    show_all: bool,
) -> anyhow::Result<()> {
    // Forensic recovery restores imperatively (a rescue branch, no snapshot, no
    // pipeline) — it can't be expressed as `Vec<PipelineStep>`, so handle it here.
    if matches!(op, ScopedWriteOp::ForensicRecover) {
        return handle_forensic_recover(repo, config, gate);
    }

    let steps = build_scoped_steps(repo, op, raw_query, config, gate, show_all).await?;
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
    raw_query: &str,
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
                    vec![] // empty = add_all in git_ops
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
                            println!(
                                "  Ambiguous feature. Did you mean one of: {}",
                                options.join(", ")
                            );
                            return Ok(vec![]);
                        }
                    }
                }
            };
            Ok(vec![PipelineStep::Add { files: resolved }])
        }

        ScopedWriteOp::Commit { message } => {
            // Say "nothing to commit" BEFORE asking for a message. A merge
            // commit is the exception: it may legitimately change nothing.
            if repo.state() != git2::RepositoryState::Merge && staged_change_count(repo) == 0 {
                return Err(anyhow::anyhow!(
                    "{}",
                    if has_uncommitted_changes(repo) {
                        crate::copy::NOTHING_STAGED
                    } else {
                        crate::copy::NOTHING_TO_COMMIT
                    }
                ));
            }
            // Plain commit = only staged → generate message from index-vs-HEAD diff.
            let final_msg = resolve_or_generate_commit_message(
                repo, message, /*include_workdir=*/ false, config, gate,
            )
            .await?;
            if final_msg.is_empty() {
                return crate::ux::canceled();
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
                Err(_) => {
                    // Flow 05: one line naming what this input broke, then the
                    // name prompt pre-filled with a fixed-up suggestion — Enter
                    // accepts it, editing changes it, Esc cancels.
                    println!("  {}", crate::copy::branch_invalid_intro(&name));
                    let mut proposal = sanitize_branch_name(&name);
                    let mut accepted = None;
                    for _ in 0..3 {
                        let resp = gate.prompt(&GateType::EditableText {
                            initial: proposal.clone(),
                            label: crate::copy::BRANCH_NAME_SUGGESTED.to_string(),
                        })?;
                        let typed = match resp {
                            GateResponse::TextProvided(t) => t.trim().to_string(),
                            _ => return crate::ux::canceled(),
                        };
                        match validate_branch_name(&typed) {
                            Ok(()) => {
                                accepted = Some(typed);
                                break;
                            }
                            Err(_) => {
                                println!("  {}", crate::copy::branch_invalid_intro(&typed));
                                proposal = sanitize_branch_name(&typed);
                            }
                        }
                    }
                    match accepted {
                        Some(n) => n,
                        None => {
                            return Err(anyhow::anyhow!(
                                "No valid branch name was given — nothing was created."
                            ))
                        }
                    }
                }
            };
            Ok(vec![PipelineStep::Branch { name: final_name }])
        }
        ScopedWriteOp::Checkout { target } => {
            if target.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "No target branch specified. Try: gitrust \"checkout <branch-name>\""
                ));
            }
            Ok(vec![PipelineStep::Checkout { target }])
        }
        ScopedWriteOp::Stash => Ok(vec![PipelineStep::Stash]),
        ScopedWriteOp::StashPop => Ok(vec![PipelineStep::StashPop]),
        ScopedWriteOp::Pull { remote } => Ok(vec![PipelineStep::Pull { remote }]),

        // P6: UndoIntent is the classifier's only undo target. The deterministic
        // ChoiceGate resolves the semantic. `Undo` (soft-reset) is never a
        // classifier target now, but keep the arm as an internal safety-net that
        // routes through the same gate (should never be hit).
        ScopedWriteOp::UndoIntent | ScopedWriteOp::Undo => build_undo_intent(repo, raw_query, gate),

        // S62: destructive-cleanup language terminates at a deterministic
        // ChoiceGate that spells out what would be deleted before any Clean /
        // Reset::Hard is constructed. Never the LLM.
        ScopedWriteOp::CleanupIntent => build_cleanup_intent(repo, gate),

        // Forensic recovery is handled imperatively in `handle_scoped_write`
        // (it restores to a rescue branch, not via a pipeline). Should not reach
        // here, but return no steps if it does.
        ScopedWriteOp::ForensicRecover => Ok(vec![]),

        ScopedWriteOp::Amend {
            message,
            include_staged,
        } => build_amend_step(repo, message, include_staged, config, gate).await,

        ScopedWriteOp::Revert { oid } => build_revert_step(repo, oid, gate),

        ScopedWriteOp::Unstage { paths } => build_unstage_step(repo, paths),

        ScopedWriteOp::SetUpstream { remote, branch } => {
            let branch = if branch.trim().is_empty() {
                current_branch_name(repo)
            } else {
                branch
            };
            if branch == "HEAD" {
                return Err(anyhow::anyhow!("No branch to set upstream for."));
            }
            Ok(vec![PipelineStep::SetUpstream { remote, branch }])
        }

        ScopedWriteOp::SquashLastN { n } => build_squash_step(repo, n, config, gate).await,

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
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::PullRebase { remote, branch }])
        }

        ScopedWriteOp::ForcePush { remote } => {
            let branch = current_branch_name(repo);
            if branch == "HEAD" {
                return Err(anyhow::anyhow!("No branch to force-push."));
            }
            // Type-to-confirm — destructive to anyone else watching the remote.
            let op_for_explain = ScopedWriteOp::ForcePush {
                remote: remote.clone(),
            };
            let ctx_for_danger = crate::context::build_context(repo)
                .unwrap_or_else(|_| crate::context::RepoContext::empty());
            let resp = gate.prompt(&GateType::TypeToConfirm {
                expected: branch.clone(),
                message: format!(
                    "Type '{}' to confirm force-push to {}/{}:",
                    branch, remote, branch
                ),
                explanation: Some(crate::explain::explain(&op_for_explain)),
                danger: crate::danger::intent_danger(&op_for_explain, &ctx_for_danger),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::ForcePush {
                remote,
                branch,
                expect_oid: None,
            }])
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
            let op_for_explain = ScopedWriteOp::DiscardLocal {
                remote: remote.clone(),
            };
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
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::DiscardLocal { remote, branch }])
        }

        // In-progress backout/continue ops (Phase 11). The situation gate has
        // already verified each is valid for the current Mid* state (and that
        // Continue's index is conflict-free), so map 1:1 to the step. No extra
        // gate here — execute_pipeline shows the plan + SimpleConfirm, and these
        // are low-danger (aborts especially).
        ScopedWriteOp::MergeAbort => Ok(vec![PipelineStep::MergeAbort]),
        ScopedWriteOp::RebaseContinue => Ok(vec![PipelineStep::RebaseContinue]),
        ScopedWriteOp::RebaseSkip => Ok(vec![PipelineStep::RebaseSkip]),
        ScopedWriteOp::RebaseAbort => Ok(vec![PipelineStep::RebaseAbort]),
        ScopedWriteOp::CherryPickContinue => Ok(vec![PipelineStep::CherryPickContinue]),
        ScopedWriteOp::CherryPickAbort => Ok(vec![PipelineStep::CherryPickAbort]),
        ScopedWriteOp::RevertContinue => Ok(vec![PipelineStep::RevertContinue]),
        ScopedWriteOp::RevertAbort => Ok(vec![PipelineStep::RevertAbort]),

        ScopedWriteOp::Init => {
            // Already inside a repo — re-init is harmless but confirm first
            init_repo(gate)?;
            Ok(vec![])
        }

        ScopedWriteOp::AddRemote { name, url } => {
            if url.is_empty() {
                return Err(anyhow::anyhow!("No remote URL provided. Try: gitrust 'add remote origin https://github.com/user/repo.git'"));
            }
            let resp = gate.prompt(&GateType::SimpleConfirm {
                message: format!("Add remote '{}' → {}?", name, url),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::AddRemote { name, url }])
        }

        ScopedWriteOp::AddAndCommit { message, files } => {
            use crate::commands::FileSelection;
            if matches!(files, FileSelection::All)
                && repo.state() != git2::RepositoryState::Merge
                && !has_uncommitted_changes(repo)
            {
                return Err(anyhow::anyhow!("{}", crate::copy::NOTHING_TO_COMMIT));
            }
            let add_files: Vec<String> = match files {
                FileSelection::All => vec![], // empty = add_all
                FileSelection::Explicit(fs) => fs,
                FileSelection::Pattern(_) => vec![], // fall back to add_all
            };
            // add_commit = stage then commit → generate message from full HEAD→workdir diff.
            let final_msg = resolve_or_generate_commit_message(
                repo, message, /*include_workdir=*/ true, config, gate,
            )
            .await?;
            if final_msg.is_empty() {
                return crate::ux::canceled();
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
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::RemoveRemote { name }])
        }

        ScopedWriteOp::DeleteRepo => {
            delete_repo(repo, gate)?;
            Ok(vec![]) // .git is gone; pipeline executor won't run
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
                format!(
                    "Publish '{}' to {} (set upstream tracking)?",
                    branch, remote
                )
            } else {
                format!("Push '{}' to {}?", branch, remote)
            };
            let resp = gate.prompt(&GateType::SimpleConfirm { message: label })?;
            if !matches!(resp, GateResponse::Confirmed) {
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::Push {
                remote,
                branch,
                set_upstream,
            }])
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
                    return crate::ux::canceled();
                }
            }
            // A plain "reset" intent discards the working-tree delta and keeps
            // the commit → reset to HEAD (None). Commit-dropping goes through the
            // undo gate, which passes an explicit parent oid.
            Ok(vec![PipelineStep::Reset { mode, to_oid: None }])
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
        FeaturePipelineOp::CommitPush {
            message,
            stage_first,
        } => commit_push_pipeline(repo, message, stage_first, config, gate).await,
        FeaturePipelineOp::Sync { strategy: _ } => {
            // S13 — cross-branch freshness: reconcile the current branch against
            // the remote default branch (origin/HEAD), reusing the rebase-vs-merge
            // ChoiceGate. The gate now owns the choice, so SyncStrategy is advisory
            // only (kept for prompt/classifier compatibility). Counts are recomputed
            // inside reconcile_flow after the inline fetch.
            reconcile_flow(repo, ReconcileMode::CrossBranchSync, 0, 0, config, gate).await
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
    let manifests = manifest::build_manifest(repo, 25).context("Failed to build file manifest")?;

    if manifests.is_empty() {
        println!("  Nothing modified in the repository.");
        return Ok(());
    }

    let resolution = TwoPassResolver
        .resolve(&manifests, feature_hint, config)
        .await?;

    let files = match resolution {
        ResolutionOutcome::Found(cr) => {
            let resp = gate.prompt(&GateType::FileMultiSelect {
                classification: cr,
                show_all,
            })?;
            match resp {
                GateResponse::FilesSelected(fs) => fs,
                GateResponse::Aborted => {
                    return crate::ux::canceled();
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
        return crate::ux::canceled();
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
    stage_first: Option<crate::commands::FileSelection>,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let branch = current_branch_name(repo);
    let remote = config.remote.clone();
    let set_upstream = !branch_has_upstream(repo, &branch);

    // "Commit everything and push" on a clean tree: there is nothing to commit,
    // so do the half that still applies — push the commits that exist.
    if stage_first.is_some() && !has_uncommitted_changes(repo) {
        println!("  Nothing to commit — pushing the commits you already have.");
        let steps = vec![PipelineStep::Push {
            remote,
            branch,
            set_upstream,
        }];
        return execute_pipeline(repo, steps, config, gate).await;
    }

    // Staging first → the message covers the HEAD→workdir diff; otherwise only
    // what's already staged (index-vs-HEAD).
    let final_msg = resolve_or_generate_commit_message(
        repo,
        message,
        /*include_workdir=*/ stage_first.is_some(),
        config,
        gate,
    )
    .await?;
    if final_msg.is_empty() {
        return crate::ux::canceled();
    }

    let steps = commit_push_steps(stage_first, final_msg, remote, branch, set_upstream);
    execute_pipeline(repo, steps, config, gate).await
}

/// [Add] → Commit → Push. `Add { files: [] }` means "add all", as for add_commit;
/// a `Pattern` (unresolved) also falls back to all.
fn commit_push_steps(
    stage_first: Option<crate::commands::FileSelection>,
    message: String,
    remote: String,
    branch: String,
    set_upstream: bool,
) -> Vec<PipelineStep> {
    use crate::commands::FileSelection;
    let mut steps = Vec::new();
    if let Some(sel) = stage_first {
        let files = match sel {
            FileSelection::Explicit(fs) => fs,
            FileSelection::All | FileSelection::Pattern(_) => vec![],
        };
        steps.push(PipelineStep::Add { files });
    }
    steps.push(PipelineStep::Commit { message });
    steps.push(PipelineStep::Push {
        remote,
        branch,
        set_upstream,
    });
    steps
}

/// Files whose staged (index) version differs from HEAD.
fn staged_change_count(repo: &git2::Repository) -> usize {
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    repo.diff_tree_to_index(head_tree.as_ref(), None, None)
        .map(|d| d.deltas().len())
        .unwrap_or(1) // unreadable → don't block; the commit step reports it
}

/// Any modified, staged or untracked (non-ignored) file in the working tree.
fn has_uncommitted_changes(repo: &git2::Repository) -> bool {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    repo.statuses(Some(&mut opts))
        .map(|s| !s.is_empty())
        .unwrap_or(true)
}

// =============================================================================
// Divergence reconciliation (Phase 12 — S9/S10/S13): fetch → rebase-vs-merge
// ChoiceGate → run it → (conditionally) push.
// =============================================================================

/// Which branch the reconcile targets, and whether the post-reconcile push is
/// safe to auto-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileMode {
    /// S9: reconcile the current branch against its OWN upstream. The
    /// post-reconcile push is a clean fast-forward — always safe to append.
    SameBranch,
    /// S13 (Sync): reconcile the current branch against `origin/<default>`. A
    /// rebase here rewrites already-pushed commits, so it NEVER auto-pushes
    /// (force-with-lease + PushedCommitRewrite are Phase 13); a merge pushes only
    /// if the branch isn't behind its own upstream. This closes the
    /// duplicate-commit loop where an auto-push → NonFastForward → reconcile
    /// against a stale upstream would replay rewritten commits (Amendment 1).
    CrossBranchSync,
}

/// (ahead, behind) of HEAD vs a `remote/branch` ref (e.g. `origin/main`).
/// `(0, 0)` when either side can't be resolved.
fn ahead_behind_vs(repo: &git2::Repository, target_ref: &str) -> (usize, usize) {
    let local = match repo.head().ok().and_then(|h| h.target()) {
        Some(o) => o,
        None => return (0, 0),
    };
    let target = match repo.revparse_single(target_ref).ok().map(|o| o.id()) {
        Some(o) => o,
        None => return (0, 0),
    };
    repo.graph_ahead_behind(local, target).unwrap_or((0, 0))
}

/// Append the post-reconcile Push step for the current branch.
fn with_push(
    mut steps: Vec<PipelineStep>,
    remote: &str,
    branch: &str,
    repo: &git2::Repository,
) -> Vec<PipelineStep> {
    let set_upstream = !branch_has_upstream(repo, branch);
    steps.push(PipelineStep::Push {
        remote: remote.to_string(),
        branch: branch.to_string(),
        set_upstream,
    });
    steps
}

/// The flagship divergence flow. `_ahead_hint`/`_behind_hint` are advisory (from
/// the Redirect); the inline fetch recomputes authoritative counts vs the
/// resolved target, and the degenerate-count guards run BEFORE the gate renders.
async fn reconcile_flow(
    repo: &mut git2::Repository,
    mode: ReconcileMode,
    _ahead_hint: u32,
    _behind_hint: u32,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let git_ops = Git2Operations;
    let remote = config.remote.clone();
    let branch = current_branch_name(repo);
    if branch == "HEAD" {
        return Err(anyhow::anyhow!(
            "No branch to reconcile — repository has no commits or HEAD is detached."
        ));
    }

    // Resolve the target branch + ref (SameBranch → own upstream; Sync → default).
    let target_branch = match mode {
        ReconcileMode::SameBranch => branch.clone(),
        ReconcileMode::CrossBranchSync => crate::git_ops::default_branch(repo, &remote)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not determine {r}'s default branch (origin/HEAD). Try: git remote set-head {r} -a",
                    r = remote
                )
            })?,
    };
    let target_ref = format!("{}/{}", remote, target_branch);

    // Inline fetch — refresh the tracking ref before the choice + guards.
    print!("Fetching {}… ", remote);
    std::io::Write::flush(&mut std::io::stdout()).ok();
    if let Err(e) = git_ops.fetch(repo, &remote, false) {
        println!("✗");
        return Err(anyhow::anyhow!("Could not fetch {}: {}", remote, e));
    }
    println!("✓");

    let (ahead, behind) = ahead_behind_vs(repo, &target_ref);

    // Amendment 2 — degenerate counts skip the gate entirely.
    if behind == 0 {
        println!("  {}", crate::copy::reconcile_up_to_date(&target_ref));
        if mode == ReconcileMode::SameBranch && ahead > 0 {
            let steps = with_push(vec![], &remote, &branch, repo);
            return execute_pipeline(repo, steps, config, gate).await;
        }
        return Ok(());
    }
    if ahead == 0 {
        // Plain fast-forward — rebase-vs-merge would be a meaningless choice
        // (merge would fabricate a pointless merge commit).
        println!("  {}", crate::copy::reconcile_fast_forwarded(&target_ref));
        let msg = format!("Merge {} into {}", target_ref, branch);
        let mut steps = vec![PipelineStep::Merge {
            source: target_ref.clone(),
            message: msg,
        }];
        if mode == ReconcileMode::SameBranch {
            steps = with_push(steps, &remote, &branch, repo);
        }
        return execute_pipeline(repo, steps, config, gate).await;
    }

    // True divergence — offer the rebase-vs-merge choice with consequences.
    let cross = mode == ReconcileMode::CrossBranchSync;
    let options = vec![
        crate::commands::ChoiceOption {
            label: "Rebase".to_string(),
            consequence: crate::copy::reconcile_rebase_consequence(ahead as u32, cross),
        },
        crate::commands::ChoiceOption {
            label: "Merge".to_string(),
            consequence: crate::copy::RECONCILE_MERGE_CONSEQUENCE.to_string(),
        },
        // Backing out is Esc (one calm cancel), not an "Abort" row.
    ];
    let question = format!(
        "Your branch and {} have diverged (↑{} ↓{}). Rebase or merge?",
        target_ref, ahead, behind
    );
    let choice = match gate.prompt(&GateType::ChoiceGate { question, options })? {
        GateResponse::Chose(i) => i,
        _ => {
            return crate::ux::canceled();
        }
    };

    // Build the pipeline per choice + the Amendment-1 push matrix.
    let (steps, post_note): (Vec<PipelineStep>, Option<String>) = match choice {
        0 => {
            let steps = vec![PipelineStep::Rebase {
                onto: target_ref.clone(),
            }];
            match mode {
                ReconcileMode::SameBranch => (with_push(steps, &remote, &branch, repo), None),
                // Phase 13 CLOSES Phase 12's loose end: a Sync-rebase of a
                // PUBLISHED branch now completes the push in-band through the
                // PushedCommitRewrite escalation (TypeToConfirm) + force-with-lease
                // pinned to the fetched tracking oid. If the branch was never
                // pushed, there's nothing to force — just rebase. If the user
                // declines the escalation, we rebase without pushing and print the
                // note.
                ReconcileMode::CrossBranchSync => {
                    if branch_has_upstream(repo, &branch) {
                        let with_force =
                            escalate_pushed_rewrite(repo, steps.clone(), &branch, config, gate)?;
                        if with_force.is_empty() {
                            // Declined — rebase only, no push.
                            (
                                steps,
                                Some(crate::copy::sync_rebase_force_note(&target_ref)),
                            )
                        } else {
                            (with_force, None)
                        }
                    } else {
                        // Not published — nothing to force-push against.
                        (
                            steps,
                            Some(crate::copy::sync_rebase_force_note(&target_ref)),
                        )
                    }
                }
            }
        }
        1 => {
            let msg = format!("Merge {} into {}", target_ref, branch);
            let steps = vec![PipelineStep::Merge {
                source: target_ref.clone(),
                message: msg,
            }];
            match mode {
                ReconcileMode::SameBranch => (with_push(steps, &remote, &branch, repo), None),
                ReconcileMode::CrossBranchSync => {
                    // Push only if the branch isn't behind its OWN upstream.
                    let own_ref = format!("{}/{}", remote, branch);
                    let (_own_ahead, own_behind) = ahead_behind_vs(repo, &own_ref);
                    if own_behind == 0 {
                        (with_push(steps, &remote, &branch, repo), None)
                    } else {
                        (steps, Some(crate::copy::SYNC_MERGE_BEHIND_NOTE.to_string()))
                    }
                }
            }
        }
        _ => {
            return crate::ux::canceled();
        }
    };

    let result = execute_pipeline(repo, steps, config, gate).await;
    if result.is_ok() {
        if let Some(note) = post_note {
            println!("  {}", note);
        }
    }
    result
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
    println!(
        "  {}",
        dim("(you have uncommitted changes; push needs a commit first)")
    );
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
                PipelineStep::Commit {
                    message: "update".to_string(),
                },
                PipelineStep::Push {
                    remote: remote.clone(),
                    branch,
                    set_upstream,
                },
            ]
        }
        1 => vec![
            PipelineStep::Add { files: vec![] },
            PipelineStep::Commit {
                message: "update".to_string(),
            },
        ],
        _ => return Ok(()),
    };

    let fut = Box::pin(execute_pipeline(repo, steps, config, gate));
    fut.await
}

/// Whether the recovery picker actually resolved the failed step.
///
/// Informational rows (Show/Verify/Wait) and Cancel do NOT resolve it — the
/// original failure still stands, so the caller must propagate the error and
/// exit nonzero (M-21). Only a recovery *pipeline* that runs to completion
/// resolves the failure into a success.
#[derive(Debug, PartialEq, Eq)]
enum RecoveryResult {
    Resolved,
    NotResolved,
}

/// Render a class-aware recovery picker after a failed step. Each option is
/// one keystroke. Each option *does* the thing — no chained command recipes.
async fn handle_classified_recovery(
    repo: &mut git2::Repository,
    class: &crate::git_ops::FailureClass,
    failed_verb: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<RecoveryResult> {
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
                        PipelineStep::Pull {
                            remote: remote.clone(),
                        },
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
                        PipelineStep::Commit {
                            message: "update".to_string(),
                        },
                        PipelineStep::Pull {
                            remote: remote.clone(),
                        },
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
                pipeline: Some(vec![PipelineStep::Pull {
                    remote: remote.clone(),
                }]),
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
        FC::NonFastForward => return Ok(RecoveryResult::NotResolved), // Handled by merge-preview path
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
        // Cancelled / non-choice response — the failure stands.
        _ => return Ok(RecoveryResult::NotResolved),
    };
    if chosen_idx >= options.len() {
        return Ok(RecoveryResult::NotResolved);
    }
    // The trailing "Cancel" row on every picker: chosen but resolves nothing.
    if options[chosen_idx].label == "Cancel" {
        return Ok(RecoveryResult::NotResolved);
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
                println!(
                    "  {}",
                    dim("Hook output was printed above the failure block.")
                );
            }
            _ => {}
        }
        // Informational rows print context but do NOT fix the failed step, so
        // the original error must still propagate (M-21: "Show" the hook output
        // must not turn a blocked commit into exit 0).
        return Ok(RecoveryResult::NotResolved);
    }

    // Run the chained pipeline end-to-end. Only a pipeline that runs to
    // completion resolves the failure into a success.
    if let Some(steps) = options[chosen_idx].pipeline.clone() {
        // Box::pin to break async recursion (this fn → execute_pipeline →
        // handle_classified_recovery).
        let fut = Box::pin(execute_pipeline(repo, steps, config, gate));
        fut.await?;
        return Ok(RecoveryResult::Resolved);
    }

    Ok(RecoveryResult::NotResolved)
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
    // Skip snapshot capture for a pure abort/skip pipeline: those run WITH a
    // conflicted index, and capture_snapshot's `index.write_tree()` errors on
    // unmerged entries. git's own ORIG_HEAD is the recovery net for a backout.
    // Continue is NOT a backout step here, so it still snapshots (index clean).
    let snapshot: Option<crate::recovery::PipelineSnapshot> =
        if crate::commands::pipeline_is_pure_backout(&steps) {
            None
        } else {
            Some(recovery_mgr.capture_snapshot(repo)?)
        };
    let sha7 = snapshot
        .as_ref()
        .and_then(|s| s.head_oid.as_deref())
        .map(|s| &s[..s.len().min(7)])
        .unwrap_or("");

    // Show preview with the snapshot SHA in the header.
    let ctx_for_preview = crate::context::build_context(repo)
        .unwrap_or_else(|_| crate::context::RepoContext::empty());
    let preview = validator.build_preview(&steps, &warnings, &ctx_for_preview, sha7);
    println!("{}", preview);

    // Confirmation gate: one keypress, exactly as the legend says (Enter runs,
    // q/Esc cancels). Variant C's focus/edit/skip keys are not wired, so they
    // are not advertised.
    let proceed = gate.prompt(&GateType::RunPlan)?;
    if !matches!(proceed, GateResponse::Confirmed) {
        // The snapshot was captured before the gate — clean it up since
        // nothing changed.
        if let Some(s) = &snapshot {
            let _ = recovery_mgr.delete_snapshot(s);
        }
        return crate::ux::canceled();
    }

    let executor = DefaultExecutor {
        git_ops: Git2Operations,
        hooks_mode: config.hooks.clone(),
    };

    let mut last_error: Option<anyhow::Error> = None;
    let mut no_local_change = false;
    let mut failed_push: Option<(String, String)> = None; // (remote, branch) when a Push step fails
                                                          // Set when a Merge/Rebase step paused on conflicts (Phase 12): the repo is
                                                          // left in an in-progress op and recovery belongs to git's state machine +
                                                          // the Phase 11 verbs, NOT the snapshot-restore path.
    let mut reconcile_paused: Option<(&'static str, usize)> = None;
    // Classified network/merge failure captured during the step loop. When
    // set, the executor renders a class-aware picker instead of the generic
    // anyhow chain.
    let mut classified: Option<(crate::git_ops::FailureClass, String, String)> = None; // (class, raw_stderr, step_verb)
    let gitrust_debug = std::env::var("GITRUST_DEBUG").is_ok();

    let use_color = crate::danger::use_color();
    let glyph_running = if use_color {
        crate::copy::TIMELINE_PENDING
    } else {
        crate::copy::TIMELINE_PENDING_ASCII
    };
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
        // Snapshot the trash-net BEFORE a destructive step so we can report the
        // backup from what was ACTUALLY written — not print a canned "backed up"
        // line when the destroy-set was empty and no backup exists (M-16 Defect 1).
        let trash_before = trash_dir_snapshot();
        let (verb, detail) = crate::validation::format_step_label(step);
        let level = crate::danger::step_danger(step, &ctx_for_preview);
        let marker = crate::danger::colored_marker(level);

        // "Running" placeholder, overwritten in place with the result — only on
        // a terminal: piped output would keep both copies of the line.
        let live = {
            use std::io::IsTerminal;
            std::io::stdout().is_terminal()
        };
        if live {
            print!("  {}  {:<10}{}", dim(glyph_running), verb, detail);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }

        match executor.execute(repo, step) {
            Ok(()) => {
                // Post-check
                let post_result = validator.run_post_check(repo, step, &pre_state);
                let (final_glyph, post_warning, post_failed) = match &post_result {
                    Ok(PostCheckResult::Verified) => (green(glyph_done), None, None),
                    Ok(PostCheckResult::Warning(w)) => (green(glyph_done), Some(w.clone()), None),
                    Ok(PostCheckResult::Failed(f)) => (red(glyph_fail), None, Some(f.clone())),
                    Err(e) => (
                        green(glyph_done),
                        Some(format!("post-check error: {}", e)),
                        None,
                    ),
                };

                // The result replaces the running line: ✓ verb detail. The
                // severity marker was shown in the plan; repeating it on a
                // finished step only adds noise.
                let _ = &marker;
                if live {
                    print!("\r\x1b[2K");
                }
                println!("  {}  {:<10}{}", final_glyph, verb, detail);

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
                // S30: after a destructive step, tell the user where the backup
                // lives — but ONLY if the trash-net actually wrote one this step.
                // Detect the new dir by diffing the trash snapshot taken before
                // execution. An empty destroy-set writes nothing, so no note is
                // printed (M-16 Defect 1: the note used to print unconditionally,
                // claiming a backup that did not exist).
                if matches!(
                    step,
                    PipelineStep::Clean { .. }
                        | PipelineStep::Reset {
                            mode: ResetMode::Hard,
                            ..
                        }
                        | PipelineStep::DiscardLocal { .. }
                ) {
                    if let Some(new_dir) = new_trash_dir_since(&trash_before) {
                        let note =
                            crate::copy::trash_backup_note(&new_dir, config.trash_retention_days);
                        println!("  {}  {}", dim("│"), dim(&note));
                    }
                }
                if i < last_idx {
                    println!("  {}", dim("│"));
                }
            }
            Err(e) => {
                // Replace the running line with the failure.
                if live {
                    print!("\r\x1b[2K");
                }
                println!("  {}  {:<10}{}", red(glyph_fail), verb, detail);

                // Reconcile pause (Phase 12): a Merge/Rebase step left an
                // in-progress op (conflict). This is a calm handoff into the
                // Phase 11 verbs, NOT a snapshot-recoverable failure — record it
                // and stop before the generic error spew. `repo.state() != Clean`
                // after a step error reliably indicates this (Add/Commit/Push
                // failures never leave a non-Clean state).
                if repo.state() != git2::RepositoryState::Clean {
                    let op = match step {
                        PipelineStep::Rebase { .. } => "rebase",
                        PipelineStep::Merge { .. } => "merge",
                        _ => "operation",
                    };
                    // The conflict was written by the `git` SUBPROCESS, so the
                    // parent's in-memory index handle is stale — force a fresh
                    // read from disk before counting, or we report "0 file(s)"
                    // for a real conflict (M-08 Defect 1).
                    let n = repo
                        .index()
                        .ok()
                        .and_then(|mut idx| {
                            let _ = idx.read(true); // reload from disk
                            idx.conflicts().ok().map(|c| c.count())
                        })
                        .unwrap_or(0);
                    reconcile_paused = Some((op, n));
                    last_error = Some(e);
                    break;
                }

                // Did the operation return a typed `ClassifiedFailure`?
                // If so, capture it for the post-loop picker and suppress
                // the generic "Step failed:" / "Recovery: ..." prints —
                // the picker is the action surface.
                if let Some(failure) = e.downcast_ref::<crate::git_ops::ClassifiedFailure>() {
                    // One-line headline. Details live in the picker.
                    println!("  {}  {}", red("│"), failure.class.what());
                    if gitrust_debug {
                        eprintln!(
                            "  {}  {}",
                            dim("│"),
                            dim(&format!(
                                "(git: {})",
                                failure.raw_stderr.trim().replace('\n', " · ")
                            ))
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
                    let action = snapshot.as_ref().map(|s| step.recovery_action(s));
                    no_local_change = matches!(
                        action,
                        Some(crate::commands::RecoveryAction::NoLocalChange { .. })
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
                let action = snapshot.as_ref().map(|s| step.recovery_action(s));
                no_local_change = matches!(
                    action,
                    Some(crate::commands::RecoveryAction::NoLocalChange { .. })
                ) && i == 0;
                if gitrust_debug {
                    eprintln!("  Recovery: {:?}", action);
                }
                last_error = Some(e);
                break;
            }
        }
    }

    // Reconcile pause (Phase 12): the snapshot is intentionally PRESERVED —
    // git's ORIG_HEAD + `gitrust "abort <op>"` (Phase 11) own recovery, and
    // `gitrust --recover` defers via AdviseGitAbort while the op is in progress.
    // Return the calm handoff as an Err so a paused rebase exits NONZERO (a
    // half-done op is not success — the 10.1 finding #2 applies), with no
    // "snapshot preserved / run --recover" spew. The calm text IS the message.
    if let Some((op, n)) = reconcile_paused {
        return Err(anyhow::anyhow!(
            "{}",
            crate::copy::reconcile_paused_block(op, n)
        ));
    }

    // Delete snapshot on success, or when the first step failed without changing repo state.
    // Keep it on real failures (for `gitrust --recover`).
    if last_error.is_none() {
        if let Some(s) = &snapshot {
            let _ = recovery_mgr.delete_snapshot(s);
        }
        // M-08 Defect 3: if this pipeline RESOLVED an in-progress op (a
        // continue/skip/abort) and the repo is now clean, clear any snapshot the
        // earlier PAUSE intentionally preserved — otherwise it keeps nagging
        // "interrupted pipeline / run --recover" on every later command, even a
        // read-only status, pointing at a --recover that could undo the finished
        // work. Best-effort; scoped to this repo's workdir.
        let resolved_in_progress = steps.iter().any(|s| {
            matches!(
                s,
                PipelineStep::RebaseContinue
                    | PipelineStep::RebaseSkip
                    | PipelineStep::RebaseAbort
                    | PipelineStep::MergeAbort
                    | PipelineStep::CherryPickContinue
                    | PipelineStep::CherryPickAbort
                    | PipelineStep::RevertContinue
                    | PipelineStep::RevertAbort
            )
        });
        if resolved_in_progress && repo.state() == git2::RepositoryState::Clean {
            if let Some(wd) = repo.workdir().map(|p| p.to_string_lossy().into_owned()) {
                let _ = recovery_mgr.clear_snapshots_for_workdir(&wd);
            }
        }
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
            done_line.push_str(&format!(", ↑{} ↓{}", post_ctx.ahead, post_ctx.behind));
        }
        println!("  {} {}", green("✓"), green(&done_line));
        let pushed = steps.iter().any(|s| matches!(s, PipelineStep::Push { .. }));
        println!(
            "  {}",
            dim(&crate::copy::plan_next_hint(
                commits_made,
                pushed,
                post_ctx.ahead
            ))
        );
    } else if no_local_change || failed_push.is_some() {
        // Push that fails non-FF leaves the local repo in a fine state — the
        // earlier commit is intact, only the network step refused. No snapshot
        // recovery is needed.
        if let Some(s) = &snapshot {
            let _ = recovery_mgr.delete_snapshot(s);
        }
    } else if let Some(s) = &snapshot {
        eprintln!(
            "\n  Snapshot preserved at ~/.gitrust/snapshots/{}.json",
            s.pipeline_id
        );
        eprintln!("  Run 'gitrust --recover' to restore to pre-pipeline state.");
    } else {
        // A backout op (no snapshot) failed — git's own state machine holds the
        // recovery point (ORIG_HEAD / the in-progress op is still intact).
        eprintln!("\n  The operation did not complete — your repository state is unchanged.");
    }

    // Class-aware recovery picker. Renders right at failure time so the
    // user has a one-keystroke path forward instead of a recipe to memorize.
    if let Some((class, _raw, verb)) = classified.clone() {
        // NonFastForward keeps using the merge-preview hand-off below.
        if !matches!(class, crate::git_ops::FailureClass::NonFastForward) {
            match handle_classified_recovery(repo, &class, &verb, config, gate).await {
                // A recovery pipeline ran to completion — the failure is truly
                // resolved, so a clean exit is honest.
                Ok(RecoveryResult::Resolved) => return Ok(()),
                // Show/Cancel/Wait/Verify only informed the user; the original
                // step is still failed, so fall through to `last_error` and exit
                // nonzero (M-21).
                Ok(RecoveryResult::NotResolved) => {}
                // The recovery attempt itself errored — surface it, then still
                // fall through to propagate the original failure below.
                Err(e) => eprintln!("  {}", e),
            }
        }
    }

    // When the failure was a non-fast-forward push, offer the same conflict-
    // resolution menu the user gets from `gitrust "safe to pull"`. The local
    // commit is already safe; the user just needs to decide how to reconcile.
    if failed_push.is_some() {
        eprintln!("\nPush was rejected because the remote has commits you don't have.");
        eprintln!("Let's reconcile…\n");
        // S10 — offer the reconcile flow directly (rebase-vs-merge ChoiceGate),
        // not the old dead-end "pull first" string. SameBranch: the rejected push
        // was the current branch against its own upstream. Recursive across the
        // async boundary → box the future. Propagate the result so a paused
        // reconcile still exits nonzero.
        let fut = Box::pin(reconcile_flow(
            repo,
            ReconcileMode::SameBranch,
            0,
            0,
            config,
            gate,
        ));
        return fut.await;
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
///
/// "recover" precedence (Phase 14): a pending interrupted-pipeline snapshot for
/// THIS repo means the snapshot path — finish the interrupted thing first. When
/// there is none, this is not an error: point the user sideways to the forensic
/// flow (`gitrust "find my lost work"`), which recovers lost commits from reflogs
/// and dangling objects.
pub fn execute_recovery() -> anyhow::Result<()> {
    let recovery_mgr = FileRecoveryManager;

    let repo_path = find_repo_root()?;
    let repo = git2::Repository::open(&repo_path)?;
    let this_workdir = repo.workdir().map(|p| p.to_string_lossy().into_owned());

    // Scoped to this repo (S56) — another repo's snapshot must not be restored here.
    let snapshot = match recovery_mgr.check_for_interrupted(this_workdir.as_deref())? {
        Some(s) => s,
        None => {
            println!("{}", crate::copy::RECOVER_NONE_TRY_FORENSIC);
            return Ok(());
        }
    };

    println!("Found interrupted pipeline from {}.", snapshot.captured_at);

    let current_head = repo.head().ok().and_then(|h| h.target());

    // Situation-aware: never fight git's own state machine, and never silently
    // strand a detached HEAD out of an operation that actually completed.
    match crate::recovery::recovery_plan(repo.state(), &snapshot, current_head) {
        crate::recovery::RecoveryDecision::AdviseGitAbort => {
            println!("{}", crate::copy::RECOVER_MID_OP_ADVICE);
            Ok(())
        }
        crate::recovery::RecoveryDecision::WarnDetached => {
            println!("{}", crate::copy::RECOVER_DETACHED_WARN);
            let gate = InquireGate;
            let resp = gate.prompt(&GateType::SimpleConfirm {
                message: "Restore anyway, moving to a detached HEAD?".to_string(),
            })?;
            if matches!(resp, GateResponse::Confirmed) {
                recovery_mgr.execute_recovery(&repo, &snapshot)?;
            } else {
                println!(
                    "  Left as-is. The snapshot is preserved — `gitrust recover --clean` to remove it."
                );
            }
            Ok(())
        }
        crate::recovery::RecoveryDecision::Restore => {
            println!("Restoring repository to pre-pipeline state...\n");
            recovery_mgr.execute_recovery(&repo, &snapshot)?;
            Ok(())
        }
    }
}

// =============================================================================
// Fallback interactive mode
// =============================================================================

/// Kev is unreachable: offer every operation by name. It goes through the same
/// gate as any menu (so `--yes` refuses — it can't pick for you) and the same
/// deterministic builder as a Kev route: arguments are captured from the
/// request or asked for, and destructive ops still meet their own gates (undo
/// → the undo gate, clean-up → the clean-up gate). The old manual menu listed
/// raw `git …` commands, mislabeled commit+push as "git push", and offered a
/// hard reset past every gate.
fn manual_menu(
    context: &crate::context::RepoContext,
    query: &str,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<crate::commands::IntentResult> {
    use crate::commands::{ChoiceOption, Engine, IntentResult};
    let mut ops: Vec<&str> = crate::kev::ops()
        .iter()
        .map(|(o, _)| o.as_str())
        .filter(|o| *o != crate::kev::REJECT)
        .collect();
    // Keep commit-and-push next to commit-everything.
    if let Some(i) = ops.iter().position(|o| *o == "commit_push") {
        let cp = ops.remove(i);
        let at = ops
            .iter()
            .position(|o| *o == "add_commit")
            .map_or(ops.len(), |j| j + 1);
        ops.insert(at, cp);
    }
    let options: Vec<ChoiceOption> = ops
        .iter()
        .map(|op| {
            let (title, hint) = crate::copy::op_label(op).unwrap_or((op, ""));
            ChoiceOption {
                label: title.to_string(),
                consequence: hint.to_string(),
            }
        })
        .collect();
    let op = match gate.prompt(&GateType::ChoiceGate {
        question: crate::copy::MANUAL_MENU_QUESTION.to_string(),
        options,
    })? {
        GateResponse::Chose(i) if i < ops.len() => ops[i],
        _ => return crate::ux::canceled(),
    };
    let mut ask = |kind: ArgKind| ask_argument(kind, gate);
    let intent = crate::intent::build_from_op(op, query, context, &mut ask)?;
    Ok(IntentResult {
        intent,
        confidence: 1.0,
        raw_query: query.to_string(),
        engine: Engine::User,
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
        return crate::ux::canceled();
    }

    print!("  Removing {} … ", display);
    std::fs::remove_dir_all(&git_dir).with_context(|| format!("Failed to remove {}", display))?;
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
        return crate::ux::canceled();
    }
    print!("  [1] git init … ");
    git2::Repository::init(&cwd)
        .map_err(|e| anyhow::anyhow!("Failed to initialize repository: {}", e))?;
    println!("✓");
    println!(
        "\nInitialized empty git repository in {}/.git/",
        cwd.display()
    );
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
/// Detect an explicit multi-commit count in an undo phrase ("undo the last 3
/// commits"). Returns the N when > 1. Parsing only — never decides a semantic.
fn undo_multi_commit_count(raw_query: &str) -> Option<u32> {
    let q = raw_query.to_lowercase();
    // Look for a small integer following "last"/"past" or preceding "commits".
    let tokens: Vec<&str> = q.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if let Ok(n) = t.parse::<u32>() {
            let prev = i.checked_sub(1).map(|j| tokens[j]);
            let next = tokens.get(i + 1).copied();
            let contextual = matches!(prev, Some("last" | "past" | "recent"))
                || matches!(next, Some("commits" | "commit"));
            if contextual && n > 1 {
                return Some(n);
            }
        }
    }
    None
}

/// The undo disambiguation gate (S34/P6). The LLM's job ended at recognizing
/// undo-language; this deterministic ChoiceGate decides the semantic. No undo
/// phrase can reach a destructive mode without this gate having offered every
/// option. HEAD-only in v1: a count > 1 halts with guidance (never acts on an
/// LLM-extracted count).
fn build_undo_intent(
    repo: &git2::Repository,
    raw_query: &str,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    // Step 0: multi-commit undo halts before touching anything (P6).
    if let Some(n) = undo_multi_commit_count(raw_query) {
        return Err(anyhow::anyhow!(
            "{}",
            crate::copy::undo_multi_commit_guidance(n)
        ));
    }

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
    let head_oid = head_commit.id();

    let parent = match head_commit.parent(0) {
        Ok(p) => p,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Cannot undo: this is the first commit (no parent to reset to).\n  To discard it entirely, use: gitrust \"reset --hard\" with care."
            ));
        }
    };

    let undone_sha = head_oid.to_string()[..7].to_string();
    let undone_summary = head_commit.summary().unwrap_or("(no message)").to_string();

    // Is HEAD already published? If so, reorder the gate to put the safe option
    // (Revert) first and explain why.
    let branch = current_branch_name(repo);
    let pushed =
        branch != "HEAD" && crate::git_ops::commit_is_pushed(repo, "origin", &branch, head_oid);

    // The four semantics. Each is a (label, consequence, builder).
    use crate::commands::{ChoiceOption, ResetMode};
    let keep_staged = ChoiceOption {
        label: crate::copy::UNDO_KEEP_STAGED_LABEL.to_string(),
        consequence: crate::copy::UNDO_KEEP_STAGED_CONSEQUENCE.to_string(),
    };
    let keep_in_files = ChoiceOption {
        label: crate::copy::UNDO_KEEP_IN_FILES_LABEL.to_string(),
        consequence: crate::copy::UNDO_KEEP_IN_FILES_CONSEQUENCE.to_string(),
    };
    let revert = ChoiceOption {
        label: crate::copy::UNDO_REVERT_LABEL.to_string(),
        consequence: crate::copy::UNDO_REVERT_CONSEQUENCE.to_string(),
    };
    let discard = ChoiceOption {
        label: format!(
            "{}  {}",
            crate::copy::UNDO_DISCARD_LABEL,
            crate::danger::colored_marker(crate::danger::DangerLevel::Destructive)
        ),
        consequence: crate::copy::UNDO_DISCARD_CONSEQUENCE.to_string(),
    };

    // Semantics enum kept parallel to the option ordering so index → step is
    // unambiguous regardless of the pushed reorder.
    enum UndoChoice {
        KeepStaged,
        KeepInFiles,
        Revert,
        Discard,
    }
    let (options, semantics): (Vec<ChoiceOption>, Vec<UndoChoice>) = if pushed {
        (
            vec![revert, keep_staged, keep_in_files, discard],
            vec![
                UndoChoice::Revert,
                UndoChoice::KeepStaged,
                UndoChoice::KeepInFiles,
                UndoChoice::Discard,
            ],
        )
    } else {
        (
            vec![keep_staged, keep_in_files, revert, discard],
            vec![
                UndoChoice::KeepStaged,
                UndoChoice::KeepInFiles,
                UndoChoice::Revert,
                UndoChoice::Discard,
            ],
        )
    };

    // Why revert is first when pushed goes in the question, once — not
    // prepended to an option's description.
    let question = crate::copy::undo_gate_question(&undone_sha, &undone_summary, pushed);
    let choice = match gate.prompt(&GateType::ChoiceGate { question, options })? {
        GateResponse::Chose(i) => i,
        _ => {
            return crate::ux::canceled();
        }
    };

    match &semantics[choice] {
        UndoChoice::KeepStaged => Ok(vec![PipelineStep::Undo {
            to_oid: parent.id().to_string(),
            undone_summary,
            undone_sha,
        }]),
        UndoChoice::KeepInFiles => Ok(vec![PipelineStep::Reset {
            mode: ResetMode::Mixed,
            // Drop the commit but keep its changes in the working tree: reset to
            // the PARENT, not HEAD (a HEAD-target mixed reset would no-op).
            to_oid: Some(parent.id().to_string()),
        }]),
        UndoChoice::Revert => Ok(vec![PipelineStep::Revert {
            oid: head_oid.to_string(),
        }]),
        UndoChoice::Discard => {
            // The ONLY destructive path — a type-to-confirm before the step is
            // even constructed. (AutoConfirmGate already refused the ChoiceGate
            // above, so `--yes` never reaches here.)
            let ctx = crate::context::build_context(repo)
                .unwrap_or_else(|_| crate::context::RepoContext::empty());
            let op_for_explain = ScopedWriteOp::Reset {
                mode: ResetMode::Hard,
            };
            let resp = gate.prompt(&GateType::TypeToConfirm {
                expected: undone_sha.clone(),
                message: format!(
                    "Type '{}' to DISCARD commit {} \"{}\" and all its changes:",
                    undone_sha, undone_sha, undone_summary
                ),
                explanation: Some(crate::explain::explain(&op_for_explain)),
                danger: crate::danger::intent_danger(&op_for_explain, &ctx),
            })?;
            if !matches!(resp, GateResponse::Confirmed) {
                return crate::ux::canceled();
            }
            Ok(vec![PipelineStep::Reset {
                mode: ResetMode::Hard,
                // DISCARD the commit and its changes: reset --hard to the PARENT.
                // A HEAD-target reset here was the M-16 phantom no-op (the commit
                // survived while the tool reported "Done").
                to_oid: Some(parent.id().to_string()),
            }])
        }
    }
}

/// Destructive-cleanup gate (S62/P6). Innocent-sounding "clean up" / "start
/// fresh" language never reaches a concrete `Clean`/`Reset::Hard` from the LLM;
/// this deterministic ChoiceGate spells out exactly what would be deleted
/// (tracked vs untracked, with counts) first. Degenerate guard: nothing dirty →
/// no gate. The trash-net backs everything up before deletion (in git_ops).
fn build_cleanup_intent(
    repo: &git2::Repository,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    use crate::commands::ChoiceOption;

    // Count what each destructive option would affect.
    let (tracked, untracked) = cleanup_counts(repo);

    // Degenerate guard: render no gate when there is nothing to clean.
    if tracked == 0 && untracked == 0 {
        println!("{}", crate::copy::CLEANUP_NOTHING);
        return Ok(vec![]);
    }

    // Build only the options that apply, keeping a parallel semantic list so the
    // chosen index maps unambiguously (mirrors the undo gate).
    #[derive(Clone, Copy)]
    enum CleanupChoice {
        Tracked,
        Untracked,
        Both,
    }
    let mut options: Vec<ChoiceOption> = Vec::new();
    let mut semantics: Vec<CleanupChoice> = Vec::new();
    // Backing out is Esc (one calm cancel), not an "Abort" row.
    if tracked > 0 {
        options.push(ChoiceOption {
            label: crate::copy::cleanup_tracked_label(tracked),
            consequence: crate::copy::CLEANUP_TRACKED_CONSEQUENCE.to_string(),
        });
        semantics.push(CleanupChoice::Tracked);
    }
    if untracked > 0 {
        options.push(ChoiceOption {
            label: crate::copy::cleanup_untracked_label(untracked),
            consequence: crate::copy::CLEANUP_UNTRACKED_CONSEQUENCE.to_string(),
        });
        semantics.push(CleanupChoice::Untracked);
    }
    if tracked > 0 && untracked > 0 {
        options.push(ChoiceOption {
            label: crate::copy::CLEANUP_BOTH_LABEL.to_string(),
            consequence: crate::copy::CLEANUP_BOTH_CONSEQUENCE.to_string(),
        });
        semantics.push(CleanupChoice::Both);
    }

    let choice = match gate.prompt(&GateType::ChoiceGate {
        question: crate::copy::CLEANUP_GATE_QUESTION.to_string(),
        options,
    })? {
        GateResponse::Chose(i) => i,
        _ => {
            return crate::ux::canceled();
        }
    };

    // Map choice → concrete steps. `Reset::Hard` and `Clean` each back up their
    // own destroy-set via the git_ops trash-net; ordering `Reset` before `Clean`
    // when both is chosen keeps tracked/untracked backups in separate dirs.
    let steps = match semantics[choice] {
        CleanupChoice::Tracked => vec![PipelineStep::Reset {
            mode: ResetMode::Hard,
            // Cleanup discards the DIRTY WORKING TREE but keeps the commit:
            // reset --hard to HEAD (None), not to a parent. (M-26 path.)
            to_oid: None,
        }],
        CleanupChoice::Untracked => vec![PipelineStep::Clean { trash_net: true }],
        CleanupChoice::Both => vec![
            PipelineStep::Reset {
                mode: ResetMode::Hard,
                to_oid: None,
            },
            PipelineStep::Clean { trash_net: true },
        ],
    };
    Ok(steps)
}

/// Any uncommitted work in the tree — staged, worktree-modified, or untracked.
/// Used by the branch-here rescue to decide whether to stage before resuming
/// the original intent (M-13).
fn working_tree_has_changes(repo: &git2::Repository) -> bool {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    repo.statuses(Some(&mut opts))
        .map(|st| !st.is_empty())
        .unwrap_or(false)
}

/// (tracked-modified count, untracked count) for the cleanup gate.
fn cleanup_counts(repo: &git2::Repository) -> (usize, usize) {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let mut tracked = 0usize;
    let mut untracked = 0usize;
    if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
        for e in statuses.iter() {
            let s = e.status();
            if s.contains(git2::Status::WT_NEW) {
                untracked += 1;
            } else if s.intersects(
                git2::Status::WT_MODIFIED
                    | git2::Status::WT_TYPECHANGE
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_RENAMED,
            ) {
                tracked += 1;
            }
        }
    }
    (tracked, untracked)
}

/// Forensic recovery flow (S24/26/27, P5). Scans for lost commits, shows the
/// RecoveryBrowser, and restores the chosen commit onto a `rescue/*` branch —
/// NEVER touching the current branch/index/working tree (Q4). Imperative (no
/// snapshot, no pipeline): the only mutation is creating a ref.
fn handle_forensic_recover(
    repo: &git2::Repository,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<()> {
    let cfg = crate::recovery::LostScanConfig::with_retention_days(30);
    let scan = crate::recovery::find_lost_commits(repo, &cfg);

    if scan.commits.is_empty() {
        // S29 honesty: gc'd / never-committed work may be unrecoverable.
        println!("{}", crate::copy::FORENSIC_NONE_FOUND);
        return Ok(());
    }
    if scan.total_found > scan.commits.len() {
        println!(
            "{}",
            crate::copy::forensic_truncated(scan.commits.len(), scan.total_found)
        );
    }

    let candidates = scan.commits.clone();
    let choice = match gate.prompt(&GateType::RecoveryBrowser {
        candidates: candidates.clone(),
    })? {
        GateResponse::Chose(i) => i,
        _ => {
            println!("  Nothing recovered.");
            return Ok(());
        }
    };
    let chosen = &candidates[choice];

    // rescue/<date>-<short-oid>. Date from a local timestamp (chrono is a dep;
    // Local::now is allowed in production — this is not a workflow script).
    let short = &chosen.oid[..chosen.oid.len().min(7)];
    let date = chrono::Local::now().format("%Y%m%d").to_string();
    let branch = format!("rescue/{}-{}", date, short);

    Git2Operations.create_branch_at(repo, &branch, &chosen.oid)?;
    println!("{}", crate::copy::forensic_restored(&branch, short));
    let _ = config;
    Ok(())
}

/// Build an Amend step, escalating with a PushedCommitRewrite type-to-confirm
/// when HEAD is already published (S32/S33). Returns the amend step, and on the
/// pushed path also a force-push step pinned to the fetched tracking oid.
async fn build_amend_step(
    repo: &git2::Repository,
    message: Option<String>,
    include_staged: bool,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    let _ = config;
    if repo.head_detached().unwrap_or(false) {
        return Err(anyhow::anyhow!(
            "Cannot amend in detached HEAD state. Checkout a branch first."
        ));
    }
    let head_oid = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .ok_or_else(|| anyhow::anyhow!("No commit to amend — repository is empty."))?;
    let branch = current_branch_name(repo);

    let amend = PipelineStep::Amend {
        message,
        include_staged,
    };

    // PushedCommitRewrite escalation (S33): if HEAD is on the remote, amending
    // rewrites published history → force push with lease, gated by TypeToConfirm.
    if branch != "HEAD" && crate::git_ops::commit_is_pushed(repo, &config.remote, &branch, head_oid)
    {
        return escalate_pushed_rewrite(repo, vec![amend], &branch, config, gate);
    }
    Ok(vec![amend])
}

/// Build a Revert step. HEAD-only in v1 when `oid` is empty. Revert is the
/// team-safe public undo — no escalation needed (no history rewrite).
fn build_revert_step(
    repo: &git2::Repository,
    oid: String,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    let target = if oid.trim().is_empty() {
        repo.head()
            .ok()
            .and_then(|h| h.target())
            .ok_or_else(|| anyhow::anyhow!("No commit to revert — repository is empty."))?
            .to_string()
    } else {
        // Resolve to a full oid so the step is concrete.
        repo.revparse_single(&oid)
            .map_err(|_| anyhow::anyhow!("'{}' is not a valid commit to revert.", oid))?
            .id()
            .to_string()
    };
    let short = &target[..7.min(target.len())];
    let resp = gate.prompt(&GateType::SimpleConfirm {
        message: format!("Revert {} with a new inverse commit?", short),
    })?;
    if !matches!(resp, GateResponse::Confirmed) {
        return crate::ux::canceled();
    }
    Ok(vec![PipelineStep::Revert { oid: target }])
}

/// Build an Unstage step. Enumerates the concrete staged paths when the request
/// was "unstage all" (empty paths) — nothing ambiguous reaches the executor.
fn build_unstage_step(
    repo: &git2::Repository,
    paths: Vec<String>,
) -> anyhow::Result<Vec<PipelineStep>> {
    let resolved = if paths.is_empty() {
        // Enumerate what's actually staged vs HEAD.
        let ctx = crate::context::build_context(repo)
            .unwrap_or_else(|_| crate::context::RepoContext::empty());
        ctx.status.staged.clone()
    } else {
        paths
    };
    if resolved.is_empty() {
        println!("  Nothing staged to unstage.");
        return Ok(vec![]);
    }
    Ok(vec![PipelineStep::Unstage { paths: resolved }])
}

/// Build a SquashLastN pipeline (S40): resolve the merged message, reject merge
/// commits, escalate on pushed commits. Preserves the working tree.
async fn build_squash_step(
    repo: &git2::Repository,
    n: u32,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    if n < 2 {
        return Err(anyhow::anyhow!(
            "Squash needs at least 2 commits (got {}). Nothing to combine.",
            n
        ));
    }
    if repo.head_detached().unwrap_or(false) {
        return Err(anyhow::anyhow!(
            "Cannot squash in detached HEAD state. Checkout a branch first."
        ));
    }
    // Walk the last n commits: reject any merge commit, collect summaries, find
    // the oldest so we can escalate if it's published.
    let head = repo
        .head()
        .map_err(|_| anyhow::anyhow!("No commits to squash — repository is empty."))?;
    let mut commit = head.peel_to_commit().context("HEAD is not a commit")?;
    let mut summaries: Vec<String> = Vec::new();
    let mut oldest_oid = commit.id();
    for i in 0..n {
        if commit.parent_count() > 1 {
            return Err(anyhow::anyhow!(
                "Commit {} is a merge commit — squash can't combine merge commits.",
                &commit.id().to_string()[..7]
            ));
        }
        summaries.push(commit.summary().unwrap_or("(no message)").to_string());
        oldest_oid = commit.id();
        commit = commit.parent(0).with_context(|| {
            format!("Cannot squash {} commits — only {} ancestor(s) exist", n, i)
        })?;
    }

    // Seed the squashed message from the combined summaries; let the user edit.
    let initial = summaries.join("\n");
    let message = match gate.prompt(&GateType::EditableText {
        initial,
        label: format!("Squashed commit message ({} commits):", n),
    })? {
        GateResponse::TextProvided(m) if !m.trim().is_empty() => m,
        _ => {
            return crate::ux::canceled();
        }
    };

    let squash = PipelineStep::SquashLastN { n, message };

    // PushedCommitRewrite (S33/S40): if the oldest squashed commit is published,
    // squashing rewrites shared history → escalate + force-push with lease.
    let branch = current_branch_name(repo);
    if branch != "HEAD"
        && crate::git_ops::commit_is_pushed(repo, &config.remote, &branch, oldest_oid)
    {
        return escalate_pushed_rewrite(repo, vec![squash], &branch, config, gate);
    }
    Ok(vec![squash])
}

/// Shared PushedCommitRewrite escalation (S33): a type-to-confirm on the branch
/// name, and on confirm append a force-push pinned to the fetched tracking oid
/// (`--force-with-lease=<branch>:<oid>`). Used by amend + squash on a published
/// commit, and by the Sync-rebase completion.
fn escalate_pushed_rewrite(
    repo: &git2::Repository,
    mut steps: Vec<PipelineStep>,
    branch: &str,
    config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<Vec<PipelineStep>> {
    println!("  {}", crate::copy::PUSHED_REWRITE_WARNING);
    let ctx = crate::context::build_context(repo)
        .unwrap_or_else(|_| crate::context::RepoContext::empty());
    let op_for_explain = ScopedWriteOp::ForcePush {
        remote: config.remote.clone(),
    };
    let resp = gate.prompt(&GateType::TypeToConfirm {
        expected: branch.to_string(),
        message: crate::copy::pushed_rewrite_confirm(branch),
        explanation: Some(crate::explain::explain(&op_for_explain)),
        danger: crate::danger::intent_danger(&op_for_explain, &ctx),
    })?;
    if !matches!(resp, GateResponse::Confirmed) {
        return crate::ux::canceled();
    }
    // Pin the lease to the tracking oid we have RIGHT NOW (TOCTOU hardening).
    let expect_oid = repo
        .find_reference(&format!("refs/remotes/{}/{}", config.remote, branch))
        .ok()
        .and_then(|r| r.target())
        .map(|o| o.to_string());
    steps.push(PipelineStep::ForcePush {
        remote: config.remote.clone(),
        branch: branch.to_string(),
        expect_oid,
    });
    Ok(steps)
}

/// When the pre-check found we're in detached HEAD and the user's intent
/// required writes, offer a path forward instead of just erroring.
/// Outcome of the detached-HEAD rescue, telling `run_agent` whether to resume
/// the user's original intent. `Resume` = now on a real branch, safe to continue
/// (so "commit this" actually commits — M-13). `Done` = switched away or
/// cancelled; nothing left to do.
enum RescueOutcome {
    Resume,
    Done,
}

async fn handle_detached_head_rescue(
    repo: &mut git2::Repository,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<RescueOutcome> {
    let current_sha = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .map(|o| o.to_string()[..7].to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // S21: how many commits would be stranded (reachable from HEAD, on no
    // branch) if the user switched away without branching first.
    let stranded = match crate::context::detect_situation(repo) {
        crate::commands::RepoSituation::DetachedHead { stranded_commits } => stranded_commits,
        _ => 0,
    };

    println!();
    println!("⚠  You are in DETACHED HEAD state.");
    println!(
        "   Currently at commit {} — not on any branch.",
        current_sha
    );
    if stranded > 0 {
        println!(
            "   You have {} commit(s) here that are on NO branch — switching away now would strand them.",
            stranded
        );
    }
    println!("   Any commit you make here will be orphaned (lose-able) unless you save it.");
    println!();

    let resp = gate.prompt(&GateType::Choose {
        message: "What would you like to do?".to_string(),
        // Branch-here is listed FIRST (index 0) — the anxious user's exit (S21).
        options: vec![
            "Create a branch HERE to save work".to_string(),
            "Switch back to your previous branch (no changes made)".to_string(),
            "Cancel".to_string(),
        ],
    })?;

    match resp {
        GateResponse::Chose(0) => {
            // Route the name prompt through the gate (not a raw Text::new) so the
            // rescue is driveable by ScriptedGate/AutoConfirmGate and the M-13
            // resume path is unit-testable.
            let name = match gate.prompt(&GateType::EditableText {
                // A sensible default the user can accept or overwrite — also lets
                // scripted/non-interactive runs proceed with a valid name.
                initial: "rescue-work".to_string(),
                label: "New branch name:".to_string(),
            })? {
                GateResponse::TextProvided(n) => n,
                _ => {
                    return crate::ux::canceled();
                }
            };
            let name = name.trim().to_string();
            // Resolve to a valid branch name (sanitize if the user's is invalid).
            let final_name = if let Err(r) = validate_branch_name(&name) {
                let suggestion = sanitize_branch_name(&name);
                println!(
                    "  '{}' isn't a valid branch name ({}). Using '{}'.",
                    name, r, suggestion
                );
                suggestion
            } else {
                name
            };
            let git_ops = Git2Operations;
            git_ops.create_branch(repo, &final_name)?;
            git_ops.checkout(repo, &final_name)?;
            println!("  ✓ Created and switched to branch '{}'", final_name);

            // "Create a branch HERE to save work" means save the UNCOMMITTED work
            // too. Stage the current changes now so the resumed intent (typically
            // "commit this") actually lands the edit instead of blocking on
            // "Nothing staged to commit" (M-13). No-op on a clean tree.
            if working_tree_has_changes(repo) {
                git_ops.add_all(repo)?;
                println!("  ✓ Staged your current changes on '{}'", final_name);
            }

            // Now on a real branch — resume the user's original intent so a
            // "commit this" actually commits here (M-13).
            Ok(RescueOutcome::Resume)
        }
        GateResponse::Chose(1) => {
            // S21 guard: refuse to switch away while commits would be stranded —
            // steer back to branch-here rather than silently losing them.
            if stranded > 0 {
                return Err(anyhow::anyhow!(
                    "Won't switch away — {} commit(s) here are on no branch and would be stranded. \
                     Re-run and choose \"Create a branch HERE\" first to keep them.",
                    stranded
                ));
            }
            // Try to switch to a sensible default branch.
            for fallback in &["main", "master"] {
                let git_ops = Git2Operations;
                if git_ops.checkout(repo, fallback).is_ok() {
                    println!("  ✓ Switched to '{}'", fallback);
                    // User chose to move AWAY from the detached commit — the
                    // original intent (e.g. "commit this") no longer applies here.
                    return Ok(RescueOutcome::Done);
                }
            }
            Err(anyhow::anyhow!(
                "Could not find 'main' or 'master' to switch back to. Manually: gitrust \"checkout <branch>\""
            ))
        }
        _ => crate::ux::canceled(),
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
        if matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\x7f') || (ch as u32) < 0x20 {
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
    let trimmed = out
        .trim_matches(|c| matches!(c, '-' | '.' | '/'))
        .to_string();
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
        P::Push {
            remote,
            branch,
            set_upstream,
        } => {
            let suffix = if *set_upstream { " (upstream set)" } else { "" };
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
        P::Reset { mode, .. } => Some(format!("→ Reset ({:?})", mode).to_lowercase()),
        P::Undo {
            undone_summary,
            undone_sha,
            ..
        } => Some(format!(
            "→ Undid commit {} \"{}\" (files re-staged)",
            undone_sha, undone_summary
        )),
        P::PullRebase { remote, branch } => Some(format!("→ Rebased onto {}/{}", remote, branch)),
        P::ForcePush { remote, branch, .. } => Some(format!(
            "→ Force-pushed to {}/{} (with-lease)",
            remote, branch
        )),
        P::DiscardLocal { remote, branch } => Some(format!(
            "→ Reset local to {}/{} — any unpushed work is gone (recoverable via reflog)",
            remote, branch
        )),
        P::Merge { source, .. } => Some(format!("→ Merged {} (merge commit recorded)", source)),
        P::Rebase { onto } => Some(format!("→ Rebased onto {}", onto)),
        P::MergeAbort => Some("→ Merge aborted — back to your pre-merge state".to_string()),
        P::RebaseContinue => Some("→ Rebase continued".to_string()),
        P::RebaseSkip => Some("→ Skipped the conflicting commit".to_string()),
        P::RebaseAbort => Some("→ Rebase aborted — back to the original HEAD".to_string()),
        P::CherryPickContinue => Some("→ Cherry-pick continued".to_string()),
        P::CherryPickAbort => Some("→ Cherry-pick aborted".to_string()),
        P::RevertContinue => Some("→ Revert continued".to_string()),
        P::RevertAbort => Some("→ Revert aborted".to_string()),
        P::Amend { .. } => Some("→ Amended the last commit".to_string()),
        P::Revert { oid } => {
            let short = if oid.len() >= 7 { &oid[..7] } else { oid };
            Some(format!("→ Reverted {} (inverse commit added)", short))
        }
        P::Unstage { paths } => Some(format!("→ Unstaged {} path(s)", paths.len())),
        P::SetUpstream { remote, branch } => {
            Some(format!("→ Upstream set to {}/{}", remote, branch))
        }
        P::SquashLastN { n, .. } => Some(format!("→ Squashed the last {} commits into one", n)),
        P::Clean { .. } => {
            Some("→ Deleted untracked files (backed up to the trash-net)".to_string())
        }
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
        let n = crate::ux::prompt_result(
            Text::new("Your name:")
                .with_help_message("Saved to ~/.gitconfig · Esc to cancel")
                .prompt(),
            "Name prompt",
        )?;
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
        let e = crate::ux::prompt_result(
            Text::new("Your email:")
                .with_help_message("Saved to ~/.gitconfig · Esc to cancel")
                .prompt(),
            "Email prompt",
        )?;
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

/// Drop git comment lines (those starting with `#`, after optional leading
/// whitespace) and trim the result — mirrors git's default `core.commentChar`
/// cleanup so a MERGE_MSG's "# Conflicts:" block never reaches the commit (M-12).
fn strip_comment_lines(s: &str) -> String {
    s.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Get the final commit message. GiTrust never drafts one: a `Provided`
/// message (quoted in the query) is offered for editing, a mid-merge commit
/// offers git's own MERGE_MSG, and otherwise the user is ASKED to write it.
///
/// Returns `Ok("")` when the user cancels (callers treat that as "aborted").
/// An empty message that was *submitted* — including `--yes`, which can only
/// echo the empty prompt — is an error, so a scripted run without a message
/// fails loudly instead of committing with a made-up one.
async fn resolve_or_generate_commit_message(
    repo: &git2::Repository,
    message: CommitMessage,
    _include_workdir: bool,
    _config: &Config,
    gate: &dyn ConfirmationGate,
) -> anyhow::Result<String> {
    let initial = match message {
        CommitMessage::Provided(m) => m,
        CommitMessage::Draft(_) => {
            // Mid-merge, git has already prepared a MERGE_MSG — offer it so the
            // merge commit reads conventionally (S17).
            if repo.state() == git2::RepositoryState::Merge {
                std::fs::read_to_string(repo.path().join("MERGE_MSG"))
                    .ok()
                    // Strip git's comment lines (e.g. the "# Conflicts:" block a
                    // conflicted merge appends). git drops these at commit time via
                    // core.commentChar; since we read MERGE_MSG and commit it
                    // ourselves, we must strip them or they leak into the message
                    // (M-12).
                    .map(|s| strip_comment_lines(&s))
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
    };

    let label = if initial.is_empty() {
        crate::copy::COMMIT_MESSAGE_ASK
    } else {
        "Commit message (edit or accept):"
    };
    let response = gate.prompt(&GateType::EditableText {
        initial: initial.clone(),
        label: label.to_string(),
    })?;

    let msg = match response {
        GateResponse::TextProvided(msg) => msg,
        GateResponse::Confirmed => initial,
        GateResponse::Aborted => return Ok(String::new()),
        _ => initial,
    };
    if msg.trim().is_empty() {
        anyhow::bail!("{}", crate::copy::COMMIT_MESSAGE_REQUIRED);
    }
    Ok(msg)
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
        PipelineStep::Merge { .. } => "git merge",
        PipelineStep::Rebase { .. } => "git rebase",
        PipelineStep::MergeAbort => "git merge --abort",
        PipelineStep::RebaseContinue => "git rebase --continue",
        PipelineStep::RebaseSkip => "git rebase --skip",
        PipelineStep::RebaseAbort => "git rebase --abort",
        PipelineStep::CherryPickContinue => "git cherry-pick --continue",
        PipelineStep::CherryPickAbort => "git cherry-pick --abort",
        PipelineStep::RevertContinue => "git revert --continue",
        PipelineStep::RevertAbort => "git revert --abort",
        PipelineStep::Amend { .. } => "git commit --amend",
        PipelineStep::Revert { .. } => "git revert --no-edit",
        PipelineStep::Unstage { .. } => "git restore --staged",
        PipelineStep::SetUpstream { .. } => "git branch --set-upstream-to",
        PipelineStep::SquashLastN { n, .. } => {
            // Leak a small static-ish string; this is display-only. Use a match
            // on common N to avoid allocation lifetime issues in a &str return.
            match n {
                1 => "git reset --soft HEAD~1 && git commit",
                2 => "git reset --soft HEAD~2 && git commit",
                3 => "git reset --soft HEAD~3 && git commit",
                _ => "git reset --soft HEAD~N && git commit",
            }
        }
        PipelineStep::Clean { .. } => "git clean -fd",
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
    fn test_strip_comment_lines_drops_conflicts_block() {
        // M-12: a conflicted MERGE_MSG carries a "# Conflicts:" comment block
        // that must not leak into the commit message.
        let merge_msg = "Merge branch 'feature'\n\n# Conflicts:\n#\tapp.txt\n";
        assert_eq!(strip_comment_lines(merge_msg), "Merge branch 'feature'");
        // A normal message is untouched (aside from trimming).
        assert_eq!(strip_comment_lines("feat: add auth\n"), "feat: add auth");
        // Indented comment lines are also dropped (git trims leading ws first).
        assert_eq!(
            strip_comment_lines("subject\n  # note\nbody"),
            "subject\nbody"
        );
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
        assert_eq!(
            sanitize_branch_name("Big Feature  Name"),
            "Big-Feature-Name"
        );
        assert_eq!(sanitize_branch_name("foo..bar"), "foo.bar");
        assert_eq!(sanitize_branch_name("-leading-"), "leading");
        assert_eq!(sanitize_branch_name("feat/login bug"), "feat/login-bug");
        // Empty / all-junk inputs become "branch".
        assert_eq!(sanitize_branch_name(""), "branch");
        assert_eq!(sanitize_branch_name("---"), "branch");
    }

    #[test]
    fn test_step_name_coverage() {
        assert_eq!(step_name(&PipelineStep::Add { files: vec![] }), "git add");
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
        let result = resolve_commit_message(
            CommitMessage::Provided("feat: my feature".to_string()),
            &gate,
        )
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
            .prompt(&GateType::FileMultiSelect {
                classification,
                show_all: false,
            })
            .unwrap();
        match result {
            GateResponse::FilesSelected(files) => {
                assert_eq!(files, vec!["src/login.rs".to_string()]);
            }
            _ => panic!("Expected FilesSelected"),
        }
    }
}

// =============================================================================
// Phase 12 — divergence reconciliation, outcome-based (real git in tempdirs,
// bare remote + clones — no network). The centerpiece is the two-clone diverged
// repro ending 0/0 ahead-behind with both changes present.
// =============================================================================
#[cfg(test)]
mod phase12_reconcile_tests {
    use super::*;
    use crate::commands::{IntentType, RepoSituation, ScopedWriteOp};
    use crate::config::Config;
    use crate::confirm::ScriptedGate;
    use crate::validation::{situation_gate, Redirect, SituationVerdict};
    use git2::Repository;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed:\n{}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn set_identity(dir: &Path, who: &str) {
        git(dir, &["config", "user.name", who]);
        git(dir, &["config", "user.email", &format!("{}@test.com", who)]);
    }

    fn clone_from(remote: &Path) -> TempDir {
        let dst = TempDir::new().unwrap();
        git(
            &std::env::temp_dir(),
            &[
                "clone",
                remote.to_str().unwrap(),
                dst.path().to_str().unwrap(),
            ],
        );
        dst
    }

    /// Bare remote with a `main` base commit already pushed from clone A.
    /// Returns (remote, clone_a).
    fn remote_with_base() -> (TempDir, TempDir) {
        let remote = TempDir::new().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main", "."]);
        let a = clone_from(remote.path());
        set_identity(a.path(), "A");
        fs::write(a.path().join("base.txt"), "base\n").unwrap();
        git(a.path(), &["add", "."]);
        git(a.path(), &["commit", "-m", "base"]);
        git(a.path(), &["push", "origin", "main"]);
        (remote, a)
    }

    /// Same-branch divergence on `main`: clone B advances the remote; clone A
    /// commits locally then fetches. `conflict` controls whether both edits hit
    /// the same file+line. Returns (remote, clone_a) — keep both alive.
    fn diverged_same_branch(conflict: bool) -> (TempDir, TempDir) {
        let (remote, a) = remote_with_base();

        // Clone B advances the remote.
        let b = clone_from(remote.path());
        set_identity(b.path(), "B");
        if conflict {
            fs::write(b.path().join("base.txt"), "B side\n").unwrap();
        } else {
            fs::write(b.path().join("b.txt"), "from b\n").unwrap();
        }
        git(b.path(), &["add", "."]);
        git(b.path(), &["commit", "-m", "b change"]);
        git(b.path(), &["push", "origin", "main"]);

        // Clone A commits locally (not pushed).
        if conflict {
            fs::write(a.path().join("base.txt"), "A side\n").unwrap();
        } else {
            fs::write(a.path().join("a.txt"), "from a\n").unwrap();
        }
        git(a.path(), &["add", "."]);
        git(a.path(), &["commit", "-m", "a change"]);

        // A fetches so its tracking ref reflects the divergence.
        git(a.path(), &["fetch", "origin"]);
        (remote, a)
    }

    // Centerpiece: two-clone diverged repro, choose Rebase → 0/0 and both
    // changes present.
    #[tokio::test]
    async fn diverged_rebase_lands_zero_zero_with_both_changes() {
        let (_remote, a) = diverged_same_branch(false);
        let mut repo = Repository::open(a.path()).unwrap();

        // Detection + gate route.
        let sit = crate::context::detect_situation(&repo);
        assert!(
            matches!(
                sit,
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1
                }
            ),
            "expected Diverged{{1,1}}, got {:?}",
            sit
        );
        let push_intent = IntentType::ScopedWrite(ScopedWriteOp::Push {
            remote: "origin".into(),
        });
        assert!(matches!(
            situation_gate(&sit, &push_intent),
            SituationVerdict::Redirect(Redirect::ReconcileDiverged {
                ahead: 1,
                behind: 1
            })
        ));

        // Drive the flow choosing Rebase (option 0).
        let cfg = Config::default();
        let gate = ScriptedGate::new(0);
        reconcile_flow(&mut repo, ReconcileMode::SameBranch, 1, 1, &cfg, &gate)
            .await
            .expect("reconcile should succeed");

        let (ahead, behind) = ahead_behind_vs(&repo, "origin/main");
        assert_eq!((ahead, behind), (0, 0), "expected 0/0 after rebase+push");
        assert!(a.path().join("a.txt").exists(), "local change missing");
        assert!(a.path().join("b.txt").exists(), "remote change missing");
    }

    // Merge path → the new tip records 2 parents; both changes present; 0/0.
    #[tokio::test]
    async fn diverged_merge_records_two_parents() {
        let (_remote, a) = diverged_same_branch(false);
        let mut repo = Repository::open(a.path()).unwrap();
        let cfg = Config::default();
        let gate = ScriptedGate::new(1); // Merge
        reconcile_flow(&mut repo, ReconcileMode::SameBranch, 1, 1, &cfg, &gate)
            .await
            .expect("merge reconcile should succeed");

        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent_count(), 2, "merge commit should have 2 parents");
        let (ahead, behind) = ahead_behind_vs(&repo, "origin/main");
        assert_eq!((ahead, behind), (0, 0), "expected 0/0 after merge+push");
        assert!(a.path().join("a.txt").exists());
        assert!(a.path().join("b.txt").exists());
    }

    // Conflicting rebase pauses: Err (nonzero) with the calm handoff message,
    // repo transitions to MidRebase, and the snapshot is preserved.
    #[tokio::test]
    async fn diverged_rebase_conflict_pauses_and_preserves_snapshot() {
        let (_remote, a) = diverged_same_branch(true);
        let mut repo = Repository::open(a.path()).unwrap();
        let pre_head = repo.head().unwrap().target().unwrap().to_string();

        let cfg = Config::default();
        let gate = ScriptedGate::new(0); // Rebase → conflicts
        let result = reconcile_flow(&mut repo, ReconcileMode::SameBranch, 1, 1, &cfg, &gate).await;

        // Nonzero exit (Err), calm message, no anyhow chain spew.
        let err = result.expect_err("conflicting rebase must pause with a nonzero exit");
        let msg = format!("{}", err);
        assert!(
            msg.contains("Paused on conflicts") && msg.contains("abort rebase"),
            "expected the calm handoff, got: {msg}"
        );
        // M-08 Defect 1: the conflict count must be REAL, not "0 file(s)". The
        // conflict is written by the git subprocess, so the count is only right
        // if the index is reloaded from disk before counting.
        assert!(
            msg.contains("in 1 file(s)"),
            "expected the real conflicted-file count (1), got: {msg}"
        );

        // In-progress rebase.
        assert!(
            matches!(
                crate::context::detect_situation(&repo),
                RepoSituation::MidRebase { .. }
            ),
            "expected MidRebase after conflict"
        );

        // Snapshot preserved — find the one at the pre-reconcile tip, then clean up.
        use crate::recovery::{FileRecoveryManager, RecoveryManager};
        let mgr = FileRecoveryManager;
        let snaps = mgr.list_snapshots().unwrap();
        let mine = snaps
            .into_iter()
            .find(|s| s.head_oid.as_deref() == Some(pre_head.as_str()))
            .expect("snapshot for the paused pipeline should be preserved on disk");
        assert_eq!(
            crate::recovery::recovery_plan(
                repo.state(),
                &mine,
                repo.head().ok().and_then(|h| h.target())
            ),
            crate::recovery::RecoveryDecision::AdviseGitAbort,
            "--recover should defer to git while the rebase is in progress"
        );
        mgr.delete_snapshot_by_id(&mine.pipeline_id).unwrap();
    }

    // Amendment 2: degenerate counts skip the ChoiceGate entirely.
    #[tokio::test]
    async fn ahead_only_fast_forwards_without_gate() {
        // A is behind origin/main only (fast-forwardable): B advances remote, A
        // has no local commit.
        let (remote, a) = remote_with_base();
        let b = clone_from(remote.path());
        set_identity(b.path(), "B");
        fs::write(b.path().join("b.txt"), "from b\n").unwrap();
        git(b.path(), &["add", "."]);
        git(b.path(), &["commit", "-m", "b"]);
        git(b.path(), &["push", "origin", "main"]);
        git(a.path(), &["fetch", "origin"]);

        let mut repo = Repository::open(a.path()).unwrap();
        // ahead == 0, behind == 1 (a fast-forward).
        assert_eq!(ahead_behind_vs(&repo, "origin/main"), (0, 1));

        let cfg = Config::default();
        let gate = ScriptedGate::new(0);
        reconcile_flow(&mut repo, ReconcileMode::SameBranch, 0, 1, &cfg, &gate)
            .await
            .expect("fast-forward should succeed");

        assert!(
            !gate.choice_shown.get(),
            "the gate must not render for a pure fast-forward"
        );
        assert_eq!(
            ahead_behind_vs(&repo, "origin/main"),
            (0, 0),
            "should be up to date after FF"
        );
        assert!(a.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn behind_zero_skips_gate() {
        // A is ahead only (behind == 0): local commit, remote unchanged.
        let (_remote, a) = remote_with_base();
        fs::write(a.path().join("a.txt"), "from a\n").unwrap();
        git(a.path(), &["add", "."]);
        git(a.path(), &["commit", "-m", "a"]);
        git(a.path(), &["fetch", "origin"]);

        let mut repo = Repository::open(a.path()).unwrap();
        assert_eq!(ahead_behind_vs(&repo, "origin/main"), (1, 0));

        let cfg = Config::default();
        let gate = ScriptedGate::new(0);
        reconcile_flow(&mut repo, ReconcileMode::SameBranch, 1, 0, &cfg, &gate)
            .await
            .expect("ahead-only push should succeed");

        assert!(
            !gate.choice_shown.get(),
            "the gate must not render when behind==0"
        );
        assert_eq!(
            ahead_behind_vs(&repo, "origin/main"),
            (0, 0),
            "our commit should be pushed"
        );
    }

    // Sync via origin/HEAD resolves the default branch.
    #[test]
    fn sync_default_branch_resolves_via_origin_head() {
        let (_remote, a) = remote_with_base();
        let repo = Repository::open(a.path()).unwrap();
        assert_eq!(
            crate::git_ops::default_branch(&repo, "origin").as_deref(),
            Some("main")
        );
    }

    /// Current branch = `feature` (pushed to origin/feature, so own upstream is
    /// in sync), diverged from `origin/main` (which clone B advanced). Returns
    /// (remote, clone_a). HEAD is on `feature`, fetched.
    fn feature_diverged_from_main() -> (TempDir, TempDir) {
        let (remote, a) = remote_with_base();
        git(a.path(), &["checkout", "-b", "feature"]);
        fs::write(a.path().join("feature.txt"), "feat\n").unwrap();
        git(a.path(), &["add", "."]);
        git(a.path(), &["commit", "-m", "feat1"]);
        git(a.path(), &["push", "-u", "origin", "feature"]);

        let b = clone_from(remote.path());
        set_identity(b.path(), "B");
        fs::write(b.path().join("main2.txt"), "main2\n").unwrap();
        git(b.path(), &["add", "."]);
        git(b.path(), &["commit", "-m", "main2"]);
        git(b.path(), &["push", "origin", "main"]);

        git(a.path(), &["fetch", "origin"]);
        (remote, a)
    }

    // Phase 13 (Q4) — closes Phase 12's deferred loose end. A Sync+Rebase of a
    // PUBLISHED branch now completes the push in-band: the PushedCommitRewrite
    // escalation (TypeToConfirm, which ScriptedGate confirms) runs, and
    // origin/<feature> advances to the rebased tip via force-with-lease. (Was
    // previously asserted NOT to push — Phase 12's intentional gap.)
    #[tokio::test]
    async fn sync_rebase_completes_forcepush_when_published() {
        let (_remote, a) = feature_diverged_from_main();
        let mut repo = Repository::open(a.path()).unwrap();
        let before = repo
            .refname_to_id("refs/remotes/origin/feature")
            .unwrap()
            .to_string();

        let cfg = Config::default();
        let gate = ScriptedGate::new(0); // Rebase, then confirm the TypeToConfirm
        reconcile_flow(&mut repo, ReconcileMode::CrossBranchSync, 0, 0, &cfg, &gate)
            .await
            .expect("cross-branch rebase + force-push should succeed");

        // Re-read the remote tracking ref after the push.
        git(a.path(), &["fetch", "origin"]);
        let after = Repository::open(a.path())
            .unwrap()
            .refname_to_id("refs/remotes/origin/feature")
            .unwrap()
            .to_string();
        assert_ne!(
            before, after,
            "Sync+Rebase of a published branch must force-push the rebased tip (Phase 13)"
        );
        // And the pushed tip must match local HEAD (the rebased commit).
        let local_head = repo.head().unwrap().target().unwrap().to_string();
        assert_eq!(
            after, local_head,
            "remote feature should now point at the rebased local HEAD"
        );
    }

    // Amendment 1: Sync + Merge DOES push when the branch is not behind its own
    // upstream (own_behind == 0) — the merge is a new commit on top, a safe
    // fast-forward for origin/feature.
    #[tokio::test]
    async fn sync_merge_pushes_when_own_upstream_not_behind() {
        let (_remote, a) = feature_diverged_from_main();
        let mut repo = Repository::open(a.path()).unwrap();
        let before = repo
            .refname_to_id("refs/remotes/origin/feature")
            .unwrap()
            .to_string();

        let cfg = Config::default();
        let gate = ScriptedGate::new(1); // Merge
        reconcile_flow(&mut repo, ReconcileMode::CrossBranchSync, 0, 0, &cfg, &gate)
            .await
            .expect("cross-branch merge should succeed");

        // origin/feature advanced (the merge commit was pushed), and the new tip
        // has 2 parents (feature + main).
        let after = repo
            .refname_to_id("refs/remotes/origin/feature")
            .unwrap()
            .to_string();
        assert_ne!(
            before, after,
            "Sync+Merge should push when own upstream isn't behind"
        );
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent_count(), 2, "merge should record 2 parents");
    }
}

// =============================================================================
// Phase 13 — repair vocabulary + the undo disambiguation gate (S32–S40, P6).
// All assertions outcome-based: gate-invocation flags, tree/parent oids,
// subprocess args (via the emitted PipelineStep), classifier tokens.
// =============================================================================
#[cfg(test)]
mod phase13_repair_tests {
    use super::*;
    use crate::commands::{CommitMessage, PipelineStep, ResetMode, ScopedWriteOp};
    use crate::confirm::ScriptedGate;
    use git2::Repository;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed:\n{}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with `n` linear commits on `main`, identity set. Returns the repo
    /// and its dir.
    fn repo_with_commits(n: usize) -> (Repository, TempDir) {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-b", "main", "."]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@t.com"]);
        for i in 0..n {
            std::fs::write(dir.path().join(format!("f{i}.txt")), format!("v{i}\n")).unwrap();
            git(dir.path(), &["add", "."]);
            git(dir.path(), &["commit", "-m", &format!("c{i}")]);
        }
        let repo = Repository::open(dir.path()).unwrap();
        (repo, dir)
    }

    // -- Cancel semantics (ux.rs): backing out is Err(Canceled) → exit 1, never
    //    a silent Ok (exit 0) for a not-done operation.
    #[test]
    fn escaping_the_cleanup_gate_is_a_cancel_and_it_has_no_abort_row() {
        let (repo, d) = repo_with_commits(1);
        std::fs::write(d.path().join("f0.txt"), "edited\n").unwrap();
        // One tracked change → exactly one option; backing out is Esc, not a row.
        let gate = ScriptedGate::new(0);
        let _ = build_cleanup_intent(&repo, &gate).unwrap();
        assert_eq!(gate.last_choice_options.get(), 1);
        assert!(gate
            .last_choice_labels
            .borrow()
            .iter()
            .all(|l| !l.contains("Abort")));
        let esc = crate::confirm::MockGate {
            response: GateResponse::Aborted,
        };
        let err = build_cleanup_intent(&repo, &esc).unwrap_err();
        assert!(crate::ux::is_canceled(&err), "got: {err}");
    }

    #[test]
    fn escaping_the_undo_gate_is_a_cancel() {
        let (repo, _d) = repo_with_commits(2);
        let gate = crate::confirm::MockGate {
            response: GateResponse::Aborted,
        };
        let err = build_undo_intent(&repo, "undo", &gate).unwrap_err();
        assert!(crate::ux::is_canceled(&err), "got: {err}");
    }

    #[tokio::test]
    async fn declining_the_plan_is_a_cancel_and_changes_nothing() {
        let (mut repo, d) = repo_with_commits(1);
        std::fs::write(d.path().join("f0.txt"), "edited\n").unwrap();
        let gate = crate::confirm::MockGate {
            response: GateResponse::Aborted,
        };
        let steps = vec![PipelineStep::Add { files: vec![] }];
        let err = execute_pipeline(&mut repo, steps, &Config::default(), &gate)
            .await
            .unwrap_err();
        assert!(crate::ux::is_canceled(&err), "got: {err}");
        // Nothing was staged.
        let st = repo.statuses(None).unwrap();
        assert!(st
            .iter()
            .all(|e| !e.status().intersects(git2::Status::INDEX_MODIFIED)));
    }

    #[tokio::test]
    async fn nothing_to_commit_is_said_before_asking_for_a_message() {
        let (mut repo, _d) = repo_with_commits(1);
        let gate = ScriptedGate::new(0);
        for op in [
            ScopedWriteOp::AddAndCommit {
                message: CommitMessage::Draft(String::new()),
                files: crate::commands::FileSelection::All,
            },
            ScopedWriteOp::Commit {
                message: CommitMessage::Draft(String::new()),
            },
        ] {
            let err = build_scoped_steps(&mut repo, op, "commit", &Config::default(), &gate, false)
                .await
                .unwrap_err()
                .to_string();
            // Not "a commit message is required" — the message was never asked.
            assert!(err.starts_with("Nothing"), "{err}");
        }
    }

    #[test]
    fn invalid_branch_intro_names_only_what_this_input_broke() {
        assert_eq!(
            crate::copy::branch_invalid_intro("feat/Login Page (v2)"),
            "Git won't allow spaces or parentheses in branch names."
        );
        assert_eq!(
            crate::copy::branch_invalid_intro("fix:bug"),
            "Git won't allow ':' in branch names."
        );
    }

    #[tokio::test]
    async fn invalid_branch_name_is_fixed_up_not_rejected() {
        let (mut repo, _d) = repo_with_commits(1);
        // ScriptedGate answers an EditableText with its pre-filled text, i.e.
        // the user pressed Enter on the suggestion.
        let gate = ScriptedGate::new(0);
        let steps = build_scoped_steps(
            &mut repo,
            ScopedWriteOp::Branch {
                name: "feat/Login Page (v2)".into(),
            },
            "make me a new branch",
            &Config::default(),
            &gate,
            false,
        )
        .await
        .unwrap();
        assert!(
            matches!(&steps[..], [PipelineStep::Branch { name }] if name == "feat/Login-Page-v2"),
            "{:?}",
            steps
        );
    }

    // -- #1 (centerpiece): no undo phrase reaches Reset::Hard without the
    // ChoiceGate having offered all four options, and Discard additionally
    // requires a TypeToConfirm.
    #[test]
    fn undo_gate_offers_four_options_and_discard_needs_type_to_confirm() {
        let (repo, _d) = repo_with_commits(2);

        // Choice index 3 = Discard in the unpushed ordering.
        let gate = ScriptedGate::new(3);
        let steps = build_undo_intent(&repo, "undo", &gate).unwrap();

        assert!(
            gate.choice_shown.get(),
            "the ChoiceGate must have been shown"
        );
        assert_eq!(
            gate.last_choice_options.get(),
            4,
            "the undo gate must offer exactly four options"
        );
        assert!(
            gate.type_to_confirm_shown.get(),
            "Discard must require a TypeToConfirm before Reset::Hard is built"
        );
        // Discard resolves to a hard reset TO THE PARENT — not a target-less
        // reset-to-HEAD, which was the M-16 phantom no-op (commit survived).
        let parent = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent(0)
            .unwrap()
            .id()
            .to_string();
        assert!(
            matches!(
                steps.as_slice(),
                [PipelineStep::Reset {
                    mode: ResetMode::Hard,
                    to_oid: Some(oid),
                }] if *oid == parent
            ),
            "Discard must hard-reset to the parent oid {}, got {:?}",
            parent,
            steps
        );
    }

    // -- #1 companion: the other three choices never emit a hard reset.
    #[test]
    fn undo_gate_nondiscard_choices_never_hard_reset() {
        // 0 keep-staged → Undo (soft); 1 keep-in-files → Reset Mixed; 2 → Revert.
        for (idx, want) in [(0usize, "undo"), (1usize, "mixed"), (2usize, "revert")] {
            let (repo, _d) = repo_with_commits(2);
            let gate = ScriptedGate::new(idx);
            let steps = build_undo_intent(&repo, "undo", &gate).unwrap();
            assert_eq!(gate.last_choice_options.get(), 4, "still four options");
            assert!(
                !steps.iter().any(|s| matches!(
                    s,
                    PipelineStep::Reset {
                        mode: ResetMode::Hard,
                        ..
                    }
                )),
                "choice {} ({}) must not hard-reset: {:?}",
                idx,
                want,
                steps
            );
            match (idx, steps.as_slice()) {
                (0, [PipelineStep::Undo { .. }]) => {}
                // keep-in-files: mixed reset TO THE PARENT (Some oid), so the
                // commit is actually undone rather than reset-to-HEAD no-op'd.
                (
                    1,
                    [PipelineStep::Reset {
                        mode: ResetMode::Mixed,
                        to_oid: Some(_),
                    }],
                ) => {}
                (2, [PipelineStep::Revert { .. }]) => {}
                _ => panic!("choice {} produced unexpected steps {:?}", idx, steps),
            }
        }
    }

    // -- #11: multi-commit undo ("undo the last 3 commits") halts with guidance,
    // builds no gate, touches nothing.
    #[test]
    fn multi_commit_undo_halts_without_gate() {
        let (repo, _d) = repo_with_commits(3);
        let before = repo.head().unwrap().target().unwrap();
        let gate = ScriptedGate::new(3);
        let err = build_undo_intent(&repo, "undo the last 3 commits", &gate)
            .expect_err("multi-commit undo must halt");
        assert!(
            err.to_string()
                .contains("Multi-commit undo isn't supported"),
            "should return the guidance, got: {}",
            err
        );
        assert!(!gate.choice_shown.get(), "no gate for multi-commit undo");
        assert_eq!(
            repo.head().unwrap().target().unwrap(),
            before,
            "HEAD must be untouched"
        );
    }

    // -- #2: --yes refuses the undo ChoiceGate (inherits the reconcile precedent).
    #[test]
    fn undo_under_yes_refuses() {
        let (repo, _d) = repo_with_commits(2);
        let gate = crate::confirm::AutoConfirmGate;
        let err = build_undo_intent(&repo, "undo", &gate)
            .expect_err("--yes must refuse the undo ChoiceGate");
        let msg = err.to_string();
        // Must point the user at interactive mode...
        assert!(
            msg.contains("interactively"),
            "refusal should point at interactive mode, got: {}",
            msg
        );
        // ...and must NOT misdescribe an undo as a rebase-vs-merge reconcile
        // (M-17 Defect 2). The refusal now echoes the actual gate question.
        assert!(
            !msg.contains("rebase vs merge") && !msg.contains("reconciliation strategy"),
            "undo refusal must not cite a reconciliation strategy, got: {}",
            msg
        );
    }

    // -- #3: when HEAD is pushed, the gate reorders Revert first and says why.
    #[test]
    fn undo_gate_reorders_revert_first_when_pushed() {
        let remote = TempDir::new().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main", "."]);
        let (repo, dir) = repo_with_commits(2);
        git(
            dir.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        git(dir.path(), &["push", "-u", "origin", "main"]);
        let repo = Repository::open(dir.path()).unwrap();

        let gate = ScriptedGate::new(0); // pick whatever is first
        let _ = build_undo_intent(&repo, "undo", &gate).unwrap();
        let labels = gate.last_choice_labels.borrow();
        assert!(
            labels
                .first()
                .map(|l| l.contains("revert"))
                .unwrap_or(false),
            "pushed HEAD must put Revert first, got {:?}",
            *labels
        );
    }

    // -- #6: SquashLastN(3) turns 3 commits into 1 with an identical tree oid.
    #[tokio::test]
    async fn squash_last_3_preserves_tree() {
        let (repo, _d) = repo_with_commits(4);
        let pre_tree = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .id();
        let pre_count = repo.revwalk_count();

        let cfg = Config::default();
        let gate = ScriptedGate::new(0);
        let steps = build_squash_step(&repo, 3, &cfg, &gate).await.unwrap();
        // Execute the squash step directly (git2 path — no hooks in this repo).
        let mut repo_mut = Repository::open(_d.path()).unwrap();
        for step in &steps {
            crate::git_ops::execute_step(&Git2Operations, &mut repo_mut, step).unwrap();
        }

        let post_tree = repo_mut
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .id();
        assert_eq!(pre_tree, post_tree, "squash must preserve the tree oid");
        let post_count = repo_mut.revwalk_count();
        assert_eq!(
            post_count,
            pre_count - 2,
            "3 commits collapse into 1 (net -2), got {} → {}",
            pre_count,
            post_count
        );
    }

    // -- #7a: Revert produces a commit whose tree equals the pre-target tree.
    #[tokio::test]
    async fn revert_tree_equals_pre_target() {
        let (repo, dir) = repo_with_commits(2);
        // Tree BEFORE the last commit (i.e. after reverting it we should match).
        let parent_tree = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent(0)
            .unwrap()
            .tree()
            .unwrap()
            .id();

        let gate = ScriptedGate::new(0);
        let steps = build_revert_step(&repo, String::new(), &gate).unwrap();
        let mut repo_mut = Repository::open(dir.path()).unwrap();
        for step in &steps {
            crate::git_ops::execute_step(&Git2Operations, &mut repo_mut, step).unwrap();
        }
        let post_tree = repo_mut
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .id();
        assert_eq!(
            parent_tree, post_tree,
            "reverting the last commit should restore the parent tree"
        );
    }

    // -- #4 & #7b: hook parity. A failing pre-commit BLOCKS amend (Q3) but a
    // revert SUCCEEDS (git revert doesn't run pre-commit) — the deliberate
    // contrast.
    #[test]
    fn amend_blocked_but_revert_succeeds_under_failing_pre_commit() {
        use std::os::unix::fs::PermissionsExt;
        let (_r, dir) = repo_with_commits(2);
        let hooks = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho blocked >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut repo = Repository::open(dir.path()).unwrap();
        let head_before = repo.head().unwrap().target().unwrap();

        // Amend routes through system git (hooks=auto) → HookRejected, HEAD unchanged.
        let exec = crate::executor::DefaultExecutor {
            git_ops: Git2Operations,
            hooks_mode: "auto".to_string(),
        };
        let amend_err = crate::executor::StepExecutor::execute(
            &exec,
            &mut repo,
            &PipelineStep::Amend {
                message: Some("reworded".into()),
                include_staged: false,
            },
        )
        .expect_err("amend must be blocked by the failing pre-commit hook");
        assert!(
            amend_err
                .downcast_ref::<crate::git_ops::ClassifiedFailure>()
                .map(|c| matches!(c.class, crate::git_ops::FailureClass::HookRejected { .. }))
                .unwrap_or(false),
            "amend failure should be HookRejected, got {:?}",
            amend_err
        );
        assert_eq!(
            repo.head().unwrap().target().unwrap(),
            head_before,
            "amend must leave HEAD unchanged when the hook blocks"
        );

        // Revert SUCCEEDS despite the same failing pre-commit — git revert does
        // not run pre-commit. (Deliberate contrast to the amend assertion above.)
        let head_oid = repo.head().unwrap().target().unwrap().to_string();
        crate::git_ops::execute_step(
            &Git2Operations,
            &mut repo,
            &PipelineStep::Revert { oid: head_oid },
        )
        .expect("revert should SUCCEED — git revert doesn't run pre-commit");
        assert_ne!(
            repo.head().unwrap().target().unwrap(),
            head_before,
            "revert must have added an inverse commit"
        );
    }

    // -- M-21: a hook-rejected commit whose recovery picker only *informs*
    // ("Show the hook output") must still exit NONZERO. The picker printing the
    // failure is not the same as fixing it — before the fix the informational
    // row swallowed the failure into Ok(()) and a scripting caller lost the
    // blocked-commit's nonzero status.
    #[tokio::test]
    async fn hook_rejected_commit_show_recovery_still_errors() {
        use std::os::unix::fs::PermissionsExt;
        let (_r, dir) = repo_with_commits(1);
        let hooks = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho LINT FAILED >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Stage a real change so the commit is non-empty and reaches the hook.
        std::fs::write(dir.path().join("f0.txt"), "changed\n").unwrap();
        git(dir.path(), &["add", "."]);

        let mut repo = Repository::open(dir.path()).unwrap();
        let head_before = repo.head().unwrap().target().unwrap();

        // choice 0 = "Show" (the informational recovery row), like the M-21 run.
        let gate = ScriptedGate::new(0);
        let cfg = Config::default(); // hooks = "auto" → commit shells out, hook runs
        let steps = vec![PipelineStep::Commit {
            message: "will be blocked".to_string(),
        }];
        let result = execute_pipeline(&mut repo, steps, &cfg, &gate).await;

        assert!(
            result.is_err(),
            "a hook-blocked commit must return Err even after the 'Show' recovery row — \
             informing the user is not resolving the failure (M-21)"
        );
        assert_eq!(
            repo.head().unwrap().target().unwrap(),
            head_before,
            "the blocked commit must not have moved HEAD"
        );
    }

    // -- M-21 companion: "Cancel" on the recovery picker also reports
    // NotResolved (so the failure still propagates) — the fix is not specific to
    // the "Show" row.
    #[tokio::test]
    async fn hook_rejected_commit_cancel_recovery_reports_not_resolved() {
        let gate = ScriptedGate::new(1); // index 1 = "Cancel" on the HookRejected picker
        let outcome = handle_classified_recovery(
            &mut repo_with_commits(1).0,
            &crate::git_ops::FailureClass::HookRejected {
                hook: Some("pre-commit".to_string()),
                output: "LINT FAILED".to_string(),
            },
            "commit",
            &Config::default(),
            &gate,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            RecoveryResult::NotResolved,
            "Cancel must not mark the failure resolved"
        );
    }

    // -- #5: amend on a PUSHED commit escalates (TypeToConfirm) and appends a
    // force-push pinned to the fetched tracking oid (--force-with-lease=<b>:<oid>).
    #[tokio::test]
    async fn amend_on_pushed_escalates_and_pins_lease() {
        let remote = TempDir::new().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main", "."]);
        let (_r, dir) = repo_with_commits(2);
        git(
            dir.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        git(dir.path(), &["push", "-u", "origin", "main"]);
        let repo = Repository::open(dir.path()).unwrap();
        let tracking = repo
            .refname_to_id("refs/remotes/origin/main")
            .unwrap()
            .to_string();

        let cfg = Config::default();
        let gate = ScriptedGate::new(0); // confirms the TypeToConfirm
        let steps = build_amend_step(&repo, Some("reworded".into()), false, &cfg, &gate)
            .await
            .unwrap();

        assert!(
            gate.type_to_confirm_shown.get(),
            "pushed amend must escalate with TypeToConfirm"
        );
        match steps.as_slice() {
            [PipelineStep::Amend { .. }, PipelineStep::ForcePush {
                expect_oid, branch, ..
            }] => {
                assert_eq!(branch, "main");
                assert_eq!(
                    expect_oid.as_deref(),
                    Some(tracking.as_str()),
                    "lease must be pinned to the fetched tracking oid (TOCTOU)"
                );
            }
            other => panic!("expected [Amend, ForcePush(pinned)], got {:?}", other),
        }
    }

    // -- #9: Unstage moves only the named paths out of the index; other staged
    // paths and file contents survive. It is NOT a reset.
    #[test]
    fn unstage_scoped_leaves_other_staged_files() {
        let (_r, dir) = repo_with_commits(1);
        // Stage two new files.
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "a.txt", "b.txt"]);
        let mut repo = Repository::open(dir.path()).unwrap();

        crate::git_ops::execute_step(
            &Git2Operations,
            &mut repo,
            &PipelineStep::Unstage {
                paths: vec!["a.txt".into()],
            },
        )
        .unwrap();

        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("a.txt"), 0).is_none(),
            "a.txt should be unstaged"
        );
        assert!(
            index.get_path(std::path::Path::new("b.txt"), 0).is_some(),
            "b.txt must remain staged (scoped unstage, not a reset)"
        );
        // File contents untouched on disk.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\n"
        );
    }

    // -- #10: no direct-undo token in the classifier — every undo phrase maps to
    // UndoIntent, never ScopedWriteOp::Undo.
    #[test]
    fn classifier_never_emits_direct_undo() {
        for phrase in [
            "undo",
            "undo last commit",
            "undo commit",
            "take back",
            "that was wrong",
            "undo that",
        ] {
            match crate::intent::try_canonical(phrase) {
                Some(IntentType::ScopedWrite(ScopedWriteOp::UndoIntent)) => {}
                Some(IntentType::ScopedWrite(ScopedWriteOp::Undo)) => {
                    panic!("'{}' must NOT map to the direct Undo op (P6)", phrase)
                }
                other => panic!("'{}' unexpected canonical mapping: {:?}", phrase, other),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 14 — forensic recovery + destructive-phrase routing (end-to-end).
    // -----------------------------------------------------------------------

    /// Init a repo with one commit and configured identity (`f.txt` = "base").
    fn init_repo_one_commit() -> (TempDir, Repository) {
        let (repo, dir) = repo_with_commits(0);
        std::fs::write(dir.path().join("f.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "base"]);
        let repo2 = Repository::open(dir.path()).unwrap();
        let _ = repo;
        (dir, repo2)
    }

    // Destructive-phrase routing (S62): "clean up" NEVER reaches a Clean/Reset
    // step without the ChoiceGate having been shown. ScriptedGate records it.
    #[tokio::test]
    async fn cleanup_phrase_requires_choice_gate_before_any_destroy() {
        let (dir, repo) = init_repo_one_commit();
        // Make something to clean so the gate isn't skipped by the degenerate guard.
        std::fs::write(dir.path().join("f.txt"), "modified\n").unwrap();
        std::fs::write(dir.path().join("junk.txt"), "untracked\n").unwrap();

        // "clean up" classifies to CleanupIntent (deterministic table, not LLM).
        match crate::intent::try_canonical("clean up") {
            Some(crate::commands::IntentType::ScopedWrite(ScopedWriteOp::CleanupIntent)) => {}
            other => panic!("'clean up' must map to CleanupIntent, got {:?}", other),
        }

        let gate = ScriptedGate::new(0); // pick the first option
        let steps = build_cleanup_intent(&repo, &gate).unwrap();
        assert!(
            gate.choice_shown.get(),
            "the ChoiceGate MUST be shown before any destructive step is built (S62)"
        );
        // Option 0 with both dirty tracked + untracked is "Discard tracked changes".
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, PipelineStep::Reset { .. } | PipelineStep::Clean { .. })),
            "a destructive step should be produced after the gate choice"
        );
    }

    // Degenerate guard: nothing to clean → no gate rendered, no steps.
    #[tokio::test]
    async fn cleanup_phrase_on_clean_tree_renders_no_gate() {
        let (_dir, repo) = init_repo_one_commit();
        let gate = ScriptedGate::new(0);
        let steps = build_cleanup_intent(&repo, &gate).unwrap();
        assert!(steps.is_empty(), "no steps on a pristine tree");
        assert!(
            !gate.choice_shown.get(),
            "no ChoiceGate should render when there is nothing to clean"
        );
    }

    // RecoveryBrowser under --yes is refused (Q2).
    #[tokio::test]
    async fn forensic_recover_refused_under_yes() {
        let (dir, _repo) = init_repo_one_commit();
        // Create a lost commit so the browser would have candidates.
        git(dir.path(), &["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("f.txt"), "feature\n").unwrap();
        git(dir.path(), &["commit", "-am", "feat: lost"]);
        git(dir.path(), &["checkout", "main"]);
        git(dir.path(), &["branch", "-D", "feature"]);

        let repo = Repository::open(dir.path()).unwrap();
        let cfg = crate::config::Config::default();
        let res = handle_forensic_recover(&repo, &cfg, &crate::confirm::AutoConfirmGate);
        assert!(
            res.is_err(),
            "--yes must refuse the RecoveryBrowser (never auto-pick a commit)"
        );
    }

    // CENTERPIECE end-to-end: delete a branch, forensic recover restores it to a
    // rescue/* branch at the exact oid, current HEAD byte-for-byte untouched.
    #[tokio::test]
    async fn forensic_recover_restores_rescue_branch_head_untouched() {
        let (dir, _repo) = init_repo_one_commit();
        git(dir.path(), &["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("f.txt"), "feature work\n").unwrap();
        git(dir.path(), &["commit", "-am", "feat: export csv"]);
        let lost_oid = Repository::open(dir.path())
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap();
        git(dir.path(), &["checkout", "main"]);
        git(dir.path(), &["branch", "-D", "feature"]);

        let repo = Repository::open(dir.path()).unwrap();
        let head_before = repo.head().unwrap().target().unwrap();
        let f_before = std::fs::read(dir.path().join("f.txt")).unwrap();

        let cfg = crate::config::Config::default();
        let gate = ScriptedGate::new(0); // pick the (most-recent) lost commit
        handle_forensic_recover(&repo, &cfg, &gate).unwrap();

        // A rescue/* branch now points at the exact lost oid...
        let rescue = repo
            .branches(Some(git2::BranchType::Local))
            .unwrap()
            .flatten()
            .find_map(|(b, _)| {
                let name = b.name().ok().flatten()?.to_string();
                if name.starts_with("rescue/") {
                    Some((name, b.get().target().unwrap()))
                } else {
                    None
                }
            })
            .expect("a rescue/* branch should have been created");
        assert_eq!(
            rescue.1, lost_oid,
            "rescue branch must point at the lost oid"
        );

        // ...and current HEAD + working tree are byte-for-byte untouched.
        let repo_after = Repository::open(dir.path()).unwrap();
        assert_eq!(
            repo_after.head().unwrap().target().unwrap(),
            head_before,
            "HEAD must be untouched by a forensic restore"
        );
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            f_before,
            "working tree must be byte-for-byte untouched"
        );
    }

    // -- M-13: branch-here rescue lands on a real branch and signals Resume, so
    // the caller re-dispatches the original intent (e.g. "commit this") instead
    // of dropping it. Before the fix the rescue returned unconditionally and the
    // user's commit was silently never made.
    #[tokio::test]
    async fn detached_rescue_branch_here_signals_resume_on_real_branch() {
        let (_repo, dir) = repo_with_commits(2);
        // Detach HEAD at the current tip.
        let head = Repository::open(dir.path())
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        git(dir.path(), &["checkout", &head]);
        let mut repo = Repository::open(dir.path()).unwrap();
        assert!(
            repo.head_detached().unwrap(),
            "setup: HEAD must be detached"
        );

        // Choice 0 = "Create a branch HERE"; ScriptedGate answers EditableText
        // with the default "rescue-work".
        let gate = ScriptedGate::new(0);
        let outcome = handle_detached_head_rescue(&mut repo, &gate)
            .await
            .expect("rescue should succeed");

        // It must tell the caller to RESUME the original intent...
        assert!(
            matches!(outcome, RescueOutcome::Resume),
            "branch-here must signal Resume so the original commit is not dropped (M-13)"
        );
        // ...and we must now be on a real branch, not detached.
        let repo_after = Repository::open(dir.path()).unwrap();
        assert!(
            !repo_after.head_detached().unwrap(),
            "after branch-here the repo must be on a real branch"
        );
        assert_eq!(
            repo_after.head().unwrap().shorthand(),
            Some("rescue-work"),
            "HEAD should be on the newly created rescue branch"
        );
    }

    // -- M-13 (the actual defect): branch-here rescue on a DIRTY detached HEAD
    // must STAGE the uncommitted edit, so the resumed "commit this" has something
    // to commit. Before the fix the rescue branched + switched but left the edit
    // unstaged, and the resumed commit blocked on "Nothing staged to commit" —
    // the user's work never landed.
    #[tokio::test]
    async fn detached_rescue_branch_here_stages_dirty_edit_for_resume() {
        let (_repo, dir) = repo_with_commits(2);
        // Detach at the tip, then make an uncommitted edit to a tracked file.
        let head = Repository::open(dir.path())
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        git(dir.path(), &["checkout", &head]);
        std::fs::write(dir.path().join("f0.txt"), "dirty edit\n").unwrap();

        let mut repo = Repository::open(dir.path()).unwrap();
        assert!(
            repo.head_detached().unwrap(),
            "setup: HEAD must be detached"
        );

        let gate = ScriptedGate::new(0); // "Create a branch HERE", accept default name
        let outcome = handle_detached_head_rescue(&mut repo, &gate)
            .await
            .expect("rescue should succeed");
        assert!(
            matches!(outcome, RescueOutcome::Resume),
            "branch-here must signal Resume"
        );

        // The edit must now be STAGED (index differs from HEAD), and there must be
        // no remaining unstaged worktree modification — so a resumed plain commit
        // (staged-only) actually captures it.
        let repo_after = Repository::open(dir.path()).unwrap();
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        let statuses = repo_after.statuses(Some(&mut opts)).unwrap();
        let f0 = statuses
            .iter()
            .find(|e| e.path() == Some("f0.txt"))
            .expect("f0.txt must appear in status");
        assert!(
            f0.status().intersects(
                git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_NEW
                    | git2::Status::INDEX_TYPECHANGE
            ),
            "the dirty edit must be STAGED after branch-here (M-13), got {:?}",
            f0.status()
        );
        assert!(
            !f0.status().intersects(git2::Status::WT_MODIFIED),
            "nothing should be left unstaged after the rescue stages the work"
        );
    }

    // Companion: switching AWAY (choice 1) from a clean detached HEAD ends the
    // run (Done) — there is no original intent to resume at the old location.
    #[tokio::test]
    async fn detached_rescue_switch_away_signals_done() {
        let (_repo, dir) = repo_with_commits(1);
        let head = Repository::open(dir.path())
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        git(dir.path(), &["checkout", &head]);
        let mut repo = Repository::open(dir.path()).unwrap();

        // Choice 1 = "Switch back". No stranded commits (HEAD == a branch tip),
        // so the S21 guard allows it.
        let gate = ScriptedGate::new(1);
        let outcome = handle_detached_head_rescue(&mut repo, &gate)
            .await
            .expect("switch-away should succeed");
        assert!(
            matches!(outcome, RescueOutcome::Done),
            "switching away must signal Done (nothing to resume)"
        );
    }
}

#[cfg(test)]
trait RevwalkCount {
    fn revwalk_count(&self) -> usize;
}

#[cfg(test)]
impl RevwalkCount for git2::Repository {
    fn revwalk_count(&self) -> usize {
        let mut rw = self.revwalk().unwrap();
        rw.push_head().unwrap();
        rw.count()
    }
}

/// Layer 1 with Kev: the decision paths, driven by a fixed distribution so no
/// server is needed. The safety-relevant cases: `--yes` never guesses when
/// Kev is unsure, destructive ops need the higher bar, missing arguments are
/// asked for (and refused under `--yes`), and the fast-path never calls Kev.
#[cfg(test)]
mod kev_layer1_tests {
    use super::*;
    use crate::commands::{Engine, RepoSituation};
    use crate::confirm::ScriptedGate;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Returns `main` for the main question and `in_progress` for the
    /// in-progress question (recognised by its `something_else` option).
    struct Fixed {
        main: Vec<(&'static str, f32)>,
        in_progress: Vec<(&'static str, f32)>,
        /// What Kev answers when the main options arrive in reversed order
        /// (the order-stability check); `None` → same as `main`.
        reordered: Option<Vec<(&'static str, f32)>>,
        main_calls: AtomicU32,
        in_progress_calls: AtomicU32,
    }

    impl Fixed {
        fn main(main: &[(&'static str, f32)]) -> Self {
            Self::with(main, &[])
        }
        fn with(main: &[(&'static str, f32)], in_progress: &[(&'static str, f32)]) -> Self {
            Fixed {
                main: main.to_vec(),
                in_progress: in_progress.to_vec(),
                reordered: None,
                main_calls: AtomicU32::new(0),
                in_progress_calls: AtomicU32::new(0),
            }
        }
        fn flips_to(mut self, reordered: &[(&'static str, f32)]) -> Self {
            self.reordered = Some(reordered.to_vec());
            self
        }
    }

    impl IntentClassifier for Fixed {
        async fn rank(
            &self,
            _state: serde_json::Value,
            _instructions: &str,
            options: &[(String, String)],
            _config: &Config,
        ) -> anyhow::Result<Vec<(String, f32)>> {
            let in_progress = options.iter().any(|(o, _)| o == "something_else");
            let reversed = options.first().map(|(o, _)| o.as_str())
                != crate::kev::ops().first().map(|(o, _)| o.as_str());
            let src = if in_progress {
                self.in_progress_calls.fetch_add(1, Ordering::SeqCst);
                &self.in_progress
            } else {
                self.main_calls.fetch_add(1, Ordering::SeqCst);
                match (&self.reordered, reversed) {
                    (Some(r), true) => r,
                    _ => &self.main,
                }
            };
            Ok(src.iter().map(|(o, p)| (o.to_string(), *p)).collect())
        }
    }

    fn ctx() -> crate::context::RepoContext {
        let mut c = crate::context::RepoContext::empty();
        c.branch = "main".into();
        c.is_empty_repo = false;
        c.situation = RepoSituation::Normal;
        c
    }

    async fn classify(
        k: &Fixed,
        c: &crate::context::RepoContext,
        q: &str,
        gate: &dyn ConfirmationGate,
    ) -> anyhow::Result<crate::commands::IntentResult> {
        classify_query(k, c, q, &Config::default(), gate).await
    }

    #[tokio::test]
    async fn fast_path_never_calls_kev() {
        let k = Fixed::main(&[("cleanup", 0.99)]);
        let r = classify(&k, &ctx(), "status", &AutoConfirmGate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::Fast);
        assert_eq!(k.main_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn confident_safe_op_routes() {
        let k = Fixed::main(&[("log", 0.73), ("status", 0.1)]);
        let r = classify(&k, &ctx(), "show me the history", &AutoConfirmGate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::Kev);
        assert!(matches!(
            r.intent,
            IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 })
        ));
    }

    #[tokio::test]
    async fn yes_refuses_to_guess_when_kev_is_unsure() {
        let k = Fixed::main(&[("push", 0.3), ("scoped_push", 0.25), ("add_commit", 0.1)]);
        let err = classify(&k, &ctx(), "put my work up", &AutoConfirmGate)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--yes"), "got: {}", err);
        // The refusal says what the choices were, in user terms.
        assert!(
            err.to_string().contains("Push only some changes"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn unsure_lets_the_user_pick_and_args_are_captured() {
        let k = Fixed::main(&[("push", 0.3), ("scoped_push", 0.25), ("add_commit", 0.1)]);
        let gate = ScriptedGate::new(1); // scoped_push
        let r = classify(&k, &ctx(), "ship the payments fixes", &gate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::User);
        assert_eq!(
            gate.last_choice_options.get(),
            3,
            "3 candidates — Esc cancels, no 'none' row"
        );
        let labels = gate.last_choice_labels.borrow().clone();
        assert_eq!(labels[0], "Push");
        assert_eq!(labels[1], "Push only some changes");
        assert!(
            labels.iter().all(|l| !l.contains('%') && !l.contains('_')),
            "{labels:?}"
        );
        match r.intent {
            IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush { feature_hint }) => {
                assert_eq!(feature_hint, "payments")
            }
            other => panic!("got {:?}", other),
        }
    }

    #[tokio::test]
    async fn destructive_op_below_its_bar_is_not_acted_on() {
        // 0.55 clears the read bar (0.5) but not the destructive one (0.6).
        let k = Fixed::main(&[("cleanup", 0.55), ("stash", 0.2)]);
        assert!(classify(&k, &ctx(), "nuke it all", &AutoConfirmGate)
            .await
            .is_err());
    }

    #[test]
    fn unclear_request_is_a_nonzero_refusal() {
        // "Not a Git request" did nothing, so it must not exit 0 — a script must
        // never read a refused, never-executed request as success.
        let err = unclear_request(crate::copy::NOT_A_GIT_REQUEST).unwrap_err();
        assert_eq!(err.to_string(), crate::copy::NOT_A_GIT_REQUEST);
    }

    #[tokio::test]
    async fn confident_reject_asks_to_rephrase() {
        let k = Fixed::main(&[(crate::kev::REJECT, 0.97), ("status", 0.01)]);
        let r = classify(&k, &ctx(), "make me a sandwich", &AutoConfirmGate)
            .await
            .unwrap();
        assert!(matches!(r.intent, IntentType::NeedsClarification(_)));
    }

    #[tokio::test]
    async fn unconfident_reject_offers_git_ops_instead_of_refusing() {
        // Fine-tune, dirty repo: "where am i right now" → __reject__ 0.66.
        let k = Fixed::main(&[
            (crate::kev::REJECT, 0.66),
            ("branches", 0.11),
            ("status", 0.1),
        ]);
        let gate = ScriptedGate::new(0);
        let r = classify(&k, &ctx(), "where am i right now", &gate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::User);
        assert!(matches!(
            r.intent,
            IntentType::ReadOnly(ReadOnlyOp::Branches)
        ));
        // ...and --yes refuses the menu rather than picking a Git op.
        assert!(
            classify(&k, &ctx(), "where am i right now", &AutoConfirmGate)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_named_topic_overrides_a_confident_plain_push() {
        // The fine-tune's confident miss: plain push at 0.96 for a scoped request.
        let k = Fixed::main(&[("push", 0.96), ("scoped_push", 0.02), ("add_commit", 0.01)]);
        let err = classify(&k, &ctx(), "ship the api fixes", &AutoConfirmGate)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--yes"), "got: {}", err);
        let gate = ScriptedGate::new(0);
        let r = classify(&k, &ctx(), "ship the api fixes", &gate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::User);
        assert_eq!(
            gate.last_choice_options.get(),
            2,
            "scoped_push, push — Esc cancels"
        );
        match r.intent {
            IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush { feature_hint }) => {
                assert_eq!(feature_hint, "api")
            }
            other => panic!("got {:?}", other),
        }
    }

    #[tokio::test]
    async fn commit_and_push_split_in_half_offers_commit_push() {
        // eval_set_v2 S07: the fine-tune routed "commit n push" to plain push (0.92),
        // which would push old commits and leave the changes uncommitted.
        let k = Fixed::main(&[("push", 0.92), ("add_commit", 0.05), ("commit_push", 0.01)]);
        assert!(classify(&k, &ctx(), "commit n push", &AutoConfirmGate)
            .await
            .is_err());
        let gate = ScriptedGate::new(0);
        let r = classify(&k, &ctx(), "commit n push", &gate).await.unwrap();
        assert_eq!(r.engine, Engine::User);
        assert!(
            matches!(
                r.intent,
                IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
                    stage_first: Some(_),
                    ..
                })
            ),
            "{:?}",
            r.intent
        );
    }

    #[test]
    fn commit_push_steps_stage_only_when_asked() {
        use crate::commands::FileSelection;
        let steps =
            |stage| commit_push_steps(stage, "m".into(), "origin".into(), "main".into(), false);
        let all = steps(Some(FileSelection::All));
        assert!(matches!(&all[..], [
            PipelineStep::Add { files },
            PipelineStep::Commit { .. },
            PipelineStep::Push { .. }
        ] if files.is_empty()));
        let named = steps(Some(FileSelection::Explicit(vec!["a.rs".into()])));
        assert!(
            matches!(&named[0], PipelineStep::Add { files } if files == &vec!["a.rs".to_string()])
        );
        // The manual-menu path (stage_first: None) still commits only what's staged.
        let staged = steps(None);
        assert!(matches!(
            &staged[..],
            [PipelineStep::Commit { .. }, PipelineStep::Push { .. }]
        ));
    }

    #[tokio::test]
    async fn destructive_route_is_rechecked_in_reversed_option_order() {
        let k = Fixed::main(&[("undo", 0.77), ("revert", 0.2)]);
        let r = classify(&k, &ctx(), "roll back that last commit", &AutoConfirmGate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::Kev);
        assert_eq!(k.main_calls.load(Ordering::SeqCst), 2, "asked twice");
    }

    #[tokio::test]
    async fn destructive_route_that_flips_under_reordering_asks() {
        let k = Fixed::main(&[("undo", 0.77), ("revert", 0.2)])
            .flips_to(&[("revert", 0.6), ("undo", 0.3)]);
        let err = classify(&k, &ctx(), "roll back that last commit", &AutoConfirmGate)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--yes"), "got: {}", err);
        let gate = ScriptedGate::new(1);
        let r = classify(&k, &ctx(), "roll back that last commit", &gate)
            .await
            .unwrap();
        assert_eq!(r.engine, Engine::User);
        let labels = gate.last_choice_labels.borrow().clone();
        assert!(
            labels[0].starts_with("Undo the last commit")
                && labels[1].starts_with("Revert a commit"),
            "{:?}",
            labels
        );
    }

    #[tokio::test]
    async fn safe_route_is_not_rechecked() {
        let k = Fixed::main(&[("log", 0.73), ("status", 0.1)]);
        classify(&k, &ctx(), "show me the history", &AutoConfirmGate)
            .await
            .unwrap();
        assert_eq!(k.main_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_argument_is_refused_under_yes() {
        let k = Fixed::main(&[("branch", 0.8)]);
        let err = classify(&k, &ctx(), "make me a new branch", &AutoConfirmGate)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("branch name"), "got: {}", err);
    }

    #[tokio::test]
    async fn in_progress_question_routes_continue_without_the_main_question() {
        let mut c = ctx();
        c.situation = RepoSituation::MidRebase {
            current: 2,
            total: 5,
            conflicted: vec![],
        };
        let k = Fixed::with(
            &[("status", 0.9)],
            &[("rebase_continue", 0.8), ("something_else", 0.1)],
        );
        let r = classify(&k, &c, "ok i fixed it, keep going", &AutoConfirmGate)
            .await
            .unwrap();
        assert!(matches!(
            r.intent,
            IntentType::ScopedWrite(ScopedWriteOp::RebaseContinue)
        ));
        assert_eq!(k.main_calls.load(Ordering::SeqCst), 0);
    }

    /// A classifier whose server is down.
    struct Down;
    impl IntentClassifier for Down {
        async fn rank(
            &self,
            _state: serde_json::Value,
            _instructions: &str,
            _options: &[(String, String)],
            _config: &Config,
        ) -> anyhow::Result<Vec<(String, f32)>> {
            anyhow::bail!("The Kev model server isn't running")
        }
    }

    #[tokio::test]
    async fn with_kev_down_the_manual_menu_uses_the_normal_safe_path() {
        // The old manual menu listed raw git commands, mislabeled commit+push as
        // "git push", offered a raw hard reset past every gate, and reported ⚡.
        let gate = ScriptedGate::new(0);
        let r = classify_query(
            &Down,
            &ctx(),
            "show me the history",
            &Config::default(),
            &gate,
        )
        .await
        .unwrap();
        assert_eq!(r.engine, Engine::User);
        let labels = gate.last_choice_labels.borrow().clone();
        assert!(labels.iter().all(|l| !l.starts_with("git ")), "{labels:?}");
        assert!(
            labels.contains(&"Push".to_string())
                && labels.contains(&"Undo the last commit".to_string())
        );
        assert!(matches!(r.intent, IntentType::ReadOnly(ReadOnlyOp::Status)));
        // Picking Undo reaches the undo gate's intent, never a raw reset.
        let undo_idx = labels
            .iter()
            .position(|l| l == "Undo the last commit")
            .unwrap();
        let r = classify_query(
            &Down,
            &ctx(),
            "x",
            &Config::default(),
            &ScriptedGate::new(undo_idx),
        )
        .await
        .unwrap();
        assert!(matches!(
            r.intent,
            IntentType::ScopedWrite(ScopedWriteOp::UndoIntent)
        ));
        // --yes can't pick for you.
        assert!(
            classify_query(&Down, &ctx(), "x", &Config::default(), &AutoConfirmGate)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_conflict_question_mid_operation_shows_status() {
        // eval_set_v2 S29: "whats conflicting" mid-rebase went something_else →
        // merge_preview (a fetch + "no remote-tracking ref"). Status now lists
        // the conflicted files and the way out, so that is the answer.
        let mut c = ctx();
        c.situation = RepoSituation::MidRebase {
            current: 1,
            total: 2,
            conflicted: vec!["src/login.rs".into()],
        };
        let k = Fixed::with(&[("merge_preview", 0.56)], &[("something_else", 0.68)]);
        for q in [
            "whats conflicting",
            "Before I continue or abort, show me which files have conflicts.",
            "which files have conflicts?",
        ] {
            let r = classify(&k, &c, q, &AutoConfirmGate).await.unwrap();
            assert!(
                matches!(r.intent, IntentType::ReadOnly(ReadOnlyOp::Status)),
                "{q}: {:?}",
                r.intent
            );
        }
        assert_eq!(
            k.main_calls.load(Ordering::SeqCst) + k.in_progress_calls.load(Ordering::SeqCst),
            0
        );
        // Outside an operation the word is not special.
        let k2 = Fixed::main(&[("merge_preview", 0.9)]);
        let r = classify(
            &k2,
            &ctx(),
            "will pulling cause conflicts",
            &AutoConfirmGate,
        )
        .await
        .unwrap();
        assert!(matches!(
            r.intent,
            IntentType::ReadOnly(ReadOnlyOp::MergePreview { .. })
        ));
        // "abort despite the conflicts" is a control request, not a question.
        let k3 = Fixed::with(
            &[("status", 0.9)],
            &[("rebase_abort", 0.95), ("something_else", 0.01)],
        );
        let r = classify(&k3, &c, "abort, too many conflicts", &ScriptedGate::new(0))
            .await
            .unwrap();
        assert!(!matches!(
            r.intent,
            IntentType::ReadOnly(ReadOnlyOp::Status)
        ));
    }

    #[tokio::test]
    async fn in_progress_abort_needs_the_destructive_bar() {
        let mut c = ctx();
        c.situation = RepoSituation::MidRebase {
            current: 2,
            total: 5,
            conflicted: vec![],
        };
        // 0.55 abort is below the 0.6 bar → never acted on: the family menu is
        // shown instead, which --yes refuses (nothing aborted).
        let k = Fixed::with(
            &[("status", 0.9)],
            &[("rebase_abort", 0.55), ("something_else", 0.3)],
        );
        let err = classify(&k, &c, "get me out", &AutoConfirmGate)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--yes"), "got: {}", err);
        assert_eq!(k.in_progress_calls.load(Ordering::SeqCst), 1);
        assert_eq!(k.main_calls.load(Ordering::SeqCst), 0);
        // At 0.65 it clears the bar and routes.
        let k = Fixed::with(
            &[("status", 0.9)],
            &[("rebase_abort", 0.65), ("something_else", 0.3)],
        );
        let r = classify(&k, &c, "abort the rebase", &AutoConfirmGate)
            .await
            .unwrap();
        assert!(matches!(
            r.intent,
            IntentType::ScopedWrite(ScopedWriteOp::RebaseAbort)
        ));
    }

    #[tokio::test]
    async fn unsure_in_progress_control_shows_the_family_menu() {
        let mut c = ctx();
        c.situation = RepoSituation::MidRebase {
            current: 1,
            total: 1,
            conflicted: vec![],
        };
        let k = Fixed::with(
            &[("status", 0.9)],
            &[
                ("rebase_continue", 0.34),
                ("something_else", 0.30),
                ("rebase_skip", 0.2),
                ("rebase_abort", 0.16),
            ],
        );
        // Interactive: the user picks the first row (continue).
        let gate = ScriptedGate::new(0);
        let r = classify(&k, &c, "conflicts are sorted, carry on", &gate)
            .await
            .unwrap();
        assert!(matches!(
            r.intent,
            IntentType::ScopedWrite(ScopedWriteOp::RebaseContinue)
        ));
        assert_eq!(r.engine, Engine::User);
        assert_eq!(gate.last_choice_options.get(), 4);
        assert_eq!(k.main_calls.load(Ordering::SeqCst), 0);
        // "something else" (last row) falls through to the main question.
        let gate = ScriptedGate::new(3);
        let r = classify(&k, &c, "conflicts are sorted, carry on", &gate)
            .await
            .unwrap();
        assert!(matches!(r.intent, IntentType::ReadOnly(ReadOnlyOp::Status)));
        // --yes never continues/aborts on an unsure guess.
        assert!(classify(&k, &c, "carry on", &AutoConfirmGate)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn no_in_progress_question_in_a_normal_repo() {
        let k = Fixed::with(&[("status", 0.9)], &[("rebase_abort", 0.99)]);
        classify(&k, &ctx(), "what's changed", &AutoConfirmGate)
            .await
            .unwrap();
        assert_eq!(k.in_progress_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn yes_without_a_commit_message_refuses_to_commit() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let cfg = Config::default();
        let err = resolve_or_generate_commit_message(
            &repo,
            CommitMessage::Draft(String::new()),
            true,
            &cfg,
            &AutoConfirmGate,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("commit message is required"));
        // A message the user put in the request goes through untouched.
        let ok = resolve_or_generate_commit_message(
            &repo,
            CommitMessage::Provided("fix login".into()),
            true,
            &cfg,
            &AutoConfirmGate,
        )
        .await
        .unwrap();
        assert_eq!(ok, "fix login");
    }
}
