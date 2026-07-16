//! Single source of truth for user-facing string constants touched by the UX
//! overhaul. Add strings here as they're touched — incremental migration, not
//! a one-shot rewrite.

// =============================================================================
// Engine indicators (Item 2 — design handoff §M1)
// =============================================================================

pub const ENGINE_FAST_GLYPH: &str = "⚡";
pub const ENGINE_LLM_GLYPH: &str = "◉";
pub const ENGINE_FAST_ASCII: &str = "*";
pub const ENGINE_LLM_ASCII: &str = "@";

// =============================================================================
// Plan-execution box (Item 4 — design handoff §Flow 06½)
// =============================================================================

pub const PLAN_HEADER_FMT: &str = "Plan  · {n} step(s) · snapshot @ {sha7}";

/// Bullet glyphs for the timeline visual.
pub const TIMELINE_PENDING: &str = "○";
pub const TIMELINE_RUNNING: &str = "●";
pub const TIMELINE_DONE: &str = "●";
pub const TIMELINE_FAILED: &str = "✗";
pub const TIMELINE_CONNECTOR: &str = "│";

// ASCII fallbacks for non-unicode terminals.
pub const TIMELINE_PENDING_ASCII: &str = "o";
pub const TIMELINE_RUNNING_ASCII: &str = "*";
pub const TIMELINE_DONE_ASCII: &str = "*";
pub const TIMELINE_FAILED_ASCII: &str = "X";
pub const TIMELINE_CONNECTOR_ASCII: &str = "|";

// Keymap legend shown below the plan. Trimmed to what actually works today
// (focus/edit/skip are aspirational — see prototype.jsx Variant C); avoiding
// the dishonesty of advertising unwired keys.
pub const PLAN_KEYMAP_LEGEND: &str = "[Enter] run · [q] cancel";

pub const PLAN_DONE_FOOTER: &str = "Done";
pub const PLAN_NEXT_HINT: &str = "Next: glit \"status\" · undo with `glit recover`";

pub const PLAN_CANCELED_FMT: &str = "↪ Canceled · staged work preserved · snapshot @ {sha7}";

// =============================================================================
// In-progress-operation / detached-HEAD pre-check blocks (Phase 9.1 — S12)
//
// Mid-rebase (and other in-progress ops) detach HEAD. Telling the user to
// "checkout a branch" there abandons the operation and strands commits — the
// single most identity-damaging pothole in the scenario map. The advice must
// name the operation actually in progress; "checkout a branch" is correct ONLY
// when the repo is otherwise Clean (a plain detached checkout, S20).
// =============================================================================

// (Phase 12: the detached-HEAD write case no longer Blocks with a static
// string — `situation_gate` returns `Redirect(DetachedHeadRescue)` and the gate
// drives the branch-here / switch-back way out.)
// Mid-operation blocks (updated in Phase 11 — GLit now drives continue/abort
// itself). Abort is named FIRST (the anxious user's exit), then the finish path,
// both via GLit's own verbs. The "<op> is in progress" phrase is load-bearing:
// the §13 matrix test asserts it as a substring — keep it when editing.
pub const REBASE_IN_PROGRESS_BLOCK: &str =
    "A rebase is in progress. Back out with `glit \"abort rebase\"`, or resolve the conflicts and `glit \"continue rebase\"`.";
pub const MERGE_IN_PROGRESS_BLOCK: &str =
    "A merge is in progress. Back out with `glit \"abort merge\"`, or resolve the conflicts and commit.";
pub const CHERRY_PICK_IN_PROGRESS_BLOCK: &str =
    "A cherry-pick is in progress. Back out with `glit \"abort cherry-pick\"`, or resolve the conflicts and `glit \"continue cherry-pick\"`.";
pub const REVERT_IN_PROGRESS_BLOCK: &str =
    "A revert is in progress. Back out with `glit \"abort revert\"`, or resolve the conflicts and `glit \"continue revert\"`.";
