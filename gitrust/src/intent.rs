use anyhow::Context;
use serde::Deserialize;

use crate::commands::{
    CommitMessage, FeaturePipelineOp, FileSelection, IntentResult, IntentType, ReadOnlyOp,
    ResetMode, ScopedWriteOp, SyncStrategy,
};
use crate::config::Config;
use crate::context::RepoContext;
use crate::prompts::{CLASSIFIER_PROMPT, COMMIT_MESSAGE_PROMPT, SYSTEM_PROMPT};

/// Trait for intent classification — enables swapping LLM backends.
pub trait IntentClassifier {
    fn classify(
        &self,
        context: &RepoContext,
        query: &str,
        config: &Config,
    ) -> impl std::future::Future<Output = anyhow::Result<IntentResult>> + Send;
}

/// Default Ollama-backed classifier.
pub struct OllamaClassifier;

impl IntentClassifier for OllamaClassifier {
    async fn classify(
        &self,
        context: &RepoContext,
        query: &str,
        config: &Config,
    ) -> anyhow::Result<IntentResult> {
        classify_intent(context, query, config).await
    }
}

// =============================================================================
// JSON wire format (what Ollama returns)
// =============================================================================

#[derive(Debug, Deserialize, Default)]
struct RawIntent {
    /// Model's brief justification for its choice. Consumed only for logging /
    /// debugging — never affects routing. Required by schema to force CoT.
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    intent: String,
    #[serde(default)]
    op: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    feature_hint: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    question: String,
    /// Some models use "reset_mode" for soft/mixed/hard
    #[serde(default)]
    reset_mode: String,
    /// Log count
    #[serde(default)]
    count: Option<u8>,
    /// Remote name for add_remote op
    #[serde(default)]
    remote_name: String,
    /// Remote URL for add_remote op
    #[serde(default)]
    remote_url: String,
}

// =============================================================================
// Fast-path canonical matcher
// =============================================================================

/// Match a query against a whitelist of canonical, unambiguous phrases.
/// Returns `Some` only on EXACT equality after trim + lowercase — never substring.
/// "status of branch" will NOT match "status".
pub fn try_canonical(query: &str) -> Option<IntentType> {
    // Amend-with-new-message needs the RAW query (message case matters), so it is
    // captured before the lowercase whitelist match (M-20).
    if let Some(intent) = capture_amend_message(query) {
        return Some(intent);
    }
    let q = query.trim().to_lowercase();
    let intent = match q.as_str() {
        "status" | "st" => IntentType::ReadOnly(ReadOnlyOp::Status),
        "log" => IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }),
        "diff" => IntentType::ReadOnly(ReadOnlyOp::Diff { path: None }),
        "remotes" | "remote" | "remote -v" => IntentType::ReadOnly(ReadOnlyOp::Remotes),
        "branch" | "branches" | "list branches" | "show branches" | "current branch" => {
            IntentType::ReadOnly(ReadOnlyOp::Branches)
        }
        "config" | "user" | "whoami" => IntentType::ReadOnly(ReadOnlyOp::Config),
        "push" | "publish" => IntentType::ScopedWrite(ScopedWriteOp::Push {
            remote: "origin".to_string(),
        }),
        "pull" => IntentType::ScopedWrite(ScopedWriteOp::Pull {
            remote: "origin".to_string(),
        }),
        "stash" => IntentType::ScopedWrite(ScopedWriteOp::Stash),
        "stash pop" | "pop" => IntentType::ScopedWrite(ScopedWriteOp::StashPop),
        "show" | "show head" | "last commit" | "commit details" | "last commit details" => {
            IntentType::ReadOnly(ReadOnlyOp::Show { target: None })
        }
        // All undo-language classifies to the ambiguous UndoIntent (P6). The
        // deterministic ChoiceGate — never the classifier — picks the semantic.
        // No arm here emits `ScopedWriteOp::Undo` directly.
        "undo" | "undo last commit" | "undo commit" | "take back" | "take it back"
        | "that was wrong" | "undo that" => IntentType::ScopedWrite(ScopedWriteOp::UndoIntent),
        // Repair vocabulary (Phase 13 — S32/S38/S39). oid/paths resolve at build.
        "revert" | "revert last commit" | "revert that" | "revert commit" => {
            IntentType::ScopedWrite(ScopedWriteOp::Revert { oid: String::new() })
        }
        "unstage" | "unstage all" | "unstage everything" | "unstage commit" => {
            IntentType::ScopedWrite(ScopedWriteOp::Unstage { paths: vec![] })
        }
        "amend" | "amend commit" | "amend last commit" => {
            IntentType::ScopedWrite(ScopedWriteOp::Amend {
                message: None,
                include_staged: true,
            })
        }
        // Forensic recovery (Phase 14 — S24/26/27). Distinct from `gitrust --recover`
        // (the snapshot path): this hunts reflogs + dangling objects for lost
        // commits and restores to a rescue branch.
        "find my lost work"
        | "find lost work"
        | "i lost a commit"
        | "i lost my work"
        | "lost commit"
        | "lost commits"
        | "recover lost work"
        | "recover lost commit"
        | "find lost commits" => IntentType::ScopedWrite(ScopedWriteOp::ForensicRecover),
        // Destructive-phrase routing (Phase 14 — S62). Deterministic table (NOT
        // the LLM) maps innocent-sounding cleanup language to the ambiguous
        // CleanupIntent; a ChoiceGate then spells out what would be deleted. No
        // arm emits a concrete Clean/Reset::Hard — the gate decides.
        "clean up"
        | "clean up my repo"
        | "clean up repo"
        | "start fresh"
        | "get rid of my changes"
        | "get rid of these changes"
        | "throw away my changes"
        | "throw away changes"
        | "discard everything"
        | "discard all my changes"
        | "nuke my changes" => IntentType::ScopedWrite(ScopedWriteOp::CleanupIntent),
        // Remote-sync queries — fetch first so ahead/behind is actually fresh.
        "fetch"
        | "check remote"
        | "check upstream"
        | "am i up to date"
        | "is my branch up to date"
        | "is local up to date"
        | "is local branch up to date"
        | "is my local branch up to date"
        | "is my local branch upto date"
        | "is my branch upto date"
        | "am i upto date"
        | "sync check"
        | "check sync" => IntentType::ReadOnly(ReadOnlyOp::FetchStatus {
            remote: "origin".to_string(),
        }),
        // Merge-conflict prediction
        "merge preview"
        | "preview merge"
        | "will pull conflict"
        | "will pulling conflict"
        | "any conflicts"
        | "predict merge"
        | "predict conflicts"
        | "safe to pull"
        | "is it safe to pull"
        | "check conflicts"
        | "check merge conflicts"
        | "will i have conflicts"
        | "will there be conflicts" => IntentType::ReadOnly(ReadOnlyOp::MergePreview {
            remote: "origin".to_string(),
        }),
        // Pull --rebase
        "rebase" | "pull rebase" | "pull with rebase" | "rebase onto origin"
        | "rebase my commits" => IntentType::ScopedWrite(ScopedWriteOp::PullRebase {
            remote: "origin".to_string(),
        }),
        // Force push (with lease)
        "force push"
        | "force-push"
        | "force push with lease"
        | "force-with-lease"
        | "overwrite remote" => IntentType::ScopedWrite(ScopedWriteOp::ForcePush {
            remote: "origin".to_string(),
        }),
        // Discard local commits
        "discard local"
        | "discard my commits"
        | "discard local commits"
        | "reset to origin"
        | "reset hard to origin"
        | "drop local commits"
        | "abandon local" => IntentType::ScopedWrite(ScopedWriteOp::DiscardLocal {
            remote: "origin".to_string(),
        }),
        // In-progress operation continue/skip/abort (Phase 11). Operation-
        // qualified phrases only — unambiguous, and safe to whitelist because
        // the situation gate keeps them valid ONLY in their matching Mid* state.
        "abort rebase" | "rebase abort" | "cancel rebase" => {
            IntentType::ScopedWrite(ScopedWriteOp::RebaseAbort)
        }
        "continue rebase" | "rebase continue" => {
            IntentType::ScopedWrite(ScopedWriteOp::RebaseContinue)
        }
        "skip rebase" | "rebase skip" | "skip this commit" => {
            IntentType::ScopedWrite(ScopedWriteOp::RebaseSkip)
        }
        "abort merge" | "merge abort" | "cancel merge" => {
            IntentType::ScopedWrite(ScopedWriteOp::MergeAbort)
        }
        "abort cherry-pick" | "abort cherrypick" | "cherry-pick abort" | "cancel cherry-pick" => {
            IntentType::ScopedWrite(ScopedWriteOp::CherryPickAbort)
        }
        "continue cherry-pick" | "continue cherrypick" | "cherry-pick continue" => {
            IntentType::ScopedWrite(ScopedWriteOp::CherryPickContinue)
        }
        "abort revert" | "revert abort" | "cancel revert" => {
            IntentType::ScopedWrite(ScopedWriteOp::RevertAbort)
        }
        "continue revert" | "revert continue" => {
            IntentType::ScopedWrite(ScopedWriteOp::RevertContinue)
        }
        // Parameterized fast-path forms: a known verb prefix + a trailing
        // argument the LLM used to drop (M-05). Deterministic capture keeps
        // these off the model entirely — no latency, no lost target. The name
        // is validated as a single safe token; anything richer falls through to
        // the LLM (returns None).
        _ => return try_canonical_parameterized(&q),
    };
    Some(intent)
}

