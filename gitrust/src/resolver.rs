use std::collections::HashSet;

use crate::commands::{ClassificationResult, ClassifiedFile, ResolutionOutcome};
use crate::config::Config;
use crate::manifest::FileManifest;

/// Trait for file resolution — enables swapping resolution strategy.
pub trait FileResolver {
    fn resolve(
        &self,
        manifests: &[FileManifest],
        feature_hint: &str,
        config: &Config,
    ) -> impl std::future::Future<Output = anyhow::Result<ResolutionOutcome>> + Send;
}

/// Default two-pass resolver (filename match + per-file Kev relatedness).
pub struct TwoPassResolver;

impl FileResolver for TwoPassResolver {
    async fn resolve(
        &self,
        manifests: &[FileManifest],
        feature_hint: &str,
        config: &Config,
    ) -> anyhow::Result<ResolutionOutcome> {
        if manifests.is_empty() {
            return Ok(ResolutionOutcome::NothingModified);
        }

        if manifests.len() > 50 {
            return Ok(ResolutionOutcome::TooManyFiles {
                count: manifests.len(),
            });
        }

        // Pass 1: filename substring match (zero cost)
        let filename_matches: HashSet<String> = filename_match(manifests, feature_hint)
            .into_iter()
            .collect();

        // Collect unmatched files for semantic pass
        let unmatched: Vec<&FileManifest> = manifests
            .iter()
            .filter(|m| !filename_matches.contains(&m.path))
            .collect();

        // Pass 2: per-file Kev relatedness on unmatched files
        let semantic_result = if !unmatched.is_empty() {
            classify_manifest_via_kev(&unmatched, feature_hint, config).await?
        } else {
            ClassificationResult {
                related: vec![],
                uncertain: vec![],
                unrelated: vec![],
            }
        };

        // Merge filename matches (always "related") with semantic results
        let mut related: Vec<ClassifiedFile> = filename_matches
            .iter()
            .map(|path| {
                let change = manifests
                    .iter()
                    .find(|m| &m.path == path)
                    .map(|m| (m.change_type.clone(), (m.added, m.removed)))
                    .unwrap_or((crate::manifest::ChangeType::Modified, (0, 0)));
                ClassifiedFile {
                    path: path.clone(),
                    reason: file_reason(Why::NameMatch, feature_hint, &change.0, change.1, None),
                }
            })
            .collect();
        related.extend(semantic_result.related);

        let result = ClassificationResult {
            related,
            uncertain: semantic_result.uncertain,
            unrelated: semantic_result.unrelated,
        };

        if result.related.is_empty() && result.uncertain.is_empty() {
            let modified_files: Vec<String> = manifests.iter().map(|m| m.path.clone()).collect();
            return Ok(ResolutionOutcome::NothingMatched {
                hint: feature_hint.to_string(),
                suggestion: Some(format!(
                    "Modified files: {}. Try a more specific hint.",
                    modified_files.join(", ")
                )),
            });
        }

        Ok(ResolutionOutcome::Found(result))
    }
}

/// Pass 1: zero-cost filename substring match.
pub fn filename_match(manifests: &[FileManifest], pattern: &str) -> Vec<String> {
    let words: Vec<String> = pattern
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();

    manifests
        .iter()
        .filter(|f| {
            let lower = f.path.to_lowercase();
            words.iter().any(|w| lower.contains(w))
        })
        .map(|f| f.path.clone())
        .collect()
}

/// Kev P(yes) at or above this → related (pre-selected in the picker).
pub const RELATED_AT: f32 = 0.7;
/// Kev P(yes) at or below this → unrelated (hidden unless --show-all).
pub const UNRELATED_AT: f32 = 0.3;
/// Files per Kev request. Kev-0.8B was trained on states up to ~7.5k tokens;
/// 8 files × ≤600 diff chars stays far inside that.
const FILES_PER_REQUEST: usize = 8;
const DIFF_CHARS: usize = 600;