pub const BISECT_IN_PROGRESS_BLOCK: &str =
    "A bisect is in progress. Finish with `git bisect reset` before making changes.";

// Phase 11 — continue/skip/abort gate messages.
/// A `*Continue` was requested while conflicts remain unresolved in the index.
/// Names the files so the user knows exactly what to resolve.
pub fn continue_conflicts_block(files: &[String]) -> String {
    format!(
        "{} file(s) still conflicted: {} — resolve and stage them, then continue.",
        files.len(),
        files.join(", ")
    )
}
/// A continue/skip/abort was requested but no matching operation is in progress
/// (e.g. `abort rebase` in a clean repo). `op` is the git operation word.
pub fn no_inprogress_block(op: &str) -> String {
    format!("There's no {op} in progress.")
}

// Phase 11 — situation-aware recovery (`glit --recover`). Recovery must never
// fight git's own state machine (see recovery_plan in recovery.rs).
pub const RECOVER_MID_OP_ADVICE: &str =
    "A git operation is still in progress — GLit won't reset refs underneath it. Back out with git's own `--abort` (e.g. `git rebase --abort`), or finish it, then retry recovery if still needed.";
pub const RECOVER_DETACHED_WARN: &str =
    "The interrupted operation appears to have completed (the repo is clean, but the snapshot points at a detached commit). Restoring would leave you on a detached HEAD, stranding that commit.";

// =============================================================================
// Hook-aware routing (Phase 9.3 — S47)
// =============================================================================

/// Printed before a commit that GLit routes through system `git` so hooks run.
pub const HOOKS_USING_SYSTEM_GIT: &str = "hooks detected — using system git so they run.";

// =============================================================================
// Situation-gate block messages (Phase 10 — §13 matrix). Each states the
// situation AND the way out. In-progress states reuse the Phase 9 constants
// above (REBASE/MERGE/CHERRY_PICK/REVERT/BISECT_IN_PROGRESS_BLOCK,
// DETACHED_HEAD_BLOCK). Dynamic ones (counts/age) are built by the fns below.
// =============================================================================

pub const EMPTY_REPO_BLOCK: &str =
    "No commits yet — you can stage and make the first commit, but push/pull need a commit first.";
pub const REMOTE_REWRITTEN_BLOCK: &str =
    "The remote history changed under you — don't reconcile blindly. Fetch and inspect before pulling or pushing.";

pub fn detached_stranded_block(n: u32) -> String {
    format!("You have {n} commit(s) not on any branch — create a branch here first to keep them.")
}
pub fn stale_lock_block(age_secs: u64) -> String {
    format!("A stale .git/index.lock (~{age_secs}s) is blocking writes — remove it and retry.")
}

// =============================================================================
// Divergence reconciliation (Phase 12 — S9/S10/S13). The rebase-vs-merge
// ChoiceGate consequence sentences + the calm conflict handoff + Sync notes.
// =============================================================================

/// Consequence sentence for the Rebase option. `cross_branch` (Sync) adds the
/// force-push warning, because rebasing an already-pushed branch rewrites
/// commits that exist on its remote (Amendment 1).
pub fn reconcile_rebase_consequence(ahead: u32, cross_branch: bool) -> String {
    let base = format!(
        "replay your {ahead} commit(s) on top of theirs; history stays linear. \
         Recommended when nobody else has your commits."
    );
    if cross_branch {
        format!("{base} If this branch was already pushed, rebase will require a force push.")
    } else {
        base
    }
}
pub const RECONCILE_MERGE_CONSEQUENCE: &str =
    "keep both histories, adds a merge commit. Safer if others may have pulled your commits.";
pub const RECONCILE_ABORT_CONSEQUENCE: &str = "do nothing.";

