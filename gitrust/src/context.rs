use anyhow::Context;
use git2::Repository;

use crate::commands::RepoSituation;

const MAX_FILES_PER_CATEGORY: usize = 15;
/// A `.git/index.lock` older than this is treated as stale/abandoned (S22).
const STALE_LOCK_SECS: u64 = 300;

/// Summary of the current repository state, collected before every LLM call.
#[derive(Debug, Clone)]
pub struct RepoContext {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub status: StatusSummary,
    /// Origin remote URL if configured — used to derive web URLs.
    pub origin_url: Option<String>,
    pub last_commit: String,
    pub is_empty_repo: bool,
    pub is_detached: bool,
    /// True if at least one stash entry exists.
    pub has_stash: bool,
    /// The deterministic repository situation (Layer 0 state machine, §12).
    /// Drives the situation gate and the classifier prompt. Computed by
    /// `detect_situation`; subsumes the Phase 9 raw `state` field so there is a
    /// single state representation.
    pub situation: RepoSituation,
}

/// File status grouped by category, truncated to MAX_FILES_PER_CATEGORY.
#[derive(Debug, Clone)]
pub struct StatusSummary {
    /// Unmerged paths (mid-merge/rebase/cherry-pick/revert conflicts). Listed
    /// first: they block everything else.
    pub conflicted: Vec<String>,
    pub staged: Vec<String>,
    pub staged_overflow: usize,
    pub modified: Vec<String>,
    pub modified_overflow: usize,
    pub untracked: Vec<String>,
    pub untracked_overflow: usize,
}

/// Collect full repository context (Layer 0).
pub fn build_context(repo: &Repository) -> anyhow::Result<RepoContext> {
    let is_empty_repo = repo.head().is_err();
    let is_detached = repo.head_detached().unwrap_or(false);

    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from))
        .unwrap_or_else(|| {
            if is_detached {
                "(detached HEAD)".to_string()
            } else {
                "(no branch)".to_string()
            }
        });

    let (upstream, ahead, behind) = get_upstream_info(repo, &branch);

    let last_commit = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| {
            let relative = crate::format::format_relative(c.time().seconds());
            format!(
                "{} \"{}\" ({})",
                &c.id().to_string()[..7],
                c.summary().unwrap_or(""),
                relative
            )
        })
        .unwrap_or_else(|| "(no commits yet)".to_string());

    let origin_url = repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(String::from));
    let status = build_status_summary(repo)?;
    let has_stash = repo_has_stash(repo);
    let situation = detect_situation(repo);

    Ok(RepoContext {
        branch,
        upstream,
        ahead,
        behind,
        status,
        origin_url,
        last_commit,
        is_empty_repo,
        is_detached,
        has_stash,
        situation,
    })
}

/// One-line, plain-language description of a non-trivial situation, for Kev's
/// repo-state line (`intent::repo_state_line`). Returns `None` for Normal / EmptyRepo / NotARepo
/// (EmptyRepo is already conveyed by the "(no commits yet)" last-commit line).
pub fn describe_situation(s: &RepoSituation) -> Option<String> {
    use RepoSituation as S;
    match s {
        S::Normal | S::EmptyRepo | S::NotARepo => None,
        S::DetachedHead {
            stranded_commits: 0,
        } => Some("detached HEAD".to_string()),
        S::DetachedHead { stranded_commits } => Some(format!(
            "detached HEAD ({} commit(s) not on any branch)",
            stranded_commits
        )),
        S::MidMerge {
            resolved: false,
            conflicts,
        } => Some(format!(
            "merge in progress (conflicts in {} file(s))",
            conflicts.len()
        )),
        S::MidMerge { resolved: true, .. } => {
            Some("merge in progress (conflicts resolved — ready to commit)".to_string())
        }
        S::MidRebase {
            current,
            total,
            conflicted,
        } if conflicted.is_empty() => Some(format!("rebase in progress ({}/{})", current, total)),
        S::MidRebase {
            current,
            total,
            conflicted,
        } => Some(format!(
            "rebase in progress ({}/{}, conflicts in {} file(s))",
            current,
            total,
            conflicted.len()
        )),
        S::MidCherryPick { .. } => Some("cherry-pick in progress".to_string()),
        S::MidRevert { .. } => Some("revert in progress".to_string()),
        S::MidBisect => Some("bisect in progress".to_string()),
        S::Diverged { ahead, behind } => {
            Some(format!("diverged (ahead {}, behind {})", ahead, behind))
        }
        S::RemoteRewritten { .. } => Some("remote history was rewritten".to_string()),
        S::StaleLock { age_secs } => Some(format!("stale index.lock (~{}s)", age_secs)),
    }
}