/// Prefix-capture forms of `try_canonical` — a canonical verb followed by a
/// single-token argument (a branch name). Returns `Some` only when the trailing
/// argument is exactly one whitespace-delimited, git-legal token; otherwise
/// `None` so the query falls through to the LLM.
fn try_canonical_parameterized(q: &str) -> Option<IntentType> {
    // (prefixes, build) — first matching prefix wins. Longest/most-specific
    // prefixes first so "switch to" beats a hypothetical "switch".
    let create_branch: &[&str] = &[
        "create branch ",
        "create a branch ",
        "new branch ",
        "make branch ",
        "make a branch ",
        "branch ", // "branch foo" (bare "branch" is a read-only list, matched above)
    ];
    let checkout: &[&str] = &[
        "checkout ",
        "check out ",
        "switch to ",
        "switch to branch ",
        "switch ",
    ];

    if let Some(name) = capture_single_arg(q, create_branch) {
        return Some(IntentType::ScopedWrite(ScopedWriteOp::Branch { name }));
    }
    if let Some(target) = capture_single_arg(q, checkout) {
        return Some(IntentType::ScopedWrite(ScopedWriteOp::Checkout { target }));
    }
    if let Some(n) = capture_squash_count(q) {
        return Some(IntentType::ScopedWrite(ScopedWriteOp::SquashLastN { n }));
    }
    if let Some(intent) = capture_sync(q) {
        return Some(intent);
    }
    if let Some(hint) = capture_scoped_push(q) {
        return Some(IntentType::FeaturePipeline(
            FeaturePipelineOp::AddCommitPush { feature_hint: hint },
        ));
    }
    // Note: amend-with-new-message uses the RAW (case-preserving) query, so it is
    // handled in try_canonical against `query`, not here (q is lowercased).
    None
}

/// Capture "fix/change/update the [last] commit message to '<new message>'" and
/// its variants, extracting the quoted (or trailing) new message (M-20). Operates
/// on the RAW query so the message keeps its original case. Routes to a
/// message-only Amend; GiTrust's amend flow then handles the pushed-commit /
/// force-with-lease escalation. Returns None when no new message is present
/// (bare "amend" is already a canonical phrase).
fn capture_amend_message(raw: &str) -> Option<IntentType> {
    let lower = raw.trim().to_lowercase();
    // Must be a "change the commit message" phrasing.
    let is_msg_change = (lower.starts_with("fix ")
        || lower.starts_with("change ")
        || lower.starts_with("update ")
        || lower.starts_with("reword ")
        || lower.starts_with("rename ")
        || lower.starts_with("edit "))
        && lower.contains("commit message")
        && (lower.contains(" to ") || lower.contains(":"));
    if !is_msg_change {
        return None;
    }
    // Prefer a quoted message; fall back to text after " to " / " to:" / ":".
    let msg = extract_quoted(raw).or_else(|| {
        // Find the delimiter that introduces the new message, case-preserving.
        for delim in [" to: ", " to ", ": ", ":"] {
            if let Some(i) = raw.to_lowercase().rfind(delim) {
                let after = raw[i + delim.len()..]
                    .trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string();
                if !after.is_empty() {
                    return Some(after);
                }
            }
        }
        None
    })?;
    if msg.is_empty() {
        return None;
    }
    Some(IntentType::ScopedWrite(ScopedWriteOp::Amend {
        message: Some(msg),
        include_staged: false,
    }))
}

/// Extract the first single- or double-quoted substring from `s`, if any.
fn extract_quoted(s: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        if let Some(start) = s.find(quote) {
            if let Some(rel_end) = s[start + 1..].find(quote) {
                let inner = &s[start + 1..start + 1 + rel_end];
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
        }
    }
    None
}

