use anyhow::Context;
use git2::Repository;

use crate::commands::{PipelineStep, ResetMode};
use crate::context::StatusSummary;

/// Per-file change in a commit — used for `glit show` output.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

/// Predicted outcome of a `git pull` from the current branch's upstream,
/// computed in-memory without touching the working tree.
#[derive(Debug, Clone)]
pub enum MergePreview {
    /// Local and remote are at the same commit. Nothing to do.
    UpToDate,
    /// Local has commits the remote doesn't — pull would be a no-op
    /// (or `git pull` would just confirm).
    LocalAhead { local_commits: usize },
    /// Remote has commits we don't — fast-forward possible (no conflicts).
    FastForward { remote_commits: usize },
    /// Both sides advanced, but the 3-way merge is clean (no conflicts).
    CleanMerge {
        local_commits: usize,
        remote_commits: usize,
    },
    /// 3-way merge would produce conflicts in these files.
    Conflicts {
        local_commits: usize,
        remote_commits: usize,
        files: Vec<String>,
    },
    /// No upstream is configured for this branch.
    NoUpstream,
    /// Remote tracking ref missing (never fetched).
    NoRemoteTracking,
}

/// Per-file uncommitted-vs-remote overlap — local changes that the incoming
/// merge will also touch. These will block a pull even if the 3-way merge itself
/// is clean.
#[derive(Debug, Clone, Default)]
pub struct DirtyOverlap {
    pub overlapping_files: Vec<String>,
}

/// Full metadata for a single commit.
#[derive(Debug, Clone)]
pub struct CommitDetails {
    pub sha: String,
    pub full_sha: String,
    pub author: String,
    pub email: String,
    pub time_epoch: i64,
    pub message: String,
    pub parent_sha: Option<String>,
    pub file_changes: Vec<FileChange>,
}

/// Trait defining all Git operations. Enables swapping the git2 backend
/// with mocks for testing or alternative implementations.
pub trait GitOperations {
    fn add(&self, repo: &Repository, files: &[String]) -> anyhow::Result<()>;
    fn add_all(&self, repo: &Repository) -> anyhow::Result<()>;
    fn commit(&self, repo: &Repository, message: &str) -> anyhow::Result<git2::Oid>;
    /// Commit via system `git commit -m` so configured hooks actually run (S47 —
    /// libgit2 never executes hooks). On failure returns a `ClassifiedFailure`
    /// (e.g. `HookRejected`) so the executor can offer class-aware recovery.
    fn commit_system(&self, repo: &Repository, message: &str) -> anyhow::Result<git2::Oid>;
    fn push(
        &self,
        repo: &Repository,
        remote: &str,
        branch: &str,
        set_upstream: bool,
    ) -> anyhow::Result<()>;
    fn pull(&self, repo: &Repository, remote: &str) -> anyhow::Result<()>;
    fn create_branch(&self, repo: &Repository, name: &str) -> anyhow::Result<()>;
    fn checkout(&self, repo: &Repository, target: &str) -> anyhow::Result<()>;
    fn stash(&self, repo: &mut Repository) -> anyhow::Result<git2::Oid>;
    fn stash_pop(&self, repo: &mut Repository) -> anyhow::Result<()>;
    fn reset(&self, repo: &Repository, mode: ResetMode) -> anyhow::Result<()>;
    fn log(&self, repo: &Repository, count: u8) -> anyhow::Result<Vec<String>>;
    fn get_status_summary(&self, repo: &Repository) -> anyhow::Result<StatusSummary>;
    fn get_diff_snippet(
        &self,
        repo: &Repository,
        path: &str,
        max_lines: usize,
    ) -> anyhow::Result<String>;
    fn list_remotes(&self, repo: &Repository) -> anyhow::Result<Vec<(String, String)>>;
    fn list_branches(&self, repo: &Repository) -> anyhow::Result<Vec<String>>;
    fn get_user_config(&self, repo: &Repository) -> anyhow::Result<(String, String)>;
    fn add_remote(&self, repo: &Repository, name: &str, url: &str) -> anyhow::Result<()>;
    fn remove_remote(&self, repo: &Repository, name: &str) -> anyhow::Result<()>;
    fn get_commit_details(&self, repo: &Repository, target: &str) -> anyhow::Result<CommitDetails>;
    fn undo_last_commit(&self, repo: &Repository, parent_oid: git2::Oid) -> anyhow::Result<()>;
    /// Update remote-tracking refs without merging into the working tree.
    /// Network operation — uses the same credential callbacks as push.
    fn fetch_remote(&self, repo: &Repository, remote_name: &str) -> anyhow::Result<()>;
    /// Predict the outcome of pulling the current branch from `remote_name`.
    /// Does NOT modify the working tree or index — pure in-memory analysis.
    /// Caller is expected to have already fetched.
    fn predict_pull(&self, repo: &Repository, remote_name: &str) -> anyhow::Result<MergePreview>;
    /// Files that the user has modified locally AND the incoming merge will
    /// also touch — these block a pull even when the 3-way merge is clean.
    fn dirty_overlap_for_pull(
        &self,
        repo: &Repository,
        remote_name: &str,
    ) -> anyhow::Result<DirtyOverlap>;
    /// `git pull --rebase` — shells out to git for full conflict-resolution UX.
    fn pull_rebase(&self, repo: &Repository, remote: &str, branch: &str) -> anyhow::Result<()>;
    /// `git push --force-with-lease` — safe force-push.
    fn force_push_with_lease(
        &self,
        repo: &Repository,
        remote: &str,
        branch: &str,
    ) -> anyhow::Result<()>;
    /// `git fetch <remote> && git reset --hard <remote>/<branch>`.
    fn discard_local_to_remote(
        &self,
        repo: &Repository,
        remote: &str,
        branch: &str,
    ) -> anyhow::Result<()>;

    // Divergence reconciliation (Phase 12 — S9/S13).
    /// `git fetch [--prune] <remote>` — refresh remote-tracking refs before the
    /// reconcile ChoiceGate. Run inline (not a pipeline step).
    fn fetch(&self, repo: &Repository, remote: &str, prune: bool) -> anyhow::Result<()>;
    /// git2 `merge <source>` (e.g. `origin/main`). On a clean 3-way merge it
    /// lands the merge commit (2 parents) via the merge-aware `commit(message)`;
    /// a fast-forward moves HEAD with no merge commit; on conflict it leaves the
    /// repo in `MidMerge` and returns `Err` (Phase 11 owns continue/abort).
    fn merge(&self, repo: &Repository, source: &str, message: &str) -> anyhow::Result<()>;
    /// `git rebase <onto>` (shell-out). On conflict exits into `MidRebase`.
    fn rebase(&self, repo: &Repository, onto: &str) -> anyhow::Result<()>;

    // In-progress operation backout/continue steps (Phase 11 — S16/S18/S19).
    // All shell out to system git (the blueprint's blessed convention for
    // gnarly ops); git2's rebase API can replace them later behind this trait.
    fn merge_abort(&self, repo: &Repository) -> anyhow::Result<()>;
    fn rebase_continue(&self, repo: &Repository) -> anyhow::Result<()>;
    fn rebase_skip(&self, repo: &Repository) -> anyhow::Result<()>;
    fn rebase_abort(&self, repo: &Repository) -> anyhow::Result<()>;
    fn cherry_pick_continue(&self, repo: &Repository) -> anyhow::Result<()>;
    fn cherry_pick_abort(&self, repo: &Repository) -> anyhow::Result<()>;
    fn revert_continue(&self, repo: &Repository) -> anyhow::Result<()>;
    fn revert_abort(&self, repo: &Repository) -> anyhow::Result<()>;
}

/// Open a repository by discovering it from the given path.
pub fn open_repo(path: &std::path::Path) -> anyhow::Result<Repository> {
    Repository::discover(path).context("Not a git repository. Run: git init")
}

/// Resolve a remote's default branch via `refs/remotes/<remote>/HEAD` (the
/// symbolic ref `git remote set-head` maintains). Falls back to `main` then
/// `master` if the symbolic ref is absent (S13 — cross-branch Sync target).
pub fn default_branch(repo: &Repository, remote: &str) -> Option<String> {
    let sym = format!("refs/remotes/{}/HEAD", remote);
    if let Ok(reference) = repo.find_reference(&sym) {
        if let Some(target) = reference.symbolic_target() {
            // e.g. "refs/remotes/origin/main" → "main"
            let prefix = format!("refs/remotes/{}/", remote);
            if let Some(name) = target.strip_prefix(&prefix) {
                return Some(name.to_string());
            }
        }
    }
    for fallback in ["main", "master"] {
        let r = format!("refs/remotes/{}/{}", remote, fallback);
        if repo.find_reference(&r).is_ok() {
            return Some(fallback.to_string());
        }
    }
    None
}

/// Run `git <args>` with the repository's workdir as cwd. Inherits stdio so
/// the user sees git's normal output (including conflict markers, auth prompts).
fn run_git_in_repo(repo: &Repository, args: &[&str]) -> anyhow::Result<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Repository has no workdir (bare repo?)"))?;
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(workdir)
        .status()
        .with_context(|| format!("Failed to run: git {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("git {} exited with status {}", args.join(" "), status);
    }
    Ok(())
}