/// Deterministically classify the repository into a `RepoSituation` (Layer 0
/// state machine, §12). **No LLM anywhere** — only `repo.state()`, index
/// inspection, reflog/ancestor tests, and file mtimes. Checked in a fixed
/// priority order so the most actionable state wins when several coexist:
/// EmptyRepo → in-progress op → detached → stale lock → remote-rewritten →
/// diverged → Normal.
pub fn detect_situation(repo: &Repository) -> RepoSituation {
    use git2::RepositoryState as RS;

    // 1. Empty repo — no HEAD commit.
    if repo.head().is_err() {
        return RepoSituation::EmptyRepo;
    }

    // 2. In-progress operation.
    match repo.state() {
        RS::Merge => {
            let conflicts = conflicted_paths(repo);
            return RepoSituation::MidMerge {
                resolved: conflicts.is_empty(),
                conflicts,
            };
        }
        RS::Rebase
        | RS::RebaseInteractive
        | RS::RebaseMerge
        | RS::ApplyMailbox
        | RS::ApplyMailboxOrRebase => {
            let (current, total) = rebase_progress(repo);
            return RepoSituation::MidRebase {
                current,
                total,
                conflicted: conflicted_paths(repo),
            };
        }
        RS::CherryPick | RS::CherryPickSequence => {
            return RepoSituation::MidCherryPick {
                conflicted: conflicted_paths(repo),
            };
        }
        RS::Revert | RS::RevertSequence => {
            return RepoSituation::MidRevert {
                conflicted: conflicted_paths(repo),
            };
        }
        RS::Bisect => return RepoSituation::MidBisect,
        RS::Clean => {}
    }

    // 3. Detached HEAD (clean).
    if repo.head_detached().unwrap_or(false) {
        return RepoSituation::DetachedHead {
            stranded_commits: stranded_commit_count(repo),
        };
    }

    // 4. Stale index.lock.
    if let Some(age_secs) = stale_lock_age(repo) {
        return RepoSituation::StaleLock { age_secs };
    }

    // 5/6. Remote-rewritten vs diverged (need the current branch name).
    if let Some(branch) = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from))
    {
        if let Some((old_tip, new_tip)) = detect_remote_rewritten(repo, &branch) {
            return RepoSituation::RemoteRewritten { old_tip, new_tip };
        }
        let (_up, ahead, behind) = get_upstream_info(repo, &branch);
        if ahead > 0 && behind > 0 {
            return RepoSituation::Diverged {
                ahead: ahead as u32,
                behind: behind as u32,
            };
        }
    }

    RepoSituation::Normal
}

/// Deduplicated conflicted paths from the index (empty when conflict-free).
fn conflicted_paths(repo: &Repository) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(index) = repo.index() {
        if let Ok(conflicts) = index.conflicts() {
            for c in conflicts.flatten() {
                if let Some(e) = c.our.or(c.their).or(c.ancestor) {
                    if let Ok(p) = std::str::from_utf8(&e.path) {
                        let p = p.to_string();
                        if !out.contains(&p) {
                            out.push(p);
                        }
                    }
                }
            }
        }
    }
    out
}

/// (current, total) for an in-progress rebase, read from the rebase state dir
/// (merge backend: `rebase-merge/{msgnum,end}`; apply backend:
/// `rebase-apply/{next,last}`). `(0, 0)` when unavailable.
fn rebase_progress(repo: &Repository) -> (u32, u32) {
    let gitdir = repo.path();
    let read = |dir: &str, f: &str| -> Option<u32> {
        std::fs::read_to_string(gitdir.join(dir).join(f))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    };
    if let (Some(cur), Some(tot)) = (read("rebase-merge", "msgnum"), read("rebase-merge", "end")) {
        return (cur, tot);
    }
    if let (Some(cur), Some(tot)) = (read("rebase-apply", "next"), read("rebase-apply", "last")) {
        return (cur, tot);
    }
    (0, 0)
}

