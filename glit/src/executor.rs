use crate::commands::PipelineStep;
use crate::git_ops::GitOperations;

/// Trait for step execution — enables swapping execution strategy.
pub trait StepExecutor {
    fn execute(&self, repo: &mut git2::Repository, step: &PipelineStep) -> anyhow::Result<()>;
}

/// Default executor backed by a GitOperations implementation.
pub struct DefaultExecutor<G: GitOperations> {
    pub git_ops: G,
    /// Hook routing mode from config ("auto" | "git2" | "system"). Governs
    /// whether Commit runs through system `git` (so hooks execute) or git2.
    pub hooks_mode: String,
}

impl<G: GitOperations> StepExecutor for DefaultExecutor<G> {
    fn execute(&self, repo: &mut git2::Repository, step: &PipelineStep) -> anyhow::Result<()> {
        use crate::git_ops::{hooks_for_op, should_route_to_system_git, HookOp};
        // Hook-aware routing (S47): libgit2 never runs hooks, so route commit
        // through system `git` when executable commit hooks are present (auto)
        // or always (system). Push already shells out, so it needs no routing.
        if let PipelineStep::Commit { message } = step {
            let present = !hooks_for_op(repo, HookOp::Commit).is_empty();
            if should_route_to_system_git(&self.hooks_mode, present) {
                println!("  {}", crate::copy::HOOKS_USING_SYSTEM_GIT);
                return self.git_ops.commit_system(repo, message).map(|_| ());
            }
        }
        crate::git_ops::execute_step(&self.git_ops, repo, step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_ops::{ClassifiedFailure, FailureClass, Git2Operations};
    use git2::Repository;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// S47 ⚗️: libgit2 never runs hooks, so GLit's git2 commit silently bypasses
    /// a team's pre-commit gate. With hook-aware routing, a failing executable
    /// pre-commit hook must block the commit and surface as a HookRejected
    /// (Recoverable) failure — the blueprint's own test-matrix scenario that
    /// was previously impossible to pass.
    #[test]
    fn test_commit_with_failing_pre_commit_hook_is_blocked() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
        }
        let path = dir.path();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();

        // Initial commit so HEAD exists.
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);

        // Failing executable pre-commit hook under .git/hooks.
        let hooks_dir = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook = hooks_dir.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'blocked by pre-commit hook' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Stage a change to commit.
        std::fs::write(path.join("a.txt"), "two\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();

        let exec = DefaultExecutor {
            git_ops: Git2Operations,
            hooks_mode: "auto".to_string(),
        };
        let result = exec.execute(
            &mut repo,
            &PipelineStep::Commit {
                message: "second".to_string(),
            },
        );

        let err = result.expect_err("commit must be blocked by the failing pre-commit hook");
        let classified = err
            .downcast_ref::<ClassifiedFailure>()
            .expect("error should be a ClassifiedFailure");
        assert!(
            matches!(classified.class, FailureClass::HookRejected { .. }),
            "expected HookRejected, got {:?}",
            classified.class
        );
    }
}