/// Pass 2: ask Kev one yes/no question per file — "does this file belong in a
/// commit of only the <feature> changes?" — with the change set as the state.
/// Calibrated probabilities are banded rather than forced into a binary split:
/// the middle band goes to the user as "uncertain", which is the honest answer
/// when the evidence is thin (paper §9, Problem B).
async fn classify_manifest_via_kev(
    manifests: &[&FileManifest],
    feature: &str,
    config: &Config,
) -> anyhow::Result<ClassificationResult> {
    let mut result = ClassificationResult {
        related: vec![],
        uncertain: vec![],
        unrelated: vec![],
    };
    for chunk in manifests.chunks(FILES_PER_REQUEST) {
        let files: Vec<serde_json::Value> = chunk
            .iter()
            .map(|f| {
                let diff: String = f.diff_snippet.chars().take(DIFF_CHARS).collect();
                serde_json::json!({ "path": f.path, "change": format!("{:?}", f.change_type), "diff": diff })
            })
            .collect();
        let state = serde_json::json!({
            "user_request": format!("push the {} changes", feature),
            "changed_files": files,
        });
        let questions: Vec<String> = chunk
            .iter()
            .map(|f| {
                format!(
                    "Should the changed file `{}` be included in a commit of only the '{}' changes?",
                    f.path, feature
                )
            })
            .collect();
        let probs = {
            let _spin = crate::ux::Spinner::start(crate::copy::SPINNER_MATCHING_FILES);
            crate::kev::ask_nouls(config, state, &questions).await?
        };
        for (f, p) in chunk.iter().zip(probs) {
            let file = ClassifiedFile {
                path: f.path.clone(),
                reason: file_reason(
                    Why::Band(band(p)),
                    feature,
                    &f.change_type,
                    (f.added, f.removed),
                    Some(p),
                ),
            };
            match band(p) {
                Band::Related => result.related.push(file),
                Band::Uncertain => result.uncertain.push(file),
                Band::Unrelated => result.unrelated.push(file),
            }
        }
    }
    Ok(result)
}

/// Why a file landed in its bucket — for the picker's per-file reason.
#[derive(Debug, PartialEq, Eq)]
enum Why {
    NameMatch,
    Band(Band),
}

/// The picker's per-file reason in plain words: what changed · why it's
/// grouped there ("edited · name matches “login”"). The model's probability is
/// appended only under GITRUST_DEBUG.
fn file_reason(
    why: Why,
    feature: &str,
    change: &crate::manifest::ChangeType,
    (added, removed): (usize, usize),
    p: Option<f32>,
) -> String {
    use crate::manifest::ChangeType as C;
    let what = match change {
        C::Added => "new",
        C::Modified => "edited",
        C::Deleted => "deleted",
        C::Renamed => "renamed",
    };
    let because = match why {
        Why::NameMatch => format!("name matches “{}”", feature),
        Why::Band(Band::Related) => format!("likely part of “{}”", feature),
        Why::Band(Band::Uncertain) => format!("might be part of “{}” — your call", feature),
        Why::Band(Band::Unrelated) => "looks unrelated".to_string(),
    };
    let dbg = match p {
        Some(p) if std::env::var("GITRUST_DEBUG").is_ok() => format!("  (p={:.2})", p),
        _ => String::new(),
    };
    let stat = match (added, removed) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{}", a),
        (0, r) => format!("−{}", r),
        (a, r) => format!("+{} −{}", a, r),
    };
    // Pad by characters ("−" is multi-byte), so the words line up.
    let pad = 8usize.saturating_sub(stat.chars().count());
    format!("{}{}  {} · {}{}", stat, " ".repeat(pad), what, because, dbg)
}

#[derive(Debug, PartialEq, Eq)]
enum Band {
    Related,
    Uncertain,
    Unrelated,
}

fn band(p: f32) -> Band {
    if p >= RELATED_AT {
        Band::Related
    } else if p <= UNRELATED_AT {
        Band::Unrelated
    } else {
        Band::Uncertain
    }
}

/// Validate explicitly named files exist and are modified in the repo.
pub fn validate_explicit_files(
    repo: &git2::Repository,
    files: &[String],
) -> (Vec<String>, Vec<FileValidationError>) {
    let statuses = match repo.statuses(None) {
        Ok(s) => s,
        Err(_) => return (vec![], vec![]),
    };
    let modified: HashSet<String> = statuses
        .iter()
        .filter_map(|e| e.path().map(String::from))
        .collect();

    let mut valid = Vec::new();
    let mut errors = Vec::new();

    for file in files {
        // Reject path traversal attempts
        if file.contains("..") || std::path::Path::new(file).is_absolute() {
            errors.push(FileValidationError::PathOutsideRepo(file.clone()));
            continue;
        }
        if !modified.contains(file.as_str()) {
            errors.push(FileValidationError::NotInRepo(file.clone()));
            continue;
        }
        if is_likely_binary(file) {
            errors.push(FileValidationError::BinaryFile(file.clone()));
            // Warn but still allow
        }
        valid.push(file.clone());
    }

    (valid, errors)
}

fn is_likely_binary(path: &str) -> bool {
    let binary_extensions = [
        ".png", ".jpg", ".jpeg", ".gif", ".ico", ".bmp", ".webp", ".zip", ".tar", ".gz", ".bz2",
        ".7z", ".exe", ".dll", ".so", ".dylib", ".pdf", ".doc", ".docx", ".woff", ".woff2", ".ttf",
        ".otf",
    ];
    let lower = path.to_lowercase();
    binary_extensions.iter().any(|ext| lower.ends_with(ext))
}

#[derive(Debug, Clone)]
pub enum FileValidationError {
    NotInRepo(String),
    PathOutsideRepo(String),
    BinaryFile(String),
}