/// Deterministically capture "push [the] <TOPIC> changes/files/…" and route to
/// the scoped feature pipeline with <TOPIC> as the resolver hint (M-04). The 3B
/// model reliably collapses this to a plain push regardless of prompt wording, so
/// the topic scope has to be extracted here. Only fires on the "<TOPIC> <noun>"
/// shape where <noun> marks a filter (changes/files/work/fixes/edits/stuff);
/// possessive "my …" and bare "push" fall through to the LLM/plain push.
fn capture_scoped_push(q: &str) -> Option<String> {
    let rest = q.strip_prefix("push ")?;
    // A possessive ("push my …") is the user's own work, not a topic filter.
    if rest.starts_with("my ") {
        return None;
    }
    let rest = rest.strip_prefix("the ").unwrap_or(rest);
    // Split into words; need "<topic> <filter-noun>" (exactly the scoping shape).
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.len() != 2 {
        return None;
    }
    let topic = words[0];
    let noun = words[1];
    let filter_nouns = [
        "changes", "change", "files", "file", "work", "fixes", "fix", "edits", "edit", "stuff",
        "feature", "updates", "code",
    ];
    // The topic must itself be a plain word (not a filter noun like "my"/"all").
    let topic_is_word = topic
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !filter_nouns.contains(&topic)
        && !matches!(
            topic,
            "all" | "everything" | "some" | "these" | "those" | "any"
        );
    if filter_nouns.contains(&noun) && topic_is_word {
        Some(topic.to_string())
    } else {
        None
    }
}

/// Deterministically route "sync with <branch>" / "sync my branch with main" and
/// close variants to the Sync feature-pipeline (M-10). The small model kept
/// reading these as a read-only *status* — a dangerous misclassification for what
/// is actually a rebase/merge write. The Sync flow itself presents a rebase-vs-
/// merge ChoiceGate; the strategy here is just the default the gate opens with
/// (explicit "rebase" in the phrase preselects Rebase).
fn capture_sync(q: &str) -> Option<IntentType> {
    // Require an explicit target ("... with/onto/against <branch>"). A bare
    // "sync" is ambiguous — leave it to the LLM/clarify. "sync check" / "check
    // sync" are fetch-status canonicals already matched before we get here.
    let is_sync = q.starts_with("sync ")
        && (q.contains(" with ") || q.contains(" onto ") || q.contains(" against "));
    // Also accept "update my branch with main" / "bring in main".
    let is_update_with = (q.starts_with("update ") || q.starts_with("bring in "))
        && (q.contains(" with ") || q.contains(" from "));
    if !(is_sync || is_update_with) {
        return None;
    }
    let strategy = if q.contains("rebase") {
        SyncStrategy::Rebase
    } else {
        SyncStrategy::Merge
    };
    Some(IntentType::FeaturePipeline(FeaturePipelineOp::Sync {
        strategy,
    }))
}

/// Deterministically capture `squash [the] last N [commits]` and its common
/// variants, extracting N (M-19: the small model was unreliable at routing
/// "squash the last 3" to the squash op with count=3, even though the prompt
/// asks for it). Requires an explicit count ≥ 2; a squash phrase without a
/// number falls through to the LLM. Returns `None` for anything not clearly a
/// squash request.
fn capture_squash_count(q: &str) -> Option<u32> {
    // Must start with a squash/combine verb so we don't grab a stray number from
    // an unrelated phrase.
    let looks_like_squash = q.starts_with("squash") || q.starts_with("combine");
    if !looks_like_squash {
        return None;
    }
    // Only fire on squash phrasings that reference "commit(s)" or "last N" — keep
    // the surface area tight; richer phrasings defer to the LLM.
    let mentions_target = q.contains("last")
        || q.contains("commit")
        || q.contains("into one")
        || q.contains(" to one");
    if !mentions_target {
        return None;
    }
    // First integer token in the phrase is the count.
    let n: u32 = q
        .split(|c: char| !c.is_ascii_digit())
        .find(|tok| !tok.is_empty())
        .and_then(|tok| tok.parse().ok())?;
    // A 0/1-commit squash is meaningless; require ≥ 2. (n==1 or n==0 → defer,
    // the LLM/clarifier can ask.)
    if n >= 2 {
        Some(n)
    } else {
        None
    }
}

/// Strip the first matching prefix from `q` and return the remaining argument
/// iff it is exactly ONE git-legal token (no spaces, no shell/quote noise, not a
/// flag). Multi-word or quoted arguments return `None` — they need the LLM.
fn capture_single_arg(q: &str, prefixes: &[&str]) -> Option<String> {
    for p in prefixes {
        if let Some(rest) = q.strip_prefix(p) {
            let arg = rest.trim();
            if is_single_git_ref_token(arg) {
                return Some(arg.to_string());
            }
        }
    }
    None
}

/// A conservative single-token check: non-empty, no whitespace, and only
/// characters that are plausible in a branch/ref name. Deliberately strict —
/// the goal is to catch the common `create branch feature-x` case with zero
/// false positives, deferring anything ambiguous to the LLM.
fn is_single_git_ref_token(s: &str) -> bool {
    if s.is_empty() || s.contains(char::is_whitespace) {
        return false;
    }
    if s.starts_with('-') {
        return false; // not a flag
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
}

/// Build a short list of plausible alternative intents when classifier confidence
/// is mid-range. Hand-rolled rules — NOT an LLM call.
/// Returns at most 2 alternates distinct from `top`.
pub fn alternates_for(top: &IntentType, ctx: &RepoContext) -> Vec<IntentType> {
    let mut alts: Vec<IntentType> = Vec::new();
    let has_unstaged = !ctx.status.modified.is_empty() || !ctx.status.untracked.is_empty();
    let has_staged = !ctx.status.staged.is_empty();

    match top {
        // Top is push — user might have meant commit-then-push or feature pipeline.
        IntentType::ScopedWrite(ScopedWriteOp::Push { .. }) => {
            if has_unstaged {
                alts.push(IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
                    message: CommitMessage::Draft("update".to_string()),
                    files: FileSelection::All,
                }));
            }
            if has_staged {
                alts.push(IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
                    message: CommitMessage::Draft("update".to_string()),
                }));
            }
        }
        // Top is commit-only — user might want to also stage first.
        IntentType::ScopedWrite(ScopedWriteOp::Commit { .. }) => {
            if has_unstaged {
                alts.push(IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
                    message: CommitMessage::Draft("update".to_string()),
                    files: FileSelection::All,
                }));
            }
        }
        // Top is status — user might want branches or log instead.
        IntentType::ReadOnly(ReadOnlyOp::Status) => {
            alts.push(IntentType::ReadOnly(ReadOnlyOp::Branches));
            alts.push(IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }));
        }
        // Top is branches — user might want status.
        IntentType::ReadOnly(ReadOnlyOp::Branches) => {
            alts.push(IntentType::ReadOnly(ReadOnlyOp::Status));
        }
        _ => {}
    }

    alts.truncate(2);
    alts
}

