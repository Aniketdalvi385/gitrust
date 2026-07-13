use anyhow::Context;
use git2::Repository;

use crate::commands::{
    FeaturePipelineOp, IntentType, PipelineStep, PreCheckResult, RepoSituation, ResetMode,
    ScopedWriteOp,
};

// =============================================================================
// Situation gate (Layer 3, early) — enforces the §13 situation × intent matrix.
// Runs immediately after classification and BEFORE resolution, so an intent
// that is invalid for the current RepoSituation never reaches the resolver.
// This is the single source of state-based blocking (it subsumes Phase 9's
// interim detached-HEAD pre-check).
// =============================================================================

/// Verdict for `(RepoSituation, IntentType)`.
pub enum SituationVerdict {
    /// The intent is valid in this situation — proceed to resolution/execution.
    Allowed,
    /// The intent is invalid here. `message` names the situation AND the way out.
    Blocked { message: String },
    /// Reserved for Phase 12 (e.g. Diverged → reconcile flow). Not produced by
    /// any Phase 10 row.
    #[allow(dead_code)]
    Redirect { suggested_intent: IntentType },
}

/// True for intents that mutate the repo (everything except read-only queries
/// and clarification questions).
pub fn is_write_intent(intent: &IntentType) -> bool {
    !matches!(
        intent,
        IntentType::ReadOnly(_) | IntentType::NeedsClarification(_)
    )
}

fn is_branch_create(i: &IntentType) -> bool {
    matches!(i, IntentType::ScopedWrite(ScopedWriteOp::Branch { .. }))
}
fn is_checkout(i: &IntentType) -> bool {
    matches!(i, IntentType::ScopedWrite(ScopedWriteOp::Checkout { .. }))
}
/// Staging / committing — the ops that are valid on an empty repo (first
/// commit) and as the merge-aware commit that resolves a merge.
fn is_commit_like(i: &IntentType) -> bool {
    matches!(
        i,
        IntentType::ScopedWrite(
            ScopedWriteOp::Commit { .. }
                | ScopedWriteOp::AddAndCommit { .. }
                | ScopedWriteOp::Add { .. }
        )
    )
}
/// Anything that pushes to a remote (plain, force, or a pipeline ending in push).
fn is_push_like(i: &IntentType) -> bool {
    matches!(
        i,
        IntentType::ScopedWrite(ScopedWriteOp::Push { .. } | ScopedWriteOp::ForcePush { .. })
            | IntentType::FeaturePipeline(
                FeaturePipelineOp::AddCommitPush { .. } | FeaturePipelineOp::CommitPush { .. }
            )
    )
}
/// Anything that integrates remote history into local (pull / rebase / sync /
/// discard-to-remote).
fn is_reconcile(i: &IntentType) -> bool {
    matches!(
        i,
        IntentType::ScopedWrite(
            ScopedWriteOp::Pull { .. }
                | ScopedWriteOp::PullRebase { .. }
                | ScopedWriteOp::DiscardLocal { .. }
        ) | IntentType::FeaturePipeline(FeaturePipelineOp::Sync { .. })
    )
}

/// Decide whether `intent` is allowed in `situation` (§13 matrix). Pure — no
/// repo, no I/O — so it is trivially deterministic and table-testable.
pub fn situation_gate(situation: &RepoSituation, intent: &IntentType) -> SituationVerdict {
    use RepoSituation as S;

    // Read-only queries and clarification questions are always allowed.
    if !is_write_intent(intent) {
        return SituationVerdict::Allowed;
    }
    let block = |m: String| SituationVerdict::Blocked { message: m };

    match situation {
        // No repo yet is handled before the gate (init-only); treat as pass-through.
        S::Normal | S::NotARepo => SituationVerdict::Allowed,
        S::EmptyRepo => {
            if is_commit_like(intent) || is_branch_create(intent) {
                SituationVerdict::Allowed
            } else {
                block(crate::copy::EMPTY_REPO_BLOCK.to_string())
            }
        }
        S::DetachedHead { stranded_commits } => {
            if is_branch_create(intent) {
                SituationVerdict::Allowed
            } else if is_checkout(intent) {
                if *stranded_commits > 0 {
                    // Phase 10 blocks; the branch-here way-out (S21 gate) is Phase 14.
                    block(crate::copy::detached_stranded_block(*stranded_commits))
                } else {
                    SituationVerdict::Allowed
                }
            } else {
                block(crate::copy::DETACHED_HEAD_BLOCK.to_string())
            }
        }
        // Merge-aware commit (Phase 9.2) is the way out of a resolved merge.
        S::MidMerge { resolved, .. } => {
            if *resolved && is_commit_like(intent) {
                SituationVerdict::Allowed
            } else {
                block(crate::copy::MERGE_IN_PROGRESS_BLOCK.to_string())
            }
        }
        // Continue/skip/abort steps arrive in Phase 11; for now all writes block.
        S::MidRebase { .. } => block(crate::copy::REBASE_IN_PROGRESS_BLOCK.to_string()),
        S::MidCherryPick { .. } => block(crate::copy::CHERRY_PICK_IN_PROGRESS_BLOCK.to_string()),
        S::MidRevert { .. } => block(crate::copy::REVERT_IN_PROGRESS_BLOCK.to_string()),
        S::MidBisect => block(crate::copy::BISECT_IN_PROGRESS_BLOCK.to_string()),
        // Diverged blocks the doomed plain push; reconcile/commit stay allowed
        // (the rebase-vs-merge ChoiceGate is Phase 12). Behind-only is NOT
        // Diverged → stays Normal → the BehindUpstream warn still fires.
        S::Diverged { ahead, behind } => {
            if is_push_like(intent) {
                block(crate::copy::diverged_block(*ahead, *behind))
            } else {
                SituationVerdict::Allowed
            }
        }
        // Don't reconcile blindly onto rewritten history; local commits are fine.
        S::RemoteRewritten { .. } => {
            if is_push_like(intent) || is_reconcile(intent) {
                block(crate::copy::REMOTE_REWRITTEN_BLOCK.to_string())
            } else {
                SituationVerdict::Allowed
            }
        }
        S::StaleLock { age_secs } => block(crate::copy::stale_lock_block(*age_secs)),
    }
}