/// Count commits reachable from HEAD but from no local branch ref (S20/S21).
fn stranded_commit_count(repo: &Repository) -> u32 {
    let head = match repo.head().ok().and_then(|h| h.target()) {
        Some(o) => o,
        None => return 0,
    };
    let mut revwalk = match repo.revwalk() {
        Ok(r) => r,
        Err(_) => return 0,
    };
    if revwalk.push(head).is_err() {
        return 0;
    }
    if let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) {
        for (branch, _) in branches.flatten() {
            if let Some(oid) = branch.get().target() {
                let _ = revwalk.hide(oid);
            }
        }
    }
    revwalk.filter(|r| r.is_ok()).count() as u32
}

/// Detect that origin's history was rewritten under us: the tracking ref's
/// previous tip (from its own reflog) is not an ancestor of its current tip
/// (i.e. the update was not a fast-forward). Returns `(old_tip, new_tip)`.
fn detect_remote_rewritten(repo: &Repository, branch: &str) -> Option<(git2::Oid, git2::Oid)> {
    let refname = format!("refs/remotes/origin/{}", branch);
    let new_tip = repo.refname_to_id(&refname).ok()?;
    let reflog = repo.reflog(&refname).ok()?;
    let old_tip = reflog.get(0)?.id_old();
    if old_tip.is_zero() || old_tip == new_tip {
        return None;
    }
    match repo.graph_descendant_of(new_tip, old_tip) {
        Ok(true) => None, // fast-forward — not a rewrite
        Ok(false) => Some((old_tip, new_tip)),
        Err(_) => None,
    }
}

/// Age (seconds) of a stale `.git/index.lock`, or `None` if absent/fresh.
fn stale_lock_age(repo: &Repository) -> Option<u64> {
    let meta = std::fs::metadata(repo.path().join("index.lock")).ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(meta.modified().ok()?)
        .ok()?
        .as_secs();
    (age > STALE_LOCK_SECS).then_some(age)
}

/// Returns true if the given remote is configured.
pub fn remote_exists(repo: &Repository, name: &str) -> bool {
    repo.find_remote(name).is_ok()
}

/// Cheap stash-presence check — only peeks at the first entry.
fn repo_has_stash(repo: &Repository) -> bool {
    // `stash_foreach` requires a mutable repo; checking the reflog of refs/stash is read-only.
    repo.find_reference("refs/stash").is_ok()
}

/// Serialize context to natural language for the LLM — NOT JSON.
impl RepoContext {
    /// Empty context for use when the repository cannot be read.
    pub fn empty() -> Self {
        RepoContext {
            branch: "(unknown)".to_string(),
            upstream: None,
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
            last_commit: "(unavailable)".to_string(),
            is_empty_repo: true,
            is_detached: false,
            has_stash: false,
            situation: RepoSituation::NotARepo,
        }
    }

    /// True when the working tree or index has any uncommitted changes.
    pub fn has_uncommitted(&self) -> bool {
        !self.status.conflicted.is_empty()
            || !self.status.staged.is_empty()
            || !self.status.modified.is_empty()
            || !self.status.untracked.is_empty()
            || self.status.staged_overflow > 0
            || self.status.modified_overflow > 0
            || self.status.untracked_overflow > 0
    }

    /// Cheap proxy for "this branch has commits from people other than me."
    /// We don't fetch author info into RepoContext — instead, behind-count > 0
    /// is a reasonable hint that someone else has been pushing here. Combined
    /// with `shared_branch`'s name check, this catches the common cases.
    pub fn has_foreign_commits(&self) -> bool {
        self.behind > 0
    }

    /// Human-facing one-line header for `gitrust status` / `gitrust branches`.
    /// Mirrors `git status -sb` but richer.
    pub fn render_status_header(&self) -> String {
        if self.is_empty_repo {
            return "Empty repository — no commits yet.".to_string();
        }
        if self.is_detached {
            // Lead with what it means, not the jargon (design handoff Flow 04).
            let at = self.last_commit.split_whitespace().next().unwrap_or("");
            return if at.is_empty() || at.starts_with('(') {
                "Not on a branch".to_string()
            } else {
                format!("Not on a branch · at {}", at)
            };
        }
        let mut header = format!("On branch {}", self.branch);
        match &self.upstream {
            Some(up) => {
                // Trim "refs/remotes/" prefix for readability.
                let short = up.strip_prefix("refs/remotes/").unwrap_or(up);
                header += &format!(" · tracking {}", short);
                if self.ahead == 0 && self.behind == 0 {
                    header += " · up to date";
                } else {
                    if self.ahead > 0 {
                        header += &format!(" · {} ahead", self.ahead);
                    }
                    if self.behind > 0 {
                        header += &format!(" · {} behind", self.behind);
                    }
                }
            }
            None => {
                header += " · no upstream";
            }
        }
        header
    }