/// One-line human-readable label for a candidate intent (used in menus).
pub fn intent_label(intent: &IntentType) -> String {
    match intent {
        IntentType::ReadOnly(op) => format!("Show {:?}", op).to_lowercase(),
        IntentType::ScopedWrite(ScopedWriteOp::Push { remote }) => {
            format!("Push current branch to {}", remote)
        }
        IntentType::ScopedWrite(ScopedWriteOp::Commit { .. }) => {
            "Commit already-staged files".to_string()
        }
        IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit { .. }) => {
            "Stage all changes and commit".to_string()
        }
        IntentType::ScopedWrite(ScopedWriteOp::Pull { remote }) => {
            format!("Pull from {}", remote)
        }
        IntentType::ScopedWrite(ScopedWriteOp::Init) => "Initialize repository".to_string(),
        IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush { .. }) => {
            "Commit staged files and push".to_string()
        }
        IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush { feature_hint }) => {
            format!("Push '{}' changes (file selection)", feature_hint)
        }
        IntentType::FeaturePipeline(FeaturePipelineOp::Sync { .. }) => {
            "Sync with upstream".to_string()
        }
        IntentType::NeedsClarification(q) => format!("Rephrase: {}", q),
        other => format!("{:?}", other),
    }
}

// =============================================================================
// Public API
// =============================================================================

/// Classify user intent by calling Ollama. Returns `NeedsClarification`
/// when confidence < threshold or when intent = "clarify".
pub async fn classify_intent(
    context: &RepoContext,
    query: &str,
    config: &Config,
) -> anyhow::Result<IntentResult> {
    let context_block = context.to_prompt_string();
    let prompt = format!(
        "{}\n\nCurrent repo state:\n{}\n\nUser query: {}",
        *CLASSIFIER_PROMPT, context_block, query
    );

    let raw =
        call_ollama_with_schema(&prompt, &SYSTEM_PROMPT, config, 0.0, classifier_schema()).await?;
    let cleaned = json_repair(&raw);

    let ri: RawIntent = serde_json::from_str(&cleaned)
        .with_context(|| format!("LLM returned unparseable JSON: {}", cleaned))?;

    const CONFIDENCE_THRESHOLD: f32 = 0.6;

    if ri.intent == "clarify" || ri.confidence < CONFIDENCE_THRESHOLD {
        let question = if ri.question.is_empty() {
            "I'm not sure what you want to do — could you rephrase?".to_string()
        } else {
            ri.question.clone()
        };
        return Ok(IntentResult {
            intent: IntentType::NeedsClarification(question),
            confidence: ri.confidence,
            raw_query: query.to_string(),
            engine: crate::commands::Engine::Llm,
        });
    }

    let intent = build_intent_type(&ri, query)?;

    // Surface the model's reasoning to stderr when GITRUST_DEBUG is set, or
    // when confidence is mid-range (helps the user understand the disambiguation menu).
    let debug_on = std::env::var("GITRUST_DEBUG").is_ok();
    if debug_on && !ri.reasoning.is_empty() {
        eprintln!("  (reasoning: {})", ri.reasoning);
    }

    Ok(IntentResult {
        intent,
        confidence: ri.confidence,
        raw_query: query.to_string(),
        engine: crate::commands::Engine::Llm,
    })
}