/// Trait for the validation pipeline — enables swapping validation strategy.
pub trait Validator {
    fn run_pre_checks(
        &self,
        repo: &Repository,
        steps: &[PipelineStep],
        hooks_mode: &str,
    ) -> anyhow::Result<Vec<PreCheckResult>>;

    fn build_preview(
        &self,
        steps: &[PipelineStep],
        warnings: &[String],
        ctx: &crate::context::RepoContext,
        snapshot_sha7: &str,
    ) -> String;

    fn run_post_check(
        &self,
        repo: &Repository,
        step: &PipelineStep,
        pre_state: &PreStepState,
    ) -> anyhow::Result<PostCheckResult>;
}

/// Snapshot of repo state captured immediately before a step executes.
/// Used by post-checks to verify the step had the expected effect.
#[derive(Debug, Clone)]
pub struct PreStepState {
    pub head_oid: Option<String>,
    pub staged_paths: Vec<String>,
    pub remote_head_oid: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    /// True when the repo was mid-merge (`MERGE_HEAD` present) before the step.
    /// Lets the Commit post-check verify the merge landed correctly (S17).
    pub was_merging: bool,
}

/// Result of a post-check.
#[derive(Debug, Clone)]
pub enum PostCheckResult {
    Verified,
    Warning(String),
    Failed(String),
}

/// Default validation pipeline implementation.
pub struct DefaultValidator;