    /// What's in progress and how to finish or back out — shown at the top of
    /// `status` whenever git has stopped mid-operation. `None` otherwise.
    pub fn in_progress_lines(&self) -> Option<Vec<String>> {
        use crate::copy::{in_progress_conflicts, in_progress_next};
        let (what, conflicted, family): (String, &[String], &str) = match &self.situation {
            RepoSituation::MidRebase {
                current,
                total,
                conflicted,
            } => {
                let step = if *total > 0 {
                    format!(" — stopped at commit {} of {}", current, total)
                } else {
                    String::new()
                };
                (format!("Rebase in progress{}", step), conflicted, "rebase")
            }
            RepoSituation::MidMerge { conflicts, .. } => {
                ("Merge in progress".to_string(), conflicts, "merge")
            }
            RepoSituation::MidCherryPick { conflicted } => (
                "Cherry-pick in progress".to_string(),
                conflicted,
                "cherry-pick",
            ),
            RepoSituation::MidRevert { conflicted } => {
                ("Revert in progress".to_string(), conflicted, "revert")
            }
            RepoSituation::MidBisect => {
                return Some(vec![
                    "⚠ Bisect in progress".to_string(),
                    "  Finish with `git bisect reset` before making changes.".to_string(),
                ])
            }
            _ => return None,
        };
        let mut lines = vec![format!("⚠ {}", what)];
        if !conflicted.is_empty() {
            lines.push(in_progress_conflicts(conflicted));
        }
        lines.push(in_progress_next(family, !conflicted.is_empty()));
        Some(lines)
    }

    /// Suggested next action based on current state. Returns `None` when no obvious next step.
    /// Ordering: most-actionable / safety-relevant first.
    pub fn suggestion(&self) -> Option<String> {
        // Mid-operation, the in-progress block IS the next step; anything else
        // here (commit, publish…) would be advice that git will refuse.
        if self.in_progress_lines().is_some() {
            return None;
        }
        if matches!(self.situation, RepoSituation::DetachedHead { .. }) {
            return Some(crate::copy::DETACHED_SUGGESTION.to_string());
        }
        if self.has_stash {
            return Some("Stashed changes exist. Try: gitrust \"stash pop\"".to_string());
        }
        if !self.status.modified.is_empty() || !self.status.untracked.is_empty() {
            let n = self.status.modified.len()
                + self.status.untracked.len()
                + self.status.modified_overflow
                + self.status.untracked_overflow;
            return Some(format!(
                "{} file(s) not staged. Try: gitrust \"commit all my files\"",
                n
            ));
        }
        if !self.status.staged.is_empty() {
            return Some(format!(
                "{} file(s) staged. Try: gitrust \"commit\"",
                self.status.staged.len() + self.status.staged_overflow
            ));
        }
        if self.behind > 0 && self.ahead > 0 {
            return Some(format!(
                "Diverged from upstream ({} ahead, {} behind). Pull first: gitrust \"pull\"",
                self.ahead, self.behind
            ));
        }
        if self.behind > 0 {
            return Some(format!(
                "{} commit(s) behind upstream. Try: gitrust \"pull\"",
                self.behind
            ));
        }
        if self.upstream.is_some() && self.ahead > 0 {
            return Some(format!(
                "{} unpushed commit(s). Try: gitrust \"push\"",
                self.ahead
            ));
        }
        if self.upstream.is_none() && !self.is_empty_repo {
            return Some("No upstream set. Try: gitrust \"publish\"".to_string());
        }
        // Fully clean state: surface web URL if origin is known.
        if let Some(ref url) = self.origin_url {
            if let Some(web) = crate::format::remote_to_web_url(url) {
                return Some(format!("Up to date. View: {}", web));
            }
        }
        None
    }
}

