use anyhow::Context;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::commands::{PipelineStep, PushFailureType, RecoveryAction, ResetMode};

/// Snapshot of repo state captured before any pipeline step executes.
/// Persisted to ~/.glit/snapshots/<id>.json for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSnapshot {
    pub head_oid: Option<String>,
    pub index_oid: String,
    pub branch_name: Option<String>,
    pub captured_at: String,
    pub pipeline_id: String,
}

/// What `glit --recover` should do, given the repo's CURRENT state versus the
/// snapshot. Recovery must never fight git's own state machine (Phase 11): a
/// blind HEAD/index restore is only correct when the repo is Clean and the
/// snapshot was taken on a normal (attached) branch. Pure + table-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// A git operation is still in progress (`repo.state() != Clean`). Do NOT
    /// reset refs underneath it — advise git's native `--abort`, which restores
    /// pre-op state via git's own machinery.
    AdviseGitAbort,
    /// The snapshot was captured detached and the repo is now Clean with HEAD
    /// moved — the interrupted op most likely COMPLETED. A blind restore would
    /// soft-reset HEAD back to the mid-operation detached commit, manufacturing
    /// a stranded detached HEAD out of a success. Warn before restoring.
    WarnDetached,
    /// Safe to perform the normal snapshot restore (attached branch, Clean repo).
    Restore,
}

/// Decide how to recover, given the repo's current state, the snapshot, and the
/// current HEAD oid. Pure — no I/O — so the crash-window logic is unit-testable.
pub fn recovery_plan(
    repo_state: git2::RepositoryState,
    snapshot: &PipelineSnapshot,
    current_head: Option<git2::Oid>,
) -> RecoveryDecision {
    // git's own state machine is mid-operation — never reset refs under it.
    if repo_state != git2::RepositoryState::Clean {
        return RecoveryDecision::AdviseGitAbort;
    }
    let snap_head = snapshot
        .head_oid
        .as_ref()
        .and_then(|s| git2::Oid::from_str(s).ok());
    // A snapshot taken mid-operation records a detached HEAD (`shorthand()` is
    // "HEAD"); empty-repo captures have no branch at all.
    let snap_was_detached =
        matches!(snapshot.branch_name.as_deref(), Some("HEAD")) || snapshot.branch_name.is_none();
    // Detached snapshot + now-Clean repo + HEAD moved ⇒ the op completed;
    // restoring would strand a detached HEAD. (When HEAD hasn't moved, the
    // restore is a harmless no-op on refs, so fall through to Restore.)
    if snap_was_detached && current_head != snap_head {
        return RecoveryDecision::WarnDetached;
    }
    RecoveryDecision::Restore
}

/// Trait for recovery management — enables swapping recovery strategy.
pub trait RecoveryManager {
    fn capture_snapshot(&self, repo: &Repository) -> anyhow::Result<PipelineSnapshot>;
    fn check_for_interrupted(&self) -> anyhow::Result<Option<PipelineSnapshot>>;
    fn execute_recovery(
        &self,
        repo: &Repository,
        snapshot: &PipelineSnapshot,
    ) -> anyhow::Result<()>;
    fn delete_snapshot(&self, snapshot: &PipelineSnapshot) -> anyhow::Result<()>;
    /// List all preserved snapshots without modifying them.
    fn list_snapshots(&self) -> anyhow::Result<Vec<PipelineSnapshot>>;
    /// Delete a snapshot by its pipeline_id.
    fn delete_snapshot_by_id(&self, pipeline_id: &str) -> anyhow::Result<()>;
}

/// Default file-based recovery manager using ~/.glit/snapshots/.
pub struct FileRecoveryManager;

impl FileRecoveryManager {
    fn snapshots_dir() -> anyhow::Result<PathBuf> {
        let dir = crate::config::glit_dir()?.join("snapshots");
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create snapshots dir: {}", dir.display()))?;
        Ok(dir)
    }

    fn snapshot_path(pipeline_id: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::snapshots_dir()?.join(format!("{}.json", pipeline_id)))
    }
}