impl Validator for DefaultValidator {
    fn run_pre_checks(
        &self,
        repo: &Repository,
        steps: &[PipelineStep],
        hooks_mode: &str,
    ) -> anyhow::Result<Vec<PreCheckResult>> {
        let mut results = Vec::new();

        // NOTE: state-based blocking (detached HEAD, in-progress ops) moved to
        // `situation_gate`, which runs before resolution. `run_pre_checks` now
        // only covers execution-layer concerns. `ConflictsPresent` below stays
        // as a backstop — writes in Mid* states are already blocked by the gate,
        // so it only ever sees allowed intents (e.g. the resolved-merge commit,
        // which is conflict-free and passes).

        // ConflictsPresent — block if merge conflicts exist
        if has_conflicts(repo)? {
            results.push(PreCheckResult::Block(
                "Merge conflicts detected. Resolve all conflicts before proceeding.".to_string(),
            ));
        }

        // 3. NoPushTarget — warn/block if push step has no remote
        for step in steps {
            if let PipelineStep::Push { remote, branch, .. } = step {
                if !remote_exists(repo, remote) {
                    results.push(PreCheckResult::Block(format!(
                        "Remote '{}' is not configured. Add it with: git remote add {} <url>",
                        remote, remote
                    )));
                }
                if branch.is_empty() {
                    results.push(PreCheckResult::Block(
                        "Cannot push: no target branch specified.".to_string(),
                    ));
                }
            }
        }

        // 4. BehindUpstream — warn if local is behind remote
        let (ahead, behind) = get_ahead_behind(repo).unwrap_or((0, 0));
        if behind > 0 {
            let has_push = steps.iter().any(|s| matches!(s, PipelineStep::Push { .. }));
            if has_push {
                results.push(PreCheckResult::Warn(format!(
                    "Local branch is {} commit(s) behind upstream. \
                     Consider pulling first to avoid a non-fast-forward rejection.",
                    behind
                )));
            }
        }
        let _ = ahead; // used in context, suppress warning

        // 5. EmptyCommitMessage — block if commit step has empty message
        for step in steps {
            if let PipelineStep::Commit { message } = step {
                let trimmed = message.trim();
                if trimmed.is_empty() {
                    results.push(PreCheckResult::Block(
                        "Commit message is empty. Provide a non-empty message.".to_string(),
                    ));
                } else {
                    // Warn if first line > 72 chars
                    let first_line = trimmed.lines().next().unwrap_or("");
                    if first_line.len() > 72 {
                        results.push(PreCheckResult::Warn(format!(
                            "Commit message first line is {} characters (recommended ≤72).",
                            first_line.len()
                        )));
                    }
                }
            }
        }

        // 5b. EmptyCommit — block a Commit step that would produce no changes.
        //     If a prior step in this pipeline is an Add, assume it will stage something
        //     (we can't pre-execute it), so only check when Commit has no preceding Add.
        let has_prior_add = |idx: usize| -> bool {
            steps[..idx]
                .iter()
                .any(|s| matches!(s, PipelineStep::Add { .. }))
        };
        for (i, step) in steps.iter().enumerate() {
            if matches!(step, PipelineStep::Commit { .. }) && !has_prior_add(i) {
                if index_is_empty_vs_head(repo).unwrap_or(false) {
                    results.push(PreCheckResult::Block(
                        "Nothing staged to commit. Stage files first (e.g. 'commit the <file>' or 'commit all my files').".to_string(),
                    ));
                }
            }
        }

        // 6. HookExists — inform the user how hooks will actually be handled.
        //    libgit2 never runs hooks (S47); commit routes through system git
        //    under "auto" (when present) / "system", and is skipped under
        //    "git2". Push always shells out, so its hooks always run. The old
        //    copy claimed the hook "will run" under git2 — which was false.
        for op in [crate::git_ops::HookOp::Commit, crate::git_ops::HookOp::Push] {
            let present = crate::git_ops::hooks_for_op(repo, op);
            if present.is_empty() {
                continue;
            }
            let has_relevant_step = steps.iter().any(|s| match op {
                crate::git_ops::HookOp::Commit => matches!(s, PipelineStep::Commit { .. }),
                crate::git_ops::HookOp::Push => matches!(s, PipelineStep::Push { .. }),
            });
            if !has_relevant_step {
                continue;
            }
            let hook_list = present.join(", ");
            let routed = match op {
                // Push already shells out unconditionally → hooks always run.
                crate::git_ops::HookOp::Push => true,
                crate::git_ops::HookOp::Commit => {
                    crate::git_ops::should_route_to_system_git(hooks_mode, true)
                }
            };
            let msg = if routed {
                format!(
                    "Hook(s) present ({}) — GLit will run them via system git.",
                    hook_list
                )
            } else {
                format!(
                    "Hook(s) present ({}) but will be SKIPPED (hooks=git2).",
                    hook_list
                )
            };
            results.push(PreCheckResult::Warn(msg));
        }

        // 7. BinaryFiles — warn if any Add step targets likely-binary files
        for step in steps {
            if let PipelineStep::Add { files } = step {
                let binaries: Vec<&String> = files.iter().filter(|f| is_likely_binary(f)).collect();
                if !binaries.is_empty() {
                    results.push(PreCheckResult::Warn(format!(
                        "Binary file(s) will be staged: {}. Confirm this is intentional.",
                        binaries
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }

        // 8. SubmodulePresent — warn if submodules exist
        if has_submodules(repo) {
            results.push(PreCheckResult::Warn(
                "Repository contains submodules. Ensure submodule changes are handled separately."
                    .to_string(),
            ));
        }

        // 9. HardReset — extra warning for destructive reset
        for step in steps {
            if let PipelineStep::Reset {
                mode: ResetMode::Hard,
            } = step
            {
                results.push(PreCheckResult::Warn(
                    "reset --hard will discard ALL uncommitted changes. \
                     This cannot be undone (except via git reflog)."
                        .to_string(),
                ));
            }
        }

        // If no issues found, emit a single Ok
        if results.is_empty() {
            results.push(PreCheckResult::Ok);
        }

        Ok(results)
    }

    fn build_preview(
        &self,
        steps: &[PipelineStep],
        warnings: &[String],
        ctx: &crate::context::RepoContext,
        snapshot_sha7: &str,
    ) -> String {
        let use_color = crate::danger::use_color();
        let (node, connector) = if use_color {
            (
                crate::copy::TIMELINE_RUNNING,
                crate::copy::TIMELINE_CONNECTOR,
            )
        } else {
            (
                crate::copy::TIMELINE_RUNNING_ASCII,
                crate::copy::TIMELINE_CONNECTOR_ASCII,
            )
        };
        // Dim style for sub-details and connectors.
        let dim = |s: &str| -> String {
            if use_color {
                format!("\x1b[2m{}\x1b[0m", s)
            } else {
                s.to_string()
            }
        };

        let mut out = String::new();
        out.push('\n');
        if snapshot_sha7.is_empty() {
            out.push_str(&format!("Plan  · {} steps\n", steps.len()));
        } else {
            out.push_str(&format!(
                "Plan  · {} steps · {}\n",
                steps.len(),
                dim(&format!("snapshot taken at {}", snapshot_sha7))
            ));
        }
        out.push('\n');

        let last = steps.len().saturating_sub(1);
        for (i, step) in steps.iter().enumerate() {
            let (verb, detail) = format_step_label(step);
            let level = crate::danger::step_danger(step, ctx);
            let marker = crate::danger::colored_marker(level);

            // Row 1: ● {verb pad to 10}{detail}  {marker}
            let row = if marker.is_empty() {
                format!("  {}  {:<10}{}\n", node, verb, detail)
            } else {
                format!("  {}  {:<10}{}  {}\n", node, verb, detail, marker)
            };
            out.push_str(&row);

            // Inline file list for Add steps with explicit paths
            if let PipelineStep::Add { files } = step {
                if !files.is_empty() && files.len() <= 5 {
                    for f in files {
                        out.push_str(&format!(
                            "  {}     {}\n",
                            dim(connector),
                            dim(&format!("+ {}", f))
                        ));
                    }
                } else if files.len() > 5 {
                    for f in files.iter().take(3) {
                        out.push_str(&format!(
                            "  {}     {}\n",
                            dim(connector),
                            dim(&format!("+ {}", f))
                        ));
                    }
                    out.push_str(&format!(
                        "  {}     {}\n",
                        dim(connector),
                        dim(&format!("+ … and {} more", files.len() - 3))
                    ));
                }
            }

            // Connector between steps (none after the last).
            if i < last {
                out.push_str(&format!("  {}\n", dim(connector)));
            }
        }

        if !warnings.is_empty() {
            out.push('\n');
            out.push_str("  Warnings:\n");
            for w in warnings {
                out.push_str(&format!("    · {}\n", w));
            }
        }

        out.push('\n');
        out.push_str(&format!("  {}\n", dim(crate::copy::PLAN_KEYMAP_LEGEND)));
        out
    }

    fn run_post_check(
        &self,
        repo: &Repository,
        step: &PipelineStep,
        pre_state: &PreStepState,
    ) -> anyhow::Result<PostCheckResult> {
        match step {
            PipelineStep::Add { files } => post_check_add(repo, files, pre_state),
            PipelineStep::Commit { message } => post_check_commit(repo, message, pre_state),
            PipelineStep::Push { .. } => post_check_push(repo, pre_state),
            PipelineStep::Pull { .. } => post_check_pull(repo, pre_state),
            _ => Ok(PostCheckResult::Verified),
        }
    }
}

// =============================================================================
// Pre-check helpers
// =============================================================================

fn has_conflicts(repo: &Repository) -> anyhow::Result<bool> {
    let index = repo.index().context("Failed to read index")?;
    Ok(index.has_conflicts())
}

fn remote_exists(repo: &Repository, remote: &str) -> bool {
    repo.find_remote(remote).is_ok()
}

fn get_ahead_behind(repo: &Repository) -> anyhow::Result<(usize, usize)> {
    let head = repo.head().context("Failed to read HEAD")?;
    let branch_name = head.shorthand().unwrap_or("HEAD").to_string();

    let local_oid = head.target().context("HEAD has no target")?;

    // Find upstream branch ref
    let upstream_ref = format!("refs/remotes/origin/{}", branch_name);
    let upstream_oid = match repo.find_reference(&upstream_ref) {
        Ok(r) => match r.target() {
            Some(o) => o,
            None => return Ok((0, 0)),
        },
        Err(_) => return Ok((0, 0)),
    };

    let (ahead, behind) = repo
        .graph_ahead_behind(local_oid, upstream_oid)
        .context("Failed to compute ahead/behind")?;

    Ok((ahead, behind))
}

/// True if the index has no staged differences vs HEAD (i.e. committing now would be empty).
/// For an empty repo with no HEAD, "empty" means the index itself has no entries.
fn index_is_empty_vs_head(repo: &Repository) -> anyhow::Result<bool> {
    let index = repo.index().context("Failed to read index")?;
    let head_tree = match repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
        Some(t) => t,
        None => {
            // No HEAD — first commit case. Empty if the index has nothing.
            return Ok(index.len() == 0);
        }
    };
    let diff = repo
        .diff_tree_to_index(Some(&head_tree), Some(&index), None)
        .context("Failed to diff HEAD to index")?;
    Ok(diff.deltas().count() == 0)
}

fn has_submodules(repo: &Repository) -> bool {
    repo.submodules().map(|s| !s.is_empty()).unwrap_or(false)
}

fn is_likely_binary(path: &str) -> bool {
    let binary_extensions = [
        ".png", ".jpg", ".jpeg", ".gif", ".ico", ".bmp", ".webp", ".zip", ".tar", ".gz", ".bz2",
        ".7z", ".exe", ".dll", ".so", ".dylib", ".pdf", ".doc", ".docx", ".woff", ".woff2", ".ttf",
        ".otf",
    ];
    let lower = path.to_lowercase();
    binary_extensions.iter().any(|ext| lower.ends_with(ext))
}

// =============================================================================
// Post-check helpers
// =============================================================================

fn post_check_add(
    repo: &Repository,
    files: &[String],
    _pre_state: &PreStepState,
) -> anyhow::Result<PostCheckResult> {
    let index = repo
        .index()
        .context("Failed to read index for post-check")?;

    let missing: Vec<&String> = files
        .iter()
        .filter(|f| {
            // Check if file is in the index
            index
                .get_path(std::path::Path::new(f.as_str()), 0)
                .is_none()
        })
        .collect();

    if missing.is_empty() {
        Ok(PostCheckResult::Verified)
    } else {
        Ok(PostCheckResult::Warning(format!(
            "Could not verify staging for: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

fn post_check_commit(
    repo: &Repository,
    message: &str,
    pre_state: &PreStepState,
) -> anyhow::Result<PostCheckResult> {
    let new_head = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .map(|o| o.to_string());

    if new_head == pre_state.head_oid {
        return Ok(PostCheckResult::Failed(
            "HEAD did not advance after commit — commit may have failed silently.".to_string(),
        ));
    }

    // Verify commit message matches (first 40 chars to handle LLM-trimmed messages)
    if let Some(ref oid_str) = new_head {
        if let Ok(oid) = git2::Oid::from_str(oid_str) {
            if let Ok(commit) = repo.find_commit(oid) {
                let actual_msg = commit.message().unwrap_or("");
                let expected_prefix = &message[..message.len().min(40)];
                if !actual_msg.contains(expected_prefix.trim()) && !message.trim().is_empty() {
                    return Ok(PostCheckResult::Warning(format!(
                        "Commit message mismatch. Expected to start with: '{}'",
                        expected_prefix
                    )));
                }
            }
        }
    }

    // Merge-aware verification (S17): if we began the step mid-merge, the new
    // commit must record exactly 2 parents and MERGE_HEAD must be gone.
    if pre_state.was_merging {
        let parents_ok = new_head
            .as_ref()
            .and_then(|s| git2::Oid::from_str(s).ok())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|c| c.parent_count() == 2)
            .unwrap_or(false);
        if !parents_ok {
            return Ok(PostCheckResult::Failed(
                "Merge commit should record 2 parents — the merge may have been flattened."
                    .to_string(),
            ));
        }
        if repo.path().join("MERGE_HEAD").exists() {
            return Ok(PostCheckResult::Failed(
                "MERGE_HEAD still present after commit — merge state was not cleared.".to_string(),
            ));
        }
    }

    Ok(PostCheckResult::Verified)
}

fn post_check_push(repo: &Repository, pre_state: &PreStepState) -> anyhow::Result<PostCheckResult> {
    // Check that ahead count has dropped (we pushed our commits)
    let (new_ahead, _) = get_ahead_behind(repo).unwrap_or((pre_state.ahead, 0));

    if pre_state.ahead > 0 && new_ahead >= pre_state.ahead {
        Ok(PostCheckResult::Warning(
            "Push may not have succeeded — remote HEAD appears unchanged.".to_string(),
        ))
    } else {
        Ok(PostCheckResult::Verified)
    }
}

fn post_check_pull(repo: &Repository, pre_state: &PreStepState) -> anyhow::Result<PostCheckResult> {
    let (_, new_behind) = get_ahead_behind(repo).unwrap_or((0, pre_state.behind));

    if pre_state.behind > 0 && new_behind >= pre_state.behind {
        Ok(PostCheckResult::Warning(
            "Pull may not have succeeded — behind count unchanged.".to_string(),
        ))
    } else {
        Ok(PostCheckResult::Verified)
    }
}

// =============================================================================
// Preview helpers
// =============================================================================

/// Split each pipeline step into a Glit verb and a detail string for the
/// timeline-style plan preview. The verb is the operation the user thinks in
/// (`stage`, `commit`, `push`), not the underlying git plumbing.
pub fn format_step_label(step: &PipelineStep) -> (&'static str, String) {
    match step {
        PipelineStep::Add { files } => {
            let detail = if files.is_empty() {
                "all modified files".to_string()
            } else if files.len() <= 3 {
                files.join(", ")
            } else {
                format!("{} files", files.len())
            };
            ("stage", detail)
        }
        PipelineStep::Commit { message } => {
            let preview = if message.len() > 50 {
                format!("{}…", &message[..50])
            } else {
                message.clone()
            };
            ("commit", format!("\"{}\"", preview))
        }
        PipelineStep::Push {
            remote,
            branch,
            set_upstream,
        } => {
            let detail = if *set_upstream {
                format!("→ {}/{} (set upstream)", remote, branch)
            } else {
                format!("→ {}/{}", remote, branch)
            };
            ("push", detail)
        }
        PipelineStep::Pull { remote } => ("pull", format!("from {}", remote)),
        PipelineStep::Branch { name } => ("branch", name.clone()),
        PipelineStep::Checkout { target } => ("checkout", target.clone()),
        PipelineStep::Stash => ("stash", String::new()),
        PipelineStep::StashPop => ("stash pop", String::new()),
        PipelineStep::Reset { mode } => {
            let detail = match mode {
                ResetMode::Soft => "soft (keeps staged)".to_string(),
                ResetMode::Mixed => "mixed (unstages files)".to_string(),
                ResetMode::Hard => "hard (discards working tree)".to_string(),
            };
            ("reset", detail)
        }
        PipelineStep::Log { count } => ("log", format!("-{}", count)),
        PipelineStep::Status => ("status", String::new()),
        PipelineStep::Diff { path } => {
            let detail = path.clone().unwrap_or_default();
            ("diff", detail)
        }
        PipelineStep::AddRemote { name, url } => ("add remote", format!("{} {}", name, url)),
        PipelineStep::RemoveRemote { name } => ("remove remote", name.clone()),
        PipelineStep::Undo {
            undone_sha,
            undone_summary,
            ..
        } => ("undo", format!("{} \"{}\"", undone_sha, undone_summary)),
        PipelineStep::PullRebase { remote, branch } => {
            ("rebase", format!("onto {}/{}", remote, branch))
        }
        PipelineStep::ForcePush { remote, branch } => (
            "force push",
            format!("→ {}/{} (with-lease)", remote, branch),
        ),
        PipelineStep::DiscardLocal { remote, branch } => {
            ("discard local", format!("take {}/{}", remote, branch))
        }
    }
}

/// Capture pre-step state for post-check comparison.
pub fn capture_pre_step_state(repo: &Repository) -> PreStepState {
    let head_oid = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .map(|o| o.to_string());

    let staged_paths = repo
        .index()
        .ok()
        .map(|idx| {
            idx.iter()
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect()
        })
        .unwrap_or_default();

    let (ahead, behind) = get_ahead_behind(repo).unwrap_or((0, 0));

    PreStepState {
        head_oid,
        staged_paths,
        remote_head_oid: None, // populated lazily if needed
        ahead,
        behind,
        was_merging: repo.state() == git2::RepositoryState::Merge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{PipelineStep, PreCheckResult};
    use tempfile::TempDir;

    fn init_repo(dir: &TempDir) -> Repository {
        let repo = Repository::init(dir.path()).unwrap();
        // Set up git identity to avoid config errors
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@test.com").unwrap();
        repo
    }

    fn init_repo_with_commit(dir: &TempDir) -> Repository {
        let repo = init_repo(dir);
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = git2::Signature::now("Test", "test@test.com").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
                .unwrap();
        }
        repo
    }

    // Detached-HEAD blocking now lives in the situation gate (subsumes the
    // Phase 9 pre-check). A real detached repo → DetachedHead → commit blocked.
    #[test]
    fn test_detached_head_gate_blocks_commit() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let head_oid = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(head_oid).unwrap();

        let situation = crate::context::detect_situation(&repo);
        assert!(
            matches!(situation, RepoSituation::DetachedHead { .. }),
            "expected DetachedHead, got {:?}",
            situation
        );
        let intent = IntentType::ScopedWrite(ScopedWriteOp::Commit {
            message: crate::commands::CommitMessage::Provided("test".into()),
        });
        assert!(matches!(
            situation_gate(&situation, &intent),
            SituationVerdict::Blocked { .. }
        ));
    }

    #[test]
    fn test_pre_check_empty_commit_message_blocks() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let v = DefaultValidator;
        let steps = vec![PipelineStep::Commit {
            message: "   ".to_string(),
        }];
        let results = v.run_pre_checks(&repo, &steps, "auto").unwrap();
        assert!(results
            .iter()
            .any(|r| matches!(r, PreCheckResult::Block(_))));
    }

    #[test]
    fn test_pre_check_clean_repo_returns_ok() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let v = DefaultValidator;
        let steps = vec![PipelineStep::Status];
        let results = v.run_pre_checks(&repo, &steps, "auto").unwrap();
        assert!(results.iter().any(|r| matches!(r, PreCheckResult::Ok)));
    }

    #[test]
    fn test_pre_check_missing_remote_blocks_push() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let v = DefaultValidator;
        let steps = vec![PipelineStep::Push {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            set_upstream: false,
        }];
        let results = v.run_pre_checks(&repo, &steps, "auto").unwrap();
        // No remote configured — should block
        assert!(results
            .iter()
            .any(|r| matches!(r, PreCheckResult::Block(_))));
    }

    #[test]
    fn test_pre_check_binary_file_warns() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let v = DefaultValidator;
        let steps = vec![PipelineStep::Add {
            files: vec!["logo.png".to_string()],
        }];
        let results = v.run_pre_checks(&repo, &steps, "auto").unwrap();
        assert!(results.iter().any(|r| matches!(r, PreCheckResult::Warn(_))));
    }

    #[test]
    fn test_pre_check_long_commit_message_warns() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let v = DefaultValidator;
        let long_msg = "x".repeat(80);
        let steps = vec![PipelineStep::Commit { message: long_msg }];
        let results = v.run_pre_checks(&repo, &steps, "auto").unwrap();
        assert!(results.iter().any(|r| matches!(r, PreCheckResult::Warn(_))));
    }

    #[test]
    fn test_pre_check_hard_reset_warns() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let v = DefaultValidator;
        let steps = vec![PipelineStep::Reset {
            mode: ResetMode::Hard,
        }];
        let results = v.run_pre_checks(&repo, &steps, "auto").unwrap();
        assert!(results.iter().any(|r| matches!(r, PreCheckResult::Warn(_))));
    }

    #[test]
    fn test_build_preview_ordering() {
        let v = DefaultValidator;
        let steps = vec![
            PipelineStep::Add {
                files: vec!["src/login.rs".to_string(), "src/auth.rs".to_string()],
            },
            PipelineStep::Commit {
                message: "add login feature".to_string(),
            },
            PipelineStep::Push {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                set_upstream: false,
            },
        ];
        let warnings = vec!["Branch is 1 commit behind upstream.".to_string()];
        let ctx = crate::context::RepoContext::empty();
        let preview = v.build_preview(&steps, &warnings, &ctx, "aa44620");

        assert!(preview.contains("Plan"));
        assert!(preview.contains("snapshot taken at aa44620"));
        assert!(preview.contains("stage"));
        assert!(preview.contains("commit"));
        assert!(preview.contains("push"));
        assert!(preview.contains("Warnings"));
        assert!(preview.contains("behind upstream"));
        // Box border must be gone
        assert!(!preview.contains("╔"));
        assert!(!preview.contains("╚"));
    }

    #[test]
    fn test_build_preview_no_warnings() {
        let v = DefaultValidator;
        let steps = vec![PipelineStep::Status];
        let ctx = crate::context::RepoContext::empty();
        let preview = v.build_preview(&steps, &[], &ctx, "");
        assert!(preview.contains("Plan"));
        assert!(!preview.contains("Warnings"));
    }

    #[test]
    fn test_format_step_label_uses_glit_verbs() {
        let cases = [
            (
                PipelineStep::Add {
                    files: vec!["a.rs".to_string()],
                },
                "stage",
            ),
            (
                PipelineStep::Commit {
                    message: "fix".to_string(),
                },
                "commit",
            ),
            (
                PipelineStep::Push {
                    remote: "origin".to_string(),
                    branch: "main".to_string(),
                    set_upstream: false,
                },
                "push",
            ),
            (PipelineStep::Stash, "stash"),
        ];
        for (step, expected_verb) in cases {
            let (verb, _detail) = format_step_label(&step);
            assert_eq!(verb, expected_verb, "step did not produce expected verb");
            // No raw "git " prefix anywhere
            assert!(
                !verb.starts_with("git "),
                "verb should not start with 'git '"
            );
        }
    }

    #[test]
    fn test_post_check_add_verifies_staged() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        std::fs::write(dir.path().join("new_file.rs"), "fn main() {}").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("new_file.rs")).unwrap();
        index.write().unwrap();