/// The calm handoff when a reconcile step pauses on conflicts. `op` is "rebase"
/// or "merge"; merge has no `--continue`, so its finish is "resolve and commit".
pub fn reconcile_paused_block(op: &str, n: usize) -> String {
    if op == "merge" {
        format!(
            "Paused on conflicts in {n} file(s) — nothing is lost. Resolve them and commit, \
             or `glit \"abort merge\"` to return to where you started."
        )
    } else {
        format!(
            "Paused on conflicts in {n} file(s) — nothing is lost. Resolve them, then \
             `glit \"continue {op}\"`, or `glit \"abort {op}\"` to return to where you started."
        )
    }
}

// Sync (cross-branch) completion notes. Phase 13 completes the force-push flow
// in-band (PushedCommitRewrite escalation + force-with-lease), so the note now
// only covers the case where the user DECLINED the force push.
pub fn sync_rebase_force_note(target: &str) -> String {
    format!(
        "Rebased onto {target}. Not pushed — pushing this rebased branch needs a force push; \
         re-run and confirm, or push manually with `git push --force-with-lease`."
    )
}
pub const SYNC_MERGE_BEHIND_NOTE: &str =
    "Merged, but your branch is behind its own upstream — not pushing. Reconcile the remote branch separately.";

// Degenerate-count notes (Amendment 2 — the ChoiceGate never renders).
pub fn reconcile_up_to_date(target: &str) -> String {
    format!("Already up to date with {target} — nothing to reconcile.")
}
pub fn reconcile_fast_forwarded(target: &str) -> String {
    format!("Fast-forwarded to {target} — no merge or rebase needed.")
}

// =============================================================================
// Undo disambiguation gate (Phase 13 — S34/P6). The classifier only recognizes
// undo-language → UndoIntent; this deterministic ChoiceGate decides the
// semantic. The four consequence sentences + the pushed-reorder rationale.
// =============================================================================

/// The gate question, always naming the target commit so the user sees exactly
/// what is being undone.
pub fn undo_gate_question(short_sha: &str, summary: &str) -> String {
    format!("Undo {short_sha} \"{summary}\" — what should happen to its changes?")
}

pub const UNDO_KEEP_STAGED_CONSEQUENCE: &str =
    "keep them staged (soft reset) — the commit is removed, files stay ready to re-commit.";
pub const UNDO_KEEP_IN_FILES_CONSEQUENCE: &str =
    "keep them in your files, unstaged (mixed reset) — re-stage what you want before committing.";
pub const UNDO_REVERT_CONSEQUENCE: &str =
    "add an inverse commit (revert) — history is preserved; safe even after pushing.";
pub const UNDO_DISCARD_CONSEQUENCE: &str =
    "discard them completely (hard reset) — the changes are gone (reflog holds them ~90 days). Requires typing to confirm.";

/// Prepended to the Revert consequence when HEAD is already on the remote — the
/// gate reorders Revert first and says why.
pub const UNDO_PUSHED_WHY: &str =
    "This commit is already on the remote, so revert is the safe default: ";

/// Guidance returned (as an Err) when the user asked to undo more than one
/// commit — v1 undo is single-HEAD only (P6: never act on an LLM-extracted count).
pub fn undo_multi_commit_guidance(n: u32) -> String {
    format!(
        "Multi-commit undo isn't supported yet. Did you mean: squash the last {n} into one \
         (glit \"squash last {n}\"), revert a specific commit, or undo just the most recent \
         (glit \"undo\")?"
    )
}

// =============================================================================
// PushedCommitRewrite escalation (Phase 13 — S33). Rewriting a published commit
// (amend / squash) needs a force push; escalate with a type-to-confirm.
// =============================================================================

/// The type-to-confirm message when an amend/squash targets a pushed commit.
pub fn pushed_rewrite_confirm(branch: &str) -> String {
    format!(
        "This rewrites a commit that's already on the remote — pushing it needs a force push. \
         Type '{branch}' to confirm the rewrite + force-push (with lease):"
    )
}

/// Info line shown when the pushed-rewrite escalation is entered.
pub const PUSHED_REWRITE_WARNING: &str =
    "Heads up: the target commit is already published. Amending/squashing it rewrites shared history.";