impl RecoveryManager for FileRecoveryManager {
    fn capture_snapshot(&self, repo: &Repository) -> anyhow::Result<PipelineSnapshot> {
        let head_oid = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .map(|o| o.to_string());

        let mut index = repo.index().context("Failed to read index")?;
        let index_oid = index
            .write_tree()
            .context("Failed to write index tree")?
            .to_string();

        let branch_name = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from));

        let pipeline_id = uuid::Uuid::new_v4().to_string();
        let captured_at = chrono::Local::now().to_rfc3339();

        let snapshot = PipelineSnapshot {
            head_oid,
            index_oid,
            branch_name,
            captured_at,
            pipeline_id,
        };

        // Persist to disk BEFORE any step executes
        let path = Self::snapshot_path(&snapshot.pipeline_id)?;
        let json =
            serde_json::to_string_pretty(&snapshot).context("Failed to serialize snapshot")?;
        fs::write(&path, json)
            .with_context(|| format!("Failed to write snapshot to {}", path.display()))?;

        Ok(snapshot)
    }

    fn check_for_interrupted(&self) -> anyhow::Result<Option<PipelineSnapshot>> {
        let dir = match Self::snapshots_dir() {
            Ok(d) => d,
            Err(_) => return Ok(None),
        };

        let mut snapshots: Vec<PipelineSnapshot> = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().extension().map_or(false, |e| e == "json") {
                    if let Ok(content) = fs::read_to_string(entry.path()) {
                        if let Ok(snap) = serde_json::from_str::<PipelineSnapshot>(&content) {
                            snapshots.push(snap);
                        }
                    }
                }
            }
        }

        // Return the most recent snapshot (there should typically be at most one)
        snapshots.sort_by(|a, b| b.captured_at.cmp(&a.captured_at));
        Ok(snapshots.into_iter().next())
    }

    fn execute_recovery(
        &self,
        repo: &Repository,
        snapshot: &PipelineSnapshot,
    ) -> anyhow::Result<()> {
        let current_head = repo.head().ok().and_then(|h| h.target());

        // Step 1: soft-reset HEAD if a commit was made during the pipeline
        let snap_head = snapshot
            .head_oid
            .as_ref()
            .and_then(|s| git2::Oid::from_str(s).ok());

        if current_head != snap_head {
            match snap_head {
                Some(oid) => {
                    let obj = repo
                        .find_object(oid, None)
                        .context("Cannot find snapshot HEAD object")?;
                    repo.reset(&obj, git2::ResetType::Soft, None)
                        .context("Failed to soft-reset HEAD")?;
                    println!("  Commit undone. Files remain staged.");
                }
                None => {
                    println!(
                        "  Cannot undo commit — this was the first commit in the repo.\n  \
                         The commit is your only history. Use git log to inspect it."
                    );
                    return Ok(());
                }
            }
        }

        // Step 2: restore index to pre-pipeline state
        let index_oid =
            git2::Oid::from_str(&snapshot.index_oid).context("Invalid index OID in snapshot")?;
        let tree = repo
            .find_tree(index_oid)
            .context("Cannot find snapshot index tree")?;
        let mut index = repo.index().context("Failed to read index")?;
        index
            .read_tree(&tree)
            .context("Failed to restore index from snapshot")?;
        index.write().context("Failed to write restored index")?;
        println!("  Index restored to pre-pipeline state.");

        // NEVER touch the working tree — user file edits are always preserved.
        self.delete_snapshot(snapshot)?;
        println!("\nRepo restored. Your file edits are untouched.");
        Ok(())
    }

    fn delete_snapshot(&self, snapshot: &PipelineSnapshot) -> anyhow::Result<()> {
        let path = Self::snapshot_path(&snapshot.pipeline_id)?;
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete snapshot {}", path.display()))?;
        }
        Ok(())
    }

    fn list_snapshots(&self) -> anyhow::Result<Vec<PipelineSnapshot>> {
        let dir = Self::snapshots_dir()?;
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for entry in
            fs::read_dir(&dir).with_context(|| format!("Failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let txt = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if let Ok(snap) = serde_json::from_str::<PipelineSnapshot>(&txt) {
                out.push(snap);
            }
        }
        out.sort_by(|a, b| b.captured_at.cmp(&a.captured_at));
        Ok(out)
    }

    fn delete_snapshot_by_id(&self, pipeline_id: &str) -> anyhow::Result<()> {
        let path = Self::snapshot_path(pipeline_id)?;
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete snapshot {}", path.display()))?;
        }
        Ok(())
    }
}

// =============================================================================
// Per-step recovery actions
// =============================================================================