impl std::fmt::Display for FileValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileValidationError::NotInRepo(p) => write!(f, "'{}' is not modified in the repo", p),
            FileValidationError::PathOutsideRepo(p) => {
                write!(f, "'{}' is outside the repository", p)
            }
            FileValidationError::BinaryFile(p) => write!(f, "'{}' appears to be a binary file", p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChangeType, FileManifest};
    use tempfile::TempDir;

    fn make_manifest(path: &str, snippet: &str) -> FileManifest {
        FileManifest {
            path: path.to_string(),
            change_type: ChangeType::Modified,
            diff_snippet: snippet.to_string(),
            line_count: snippet.lines().count(),
            added: 0,
            removed: 0,
        }
    }

    #[test]
    fn test_filename_match_basic() {
        let manifests = vec![
            make_manifest("src/login/page.tsx", "+login form"),
            make_manifest("src/utils/auth.rs", "+auth helper"),
            make_manifest("src/config.rs", "+config changes"),
        ];
        let matches = filename_match(&manifests, "login");
        assert_eq!(matches, vec!["src/login/page.tsx"]);
    }

    #[test]
    fn test_filename_match_case_insensitive() {
        let manifests = vec![
            make_manifest("src/Login/Page.tsx", "+form"),
            make_manifest("src/other.rs", "+stuff"),
        ];
        let matches = filename_match(&manifests, "login");
        assert_eq!(matches, vec!["src/Login/Page.tsx"]);
    }

    #[test]
    fn test_filename_match_multiple_words() {
        let manifests = vec![
            make_manifest("src/login/page.tsx", "+form"),
            make_manifest("src/pages/home.tsx", "+home page"),
            make_manifest("src/export/handler.rs", "+export"),
            make_manifest("src/config.rs", "+config"),
        ];
        let matches = filename_match(&manifests, "login page");
        assert_eq!(matches.len(), 2); // login/page.tsx and pages/home.tsx
        assert!(matches.contains(&"src/login/page.tsx".to_string()));
        assert!(matches.contains(&"src/pages/home.tsx".to_string()));
    }

    #[test]
    fn test_filename_match_no_results() {
        let manifests = vec![
            make_manifest("src/config.rs", "+config"),
            make_manifest("src/main.rs", "+main"),
        ];
        let matches = filename_match(&manifests, "authentication");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_validate_explicit_files() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();

        let (valid, errors) = validate_explicit_files(
            &repo,
            &["existing.txt".to_string(), "missing.txt".to_string()],
        );

        // existing.txt should be in status (untracked), missing.txt not
        assert!(valid.contains(&"existing.txt".to_string()));
        assert!(errors
            .iter()
            .any(|e| matches!(e, FileValidationError::NotInRepo(p) if p == "missing.txt")));
    }

    #[test]
    fn test_validate_rejects_path_traversal() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let (_, errors) = validate_explicit_files(&repo, &["../etc/passwd".to_string()]);
        assert!(errors
            .iter()
            .any(|e| matches!(e, FileValidationError::PathOutsideRepo(_))));
    }

    #[test]
    fn test_validate_warns_binary() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("image.png"), &[0u8; 10]).unwrap();

        let (valid, errors) = validate_explicit_files(&repo, &["image.png".to_string()]);
        // Binary files are valid but generate a warning
        assert!(valid.contains(&"image.png".to_string()));
        assert!(errors
            .iter()
            .any(|e| matches!(e, FileValidationError::BinaryFile(_))));
    }

    #[test]
    fn test_file_reasons_are_plain_language() {
        // The picker used to show "kev p=0.77" — a model internal. "+N −M"
        // comes first, padded so the words line up.
        let r = |w, c, n, p| file_reason(w, "login", c, n, p);
        assert_eq!(
            r(Why::NameMatch, &ChangeType::Modified, (3, 1), None),
            "+3 −1     edited · name matches “login”"
        );
        assert_eq!(
            r(
                Why::Band(Band::Related),
                &ChangeType::Added,
                (12, 0),
                Some(0.77)
            ),
            "+12       new · likely part of “login”"
        );
        assert_eq!(
            r(
                Why::Band(Band::Uncertain),
                &ChangeType::Modified,
                (120, 34),
                Some(0.63)
            ),
            "+120 −34  edited · might be part of “login” — your call"
        );
        assert_eq!(
            r(
                Why::Band(Band::Unrelated),
                &ChangeType::Deleted,
                (0, 7),
                Some(0.1)
            ),
            "−7        deleted · looks unrelated"
        );
    }

    #[test]
    fn test_band_thresholds() {
        assert_eq!(band(0.88), Band::Related);
        assert_eq!(band(RELATED_AT), Band::Related);
        assert_eq!(band(0.58), Band::Uncertain);
        assert_eq!(band(0.31), Band::Uncertain);
        assert_eq!(band(UNRELATED_AT), Band::Unrelated);
        assert_eq!(band(0.08), Band::Unrelated);
    }
}
