use anyhow::Context;
use git2::Repository;
use serde::{Deserialize, Serialize};

/// A modified file with its diff snippet for LLM classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileManifest {
    pub path: String,
    pub change_type: ChangeType,
    pub diff_snippet: String,
    pub line_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// Build manifests for all modified files in the repository.
/// Each manifest includes a diff snippet (max_lines_per_file lines, whitespace-ignored).
pub fn build_manifest(
    repo: &Repository,
    max_lines_per_file: usize,
) -> anyhow::Result<Vec<FileManifest>> {
    let statuses = repo
        .statuses(None)
        .context("Failed to read repository status")?;

    let mut manifests = Vec::new();

    for entry in statuses.iter() {
        let path = match entry.path() {
            Some(p) => p.to_string(),
            None => continue,
        };

        let status = entry.status();
        let change_type = if status.intersects(
            git2::Status::INDEX_NEW | git2::Status::WT_NEW,
        ) {
            ChangeType::Added
        } else if status.intersects(
            git2::Status::INDEX_DELETED | git2::Status::WT_DELETED,
        ) {
            ChangeType::Deleted
        } else if status.intersects(
            git2::Status::INDEX_RENAMED | git2::Status::WT_RENAMED,
        ) {
            ChangeType::Renamed
        } else {
            ChangeType::Modified
        };

        let snippet = get_whitespace_ignored_diff(repo, &path, max_lines_per_file)
            .unwrap_or_default();
        let line_count = snippet.lines().count();

        manifests.push(FileManifest {
            path,
            change_type,
            diff_snippet: snippet,
            line_count,
        });
    }

    Ok(manifests)
}

/// Get a diff snippet with whitespace changes ignored.
fn get_whitespace_ignored_diff(
    repo: &Repository,
    path: &str,
    max_lines: usize,
) -> anyhow::Result<String> {
    let mut diff_opts = git2::DiffOptions::new();
    diff_opts.pathspec(path);
    diff_opts.ignore_whitespace(true);
    diff_opts.ignore_whitespace_change(true);

    // Try workdir diff first, then staged diff
    let diff = repo
        .diff_index_to_workdir(None, Some(&mut diff_opts))
        .context("Failed to generate diff")?;

    let mut snippet = String::new();
    let mut count = 0;

    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        if count >= max_lines {
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
            count += 1;
        }
        true
    })?;

    if snippet.is_empty() {
        // Try HEAD-to-index (staged changes)
        let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let mut diff_opts2 = git2::DiffOptions::new();
        diff_opts2.pathspec(path);
        diff_opts2.ignore_whitespace(true);
        diff_opts2.ignore_whitespace_change(true);

        let diff2 = repo
            .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts2))
            .context("Failed to generate staged diff")?;

        diff2.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            if count >= max_lines {
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
                count += 1;
            }
            true
        })?;
    }

    Ok(snippet)
}

/// Build a single text block from manifests for the LLM, respecting a token budget.
/// Sort by diff size ascending — small diffs = higher signal density = show first.
pub fn build_manifest_block(
    manifests: &[FileManifest],
    max_lines_per_file: usize,
    token_budget: usize,
) -> String {
    let mut sorted: Vec<&FileManifest> = manifests.iter().collect();
    sorted.sort_by_key(|f| f.line_count);

    let mut used = 0usize;
    let mut block = String::new();

    for (i, f) in sorted.iter().enumerate() {
        let snippet = if used < token_budget {
            let lines = f.diff_snippet.lines().count().min(max_lines_per_file);
            let file_tokens = 10 + lines * 5; // rough token estimate
            used += file_tokens;

            if f.diff_snippet.lines().count() > max_lines_per_file {
                f.diff_snippet
                    .lines()
                    .take(max_lines_per_file)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                f.diff_snippet.clone()
            }
        } else {
            format!(
                "({} lines changed — budget exceeded, snippet omitted)",
                f.line_count
            )
        };

        block += &format!(
            "FILE {} [{:?}] {}\n{}\n---\n",
            i, f.change_type, f.path, snippet
        );
    }

    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo_with_commit(dir: &TempDir) -> Repository {
        let repo = Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();
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
    fn test_build_manifest_with_modifications() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_commit(&dir);

        // Modify existing file and create new file
        std::fs::write(dir.path().join("test.txt"), "modified content").unwrap();
        std::fs::write(dir.path().join("new_file.rs"), "fn main() {}").unwrap();

        let manifests = build_manifest(&repo, 25).unwrap();
        assert!(manifests.len() >= 1); // at least the modified file

        let modified = manifests.iter().find(|m| m.path == "test.txt");
        assert!(modified.is_some());
    }

    #[test]
    fn test_build_manifest_empty_repo() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let manifests = build_manifest(&repo, 25).unwrap();
        assert!(manifests.is_empty());
    }

    #[test]
    fn test_build_manifest_block_token_budget() {
        let manifests = vec![
            FileManifest {
                path: "small.rs".to_string(),
                change_type: ChangeType::Modified,
                diff_snippet: "+one\n+two\n+three".to_string(),
                line_count: 3,
            },
            FileManifest {
                path: "large.rs".to_string(),
                change_type: ChangeType::Modified,
                diff_snippet: (0..100)
                    .map(|i| format!("+line {}", i))
                    .collect::<Vec<_>>()
                    .join("\n"),
                line_count: 100,
            },
        ];

        // Very small budget — large file snippet should be omitted
        let block = build_manifest_block(&manifests, 25, 20);
        assert!(block.contains("small.rs"));
        assert!(block.contains("large.rs"));
        // The large file should have its snippet omitted due to budget
        assert!(block.contains("budget exceeded"));
    }

    #[test]
    fn test_build_manifest_block_ordering() {
        let manifests = vec![
            FileManifest {
                path: "big.rs".to_string(),
                change_type: ChangeType::Modified,
                diff_snippet: "lots of lines".to_string(),
                line_count: 50,
            },
            FileManifest {
                path: "tiny.rs".to_string(),
                change_type: ChangeType::Added,
                diff_snippet: "+one".to_string(),
                line_count: 1,
            },
        ];

        let block = build_manifest_block(&manifests, 25, 3000);
        let tiny_pos = block.find("tiny.rs").unwrap();
        let big_pos = block.find("big.rs").unwrap();
        // Small files should come first (sorted by line_count ascending)
        assert!(tiny_pos < big_pos);
    }
}
