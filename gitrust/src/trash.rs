//! Trash-net (Phase 14 — S30/S62). A GiTrust invariant, sibling to "the LLM never
//! executes": any operation that would delete content **not in the object
//! database** — untracked files (via clean), tracked working-tree modifications
//! (via hard reset / discard-local) — first copies that content to
//! `~/.gitrust/trash/<timestamp>/`, so the destruction is undoable. A sentence git
//! itself cannot say.
//!
//! **Fail-closed:** if the backup cannot be written, the caller (a
//! `GitOperations` destructive method) must return `Err` and NOT proceed —
//! nothing gets deleted without a backup first. See `git_ops`'s destructive
//! methods for the enforcement.
//!
//! **Filesystem-safe timestamps:** trash sub-directory names use
//! `%Y%m%d-%H%M%S` (no colons), because colons are illegal in Windows filenames
//! — an RFC3339 name would make the backup dir un-creatable on Windows exactly
//! when the trash-net must save someone.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// Timestamp format for trash sub-directory names. No colons → valid on every
/// filesystem including Windows.
const TS_FMT: &str = "%Y%m%d-%H%M%S";

/// The trash root: `~/.gitrust/trash/`. Created on access (mirrors
/// `recovery::FileRecoveryManager::snapshots_dir`).
pub fn trash_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::gitrust_dir()?.join("trash"))
}

/// Back up `paths` (files and/or directories, relative to `repo_workdir`) to a
/// fresh timestamped directory under `trash_root`, preserving the relative
/// layout byte-for-byte. Returns the backup directory that was created.
///
/// Directories are copied **recursively** — required for correctness: git2's
/// status collapses an untracked directory to a single entry, but `git clean
/// -fd` deletes its whole tree, so a non-recursive copy would miss files inside
/// untracked dirs (the trash-net's one job, exactly where untracked content
/// lives).
///
/// Fail-closed contract: on any I/O error this returns `Err`, and the caller
/// must abort the destructive operation.
pub fn backup_to_trash(
    trash_root: &Path,
    repo_workdir: &Path,
    paths: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    let ts = chrono::Local::now().format(TS_FMT).to_string();
    let dest_root = trash_root.join(&ts);
    std::fs::create_dir_all(&dest_root)
        .with_context(|| format!("Failed to create trash dir {}", dest_root.display()))?;

    for rel in paths {
        // `rel` may be absolute or relative; normalize to a path under the
        // workdir so the trash mirrors the repo layout.
        let rel = rel.strip_prefix(repo_workdir).unwrap_or(rel);
        let src = repo_workdir.join(rel);
        let dest = dest_root.join(rel);
        if src.is_dir() {
            copy_dir_recursive(&src, &dest)?;
        } else if src.exists() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
            std::fs::copy(&src, &dest)
                .with_context(|| format!("Failed to back up {}", src.display()))?;
        }
        // A path that no longer exists is silently skipped — nothing to back up.
    }

    Ok(dest_root)
}

/// Recursively copy `src` directory into `dest`, creating directories as needed.
fn copy_dir_recursive(src: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("Failed to read dir {}", src.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("Failed to copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// Delete trash sub-directories older than `retention_days`, under `trash_root`.
/// Best-effort: a directory whose name can't be parsed as a timestamp is left
/// alone (never delete what we can't date). Called at startup; the caller
/// ignores its `Err` so pruning never fails the run.
pub fn prune_trash_in(trash_root: &Path, retention_days: u64) -> anyhow::Result<()> {
    if !trash_root.exists() {
        return Ok(());
    }
    let cutoff = chrono::Local::now() - chrono::Duration::days(retention_days as i64);
    let cutoff_naive = cutoff.naive_local();

    for entry in std::fs::read_dir(trash_root)
        .with_context(|| format!("Failed to read trash {}", trash_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        // Parse the filesystem-safe timestamp back. Unparseable → leave alone.
        match chrono::NaiveDateTime::parse_from_str(name, TS_FMT) {
            Ok(ts) if ts < cutoff_naive => {
                let _ = std::fs::remove_dir_all(entry.path());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Convenience wrapper resolving the trash root from `~/.gitrust/trash`.
pub fn prune_trash(retention_days: u64) -> anyhow::Result<()> {
    let root = trash_dir()?;
    prune_trash_in(&root, retention_days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn backs_up_a_loose_file_byte_identical() {
        let repo = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        fs::write(repo.path().join("a.txt"), b"hello world").unwrap();

        let dest = backup_to_trash(trash.path(), repo.path(), &[PathBuf::from("a.txt")]).unwrap();

        let backed = fs::read(dest.join("a.txt")).unwrap();
        assert_eq!(backed, b"hello world");
    }

    // The hole-catching test: an untracked DIRECTORY. git2 status reports it as
    // one entry; the backup must recurse so files inside survive.
    #[test]
    fn backs_up_nested_untracked_directory_recursively() {
        let repo = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        fs::create_dir_all(repo.path().join("newdir/inner")).unwrap();
        fs::write(repo.path().join("newdir/inner/file.txt"), b"deep content").unwrap();

        // The caller passes the DIRECTORY (as git2 status would surface it).
        let dest = backup_to_trash(trash.path(), repo.path(), &[PathBuf::from("newdir")]).unwrap();

        let backed = fs::read(dest.join("newdir/inner/file.txt")).unwrap();
        assert_eq!(
            backed, b"deep content",
            "nested file inside untracked dir must be backed up byte-identical"
        );
    }

    #[test]
    fn fails_closed_when_trash_root_unwritable() {
        // Point the trash root at a path under a *file* — create_dir_all can't
        // make a directory there, so backup must return Err (caller aborts).
        let repo = TempDir::new().unwrap();
        let blocker = TempDir::new().unwrap();
        let file_path = blocker.path().join("not-a-dir");
        fs::write(&file_path, b"x").unwrap();
        let bad_root = file_path.join("trash"); // child of a regular file

        fs::write(repo.path().join("a.txt"), b"data").unwrap();
        let res = backup_to_trash(&bad_root, repo.path(), &[PathBuf::from("a.txt")]);
        assert!(
            res.is_err(),
            "backup into an unwritable root must fail-closed"
        );
    }

    #[test]
    fn prune_removes_old_keeps_recent_and_unparseable() {
        let trash = TempDir::new().unwrap();
        // Old (well past retention), recent (today), and an unparseable name.
        let old = chrono::Local::now() - chrono::Duration::days(30);
        let old_name = old.format(TS_FMT).to_string();
        let recent_name = chrono::Local::now().format(TS_FMT).to_string();
        for n in [&old_name, &recent_name, &"not-a-timestamp".to_string()] {
            fs::create_dir_all(trash.path().join(n)).unwrap();
        }

        prune_trash_in(trash.path(), 7).unwrap();

        assert!(!trash.path().join(&old_name).exists(), "old backup pruned");
        assert!(
            trash.path().join(&recent_name).exists(),
            "recent backup kept"
        );
        assert!(
            trash.path().join("not-a-timestamp").exists(),
            "undatable dir left alone"
        );
    }
}
