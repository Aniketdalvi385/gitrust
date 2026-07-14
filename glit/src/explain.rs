use crate::commands::{ResetMode, ScopedWriteOp};

/// Newtype for translatable strings. Keeps i18n future open without
/// invasive churn: today `Msg(&'static str)`, tomorrow could be a key-lookup.
#[derive(Debug, Clone, Copy)]
pub struct Msg(pub &'static str);

impl std::fmt::Display for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Plain-English explanation for a single operation. Shown before destructive
/// gates and (in short form) in disambiguation menus.
#[derive(Debug, Clone, Copy)]
pub struct Explanation {
    /// One sentence: what this operation actually does.
    pub what: Msg,
    /// One sentence: what can go wrong / what the user should think twice about.
    pub risks: Msg,
    /// How to undo or rescue if the user later regrets it.
    pub recovery: Option<Msg>,
    /// Any built-in safety nets the user should know about.
    pub safety: Option<Msg>,
}

/// Return the explanation for a `ScopedWriteOp`. **Exhaustive match — no `_ =>`**
/// so adding a new variant becomes a compile error until its explanation is written.
/// That is the drift defense.
pub fn explain(op: &ScopedWriteOp) -> &'static Explanation {
    use ScopedWriteOp as W;
    match op {
        W::Add { .. } => &Explanation {
            what: Msg("Marks file changes as ready to be included in your next commit."),
            risks: Msg("Staged files are not yet committed — they can still be edited or unstaged."),
            recovery: Some(Msg("Run `glit \"reset\"` (mixed) to unstage; the file contents are not changed.")),
            safety: None,
        },
        W::Commit { .. } => &Explanation {
            what: Msg("Records your staged changes as a permanent point in the project's history."),
            risks: Msg("Commits with the wrong files or message are easy to make; rewriting after pushing affects others."),
            recovery: Some(Msg("Run `glit \"undo\"` immediately after to bring the files back to staged.")),
            safety: None,
        },
        W::AddAndCommit { .. } => &Explanation {
            what: Msg("Stages all your local changes and records them as a single commit in one step."),
            risks: Msg("Pulls in EVERY modified and untracked file. Easy to commit something you didn't mean to."),
            recovery: Some(Msg("Run `glit \"undo\"` to unwind the commit (files return to staged).")),
            safety: Some(Msg("The plan preview shows the message and file count before execution.")),
        },
        W::Branch { .. } => &Explanation {
            what: Msg("Creates a new branch pointing at the current commit. You stay on your current branch unless you also check it out."),
            risks: Msg("If the name conflicts with an existing branch, creation fails. Branch names with spaces or invalid characters are rejected by git."),
            recovery: Some(Msg("Delete the unwanted branch with `git branch -d <name>`.")),
            safety: None,
        },
        W::Checkout { .. } => &Explanation {
            what: Msg("Switches your working tree to a different branch or commit."),
            risks: Msg("If you have uncommitted changes that would be overwritten, git will refuse and ask you to commit or stash first."),
            recovery: Some(Msg("Run `glit \"checkout <previous-branch>\"` to switch back; your stash / commits are preserved.")),
            safety: None,
        },
        W::Stash => &Explanation {
            what: Msg("Captures your uncommitted changes into a temporary save and reverts the working tree to clean."),
            risks: Msg("If you forget about the stash, it can hide work you wanted. Stashes are local — they aren't pushed anywhere."),
            recovery: Some(Msg("Run `glit \"stash pop\"` to restore the captured changes. List stashes with `git stash list`.")),
            safety: None,
        },
        W::StashPop => &Explanation {
            what: Msg("Re-applies the most recent stash to your working tree and removes that stash entry."),
            risks: Msg("If the files have changed since the stash was made, applying it can produce merge conflicts."),
            recovery: Some(Msg("If a conflict prevents the pop, the stash entry is kept — re-try after committing or discarding the conflicting changes.")),
            safety: None,
        },
        W::Reset { mode: ResetMode::Soft } => &Explanation {
            what: Msg("Moves HEAD to the parent commit but keeps every change staged in the index."),
            risks: Msg("You lose the commit metadata (message, SHA, timestamp). Files themselves are untouched."),
            recovery: Some(Msg("Re-commit immediately to restore, or use `git reflog` to recover the original commit SHA.")),
            safety: None,
        },
        W::Reset { mode: ResetMode::Mixed } => &Explanation {
            what: Msg("Moves HEAD to the parent commit and unstages everything. Files in the working tree are untouched."),
            risks: Msg("You lose the commit metadata AND the staging arrangement. File contents are still safe."),
            recovery: Some(Msg("Re-stage with `glit \"add\"` and re-commit, or use `git reflog` to find the original commit SHA.")),
            safety: None,
        },
        W::Reset { mode: ResetMode::Hard } => &Explanation {
            what: Msg("Moves HEAD to the parent commit AND throws away every change in the working tree and index."),
            risks: Msg("Any uncommitted work — staged or unstaged — is gone. The commit itself is also dropped from the branch."),
            recovery: Some(Msg("`git reflog` keeps a record of the old HEAD for ~90 days; you can recover the commit if you remember the SHA.")),
            safety: Some(Msg("This op requires you to type the branch name to confirm.")),
        },
        W::Pull { .. } => &Explanation {
            what: Msg("Fetches changes from the remote and merges them into your current branch."),
            risks: Msg("If you and the remote both changed the same lines, you'll get conflict markers in your files to resolve manually."),
            recovery: Some(Msg("To undo a merge in progress: `git merge --abort`. To undo a completed merge: `glit \"undo\"`.")),
            safety: Some(Msg("Run `glit \"safe to pull\"` first to preview conflicts without modifying anything.")),
        },
        W::Push { .. } => &Explanation {
            what: Msg("Sends your local commits to the remote so others can see them."),
            risks: Msg("If the remote has commits you don't have, push will be rejected (non-fast-forward)."),
            recovery: Some(Msg("If rejected, run `glit \"safe to pull\"` to see what's new, then `glit \"pull\"` or rebase, then push again.")),
            safety: Some(Msg("Sets upstream tracking automatically on first push so future pushes are simpler.")),
        },
        W::Init => &Explanation {
            what: Msg("Creates a new empty git repository in the current directory."),
            risks: Msg("If you're already inside a git repo, this re-initializes it (mostly harmless but unusual)."),
            recovery: Some(Msg("Run `glit \"delete repository\"` to remove the .git folder if you didn't mean to init.")),
            safety: None,
        },
        W::DeleteRepo => &Explanation {
            what: Msg("Removes the .git folder, effectively un-tracking the directory."),
            risks: Msg("All commit history and branches in this clone are gone. If commits exist that aren't pushed, they cannot be recovered."),
            recovery: Some(Msg("If your commits were pushed to a remote, you can re-clone. Otherwise the history is lost.")),
            safety: Some(Msg("This op refuses when the repo has commits, unless you explicitly opt in.")),
        },
        W::AddRemote { .. } => &Explanation {
            what: Msg("Registers a remote URL under a name (default `origin`) so you can push/pull to/from it."),
            risks: Msg("Adding the wrong URL means push/pull will go to the wrong place. Easy to fix — just remove and re-add."),
            recovery: Some(Msg("Run `glit \"remove remote <name>\"` and add the correct one.")),
            safety: None,
        },
        W::RemoveRemote { .. } => &Explanation {
            what: Msg("Removes a remote definition from your local repository configuration."),
            risks: Msg("Future push/pull to that remote will fail with 'remote not found'. Doesn't touch the remote server itself."),
            recovery: Some(Msg("Re-add with `glit \"add remote <name> <url>\"` — the remote server is unaffected.")),
            safety: None,
        },
        W::Undo => &Explanation {
            what: Msg("Soft-resets HEAD to the parent commit. Files stay staged, the commit is removed from the branch."),
            risks: Msg("If the commit was already pushed, your local branch will be behind the remote. Re-push only with force."),
            recovery: Some(Msg("Re-commit the same files to recreate it. The original commit SHA lives in `git reflog` for ~90 days.")),
            safety: Some(Msg("Refuses on the very first commit (nothing to soft-reset to) and in detached HEAD state.")),
        },
        W::PullRebase { .. } => &Explanation {
            what: Msg("Fetches changes from the remote and replays your local commits on top — produces a linear history."),
            risks: Msg("Each of your commits is replayed individually; conflicts may appear at any step and pause the rebase."),
            recovery: Some(Msg("If a rebase goes wrong: `git rebase --abort` returns to your pre-rebase state.")),
            safety: Some(Msg("Run `glit \"safe to pull\"` first to predict conflicts.")),
        },
        W::ForcePush { .. } => &Explanation {
            what: Msg("Replaces the remote branch with your local version, overwriting whatever is there."),
            risks: Msg("Anyone who fetched the old version will need to reset. Their commits made on top are abandoned."),
            recovery: Some(Msg("Recovery exists only if someone has the old SHA: `git reflog` finds it locally; otherwise it's gone.")),
            safety: Some(Msg("Uses --force-with-lease — refuses if the remote moved since your last fetch (catches accidental teammate overwrites).")),
        },
        W::DiscardLocal { .. } => &Explanation {
            what: Msg("Throws away your local commits and resets your branch to exactly match the remote."),
            risks: Msg("Any commit you made that hasn't been pushed will be gone from the branch."),
            recovery: Some(Msg("`git reflog` lists abandoned commits for ~90 days; you can still check them out by SHA.")),
            safety: None,
        },
        // In-progress backout/continue ops (Phase 11).
        W::MergeAbort => &Explanation {
            what: Msg("Backs out of the in-progress merge and restores your branch to its pre-merge state."),
            risks: Msg("Any conflict resolutions you'd staged for this merge are discarded."),
            recovery: Some(Msg("Nothing to recover — this IS the recovery. Start the merge again if you still want it.")),
            safety: Some(Msg("Restores via git's own machinery; your committed history is untouched.")),
        },
        W::RebaseContinue => &Explanation {
            what: Msg("Resumes the paused rebase, applying the current commit now that conflicts are resolved."),
            risks: Msg("If more commits still conflict, the rebase pauses again on the next one."),
            recovery: Some(Msg("`git rebase --abort` (or `glit \"abort rebase\"`) returns to your pre-rebase state.")),
            safety: Some(Msg("Only runs once the index is conflict-free — the gate checks first.")),
        },
        W::RebaseSkip => &Explanation {
            what: Msg("Drops the commit that's currently causing conflicts and continues the rebase."),
            risks: Msg("The skipped commit's changes are left out of the rebased branch entirely."),
            recovery: Some(Msg("`git rebase --abort` returns to your pre-rebase state, skipped commit included.")),
            safety: None,
        },
        W::RebaseAbort => &Explanation {
            what: Msg("Backs out of the in-progress rebase and restores the original HEAD."),
            risks: Msg("Any conflict resolutions staged during this rebase are discarded."),
            recovery: Some(Msg("Nothing to recover — this IS the recovery. Start the rebase again if you still want it.")),
            safety: Some(Msg("Restores the original branch tip via git's own machinery.")),
        },
        W::CherryPickContinue => &Explanation {
            what: Msg("Resumes the paused cherry-pick now that conflicts are resolved and staged."),
            risks: Msg("If further picks conflict, the operation pauses again."),
            recovery: Some(Msg("`git cherry-pick --abort` (or `glit \"abort cherry-pick\"`) returns to your pre-pick state.")),
            safety: Some(Msg("Only runs once the index is conflict-free — the gate checks first.")),
        },
        W::CherryPickAbort => &Explanation {
            what: Msg("Backs out of the in-progress cherry-pick and restores your pre-pick state."),
            risks: Msg("Any conflict resolutions staged for this cherry-pick are discarded."),
            recovery: Some(Msg("Nothing to recover — this IS the recovery. Re-run the cherry-pick if you still want it.")),
            safety: Some(Msg("Restores via git's own machinery; committed history is untouched.")),
        },
        W::RevertContinue => &Explanation {
            what: Msg("Resumes the paused revert now that conflicts are resolved and staged."),
            risks: Msg("If further reverts conflict, the operation pauses again."),
            recovery: Some(Msg("`git revert --abort` (or `glit \"abort revert\"`) returns to your pre-revert state.")),
            safety: Some(Msg("Only runs once the index is conflict-free — the gate checks first.")),
        },
        W::RevertAbort => &Explanation {
            what: Msg("Backs out of the in-progress revert and restores your pre-revert state."),
            risks: Msg("Any conflict resolutions staged for this revert are discarded."),
            recovery: Some(Msg("Nothing to recover — this IS the recovery. Re-run the revert if you still want it.")),
            safety: Some(Msg("Restores via git's own machinery; committed history is untouched.")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CommitMessage, FileSelection, ResetMode, ScopedWriteOp};

    #[test]
    fn every_variant_has_explanation() {
        // The match in `explain()` is exhaustive — this test just exercises
        // each variant once so we'd notice runtime panics or empty strings.
        let ops = vec![
            ScopedWriteOp::Add {
                files: FileSelection::All,
            },
            ScopedWriteOp::Commit {
                message: CommitMessage::Provided("x".into()),
            },
            ScopedWriteOp::AddAndCommit {
                message: CommitMessage::Provided("x".into()),
                files: FileSelection::All,
            },
            ScopedWriteOp::Branch { name: "x".into() },
            ScopedWriteOp::Checkout { target: "x".into() },
            ScopedWriteOp::Stash,
            ScopedWriteOp::StashPop,
            ScopedWriteOp::Reset {
                mode: ResetMode::Soft,
            },
            ScopedWriteOp::Reset {
                mode: ResetMode::Mixed,
            },
            ScopedWriteOp::Reset {
                mode: ResetMode::Hard,
            },
            ScopedWriteOp::Pull {
                remote: "origin".into(),
            },
            ScopedWriteOp::Push {
                remote: "origin".into(),
            },
            ScopedWriteOp::Init,
            ScopedWriteOp::DeleteRepo,
            ScopedWriteOp::AddRemote {
                name: "origin".into(),
                url: "x".into(),
            },
            ScopedWriteOp::RemoveRemote {
                name: "origin".into(),
            },
            ScopedWriteOp::Undo,
            ScopedWriteOp::PullRebase {
                remote: "origin".into(),
            },
            ScopedWriteOp::ForcePush {
                remote: "origin".into(),
            },
            ScopedWriteOp::DiscardLocal {
                remote: "origin".into(),
            },
        ];
        for op in &ops {
            let e = explain(op);
            assert!(!e.what.0.is_empty(), "what is empty for {:?}", op);
            assert!(!e.risks.0.is_empty(), "risks is empty for {:?}", op);
        }
    }

    #[test]
    fn force_push_safety_mentions_force_with_lease() {
        // Snapshot drift test: the explanation must mention the actual safety flag.
        let e = explain(&ScopedWriteOp::ForcePush {
            remote: "origin".into(),
        });
        let safety = e.safety.expect("force_push must have safety").0;
        assert!(
            safety.contains("--force-with-lease"),
            "safety should mention --force-with-lease, got: {}",
            safety
        );
    }

    #[test]
    fn destructive_ops_have_recovery() {
        let destructive = vec![
            ScopedWriteOp::Reset {
                mode: ResetMode::Hard,
            },
            ScopedWriteOp::ForcePush {
                remote: "origin".into(),
            },
            ScopedWriteOp::DiscardLocal {
                remote: "origin".into(),
            },
        ];
        for op in &destructive {
            let e = explain(op);
            assert!(
                e.recovery.is_some(),
                "destructive op must explain recovery: {:?}",
                op
            );
        }
    }
}