/// Like `run_git_in_repo` but forces a non-interactive editor. `--continue`
/// ops (rebase/cherry-pick/revert) reuse the prepared commit message and would
/// otherwise block on `$EDITOR` — this makes them accept it unattended (and
/// keeps tests / non-TTY runs from hanging).
fn run_git_noninteractive(repo: &Repository, args: &[&str]) -> anyhow::Result<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Repository has no workdir (bare repo?)"))?;
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(workdir)
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .status()
        .with_context(|| format!("Failed to run: git {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("git {} exited with status {}", args.join(" "), status);
    }
    Ok(())
}

/// Run `git <args>` capturing stdout + stderr. Used for network operations
/// where we need to classify failures (auth, dirty tree, network) into a
/// `FailureClass` for human-readable error reporting.
fn run_git_capture(repo: &Repository, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Repository has no workdir (bare repo?)"))?;
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .with_context(|| format!("Failed to spawn: git {}", args.join(" ")))?;
    Ok(output)
}

/// Coarse classification of network/merge failures from system `git` stderr.
/// Used by the executor to render a human-readable error block instead of
/// "exited with status 1" or libgit2-internal jargon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// `git pull` / `merge` refused because uncommitted edits overlap with
    /// files the remote touches. User must commit/stash before retrying.
    DirtyTreeBlocksMerge { files: Vec<String> },
    /// Push rejected because remote has commits we don't have. Routes the
    /// user into the merge-preview conflict picker.
    NonFastForward,
    /// SSH / HTTPS credentials failed.
    AuthFailed,
    /// DNS, connection refused, timeout, host unreachable.
    Network,
    /// Hook (pre-commit, pre-push, etc.) refused the operation. `output` carries
    /// the captured hook/git stderr so the failure message can show the cause.
    HookRejected {
        hook: Option<String>,
        output: String,
    },
    /// `.git/index.lock` is held by another process.
    LockHeld,
    /// Fall-through: original stderr text preserved verbatim.
    Unknown(String),
}

impl FailureClass {
    /// One-line plain-English description for the error block header.
    pub fn what(&self) -> String {
        match self {
            FailureClass::DirtyTreeBlocksMerge { .. } => {
                "uncommitted edits would be overwritten by the merge".into()
            }
            FailureClass::NonFastForward => {
                "push rejected — remote has commits you don't have".into()
            }
            FailureClass::AuthFailed => "couldn't authenticate to the remote".into(),
            FailureClass::Network => "couldn't reach the remote".into(),
            FailureClass::HookRejected { hook, output } => {
                let who = match hook {
                    Some(h) => format!("the {} hook refused the operation", h),
                    None => "a git hook refused the operation".to_string(),
                };
                let out = output.trim();
                if out.is_empty() {
                    who
                } else {
                    format!("{}:\n{}", who, out)
                }
            }
            FailureClass::LockHeld => {
                ".git/index.lock is held — another git process is running".into()
            }
            FailureClass::Unknown(_) => "command failed".into(),
        }
    }

    /// One actionable next step the user can take.
    pub fn fix(&self) -> String {
        match self {
            FailureClass::DirtyTreeBlocksMerge { files } if files.len() == 1 => {
                format!("commit or stash your edits to {} first", files[0])
            }
            FailureClass::DirtyTreeBlocksMerge { files } => format!(
                "commit or stash your edits to {} file(s) first",
                files.len()
            ),
            FailureClass::NonFastForward => "glit \"pull\" to reconcile, then push again".into(),
            FailureClass::AuthFailed => {
                "verify with: git -C . ls-remote (this uses your system git's credentials)".into()
            }
            FailureClass::Network => "check your connection, then retry".into(),
            FailureClass::HookRejected { .. } => {
                "see hook output above; fix the cause and retry".into()
            }
            FailureClass::LockHeld => {
                "wait for the other git process or delete .git/index.lock if stale".into()
            }
            FailureClass::Unknown(_) => "see the error above for context".into(),
        }
    }
}

/// Typed error wrapping a `FailureClass` so the executor can downcast and
/// render a class-aware recovery picker instead of dumping the anyhow chain.
#[derive(Debug)]
pub struct ClassifiedFailure {
    pub class: FailureClass,
    pub raw_stderr: String,
}

impl std::fmt::Display for ClassifiedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Default rendering — used when the caller does NOT recognize and
        // unwrap us. Keep it terse: one line. The recovery picker handles
        // the full UI when we are recognized.
        write!(f, "{}", self.class.what())
    }
}

impl std::error::Error for ClassifiedFailure {}

/// Render a `FailureClass` as a multi-line error block. Raw stderr is
/// preserved at the bottom so power users can see exactly what git said
/// (this is what `anyhow`'s `caused by` chain will show).
pub fn format_failure_class(class: &FailureClass, raw_stderr: &str) -> String {
    let mut out = String::new();
    out.push_str(&class.what());
    out.push_str(&format!("\n  → {}", class.fix()));
    if let FailureClass::DirtyTreeBlocksMerge { files } = class {
        if !files.is_empty() {
            out.push_str("\n  Files:");
            for f in files {
                out.push_str(&format!("\n    · {}", f));
            }
        }
    }
    // Always include the raw stderr for debuggability, but as a one-line tail.
    let stderr_trimmed = raw_stderr.trim();
    if !stderr_trimmed.is_empty() {
        let one_line = stderr_trimmed.replace('\n', " · ");
        out.push_str(&format!("\n  (git: {})", one_line));
    }
    out
}

/// Parse system `git` stderr into a `FailureClass`. Heuristic — string matches
/// the well-known phrasing git uses across versions. Falls through to
/// `Unknown` with the original text preserved.
pub fn classify_git_stderr(stderr: &str) -> FailureClass {
    let lower = stderr.to_lowercase();

    // Dirty-tree-blocks-merge: extract the listed files.
    if lower.contains("your local changes to the following files would be overwritten")
        || lower.contains("would be overwritten by merge")
        || lower.contains("would be overwritten by checkout")
    {
        let mut files = Vec::new();
        let mut in_list = false;
        for line in stderr.lines() {
            if line.to_lowercase().contains("would be overwritten") {
                in_list = true;
                continue;
            }
            if in_list {
                let trimmed = line.trim();
                if trimmed.is_empty()
                    || trimmed.starts_with("Please")
                    || trimmed.starts_with("error:")
                    || trimmed.starts_with("Aborting")
                {
                    break;
                }
                files.push(trimmed.to_string());
            }
        }
        return FailureClass::DirtyTreeBlocksMerge { files };
    }

    if lower.contains("non-fast-forward")
        || (lower.contains("rejected") && lower.contains("fetch first"))
        || (lower.contains("rejected") && lower.contains("non-fast-forward"))
    {
        return FailureClass::NonFastForward;
    }

    // Network checks come BEFORE auth — "could not read from remote
    // repository" appears in both auth- and network-failure messages, so
    // we rely on the more specific signals: DNS lookup, connection refused,
    // unreachable, or explicit timeouts.
    if lower.contains("could not resolve host")
        || lower.contains("could not resolve hostname")
        || lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("network is unreachable")
        || lower.contains("operation timed out")
    {
        return FailureClass::Network;
    }

    if lower.contains("permission denied (publickey)")
        || lower.contains("permission denied (password)")
        || lower.contains("authentication failed")
        || lower.contains("invalid username or password")
        || lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("401 unauthorized")
    {
        return FailureClass::AuthFailed;
    }

    if lower.contains("hook declined") || lower.contains("pre-commit hook") {
        let hook = if lower.contains("pre-push") {
            Some("pre-push".to_string())
        } else if lower.contains("pre-commit") {
            Some("pre-commit".to_string())
        } else if lower.contains("commit-msg") {
            Some("commit-msg".to_string())
        } else {
            None
        };
        return FailureClass::HookRejected {
            hook,
            output: stderr.trim().to_string(),
        };
    }

    if lower.contains("unable to create '") && lower.contains("index.lock") {
        return FailureClass::LockHeld;
    }

    FailureClass::Unknown(stderr.trim().to_string())
}

/// Read the oid(s) recorded in `.git/MERGE_HEAD` — one per line, with multiple
/// lines for an octopus merge. Returns an empty vec when no merge is in
/// progress. libgit2 exposes no direct getter, so we read the pseudo-ref file.
fn read_merge_heads(repo: &Repository) -> anyhow::Result<Vec<git2::Oid>> {
    let path = repo.path().join("MERGE_HEAD");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::Error::new(e).context("Failed to read MERGE_HEAD")),
    };
    let mut oids = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let oid = git2::Oid::from_str(line)
            .with_context(|| format!("Invalid oid in MERGE_HEAD: {}", line))?;
        oids.push(oid);
    }
    Ok(oids)
}

/// The operation we're deciding hook routing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOp {
    Commit,
    Push,
}

/// Hook filenames relevant to an operation.
fn relevant_hook_names(op: HookOp) -> &'static [&'static str] {
    match op {
        HookOp::Commit => &["pre-commit", "commit-msg", "prepare-commit-msg"],
        HookOp::Push => &["pre-push"],
    }
}