fn get_upstream_info(repo: &Repository, branch: &str) -> (Option<String>, usize, usize) {
    let upstream_ref = format!("refs/remotes/origin/{}", branch);
    let local_oid = match repo.head().ok().and_then(|h| h.target()) {
        Some(oid) => oid,
        None => return (None, 0, 0),
    };
    let upstream_oid = match repo.refname_to_id(&upstream_ref) {
        Ok(oid) => oid,
        Err(_) => return (None, 0, 0),
    };
    let (ahead, behind) = repo
        .graph_ahead_behind(local_oid, upstream_oid)
        .unwrap_or((0, 0));
    (Some(upstream_ref), ahead, behind)
}

fn build_status_summary(repo: &Repository) -> anyhow::Result<StatusSummary> {
    let statuses = repo
        .statuses(None)
        .context("Failed to read repository status")?;

    let mut conflicted = Vec::new();
    let mut staged = Vec::new();
    let mut modified = Vec::new();
    let mut untracked = Vec::new();

    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("(invalid utf8)").to_string();
        let s = entry.status();
        if s.contains(git2::Status::CONFLICTED) {
            conflicted.push(path);
            continue;
        }

        if s.intersects(
            git2::Status::INDEX_NEW
                | git2::Status::INDEX_MODIFIED
                | git2::Status::INDEX_DELETED
                | git2::Status::INDEX_RENAMED
                | git2::Status::INDEX_TYPECHANGE,
        ) {
            staged.push(path.clone());
        }
        if s.intersects(
            git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED
                | git2::Status::WT_TYPECHANGE
                | git2::Status::WT_RENAMED,
        ) {
            modified.push(path.clone());
        }
        if s.contains(git2::Status::WT_NEW) {
            untracked.push(path);
        }
    }

    let staged_overflow = staged.len().saturating_sub(MAX_FILES_PER_CATEGORY);
    let modified_overflow = modified.len().saturating_sub(MAX_FILES_PER_CATEGORY);
    let untracked_overflow = untracked.len().saturating_sub(MAX_FILES_PER_CATEGORY);

    staged.truncate(MAX_FILES_PER_CATEGORY);
    modified.truncate(MAX_FILES_PER_CATEGORY);
    untracked.truncate(MAX_FILES_PER_CATEGORY);

    Ok(StatusSummary {
        conflicted,
        staged,
        staged_overflow,
        modified,
        modified_overflow,
        untracked,
        untracked_overflow,
    })
}

