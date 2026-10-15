//! Single source of truth for user-facing string constants touched by the UX
//! overhaul. Add strings here as they're touched — incremental migration, not
//! a one-shot rewrite.

// =============================================================================
// Engine indicators (Item 2 — design handoff §M1)
// =============================================================================

pub const ENGINE_FAST_GLYPH: &str = "⚡";
pub const ENGINE_KEV_GLYPH: &str = "✦";
pub const ENGINE_USER_GLYPH: &str = "◇";
pub const ENGINE_FAST_ASCII: &str = "*";
pub const ENGINE_KEV_ASCII: &str = "+";
pub const ENGINE_USER_ASCII: &str = ">";

// =============================================================================
// Plan-execution box (Item 4 — design handoff §Flow 06½)
// =============================================================================

pub const PLAN_HEADER_FMT: &str = "Plan  · {n} step(s) · snapshot @ {sha7}";

/// Bullet glyphs for the timeline visual.
pub const TIMELINE_PENDING: &str = "○";
pub const TIMELINE_RUNNING: &str = "●";
pub const TIMELINE_DONE: &str = "✓";
pub const TIMELINE_FAILED: &str = "✗";
pub const TIMELINE_CONNECTOR: &str = "│";

// ASCII fallbacks for non-unicode terminals.
pub const TIMELINE_PENDING_ASCII: &str = "o";
pub const TIMELINE_RUNNING_ASCII: &str = "*";
pub const TIMELINE_DONE_ASCII: &str = "ok";
pub const TIMELINE_FAILED_ASCII: &str = "X";
pub const TIMELINE_CONNECTOR_ASCII: &str = "|";

// Keymap legend shown below the plan. Trimmed to what actually works today
// (focus/edit/skip are aspirational — see prototype.jsx Variant C); avoiding
// the dishonesty of advertising unwired keys.
pub const PLAN_KEYMAP_LEGEND: &str = "[Enter] run · [q] cancel";

/// Said when `s` drops the plan's trailing push.
pub const PLAN_PUSH_SKIPPED: &str =
    "↳ Not pushing this time — the commit stays local. Push later with gitrust \"push\".";

/// The plan prompt's legend: only the keys that do something for this plan.
pub fn plan_legend(can_edit_message: bool, can_skip_push: bool) -> String {
    let mut keys = vec!["[Enter] run"];
    if can_edit_message {
        keys.push("[e] edit message");
    }
    if can_skip_push {
        keys.push("[s] don't push");
    }
    keys.push("[q] cancel");
    keys.join(" · ")
}