/// Resolve the hooks directory: `core.hooksPath` when set (relative paths are
/// resolved against the working dir, per git), else `<gitdir>/hooks`.
fn hooks_dir(repo: &Repository) -> std::path::PathBuf {
    if let Ok(cfg) = repo.config() {
        if let Ok(p) = cfg.get_path("core.hooksPath") {
            if p.is_absolute() {
                return p;
            }
            if let Some(wd) = repo.workdir() {
                return wd.join(p);
            }
            return p;
        }
    }
    repo.path().join("hooks")
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(p: &std::path::Path) -> bool {
    // Non-unix: git treats a present hook file as runnable.
    p.is_file()
}

/// Executable hooks present for `op`. Empty when none apply.
pub fn hooks_for_op(repo: &Repository, op: HookOp) -> Vec<String> {
    let dir = hooks_dir(repo);
    relevant_hook_names(op)
        .iter()
        .filter(|name| is_executable_file(&dir.join(name)))
        .map(|name| (*name).to_string())
        .collect()
}

/// Decide whether to route an operation through system `git` (so hooks run)
/// instead of git2: "system" always; "git2" never; "auto" (and any unknown
/// value) only when relevant hooks are present.
pub fn should_route_to_system_git(hooks_mode: &str, hooks_present: bool) -> bool {
    match hooks_mode {
        "system" => true,
        "git2" => false,
        _ => hooks_present,
    }
}

/// Default git2-backed implementation.
pub struct Git2Operations;

impl GitOperations for Git2Operations {
    fn add(&self, repo: &Repository, files: &[String]) -> anyhow::Result<()> {
        let mut index = repo.index().context("Failed to read index")?;
        for file in files {
            // Reject path traversal
            if file.contains("..") || std::path::Path::new(file).is_absolute() {
                anyhow::bail!(
                    "Invalid file path: {}. Paths must be relative and within the repo.",
                    file
                );
            }
            index
                .add_path(std::path::Path::new(file))
                .with_context(|| format!("Failed to stage file: {}", file))?;
        }
        index.write().context("Failed to write index")?;
        Ok(())
    }

    fn add_all(&self, repo: &Repository) -> anyhow::Result<()> {
        let mut index = repo.index().context("Failed to read index")?;
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .context("Failed to stage all files")?;
        index.write().context("Failed to write index")?;
        Ok(())
    }

    fn commit(&self, repo: &Repository, message: &str) -> anyhow::Result<git2::Oid> {
        if repo.head_detached().unwrap_or(false) {
            anyhow::bail!("HEAD is detached. Run: git checkout <branch-name>");
        }
        if message.trim().is_empty() {
            anyhow::bail!("Commit message cannot be empty");
        }

        let sig = repo.signature().context(
            "Git user not configured. Run: git config user.name && git config user.email",
        )?;
        let mut index = repo.index().context("Failed to read index")?;
        let tree_oid = index
            .write_tree()
            .context("Failed to write tree from index")?;
        let tree = repo
            .find_tree(tree_oid)
            .context("Failed to find tree object")?;

        // Handle empty repo (no HEAD = first commit, no parents)
        let mut parents: Vec<git2::Commit> = match repo.head() {
            Ok(head) => vec![head
                .peel_to_commit()
                .context("Failed to resolve HEAD to commit")?],
            Err(_) => vec![], // First commit in empty repo
        };

        // Merge-aware commit (S17): while MERGE_HEAD exists, a correct commit
        // records it as an additional parent. Without this the merge relationship
        // is silently destroyed (single-parent commit) and MERGE_HEAD lingers.
        let merging = repo.state() == git2::RepositoryState::Merge;
        if merging {
            for oid in read_merge_heads(repo)? {
                let c = repo
                    .find_commit(oid)
                    .with_context(|| format!("MERGE_HEAD commit {} not found", oid))?;
                parents.push(c);
            }
        }
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .context("Commit failed")?;

        // Return to a clean state: clear MERGE_HEAD / MERGE_MSG after the merge
        // commit lands, so the repo no longer reports `RepositoryState::Merge`.
        if merging {
            repo.cleanup_state()
                .context("Failed to clean up merge state after commit")?;
        }
        Ok(oid)
    }

    fn commit_system(&self, repo: &Repository, message: &str) -> anyhow::Result<git2::Oid> {
        if message.trim().is_empty() {
            anyhow::bail!("Commit message cannot be empty");
        }
        // Shell out so pre-commit / commit-msg / prepare-commit-msg hooks run.
        let output = run_git_capture(repo, &["commit", "-m", message])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let combined = format!("{}\n{}", stdout, stderr);
            let mut class = classify_git_stderr(&combined);
            // git prints no standard phrase when a pre-commit/commit-msg hook
            // fails, so a generic Unknown with commit hooks present is almost
            // certainly a hook rejection — surface it as such (but never for a
            // genuine "nothing to commit").
            if matches!(class, FailureClass::Unknown(_))
                && !combined.to_lowercase().contains("nothing to commit")
            {
                let present = hooks_for_op(repo, HookOp::Commit);
                if !present.is_empty() {
                    class = FailureClass::HookRejected {
                        hook: present.first().cloned(),
                        output: stderr.trim().to_string(),
                    };
                }
            }
            return Err(anyhow::Error::new(ClassifiedFailure {
                class,
                raw_stderr: stderr,
            }));
        }
        // Resolve the commit git just created.
        repo.head()
            .ok()
            .and_then(|h| h.target())
            .context("commit succeeded but HEAD could not be resolved")
    }

    fn push(
        &self,
        repo: &Repository,
        remote_name: &str,
        branch: &str,
        set_upstream: bool,
    ) -> anyhow::Result<()> {
        // Shell out to system git so the user's credential helper, ssh-agent,
        // and ~/.ssh/config all apply. libgit2's credential layer needs an
        // explicit callback per request and silently fails when no key matches.
        let mut args: Vec<&str> = vec!["push"];
        if set_upstream {
            args.push("-u");
        }
        args.push(remote_name);
        args.push(branch);
        let output = run_git_capture(repo, &args)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let combined = format!("{}\n{}", stdout, stderr);
        let mut class = classify_git_stderr(&combined);

        // Distinguish a LOCAL pre-push hook rejection from a SERVER-side one.
        // Server output carries a "remote:" line and a "! [rejected]" /
        // "! [remote rejected]" marker — and git labels a server pre-receive
        // failure "... hook declined", which the base classifier reads as
        // HookRejected. That is server policy, NOT the local pre-push hook: a
        // dedicated ServerPolicy class is Tier 2 (S43), so keep it Unknown for
        // now rather than blaming the local hook.
        let has_remote_markers = {
            let l = combined.to_lowercase();
            l.contains("remote:") || l.contains("! [rejected]") || l.contains("! [remote rejected]")
        };
        if has_remote_markers {
            if matches!(class, FailureClass::HookRejected { .. }) {
                class = FailureClass::Unknown(stderr.trim().to_string());
            }
        } else if matches!(class, FailureClass::Unknown(_)) {
            // No remote involvement + an executable pre-push hook present ⇒ the
            // local pre-push hook rejected. git prints no standard phrase for
            // that, so upgrade Unknown → HookRejected and carry the hook output
            // (mirrors the commit path, S47/S46).
            let hooks = hooks_for_op(repo, HookOp::Push);
            if !hooks.is_empty() {
                class = FailureClass::HookRejected {
                    hook: hooks.into_iter().next(),
                    output: stderr.trim().to_string(),
                };
            }
        }
        Err(anyhow::Error::new(ClassifiedFailure {
            class,
            raw_stderr: stderr,
        }))
    }

    fn pull(&self, repo: &Repository, remote_name: &str) -> anyhow::Result<()> {
        let branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
            .context("Cannot determine current branch for pull")?;
        let output = run_git_capture(repo, &["pull", remote_name, &branch])?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let class = classify_git_stderr(&format!("{}\n{}", stdout, stderr));
        Err(anyhow::Error::new(ClassifiedFailure {
            class,
            raw_stderr: stderr,
        }))
    }

    fn create_branch(&self, repo: &Repository, name: &str) -> anyhow::Result<()> {
        let head = repo.head().context("No HEAD — is this an empty repo?")?;
        let commit = head
            .peel_to_commit()
            .context("HEAD does not point to a commit")?;
        repo.branch(name, &commit, false).with_context(|| {
            format!("Failed to create branch '{}'. Does it already exist?", name)
        })?;
        Ok(())
    }

    fn checkout(&self, repo: &Repository, target: &str) -> anyhow::Result<()> {
        let (object, reference) = repo
            .revparse_ext(target)
            .map_err(|_| {
                let available: Vec<String> = self
                    .list_branches(repo)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|b| b.trim_start_matches(['*', ' ']).to_string())
                    .collect();
                if available.is_empty() {
                    anyhow::anyhow!("'{}' is not a valid branch or commit", target)
                } else {
                    anyhow::anyhow!(
                        "Branch '{}' not found.\n  Available branches: {}\n  Create it with: glit \"create branch {}\"",
                        target,
                        available.join(", "),
                        target
                    )
                }
            })?;

        repo.checkout_tree(&object, None)
            .with_context(|| format!("Failed to checkout '{}'", target))?;

        match reference {
            Some(r) => {
                repo.set_head(r.name().unwrap_or(""))
                    .context("Failed to set HEAD")?;
            }
            None => {
                // Detached HEAD
                repo.set_head_detached(object.id())
                    .context("Failed to detach HEAD")?;
            }
        }
        Ok(())
    }

    fn stash(&self, repo: &mut Repository) -> anyhow::Result<git2::Oid> {
        let sig = repo.signature().context("Git user not configured")?;
        let oid = repo
            .stash_save(&sig, "GLit auto-stash", None)
            .context("Nothing to stash — working tree is clean")?;
        Ok(oid)
    }

    fn stash_pop(&self, repo: &mut Repository) -> anyhow::Result<()> {
        repo.stash_pop(0, None).context("No stash entries to pop")?;
        Ok(())
    }

    fn reset(&self, repo: &Repository, mode: ResetMode) -> anyhow::Result<()> {
        let head = repo.head().context("No HEAD to reset to")?;
        let commit = head
            .peel_to_commit()
            .context("HEAD does not point to a commit")?;
        let object = commit.as_object();

        let reset_type = match mode {
            ResetMode::Soft => git2::ResetType::Soft,
            ResetMode::Mixed => git2::ResetType::Mixed,
            ResetMode::Hard => git2::ResetType::Hard,
        };

        repo.reset(object, reset_type, None)
            .with_context(|| format!("Reset ({:?}) failed", mode))?;
        Ok(())
    }

    fn log(&self, repo: &Repository, count: u8) -> anyhow::Result<Vec<String>> {
        let mut revwalk = repo.revwalk().context("Failed to create revwalk")?;
        revwalk.push_head().context("No commits in repository")?;
        revwalk
            .set_sorting(git2::Sort::TIME)
            .context("Failed to set sort order")?;

        let mut entries = Vec::new();
        for (i, oid_result) in revwalk.enumerate() {
            if i >= count as usize {
                break;
            }
            let oid = oid_result.context("Failed to read commit OID")?;
            let commit = repo
                .find_commit(oid)
                .context("Failed to find commit object")?;
            let short_id = &commit.id().to_string()[..7];
            let summary = commit.summary().unwrap_or("(no message)");
            let relative = crate::format::format_relative(commit.time().seconds());
            entries.push(format!("{}  ({})  {}", short_id, relative, summary));
        }
        Ok(entries)
    }

    fn list_remotes(&self, repo: &Repository) -> anyhow::Result<Vec<(String, String)>> {
        let names = repo.remotes().context("Failed to list remotes")?;
        let mut remotes = Vec::new();
        for name in names.iter().flatten() {
            let remote = repo.find_remote(name).ok();
            let url = remote
                .and_then(|r| r.url().map(String::from))
                .unwrap_or_else(|| "(no url)".to_string());
            remotes.push((name.to_string(), url));
        }
        Ok(remotes)
    }

    fn list_branches(&self, repo: &Repository) -> anyhow::Result<Vec<String>> {
        let mut branches = Vec::new();
        let current = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from));
        for branch_result in repo.branches(None).context("Failed to list branches")? {
            let (branch, _) = branch_result.context("Failed to read branch")?;
            if let Some(name) = branch.name()? {
                let marker = if current.as_deref() == Some(name) {
                    "* "
                } else {
                    "  "
                };
                branches.push(format!("{}{}", marker, name));
            }
        }
        Ok(branches)
    }

    fn get_user_config(&self, repo: &Repository) -> anyhow::Result<(String, String)> {
        let config = repo.config().context("Failed to read git config")?;
        let name = config
            .get_string("user.name")
            .unwrap_or_else(|_| "(not set)".to_string());
        let email = config
            .get_string("user.email")
            .unwrap_or_else(|_| "(not set)".to_string());
        Ok((name, email))
    }

    fn add_remote(&self, repo: &Repository, name: &str, url: &str) -> anyhow::Result<()> {
        repo.remote(name, url).with_context(|| {
            format!(
                "Failed to add remote '{}' ({}). Does it already exist?",
                name, url
            )
        })?;
        Ok(())
    }

    fn remove_remote(&self, repo: &Repository, name: &str) -> anyhow::Result<()> {
        repo.remote_delete(name)
            .with_context(|| format!("Failed to remove remote '{}'. Does it exist?", name))?;
        Ok(())
    }

    fn get_commit_details(&self, repo: &Repository, target: &str) -> anyhow::Result<CommitDetails> {
        let target = if target.is_empty() { "HEAD" } else { target };
        let obj = repo
            .revparse_single(target)
            .with_context(|| format!("'{}' is not a valid revision", target))?;
        let commit = obj
            .peel_to_commit()
            .with_context(|| format!("'{}' does not resolve to a commit", target))?;

        let full_sha = commit.id().to_string();
        let sha = full_sha[..7.min(full_sha.len())].to_string();
        let author_sig = commit.author();
        let author = author_sig.name().unwrap_or("(unknown)").to_string();
        let email = author_sig.email().unwrap_or("").to_string();
        let time_epoch = commit.time().seconds();
        let message = commit.message().unwrap_or("").to_string();

        let parent = commit.parent(0).ok();
        let parent_sha = parent.as_ref().map(|p| p.id().to_string());

        // Per-file diff stats (only meaningful when a parent exists).
        let mut file_changes: Vec<FileChange> = Vec::new();
        if let Some(parent_commit) = parent.as_ref() {
            let new_tree = commit.tree()?;
            let old_tree = parent_commit.tree()?;
            let diff = repo
                .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
                .context("Failed to diff against parent")?;

            // Walk per-file via the patch API.
            for i in 0..diff.deltas().count() {
                let path = diff
                    .get_delta(i)
                    .and_then(|d| d.new_file().path().or_else(|| d.old_file().path()))
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "(unknown)".to_string());

                let patch = git2::Patch::from_diff(&diff, i).ok().flatten();
                let (additions, deletions) = match patch {
                    Some(p) => {
                        let (_ctx, adds, dels) = p.line_stats().unwrap_or((0, 0, 0));
                        (adds, dels)
                    }
                    None => (0, 0),
                };

                file_changes.push(FileChange {
                    path,
                    additions,
                    deletions,
                });
            }
        }

        Ok(CommitDetails {
            sha,
            full_sha,
            author,
            email,
            time_epoch,
            message,
            parent_sha,
            file_changes,
        })
    }

    fn undo_last_commit(&self, repo: &Repository, parent_oid: git2::Oid) -> anyhow::Result<()> {
        let parent_obj = repo
            .find_object(parent_oid, None)
            .context("Failed to locate parent commit for undo")?;
        repo.reset(&parent_obj, git2::ResetType::Soft, None)
            .context("Failed to soft-reset HEAD to parent")?;
        Ok(())
    }

    fn fetch_remote(&self, repo: &Repository, remote_name: &str) -> anyhow::Result<()> {
        // Shell out to system git — same rationale as `push`/`pull`. Inherits
        // the user's credential helper, ssh-agent, and ~/.ssh/config.
        let output = run_git_capture(repo, &["fetch", remote_name])?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let class = classify_git_stderr(&format!("{}\n{}", stdout, stderr));
        Err(anyhow::Error::new(ClassifiedFailure {
            class,
            raw_stderr: stderr,
        }))
    }

    fn predict_pull(&self, repo: &Repository, remote_name: &str) -> anyhow::Result<MergePreview> {
        // Resolve current branch
        let head = match repo.head() {
            Ok(h) => h,
            Err(_) => return Ok(MergePreview::NoRemoteTracking),
        };
        let branch = match head.shorthand() {
            Some(b) => b.to_string(),
            None => return Ok(MergePreview::NoRemoteTracking),
        };

        // Local commit
        let local_commit = head.peel_to_commit().context("HEAD has no commit")?;

        // Find the remote-tracking ref. If we can't find it, the upstream isn't set up.
        let remote_ref_name = format!("refs/remotes/{}/{}", remote_name, branch);
        let remote_ref = match repo.find_reference(&remote_ref_name) {
            Ok(r) => r,
            Err(_) => {
                // Check if upstream is configured at all — if not, that's the real story.
                let local_branch = repo.find_branch(&branch, git2::BranchType::Local).ok();
                if let Some(b) = local_branch {
                    if b.upstream().is_err() {
                        return Ok(MergePreview::NoUpstream);
                    }
                }
                return Ok(MergePreview::NoRemoteTracking);
            }
        };
        let remote_commit = remote_ref
            .peel_to_commit()
            .context("Remote-tracking ref doesn't point to a commit")?;

        let local_oid = local_commit.id();
        let remote_oid = remote_commit.id();

        let (ahead, behind) = repo
            .graph_ahead_behind(local_oid, remote_oid)
            .unwrap_or((0, 0));

        if local_oid == remote_oid {
            return Ok(MergePreview::UpToDate);
        }
        if ahead > 0 && behind == 0 {
            return Ok(MergePreview::LocalAhead {
                local_commits: ahead,
            });
        }
        if behind > 0 && ahead == 0 {
            return Ok(MergePreview::FastForward {
                remote_commits: behind,
            });
        }

        // Both sides have moved — run a 3-way merge in memory.
        let merged_index = repo
            .merge_commits(&local_commit, &remote_commit, None)
            .context("Failed to simulate merge in memory")?;

        if !merged_index.has_conflicts() {
            return Ok(MergePreview::CleanMerge {
                local_commits: ahead,
                remote_commits: behind,
            });
        }

        // Collect conflicting paths (dedup since each conflict has up to 3 entries).
        let mut conflict_paths: Vec<String> = Vec::new();
        if let Ok(conflicts) = merged_index.conflicts() {
            for c in conflicts.flatten() {
                let path = c
                    .our
                    .as_ref()
                    .or(c.their.as_ref())
                    .or(c.ancestor.as_ref())
                    .and_then(|e| std::str::from_utf8(&e.path).ok())
                    .map(String::from);
                if let Some(p) = path {
                    if !conflict_paths.contains(&p) {
                        conflict_paths.push(p);
                    }
                }
            }
        }

        Ok(MergePreview::Conflicts {
            local_commits: ahead,
            remote_commits: behind,
            files: conflict_paths,
        })
    }

    fn dirty_overlap_for_pull(
        &self,
        repo: &Repository,
        remote_name: &str,
    ) -> anyhow::Result<DirtyOverlap> {
        // Locally-changed file set (staged + modified + untracked-but-tracked? — exclude untracked).
        let mut local_changes: Vec<String> = Vec::new();
        let statuses = repo
            .statuses(None)
            .context("Failed to read status for dirty-overlap check")?;
        for entry in statuses.iter() {
            let s = entry.status();
            let touches_workdir_or_index = s.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE
                    | git2::Status::WT_MODIFIED
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_TYPECHANGE
                    | git2::Status::WT_RENAMED,
            );
            if touches_workdir_or_index {
                if let Some(p) = entry.path() {
                    local_changes.push(p.to_string());
                }
            }
        }
        if local_changes.is_empty() {
            return Ok(DirtyOverlap::default());
        }

        // Remote-changed file set: diff local HEAD tree against remote-tracking tree.
        let head_commit = match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
            Some(c) => c,
            None => return Ok(DirtyOverlap::default()),
        };
        let branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
            .unwrap_or_default();
        let remote_ref_name = format!("refs/remotes/{}/{}", remote_name, branch);
        let remote_commit = match repo
            .find_reference(&remote_ref_name)
            .ok()
            .and_then(|r| r.peel_to_commit().ok())
        {
            Some(c) => c,
            None => return Ok(DirtyOverlap::default()),
        };

        let local_tree = head_commit.tree()?;
        let remote_tree = remote_commit.tree()?;
        let diff = repo
            .diff_tree_to_tree(Some(&local_tree), Some(&remote_tree), None)
            .context("Failed to diff local HEAD vs remote tree")?;

        let mut remote_touched: Vec<String> = Vec::new();
        for d in diff.deltas() {
            if let Some(p) = d.new_file().path().or_else(|| d.old_file().path()) {
                remote_touched.push(p.to_string_lossy().into_owned());
            }
        }

        let overlap: Vec<String> = local_changes
            .into_iter()
            .filter(|p| remote_touched.iter().any(|r| r == p))
            .collect();
        Ok(DirtyOverlap {
            overlapping_files: overlap,
        })
    }

    fn pull_rebase(&self, repo: &Repository, remote: &str, branch: &str) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["pull", "--rebase", remote, branch])
    }

    fn force_push_with_lease(
        &self,
        repo: &Repository,
        remote: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["push", "--force-with-lease", remote, branch])
    }

    fn discard_local_to_remote(
        &self,
        repo: &Repository,
        remote: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        // Fetch first so the remote-tracking ref is fresh.
        run_git_in_repo(repo, &["fetch", remote])?;
        let target = format!("{}/{}", remote, branch);
        run_git_in_repo(repo, &["reset", "--hard", &target])
    }

    // Divergence reconciliation (Phase 12 — S9/S13).
    fn fetch(&self, repo: &Repository, remote: &str, prune: bool) -> anyhow::Result<()> {
        let args: &[&str] = if prune {
            &["fetch", "--prune"]
        } else {
            &["fetch"]
        };
        let mut full = args.to_vec();
        full.push(remote);
        let output = run_git_capture(repo, &full)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let class = classify_git_stderr(&format!("{}\n{}", stdout, stderr));
        Err(anyhow::Error::new(ClassifiedFailure {
            class,
            raw_stderr: stderr,
        }))
    }

    fn merge(&self, repo: &Repository, source: &str, message: &str) -> anyhow::Result<()> {
        // Resolve the merge source (e.g. "origin/main") to an annotated commit.
        let (obj, _r) = repo
            .revparse_ext(source)
            .with_context(|| format!("Could not resolve merge source '{}'", source))?;
        let oid = obj.id();
        let annotated = repo
            .find_annotated_commit(oid)
            .with_context(|| format!("'{}' does not point to a commit", source))?;

        let (analysis, _pref) = repo
            .merge_analysis(&[&annotated])
            .context("Failed to analyze merge")?;

        if analysis.is_up_to_date() {
            return Ok(()); // nothing to integrate
        }

        // Fast-forward: move the current branch ref forward, no merge commit
        // (used by the ahead==0 reconcile guard). Safe checkout — the reconcile
        // precondition is a clean tree, so this never clobbers uncommitted work.
        if analysis.is_fast_forward() {
            let head = repo.head().context("HEAD unavailable for fast-forward")?;
            let refname = head
                .name()
                .ok_or_else(|| anyhow::anyhow!("HEAD has no ref name"))?
                .to_string();
            let mut reference = repo.find_reference(&refname)?;
            reference.set_target(oid, "reconcile: fast-forward")?;
            repo.set_head(&refname)?;
            // Force is safe here: the reconcile precondition is a clean tree
            // (the user committed before diverging), so there is no uncommitted
            // work to clobber, and the working tree must actually advance to the
            // target — a SAFE checkout can no-op when the index still reflects
            // the old tree.
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .context("Fast-forward checkout failed")?;
            return Ok(());
        }

        // Normal 3-way merge. On conflict this writes conflict markers, sets
        // MERGE_HEAD, and leaves RepositoryState::Merge — we surface that as an
        // Err so the pipeline pauses into the Phase 11 verbs (MidMerge).
        repo.merge(&[&annotated], None, None)
            .context("Merge failed")?;
        let index = repo.index().context("Failed to read index after merge")?;
        if index.has_conflicts() {
            anyhow::bail!("merge left conflicts to resolve");
        }
        // Clean merge — land the merge commit (2 parents) via the merge-aware
        // commit(), which also clears MERGE_HEAD/MERGE_MSG (S17).
        self.commit(repo, message).map(|_| ())
    }

    fn rebase(&self, repo: &Repository, onto: &str) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["rebase", onto])
    }

    // In-progress backout/continue steps (Phase 11). Continues run
    // non-interactively (reuse the prepared message); aborts/skips don't need
    // an editor. Inherited stdio surfaces git's own conflict output to the user.
    fn merge_abort(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["merge", "--abort"])
    }
    fn rebase_continue(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_noninteractive(repo, &["rebase", "--continue"])
    }
    fn rebase_skip(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["rebase", "--skip"])
    }
    fn rebase_abort(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["rebase", "--abort"])
    }
    fn cherry_pick_continue(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_noninteractive(repo, &["cherry-pick", "--continue"])
    }
    fn cherry_pick_abort(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["cherry-pick", "--abort"])
    }
    fn revert_continue(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_noninteractive(repo, &["revert", "--continue"])
    }
    fn revert_abort(&self, repo: &Repository) -> anyhow::Result<()> {
        run_git_in_repo(repo, &["revert", "--abort"])
    }

    fn get_status_summary(&self, repo: &Repository) -> anyhow::Result<StatusSummary> {
        crate::context::build_context(repo).map(|ctx| ctx.status)
    }

    fn get_diff_snippet(
        &self,
        repo: &Repository,
        path: &str,
        max_lines: usize,
    ) -> anyhow::Result<String> {
        let mut diff_opts = git2::DiffOptions::new();
        diff_opts.pathspec(path);
        diff_opts.ignore_whitespace(true);
        diff_opts.ignore_whitespace_change(true);

        let diff = repo
            .diff_index_to_workdir(None, Some(&mut diff_opts))
            .context("Failed to generate diff")?;

        let mut snippet = String::new();
        let mut line_count = 0;

        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            if line_count >= max_lines {
                return true;
            }
            let prefix = match line.origin() {
                '+' => "+",
                '-' => "-",
                _ => " ",
            };
            if let Ok(content) = std::str::from_utf8(line.content()) {
                snippet.push_str(prefix);
                snippet.push_str(content);
                line_count += 1;
            }
            true
        })
        .context("Failed to print diff")?;

        if snippet.is_empty() {
            // Try HEAD-to-index diff (for staged changes)
            let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

            let mut diff_opts2 = git2::DiffOptions::new();
            diff_opts2.pathspec(path);
            diff_opts2.ignore_whitespace(true);
            diff_opts2.ignore_whitespace_change(true);

            let diff2 = repo
                .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts2))
                .context("Failed to generate staged diff")?;

            diff2
                .print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
                    if line_count >= max_lines {
                        return true;
                    }
                    let prefix = match line.origin() {
                        '+' => "+",
                        '-' => "-",
                        _ => " ",
                    };
                    if let Ok(content) = std::str::from_utf8(line.content()) {
                        snippet.push_str(prefix);
                        snippet.push_str(content);
                        line_count += 1;
                    }
                    true
                })
                .context("Failed to print staged diff")?;
        }

        Ok(snippet)
    }
}