impl PipelineStep {
    /// Determine the appropriate recovery action for a failed step.
    pub fn recovery_action(&self, snapshot: &PipelineSnapshot) -> RecoveryAction {
        match self {
            PipelineStep::Add { .. } => {
                let oid =
                    git2::Oid::from_str(&snapshot.index_oid).unwrap_or_else(|_| git2::Oid::zero());
                RecoveryAction::RestoreIndex {
                    to_oid: oid,
                    description: "unstage all files staged by this pipeline".into(),
                }
            }
            PipelineStep::Commit { .. } => RecoveryAction::SoftReset {
                to_oid: snapshot
                    .head_oid
                    .as_ref()
                    .and_then(|s| git2::Oid::from_str(s).ok()),
                description: "undo commit, keep files staged".into(),
            },
            PipelineStep::Push { .. } => RecoveryAction::NoLocalChange {
                description: "commit is safe locally — push can be retried".into(),
                suggestion: "glit push (when network or auth issue is resolved)".into(),
            },
            PipelineStep::Reset {
                mode: ResetMode::Hard,
            } => RecoveryAction::ReflogOnly {
                description: "working tree changes are gone".into(),
                hint: "git reflog → find hash → git checkout <hash>".into(),
            },
            _ => RecoveryAction::NoLocalChange {
                description: "operation had no effect on repo state".into(),
                suggestion: String::new(),
            },
        }
    }
}

// =============================================================================
// Push failure classification
// =============================================================================

