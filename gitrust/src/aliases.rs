//! Learned phrases (design handoff M8): when the user picks an option from one
//! of Kev's "which did you mean?" menus, the phrase → op mapping is saved in
//! the repo's `.git/gitrust/aliases.toml` and consulted after the exact-phrase
//! fast path, before the model. The same words then work instantly.
//!
//! Rules:
//! * Only ops outside `kev::DESTRUCTIVE_OPS` are learned — a destructive op
//!   always goes back through the model's checks and its own gates.
//! * The file is plain TOML, human-editable; `gitrust aliases` lists and
//!   forgets entries. A missing or unreadable file means "no aliases" (never an
//!   error); an entry naming an unknown op is ignored.
//! * Arguments are never stored: the op is rebuilt from the request by
//!   `build_from_op`, so captures and asks work exactly as for a Kev route.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct AliasFile {
    #[serde(default)]
    aliases: BTreeMap<String, String>,
}

const HEADER: &str = "# GiTrust learned phrases — checked after exact phrases, before the model.\n\
# Each line maps a request (lower-cased) to an operation. Edit or delete freely,\n\
# or use `gitrust aliases --forget \"<phrase>\"`.\n\n";

/// `<repo>/.git/gitrust/aliases.toml` for a repository's git dir.
pub fn path_for(git_dir: &Path) -> PathBuf {
    git_dir.join("gitrust").join("aliases.toml")
}

/// The form a phrase is stored and looked up in: trimmed, lower-cased, inner
/// whitespace collapsed, trailing ".?!" dropped.
pub fn normalize(phrase: &str) -> String {
    phrase
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches(['.', '?', '!'])
        .to_string()
}

fn read(path: &Path) -> AliasFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn write(path: &Path, file: &AliasFile) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(file)?;
    std::fs::write(path, format!("{}{}", HEADER, body))?;
    Ok(())
}

/// Whether `op` may be learned at all.
pub fn learnable(op: &str) -> bool {
    op != crate::kev::REJECT
        && !crate::kev::DESTRUCTIVE_OPS.contains(&op)
        && crate::kev::ops().iter().any(|(o, _)| o == op)
}

/// The learned op for a request, if any (and still a known, learnable op).
pub fn lookup(path: &Path, phrase: &str) -> Option<String> {
    let key = normalize(phrase);
    read(path)
        .aliases
        .get(&key)
        .filter(|op| learnable(op))
        .cloned()
}

/// Remember `phrase` → `op`. Returns false (and writes nothing) for ops that
/// are never learned or an empty phrase.
pub fn record(path: &Path, phrase: &str, op: &str) -> anyhow::Result<bool> {
    let key = normalize(phrase);
    if key.is_empty() || !learnable(op) {
        return Ok(false);
    }
    let mut file = read(path);
    if file.aliases.get(&key).map(String::as_str) == Some(op) {
        return Ok(false); // already known
    }
    file.aliases.insert(key, op.to_string());
    write(path, &file)?;
    Ok(true)
}

/// Forget one phrase; returns whether it was there.
pub fn forget(path: &Path, phrase: &str) -> anyhow::Result<bool> {
    let mut file = read(path);
    let removed = file.aliases.remove(&normalize(phrase)).is_some();
    if removed {
        write(path, &file)?;
    }
    Ok(removed)
}

/// Forget everything learned in this repo; returns how many were removed.
pub fn clear(path: &Path) -> anyhow::Result<usize> {
    let n = read(path).aliases.len();
    if n > 0 {
        write(path, &AliasFile::default())?;
    }
    Ok(n)
}

/// What `gitrust aliases` prints.
pub fn report(path: &Path) -> String {
    let all = list(path);
    if all.is_empty() {
        return "No learned phrases yet. When you pick an option from a \"which did you mean?\"\n\
                menu, GiTrust remembers the phrase here so it's instant next time."
            .to_string();
    }
    let width = all
        .iter()
        .map(|(p, _)| p.chars().count() + 2)
        .max()
        .unwrap_or(0);
    let mut out = String::from("Learned phrases in this repo (.git/gitrust/aliases.toml):\n");
    for (phrase, op) in &all {
        let title = crate::copy::op_label(op).map_or(op.as_str(), |(t, _)| t);
        let quoted = format!("“{}”", phrase);
        out.push_str(&format!(
            "  {:<width$}  →  {}\n",
            quoted,
            title,
            width = width
        ));
    }
    out.push_str(
        "Forget one: gitrust aliases --forget \"<phrase>\" · all: gitrust aliases --clear",
    );
    out
}

/// All learned phrases, sorted.
pub fn list(path: &Path) -> Vec<(String, String)> {
    read(path).aliases.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn normalizes_case_space_and_trailing_punctuation() {
        assert_eq!(
            normalize("  Put my work   up on GitHub? "),
            "put my work up on github"
        );
    }

    #[test]
    fn records_and_looks_up_and_forgets() {
        let d = TempDir::new().unwrap();
        let p = path_for(d.path());
        assert_eq!(lookup(&p, "put my work up on github"), None);
        assert!(record(&p, "Put my work up on GitHub?", "scoped_push").unwrap());
        assert_eq!(
            lookup(&p, "put my work up on github").as_deref(),
            Some("scoped_push")
        );
        assert!(
            !record(&p, "put my work up on github", "scoped_push").unwrap(),
            "already known"
        );
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# GiTrust learned phrases"), "{text}");
        assert!(
            text.contains("\"put my work up on github\" = \"scoped_push\""),
            "{text}"
        );
        assert_eq!(
            list(&p),
            vec![("put my work up on github".into(), "scoped_push".into())]
        );
        assert!(forget(&p, "PUT my work up on github").unwrap());
        assert_eq!(lookup(&p, "put my work up on github"), None);
    }

    #[test]
    fn destructive_and_unknown_ops_are_never_learned_or_used() {
        let d = TempDir::new().unwrap();
        let p = path_for(d.path());
        assert!(!record(&p, "nuke it", "cleanup").unwrap());
        assert!(!record(&p, "x", crate::kev::REJECT).unwrap());
        assert!(!p.exists(), "nothing written");
        // A hand-edited entry naming a destructive or unknown op is ignored.
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            "[aliases]\n\"tidy\" = \"cleanup\"\n\"zap\" = \"no_such_op\"\n",
        )
        .unwrap();
        assert_eq!(lookup(&p, "tidy"), None);
        assert_eq!(lookup(&p, "zap"), None);
    }

    #[test]
    fn report_lists_phrases_by_title_and_says_how_to_forget() {
        let d = TempDir::new().unwrap();
        let p = path_for(d.path());
        assert!(report(&p).starts_with("No learned phrases yet"));
        record(&p, "put my work up on github", "scoped_push").unwrap();
        let r = report(&p);
        assert!(
            r.contains("“put my work up on github”") && r.contains("Push only some changes"),
            "{r}"
        );
        assert!(r.contains("--forget"));
        assert_eq!(clear(&p).unwrap(), 1);
        assert!(list(&p).is_empty());
    }

    #[test]
    fn a_broken_file_means_no_aliases_not_an_error() {
        let d = TempDir::new().unwrap();
        let p = path_for(d.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "this is { not toml").unwrap();
        assert_eq!(lookup(&p, "anything"), None);
        assert!(list(&p).is_empty());
    }
}
