use std::collections::HashSet;

use anyhow::Context;

use crate::commands::{ClassificationResult, ClassifiedFile, ResolutionOutcome};
use crate::config::Config;
use crate::intent;
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

/// Default two-pass resolver (filename match + semantic LLM scan).
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

        // Pass 2: semantic LLM classification on unmatched files
        let semantic_result = if !unmatched.is_empty() {
            classify_manifest_via_llm(&unmatched, feature_hint, config).await?
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
            .map(|path| ClassifiedFile {
                path: path.clone(),
                reason: "filename match".to_string(),
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

/// Pass 2: semantic LLM classification via a single Ollama call.
async fn classify_manifest_via_llm(
    manifests: &[&FileManifest],
    feature: &str,
    config: &Config,
) -> anyhow::Result<ClassificationResult> {
    let manifest_block = build_classification_block(manifests);
    let prompt = format!(
        r#"A developer said: "push all changes related to {feature}".
Below are ALL modified files with their diffs.
Classify each file as related, uncertain, or unrelated to "{feature}".

Key rules:
- Look for SEMANTIC relevance — not just keyword matches in filename.
- A file adding a nav link to /export IS related to export functionality.
- A file registering a route IS related to that route's feature.
- If you cannot tell from the diff alone, classify as uncertain.

Return ONLY JSON — no prose:
{{
  "related":   [{{"index": N, "reason": "one sentence"}}],
  "uncertain": [{{"index": N, "reason": "one sentence"}}],
  "unrelated": [{{"index": N, "reason": "one sentence"}}]
}}

FILES:
{manifest_block}"#
    );

    let raw = intent::call_ollama(&prompt, "", config, 0.0)
        .await
        .context("Semantic file classification failed. Is ollama serve running?")?;
    let cleaned = intent::json_repair(&raw);

    parse_classification_response(&cleaned, manifests)
}

fn build_classification_block(manifests: &[&FileManifest]) -> String {
    let mut block = String::new();
    for (i, f) in manifests.iter().enumerate() {
        let snippet = if f.diff_snippet.len() > 2000 {
            format!(
                "({} lines — large diff, showing first 500 chars)\n{}",
                f.line_count,
                &f.diff_snippet[..500.min(f.diff_snippet.len())]
            )
        } else {
            f.diff_snippet.clone()
        };
        block += &format!("FILE {} [{:?}] {}\n{}\n---\n", i, f.change_type, f.path, snippet);
    }
    block
}

fn parse_classification_response(
    json_str: &str,
    manifests: &[&FileManifest],
) -> anyhow::Result<ClassificationResult> {
    let v: serde_json::Value =
        serde_json::from_str(json_str).context("LLM returned unparseable JSON for classification")?;

    fn extract_files(v: &serde_json::Value, key: &str, manifests: &[&FileManifest]) -> Vec<ClassifiedFile> {
        let mut result = Vec::new();
        if let Some(arr) = v[key].as_array() {
            for item in arr {
                let idx = item["index"].as_u64().unwrap_or(999) as usize;
                let reason = item["reason"]
                    .as_str()
                    .unwrap_or("no reason given")
                    .to_string();
                if idx < manifests.len() {
                    result.push(ClassifiedFile {
                        path: manifests[idx].path.clone(),
                        reason,
                    });
                }
            }
        }
        result
    }

    let related = extract_files(&v, "related", manifests);
    let uncertain = extract_files(&v, "uncertain", manifests);
    let unrelated = extract_files(&v, "unrelated", manifests);

    Ok(ClassificationResult {
        related,
        uncertain,
        unrelated,
    })
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
        ".png", ".jpg", ".jpeg", ".gif", ".ico", ".bmp", ".webp",
        ".zip", ".tar", ".gz", ".bz2", ".7z",
        ".exe", ".dll", ".so", ".dylib",
        ".pdf", ".doc", ".docx",
        ".woff", ".woff2", ".ttf", ".otf",
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

        let (valid, errors) =
            validate_explicit_files(&repo, &["existing.txt".to_string(), "missing.txt".to_string()]);

        // existing.txt should be in status (untracked), missing.txt not
        assert!(valid.contains(&"existing.txt".to_string()));
        assert!(errors.iter().any(|e| matches!(e, FileValidationError::NotInRepo(p) if p == "missing.txt")));
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
    fn test_parse_classification_response() {
        let manifests = vec![
            make_manifest("src/login.rs", "+login"),
            make_manifest("src/main.rs", "+route"),
            make_manifest("src/config.rs", "+config"),
        ];
        let refs: Vec<&FileManifest> = manifests.iter().collect();

        let json = r#"{
            "related": [{"index": 0, "reason": "login form"}, {"index": 1, "reason": "registers route"}],
            "uncertain": [{"index": 2, "reason": "might configure login"}],
            "unrelated": []
        }"#;

        let result = parse_classification_response(json, &refs).unwrap();
        assert_eq!(result.related.len(), 2);
        assert_eq!(result.uncertain.len(), 1);
        assert_eq!(result.unrelated.len(), 0);
        assert_eq!(result.related[0].path, "src/login.rs");
        assert_eq!(result.related[1].path, "src/main.rs");
    }
}