impl std::fmt::Display for StatusSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.conflicted.is_empty()
            && self.staged.is_empty()
            && self.modified.is_empty()
            && self.untracked.is_empty()
        {
            return writeln!(f, "Nothing to commit, working tree clean.");
        }
        if !self.conflicted.is_empty() {
            writeln!(f, "Conflicted:")?;
            for p in &self.conflicted {
                writeln!(f, "  ! {}", p)?;
            }
        }
        if !self.staged.is_empty() {
            writeln!(f, "Staged:")?;
            for p in &self.staged {
                writeln!(f, "  + {}", p)?;
            }
            if self.staged_overflow > 0 {
                writeln!(f, "  … and {} more", self.staged_overflow)?;
            }
        }
        if !self.modified.is_empty() {
            writeln!(f, "Modified:")?;
            for p in &self.modified {
                writeln!(f, "  M {}", p)?;
            }
            if self.modified_overflow > 0 {
                writeln!(f, "  … and {} more", self.modified_overflow)?;
            }
        }
        if !self.untracked.is_empty() {
            writeln!(f, "Untracked:")?;
            for p in &self.untracked {
                writeln!(f, "  ? {}", p)?;
            }
            if self.untracked_overflow > 0 {
                writeln!(f, "  … and {} more", self.untracked_overflow)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_build_context_empty_repo() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let ctx = build_context(&repo).unwrap();
        assert!(ctx.is_empty_repo);
        assert_eq!(ctx.last_commit, "(no commits yet)");
        assert!(ctx.status.staged.is_empty());
    }

    #[test]
    fn test_build_context_with_commit() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        // Create a file and commit it
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
            .unwrap();

        let ctx = build_context(&repo).unwrap();
        assert!(!ctx.is_empty_repo);
        assert!(ctx.last_commit.contains("initial commit"));
        assert!(ctx.branch == "main" || ctx.branch == "master");
    }

    // -----------------------------------------------------------------------
    // detect_situation fixtures (deterministic, real git2 tempdirs)
    // -----------------------------------------------------------------------

    fn init_with_identity(dir: &TempDir) -> Repository {
        let repo = Repository::init(dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        repo
    }

    fn commit_file(
        repo: &Repository,
        path: &str,
        content: &str,
        msg: &str,
        update_ref: &str,
    ) -> git2::Oid {
        let wd = repo.workdir().unwrap();
        std::fs::write(wd.join(path), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some(update_ref), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    #[test]
    fn test_detect_situation_empty_and_normal() {
        let dir = TempDir::new().unwrap();
        let repo = init_with_identity(&dir);
        assert_eq!(detect_situation(&repo), RepoSituation::EmptyRepo);
        commit_file(&repo, "f.txt", "one\n", "init", "HEAD");
        assert_eq!(detect_situation(&repo), RepoSituation::Normal);
    }

    #[test]
    fn test_detect_situation_mid_merge_unresolved_then_resolved() {
        let dir = TempDir::new().unwrap();
        let repo = init_with_identity(&dir);
        let base = repo
            .find_commit(commit_file(&repo, "f.txt", "base\n", "base", "HEAD"))
            .unwrap();
        // theirs off base
        commit_file(&repo, "f.txt", "theirs\n", "theirs", "refs/heads/theirs");
        // ours off base on the current branch
        let base_obj = base.as_object();
        repo.reset(base_obj, git2::ResetType::Hard, None).unwrap();
        commit_file(&repo, "f.txt", "ours\n", "ours", "HEAD");

        let their_ac = repo
            .find_annotated_commit(repo.refname_to_id("refs/heads/theirs").unwrap())
            .unwrap();
        repo.merge(&[&their_ac], None, None).unwrap();

        match detect_situation(&repo) {
            RepoSituation::MidMerge {
                resolved,
                conflicts,
            } => {
                assert!(!resolved, "should be unresolved");
                assert!(conflicts.iter().any(|p| p == "f.txt"));
            }
            other => panic!("expected MidMerge unresolved, got {:?}", other),
        }

        // Resolve.
        std::fs::write(dir.path().join("f.txt"), "resolved\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("f.txt")).unwrap();
        index.write().unwrap();
        assert!(matches!(
            detect_situation(&repo),
            RepoSituation::MidMerge { resolved: true, .. }
        ));
    }

    #[test]
    fn test_detect_situation_mid_rebase() {
        // Divergent same-line edits, then a real `git rebase` that conflicts.
        let dir = TempDir::new().unwrap();
        let repo = init_with_identity(&dir);
        let base = repo
            .find_commit(commit_file(&repo, "f.txt", "base\n", "base", "HEAD"))
            .unwrap();
        commit_file(&repo, "f.txt", "feature\n", "feature", "refs/heads/feature");
        repo.reset(base.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        commit_file(&repo, "f.txt", "main\n", "main", "HEAD");
        let out = std::process::Command::new("git")
            .args(["rebase", "feature"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "rebase should conflict");

        let repo = Repository::open(dir.path()).unwrap();
        match detect_situation(&repo) {
            RepoSituation::MidRebase {
                total, conflicted, ..
            } => {
                assert!(total >= 1, "expected a total step count");
                assert!(conflicted.iter().any(|p| p == "f.txt"));
            }
            other => panic!("expected MidRebase, got {:?}", other),
        }
        // The status summary sees the conflict too (it used to report "clean").
        let ctx = build_context(&repo).unwrap();
        assert!(ctx.status.conflicted.iter().any(|p| p == "f.txt"));
    }

    #[test]
    fn test_status_lists_conflicted_files_and_never_says_clean() {
        let mut st = RepoContext::empty().status;
        st.conflicted = vec!["src/login.rs".into()];
        let out = st.to_string();
        assert!(
            out.contains("Conflicted:") && out.contains("src/login.rs"),
            "{out}"
        );
        assert!(!out.contains("working tree clean"), "{out}");
    }

    #[test]
    fn test_mid_rebase_status_points_at_continue_and_abort_never_publish() {
        let mut ctx = RepoContext::empty();
        ctx.is_empty_repo = false;
        ctx.is_detached = true;
        ctx.branch = "HEAD".into();
        ctx.last_commit = "e6cb6c2 \"Main change\" (just now)".into();
        ctx.situation = RepoSituation::MidRebase {
            current: 1,
            total: 2,
            conflicted: vec!["src/login.rs".into()],
        };
        let lines = ctx
            .in_progress_lines()
            .expect("mid-rebase must explain itself")
            .join("\n");
        assert!(
            lines.contains("Rebase in progress") && lines.contains("1 of 2"),
            "{lines}"
        );
        assert!(lines.contains("1 conflicted file"), "{lines}");
        assert!(
            lines.contains("gitrust \"continue rebase\"")
                && lines.contains("gitrust \"abort rebase\""),
            "{lines}"
        );
        assert!(ctx.suggestion().map_or(true, |s| !s.contains("publish")));
        assert!(!ctx.render_status_header().contains("HEAD detached at HEAD"));
        // Resolved conflicts → the next step is continue.
        ctx.situation = RepoSituation::MidRebase {
            current: 1,
            total: 2,
            conflicted: vec![],
        };
        let lines = ctx.in_progress_lines().unwrap().join("\n");
        assert!(lines.contains("continue rebase"), "{lines}");
        // A normal repo has no in-progress block.
        ctx.situation = RepoSituation::Normal;
        assert!(ctx.in_progress_lines().is_none());
    }

    #[test]
    fn test_detached_header_leads_with_not_on_a_branch() {
        let mut ctx = RepoContext::empty();
        ctx.is_empty_repo = false;
        ctx.is_detached = true;
        ctx.branch = "HEAD".into();
        ctx.last_commit = "a3f9c12 \"Release\" (2 days ago)".into();
        ctx.situation = RepoSituation::DetachedHead {
            stranded_commits: 0,
        };
        let h = ctx.render_status_header();
        assert!(
            h.starts_with("Not on a branch") && h.contains("a3f9c12"),
            "{h}"
        );
    }

    #[test]
    fn test_detect_situation_detached_with_two_stranded() {
        let dir = TempDir::new().unwrap();
        let repo = init_with_identity(&dir);
        commit_file(&repo, "f.txt", "1\n", "c1", "HEAD");
        // Detach at c1, then make 2 commits reachable from HEAD but no branch.
        let c1 = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(c1).unwrap();
        commit_file(&repo, "f.txt", "2\n", "c2", "HEAD");
        commit_file(&repo, "f.txt", "3\n", "c3", "HEAD");
        assert_eq!(
            detect_situation(&repo),
            RepoSituation::DetachedHead {
                stranded_commits: 2
            }
        );
    }

    #[test]
    fn test_detect_situation_stale_lock() {
        let dir = TempDir::new().unwrap();
        let repo = init_with_identity(&dir);
        commit_file(&repo, "f.txt", "1\n", "c1", "HEAD");
        // Create .git/index.lock and backdate its mtime far past the threshold.
        // `touch -t CCYYMMDDhhmm` is supported by both BSD (macOS) and GNU touch.
        let lock = repo.path().join("index.lock");
        std::fs::write(&lock, b"").unwrap();
        let out = std::process::Command::new("touch")
            .args(["-t", "202001010000", lock.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "touch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        match detect_situation(&repo) {
            RepoSituation::StaleLock { age_secs } => assert!(age_secs > STALE_LOCK_SECS),
            other => panic!("expected StaleLock, got {:?}", other),
        }
    }

    #[test]
    fn test_detect_situation_remote_rewritten() {
        // Tracking ref advanced non-fast-forward (its reflog's old tip is not an
        // ancestor of the new tip).
        let dir = TempDir::new().unwrap();
        let repo = init_with_identity(&dir);
        let base = repo
            .find_commit(commit_file(&repo, "f.txt", "base\n", "base", "HEAD"))
            .unwrap();
        // Two divergent commits off base: "old remote" and "new remote".
        let old_remote = commit_file(&repo, "f.txt", "old\n", "old", "refs/heads/tmp_old");
        repo.reset(base.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        let new_remote = commit_file(&repo, "g.txt", "new\n", "new", "refs/heads/tmp_new");

        // Point the tracking ref at old, then move it to new (recording reflog).
        let branch = repo.head().unwrap().shorthand().unwrap().to_string();
        let tracking = format!("refs/remotes/origin/{}", branch);
        repo.reference(&tracking, old_remote, true, "set old")
            .unwrap();
        repo.reference(&tracking, new_remote, true, "force-push rewrite")
            .unwrap();

        match detect_situation(&repo) {
            RepoSituation::RemoteRewritten { old_tip, new_tip } => {
                assert_eq!(old_tip, old_remote);
                assert_eq!(new_tip, new_remote);
            }
            other => panic!("expected RemoteRewritten, got {:?}", other),
        }
    }
}
