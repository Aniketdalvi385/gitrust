use crate::commands::{PipelineStep, ResetMode, ScopedWriteOp};
use crate::context::RepoContext;

/// Severity of a git operation — determined by **blast radius** (who/what can be
/// damaged), not just by reversibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerLevel {
    /// Read-only or trivially undo-able. No warning needed.
    Safe,
    /// Easily reversible: commit, push to own branch, stash, branch creation.
    Caution,
    /// Local-only damage, recoverable via `git reflog` (~90 days).
    Destructive,
    /// Affects shared / remote state others depend on. Cannot be cleanly undone.
    Irreversible,
}

/// Returns a short text marker for the level. Empty string for `Safe` so
/// formatters can append unconditionally without adding stray whitespace.
pub fn marker(level: DangerLevel) -> &'static str {
    match level {
        DangerLevel::Safe => "",
        DangerLevel::Caution => "⚠",
        DangerLevel::Destructive => "⚠⚠",
        DangerLevel::Irreversible => "⛔",
    }
}

/// Whether to emit ANSI color codes. Honors `NO_COLOR` and requires stdout to
/// be a TTY (so piped output stays clean).
pub fn use_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn ansi_for(level: DangerLevel) -> Option<&'static str> {
    match level {
        DangerLevel::Safe => None,
        DangerLevel::Caution => Some("\x1b[33m"), // yellow
        DangerLevel::Destructive => Some("\x1b[31m"), // red
        DangerLevel::Irreversible => Some("\x1b[1;31m"), // bold red
    }
}

/// Marker wrapped in ANSI color codes appropriate for the danger level.
/// Falls back to the plain marker when stdout is not a TTY or NO_COLOR is set.
pub fn colored_marker(level: DangerLevel) -> String {
    let m = marker(level);
    if m.is_empty() || !use_color() {
        return m.to_string();
    }
    match ansi_for(level) {
        Some(code) => format!("{}{}\x1b[0m", code, m),
        None => m.to_string(),
    }
}

/// Wrap arbitrary text in the color for `level`. No-op if color is disabled.
pub fn colorize(level: DangerLevel, text: &str) -> String {
    if text.is_empty() || !use_color() {
        return text.to_string();
    }
    match ansi_for(level) {
        Some(code) => format!("{}{}\x1b[0m", code, text),
        None => text.to_string(),
    }
}

/// Short label used in the explanation header.
pub fn label(level: DangerLevel) -> &'static str {
    match level {
        DangerLevel::Safe => "Safe",
        DangerLevel::Caution => "Caution",
        DangerLevel::Destructive => "Destructive",
        DangerLevel::Irreversible => "Irreversible",
    }
}