/// Build an `IntentType` from the parsed Ollama JSON.
fn build_intent_type(ri: &RawIntent, query: &str) -> anyhow::Result<IntentType> {
    match ri.intent.as_str() {
        "read_only" => {
            let op = match ri.op.as_str() {
                "log" => ReadOnlyOp::Log {
                    count: ri.count.unwrap_or(10),
                },
                "diff" => ReadOnlyOp::Diff {
                    path: if ri.files.is_empty() {
                        None
                    } else {
                        Some(ri.files[0].clone())
                    },
                },
                "remotes" => ReadOnlyOp::Remotes,
                "branches" => ReadOnlyOp::Branches,
                "config" => ReadOnlyOp::Config,
                "show" => ReadOnlyOp::Show {
                    target: if ri.feature_hint.is_empty() {
                        None
                    } else {
                        Some(ri.feature_hint.clone())
                    },
                },
                "fetch" | "fetch_status" => ReadOnlyOp::FetchStatus {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                "merge_preview" | "predict_merge" | "preview_merge" => ReadOnlyOp::MergePreview {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                _ => ReadOnlyOp::Status,
            };
            Ok(IntentType::ReadOnly(op))
        }

        "scoped_write" => {
            let op = match ri.op.as_str() {
                "add" => ScopedWriteOp::Add {
                    files: if ri.files.is_empty() {
                        FileSelection::All
                    } else {
                        FileSelection::Explicit(ri.files.clone())
                    },
                },
                "commit" => ScopedWriteOp::Commit {
                    message: if ri.message.is_empty() {
                        CommitMessage::Draft(derive_draft_message(query))
                    } else {
                        CommitMessage::Provided(ri.message.clone())
                    },
                },
                "branch" => ScopedWriteOp::Branch {
                    name: ri.feature_hint.clone(),
                },
                "checkout" => ScopedWriteOp::Checkout {
                    target: ri.feature_hint.clone(),
                },
                "stash" => ScopedWriteOp::Stash,
                "stash_pop" => ScopedWriteOp::StashPop,
                "reset" => ScopedWriteOp::Reset {
                    mode: parse_reset_mode(&ri.reset_mode),
                },
                "pull" => ScopedWriteOp::Pull {
                    remote: "origin".to_string(),
                },
                "init" => ScopedWriteOp::Init,
                "delete_repo" => ScopedWriteOp::DeleteRepo,
                // P6: undo-language is ambiguous → UndoIntent, never a concrete
                // undo semantic. The ChoiceGate decides.
                "undo" => ScopedWriteOp::UndoIntent,
                // Phase 14: forensic recovery (find/restore lost commits) and the
                // ambiguous destructive-cleanup intent (S62 — the ChoiceGate, not
                // the LLM, picks what to delete). Neither emits a concrete
                // destructive step here.
                "find_lost_work" | "forensic_recover" | "recover_lost" => {
                    ScopedWriteOp::ForensicRecover
                }
                "cleanup" | "clean_up" | "start_fresh" | "discard_changes" => {
                    ScopedWriteOp::CleanupIntent
                }
                "amend" => ScopedWriteOp::Amend {
                    message: if ri.message.is_empty() {
                        None
                    } else {
                        Some(ri.message.clone())
                    },
                    include_staged: true,
                },
                "revert" => ScopedWriteOp::Revert {
                    // Empty = HEAD (resolved at build time). A specific target in
                    // feature_hint resolves there too.
                    oid: ri.feature_hint.clone(),
                },
                "unstage" => ScopedWriteOp::Unstage {
                    paths: ri.files.clone(),
                },
                "set_upstream" => ScopedWriteOp::SetUpstream {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                    // Empty = current branch (resolved at build time).
                    branch: ri.feature_hint.clone(),
                },
                "squash" => ScopedWriteOp::SquashLastN {
                    n: ri.count.unwrap_or(2) as u32,
                },
                "pull_rebase" | "rebase" => ScopedWriteOp::PullRebase {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                "force_push" | "force_push_with_lease" => ScopedWriteOp::ForcePush {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                "discard_local" | "reset_to_remote" => ScopedWriteOp::DiscardLocal {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                "add_commit" => ScopedWriteOp::AddAndCommit {
                    message: if ri.message.is_empty() {
                        CommitMessage::Draft(derive_draft_message(query))
                    } else {
                        CommitMessage::Provided(ri.message.clone())
                    },
                    files: if ri.files.is_empty() {
                        FileSelection::All
                    } else {
                        FileSelection::Explicit(ri.files.clone())
                    },
                },
                "remove_remote" => ScopedWriteOp::RemoveRemote {
                    name: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                "push" => ScopedWriteOp::Push {
                    remote: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                },
                "add_remote" => ScopedWriteOp::AddRemote {
                    name: if ri.remote_name.is_empty() {
                        "origin".to_string()
                    } else {
                        ri.remote_name.clone()
                    },
                    url: ri.remote_url.clone(),
                },
                // In-progress operation continue/skip/abort (Phase 11).
                "rebase_continue" => ScopedWriteOp::RebaseContinue,
                "rebase_skip" => ScopedWriteOp::RebaseSkip,
                "rebase_abort" => ScopedWriteOp::RebaseAbort,
                "merge_abort" => ScopedWriteOp::MergeAbort,
                "cherry_pick_continue" | "cherrypick_continue" => ScopedWriteOp::CherryPickContinue,
                "cherry_pick_abort" | "cherrypick_abort" => ScopedWriteOp::CherryPickAbort,
                "revert_continue" => ScopedWriteOp::RevertContinue,
                "revert_abort" => ScopedWriteOp::RevertAbort,
                _ => ScopedWriteOp::Add {
                    files: FileSelection::All,
                },
            };
            Ok(IntentType::ScopedWrite(op))
        }

        "feature_pipeline" => {
            let op = match ri.op.as_str() {
                "push" => FeaturePipelineOp::AddCommitPush {
                    feature_hint: ri.feature_hint.clone(),
                },
                "sync" => FeaturePipelineOp::Sync {
                    strategy: if query.to_lowercase().contains("rebase") {
                        SyncStrategy::Rebase
                    } else {
                        SyncStrategy::Merge
                    },
                },
                _ => {
                    // "commit" op in a feature_pipeline → CommitPush
                    let msg = if ri.message.is_empty() {
                        CommitMessage::Draft(derive_draft_message(query))
                    } else {
                        CommitMessage::Provided(ri.message.clone())
                    };
                    FeaturePipelineOp::CommitPush { message: msg }
                }
            };
            Ok(IntentType::FeaturePipeline(op))
        }

        "clarify" => Ok(IntentType::NeedsClarification(ri.question.clone())),

        // Some models return "init" as a top-level intent instead of scoped_write+op
        "init" => Ok(IntentType::ScopedWrite(ScopedWriteOp::Init)),

        other => Err(anyhow::anyhow!("Unknown intent type from LLM: '{}'", other)),
    }
}

fn parse_reset_mode(s: &str) -> ResetMode {
    match s.to_lowercase().as_str() {
        "hard" => ResetMode::Hard,
        "mixed" => ResetMode::Mixed,
        _ => ResetMode::Soft,
    }
}

/// Derive a rough draft commit message from the raw user query.
fn derive_draft_message(query: &str) -> String {
    // Remove common verbs at the start
    let cleaned = query
        .trim()
        .trim_start_matches(|c: char| c.is_whitespace())
        .to_string();

    let prefixes = ["commit ", "push ", "save ", "add "];
    let mut msg = cleaned.as_str();
    for p in &prefixes {
        msg = msg.strip_prefix(p).unwrap_or(msg);
    }

    // Capitalize first letter
    let mut chars = msg.chars();
    match chars.next() {
        None => "update files".to_string(),
        Some(first) => {
            let capitalized = first.to_uppercase().to_string() + chars.as_str();
            // "…" is 3 UTF-8 bytes; keep total byte length ≤ 73
            if capitalized.len() > 72 {
                format!("{}…", &capitalized[..70])
            } else {
                capitalized
            }
        }
    }
}

// =============================================================================
// Commit-message generation from diff
// =============================================================================

/// Cap on the diff bytes we feed to the model — keeps the prompt small for 3B.
const MAX_DIFF_BYTES_FOR_MSG: usize = 6000;

/// Build a compact patch text describing the changes that would be committed.
/// `include_workdir=true` covers both staged AND unstaged changes (used by add_commit).
/// `include_workdir=false` covers only the index vs HEAD (used by plain commit).
fn build_diff_for_message(
    repo: &git2::Repository,
    include_workdir: bool,
) -> anyhow::Result<String> {
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

    let mut diff_opts = git2::DiffOptions::new();
    diff_opts.context_lines(2);

    let diff = if include_workdir {
        repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut diff_opts))?
    } else {
        repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts))?
    };

    let mut buf = String::new();
    let mut bytes_so_far = 0usize;
    let mut truncated = false;

    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        if bytes_so_far >= MAX_DIFF_BYTES_FOR_MSG {
            truncated = true;
            return true;
        }
        let prefix = match line.origin() {
            '+' => "+",
            '-' => "-",
            _ => " ",
        };
        if let Ok(content) = std::str::from_utf8(line.content()) {
            buf.push_str(prefix);
            buf.push_str(content);
            bytes_so_far += content.len() + 1;
        }
        true
    })?;

    if truncated {
        buf.push_str("\n(diff truncated)\n");
    }
    Ok(buf)
}