        let v = DefaultValidator;
        let pre = capture_pre_step_state(&repo);
        let result = v
            .run_post_check(
                &repo,
                &PipelineStep::Add {
                    files: vec!["new_file.rs".to_string()],
                },
                &pre,
            )
            .unwrap();
        assert!(matches!(result, PostCheckResult::Verified));
    }

    #[test]
    fn test_post_check_commit_verifies_head_advanced() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        let pre = capture_pre_step_state(&repo);

        // Make a commit
        std::fs::write(dir.path().join("test.txt"), "updated").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = git2::Signature::now("Test", "test@test.com").unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "update test file",
                &tree,
                &[&parent],
            )
            .unwrap();
        }

        let v = DefaultValidator;
        let result = v
            .run_post_check(
                &repo,
                &PipelineStep::Commit {
                    message: "update test file".to_string(),
                },
                &pre,
            )
            .unwrap();
        assert!(matches!(result, PostCheckResult::Verified));
    }

    /// Build a repo whose current branch and `feature` diverge on the same line,
    /// then shell out to real `git` to start a rebase that stops on conflict.
    /// Mid-rebase, HEAD is detached and `.git/rebase-merge/` exists (verified
    /// libgit2 behavior — S12). Returns a freshly-opened repo handle.
    fn setup_conflicting_rebase(dir: &TempDir) -> Repository {
        let repo = init_repo(dir);
        let path = dir.path();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();

        let write_tree = |content: &str| -> git2::Oid {
            std::fs::write(path.join("f.txt"), content).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };

        // base commit on the default branch (main/master)
        let tree_base = repo.find_tree(write_tree("base\n")).unwrap();
        let base = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree_base, &[])
            .unwrap();
        let base_commit = repo.find_commit(base).unwrap();

        // feature commit as a child of base, on refs/heads/feature (HEAD stays put)
        let tree_feat = repo.find_tree(write_tree("feature\n")).unwrap();
        repo.commit(
            Some("refs/heads/feature"),
            &sig,
            &sig,
            "feature",
            &tree_feat,
            &[&base_commit],
        )
        .unwrap();

        // main commit as a child of base, advancing the current branch; leaves
        // workdir+index at "main\n" so the tree is clean for `git rebase`.
        let tree_main = repo.find_tree(write_tree("main\n")).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "main",
            &tree_main,
            &[&base_commit],
        )
        .unwrap();

        // Real git so the rebase state files are produced faithfully.
        let out = std::process::Command::new("git")
            .args(["rebase", "feature"])
            .current_dir(path)
            .output()
            .expect("failed to run git rebase");
        // The rebase is EXPECTED to stop on conflict (non-zero exit).
        assert!(
            !out.status.success(),
            "git rebase should have conflicted, but succeeded"
        );

        Repository::open(path).unwrap()
    }

    // Phase 9's mid-rebase guard, now expressed through the Phase 10 single
    // path: detect_situation → situation_gate. The S12 property still holds —
    // mid-rebase never advises "checkout a branch".
    #[test]
    fn test_mid_rebase_gate_blocks_without_checkout_advice() {
        let dir = TempDir::new().unwrap();
        let repo = setup_conflicting_rebase(&dir);

        let situation = crate::context::detect_situation(&repo);
        assert!(
            matches!(situation, RepoSituation::MidRebase { .. }),
            "expected MidRebase, got {:?}",
            situation
        );

        let intent = IntentType::ScopedWrite(ScopedWriteOp::Commit {
            message: crate::commands::CommitMessage::Provided("wip".into()),
        });
        match situation_gate(&situation, &intent) {
            SituationVerdict::Blocked { message } => {
                assert!(
                    !message.contains("Checkout a branch")
                        && !message.to_lowercase().contains("detached head"),
                    "mid-rebase must not advise checkout/detached: {}",
                    message
                );
                assert!(
                    message.to_lowercase().contains("rebase"),
                    "should name the rebase: {}",
                    message
                );
            }
            _ => panic!("commit mid-rebase must be Blocked"),
        }
    }

    #[test]
    fn test_post_check_commit_verifies_merge_two_parents() {
        use crate::git_ops::{Git2Operations, GitOperations};
        let dir = TempDir::new().unwrap();
        let repo = init_repo(&dir);
        let path = dir.path();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();

        let stage = |content: &str| -> git2::Oid {
            std::fs::write(path.join("f.txt"), content).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };

        let tree_base = repo.find_tree(stage("base\n")).unwrap();
        let base = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree_base, &[])
            .unwrap();
        let base_commit = repo.find_commit(base).unwrap();

        let tree_theirs = repo.find_tree(stage("theirs\n")).unwrap();
        let theirs = repo
            .commit(
                Some("refs/heads/theirs"),
                &sig,
                &sig,
                "theirs",
                &tree_theirs,
                &[&base_commit],
            )
            .unwrap();

        let tree_ours = repo.find_tree(stage("ours\n")).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "ours",
            &tree_ours,
            &[&base_commit],
        )
        .unwrap();

        let their_ac = repo.find_annotated_commit(theirs).unwrap();
        repo.merge(&[&their_ac], None, None).unwrap();

        // Resolve the conflict.
        std::fs::write(path.join("f.txt"), "resolved\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("f.txt")).unwrap();
        index.write().unwrap();

        // Capture pre-state while still mid-merge, then land the merge commit.
        let pre = capture_pre_step_state(&repo);
        assert!(
            pre.was_merging,
            "pre-state should record the in-progress merge"
        );
        Git2Operations.commit(&repo, "merge theirs").unwrap();

        let v = DefaultValidator;
        let result = v
            .run_post_check(
                &repo,
                &PipelineStep::Commit {
                    message: "merge theirs".to_string(),
                },
                &pre,
            )
            .unwrap();
        assert!(
            matches!(result, PostCheckResult::Verified),
            "expected Verified for a correct merge commit, got {:?}",
            result
        );
    }

    #[test]
    fn test_hook_precheck_copy_reflects_routing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let hooks = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let v = DefaultValidator;
        let steps = vec![PipelineStep::Commit {
            message: "x".to_string(),
        }];

        let warns = |mode: &str| -> Vec<String> {
            v.run_pre_checks(&repo, &steps, mode)
                .unwrap()
                .into_iter()
                .filter_map(|r| match r {
                    PreCheckResult::Warn(w) => Some(w),
                    _ => None,
                })
                .collect()
        };

        // The false "will run during execution" copy is retired everywhere.
        let auto = warns("auto");
        assert!(
            !auto.iter().any(|w| w.contains("will run during execution")),
            "stale hook copy present: {:?}",
            auto
        );
        // auto + present → routed through system git so hooks run.
        assert!(
            auto.iter().any(|w| w.contains("system git")),
            "expected a system-git routing note; got {:?}",
            auto
        );
        // git2 → hooks are skipped, and the copy says so.
        let git2 = warns("git2");
        assert!(
            git2.iter().any(|w| w.contains("SKIPPED")),
            "expected a skipped note under hooks=git2; got {:?}",
            git2
        );
    }

    // Verification centerpiece: the full §13 situation × intent matrix. Pure
    // function → no fixtures, fully deterministic.
    #[test]
    fn test_situation_gate_matrix() {
        use crate::commands::{CommitMessage, FileSelection, ReadOnlyOp, SyncStrategy};

        let mk = |tag: &str| -> IntentType {
            match tag {
                "ro" => IntentType::ReadOnly(ReadOnlyOp::Status),
                "log" => IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }),
                "diff" => IntentType::ReadOnly(ReadOnlyOp::Diff { path: None }),
                "commit" => IntentType::ScopedWrite(ScopedWriteOp::Commit {
                    message: CommitMessage::Provided("m".into()),
                }),
                "push" => IntentType::ScopedWrite(ScopedWriteOp::Push {
                    remote: "origin".into(),
                }),
                "checkout" => IntentType::ScopedWrite(ScopedWriteOp::Checkout {
                    target: "main".into(),
                }),
                "branch" => IntentType::ScopedWrite(ScopedWriteOp::Branch { name: "x".into() }),
                "pull" => IntentType::ScopedWrite(ScopedWriteOp::Pull {
                    remote: "origin".into(),
                }),
                // Destructive / fall-through ops — never exercised before, so a
                // mis-categorization in the is_*_like predicates used to pass CI
                // green (finding #12). Pin them here.
                "force" => IntentType::ScopedWrite(ScopedWriteOp::ForcePush {
                    remote: "origin".into(),
                }),
                "discard" => IntentType::ScopedWrite(ScopedWriteOp::DiscardLocal {
                    remote: "origin".into(),
                }),
                "pullrebase" => IntentType::ScopedWrite(ScopedWriteOp::PullRebase {
                    remote: "origin".into(),
                }),
                "reset" => IntentType::ScopedWrite(ScopedWriteOp::Reset {
                    mode: ResetMode::Hard,
                }),
                "undo" => IntentType::ScopedWrite(ScopedWriteOp::Undo),
                "stash" => IntentType::ScopedWrite(ScopedWriteOp::Stash),
                "add" => IntentType::ScopedWrite(ScopedWriteOp::Add {
                    files: FileSelection::All,
                }),
                "acp" => IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush {
                    feature_hint: "x".into(),
                }),
                "commitpush" => IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
                    message: CommitMessage::Provided("m".into()),
                }),
                "sync" => IntentType::FeaturePipeline(FeaturePipelineOp::Sync {
                    strategy: SyncStrategy::Rebase,
                }),
                _ => unreachable!(),
            }
        };
        let z = git2::Oid::zero();

        // (situation, intent tag, None = Allowed | Some(substr) = Blocked w/ substr)
        let rows: Vec<(RepoSituation, &str, Option<&str>)> = vec![
            (RepoSituation::Normal, "ro", None),
            (RepoSituation::Normal, "commit", None),
            (RepoSituation::Normal, "push", None),
            (RepoSituation::Normal, "checkout", None),
            (RepoSituation::Normal, "branch", None),
            (RepoSituation::Normal, "pull", None),
            (RepoSituation::EmptyRepo, "ro", None),
            (RepoSituation::EmptyRepo, "commit", None),
            (RepoSituation::EmptyRepo, "branch", None),
            (RepoSituation::EmptyRepo, "push", Some("commits yet")),
            (RepoSituation::EmptyRepo, "pull", Some("commits yet")),
            (RepoSituation::EmptyRepo, "checkout", Some("commits yet")),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 0,
                },
                "ro",
                None,
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 0,
                },
                "checkout",
                None,
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 0,
                },
                "branch",
                None,
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 0,
                },
                "commit",
                Some("Checkout a branch"),
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 0,
                },
                "push",
                Some("Checkout a branch"),
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 0,
                },
                "pull",
                Some("Checkout a branch"),
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 2,
                },
                "ro",
                None,
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 2,
                },
                "branch",
                None,
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 2,
                },
                "checkout",
                Some("not on any branch"),
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 2,
                },
                "commit",
                Some("Checkout a branch"),
            ),
            (
                RepoSituation::DetachedHead {
                    stranded_commits: 2,
                },
                "push",
                Some("Checkout a branch"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec!["f".into()],
                    resolved: false,
                },
                "ro",
                None,
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec!["f".into()],
                    resolved: false,
                },
                "commit",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec!["f".into()],
                    resolved: false,
                },
                "push",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec!["f".into()],
                    resolved: false,
                },
                "checkout",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec!["f".into()],
                    resolved: false,
                },
                "pull",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec![],
                    resolved: true,
                },
                "ro",
                None,
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec![],
                    resolved: true,
                },
                "commit",
                None,
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec![],
                    resolved: true,
                },
                "push",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec![],
                    resolved: true,
                },
                "checkout",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidMerge {
                    conflicts: vec![],
                    resolved: true,
                },
                "branch",
                Some("merge is in progress"),
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "ro",
                None,
            ),
            // Read-only ops stay Allowed even in a blocking state — pins the
            // ReadOnly(_) short-circuit so a future reclassification of Log/Diff
            // out of ReadOnlyOp would fail here rather than silently regress.
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "log",
                None,
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "diff",
                None,
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "commit",
                Some("rebase is in progress"),
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "push",
                Some("rebase is in progress"),
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "checkout",
                Some("rebase is in progress"),
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "pull",
                Some("rebase is in progress"),
            ),
            (
                RepoSituation::MidCherryPick { conflicted: vec![] },
                "ro",
                None,
            ),
            (
                RepoSituation::MidCherryPick { conflicted: vec![] },
                "commit",
                Some("cherry-pick is in progress"),
            ),
            (
                RepoSituation::MidCherryPick { conflicted: vec![] },
                "push",
                Some("cherry-pick is in progress"),
            ),
            (RepoSituation::MidRevert { conflicted: vec![] }, "ro", None),
            (
                RepoSituation::MidRevert { conflicted: vec![] },
                "commit",
                Some("revert is in progress"),
            ),
            (RepoSituation::MidBisect, "ro", None),
            (
                RepoSituation::MidBisect,
                "commit",
                Some("bisect is in progress"),
            ),
            (
                RepoSituation::MidBisect,
                "push",
                Some("bisect is in progress"),
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "ro",
                None,
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "commit",
                None,
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "pull",
                None,
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "push",
                Some("diverged"),
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "ro",
                None,
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "commit",
                None,
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "checkout",
                None,
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "pull",
                Some("remote history changed"),
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "push",
                Some("remote history changed"),
            ),
            (RepoSituation::StaleLock { age_secs: 600 }, "ro", None),
            (RepoSituation::StaleLock { age_secs: 600 }, "log", None),
            (RepoSituation::StaleLock { age_secs: 600 }, "diff", None),
            (
                RepoSituation::StaleLock { age_secs: 600 },
                "commit",
                Some("index.lock"),
            ),
            (
                RepoSituation::StaleLock { age_secs: 600 },
                "push",
                Some("index.lock"),
            ),
            (
                RepoSituation::StaleLock { age_secs: 600 },
                "checkout",
                Some("index.lock"),
            ),
            (
                RepoSituation::StaleLock { age_secs: 600 },
                "pull",
                Some("index.lock"),
            ),
            // ---- Destructive / fall-through op coverage (finding #12) ----
            // push-like family (Push/ForcePush/AddCommitPush/CommitPush) blocks
            // wherever plain push blocks.
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "force",
                Some("diverged"),
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "acp",
                Some("diverged"),
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "commitpush",
                Some("diverged"),
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "force",
                Some("remote history changed"),
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "commitpush",
                Some("remote history changed"),
            ),
            (RepoSituation::EmptyRepo, "acp", Some("commits yet")),
            (RepoSituation::EmptyRepo, "commitpush", Some("commits yet")),
            // reconcile family (Pull/PullRebase/DiscardLocal/Sync) blocks under a
            // rewritten remote. Under plain divergence it is still Allowed (the
            // rebase-vs-merge ChoiceGate is Phase 12; destructive DiscardLocal/Reset
            // in a diverged tree stay guarded by danger.rs type-to-confirm — the
            // known matrix gap in finding #11, ticketed, encoded here as current
            // behavior so a future tightening trips this row).
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "discard",
                Some("remote history changed"),
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "pullrebase",
                Some("remote history changed"),
            ),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "sync",
                Some("remote history changed"),
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "discard",
                None,
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "pullrebase",
                None,
            ),
            (
                RepoSituation::Diverged {
                    ahead: 1,
                    behind: 1,
                },
                "sync",
                None,
            ),
            // fall-through writes (Reset/Undo/Stash) block in every in-progress
            // state and on an empty repo, but are neither push-like nor reconcile,
            // so a rewritten remote leaves them Allowed.
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "reset",
                Some("rebase is in progress"),
            ),
            (
                RepoSituation::MidRebase {
                    current: 1,
                    total: 3,
                    conflicted: vec!["f".into()],
                },
                "stash",
                Some("rebase is in progress"),
            ),
            (
                RepoSituation::MidBisect,
                "reset",
                Some("bisect is in progress"),
            ),
            (RepoSituation::EmptyRepo, "reset", Some("commits yet")),
            (RepoSituation::EmptyRepo, "undo", Some("commits yet")),
            (
                RepoSituation::RemoteRewritten {
                    old_tip: z,
                    new_tip: z,
                },
                "reset",
                None,
            ),
            // commit-like Add is the staging step allowed on an empty repo and on
            // a resolved merge.
            (RepoSituation::EmptyRepo, "add", None),
            (
                RepoSituation::MidMerge {
                    conflicts: vec![],
                    resolved: true,
                },
                "add",
                None,
            ),
        ];

        for (situation, tag, expect) in &rows {
            let intent = mk(tag);
            let verdict = situation_gate(situation, &intent);
            match *expect {
                None => assert!(
                    matches!(verdict, SituationVerdict::Allowed),
                    "expected Allowed: {:?} + {}",
                    situation,
                    tag
                ),
                Some(substr) => match verdict {
                    SituationVerdict::Blocked { message } => assert!(
                        message.contains(substr),
                        "{:?} + {}: message {:?} missing {:?}",
                        situation,
                        tag,
                        message,
                        substr
                    ),
                    _ => panic!("expected Blocked: {:?} + {}", situation, tag),
                },
            }
        }
    }
}
