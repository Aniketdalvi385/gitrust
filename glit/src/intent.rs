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
        "undo" | "undo last commit" | "undo commit" | "unstage commit" => {
            IntentType::ScopedWrite(ScopedWriteOp::Undo)
        }
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
        "rebase"
        | "pull rebase"
        | "pull with rebase"
        | "rebase onto origin"
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
        _ => return None,
    };
    Some(intent)
}

/// Build a short list of plausible alternative intents when classifier confidence
/// is mid-range. Hand-rolled rules — NOT an LLM call.
/// Returns at most 2 alternates distinct from `top`.
pub fn alternates_for(top: &IntentType, ctx: &RepoContext) -> Vec<IntentType> {
    let mut alts: Vec<IntentType> = Vec::new();
    let has_unstaged =
        !ctx.status.modified.is_empty() || !ctx.status.untracked.is_empty();
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
        IntentType::FeaturePipeline(FeaturePipelineOp::Sync { .. }) => "Sync with upstream".to_string(),
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

    let raw = call_ollama_with_schema(&prompt, &SYSTEM_PROMPT, config, 0.0, classifier_schema()).await?;
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

    // Surface the model's reasoning to stderr when GLIT_DEBUG is set, or
    // when confidence is mid-range (helps the user understand the disambiguation menu).
    let debug_on = std::env::var("GLIT_DEBUG").is_ok();
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
                "merge_preview" | "predict_merge" | "preview_merge" => {
                    ReadOnlyOp::MergePreview {
                        remote: if ri.remote_name.is_empty() {
                            "origin".to_string()
                        } else {
                            ri.remote_name.clone()
                        },
                    }
                }
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
                "undo" => ScopedWriteOp::Undo,
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
                "Failed to connect to Ollama at {}.\n  Run `glit setup` to install and configure Ollama, \
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
        for q in &["branch", "branches", "list branches", "show branches", "current branch"] {
            assert!(
                matches!(try_canonical(q), Some(IntentType::ReadOnly(ReadOnlyOp::Branches))),
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
        assert!(try_canonical("create branch foo").is_none());
    }

    #[test]
    fn test_canonical_full_whitelist() {
        for q in &[
            "status", "st", "log", "diff", "remotes", "remote", "remote -v",
            "branch", "branches", "list branches", "show branches", "current branch",
            "config", "user", "whoami", "push", "publish", "pull", "stash",
            "stash pop", "pop",
        ] {
            assert!(try_canonical(q).is_some(), "canonical missed: {:?}", q);
        }
    }
}
