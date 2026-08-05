use anyhow::Context;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::commands::{PipelineStep, PushFailureType, RecoveryAction, ResetMode};

/// Snapshot of repo state captured before any pipeline step executes.
/// Persisted to ~/.gitrust/snapshots/<id>.json for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSnapshot {
    pub head_oid: Option<String>,
    pub index_oid: String,
    pub branch_name: Option<String>,
    pub captured_at: String,
    pub pipeline_id: String,
    /// Absolute workdir of the repo this snapshot belongs to (S56). Filters
    /// `check_for_interrupted` so a crash in worktree/repo A never prompts
    /// recovery inside worktree/repo B. `#[serde(default)]` keeps pre-Phase-14
    /// snapshot files (which lack the field) deserializable — they get an empty
    /// path and simply won't match any repo, which is the safe default.
    #[serde(default)]
    pub repo_workdir: String,
}

/// What `gitrust --recover` should do, given the repo's CURRENT state versus the
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
    /// Find the most-recent interrupted-pipeline snapshot. When `repo_workdir`
    /// is `Some`, only snapshots captured in that repo are considered (S56 —
    /// prevents worktree/multi-repo recovery cross-talk). `None` disables the
    /// filter (used where the current repo isn't known).
    fn check_for_interrupted(
        &self,
        repo_workdir: Option<&str>,
    ) -> anyhow::Result<Option<PipelineSnapshot>>;
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

    /// Delete every snapshot captured in `repo_workdir`, returning the count
    /// removed. Used when an in-progress op (rebase/merge) is RESOLVED to a
    /// clean state — the snapshot that was intentionally preserved at the pause
    /// is now stale and must not keep nagging "interrupted pipeline" on later
    /// commands (M-08 Defect 3). Scoped to the workdir so it never clears
    /// another repo's live snapshot. Best-effort per entry.
    fn clear_snapshots_for_workdir(&self, repo_workdir: &str) -> anyhow::Result<usize> {
        let mut cleared = 0;
        for snap in self.list_snapshots()?.into_iter() {
            if snap.repo_workdir == repo_workdir && self.delete_snapshot(&snap).is_ok() {
                cleared += 1;
            }
        }
        Ok(cleared)
    }
}

/// Default file-based recovery manager using ~/.gitrust/snapshots/.
pub struct FileRecoveryManager;

impl FileRecoveryManager {
    fn snapshots_dir() -> anyhow::Result<PathBuf> {
        let dir = crate::config::gitrust_dir()?.join("snapshots");
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
        let repo_workdir = repo
            .workdir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let snapshot = PipelineSnapshot {
            head_oid,
            index_oid,
            branch_name,
            captured_at,
            pipeline_id,
            repo_workdir,
        };

        // Persist to disk BEFORE any step executes
        let path = Self::snapshot_path(&snapshot.pipeline_id)?;
        let json =
            serde_json::to_string_pretty(&snapshot).context("Failed to serialize snapshot")?;
        fs::write(&path, json)
            .with_context(|| format!("Failed to write snapshot to {}", path.display()))?;

        Ok(snapshot)
    }