/// Ask the LLM for a one-line conventional-commit message based on the diff.
/// Falls back to a generic message on any failure.
pub async fn generate_commit_message(
    repo: &git2::Repository,
    include_workdir: bool,
    config: &Config,
) -> String {
    let diff_text = match build_diff_for_message(repo, include_workdir) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return "update files".to_string(),
    };

    let prompt = format!("{}\n\nChanges:\n{}", *COMMIT_MESSAGE_PROMPT, diff_text);

    // Bare-text mode — must NOT use call_ollama (which forces format:"json").
    let raw = match call_ollama_text(&prompt, "", config, 0.2).await {
        Ok(r) => r,
        Err(_) => return "update files".to_string(),
    };

    // Pick the first line that actually looks like a commit message.
    // Skip empty lines, markdown fences, and JSON noise ({}, [], "...":).
    let candidate = raw
        .lines()
        .map(|l| l.trim())
        .find(|l| {
            !l.is_empty()
                && !l.starts_with("```")
                && !l.starts_with('{')
                && !l.starts_with('}')
                && !l.starts_with('[')
                && !l.starts_with(']')
                // Reject lines that look like JSON key-value: "message":
                && !(l.starts_with('"') && l.contains("\":"))
        })
        .unwrap_or("update files");

    let cleaned = candidate
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim_matches(|c: char| c == '"' || c == '`' || c == '\'')
        .trim();

    if cleaned.is_empty() {
        return "update files".to_string();
    }

    if cleaned.len() > 72 {
        // Truncate at byte boundary safely.
        let mut end = 70;
        while !cleaned.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        return format!("{}…", &cleaned[..end]);
    }
    cleaned.to_string()
}

// =============================================================================
// Ollama HTTP client
// =============================================================================

/// Build the JSON schema that constrains the classifier output.
/// Ollama (>= v0.5) decodes against this schema at the token level,
/// making it impossible to return invalid enum values or missing required fields.
fn classifier_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            // Property order MATTERS — llama.cpp's grammar generator emits
            // properties in declaration order. Forcing "reasoning" first means
            // the model must commit to a brief justification before picking
            // an intent. Costs ~50 tokens, materially improves accuracy on
            // ambiguous queries.
            "reasoning": {
                "type": "string",
                "minLength": 10,
                "maxLength": 200
            },
            "intent": {
                "type": "string",
                "enum": ["read_only", "scoped_write", "feature_pipeline", "clarify"]
            },
            "op": {
                "type": "string",
                "enum": [
                    "status", "log", "diff", "remotes", "branches", "config",
                    "add", "commit", "add_commit", "branch", "checkout", "stash",
                    "stash_pop", "reset", "pull", "push", "sync", "init",
                    "add_remote", "remove_remote", "delete_repo", "show", "undo",
                    "fetch", "merge_preview",
                    "pull_rebase", "rebase", "force_push", "force_push_with_lease",
                    "discard_local", "reset_to_remote",
                    "amend", "revert", "unstage", "set_upstream", "squash",
                    ""
                ]
            },
            "files": { "type": "array", "items": { "type": "string" } },
            "feature_hint": { "type": "string" },
            "message": { "type": "string" },
            "remote_name": { "type": "string" },
            "remote_url": { "type": "string" },
            "reset_mode": { "type": "string", "enum": ["soft", "mixed", "hard", ""] },
            "count": { "type": "integer", "minimum": 1, "maximum": 100 },
            "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "question": { "type": "string" }
        },
        "required": ["reasoning", "intent", "confidence"]
    })
}

/// Call Ollama with a strict JSON schema, falling back to free-form JSON
/// if the server rejects the schema (older Ollama versions).
pub async fn call_ollama_with_schema(
    prompt: &str,
    system: &str,
    config: &Config,
    temperature: f32,
    schema: serde_json::Value,
) -> anyhow::Result<String> {
    match call_ollama_inner(prompt, system, config, temperature, schema).await {
        Ok(text) => Ok(text),
        Err(_) => {
            // Older Ollama versions reject schema with 4xx — retry with free-form JSON.
            call_ollama(prompt, system, config, temperature).await
        }
    }
}

async fn call_ollama_inner(
    prompt: &str,
    system: &str,
    config: &Config,
    temperature: f32,
    format: serde_json::Value,
) -> anyhow::Result<String> {
    let url = format!("{}/api/generate", config.ollama_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": config.model,
        "prompt": prompt,
        "system": system,
        "format": format,
        "stream": false,
        "options": {
            "temperature": temperature,
            // Greedy decode — eliminates sampling noise for classification.
            "top_k": 1,
            "top_p": 0.1,
            // Repeat penalty is meaningless for short JSON output.
            "repeat_penalty": 1.0,
            // Our prompts are ~2KB; 2048 is plenty and halves KV-cache cost.
            "num_ctx": 2048,
            // Cap output — even with CoT field, a classifier should never need more.
            "num_predict": 512
        }
    });

    call_ollama_post(&url, &body).await
}