/// Execute a resolved pipeline step against a repository.
pub fn execute_step(
    ops: &dyn GitOperations,
    repo: &mut Repository,
    step: &PipelineStep,
) -> anyhow::Result<()> {
    match step {
        PipelineStep::Add { files } => {
            if files.is_empty() {
                ops.add_all(repo)
            } else {
                ops.add(repo, files)
            }
        }
        PipelineStep::Commit { message } => ops.commit(repo, message).map(|_| ()),
        PipelineStep::Push {
            remote,
            branch,
            set_upstream,
        } => ops.push(repo, remote, branch, *set_upstream),
        PipelineStep::Pull { remote } => ops.pull(repo, remote),
        PipelineStep::Branch { name } => ops.create_branch(repo, name),
        PipelineStep::Checkout { target } => ops.checkout(repo, target),
        PipelineStep::Stash => ops.stash(repo).map(|_| ()),
        PipelineStep::StashPop => ops.stash_pop(repo),
        PipelineStep::Reset { mode } => ops.reset(repo, *mode),
        PipelineStep::Log { count } => {
            let entries = ops.log(repo, *count)?;
            for entry in &entries {
                println!("{}", entry);
            }
            Ok(())
        }
        PipelineStep::Status => {
            let summary = ops.get_status_summary(repo)?;
            if !summary.staged.is_empty() {
                println!("Staged:");
                for f in &summary.staged {
                    println!("  {}", f);
                }
            }
            if !summary.modified.is_empty() {
                println!("Modified:");
                for f in &summary.modified {
                    println!("  {}", f);
                }
            }
            if !summary.untracked.is_empty() {
                println!("Untracked:");
                for f in &summary.untracked {
                    println!("  {}", f);
                }
            }
            if summary.staged.is_empty()
                && summary.modified.is_empty()
                && summary.untracked.is_empty()
            {
                println!("Nothing to commit, working tree clean");
            }
            Ok(())
        }
        PipelineStep::Diff { path } => {
            let diff_path = path.as_deref().unwrap_or(".");
            let snippet = ops.get_diff_snippet(repo, diff_path, 100)?;
            if snippet.is_empty() {
                println!("No changes.");
            } else {
                print!("{}", snippet);
            }
            Ok(())
        }
        PipelineStep::AddRemote { name, url } => ops.add_remote(repo, name, url),
        PipelineStep::RemoveRemote { name } => ops.remove_remote(repo, name),
        PipelineStep::Undo { to_oid, .. } => {
            let oid = git2::Oid::from_str(to_oid)
                .with_context(|| format!("Invalid OID in Undo step: {}", to_oid))?;
            ops.undo_last_commit(repo, oid)
        }
        PipelineStep::PullRebase { remote, branch } => ops.pull_rebase(repo, remote, branch),
        PipelineStep::ForcePush { remote, branch } => {
            ops.force_push_with_lease(repo, remote, branch)
        }
        PipelineStep::DiscardLocal { remote, branch } => {
            ops.discard_local_to_remote(repo, remote, branch)
        }
        PipelineStep::Merge { source, message } => ops.merge(repo, source, message),
        PipelineStep::Rebase { onto } => ops.rebase(repo, onto),
        PipelineStep::MergeAbort => ops.merge_abort(repo),
        PipelineStep::RebaseContinue => ops.rebase_continue(repo),
        PipelineStep::RebaseSkip => ops.rebase_skip(repo),
        PipelineStep::RebaseAbort => ops.rebase_abort(repo),
        PipelineStep::CherryPickContinue => ops.cherry_pick_continue(repo),
        PipelineStep::CherryPickAbort => ops.cherry_pick_abort(repo),
        PipelineStep::RevertContinue => ops.revert_continue(repo),
        PipelineStep::RevertAbort => ops.revert_abort(repo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // FailureClass classifier — string-match heuristics over system git stderr
    // -----------------------------------------------------------------------

    #[test]
    fn classify_dirty_tree_blocks_merge() {
        let stderr = "\
error: Your local changes to the following files would be overwritten by merge:
\tREADME.md
\tsrc/main.rs
Please commit your changes or stash them before you merge.
Aborting";
        let class = classify_git_stderr(stderr);
        match class {
            FailureClass::DirtyTreeBlocksMerge { files } => {
                assert_eq!(
                    files,
                    vec!["README.md".to_string(), "src/main.rs".to_string()]
                );
            }
            other => panic!("expected DirtyTreeBlocksMerge, got {:?}", other),
        }
    }

    #[test]
    fn classify_non_fast_forward() {
        let stderr = "\
To github.com:foo/bar.git
 ! [rejected]        main -> main (non-fast-forward)
error: failed to push some refs to 'github.com:foo/bar.git'
hint: Updates were rejected because the tip of your current branch is behind\nhint: its remote counterpart. Integrate the remote changes (e.g.\nhint: 'git pull ...') before pushing again.";
        assert_eq!(classify_git_stderr(stderr), FailureClass::NonFastForward);
    }

    #[test]
    fn classify_auth_ssh_publickey() {
        let stderr = "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.";
        assert_eq!(classify_git_stderr(stderr), FailureClass::AuthFailed);
    }

    #[test]
    fn classify_auth_https_403() {
        let stderr = "remote: HTTP 403: Forbidden\nfatal: unable to access 'https://github.com/foo/bar.git/': The requested URL returned error: 403";
        assert_eq!(classify_git_stderr(stderr), FailureClass::AuthFailed);
    }

    #[test]
    fn classify_network_dns() {
        let stderr = "ssh: Could not resolve hostname github.com: Name or service not known\nfatal: Could not read from remote repository.";
        assert_eq!(classify_git_stderr(stderr), FailureClass::Network);
    }

    #[test]
    fn classify_lock_held() {
        let stderr = "fatal: Unable to create '/repo/.git/index.lock': File exists.";
        assert_eq!(classify_git_stderr(stderr), FailureClass::LockHeld);
    }

    #[test]
    fn classify_unknown_preserves_stderr() {
        let stderr = "fatal: some unexpected git failure";
        match classify_git_stderr(stderr) {
            FailureClass::Unknown(s) => assert!(s.contains("unexpected git failure")),
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn failure_class_fix_messages_are_actionable() {
        // Every fix() string must be non-empty and start with a verb-ish
        // word, not a question. Keep this loose — just guard against
        // accidentally introducing prompts ("Do you want to…?") as fixes.
        let cases = [
            FailureClass::DirtyTreeBlocksMerge {
                files: vec!["a".into()],
            },
            FailureClass::NonFastForward,
            FailureClass::AuthFailed,
            FailureClass::Network,
            FailureClass::LockHeld,
            FailureClass::HookRejected {
                hook: Some("pre-commit".into()),
                output: "blocked".into(),
            },
            FailureClass::Unknown("x".into()),
        ];
        for c in cases {
            let fix = c.fix();
            assert!(!fix.is_empty(), "fix for {:?} was empty", c);
            assert!(
                !fix.contains('?'),
                "fix for {:?} shouldn't be a question",
                c
            );
        }
    }

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
    fn test_add_files() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        std::fs::write(dir.path().join("new.txt"), "new file").unwrap();

        let ops = Git2Operations;
        ops.add(&repo, &["new.txt".to_string()]).unwrap();

        let statuses = repo.statuses(None).unwrap();
        let has_staged = statuses
            .iter()
            .any(|e| e.path() == Some("new.txt") && e.status().contains(git2::Status::INDEX_NEW));
        assert!(has_staged, "new.txt should be staged");
    }

    #[test]
    fn test_add_rejects_path_traversal() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let ops = Git2Operations;
        let result = ops.add(&repo, &["../etc/passwd".to_string()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid file path"));
    }

    #[test]
    fn test_commit_on_empty_repo() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        // Create and stage a file
        std::fs::write(dir.path().join("init.txt"), "first").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("init.txt")).unwrap();
        index.write().unwrap();

        let ops = Git2Operations;
        let oid = ops.commit(&repo, "first commit").unwrap();
        assert!(!oid.is_zero());

        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.summary(), Some("first commit"));
        assert_eq!(commit.parent_count(), 0); // No parents — first commit
    }

    #[test]
    fn test_commit_with_parent() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Modify and stage
        std::fs::write(dir.path().join("test.txt"), "modified").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();

        let ops = Git2Operations;
        let oid = ops.commit(&repo, "second commit").unwrap();
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.parent_count(), 1);
    }

    #[test]
    fn test_commit_detached_head() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Detach HEAD
        let head_oid = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(head_oid).unwrap();

        let ops = Git2Operations;
        let result = ops.commit(&repo, "should fail");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HEAD is detached"));
    }

    #[test]
    fn test_commit_empty_message() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let ops = Git2Operations;
        let result = ops.commit(&repo, "  ");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_create_branch() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let ops = Git2Operations;
        ops.create_branch(&repo, "feat/test").unwrap();

        let branch = repo.find_branch("feat/test", git2::BranchType::Local);
        assert!(branch.is_ok());
    }

    #[test]
    fn test_checkout_branch() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let ops = Git2Operations;

        ops.create_branch(&repo, "feat/checkout-test").unwrap();
        ops.checkout(&repo, "feat/checkout-test").unwrap();

        let head = repo.head().unwrap();
        assert_eq!(head.shorthand(), Some("feat/checkout-test"));
    }

    #[test]
    fn test_log() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Make a second commit
        std::fs::write(dir.path().join("test.txt"), "updated").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second commit", &tree, &[&parent])
            .unwrap();

        let ops = Git2Operations;
        let entries = ops.log(&repo, 5).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("second commit"));
        assert!(entries[1].contains("initial commit"));
    }

    #[test]
    fn test_reset_soft() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let first_oid = repo.head().unwrap().target().unwrap();

        // Make second commit
        std::fs::write(dir.path().join("test.txt"), "changed").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        // Reset soft back to first commit
        let first_commit = repo.find_commit(first_oid).unwrap();
        let obj = first_commit.as_object();
        repo.reset(obj, git2::ResetType::Soft, None).unwrap();

        let current_oid = repo.head().unwrap().target().unwrap();
        assert_eq!(current_oid, first_oid);
    }

    #[test]
    fn test_diff_snippet() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        std::fs::write(dir.path().join("test.txt"), "modified content").unwrap();

        let ops = Git2Operations;
        let snippet = ops.get_diff_snippet(&repo, "test.txt", 25).unwrap();
        assert!(!snippet.is_empty());
        assert!(snippet.contains("+modified content") || snippet.contains("-hello"));
    }

    #[test]
    fn test_execute_step_status() {
        let dir = TempDir::new().unwrap();
        let mut repo = init_repo_with_commit(&dir);
        let ops = Git2Operations;
        let result = execute_step(&ops, &mut repo, &PipelineStep::Status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stash_and_pop() {
        let dir = TempDir::new().unwrap();
        let mut repo = init_repo_with_commit(&dir);

        // Modify a tracked file
        std::fs::write(dir.path().join("test.txt"), "stash me").unwrap();

        let ops = Git2Operations;
        let oid = ops.stash(&mut repo).unwrap();
        assert!(!oid.is_zero());

        // Working tree should be clean after stash
        let content = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "hello");

        // Pop should restore
        ops.stash_pop(&mut repo).unwrap();
        let restored = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(restored, "stash me");
    }

    #[test]
    fn test_get_commit_details_head() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let ops = Git2Operations;

        let details = ops.get_commit_details(&repo, "HEAD").unwrap();
        assert_eq!(details.sha.len(), 7);
        assert_eq!(details.full_sha.len(), 40);
        assert_eq!(details.author, "Test");
        assert_eq!(details.email, "test@test.com");
        assert!(details.message.contains("initial commit"));
        // Root commit — no parent, no file_changes from diff.
        assert!(details.parent_sha.is_none());
    }

    #[test]
    fn test_get_commit_details_with_parent_has_file_changes() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Make a second commit modifying test.txt
        std::fs::write(dir.path().join("test.txt"), "modified content\nline 2\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        let ops = Git2Operations;
        let details = ops.get_commit_details(&repo, "HEAD").unwrap();
        assert!(details.parent_sha.is_some());
        assert!(!details.file_changes.is_empty());
        assert_eq!(details.file_changes[0].path, "test.txt");
    }

    /// Stage a path with the given content, write the index, and create a commit.
    fn commit_with_content(
        repo: &Repository,
        path: &str,
        content: &str,
        msg: &str,
        parent: Option<&git2::Commit>,
    ) -> git2::Oid {
        let workdir = repo.workdir().unwrap();
        std::fs::write(workdir.join(path), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parents: Vec<&git2::Commit> = parent.into_iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    #[test]
    fn test_predict_pull_clean_merge_no_conflicts() {
        // Local edits file A, remote edits file B → no overlap, clean merge.
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let base = repo.head().unwrap().peel_to_commit().unwrap();

        // Simulate remote: create a commit modifying b.txt off the base.
        let remote_oid =
            commit_with_content(&repo, "b.txt", "remote content\n", "remote", Some(&base));
        let remote_commit = repo.find_commit(remote_oid).unwrap();
        repo.reference(
            "refs/remotes/origin/master",
            remote_oid,
            true,
            "set remote tracking",
        )
        .unwrap();

        // Reset HEAD back to base, then make a local commit modifying test.txt (the existing file).
        let base_obj = base.as_object();
        repo.reset(base_obj, git2::ResetType::Hard, None).unwrap();
        commit_with_content(&repo, "test.txt", "local content\n", "local", Some(&base));

        // Set up local 'master' to track origin/master so predict_pull can find both refs.
        // (init_repo_with_commit uses default branch which may be master or main; assume master.)
        let _ = remote_commit;

        let ops = Git2Operations;
        let preview = ops.predict_pull(&repo, "origin").unwrap();
        match preview {
            MergePreview::CleanMerge {
                local_commits,
                remote_commits,
            } => {
                assert_eq!(local_commits, 1);
                assert_eq!(remote_commits, 1);
            }
            other => panic!("Expected CleanMerge, got {:?}", other),
        }
    }

    #[test]
    fn test_predict_pull_conflicts_when_both_edit_same_file() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let base = repo.head().unwrap().peel_to_commit().unwrap();

        // Remote modifies test.txt with one content.
        let remote_oid =
            commit_with_content(&repo, "test.txt", "REMOTE EDIT\n", "remote", Some(&base));
        repo.reference("refs/remotes/origin/master", remote_oid, true, "remote")
            .unwrap();

        // Reset, then local also modifies test.txt with different content.
        let base_obj = base.as_object();
        repo.reset(base_obj, git2::ResetType::Hard, None).unwrap();
        commit_with_content(&repo, "test.txt", "LOCAL EDIT\n", "local", Some(&base));

        let ops = Git2Operations;
        let preview = ops.predict_pull(&repo, "origin").unwrap();
        match preview {
            MergePreview::Conflicts { files, .. } => {
                assert!(
                    files.iter().any(|f| f == "test.txt"),
                    "expected test.txt in conflicts, got {:?}",
                    files
                );
            }
            other => panic!("Expected Conflicts, got {:?}", other),
        }
    }

    #[test]
    fn test_undo_last_commit_keeps_files_staged() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Make a second commit
        std::fs::write(dir.path().join("test.txt"), "version 2").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "v2", &tree, &[&parent])
            .unwrap();
        let head_oid_before_undo = repo.head().unwrap().target().unwrap();
        assert_ne!(head_oid_before_undo, parent.id()); // sanity

        let ops = Git2Operations;
        ops.undo_last_commit(&repo, parent.id()).unwrap();

        // HEAD should now point to the parent (the original commit).
        let head_oid_after = repo.head().unwrap().target().unwrap();
        assert_eq!(head_oid_after, parent.id());
        assert_ne!(head_oid_after, head_oid_before_undo);

        // Index should still contain the modified file (staged).
        let idx = repo.index().unwrap();
        let entry = idx.get_path(std::path::Path::new("test.txt"), 0);
        assert!(entry.is_some());
    }

    #[test]
    fn test_commit_during_merge_has_two_parents() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
        }
        let path = dir.path();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();

        let stage = |content: &str| -> git2::Oid {
            std::fs::write(path.join("f.txt"), content).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };

        // base commit
        let tree_base = repo.find_tree(stage("base\n")).unwrap();
        let base = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree_base, &[])
            .unwrap();
        let base_commit = repo.find_commit(base).unwrap();

        // `theirs` branch: child of base editing the same line differently
        let tree_theirs = repo.find_tree(stage("theirs\n")).unwrap();
        let theirs = repo
            .commit(
                Some("refs/heads/theirs"),
                &sig,
                &sig,
                "theirs",
                &tree_theirs,
                &[&base_commit],
            )
            .unwrap();

        // `ours` on the current branch: another child of base, conflicting edit
        let tree_ours = repo.find_tree(stage("ours\n")).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "ours",
            &tree_ours,
            &[&base_commit],
        )
        .unwrap();

        // Start a real merge of `theirs` — conflicts, sets MERGE_HEAD.
        let their_ac = repo.find_annotated_commit(theirs).unwrap();
        repo.merge(&[&their_ac], None, None).unwrap();
        assert!(
            repo.index().unwrap().has_conflicts(),
            "expected a conflicted merge"
        );
        assert_eq!(repo.state(), git2::RepositoryState::Merge);

        // Resolve the conflict: write a resolution and stage it (clears conflict).
        std::fs::write(path.join("f.txt"), "resolved\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("f.txt")).unwrap();
        index.write().unwrap();
        assert!(!repo.index().unwrap().has_conflicts());

        // Commit through GLit's own path.
        let ops = Git2Operations;
        let oid = ops.commit(&repo, "merge theirs into ours").unwrap();

        // The bug: a single-parent commit here silently destroys the merge and
        // leaves MERGE_HEAD lingering (S17). A correct merge commit has 2 parents.
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(
            commit.parent_count(),
            2,
            "merge commit must have exactly 2 parents"
        );
        assert_eq!(
            repo.state(),
            git2::RepositoryState::Clean,
            "merge state must be cleared after committing"
        );
        assert!(
            repo.find_reference("MERGE_HEAD").is_err(),
            "MERGE_HEAD must be gone after the merge commit"
        );
    }

    #[test]
    fn test_hook_routing_decision() {
        assert!(should_route_to_system_git("system", false));
        assert!(should_route_to_system_git("system", true));
        assert!(!should_route_to_system_git("git2", true));
        assert!(!should_route_to_system_git("git2", false));
        assert!(should_route_to_system_git("auto", true));
        assert!(!should_route_to_system_git("auto", false));
        assert!(should_route_to_system_git("", true)); // unknown value → auto
    }

    #[test]
    fn test_hooks_for_op_detects_executable_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);
        let hooks = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");

        // Present but NOT executable → not detected (matches libgit2/git).
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(hooks_for_op(&repo, HookOp::Commit).is_empty());

        // Executable → detected.
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            hooks_for_op(&repo, HookOp::Commit),
            vec!["pre-commit".to_string()]
        );
        // A commit hook is not a push hook.
        assert!(hooks_for_op(&repo, HookOp::Push).is_empty());
    }

    #[test]
    fn test_push_with_failing_pre_push_hook_is_blocked() {
        use std::os::unix::fs::PermissionsExt;

        // Bare remote to push into — a local path, so no network/auth needed.
        let remote_dir = TempDir::new().unwrap();
        Repository::init_bare(remote_dir.path()).unwrap();

        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
        }
        let path = dir.path();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();

        // Initial commit.
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);

        repo.remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();

        // Failing executable pre-push hook (runs on the pushing side).
        let hooks_dir = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook = hooks_dir.join("pre-push");
        std::fs::write(
            &hook,
            "#!/bin/sh\necho 'HOOK: blocked by pre-push' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let branch = repo.head().unwrap().shorthand().unwrap().to_string();
        let ops = Git2Operations;
        let result = ops.push(&repo, "origin", &branch, true);

        // Push must be blocked and, mirroring the commit path, classify as
        // HookRejected (not a generic Unknown) so the hook-specific recovery
        // surfaces (S47/S46).
        let err = result.expect_err("push must be blocked by the failing pre-push hook");
        let classified = err
            .downcast_ref::<ClassifiedFailure>()
            .expect("error should be a ClassifiedFailure");
        assert!(
            matches!(classified.class, FailureClass::HookRejected { .. }),
            "expected HookRejected, got {:?}",
            classified.class
        );
        // The captured hook output is surfaced in the failure message (req. 3).
        let msg = classified.class.what();
        assert!(
            msg.contains("blocked by pre-push"),
            "hook output should appear in the message, got: {:?}",
            msg
        );
    }

    #[test]
    fn test_push_server_side_rejection_is_not_hookrejected() {
        use std::os::unix::fs::PermissionsExt;

        // Bare remote with a REJECTING server-side pre-receive hook.
        let remote_dir = TempDir::new().unwrap();
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        let remote_hooks = remote.path().join("hooks");
        std::fs::create_dir_all(&remote_hooks).unwrap();
        let pre_receive = remote_hooks.join("pre-receive");
        std::fs::write(
            &pre_receive,
            "#!/bin/sh\necho 'SERVER: branch is protected' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&pre_receive, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
        }
        let path = dir.path();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        repo.remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();

        // A PASSING local pre-push hook: the client hook clears, but the server
        // rejects. This must NOT be mistaken for a local pre-push rejection.
        let hooks_dir = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let pre_push = hooks_dir.join("pre-push");
        std::fs::write(&pre_push, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&pre_push, std::fs::Permissions::from_mode(0o755)).unwrap();

        let branch = repo.head().unwrap().shorthand().unwrap().to_string();
        let ops = Git2Operations;
        let result = ops.push(&repo, "origin", &branch, true);

        let err = result.expect_err("server pre-receive hook should reject the push");
        let classified = err
            .downcast_ref::<ClassifiedFailure>()
            .expect("ClassifiedFailure");
        // Server-side policy rejection is NOT a local hook rejection. A dedicated
        // ServerPolicy class is Tier 2 (S43); until then Unknown is acceptable —
        // just not a misleading HookRejected pointing at the local pre-push hook.
        assert!(
            !matches!(classified.class, FailureClass::HookRejected { .. }),
            "server rejection must not be HookRejected, got {:?}\n--- stderr ---\n{}",
            classified.class,
            classified.raw_stderr
        );
    }
}