/// Danger level for an intent (`ScopedWriteOp`). Context is consulted for
/// situational bumps so the marker reflects reality, not worst-case-always.
pub fn intent_danger(op: &ScopedWriteOp, ctx: &RepoContext) -> DangerLevel {
    use ScopedWriteOp as W;
    match op {
        W::Add { .. }
        | W::Init
        | W::Stash
        | W::StashPop
        | W::Undo
        | W::AddRemote { .. }
        | W::AddAndCommit { .. }
        | W::Commit { .. }
        | W::Branch { .. }
        | W::Checkout { .. }
        | W::Push { .. } => DangerLevel::Caution,

        W::Pull { .. } => {
            if ctx.has_uncommitted() {
                DangerLevel::Caution
            } else {
                DangerLevel::Safe
            }
        }

        W::Reset {
            mode: ResetMode::Soft,
        } => DangerLevel::Caution,
        W::Reset {
            mode: ResetMode::Mixed,
        } => DangerLevel::Caution,
        W::Reset {
            mode: ResetMode::Hard,
        } => DangerLevel::Destructive,

        W::PullRebase { .. } => {
            if ctx.has_uncommitted() {
                DangerLevel::Destructive
            } else {
                DangerLevel::Caution
            }
        }

        W::DiscardLocal { .. } => DangerLevel::Destructive,
        W::RemoveRemote { .. } => DangerLevel::Destructive,

        W::ForcePush { .. } => {
            if shared_branch(ctx) {
                DangerLevel::Irreversible
            } else {
                DangerLevel::Destructive
            }
        }

        W::DeleteRepo => {
            if ctx.is_empty_repo {
                DangerLevel::Destructive
            } else {
                DangerLevel::Irreversible
            }
        }

        // In-progress backout/continue ops (Phase 11). Aborts/skips are the
        // low-danger exit — they restore or drop pre-op state and git's
        // ORIG_HEAD is the safety net. Continues advance an op the user already
        // started; still reflog-recoverable → Caution.
        W::MergeAbort
        | W::RebaseAbort
        | W::RebaseSkip
        | W::CherryPickAbort
        | W::RevertAbort
        | W::RebaseContinue
        | W::CherryPickContinue
        | W::RevertContinue => DangerLevel::Caution,

        // Repair vocabulary (Phase 13). Amend/Squash rewrite HEAD — reflog-
        // recoverable locally → Caution. The Hard-adjacent escalation for a
        // PUSHED commit (force-push required) is enforced by the build-path
        // `PushedCommitRewrite` TypeToConfirm gate, not by this marker (pushed-
        // ness isn't in `RepoContext`). Revert is history-preserving and team-
        // safe — the steered-toward default. Unstage is index-only (contents
        // safe). SetUpstream is config-only.
        W::UndoIntent
        | W::Amend { .. }
        | W::SquashLastN { .. }
        | W::Revert { .. }
        | W::Unstage { .. }
        | W::SetUpstream { .. } => DangerLevel::Caution,

        // Forensic recovery only creates a rescue ref → Caution. The cleanup
        // intent is ambiguous until the gate resolves it; the resolved step
        // (Clean/Reset::Hard) carries the real danger, so the intent itself is
        // graded conservatively as Destructive to colour its preview honestly.
        W::ForensicRecover => DangerLevel::Caution,
        W::CleanupIntent => DangerLevel::Destructive,
    }
}

/// Danger level for an individual pipeline step. Used for the inline marker
/// in the plan-preview box. Coarser than intent danger.
pub fn step_danger(step: &PipelineStep, ctx: &RepoContext) -> DangerLevel {
    use PipelineStep as P;
    match step {
        // Read-only steps
        P::Status | P::Log { .. } | P::Diff { .. } => DangerLevel::Safe,

        // Staging is fully reversible — no marker (keeps ⚠ meaningful).
        P::Add { .. } => DangerLevel::Safe,

        // Local additions / setup
        P::Branch { .. }
        | P::Checkout { .. }
        | P::Stash
        | P::StashPop
        | P::AddRemote { .. }
        | P::Commit { .. }
        | P::Undo { .. } => DangerLevel::Caution,

        // Pull is Caution when there are local changes, Safe otherwise
        P::Pull { .. } => {
            if ctx.has_uncommitted() {
                DangerLevel::Caution
            } else {
                DangerLevel::Safe
            }
        }

        // Severity reflects the branch (design principle 4): pushing your own
        // feature branch is routine; pushing a shared branch is Caution.
        P::Push { .. } => {
            if shared_branch(ctx) {
                DangerLevel::Caution
            } else {
                DangerLevel::Safe
            }
        }

        // Reset depends on mode.
        P::Reset {
            mode: ResetMode::Soft,
            ..
        } => DangerLevel::Caution,
        P::Reset {
            mode: ResetMode::Mixed,
            ..
        } => DangerLevel::Caution,
        P::Reset {
            mode: ResetMode::Hard,
            ..
        } => DangerLevel::Destructive,

        // Pull-rebase potentially rewrites work in progress.
        P::PullRebase { .. } => {
            if ctx.has_uncommitted() {
                DangerLevel::Destructive
            } else {
                DangerLevel::Caution
            }
        }

        // Divergence reconciliation (Phase 12). Both integrate remote history;
        // conflict-capable but reflog/ORIG_HEAD-recoverable → Caution.
        P::Merge { .. } | P::Rebase { .. } => DangerLevel::Caution,

        // Local damage, reflog-recoverable
        P::DiscardLocal { .. } | P::RemoveRemote { .. } => DangerLevel::Destructive,

        // Shared-state damage
        P::ForcePush { .. } => {
            if shared_branch(ctx) {
                DangerLevel::Irreversible
            } else {
                DangerLevel::Destructive
            }
        }

        // Clean deletes UNTRACKED files (Phase 14 / S62). Graded ABOVE hard reset:
        // a hard reset's discarded commits are reflog-recoverable (~90 days), but
        // untracked files have NO reflog and NO object-db copy — only GiTrust's
        // trash-net stands between them and permanent loss. Irreversible from
        // git's point of view.
        P::Clean { .. } => DangerLevel::Irreversible,

        // In-progress backout/continue ops (Phase 11) — all Caution; aborts are
        // the low-danger exit and (via format_step_label ordering + copy) are
        // named first as the anxious user's way out.
        P::MergeAbort
        | P::RebaseAbort
        | P::RebaseSkip
        | P::CherryPickAbort
        | P::RevertAbort
        | P::RebaseContinue
        | P::CherryPickContinue
        | P::RevertContinue => DangerLevel::Caution,

        // Repair vocabulary (Phase 13) — reflog-recoverable / index-only /
        // config-only, all Caution. The pushed-commit-rewrite escalation is the
        // build-path TypeToConfirm gate, not this marker.
        P::Amend { .. }
        | P::Revert { .. }
        | P::Unstage { .. }
        | P::SetUpstream { .. }
        | P::SquashLastN { .. } => DangerLevel::Caution,
    }
}