async fn call_ollama_post(url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .with_context(|| {
            format!(
                "Failed to connect to Ollama at {}.\n  Run `gitrust setup` to install and configure Ollama, \
                 or start it manually with `ollama serve`.",
                url
            )
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Ollama returned HTTP {}: {}", status, text));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .context("Ollama response was not valid JSON")?;

    let response_text = json["response"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Ollama response missing 'response' field"))?
        .to_string();

    Ok(response_text)
}

/// Call Ollama's /api/generate endpoint with free-form JSON output mode.
/// Returns the raw response text (may be JSON or prose with fences).
pub async fn call_ollama(
    prompt: &str,
    system: &str,
    config: &Config,
    temperature: f32,
) -> anyhow::Result<String> {
    let url = format!("{}/api/generate", config.ollama_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": config.model,
        "prompt": prompt,
        "system": system,
        "format": "json",
        "stream": false,
        "options": {
            "temperature": temperature,
            "top_k": 1,
            "top_p": 0.1,
            "repeat_penalty": 1.0,
            "num_ctx": 2048,
            "num_predict": 512
        }
    });

    let response_text = call_ollama_post(&url, &body).await?;

    Ok(response_text)
}

/// Call Ollama's /api/generate WITHOUT forcing JSON output.
/// Used when we want a bare-text response (e.g. commit messages).
pub async fn call_ollama_text(
    prompt: &str,
    system: &str,
    config: &Config,
    temperature: f32,
) -> anyhow::Result<String> {
    let url = format!("{}/api/generate", config.ollama_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": config.model,
        "prompt": prompt,
        "system": system,
        // No "format" — the model is free to emit plain text.
        "stream": false,
        "options": {
            "temperature": temperature,
            "top_k": 40,
            "top_p": 0.9,
            "repeat_penalty": 1.1,
            "num_ctx": 4096,
            "num_predict": 128
        }
    });

    call_ollama_post(&url, &body).await
}

// =============================================================================
// JSON repair
// =============================================================================

/// Strip markdown fences, trim whitespace, fix common JSON issues from LLM output.
pub fn json_repair(raw: &str) -> String {
    let mut s = raw.trim().to_string();

    // Strip markdown code fences
    for prefix in &["```json\n", "```json", "```\n", "```"] {
        if s.starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            break;
        }
    }
    if s.ends_with("```") {
        s = s[..s.len() - 3].to_string();
    }

    s = s.trim().to_string();

    // If the string starts with `{` and ends before a `}`, try to close it
    if s.starts_with('{') && !s.ends_with('}') {
        s.push('}');
    }

    // Replace common LLM typos: Python None/True/False → JSON null/true/false
    s = s.replace(": None", ": null");
    s = s.replace(": True", ": true");
    s = s.replace(": False", ": false");

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_repair_strips_markdown_fence() {
        let raw = "```json\n{\"intent\": \"read_only\"}\n```";
        let result = json_repair(raw);
        assert_eq!(result, "{\"intent\": \"read_only\"}");
    }

    #[test]
    fn test_json_repair_strips_plain_fence() {
        let raw = "```\n{\"op\": \"status\"}\n```";
        let result = json_repair(raw);
        assert_eq!(result, "{\"op\": \"status\"}");
    }

    #[test]
    fn test_json_repair_no_fence() {
        let raw = "  {\"intent\": \"clarify\"} ";
        let result = json_repair(raw);
        assert_eq!(result, "{\"intent\": \"clarify\"}");
    }

    #[test]
    fn test_json_repair_fixes_python_literals() {
        let raw = r#"{"flag": None, "ok": True}"#;
        let result = json_repair(raw);
        assert!(result.contains("null"));
        assert!(result.contains("true"));
    }

    #[test]
    fn test_json_repair_closes_open_brace() {
        let raw = r#"{"intent": "status""#;
        let result = json_repair(raw);
        assert!(result.ends_with('}'));
    }

    #[test]
    fn test_build_intent_type_read_only_status() {
        let ri = RawIntent {
            intent: "read_only".to_string(),
            op: "status".to_string(),
            confidence: 0.9,
            ..Default::default()
        };
        let result = build_intent_type(&ri, "show status").unwrap();
        assert!(matches!(result, IntentType::ReadOnly(ReadOnlyOp::Status)));
    }

    #[test]
    fn test_build_intent_type_read_only_log() {
        let ri = RawIntent {
            intent: "read_only".to_string(),
            op: "log".to_string(),
            confidence: 0.9,
            count: Some(5),
            ..Default::default()
        };
        let result = build_intent_type(&ri, "show last 5 commits").unwrap();
        match result {
            IntentType::ReadOnly(ReadOnlyOp::Log { count }) => assert_eq!(count, 5),
            _ => panic!("Expected ReadOnly Log"),
        }
    }

    #[test]
    fn test_build_intent_type_scoped_write_commit_with_message() {
        let ri = RawIntent {
            intent: "scoped_write".to_string(),
            op: "commit".to_string(),
            message: "feat: add login".to_string(),
            confidence: 0.85,
            ..Default::default()
        };
        let result = build_intent_type(&ri, "commit login changes").unwrap();
        match result {
            IntentType::ScopedWrite(ScopedWriteOp::Commit { message }) => {
                assert!(matches!(message, CommitMessage::Provided(_)));
                if let CommitMessage::Provided(m) = message {
                    assert_eq!(m, "feat: add login");
                }
            }
            _ => panic!("Expected ScopedWrite Commit"),
        }
    }

    #[test]
    fn test_build_intent_type_feature_pipeline_push() {
        let ri = RawIntent {
            intent: "feature_pipeline".to_string(),
            op: "push".to_string(),
            feature_hint: "login".to_string(),
            confidence: 0.9,
            ..Default::default()
        };
        let result = build_intent_type(&ri, "push login changes").unwrap();
        match result {
            IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush { feature_hint }) => {
                assert_eq!(feature_hint, "login");
            }
            _ => panic!("Expected FeaturePipeline AddCommitPush"),
        }
    }

    #[test]
    fn test_build_intent_type_feature_pipeline_sync_rebase() {
        let ri = RawIntent {
            intent: "feature_pipeline".to_string(),
            op: "sync".to_string(),
            confidence: 0.8,
            ..Default::default()
        };
        let result = build_intent_type(&ri, "sync with main via rebase").unwrap();
        match result {
            IntentType::FeaturePipeline(FeaturePipelineOp::Sync { strategy }) => {
                assert!(matches!(strategy, SyncStrategy::Rebase));
            }
            _ => panic!("Expected FeaturePipeline Sync"),
        }
    }

    #[test]
    fn test_build_intent_type_scoped_write_reset_hard() {
        let ri = RawIntent {
            intent: "scoped_write".to_string(),
            op: "reset".to_string(),
            reset_mode: "hard".to_string(),
            confidence: 0.9,
            ..Default::default()
        };
        let result = build_intent_type(&ri, "hard reset").unwrap();
        match result {
            IntentType::ScopedWrite(ScopedWriteOp::Reset { mode }) => {
                assert_eq!(mode, ResetMode::Hard);
            }
            _ => panic!("Expected ScopedWrite Reset Hard"),
        }
    }

    #[test]
    fn test_build_intent_type_unknown_returns_error() {
        let ri = RawIntent {
            intent: "totally_unknown".to_string(),
            confidence: 0.9,
            ..Default::default()
        };
        let result = build_intent_type(&ri, "do something");
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_draft_message_strips_verb() {
        let msg = derive_draft_message("commit login form changes");
        assert!(msg.starts_with("Login"));
    }

    #[test]
    fn test_derive_draft_message_truncates() {
        let long_query = "add ".to_string() + &"x".repeat(100);
        let msg = derive_draft_message(&long_query);
        assert!(msg.len() <= 73); // 72 chars + ellipsis
    }

    #[test]
    fn test_canonical_status() {
        assert!(matches!(
            try_canonical("status"),
            Some(IntentType::ReadOnly(ReadOnlyOp::Status))
        ));
        assert!(matches!(
            try_canonical("  STATUS  "),
            Some(IntentType::ReadOnly(ReadOnlyOp::Status))
        ));
        assert!(matches!(
            try_canonical("st"),
            Some(IntentType::ReadOnly(ReadOnlyOp::Status))
        ));
    }

    #[test]
    fn test_canonical_branch_variants() {
        for q in &[
            "branch",
            "branches",
            "list branches",
            "show branches",
            "current branch",
        ] {
            assert!(
                matches!(
                    try_canonical(q),
                    Some(IntentType::ReadOnly(ReadOnlyOp::Branches))
                ),
                "canonical match failed for {:?}",
                q
            );
        }
    }

    #[test]
    fn test_canonical_push_publish() {
        assert!(matches!(
            try_canonical("push"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Push { .. }))
        ));
        assert!(matches!(
            try_canonical("publish"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Push { .. }))
        ));
    }

    #[test]
    fn test_canonical_no_substring_match() {
        // "status of branch" must NOT match "status"
        assert!(try_canonical("status of branch").is_none());
        assert!(try_canonical("push my login work").is_none());
        assert!(try_canonical("show me the status").is_none());
    }

    #[test]
    fn test_canonical_parameterized_branch_and_checkout() {
        // M-05 regression: a verb prefix + single-token name is captured
        // deterministically (no LLM), instead of dropping the name.
        assert!(matches!(
            try_canonical("create branch feature-x"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Branch { name })) if name == "feature-x"
        ));
        assert!(matches!(
            try_canonical("new branch bugfix/123"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Branch { name })) if name == "bugfix/123"
        ));
        assert!(matches!(
            try_canonical("checkout main"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Checkout { target })) if target == "main"
        ));
        assert!(matches!(
            try_canonical("switch to develop"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Checkout { target })) if target == "develop"
        ));
    }

    #[test]
    fn test_canonical_parameterized_squash_count() {
        // M-19 regression: squash phrasings with an explicit count route
        // deterministically to SquashLastN, off the flaky model.
        for (q, want) in &[
            ("squash the last 3 commits", 3u32),
            ("squash last 3", 3),
            ("squash my last 2 commits", 2),
            ("combine my last 4 commits", 4),
            ("squash the last 5 commits into one", 5),
        ] {
            assert!(
                matches!(
                    try_canonical(q),
                    Some(IntentType::ScopedWrite(ScopedWriteOp::SquashLastN { n })) if n == *want
                ),
                "squash count capture failed for {:?} (wanted {})",
                q,
                want
            );
        }
        // Squash without a count, or a 1-commit squash, defers to the LLM.
        assert!(try_canonical("squash my commits").is_none());
        assert!(try_canonical("squash the last commit").is_none());
        assert!(try_canonical("squash last 1").is_none());
        // A number in a non-squash phrase is not grabbed.
        assert!(try_canonical("show me the last 3 commits").is_none());
    }

    #[test]
    fn test_canonical_parameterized_sync() {
        use crate::commands::{FeaturePipelineOp, SyncStrategy};
        // M-10 regression: "sync with <branch>" routes to the Sync pipeline
        // (a write that offers rebase/merge), NOT a read-only status.
        for q in &[
            "sync with main",
            "sync my branch with main",
            "sync with origin/main",
            "update my branch with main",
        ] {
            assert!(
                matches!(
                    try_canonical(q),
                    Some(IntentType::FeaturePipeline(FeaturePipelineOp::Sync { .. }))
                ),
                "sync capture failed for {:?}",
                q
            );
        }
        // Explicit "rebase" preselects the Rebase strategy.
        assert!(matches!(
            try_canonical("sync with main using rebase"),
            Some(IntentType::FeaturePipeline(FeaturePipelineOp::Sync {
                strategy: SyncStrategy::Rebase
            }))
        ));
        // Fetch-status canonicals must NOT be captured as sync.
        assert!(matches!(
            try_canonical("sync check"),
            Some(IntentType::ReadOnly(ReadOnlyOp::FetchStatus { .. }))
        ));
        // A bare "sync" (no target) defers to the LLM.
        assert!(try_canonical("sync").is_none());
    }

    #[test]
    fn test_canonical_parameterized_scoped_push() {
        use crate::commands::FeaturePipelineOp;
        // M-04 regression: "push the <topic> changes" scopes to the topic via the
        // feature pipeline (the 3B model collapses this to a plain push).
        for (q, want) in &[
            ("push the login changes", "login"),
            ("push login changes", "login"),
            ("push the auth fixes", "auth"),
            ("push the export work", "export"),
        ] {
            match try_canonical(q) {
                Some(IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush {
                    feature_hint,
                })) => assert_eq!(&feature_hint, want, "wrong topic for {:?}", q),
                other => panic!("expected scoped push for {:?}, got {:?}", q, other),
            }
        }
        // Possessive / bare / all-files forms must NOT be captured (plain push).
        assert!(try_canonical("push my work").is_none());
        assert!(try_canonical("push my login work").is_none());
        assert!(try_canonical("push everything").is_none());
        assert!(try_canonical("push all changes").is_none());
        assert!(try_canonical("push the code").is_none()); // 1 word after 'the'
    }

    #[test]
    fn test_canonical_amend_message_capture() {
        // M-20 regression: "fix the last commit message to 'X'" routes to a
        // message-only Amend with X extracted (case preserved).
        let cases = [
            (
                "fix the last commit message to 'typo message fixed'",
                "typo message fixed",
            ),
            (
                "change the commit message to \"feat: add auth\"",
                "feat: add auth",
            ),
            ("reword the last commit message to 'WIP'", "WIP"),
            (
                "update the commit message to: refactor login",
                "refactor login",
            ),
        ];
        for (q, want) in &cases {
            match try_canonical(q) {
                Some(IntentType::ScopedWrite(ScopedWriteOp::Amend {
                    message: Some(m),
                    include_staged,
                })) => {
                    assert_eq!(&m, want, "wrong amend message for {:?}", q);
                    assert!(
                        !include_staged,
                        "message-only amend must not fold staged changes"
                    );
                }
                other => panic!("expected amend for {:?}, got {:?}", q, other),
            }
        }
        // Bare "amend" (no new message) stays the canonical amend (message=None).
        assert!(matches!(
            try_canonical("amend"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Amend {
                message: None,
                ..
            }))
        ));
        // A non-message "fix" phrase is not hijacked.
        assert!(!matches!(
            try_canonical("fix the login bug"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::Amend { .. }))
        ));
    }

    #[test]
    fn test_canonical_parameterized_defers_ambiguous_to_llm() {
        // Bare read-only verbs stay read-only (the parameterized arm never fires
        // without a trailing token).
        assert!(matches!(
            try_canonical("branch"),
            Some(IntentType::ReadOnly(ReadOnlyOp::Branches))
        ));
        // Multi-word / quoted / flag-shaped arguments fall through to the LLM.
        assert!(try_canonical("create branch for the login feature").is_none());
        assert!(try_canonical("checkout the main branch please").is_none());
        assert!(try_canonical("branch protection rules").is_none());
        assert!(try_canonical("checkout -b foo").is_none());
    }

    #[test]
    fn test_canonical_full_whitelist() {
        for q in &[
            "status",
            "st",
            "log",
            "diff",
            "remotes",
            "remote",
            "remote -v",
            "branch",
            "branches",
            "list branches",
            "show branches",
            "current branch",
            "config",
            "user",
            "whoami",
            "push",
            "publish",
            "pull",
            "stash",
            "stash pop",
            "pop",
        ] {
            assert!(try_canonical(q).is_some(), "canonical missed: {:?}", q);
        }
    }
}