// =============================================================================
// Phase 11 — in-progress backout/continue outcome tests (real git in tempdir).
// =============================================================================
#[cfg(test)]
mod phase11_ops_tests {
    use super::{Git2Operations, GitOperations};
    use git2::Repository;
    use std::path::Path;
    use tempfile::TempDir;

    fn git(path: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("failed to spawn git")
    }
    fn git_ok(path: &Path, args: &[&str]) {
        let o = git(path, args);
        assert!(
            o.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&o.stderr)
        );
    }

    /// A repo mid-rebase, stopped on a conflict in f.txt. Returns the opened repo
    /// and the pre-rebase branch tip (what an abort must restore HEAD to).
    fn setup_conflicting_rebase(path: &Path) -> (Repository, git2::Oid) {
        git_ok(path, &["init", "-q", "-b", "main", "."]);
        git_ok(path, &["config", "user.email", "t@t.co"]);
        git_ok(path, &["config", "user.name", "T"]);
        std::fs::write(path.join("f.txt"), "base\n").unwrap();
        git_ok(path, &["add", "f.txt"]);
        git_ok(path, &["commit", "-qm", "base"]);
        git_ok(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(path.join("f.txt"), "feature\n").unwrap();
        git_ok(path, &["commit", "-qam", "feature"]);
        git_ok(path, &["checkout", "-q", "main"]);
        std::fs::write(path.join("f.txt"), "main\n").unwrap();
        git_ok(path, &["commit", "-qam", "main"]);
        let repo = Repository::open(path).unwrap();
        let orig = repo.head().unwrap().target().unwrap();
        let o = git(path, &["rebase", "feature"]);
        assert!(!o.status.success(), "rebase should have conflicted");
        (Repository::open(path).unwrap(), orig)
    }

    /// A repo mid-merge, stopped on a conflict in f.txt. Returns the opened repo
    /// and the pre-merge branch tip.
    fn setup_conflicting_merge(path: &Path) -> (Repository, git2::Oid) {
        git_ok(path, &["init", "-q", "-b", "main", "."]);
        git_ok(path, &["config", "user.email", "t@t.co"]);
        git_ok(path, &["config", "user.name", "T"]);
        std::fs::write(path.join("f.txt"), "base\n").unwrap();
        git_ok(path, &["add", "f.txt"]);
        git_ok(path, &["commit", "-qm", "base"]);
        git_ok(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(path.join("f.txt"), "feature\n").unwrap();
        git_ok(path, &["commit", "-qam", "feature"]);
        git_ok(path, &["checkout", "-q", "main"]);
        std::fs::write(path.join("f.txt"), "main\n").unwrap();
        git_ok(path, &["commit", "-qam", "main"]);
        let repo = Repository::open(path).unwrap();
        let orig = repo.head().unwrap().target().unwrap();
        let o = git(path, &["merge", "feature"]);
        assert!(!o.status.success(), "merge should have conflicted");
        (Repository::open(path).unwrap(), orig)
    }

    #[test]
    fn rebase_abort_restores_original_head() {
        let dir = TempDir::new().unwrap();
        let (repo, orig) = setup_conflicting_rebase(dir.path());
        assert_ne!(
            repo.state(),
            git2::RepositoryState::Clean,
            "should be mid-rebase"
        );
        Git2Operations.rebase_abort(&repo).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(repo.state(), git2::RepositoryState::Clean);
        assert_eq!(
            repo.head().unwrap().target().unwrap(),
            orig,
            "abort must restore the pre-rebase HEAD oid"
        );
    }

    #[test]
    fn rebase_continue_completes_after_resolution() {
        let dir = TempDir::new().unwrap();
        let (repo, _orig) = setup_conflicting_rebase(dir.path());
        // Resolve + stage the conflict, then continue.
        std::fs::write(dir.path().join("f.txt"), "resolved\n").unwrap();
        git_ok(dir.path(), &["add", "f.txt"]);
        Git2Operations.rebase_continue(&repo).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(
            repo.state(),
            git2::RepositoryState::Clean,
            "rebase should be complete"
        );
        assert!(
            !repo.head_detached().unwrap(),
            "should land back on a branch"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "resolved\n"
        );
    }

    #[test]
    fn merge_abort_restores_original_head() {
        let dir = TempDir::new().unwrap();
        let (repo, orig) = setup_conflicting_merge(dir.path());
        assert_eq!(
            repo.state(),
            git2::RepositoryState::Merge,
            "should be mid-merge"
        );
        Git2Operations.merge_abort(&repo).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(repo.state(), git2::RepositoryState::Clean);
        assert!(
            repo.find_reference("MERGE_HEAD").is_err(),
            "MERGE_HEAD must be gone"
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), orig);
    }
}
