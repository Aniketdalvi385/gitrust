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
    }
}

/// Danger level for an individual pipeline step. Used for the inline marker
/// in the plan-preview box. Coarser than intent danger.
pub fn step_danger(step: &PipelineStep, ctx: &RepoContext) -> DangerLevel {
    use PipelineStep as P;
    match step {
        // Read-only steps
        P::Status | P::Log { .. } | P::Diff { .. } => DangerLevel::Safe,

        // Local additions / setup
        P::Add { .. }
        | P::Branch { .. }
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

        // Pushes are Caution (own branch) — escalated only for ForcePush below.
        P::Push { .. } => DangerLevel::Caution,

        // Reset depends on mode.
        P::Reset {
            mode: ResetMode::Soft,
        } => DangerLevel::Caution,
        P::Reset {
            mode: ResetMode::Mixed,
        } => DangerLevel::Caution,
        P::Reset {
            mode: ResetMode::Hard,
        } => DangerLevel::Destructive,

        // Pull-rebase potentially rewrites work in progress.
        P::PullRebase { .. } => {
            if ctx.has_uncommitted() {
                DangerLevel::Destructive
            } else {
                DangerLevel::Caution
            }
        }

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
                staged: vec![],
                staged_overflow: 0,
                modified: vec![],
                modified_overflow: 0,
                untracked: vec![],
                untracked_overflow: 0,
            },
            remotes: vec!["origin".to_string()],
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
        // Sanity: add staging step is Caution, not Safe
        let step = PipelineStep::Add { files: vec![] };
        assert_eq!(step_danger(&step, &ctx_clean("x")), DangerLevel::Caution);
    }

    #[test]
    fn marker_strings() {
        assert_eq!(marker(DangerLevel::Safe), "");
        assert_eq!(marker(DangerLevel::Caution), "⚠");
        assert_eq!(marker(DangerLevel::Destructive), "⚠⚠");
        assert_eq!(marker(DangerLevel::Irreversible), "⛔");
    }
}