    fn check_for_interrupted(
        &self,
        repo_workdir: Option<&str>,
    ) -> anyhow::Result<Option<PipelineSnapshot>> {
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

        // S56: only consider snapshots from the current repo when a workdir is
        // given. A snapshot from another repo/worktree must never prompt
        // recovery here. Legacy snapshots (empty `repo_workdir`) never match a
        // real workdir, so they are silently skipped under the filter.
        if let Some(wd) = repo_workdir {
            snapshots.retain(|s| s.repo_workdir == wd);
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
                suggestion: "gitrust push (when network or auth issue is resolved)".into(),
            },
            PipelineStep::Reset {
                mode: ResetMode::Hard,
                ..
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

// =============================================================================
// Forensic recovery (Phase 14 — S24/S26/S27; P5). Deterministic, zero-LLM: find
// commits that are no longer reachable from any ref so the user can restore
// them onto a rescue branch. TWO sources are required by verified git behavior:
//
//   Source A — reflog walk (HEAD + LIVE local-branch reflogs). Carries the
//     centerpiece: a commit made on a branch that is then deleted still lives in
//     HEAD's reflog. (Deleted-branch reflogs are REMOVED by `git branch -D`, so
//     enumerating them returns nothing — deliberately not attempted.)
//
//   Source B — bounded unreachable-object scan (`odb.foreach`). The only path
//     that surfaces DROPPED STASHES (they vanish from the stash reflog and never
//     appear in HEAD's reflog) and branches never checked out locally.
//
// Residual gap (documented, S29): objects already garbage-collected are gone;
// never-committed work was never in the odb. The browser's empty-result copy
// says so plainly (copy::FORENSIC_NONE_FOUND).
// =============================================================================

/// A commit unreachable from any current ref — a recovery candidate.
#[derive(Debug, Clone)]
pub struct LostCommit {
    pub oid: String,
    pub age_secs: u64,
    pub summary: String,
    pub files_touched: usize,
    /// Distinctive stash shape ("WIP on …"/"On …", 2–3 parents) — a dropped
    /// stash (S27). Surfaced only by the odb scan.
    pub is_stash_shaped: bool,
}

/// Bounds for `find_lost_commits` so an ancient/huge repo never hangs or floods.
#[derive(Debug, Clone, Copy)]
pub struct LostScanConfig {
    /// Max reflog entries to inspect per reflog (Source A).
    pub max_per_source: usize,
    /// Exclude commits older than this many seconds (both sources + the
    /// reachability walk share this cutoff).
    pub age_cutoff_secs: u64,
    /// Hard cap on the number of candidates returned.
    pub max_candidates: usize,
    /// Test hook: disable Source B (the odb scan) to prove Source A alone
    /// carries the deleted-branch centerpiece. Always `false` in production.
    pub disable_odb_scan: bool,
}

impl LostScanConfig {
    /// Sensible defaults: 30-day window, generous but bounded caps.
    pub fn with_retention_days(days: u64) -> Self {
        Self {
            max_per_source: 500,
            age_cutoff_secs: days.saturating_mul(24 * 60 * 60),
            max_candidates: 100,
            disable_odb_scan: false,
        }
    }
}

/// Result of a scan: the (bounded) candidates plus the total seen before the
/// `max_candidates` cap, so the caller can honestly report truncation.
#[derive(Debug, Clone)]
pub struct LostScan {
    pub commits: Vec<LostCommit>,
    pub total_found: usize,
}

/// Find commits unreachable from any current ref. See module docs for the
/// two-source rationale. Pure read — never mutates the repo.
pub fn find_lost_commits(repo: &Repository, cfg: &LostScanConfig) -> LostScan {
    use std::collections::HashSet;

    let now = chrono::Local::now().timestamp();
    let cutoff = now - cfg.age_cutoff_secs as i64;

    // Reachability set: every oid reachable from any current ref, bounded by the
    // same age cutoff. Over-tight only yields a harmless false-lost (a redundant
    // rescue branch) — it can never HIDE a genuinely lost commit.
    let reachable = reachable_oids(repo, cutoff);

    let mut candidates: Vec<LostCommit> = Vec::new();
    let mut seen: HashSet<git2::Oid> = HashSet::new();

    let mut consider = |oid: git2::Oid, candidates: &mut Vec<LostCommit>| {
        if reachable.contains(&oid) || seen.contains(&oid) {
            return;
        }
        if let Some(lc) = lost_commit_if_recent(repo, oid, cutoff) {
            seen.insert(oid);
            candidates.push(lc);
        }
    };

    // -- Source A: HEAD + live local-branch reflogs.
    let mut reflog_names: Vec<String> = vec!["HEAD".to_string()];
    if let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) {
        for (branch, _) in branches.flatten() {
            if let Some(name) = branch.name().ok().flatten() {
                reflog_names.push(format!("refs/heads/{}", name));
            }
        }
    }
    for name in reflog_names {
        if let Ok(reflog) = repo.reflog(&name) {
            for entry in reflog.iter().take(cfg.max_per_source) {
                consider(entry.id_new(), &mut candidates);
                consider(entry.id_old(), &mut candidates);
            }
        }
    }

    // -- Source B: bounded unreachable-object scan (finds dropped stashes).
    if !cfg.disable_odb_scan {
        if let Ok(odb) = repo.odb() {
            let mut scanned_commits = 0usize;
            let _ = odb.foreach(|oid| {
                if scanned_commits >= cfg.max_per_source {
                    return true; // keep iterating cheaply; we just stop considering
                }
                // Cheap header read filters to commits before any full parse.
                if let Ok((_, kind)) = odb.read_header(*oid) {
                    if kind == git2::ObjectType::Commit {
                        scanned_commits += 1;
                        consider(*oid, &mut candidates);
                    }
                }
                true
            });
        }
    }

    // Most-recent first.
    candidates.sort_by(|a, b| a.age_secs.cmp(&b.age_secs));
    let total_found = candidates.len();
    candidates.truncate(cfg.max_candidates);
    LostScan {
        commits: candidates,
        total_found,
    }
}

/// Build the set of oids reachable from every current ref (branches, tags, HEAD,
/// remote-tracking), bounded by `cutoff` (stop descending past older commits).
fn reachable_oids(repo: &Repository, cutoff: i64) -> std::collections::HashSet<git2::Oid> {
    use std::collections::HashSet;
    let mut set: HashSet<git2::Oid> = HashSet::new();
    let mut revwalk = match repo.revwalk() {
        Ok(r) => r,
        Err(_) => return set,
    };
    // Push every ref target.
    if let Ok(refs) = repo.references() {
        for r in refs.flatten() {
            if let Some(oid) = r.target() {
                let _ = revwalk.push(oid);
            }
        }
    }
    let _ = revwalk.push_head();
    // We only need to recognize oids that could collide with a candidate, and
    // candidates are all newer than `cutoff`. So stop inserting once a commit is
    // older than the cutoff — its ancestors are older still and can't be
    // candidates. This bounds the walk in an ancient repo. Erring by keeping a
    // few extra (a slightly looser set) only ever suppresses a false-lost, never
    // hides a real one.
    for oid in revwalk.flatten() {
        if let Ok(c) = repo.find_commit(oid) {
            if c.time().seconds() < cutoff {
                continue;
            }
        }
        set.insert(oid);
    }
    set
}

/// Parse `oid` as a commit; return a `LostCommit` iff it is a commit newer than
/// `cutoff`. Peeks nothing that a full `find_commit` doesn't already need.
fn lost_commit_if_recent(repo: &Repository, oid: git2::Oid, cutoff: i64) -> Option<LostCommit> {
    let commit = repo.find_commit(oid).ok()?;
    let secs = commit.time().seconds();
    if secs < cutoff {
        return None;
    }
    let age_secs = (chrono::Local::now().timestamp() - secs).max(0) as u64;
    let summary = commit.summary().unwrap_or("(no message)").to_string();
    let full_msg = commit.message().unwrap_or("");
    let parents = commit.parent_count();
    let is_stash_shaped = (full_msg.starts_with("WIP on ") || full_msg.starts_with("On "))
        && (2..=3).contains(&parents);
    // files_touched: diff against first parent (or empty tree for a root commit).
    let files_touched = count_files_touched(repo, &commit);
    Some(LostCommit {
        oid: oid.to_string(),
        age_secs,
        summary,
        files_touched,
        is_stash_shaped,
    })
}

/// Number of files changed by `commit` vs its first parent (root commit → whole tree).
fn count_files_touched(repo: &Repository, commit: &git2::Commit) -> usize {
    let this_tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&this_tree), None)
        .map(|d| d.deltas().count())
        .unwrap_or(0)
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
        // (unfiltered — other test threads may share the directory).
        let found = mgr.check_for_interrupted(None).unwrap();
        assert!(found.is_some(), "should find at least one snapshot");