/// Heuristic: is this branch likely shared with teammates?
///
/// True if the branch name is a known shared-default (main/master/develop),
/// OR if recent commits have authors other than the current user.
fn shared_branch(ctx: &RepoContext) -> bool {
    matches!(ctx.branch.as_str(), "main" | "master" | "develop" | "trunk")
        || ctx.has_foreign_commits()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CommitMessage, FileSelection, ResetMode, ScopedWriteOp};
    use crate::context::{RepoContext, StatusSummary};

    fn ctx_clean(branch: &str) -> RepoContext {
        RepoContext {
            branch: branch.to_string(),
            upstream: Some(format!("refs/remotes/origin/{}", branch)),
            ahead: 0,
            behind: 0,
            status: StatusSummary {
                conflicted: vec![],
                staged: vec![],
                staged_overflow: 0,
                modified: vec![],
                modified_overflow: 0,
                untracked: vec![],
                untracked_overflow: 0,
            },
            origin_url: None,
            last_commit: "abc1234 \"x\" (1m ago)".to_string(),
            is_empty_repo: false,
            is_detached: false,
            has_stash: false,
            situation: crate::commands::RepoSituation::Normal,
        }
    }

    fn ctx_dirty(branch: &str) -> RepoContext {
        let mut c = ctx_clean(branch);
        c.status.modified.push("README.md".to_string());
        c
    }

    #[test]
    fn safe_pull_when_clean() {
        let op = ScopedWriteOp::Pull {
            remote: "origin".to_string(),
        };
        assert_eq!(intent_danger(&op, &ctx_clean("feat/x")), DangerLevel::Safe);
    }

    #[test]
    fn caution_pull_when_dirty() {
        let op = ScopedWriteOp::Pull {
            remote: "origin".to_string(),
        };
        assert_eq!(
            intent_danger(&op, &ctx_dirty("feat/x")),
            DangerLevel::Caution
        );
    }

    #[test]
    fn commit_is_caution() {
        let op = ScopedWriteOp::Commit {
            message: CommitMessage::Provided("x".to_string()),
        };
        assert_eq!(
            intent_danger(&op, &ctx_clean("feat/x")),
            DangerLevel::Caution
        );
    }

    #[test]
    fn hard_reset_destructive() {
        let op = ScopedWriteOp::Reset {
            mode: ResetMode::Hard,
        };
        assert_eq!(
            intent_danger(&op, &ctx_clean("feat/x")),
            DangerLevel::Destructive
        );
    }

    #[test]
    fn force_push_irreversible_on_main() {
        let op = ScopedWriteOp::ForcePush {
            remote: "origin".to_string(),
        };
        assert_eq!(
            intent_danger(&op, &ctx_clean("main")),
            DangerLevel::Irreversible
        );
    }

    #[test]
    fn force_push_destructive_on_personal_branch() {
        let op = ScopedWriteOp::ForcePush {
            remote: "origin".to_string(),
        };
        assert_eq!(
            intent_danger(&op, &ctx_clean("feat/personal-thing")),
            DangerLevel::Destructive
        );
    }

    #[test]
    fn delete_repo_destructive_when_empty() {
        let op = ScopedWriteOp::DeleteRepo;
        let mut c = ctx_clean("master");
        c.is_empty_repo = true;
        assert_eq!(intent_danger(&op, &c), DangerLevel::Destructive);
    }

    #[test]
    fn delete_repo_irreversible_when_has_commits() {
        let op = ScopedWriteOp::DeleteRepo;
        assert_eq!(
            intent_danger(&op, &ctx_clean("master")),
            DangerLevel::Irreversible
        );
    }

    #[test]
    fn add_is_safe_in_pipeline() {
        // Staging is fully reversible: no marker, so ⚠ keeps its meaning
        // (design handoff Flow 06½: "drop ⚠ on git add").
        let step = PipelineStep::Add { files: vec![] };
        assert_eq!(step_danger(&step, &ctx_clean("x")), DangerLevel::Safe);
    }

    #[test]
    fn push_severity_reflects_the_branch() {
        // Principle 4: a push to your own feature branch carries no marker; a
        // push to a shared branch (main/master/develop/trunk) is Caution.
        let push = PipelineStep::Push {
            remote: "origin".into(),
            branch: "x".into(),
            set_upstream: false,
        };
        assert_eq!(
            step_danger(&push, &ctx_clean("feat/login")),
            DangerLevel::Safe
        );
        assert_eq!(step_danger(&push, &ctx_clean("main")), DangerLevel::Caution);
    }

    #[test]
    fn aborts_are_low_danger() {
        // Phase 11: aborts/skips restore or drop pre-op state — the anxious
        // user's exit. They must never grade above Caution.
        for step in [
            PipelineStep::MergeAbort,
            PipelineStep::RebaseAbort,
            PipelineStep::RebaseSkip,
            PipelineStep::CherryPickAbort,
            PipelineStep::RevertAbort,
        ] {
            let level = step_danger(&step, &ctx_clean("main"));
            assert_eq!(
                level,
                DangerLevel::Caution,
                "{:?} must be low danger (Caution), got {:?}",
                step,
                level
            );
        }
    }

    #[test]
    fn clean_grades_above_hard_reset() {
        // S62: untracked deletion (Clean) outranks hard reset — hard reset has a
        // reflog, untracked files have only the trash-net.
        let clean = step_danger(&PipelineStep::Clean { trash_net: true }, &ctx_clean("main"));
        let hard = step_danger(
            &PipelineStep::Reset {
                mode: ResetMode::Hard,
                to_oid: None,
            },
            &ctx_clean("main"),
        );
        assert_eq!(clean, DangerLevel::Irreversible);
        assert_eq!(hard, DangerLevel::Destructive);
        // Ordering is by enum discriminant (Safe < Caution < Destructive < Irreversible).
        assert!(
            clean as u8 > hard as u8,
            "Clean must grade strictly above Reset::Hard"
        );
    }

    #[test]
    fn marker_strings() {
        assert_eq!(marker(DangerLevel::Safe), "");
        assert_eq!(marker(DangerLevel::Caution), "⚠");
        assert_eq!(marker(DangerLevel::Destructive), "⚠⚠");
        assert_eq!(marker(DangerLevel::Irreversible), "⛔");
    }
}
