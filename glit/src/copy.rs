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

pub const PLAN_CANCELED_FMT: &str =
    "↪ Canceled · staged work preserved · snapshot @ {sha7}";

// =============================================================================
// In-progress-operation / detached-HEAD pre-check blocks (Phase 9.1 — S12)
//
// Mid-rebase (and other in-progress ops) detach HEAD. Telling the user to
// "checkout a branch" there abandons the operation and strands commits — the
// single most identity-damaging pothole in the scenario map. The advice must
// name the operation actually in progress; "checkout a branch" is correct ONLY
// when the repo is otherwise Clean (a plain detached checkout, S20).
// =============================================================================

pub const DETACHED_HEAD_BLOCK: &str =
    "Repository is in detached HEAD state. Checkout a branch before making changes.";
pub const REBASE_IN_PROGRESS_BLOCK: &str =
    "A rebase is in progress. GLit can't act safely mid-rebase yet — finish with `git rebase --continue` or back out with `git rebase --abort`.";
pub const MERGE_IN_PROGRESS_BLOCK: &str =
    "A merge is in progress. Resolve the conflicts and commit, or back out with `git merge --abort`.";
pub const CHERRY_PICK_IN_PROGRESS_BLOCK: &str =
    "A cherry-pick is in progress. Finish with `git cherry-pick --continue` or back out with `git cherry-pick --abort`.";
pub const REVERT_IN_PROGRESS_BLOCK: &str =
    "A revert is in progress. Finish with `git revert --continue` or back out with `git revert --abort`.";
pub const BISECT_IN_PROGRESS_BLOCK: &str =
    "A bisect is in progress. Finish with `git bisect reset` before making changes.";

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
pub fn diverged_block(ahead: u32, behind: u32) -> String {
    format!("Your branch has diverged — ahead {ahead}, behind {behind}. Reconcile (pull or rebase) before pushing.")
}
pub fn stale_lock_block(age_secs: u64) -> String {
    format!("A stale .git/index.lock (~{age_secs}s) is blocking writes — remove it and retry.")
}