/// Highlight `[key]` labels in a legend (accent yellow, design handoff colour
/// table) when color is on; plain text otherwise.
pub fn style_keys(legend: &str) -> String {
    if !crate::danger::use_color() {
        return legend.to_string();
    }
    let mut out = String::new();
    let mut rest = legend;
    while let Some(i) = rest.find('[') {
        match rest[i..].find(']') {
            Some(j) => {
                out.push_str(&rest[..i]);
                out.push_str(&format!("\x1b[38;5;220m{}\x1b[0m", &rest[i..i + j + 1]));
                rest = &rest[i + j + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

pub const PLAN_DONE_FOOTER: &str = "Done";

/// The line under "✓ Done". Honest about the way back: `gitrust "undo"` undoes
/// the last commit (offering a revert when it's already pushed); it is only
/// suggested when this run made a commit. (`gitrust recover` restores a
/// *crashed* run's snapshot — after a successful run there is none, so it was
/// the wrong thing to point at.)
pub fn plan_next_hint(commits_made: usize, pushed: bool, ahead: usize) -> String {
    match (commits_made > 0, pushed) {
        (true, true) => "Next: gitrust \"status\" · changed your mind? gitrust \"undo\" (reverts it — it's pushed)".to_string(),
        (true, false) if ahead > 0 => "Next: gitrust \"push\" · changed your mind? gitrust \"undo\"".to_string(),
        (true, false) => "Next: gitrust \"status\" · changed your mind? gitrust \"undo\"".to_string(),
        (false, _) => "Next: gitrust \"status\"".to_string(),
    }
}

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
// Mid-operation blocks (updated in Phase 11 — GiTrust now drives continue/abort
// itself). Abort is named FIRST (the anxious user's exit), then the finish path,
// both via GiTrust's own verbs. The "<op> is in progress" phrase is load-bearing:
// the §13 matrix test asserts it as a substring — keep it when editing.
pub const REBASE_IN_PROGRESS_BLOCK: &str =
    "A rebase is in progress. Back out with `gitrust \"abort rebase\"`, or resolve the conflicts and `gitrust \"continue rebase\"`.";
pub const MERGE_IN_PROGRESS_BLOCK: &str =
    "A merge is in progress. Back out with `gitrust \"abort merge\"`, or resolve the conflicts and commit.";
pub const CHERRY_PICK_IN_PROGRESS_BLOCK: &str =
    "A cherry-pick is in progress. Back out with `gitrust \"abort cherry-pick\"`, or resolve the conflicts and `gitrust \"continue cherry-pick\"`.";
pub const REVERT_IN_PROGRESS_BLOCK: &str =
    "A revert is in progress. Back out with `gitrust \"abort revert\"`, or resolve the conflicts and `gitrust \"continue revert\"`.";
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

// Phase 11 — situation-aware recovery (`gitrust --recover`). Recovery must never
// fight git's own state machine (see recovery_plan in recovery.rs).
pub const RECOVER_MID_OP_ADVICE: &str =
    "A git operation is still in progress — GiTrust won't reset refs underneath it. Back out with git's own `--abort` (e.g. `git rebase --abort`), or finish it, then retry recovery if still needed.";
pub const RECOVER_DETACHED_WARN: &str =
    "The interrupted operation appears to have completed (the repo is clean, but the snapshot points at a detached commit). Restoring would leave you on a detached HEAD, stranding that commit.";

// =============================================================================
// Hook-aware routing (Phase 9.3 — S47)
// =============================================================================

/// Printed before a commit that GiTrust routes through system `git` so hooks run.
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

// (Phase 14: the detached-stranded checkout no longer dead-ends in a block — it
// redirects to the branch-here rescue, so the old `detached_stranded_block`
// message is gone. See `handle_detached_head_rescue`.)
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

/// The calm handoff when a reconcile step pauses on conflicts. `op` is "rebase"
/// or "merge"; merge has no `--continue`, so its finish is "resolve and commit".
pub fn reconcile_paused_block(op: &str, n: usize) -> String {
    if op == "merge" {
        format!(
            "Paused on conflicts in {n} file(s) — nothing is lost. Resolve them and commit, \
             or `gitrust \"abort merge\"` to return to where you started."
        )
    } else {
        format!(
            "Paused on conflicts in {n} file(s) — nothing is lost. Resolve them, then \
             `gitrust \"continue {op}\"`, or `gitrust \"abort {op}\"` to return to where you started."
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
/// what is being undone. When it's already pushed, say once why Revert is
/// listed first.
pub fn undo_gate_question(short_sha: &str, summary: &str, pushed: bool) -> String {
    if pushed {
        format!(
            "Undo {short_sha} \"{summary}\"? It's already pushed, so reverting is the safe choice."
        )
    } else {
        format!("Undo {short_sha} \"{summary}\" — what should happen to its changes?")
    }
}

// Each option: a distinct label + a short consequence that doesn't repeat it.
pub const UNDO_KEEP_STAGED_LABEL: &str = "Undo, keep the changes staged";
pub const UNDO_KEEP_STAGED_CONSEQUENCE: &str =
    "the commit goes; its changes stay staged, ready to re-commit";
pub const UNDO_KEEP_IN_FILES_LABEL: &str = "Undo, keep the changes in my files";
pub const UNDO_KEEP_IN_FILES_CONSEQUENCE: &str =
    "the commit goes; its changes stay in your files, unstaged";
pub const UNDO_REVERT_LABEL: &str = "Undo with a revert commit";
pub const UNDO_REVERT_CONSEQUENCE: &str =
    "a new commit reverses it; history is kept — safe after pushing";
pub const UNDO_DISCARD_LABEL: &str = "Undo and throw the changes away";
pub const UNDO_DISCARD_CONSEQUENCE: &str =
    "both go (git keeps them ~90 days) · you'll type to confirm";

/// Guidance returned (as an Err) when the user asked to undo more than one
/// commit — v1 undo is single-HEAD only (P6: never act on an LLM-extracted count).
pub fn undo_multi_commit_guidance(n: u32) -> String {
    format!(
        "Multi-commit undo isn't supported yet. Did you mean: squash the last {n} into one \
         (gitrust \"squash last {n}\"), revert a specific commit, or undo just the most recent \
         (gitrust \"undo\")?"
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

// =============================================================================
// Phase 14 — forensic recovery, trash-net, destructive-phrase gate (S21/S24/
// S26/S27/S30/S62; P5).
// =============================================================================

/// `gitrust --recover` found no pending snapshot for this repo. Not an error —
/// point the user sideways to the forensic flow (the "recover" precedence rule).
pub const RECOVER_NONE_TRY_FORENSIC: &str =
    "No interrupted GiTrust pipeline found for this repo. If you're looking for lost commits, try `gitrust \"find my lost work\"`.";

// -- Status while git is stopped mid-operation.

/// "  1 conflicted file (listed below) — edit it and remove the markers."
/// The files themselves are listed once, in the status body.
pub fn in_progress_conflicts(files: &[String]) -> String {
    let n = files.len();
    format!(
        "  {} conflicted file{} (listed below) — edit {} and remove the <<<<<<< / >>>>>>> markers.",
        n,
        if n == 1 { "" } else { "s" },
        if n == 1 { "it" } else { "them" }
    )
}

/// The way forward (and back) for an in-progress operation. `family` is the
/// git word ("rebase", "merge", "cherry-pick", "revert"); the phrases are
/// GiTrust fast-path canonicals, so they run instantly.
pub fn in_progress_next(family: &str, conflicts_left: bool) -> String {
    let finish = if family == "merge" {
        "gitrust \"commit\"".to_string()
    } else {
        format!("gitrust \"continue {}\"", family)
    };
    if conflicts_left {
        format!(
            "  Then: {} · or go back to before it started: gitrust \"abort {}\"",
            finish, family
        )
    } else {
        format!(
            "  Conflicts resolved — finish with {} · or go back: gitrust \"abort {}\"",
            finish, family
        )
    }
}

/// Plan warning for a hard reset. True to what happens: the trash-net copies
/// the uncommitted changes first, and dropped commits stay in git's reflog.
pub const HARD_RESET_WARNING: &str =
    "Discards uncommitted changes in tracked files — they're backed up to ~/.gitrust/trash first; any dropped commit stays recoverable from git's reflog.";

/// Flow 05: say only the rules this input broke — "Git won't allow spaces or
/// parentheses in branch names." No "Error:", nothing has failed yet.
pub fn branch_invalid_intro(name: &str) -> String {
    let mut what: Vec<String> = Vec::new();
    if name.contains(char::is_whitespace) {
        what.push("spaces".into());
    }
    if name.contains('(') || name.contains(')') {
        what.push("parentheses".into());
    }
    if name.contains("..") {
        what.push("\"..\"".into());
    }
    let mut odd: Vec<char> = name
        .chars()
        .filter(|c| {
            !c.is_ascii_alphanumeric()
                && !c.is_whitespace()
                && !matches!(c, '-' | '_' | '/' | '.' | '(' | ')')
        })
        .collect();
    odd.dedup();
    if !odd.is_empty() {
        what.push(
            odd.iter()
                .map(|c| format!("'{}'", c))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if what.is_empty() {
        return format!("Git won't accept \"{}\" as a branch name.", name);
    }
    let list = match what.len() {
        1 => what[0].clone(),
        _ => format!(
            "{} or {}",
            what[..what.len() - 1].join(", "),
            what[what.len() - 1]
        ),
    };
    format!("Git won't allow {} in branch names.", list)
}

/// Label of the pre-filled branch-name prompt after an invalid name.
pub const BRANCH_NAME_SUGGESTED: &str = "Branch name (suggested — Enter to use, or edit):";

/// How old status's view of the remote is (ahead/behind come from the last
/// fetch). `secs` = age of .git/FETCH_HEAD; `None` = never fetched.
pub fn fetch_age_note(secs: Option<u64>) -> String {
    let Some(s) = secs else {
        return "Remote state: never fetched — gitrust \"fetch\" checks the remote.".to_string();
    };
    let age = match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => {
            let h = s / 3600;
            format!("{} hour{} ago", h, if h == 1 { "" } else { "s" })
        }
        _ => {
            let d = s / 86400;
            format!("{} day{} ago", d, if d == 1 { "" } else { "s" })
        }
    };
    format!(
        "Remote state as of the last fetch, {} — gitrust \"fetch\" refreshes it.",
        age
    )
}

/// Older than 10 minutes (or never): status shows the note in yellow.
pub fn fetch_is_stale(secs: Option<u64>) -> bool {
    secs.map_or(true, |s| s > 10 * 60)
}

/// Status suggestion when HEAD is detached outside any operation.
pub const DETACHED_SUGGESTION: &str =
    "You're not on a branch — new commits here are easy to lose. Save your spot: gitrust \"create branch <name>\"";

/// Header shown before the recovery browser lists lost-commit candidates.
pub const FORENSIC_BROWSER_HEADER: &str =
    "Lost & recoverable commits (restoring creates a rescue branch — your current work is never touched):";

/// Empty-result copy (S29 honesty): admit that gc'd / never-committed work may
/// be unrecoverable, rather than implying a clean tree.
pub const FORENSIC_NONE_FOUND: &str =
    "Nothing found. Work that was never committed, or that git has already garbage-collected, may be unrecoverable.";

/// Success line after restoring a lost commit to a rescue branch.
pub fn forensic_restored(branch: &str, short_oid: &str) -> String {
    format!("Recovered {short_oid} onto new branch `{branch}`. Your current branch and files are unchanged.")
}

/// Note appended when a bound trims the lost-commit scan (never silent).
pub fn forensic_truncated(shown: usize, total: usize) -> String {
    format!("Showing {shown} of {total} candidates — older or excess results were hidden.")
}

/// Trash-net success line (S30) — appended after a destructive op whose deleted
/// content was backed up. `days` = retention; `dir` = the trash subdir.
pub fn trash_backup_note(dir: &str, days: u64) -> String {
    format!("A backup of the discarded changes is in {dir} for {days} days.")
}

// -- Destructive-phrase gate (S62). "clean up" / "start fresh" / "throw away my
//    changes" terminate here; the gate spells out what would be deleted BEFORE
//    any Clean/DiscardLocal is constructed. The LLM never picks the semantic.

/// The gate question for a destructive-cleanup phrase.
pub const CLEANUP_GATE_QUESTION: &str = "Clean up — what should be removed?";

/// Clean-up options: the count goes in the label (what you'll lose), the
/// safety net in the consequence.
pub fn cleanup_tracked_label(n: usize) -> String {
    format!(
        "Discard changes to {} tracked file{}",
        n,
        if n == 1 { "" } else { "s" }
    )
}
pub const CLEANUP_TRACKED_CONSEQUENCE: &str = "backed up to ~/.gitrust/trash first";
pub fn cleanup_untracked_label(n: usize) -> String {
    format!(
        "Delete {} untracked file{}",
        n,
        if n == 1 { "" } else { "s" }
    )
}
pub const CLEANUP_UNTRACKED_CONSEQUENCE: &str =
    "never committed, so the trash backup is their only copy";
pub const CLEANUP_BOTH_LABEL: &str = "Both";
pub const CLEANUP_BOTH_CONSEQUENCE: &str = "discard and delete — both backed up first";

/// Degenerate guard: nothing to clean, so no gate is rendered.
pub const CLEANUP_NOTHING: &str =
    "Nothing to clean — the working tree has no changes or untracked files.";

// ---------------------------------------------------------------------------
// Cancel (ux::Canceled) — one calm line for every backed-out run.
// ---------------------------------------------------------------------------

pub const CANCELED: &str = "Canceled — nothing was done.";
pub const CANCELED_GLYPH: &str = "↪";

/// Help line under every pick-one menu (inquire's default omits Esc).
pub const CHOICE_HELP: &str = "↑↓ move · Enter choose · Esc cancel";
/// Help line under the numbered pick-one menu (ux::pick_one).
pub const PICKER_HELP: &str = "1–9 or ↑↓ + Enter to choose · Esc cancel";
/// Help line under the file picker.
pub const FILE_PICKER_HELP: &str =
    "↑↓ move · Space toggle · → all · ← none · Enter stage · Esc cancel";
/// Help line under free-text prompts (branch names, commit messages, URLs…).
pub const TEXT_PROMPT_HELP: &str = "Enter to continue · Esc to cancel";
/// Validator message for an empty free-text answer — generic on purpose: the
/// same prompt asks for branch names, counts, URLs and commit messages.
pub const TEXT_PROMPT_EMPTY: &str = "This can't be empty — type a value, or press Esc to cancel.";

// ---------------------------------------------------------------------------
// Kev (local decision model) — Layer 1 copy
// ---------------------------------------------------------------------------

/// What a menu shows for a Kev option: (title, short hint). User-facing only —
/// the model is asked with kev_ops.json's descriptions, whose wording is
/// load-bearing; these can change freely. `None` = no such option.
pub fn op_label(op: &str) -> Option<(&'static str, &'static str)> {
    Some(match op {
        "status" => ("Show status", "what's changed, staged or untracked"),
        "log" => ("Show history", "recent commits"),
        "diff" => ("Show changes", "line-by-line diff of your edits"),
        "branches" => ("List branches", "and which one you're on"),
        "config" => ("Show Git settings", "your name, email and other config"),
        "show" => ("Show a commit", "author, date, message and files"),
        "fetch" => ("Check the remote", "ahead or behind? changes nothing"),
        "merge_preview" => ("Preview a pull", "would it conflict? changes nothing"),
        "add_commit" => (
            "Commit everything",
            "stage all changes and commit — no push",
        ),
        "commit" => ("Commit staged files", "only what's already staged"),
        "push" => ("Push", "upload all your commits"),
        "scoped_push" => (
            "Push only some changes",
            "pick one topic's files, commit, push",
        ),
        "commit_push" => ("Commit and push everything", "stage all, commit, push"),
        "branch" => ("Create a branch", "a new branch from here"),
        "checkout" => ("Switch branch", "to a branch that exists"),
        "pull" => ("Pull", "download and merge the remote's commits"),
        "pull_rebase" => ("Pull with rebase", "put your commits on top of theirs"),
        "sync" => ("Update from main", "bring main into this branch"),
        "undo" => ("Undo the last commit", "you'll choose how, next"),
        "amend" => ("Amend the last commit", "change its message or add files"),
        "squash" => ("Squash commits", "combine recent commits into one"),
        "revert" => ("Revert a commit", "a new commit that reverses it"),
        "unstage" => ("Unstage files", "your edits stay"),
        "stash" => ("Stash changes", "set them aside for later"),
        "stash_pop" => ("Restore stashed changes", "bring back what you set aside"),
        "cleanup" => (
            "Discard uncommitted changes",
            "backed up first · you'll choose what",
        ),
        "discard_local" => ("Reset to the remote", "drops commits you haven't pushed"),
        "force_push" => ("Force push", "overwrite the remote branch with yours"),
        "init" => ("Start a repository here", "git init"),
        "add_remote" => ("Add a remote", "or change a remote's URL"),
        "remotes" => ("List remotes", "and their URLs"),
        "remove_remote" => ("Remove a remote", "the remote's URL is forgotten"),
        "set_upstream" => ("Track a remote branch", "so push and pull just work"),
        "find_lost_work" => ("Find lost work", "search for deleted commits"),
        "delete_repo" => (
            "Remove Git from this folder",
            "deletes .git, keeps your files",
        ),
        "__reject__" => ("Not a Git request", ""),
        // In-progress control (asked only mid-operation).
        "rebase_continue" => ("Continue the rebase", "conflicts are fixed"),
        "rebase_skip" => ("Skip this commit", "drop it and carry on"),
        "rebase_abort" => ("Abort the rebase", "back to before it started"),
        "merge_abort" => ("Abort the merge", "back to before it started"),
        "cherry_pick_continue" => ("Continue the cherry-pick", "conflicts are fixed"),
        "cherry_pick_abort" => ("Abort the cherry-pick", "back to before it started"),
        "revert_continue" => ("Continue the revert", "conflicts are fixed"),
        "revert_abort" => ("Abort the revert", "back to before it started"),
        "something_else" => ("Something else", "not about the operation in progress"),
        _ => return None,
    })
}

// Spinner labels (stderr, terminal only — ux::Spinner).
pub const SPINNER_UNDERSTANDING: &str = "Understanding your request…";
pub const SPINNER_MATCHING_FILES: &str = "Matching files to your request…";
pub const SPINNER_COLD_START: &str =
    "Starting the local model (about 25 s after it has been idle)…";

/// The manual menu when the local model is unreachable.
pub const MANUAL_MENU_QUESTION: &str =
    "The local model isn't available — pick what you want to do (type to filter):";

/// `gitrust help`: phrases that always work instantly (no model), by task.
/// `<name>` / `<url>` / `<file>` are placeholders. A test checks every one
/// really is a fast-path phrase, so help never advertises something slow.
pub const HELP_SECTIONS: &[(&str, &[&str])] = &[
    (
        "Look",
        &[
            "status", "log", "diff", "show", "branches", "remotes", "config",
        ],
    ),
    (
        "Save",
        &[
            "commit all my files",
            "commit and push",
            "commit everything and push",
        ],
    ),
    (
        "Share",
        &["push", "publish", "pull", "fetch", "safe to pull"],
    ),
    (
        "Branches",
        &["create branch <name>", "switch to <name>", "current branch"],
    ),
    (
        "Undo & tidy",
        &[
            "undo",
            "stash",
            "stash pop",
            "clean up my repo",
            "find my lost work",
        ],
    ),
    (
        "Mid-rebase",
        &[
            "continue rebase",
            "skip rebase",
            "abort rebase",
            "abort merge",
        ],
    ),
    (
        "Setup",
        &["init", "add remote origin <url>", "add remote <url>"],
    ),
];

/// The trust line (design handoff M6/M7), kept true: the model only picks
/// from a closed list; the code decides whether to act, ask or refuse.
pub const HELP_TRUST: &str = "A small local model picks from a fixed list of Git operations — it never\ninvents a command — and GiTrust asks you whenever it isn't sure.";

/// Fill a help placeholder with a sample value (for the help self-test).
#[cfg(test)]
pub fn help_sample(phrase: &str) -> String {
    phrase
        .replace("<name>", "feature-x")
        .replace("<url>", "https://example.com/repo.git")
        .replace("<file>", "src/main.rs")
}

/// Kev was confident the request isn't about Git.
pub const NOT_A_GIT_REQUEST: &str =
    "That doesn't look like a Git request. Try e.g. \"show status\" or \"push my commits\" — or `gitrust help` for phrases that always work.";

/// Header of the pick-from-candidates ChoiceGate when Kev isn't confident.
pub const KEV_UNSURE_QUESTION: &str = "I'm not sure which operation you meant. Which one?";

/// Same gate when Kev's best guess was "not a Git request" (but unconfident).
pub const KEV_MAYBE_NOT_GIT_QUESTION: &str =
    "That may not be a Git request. If it is, which operation did you mean?";

/// Same gate when Kev's choice ignored something the request names (e.g. a
/// plain push for "push the api changes").
pub const KEV_CONFLICT_QUESTION: &str =
    "Your request names specific changes, but that operation would ignore them. Which did you mean?";

/// Same gate when the request asks to commit AND push but Kev picked only one.
pub const KEV_COMMIT_PUSH_CONFLICT_QUESTION: &str =
    "Your request says both commit and push, but that operation does only one. Which did you mean?";

/// Same gate when Kev picked commit-and-push but the request never says push.
pub const KEV_NO_PUSH_CONFLICT_QUESTION: &str =
    "Your request doesn't mention pushing, but that operation would also push. Which did you mean?";

/// Same gate when a destructive pick changed once the options were reordered.
pub const KEV_UNSTABLE_QUESTION: &str =
    "That could rewrite or discard work, and I'm not certain which operation you meant. Which one?";

/// Header of the in-progress menu (continue / skip / abort / something else).
pub const KEV_IN_PROGRESS_QUESTION: &str =
    "An operation is in progress. Did you mean to control it?";

/// Said before any message prompt, so nobody types a message for nothing.
pub const NOTHING_TO_COMMIT: &str = "Nothing to commit — the working tree is clean.";
pub const NOTHING_STAGED: &str =
    "Nothing is staged, so there's nothing to commit. Commit everything with: gitrust \"commit all my files\"";

/// Label of the commit-message prompt when GiTrust has nothing to offer.
pub const COMMIT_MESSAGE_ASK: &str = "Commit message:";

/// An empty commit message was submitted (or --yes had none to submit).
pub const COMMIT_MESSAGE_REQUIRED: &str = "A commit message is required — nothing was committed. \
Put one in your request, e.g. commit everything with message 'fix login'.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_age_note_says_how_old_the_remote_view_is() {
        assert_eq!(
            fetch_age_note(None),
            "Remote state: never fetched — gitrust \"fetch\" checks the remote."
        );
        assert_eq!(
            fetch_age_note(Some(30)),
            "Remote state as of the last fetch, just now — gitrust \"fetch\" refreshes it."
        );
        assert_eq!(
            fetch_age_note(Some(8 * 60)),
            "Remote state as of the last fetch, 8 min ago — gitrust \"fetch\" refreshes it."
        );
        assert!(fetch_age_note(Some(3 * 3600)).contains("3 hours ago"));
        assert!(fetch_age_note(Some(2 * 86400)).contains("2 days ago"));
        // Older than 10 minutes → worth flagging (status colors it).
        assert!(
            !fetch_is_stale(Some(9 * 60)) && fetch_is_stale(Some(11 * 60)) && fetch_is_stale(None)
        );
    }

    #[test]
    fn every_help_phrase_is_instant() {
        let slow: Vec<&str> = HELP_SECTIONS
            .iter()
            .flat_map(|(_, phrases)| phrases.iter().copied())
            .filter(|p| crate::intent::try_canonical(&help_sample(p)).is_none())
            .collect();
        assert!(
            slow.is_empty(),
            "`gitrust help` lists phrases that aren't instant: {slow:?}"
        );
    }

    #[test]
    fn every_kev_option_has_a_menu_label() {
        for (op, _) in crate::kev::ops() {
            let (title, _hint) = op_label(op).unwrap_or_else(|| panic!("no menu label for {op}"));
            assert!(!title.is_empty() && !title.contains('_'), "{op}: {title}");
        }
        for fam in ["rebase", "merge", "cherry_pick", "revert"] {
            let (_, opts) = crate::kev::in_progress_question(fam).unwrap();
            for (op, _) in opts {
                assert!(op_label(&op).is_some(), "no menu label for {op}");
            }
        }
    }
}