        mgr.delete_snapshot(&snap).unwrap();
        assert!(!path.exists(), "snapshot file should be deleted");
    }

    // S56: a snapshot captured in repo A must NOT surface when checking scoped
    // to repo B's workdir. Snapshots share ~/.gitrust/snapshots across the machine,
    // so the workdir filter is what prevents worktree/multi-repo cross-talk.
    #[test]
    fn check_for_interrupted_scoped_to_repo_workdir() {
        let dir_a = TempDir::new().unwrap();
        let repo_a = init_repo_with_commit(&dir_a);
        let mgr = FileRecoveryManager;
        let snap = mgr.capture_snapshot(&repo_a).unwrap();
        assert!(
            !snap.repo_workdir.is_empty(),
            "capture must record the workdir"
        );

        // Repo B: a different workdir path that never captured a snapshot.
        let dir_b = TempDir::new().unwrap();
        let workdir_b = dir_b.path().to_string_lossy().into_owned();
        let found_b = mgr.check_for_interrupted(Some(&workdir_b)).unwrap();
        assert!(
            found_b.map(|s| s.pipeline_id) != Some(snap.pipeline_id.clone()),
            "repo A's snapshot must not surface when scoped to repo B"
        );

        // Sanity: scoped to repo A's own workdir, it IS found.
        let found_a = mgr.check_for_interrupted(Some(&snap.repo_workdir)).unwrap();
        assert_eq!(
            found_a.map(|s| s.pipeline_id),
            Some(snap.pipeline_id.clone()),
            "repo A's snapshot must surface when scoped to repo A"
        );

        mgr.delete_snapshot(&snap).unwrap();
    }

    #[test]
    fn clear_snapshots_for_workdir_removes_only_matching_repo() {
        // M-08 Defect 3: resolving an in-progress op clears the stale snapshot
        // for THIS repo's workdir, and only that repo's.
        let dir_a = TempDir::new().unwrap();
        let repo_a = init_repo_with_commit(&dir_a);
        let dir_b = TempDir::new().unwrap();
        let repo_b = init_repo_with_commit(&dir_b);
        let mgr = FileRecoveryManager;

        let snap_a = mgr.capture_snapshot(&repo_a).unwrap();
        let snap_b = mgr.capture_snapshot(&repo_b).unwrap();

        let cleared = mgr
            .clear_snapshots_for_workdir(&snap_a.repo_workdir)
            .unwrap();
        assert_eq!(cleared, 1, "exactly repo A's snapshot should be cleared");

        // A's snapshot is gone...
        assert!(
            mgr.check_for_interrupted(Some(&snap_a.repo_workdir))
                .unwrap()
                .is_none(),
            "repo A's snapshot must be cleared"
        );
        // ...but B's survives (never nag another repo).
        assert_eq!(
            mgr.check_for_interrupted(Some(&snap_b.repo_workdir))
                .unwrap()
                .map(|s| s.pipeline_id),
            Some(snap_b.pipeline_id.clone()),
            "repo B's snapshot must be untouched"
        );

        mgr.delete_snapshot(&snap_b).unwrap();
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
            repo_workdir: String::new(),
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
            repo_workdir: String::new(),
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
            repo_workdir: String::new(),
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
            repo_workdir: String::new(),
        };
        let step = PipelineStep::Reset {
            mode: ResetMode::Hard,
            to_oid: None,
        };
        match step.recovery_action(&snapshot) {
            RecoveryAction::ReflogOnly { hint, .. } => {
                assert!(hint.contains("reflog"));
            }
            _ => panic!("Expected ReflogOnly"),
        }
    }

    // -------------------------------------------------------------------------
    // Forensic recovery (Phase 14). Uses REAL git via git2 in a tempdir.
    // -------------------------------------------------------------------------

    /// Commit `content` to `f.txt` on the current HEAD; return the new oid.
    fn commit_file(repo: &Repository, dir: &TempDir, content: &str, msg: &str) -> git2::Oid {
        std::fs::write(dir.path().join("f.txt"), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("f.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    fn cfg_days(days: u64) -> LostScanConfig {
        LostScanConfig::with_retention_days(days)
    }

    // CENTERPIECE: commit on a branch, delete the branch, the scan finds it.
    #[test]
    fn find_lost_commits_surfaces_deleted_branch_commit() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Branch off, commit on it, return to the original branch, delete it.
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &base, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        let lost = commit_file(&repo, &dir, "feature work\n", "feat: export csv");
        // Switch back to the base branch (main/master) and delete feature.
        let base_branch = {
            // Any local branch that isn't "feature".
            let mut name = None;
            for (b, _) in repo
                .branches(Some(git2::BranchType::Local))
                .unwrap()
                .flatten()
            {
                if let Some(n) = b.name().ok().flatten() {
                    if n != "feature" {
                        name = Some(n.to_string());
                    }
                }
            }
            name.unwrap()
        };
        repo.set_head(&format!("refs/heads/{}", base_branch))
            .unwrap();
        repo.reset(base.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        repo.find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let scan = find_lost_commits(&repo, &cfg_days(30));
        assert!(
            scan.commits.iter().any(|c| c.oid == lost.to_string()),
            "deleted-branch commit must surface; found {:?}",
            scan.commits.iter().map(|c| &c.summary).collect::<Vec<_>>()
        );
        let found = scan
            .commits
            .iter()
            .find(|c| c.oid == lost.to_string())
            .unwrap();
        assert_eq!(found.summary, "feat: export csv");
        assert!(found.files_touched >= 1);
    }

    // Source A (reflog) ALONE carries the centerpiece — prove it with the odb
    // scan disabled. (If this failed, we'd know the reflog source was broken.)
    #[test]
    fn deleted_branch_commit_found_by_reflog_alone() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &base, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        let lost = commit_file(&repo, &dir, "work\n", "feat: reflog-only");
        // The commit is now in HEAD's reflog regardless of the branch's fate.
        // Make it genuinely lost: leave feature, hard-reset base, delete feature.
        repo.set_head("refs/heads/master")
            .or_else(|_| repo.set_head("refs/heads/main"))
            .ok();
        repo.reset(base.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        repo.find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let mut cfg = cfg_days(30);
        cfg.disable_odb_scan = true; // Source A (reflog) only.
        let scan = find_lost_commits(&repo, &cfg);
        assert!(
            scan.commits.iter().any(|c| c.oid == lost.to_string()),
            "reflog source alone must find the deleted-branch commit"
        );
    }

    // Dropped stash → surfaced ONLY by Source B (odb scan) with is_stash_shaped.
    // This test is impossible under a reflog-only design.
    #[test]
    fn find_lost_commits_surfaces_dropped_stash() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        // Create a dirty change and stash it via real git so the stash commit is
        // shaped exactly as git makes it.
        std::fs::write(dir.path().join("f.txt"), "dirty\n").unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["stash", "push", "-u", "-m", "wip"]);
        // Drop it — now it's dangling: gone from the stash reflog, never in HEAD's.
        run(&["stash", "drop"]);

        let scan = find_lost_commits(&repo, &cfg_days(30));
        assert!(
            scan.commits.iter().any(|c| c.is_stash_shaped),
            "dropped stash must surface with is_stash_shaped; got {:?}",
            scan.commits
                .iter()
                .map(|c| (&c.summary, c.is_stash_shaped))
                .collect::<Vec<_>>()
        );
    }

    // Bounded: max_candidates truncates and total_found reports the real total.
    #[test]
    fn find_lost_commits_respects_candidate_cap() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        // Make several lost commits by committing then resetting HEAD back each
        // time — each abandoned commit stays in the reflog.
        for i in 0..5 {
            commit_file(&repo, &dir, &format!("v{i}\n"), &format!("lost {i}"));
            repo.reset(base.as_object(), git2::ResetType::Hard, None)
                .unwrap();
        }
        let mut cfg = cfg_days(30);
        cfg.max_candidates = 2;
        let scan = find_lost_commits(&repo, &cfg);
        assert!(scan.commits.len() <= 2, "cap must bound returned commits");
        assert!(
            scan.total_found >= scan.commits.len(),
            "total_found must be >= returned; total={} returned={}",
            scan.total_found,
            scan.commits.len()
        );
    }
}