/// Classify a push failure from the git2 error.
pub fn classify_push_failure(error: &git2::Error) -> PushFailureType {
    let msg = error.message().to_lowercase();
    match error.code() {
        git2::ErrorCode::NotFastForward => PushFailureType::NonFastForward,
        git2::ErrorCode::Auth => PushFailureType::AuthFailure,
        _ => {
            if msg.contains("network")
                || msg.contains("connection")
                || msg.contains("resolve")
                || msg.contains("timed out")
            {
                PushFailureType::NetworkError
            } else {
                PushFailureType::VerificationFailed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo_with_commit(dir: &TempDir) -> Repository {
        let repo = Repository::init(dir.path()).unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();
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

    #[test]
    fn test_capture_snapshot() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let mgr = FileRecoveryManager;

        let snapshot = mgr.capture_snapshot(&repo).unwrap();
        assert!(snapshot.head_oid.is_some());
        assert!(!snapshot.index_oid.is_empty());
        assert!(!snapshot.pipeline_id.is_empty());

        // Verify file was persisted
        let path = FileRecoveryManager::snapshot_path(&snapshot.pipeline_id).unwrap();
        assert!(path.exists());

        // Clean up
        mgr.delete_snapshot(&snapshot).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn test_check_for_interrupted() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let mgr = FileRecoveryManager;

        let snap = mgr.capture_snapshot(&repo).unwrap();

        // Verify via the snapshot file path directly — avoid flaking due to
        // parallel tests sharing the same snapshots directory (check_for_interrupted
        // returns the most-recent snapshot, which may be from another test thread).
        let path = FileRecoveryManager::snapshot_path(&snap.pipeline_id).unwrap();
        assert!(path.exists(), "snapshot file should exist on disk");

        // Also verify check_for_interrupted finds at least one snapshot
        let found = mgr.check_for_interrupted().unwrap();
        assert!(found.is_some(), "should find at least one snapshot");

        mgr.delete_snapshot(&snap).unwrap();
        assert!(!path.exists(), "snapshot file should be deleted");
    }

    // Phase 11 — situation-aware recovery. `recovery_plan` must never fight
    // git's own state machine.
    fn snap(head: &str, branch: &str) -> PipelineSnapshot {
        PipelineSnapshot {
            head_oid: Some(head.to_string()),
            index_oid: "b".repeat(40),
            branch_name: Some(branch.to_string()),
            captured_at: "t".to_string(),
            pipeline_id: "id".to_string(),
        }
    }

    #[test]
    fn recovery_plan_advises_git_abort_when_mid_operation() {
        let s = snap(&"a".repeat(40), "HEAD");
        for state in [
            git2::RepositoryState::Rebase,
            git2::RepositoryState::Merge,
            git2::RepositoryState::CherryPick,
        ] {
            assert_eq!(
                recovery_plan(state, &s, None),
                RecoveryDecision::AdviseGitAbort,
                "non-Clean state {:?} must advise git-native abort",
                state
            );
        }
    }

    #[test]
    fn recovery_plan_warns_on_completed_detached_op() {
        // The trap: snapshot captured detached (mid-rebase), repo now Clean, and
        // HEAD moved on to the rebased tip. A blind restore would strand HEAD.
        let s = snap(&"a".repeat(40), "HEAD");
        let completed = git2::Oid::from_str(&"c".repeat(40)).unwrap();
        assert_eq!(
            recovery_plan(git2::RepositoryState::Clean, &s, Some(completed)),
            RecoveryDecision::WarnDetached
        );
    }

    #[test]
    fn recovery_plan_restores_detached_when_head_unchanged() {
        // Continue crashed mid-way: repo Clean-ish but HEAD == snapshot ⇒ the
        // ref restore is a harmless no-op, so Restore (not a false warning).
        let same = "a".repeat(40);
        let same_oid = git2::Oid::from_str(&same).unwrap();
        let s = snap(&same, "HEAD");
        assert_eq!(
            recovery_plan(git2::RepositoryState::Clean, &s, Some(same_oid)),
            RecoveryDecision::Restore
        );
    }

    #[test]
    fn recovery_plan_restores_normal_commit_case() {
        // Attached branch, Clean repo, HEAD moved by a Commit → normal restore.
        let s = snap(&"a".repeat(40), "main");
        let after_commit = git2::Oid::from_str(&"c".repeat(40)).unwrap();
        assert_eq!(
            recovery_plan(git2::RepositoryState::Clean, &s, Some(after_commit)),
            RecoveryDecision::Restore
        );
    }

    #[test]
    fn test_full_recovery_roundtrip() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let mgr = FileRecoveryManager;

        // Capture snapshot
        let snapshot = mgr.capture_snapshot(&repo).unwrap();
        let original_head = repo.head().unwrap().target().unwrap();

        // Make a second commit (simulating a pipeline that committed)
        std::fs::write(dir.path().join("test.txt"), "modified").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("test.txt")).unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = git2::Signature::now("Test", "test@test.com").unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "pipeline commit",
                &tree,
                &[&parent],
            )
            .unwrap();
        }

        // HEAD should have advanced
        let new_head = repo.head().unwrap().target().unwrap();
        assert_ne!(new_head, original_head);

        // Recovery should restore HEAD to original
        mgr.execute_recovery(&repo, &snapshot).unwrap();

        let restored_head = repo.head().unwrap().target().unwrap();
        assert_eq!(restored_head, original_head);

        // Working tree should be untouched
        let content = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "modified"); // Working tree preserved!
    }

    #[test]
    fn test_recovery_action_for_add() {
        let snapshot = PipelineSnapshot {
            head_oid: Some("abc123".to_string()),
            index_oid: "def456".to_string(),
            branch_name: Some("main".to_string()),
            captured_at: "2024-01-01T00:00:00".to_string(),
            pipeline_id: "test".to_string(),
        };
        let step = PipelineStep::Add {
            files: vec!["test.rs".to_string()],
        };
        match step.recovery_action(&snapshot) {
            RecoveryAction::RestoreIndex { description, .. } => {
                assert!(description.contains("unstage"));
            }
            _ => panic!("Expected RestoreIndex"),
        }
    }

    #[test]
    fn test_recovery_action_for_push() {
        let snapshot = PipelineSnapshot {
            head_oid: Some("abc123".to_string()),
            index_oid: "def456".to_string(),
            branch_name: Some("main".to_string()),
            captured_at: "2024-01-01T00:00:00".to_string(),
            pipeline_id: "test".to_string(),
        };
        let step = PipelineStep::Push {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            set_upstream: false,
        };
        match step.recovery_action(&snapshot) {
            RecoveryAction::NoLocalChange { suggestion, .. } => {
                assert!(suggestion.contains("push"));
            }
            _ => panic!("Expected NoLocalChange"),
        }
    }

    #[test]
    fn test_recovery_action_for_hard_reset() {
        let snapshot = PipelineSnapshot {
            head_oid: Some("abc123".to_string()),
            index_oid: "def456".to_string(),
            branch_name: Some("main".to_string()),
            captured_at: "2024-01-01T00:00:00".to_string(),
            pipeline_id: "test".to_string(),
        };
        let step = PipelineStep::Reset {
            mode: ResetMode::Hard,
        };
        match step.recovery_action(&snapshot) {
            RecoveryAction::ReflogOnly { hint, .. } => {
                assert!(hint.contains("reflog"));
            }
            _ => panic!("Expected ReflogOnly"),
        }
    }
}
