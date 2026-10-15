use crate::commands::{
    CommitMessage, FeaturePipelineOp, FileSelection, IntentType, ReadOnlyOp, ScopedWriteOp,
    SyncStrategy,
};
use crate::config::Config;
use crate::context::RepoContext;

/// Layer-1 model seam: rank a closed list of options for a state, best first.
/// The model only RANKS options we wrote; deciding whether to act, ask or
/// refuse is `kev::decide`, and building the intent (arguments included) is
/// `build_from_op` — both deterministic. Tests swap in a fixed distribution.
pub trait IntentClassifier {
    fn rank(
        &self,
        state: serde_json::Value,
        instructions: &str,
        options: &[(String, String)],
        config: &Config,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<(String, f32)>>> + Send;
}

/// The local Kev decision model (see `kev.rs`).
pub struct KevClassifier;

impl IntentClassifier for KevClassifier {
    async fn rank(
        &self,
        state: serde_json::Value,
        instructions: &str,
        options: &[(String, String)],
        config: &Config,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        let _spin = crate::ux::Spinner::start(crate::copy::SPINNER_UNDERSTANDING);
        crate::kev::ask_choice(config, state, instructions, options).await
    }
}

/// Which in-progress control family applies, if the repo is mid-operation.
pub fn in_progress_family(ctx: &RepoContext) -> Option<&'static str> {
    use crate::commands::RepoSituation as S;
    match ctx.situation {
        S::MidRebase { .. } => Some("rebase"),
        S::MidMerge { .. } => Some("merge"),
        S::MidCherryPick { .. } => Some("cherry_pick"),
        S::MidRevert { .. } => Some("revert"),
        _ => None,
    }
}

/// Mid-operation, a *question about the conflicts* ("whats conflicting",
/// "which files have conflicts?") is answered by `status`, which lists the
/// conflicted files and the way forward/back. A control request that merely
/// mentions conflicts ("abort, too many conflicts") is not a question.
pub fn is_conflict_question(raw: &str) -> bool {
    let q = raw.to_lowercase();
    let words: Vec<&str> = q
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let about_conflicts = words.iter().any(|w| w.starts_with("conflict"));
    let asks = q.trim_end().ends_with('?')
        || words.iter().any(|w| {
            matches!(
                *w,
                "show" | "which" | "what" | "whats" | "list" | "where" | "see" | "view" | "tell"
            )
        });
    about_conflicts && asks
}

/// The intent for an in-progress control op (from the in-progress question).
pub fn build_in_progress(op: &str) -> Option<IntentType> {
    let w = match op {
        "rebase_continue" => ScopedWriteOp::RebaseContinue,
        "rebase_skip" => ScopedWriteOp::RebaseSkip,
        "rebase_abort" => ScopedWriteOp::RebaseAbort,
        "merge_abort" => ScopedWriteOp::MergeAbort,
        "cherry_pick_continue" => ScopedWriteOp::CherryPickContinue,
        "cherry_pick_abort" => ScopedWriteOp::CherryPickAbort,
        "revert_continue" => ScopedWriteOp::RevertContinue,
        "revert_abort" => ScopedWriteOp::RevertAbort,
        _ => return None,
    };
    Some(IntentType::ScopedWrite(w))
}

/// The state Kev reads: the request plus a one-line repo summary, in the same
/// shape it was fine-tuned on (docs/classifier_eval/make_kev_train.py).
pub fn kev_state(context: &RepoContext, query: &str) -> serde_json::Value {
    serde_json::json!({ "user_request": query, "repo_state": repo_state_line(context) })
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {}", one)
    } else {
        format!("{} {}", n, many)
    }
}

/// e.g. "clean repo, on branch main" / "on branch feature-x, 2 files modified,
/// 1 commit ahead of origin". Deliberately short — Kev-0.8B is trained on short states.
pub fn repo_state_line(ctx: &RepoContext) -> String {
    if ctx.is_empty_repo {
        return "empty repository, no commits yet".to_string();
    }
    let at = if ctx.is_detached {
        "detached HEAD".to_string()
    } else {
        format!("on branch {}", ctx.branch)
    };
    // `upstream` is the full ref ("refs/remotes/origin/main") → remote "origin".
    let remote = ctx
        .upstream
        .as_deref()
        .map(|u| u.strip_prefix("refs/remotes/").unwrap_or(u))
        .and_then(|u| u.split('/').next())
        .unwrap_or("origin");
    let s = &ctx.status;
    let mut parts = vec![at.clone()];
    let modified = s.modified.len() + s.modified_overflow;
    let staged = s.staged.len() + s.staged_overflow;
    let untracked = s.untracked.len() + s.untracked_overflow;
    if modified > 0 {
        parts.push(format!("{} modified", plural(modified, "file", "files")));
    }
    if staged > 0 {
        parts.push(plural(staged, "staged file", "staged files"));
    }
    if untracked > 0 {
        parts.push(plural(untracked, "untracked file", "untracked files"));
    }
    if ctx.ahead > 0 {
        parts.push(format!(
            "{} ahead of {}",
            plural(ctx.ahead, "commit", "commits"),
            remote
        ));
    }
    if ctx.behind > 0 {
        parts.push(format!(
            "behind {} by {}",
            remote,
            plural(ctx.behind, "commit", "commits")
        ));
    }
    // In-progress operations are spelled out so the in-progress question can
    // route "continue"/"abort". Other situations are already covered above
    // (detached → `at`, diverged → ahead/behind) or gated in Layer 1.5.
    use crate::commands::RepoSituation as S;
    if matches!(
        ctx.situation,
        S::MidMerge { .. }
            | S::MidRebase { .. }
            | S::MidCherryPick { .. }
            | S::MidRevert { .. }
            | S::MidBisect
    ) {
        if let Some(desc) = crate::context::describe_situation(&ctx.situation) {
            parts.push(desc);
        }
    }
    if parts.len() == 1 {
        format!("clean repo, {}", at)
    } else {
        parts.join(", ")
    }
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
    // "add remote [name] <url>" — the URL is captured from the RAW query (its
    // path can be case-sensitive).
    if let Some(intent) = capture_add_remote(query) {
        return Some(intent);
    }
    let q = query.trim().to_lowercase();
    let intent = match q.as_str() {
        "status" | "st" => IntentType::ReadOnly(ReadOnlyOp::Status),
        "init" | "git init" | "initialize a repository" => {
            IntentType::ScopedWrite(ScopedWriteOp::Init)
        }
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
        // Commit-ALL language (stage every change, then commit) — deterministic
        // so it can never misroute to a stage-only add (M-03b: the 3B model read
        // "save my changes" as a bare add and never committed). Message is a
        // Draft → generated from the diff at build time (the seed is ignored).
        // "commit" ALONE (staged-only) is intentionally NOT here — it stays with
        // the LLM/build path so bare-commit semantics are preserved.
        "commit this"
        | "commit everything"
        | "commit all"
        | "commit all my files"
        | "commit all my changes"
        | "commit all changes"
        | "commit my changes"
        | "commit my work"
        | "save my changes"
        | "save my work"
        | "save all my changes"
        | "save changes"
        | "save everything"
        | "stage and commit"
        | "stage all and commit"
        | "add and commit"
        | "add all and commit" => IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
            message: CommitMessage::Draft(String::new()),
            files: FileSelection::All,
        }),
        // Stage everything, commit (the user writes the message), push.
        "commit and push"
        | "commit & push"
        | "commit and push everything"
        | "commit and push all"
        | "commit everything and push"
        | "add commit and push" => IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
            message: CommitMessage::Draft(String::new()),
            stage_first: Some(FileSelection::All),
        }),
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
    let starts_with_edit_verb = lower.starts_with("fix ")
        || lower.starts_with("change ")
        || lower.starts_with("update ")
        || lower.starts_with("reword ")
        || lower.starts_with("rename ")
        || lower.starts_with("edit ");
    let has_target = lower.contains(" to ") || lower.contains(":");
    // Two accepted shapes, both requiring an edit verb + a "to <new text>":
    //   1. explicit  — mentions "commit message" ("fix the commit message to X")
    //   2. reword-shape — "reword"/"rename" + "commit" WITHOUT the literal word
    //      "message" ("reword the last commit to X"). reword/rename a commit
    //      unambiguously means its message, so this is safe (M-20b: the 3B model
    //      read "reword the last commit to X" as a NEW commit and blocked).
    let explicit = lower.contains("commit message");
    let reword_shape =
        (lower.starts_with("reword ") || lower.starts_with("rename ")) && lower.contains("commit");
    let is_msg_change = starts_with_edit_verb && has_target && (explicit || reword_shape);
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
    // Strip a leading determiner so both "push the login changes" and "push all
    // docs changes" reduce to "<topic> <filter-noun>". "all"/"the" here are
    // determiners, NOT the topic (the topic-is-word check below still rejects a
    // bare determiner as the topic).
    // A trailing destination ("… to github") names where, not what.
    let rest = [
        " to the remote",
        " to remote",
        " to github",
        " to gitlab",
        " to origin",
        " to upstream",
    ]
    .iter()
    .find_map(|d| rest.strip_suffix(d))
    .unwrap_or(rest);
    let rest = rest
        .strip_prefix("the ")
        .or_else(|| rest.strip_prefix("all the "))
        .or_else(|| rest.strip_prefix("all "))
        .unwrap_or(rest);
    // Need "<topic> <filter-noun>" — the topic is the word immediately BEFORE a
    // trailing filter noun. Extra leading words (adjectives) are ignored; we key
    // off the last two tokens so "push all the new login changes" → topic=login.
    let filter_nouns = [
        "changes", "change", "files", "file", "work", "fixes", "fix", "edits", "edit", "stuff",
        "feature", "updates", "code",
    ];
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    // "search feature files": stacked filter nouns — the topic precedes them all.
    while words.len() >= 3
        && filter_nouns.contains(&words[words.len() - 1])
        && filter_nouns.contains(&words[words.len() - 2])
    {
        words.pop();
    }
    if words.len() < 2 {
        return None;
    }
    let topic = words[words.len() - 2];
    let noun = words[words.len() - 1];
    // The topic must itself be a plain word (not a filter noun or a determiner).
    let topic_is_word = topic
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !filter_nouns.contains(&topic)
        && !matches!(
            topic,
            "all" | "everything" | "some" | "these" | "those" | "any" | "the" | "my"
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
    // Direction form: "pull/merge/bring <src> into <dst>" — bringing another
    // branch's commits into the current one IS a sync (M-10c: the model read
    // "pull main into feature" as a plain pull of the current branch and did
    // nothing). The " into " keyword is what disambiguates this from a bare
    // "pull" (already a canonical) — a plain pull has no "into <branch>".
    let is_into = (q.starts_with("pull ") || q.starts_with("merge ") || q.starts_with("bring "))
        && q.contains(" into ");
    if !(is_sync || is_update_with || is_into) {
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
    // The count may be a digit ("last 3") or a spelled-out word ("last three").
    // Try a digit token first, then a number word — the M-19b variant
    // ("squash the last three commits") failed live because only digits parsed.
    let n: u32 = q
        .split(|c: char| !c.is_ascii_digit())
        .find(|tok| !tok.is_empty())
        .and_then(|tok| tok.parse().ok())
        .or_else(|| word_to_number(q))?;
    // A 0/1-commit squash is meaningless; require ≥ 2. (n==1 or n==0 → defer,
    // the LLM/clarifier can ask.)
    if n >= 2 {
        Some(n)
    } else {
        None
    }
}

/// Map a spelled-out small number word appearing in `q` to its value (2–10).
/// Deliberately tiny — squashing more than ~10 commits at once is rare and can
/// defer to the LLM. Returns the FIRST such word found.
fn word_to_number(q: &str) -> Option<u32> {
    const WORDS: &[(&str, u32)] = &[
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
    ];
    // Word-boundary match so "seventeen" doesn't match "seven" — check tokens.
    q.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|t| !t.is_empty())
        .find_map(|tok| WORDS.iter().find(|(w, _)| *w == tok).map(|(_, n)| *n))
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

// =============================================================================
// Kev-routed op → concrete IntentType (arguments are captured, never generated)
// =============================================================================

/// An argument a routed op needs that the query didn't contain. The agent asks
/// the user for it; `--yes` refuses rather than invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    BranchName,
    CheckoutTarget,
    Topic,
    SquashCount,
    RemoteUrl,
}

impl ArgKind {
    /// The prompt label shown to the user.
    pub fn prompt(&self) -> &'static str {
        match self {
            ArgKind::BranchName => "Name for the new branch:",
            ArgKind::CheckoutTarget => "Branch to switch to:",
            ArgKind::Topic => "Which feature/topic should the push include (e.g. login)?",
            ArgKind::SquashCount => "How many recent commits to squash into one?",
            ArgKind::RemoteUrl => "Remote URL:",
        }
    }
    /// Short noun for error messages ("the branch name").
    pub fn noun(&self) -> &'static str {
        match self {
            ArgKind::BranchName => "the new branch name",
            ArgKind::CheckoutTarget => "the branch to switch to",
            ArgKind::Topic => "the feature/topic to push",
            ArgKind::SquashCount => "how many commits to squash",
            ArgKind::RemoteUrl => "the remote URL",
        }
    }
}

/// Build the concrete `IntentType` for an op Kev routed to (or the user picked
/// from Kev's candidates). Every argument comes from a deterministic capture over
/// the raw query and the repo context; when one is missing, `ask` is called —
/// the model never supplies an argument. Multi-commit undo, cleanup scope and
/// similar semantics are still decided downstream by their gates.
pub fn build_from_op(
    op: &str,
    raw: &str,
    ctx: &RepoContext,
    ask: &mut dyn FnMut(ArgKind) -> anyhow::Result<String>,
) -> anyhow::Result<IntentType> {
    let q = raw.trim().to_lowercase();
    let origin = || "origin".to_string();
    let named = named_files(raw, ctx);
    let message = || match capture_commit_message(raw) {
        Some(m) => CommitMessage::Provided(m),
        None => CommitMessage::Draft(String::new()),
    };
    let it = match op {
        "status" => IntentType::ReadOnly(ReadOnlyOp::Status),
        "log" => IntentType::ReadOnly(ReadOnlyOp::Log {
            count: first_number(&q)
                .filter(|n| (1..=100).contains(n))
                .map(|n| n as u8)
                .unwrap_or(10),
        }),
        "diff" => IntentType::ReadOnly(ReadOnlyOp::Diff {
            path: named.first().cloned(),
        }),
        "branches" => IntentType::ReadOnly(ReadOnlyOp::Branches),
        "config" => IntentType::ReadOnly(ReadOnlyOp::Config),
        "show" => IntentType::ReadOnly(ReadOnlyOp::Show {
            target: capture_commit_ref(raw),
        }),
        "fetch" => IntentType::ReadOnly(ReadOnlyOp::FetchStatus { remote: origin() }),
        "merge_preview" => IntentType::ReadOnly(ReadOnlyOp::MergePreview { remote: origin() }),
        "add_commit" => IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
            message: message(),
            files: if named.is_empty() {
                FileSelection::All
            } else {
                FileSelection::Explicit(named)
            },
        }),
        "commit" => IntentType::ScopedWrite(ScopedWriteOp::Commit { message: message() }),
        "commit_push" => IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
            message: message(),
            stage_first: Some(if named.is_empty() {
                FileSelection::All
            } else {
                FileSelection::Explicit(named.clone())
            }),
        }),
        "push" => IntentType::ScopedWrite(ScopedWriteOp::Push { remote: origin() }),
        "scoped_push" => {
            let topic = match capture_topic(&q) {
                Some(t) => t,
                None => required(ask, ArgKind::Topic)?,
            };
            IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush {
                feature_hint: topic,
            })
        }
        "branch" => {
            // A typed name is passed through as-is: the Branch step validates
            // it and offers a fixed-up suggestion (Flow 05) — no dead end here.
            let name = match capture_new_branch_name(&q) {
                Some(n) => n,
                None => required(ask, ArgKind::BranchName)?,
            };
            IntentType::ScopedWrite(ScopedWriteOp::Branch { name })
        }
        "checkout" => {
            let target = match capture_checkout_target(&q) {
                Some(t) => t,
                None => ref_token(required(ask, ArgKind::CheckoutTarget)?)?,
            };
            IntentType::ScopedWrite(ScopedWriteOp::Checkout { target })
        }
        "pull" => IntentType::ScopedWrite(ScopedWriteOp::Pull { remote: origin() }),
        "sync" => {
            capture_sync(&q).unwrap_or(IntentType::FeaturePipeline(FeaturePipelineOp::Sync {
                strategy: if q.contains("rebase") {
                    SyncStrategy::Rebase
                } else {
                    SyncStrategy::Merge
                },
            }))
        }
        // The undo ChoiceGate decides the semantic; a multi-commit count in the
        // raw query makes the builder HALT downstream (never trust a count here).
        "undo" => IntentType::ScopedWrite(ScopedWriteOp::UndoIntent),
        // A new message comes from the existing capture or any quoted text;
        // without one, amend folds in staged changes and keeps the message.
        "amend" => capture_amend_message(raw)
            .or_else(|| {
                extract_quoted(raw).map(|m| {
                    IntentType::ScopedWrite(ScopedWriteOp::Amend {
                        message: Some(m),
                        include_staged: false,
                    })
                })
            })
            .unwrap_or(IntentType::ScopedWrite(ScopedWriteOp::Amend {
                message: None,
                include_staged: true,
            })),
        "squash" => {
            let n = match capture_squash_count(&q).or_else(|| count_in(&q)) {
                Some(n) => n,
                None => {
                    let a = required(ask, ArgKind::SquashCount)?;
                    a.trim()
                        .parse::<u32>()
                        .ok()
                        .or_else(|| word_to_number(&a.to_lowercase()))
                        .ok_or_else(|| anyhow::anyhow!("'{}' is not a number of commits.", a))?
                }
            };
            if n < 2 {
                anyhow::bail!("Squashing needs at least 2 commits (got {}).", n);
            }
            IntentType::ScopedWrite(ScopedWriteOp::SquashLastN { n })
        }
        "revert" => IntentType::ScopedWrite(ScopedWriteOp::Revert {
            oid: capture_commit_ref(raw).unwrap_or_default(),
        }),
        "unstage" => IntentType::ScopedWrite(ScopedWriteOp::Unstage { paths: named }),
        "stash" => IntentType::ScopedWrite(ScopedWriteOp::Stash),
        "cleanup" => IntentType::ScopedWrite(ScopedWriteOp::CleanupIntent),
        "discard_local" => {
            IntentType::ScopedWrite(ScopedWriteOp::DiscardLocal { remote: origin() })
        }
        "force_push" => IntentType::ScopedWrite(ScopedWriteOp::ForcePush { remote: origin() }),
        "init" => IntentType::ScopedWrite(ScopedWriteOp::Init),
        "add_remote" => {
            let url = match capture_url(raw) {
                Some(u) => u,
                None => required(ask, ArgKind::RemoteUrl)?,
            };
            IntentType::ScopedWrite(ScopedWriteOp::AddRemote {
                name: capture_after_word(&q, &["called", "named"]).unwrap_or_else(origin),
                url,
            })
        }
        "find_lost_work" => IntentType::ScopedWrite(ScopedWriteOp::ForensicRecover),
        "remotes" => IntentType::ReadOnly(ReadOnlyOp::Remotes),
        "stash_pop" => IntentType::ScopedWrite(ScopedWriteOp::StashPop),
        "pull_rebase" => IntentType::ScopedWrite(ScopedWriteOp::PullRebase { remote: origin() }),
        "set_upstream" => {
            // "track origin/develop" → (origin, develop); no target → current branch.
            let (remote, branch) = match capture_after_word(&q, &["track", "to"]) {
                Some(t) if t.contains('/') => {
                    let (r, b) = t.split_once('/').unwrap_or(("origin", ""));
                    (r.to_string(), b.to_string())
                }
                Some(t) => (origin(), t),
                None => (origin(), String::new()),
            };
            IntentType::ScopedWrite(ScopedWriteOp::SetUpstream { remote, branch })
        }
        // Removal is confirmed by name downstream; default origin as before.
        "remove_remote" => IntentType::ScopedWrite(ScopedWriteOp::RemoveRemote {
            name: capture_after_word(&q, &["remote", "called", "named"])
                .or_else(|| word_before(&q, "remote"))
                .unwrap_or_else(origin),
        }),
        // Guarded downstream: only allowed when the repo has zero commits.
        "delete_repo" => IntentType::ScopedWrite(ScopedWriteOp::DeleteRepo),
        other => match build_in_progress(other) {
            Some(it) => it,
            None => anyhow::bail!("No operation '{}' to build.", other),
        },
    };
    Ok(it)
}

/// Ask for a missing argument; an empty answer (including `--yes`, which can't
/// type one) is an error — never a guessed default.
fn required(
    ask: &mut dyn FnMut(ArgKind) -> anyhow::Result<String>,
    kind: ArgKind,
) -> anyhow::Result<String> {
    let a = ask(kind)?;
    let a = a.trim().to_string();
    if a.is_empty() {
        anyhow::bail!(
            "Couldn't find {} in your request — nothing was done.",
            kind.noun()
        );
    }
    Ok(a)
}

/// A user-typed ref must be a single git-legal token.
fn ref_token(s: String) -> anyhow::Result<String> {
    if is_single_git_ref_token(&s) {
        Ok(s)
    } else {
        anyhow::bail!("'{}' is not a valid branch name.", s)
    }
}

/// Words that are never a topic/branch on their own.
const FILLER: &[&str] = &[
    "the",
    "a",
    "an",
    "my",
    "this",
    "that",
    "these",
    "those",
    "all",
    "some",
    "any",
    "new",
    "only",
    "just",
    "everything",
    "it",
    "me",
    "up",
    "please",
    "branch",
    "called",
    "named",
    "to",
    "onto",
    "into",
    "over",
    "on",
    "of",
    "and",
    "but",
    "nothing",
    "else",
    "related",
    // verbs: never a ref name when they sit next to "branch"/"remote"
    "remove",
    "delete",
    "drop",
    "unset",
    "create",
    "make",
    "start",
    "open",
    "switch",
    "change",
    "go",
    "checkout",
    "use",
    "set",
];

/// A deterministic cross-check of Kev's route against the request's own words:
/// returns the op the words point to when they contradict Kev's choice, so the
/// caller can ask instead of acting. Calibration can't see a confident confusion
/// (the fine-tune sent "push all api changes" to plain `push` at p 0.96), but the
/// topic capture can: a request that names a topic is never "push everything".
/// Likewise a request that says both "commit" and "push" is never only one of
/// them (eval_set_v2: "commit n push" → plain push at 0.92), and `commit_push`
/// — which publishes every changed file — needs a push word and no topic
/// (gitrust-v1, never trained on it, chose it for "push all schema changes" at
/// 0.89 and "commit everything" at 0.51).
pub fn conflicting_op(op: &str, raw: &str) -> Option<&'static str> {
    let q = raw.trim().to_lowercase();
    let says = |w: &str| {
        q.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| t == w)
    };
    let says_push = ["push", "upload", "send", "ship", "publish"]
        .iter()
        .any(|w| says(w));
    match op {
        "push" | "add_commit" | "commit" | "commit_push" if capture_topic(&q).is_some() => {
            Some("scoped_push")
        }
        "push" | "add_commit" | "commit" if says("commit") && says("push") => Some("commit_push"),
        "commit_push" if !says_push => Some("add_commit"),
        _ => None,
    }
}

/// Topic for a scoped push. Generalises `capture_scoped_push` beyond the
/// literal "push" verb ("ship the X fixes", "commit and push only the X work",
/// "push everything related to X"); anything else → None (the user is asked).
fn capture_topic(q: &str) -> Option<String> {
    if let Some(t) = capture_scoped_push(q) {
        return Some(t);
    }
    if let Some(i) = q.find(" related to ") {
        let w = q[i + " related to ".len()..]
            .split_whitespace()
            .find(|w| !FILLER.contains(w))?;
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        return (!w.is_empty()).then(|| w.to_string());
    }
    for verb in [
        "commit and push only ",
        "commit and push ",
        "only push ",
        "push only ",
        "ship ",
        "send up just ",
        "send up ",
        "send ",
        "upload ",
        "publish ",
    ] {
        if let Some(rest) = q.strip_prefix(verb) {
            // Re-use the "<topic> <filter-noun>" shape of capture_scoped_push.
            // Stop at a clause boundary: "but"/"and"/"not", or a word ending a
            // clause ("files, and leave the docs …").
            let mut words: Vec<&str> = Vec::new();
            for w in rest.split_whitespace() {
                if matches!(w, "but" | "and" | "not") {
                    break;
                }
                let bare = w.trim_end_matches([',', ';', '.']);
                if !FILLER.contains(&bare) {
                    words.push(bare);
                }
                if bare.len() != w.len() {
                    break;
                }
            }
            if let Some(t) = capture_scoped_push(&format!("push {}", words.join(" "))) {
                return Some(t);
            }
        }
    }
    None
}

/// Name for a new branch: the canonical prefixes, or a ref token right after
/// "called"/"named"/"as".
fn capture_new_branch_name(q: &str) -> Option<String> {
    if let Some(IntentType::ScopedWrite(ScopedWriteOp::Branch { name })) =
        try_canonical_parameterized(q)
    {
        return Some(name);
    }
    capture_after_word(q, &["called", "named", "as"]).or_else(|| word_before_branch(q))
}

/// Target of a checkout: the canonical prefixes, "the X branch", or a ref token
/// after "to"/"onto".
fn capture_checkout_target(q: &str) -> Option<String> {
    if let Some(IntentType::ScopedWrite(ScopedWriteOp::Checkout { target })) =
        try_canonical_parameterized(q)
    {
        return Some(target);
    }
    capture_after_word(q, &["to", "onto"]).or_else(|| word_before_branch(q))
}

/// "the release-2.1 branch" → release-2.1 (the token right before "branch").
fn word_before_branch(q: &str) -> Option<String> {
    word_before(q, "branch")
}

/// The ref-shaped, non-filler token right before `marker`.
fn word_before(q: &str, marker: &str) -> Option<String> {
    let words: Vec<&str> = q.split_whitespace().collect();
    let i = words.iter().position(|w| *w == marker)?;
    let w = *words.get(i.checked_sub(1)?)?;
    (!FILLER.contains(&w) && is_single_git_ref_token(w)).then(|| w.to_string())
}

/// The first ref-shaped, non-filler token after any of `markers`.
fn capture_after_word(q: &str, markers: &[&str]) -> Option<String> {
    let words: Vec<&str> = q.split_whitespace().collect();
    let i = words.iter().position(|w| markers.contains(w))?;
    words[i + 1..]
        .iter()
        .map(|w| w.trim_matches(|c: char| matches!(c, ',' | '.' | '?' | '!' | '\'' | '"')))
        .find(|w| !FILLER.contains(w))
        .filter(|w| is_single_git_ref_token(w))
        .map(String::from)
}

/// A commit count anywhere in the query (digit or number word), ≥ 2.
fn count_in(q: &str) -> Option<u32> {
    first_number(q)
        .or_else(|| word_to_number(q))
        .filter(|n| *n >= 2)
}

fn first_number(q: &str) -> Option<u32> {
    q.split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())
        .and_then(|t| t.parse().ok())
}

/// A user-supplied commit message: a quoted string, or text after
/// "message:"/"with message"/"msg:"/"-m". Case preserved.
fn capture_commit_message(raw: &str) -> Option<String> {
    if let Some(m) = extract_quoted(raw) {
        return Some(m);
    }
    let lower = raw.to_lowercase();
    for delim in [
        "with message ",
        "message: ",
        "message:",
        "msg: ",
        "msg:",
        "-m ",
    ] {
        if let Some(i) = lower.find(delim) {
            let m = raw[i + delim.len()..].trim();
            if !m.is_empty() {
                return Some(m.to_string());
            }
        }
    }
    None
}

/// A commit reference: a 7–40 char hex sha or HEAD~N / HEAD^.
fn capture_commit_ref(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .map(|w| w.trim_matches(|c: char| matches!(c, ',' | '.' | '?' | '!' | '\'' | '"' | '`')))
        .find(|w| {
            let hex = w.len() >= 7
                && w.len() <= 40
                && w.chars().all(|c| c.is_ascii_hexdigit())
                && w.chars().any(|c| c.is_ascii_digit());
            let head = w.starts_with("HEAD~") || w.starts_with("HEAD^");
            hex || head
        })
        .map(String::from)
}

/// A remote URL token (scheme://… or scp-style git@host:path).
/// "add remote <url>" / "add remote <name> <url>" (also "git remote add …").
/// Exactly that shape, or None — anything looser goes to the model.
fn capture_add_remote(raw: &str) -> Option<IntentType> {
    let t = raw.trim();
    let lower = t.to_lowercase();
    let prefix = ["add remote ", "git remote add ", "remote add "]
        .iter()
        .find(|p| lower.starts_with(*p))?;
    let rest: Vec<&str> = t[prefix.len()..].split_whitespace().collect();
    let is_url = |w: &str| w.contains("://") || (w.starts_with("git@") && w.contains(':'));
    let (name, url) = match rest.as_slice() {
        [url] if is_url(url) => ("origin".to_string(), url.to_string()),
        [name, url] if is_url(url) && is_single_git_ref_token(name) => {
            (name.to_string(), url.to_string())
        }
        _ => return None,
    };
    Some(IntentType::ScopedWrite(ScopedWriteOp::AddRemote {
        name,
        url,
    }))
}

fn capture_url(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .find(|w| w.contains("://") || (w.starts_with("git@") && w.contains(':')))
        .map(|w| {
            w.trim_matches(|c: char| matches!(c, ',' | '\'' | '"'))
                .to_string()
        })
}

/// Files the query names, matched against the repo's changed files by full
/// path or basename (case-insensitive). Deterministic replacement for the old
/// LLM `files` field; unknown names are simply not matched.
fn named_files(raw: &str, ctx: &RepoContext) -> Vec<String> {
    let s = &ctx.status;
    let known: Vec<&String> = s
        .staged
        .iter()
        .chain(&s.modified)
        .chain(&s.untracked)
        .collect();
    let mut out: Vec<String> = Vec::new();
    for w in raw.split(|c: char| c.is_whitespace() || c == ',') {
        let w = w.trim_matches(|c: char| matches!(c, '.' | '?' | '!' | '\'' | '"' | '`'));
        if w.is_empty() {
            continue;
        }
        let wl = w.to_lowercase();
        for path in &known {
            let pl = path.to_lowercase();
            let base = pl.rsplit('/').next().unwrap_or(&pl);
            if (pl == wl || base == wl) && !out.contains(path) {
                out.push((*path).clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_canonical_init_and_add_remote() {
        // Advertised by `gitrust help` (and the not-a-repo message) as instant;
        // they used to need the model — even outside a repository.
        for q in ["init", "git init", "initialize a repository"] {
            assert!(
                matches!(
                    try_canonical(q),
                    Some(IntentType::ScopedWrite(ScopedWriteOp::Init))
                ),
                "{q}"
            );
        }
        assert!(matches!(
            try_canonical("add remote origin https://github.com/Acme/Site.git"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::AddRemote { ref name, ref url }))
                if name == "origin" && url == "https://github.com/Acme/Site.git"
        ));
        assert!(matches!(
            try_canonical("add remote git@github.com:me/proj.git"),
            Some(IntentType::ScopedWrite(ScopedWriteOp::AddRemote { ref name, .. })) if name == "origin"
        ));
        assert!(try_canonical("add remote").is_none());
        assert!(try_canonical("add remote origin").is_none());
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
            // Spelled-out counts (M-19b variant — "three" failed live).
            ("squash the last three commits", 3),
            ("squash last two", 2),
            ("combine the last four commits into one", 4),
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
            // Direction form — "<verb> <src> into <dst>" (M-10c).
            "pull main into feature",
            "merge main into my branch",
            "bring develop into feature",
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
        // A plain pull (no "into <branch>") must NOT be captured as sync.
        assert!(!matches!(
            try_canonical("pull"),
            Some(IntentType::FeaturePipeline(FeaturePipelineOp::Sync { .. }))
        ));
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
            // "all <topic> <noun>" — the v0.3.0 variant forms (M-04b/d) that used
            // to fall through to the LLM as a bare push.
            ("push all docs changes", "docs"),
            ("push all api changes", "api"),
            ("push all schema changes", "schema"),
            ("push all the login changes", "login"),
        ] {
            match try_canonical(q) {
                Some(IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush {
                    feature_hint,
                })) => assert_eq!(&feature_hint, want, "wrong topic for {:?}", q),
                other => panic!("expected scoped push for {:?}, got {:?}", q, other),
            }
        }
        // Possessive / bare / determiner-only forms must NOT be captured (plain push).
        assert!(try_canonical("push my work").is_none());
        assert!(try_canonical("push my login work").is_none());
        assert!(try_canonical("push everything").is_none());
        assert!(try_canonical("push all changes").is_none()); // determiner + noun only, no topic
        assert!(try_canonical("push the changes").is_none());
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
            // reword/rename WITHOUT the literal word "message" (M-20b variant —
            // "reword the last commit to X" was misread as a new commit).
            (
                "reword the last commit to 'cleaner subject line'",
                "cleaner subject line",
            ),
            ("rename the last commit to 'feat: x'", "feat: x"),
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
    fn test_canonical_commit_all() {
        use crate::commands::{CommitMessage, FileSelection};
        // M-03b regression: commit-ALL language must deterministically STAGE +
        // COMMIT (AddAndCommit), never a stage-only add. "save my changes" used
        // to route (via the LLM) to a bare add that never committed.
        for q in &[
            "commit this",
            "commit everything",
            "commit all my files",
            "commit all my changes",
            "commit my changes",
            "save my changes",
            "save my work",
            "stage and commit",
            "add all and commit",
        ] {
            match try_canonical(q) {
                Some(IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
                    files: FileSelection::All,
                    message: CommitMessage::Draft(_),
                })) => {}
                other => panic!(
                    "expected commit-all AddAndCommit for {:?}, got {:?}",
                    q, other
                ),
            }
        }
        // Bare "commit" (staged-only) must NOT be hijacked — it stays out of the
        // canonical set so bare-commit semantics are preserved (LLM/build path).
        assert!(try_canonical("commit").is_none());
        assert!(try_canonical("commit staged files").is_none());
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

    // ---- Kev routing: repo-state line + deterministic argument capture ----

    fn kctx() -> RepoContext {
        let mut c = RepoContext::empty();
        c.branch = "main".into();
        c.is_empty_repo = false;
        c.situation = crate::commands::RepoSituation::Normal;
        c
    }

    /// Build with an `ask` that must not be called.
    fn build(op: &str, q: &str) -> IntentType {
        build_from_op(op, q, &kctx(), &mut |k| {
            panic!("unexpected ask for {:?}", k)
        })
        .unwrap()
    }

    #[test]
    fn test_repo_state_line_matches_training_shapes() {
        let mut c = kctx();
        assert_eq!(repo_state_line(&c), "clean repo, on branch main");
        c.status.modified = vec!["a.rs".into(), "b.rs".into()];
        c.status.staged = vec!["c.rs".into()];
        c.upstream = Some("refs/remotes/origin/main".into());
        c.ahead = 1;
        assert_eq!(
            repo_state_line(&c),
            "on branch main, 2 files modified, 1 staged file, 1 commit ahead of origin"
        );
        c.situation = crate::commands::RepoSituation::MidRebase {
            current: 2,
            total: 5,
            conflicted: vec![],
        };
        assert!(repo_state_line(&c).ends_with("rebase in progress (2/5)"));
    }

    #[test]
    fn test_repo_state_line_uses_the_upstream_remote_name() {
        let mut c = kctx();
        c.upstream = Some("refs/remotes/upstream/main".into());
        c.behind = 4;
        assert_eq!(
            repo_state_line(&c),
            "on branch main, behind upstream by 4 commits"
        );
    }

    #[test]
    fn test_build_topic_beyond_the_push_verb() {
        for (q, want) in [
            ("ship the payments fixes", "payments"),
            ("commit and push only the billing feature", "billing"),
            ("push everything related to search", "search"),
            ("send up just the cart stuff", "cart"),
        ] {
            match build("scoped_push", q) {
                IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush { feature_hint }) => {
                    assert_eq!(feature_hint, want, "{}", q)
                }
                other => panic!("{}: {:?}", q, other),
            }
        }
    }

    #[test]
    fn test_build_asks_for_a_missing_topic() {
        let mut asked = None;
        let it = build_from_op("scoped_push", "push only some of it", &kctx(), &mut |k| {
            asked = Some(k);
            Ok("export".into())
        })
        .unwrap();
        assert_eq!(asked, Some(ArgKind::Topic));
        assert!(
            matches!(it, IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush { ref feature_hint }) if feature_hint == "export")
        );
    }

    #[test]
    fn test_build_empty_answer_is_an_error_not_a_default() {
        let r = build_from_op("branch", "make me a new branch", &kctx(), &mut |_| {
            Ok("  ".into())
        });
        assert!(r.is_err());
    }

    #[test]
    fn test_build_branch_and_checkout_names() {
        for (q, want) in [
            ("spin up a branch called hotfix", "hotfix"),
            ("start a feature-x branch", "feature-x"),
            ("branch off as release-2.1", "release-2.1"),
        ] {
            assert!(
                matches!(build("branch", q), IntentType::ScopedWrite(ScopedWriteOp::Branch { ref name }) if name == want),
                "{}",
                q
            );
        }
        for (q, want) in [
            ("jump over to the dev branch", "dev"),
            ("hop onto staging", "staging"),
            ("change branch to develop", "develop"),
            ("go to the release-2.1 branch", "release-2.1"),
        ] {
            assert!(
                matches!(build("checkout", q), IntentType::ScopedWrite(ScopedWriteOp::Checkout { ref target }) if target == want),
                "{}",
                q
            );
        }
    }

    #[test]
    fn test_typed_branch_name_reaches_the_suggestion_flow() {
        // An invalid typed name is passed through: the Branch step's validator
        // offers a fixed-up suggestion instead of a dead-end error here.
        let it = build_from_op("branch", "make me a new branch", &kctx(), &mut |_| {
            Ok("feat/Login Page (v2)".into())
        })
        .unwrap();
        assert!(
            matches!(it, IntentType::ScopedWrite(ScopedWriteOp::Branch { ref name }) if name == "feat/Login Page (v2)")
        );
    }

    #[test]
    fn test_build_squash_count_or_ask() {
        assert!(matches!(
            build("squash", "collapse my last 4 commits"),
            IntentType::ScopedWrite(ScopedWriteOp::SquashLastN { n: 4 })
        ));
        assert!(matches!(
            build("squash", "fold the last five together"),
            IntentType::ScopedWrite(ScopedWriteOp::SquashLastN { n: 5 })
        ));
        let it = build_from_op("squash", "squash my recent commits", &kctx(), &mut |_| {
            Ok("three".into())
        })
        .unwrap();
        assert!(matches!(
            it,
            IntentType::ScopedWrite(ScopedWriteOp::SquashLastN { n: 3 })
        ));
        assert!(
            build_from_op("squash", "squash my recent commits", &kctx(), &mut |_| Ok(
                "1".into()
            ))
            .is_err()
        );
    }

    #[test]
    fn test_build_commit_message_is_captured_never_drafted() {
        match build("add_commit", "commit all with message 'add search'") {
            IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
                message: CommitMessage::Provided(m),
                ..
            }) => assert_eq!(m, "add search"),
            other => panic!("{:?}", other),
        }
        match build("add_commit", "record all my changes in a commit") {
            IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
                message: CommitMessage::Draft(d),
                files: FileSelection::All,
            }) => assert!(d.is_empty()),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn test_build_named_files_match_changed_paths() {
        let mut c = kctx();
        c.status.modified = vec!["src/app.py".into(), "README.md".into()];
        c.status.staged = vec!["notes.txt".into()];
        let it = build_from_op(
            "add_commit",
            "commit app.py and readme.md",
            &c,
            &mut |_| unreachable!(),
        )
        .unwrap();
        match it {
            IntentType::ScopedWrite(ScopedWriteOp::AddAndCommit {
                files: FileSelection::Explicit(f),
                ..
            }) => {
                assert_eq!(f, vec!["src/app.py".to_string(), "README.md".to_string()])
            }
            other => panic!("{:?}", other),
        }
        let it = build_from_op(
            "unstage",
            "take notes.txt out of staging",
            &c,
            &mut |_| unreachable!(),
        )
        .unwrap();
        assert!(
            matches!(it, IntentType::ScopedWrite(ScopedWriteOp::Unstage { ref paths }) if paths == &vec!["notes.txt".to_string()])
        );
    }

    #[test]
    fn test_build_amend_keeps_a_quoted_message() {
        match build(
            "amend",
            "change the message of my latest commit to 'add search'",
        ) {
            IntentType::ScopedWrite(ScopedWriteOp::Amend {
                message: Some(m), ..
            }) => assert_eq!(m, "add search"),
            other => panic!("{:?}", other),
        }
        assert!(matches!(
            build("amend", "put app.py into the previous commit"),
            IntentType::ScopedWrite(ScopedWriteOp::Amend {
                message: None,
                include_staged: true
            })
        ));
    }

    #[test]
    fn test_build_refs_urls_and_remotes() {
        assert!(
            matches!(build("revert", "revert commit 9f2e7d1"), IntentType::ScopedWrite(ScopedWriteOp::Revert { ref oid }) if oid == "9f2e7d1")
        );
        assert!(
            matches!(build("revert", "revert the bad one"), IntentType::ScopedWrite(ScopedWriteOp::Revert { ref oid }) if oid.is_empty())
        );
        assert!(
            matches!(build("show", "show me HEAD~2"), IntentType::ReadOnly(ReadOnlyOp::Show { target: Some(ref t) }) if t == "HEAD~2")
        );
        assert!(
            matches!(build("add_remote", "add a remote called upstream at https://github.com/acme/site.git"),
            IntentType::ScopedWrite(ScopedWriteOp::AddRemote { ref name, ref url }) if name == "upstream" && url == "https://github.com/acme/site.git")
        );
        assert!(
            matches!(build("remove_remote", "delete the upstream remote"), IntentType::ScopedWrite(ScopedWriteOp::RemoveRemote { ref name }) if name == "upstream")
        );
        assert!(
            matches!(build("remove_remote", "delete remote"), IntentType::ScopedWrite(ScopedWriteOp::RemoveRemote { ref name }) if name == "origin")
        );
        assert!(
            matches!(build("set_upstream", "make this branch track origin/develop"),
            IntentType::ScopedWrite(ScopedWriteOp::SetUpstream { ref remote, ref branch }) if remote == "origin" && branch == "develop")
        );
    }

    #[test]
    fn test_topic_survives_a_trailing_destination() {
        for q in [
            "push only the login stuff to github",
            "push the login changes to origin",
            "push the login changes to the remote",
        ] {
            assert_eq!(capture_topic(q).as_deref(), Some("login"), "{}", q);
        }
        // "to" that isn't a destination is left alone.
        assert_eq!(capture_topic("push the changes to login"), None);
    }

    #[test]
    fn test_topic_from_owner_v2_phrasings() {
        // eval_set_v2 S08: Kev routed scoped_push at 0.98–1.00, capture missed the topic.
        for q in [
            "only push the search stuff not the docs",
            "commit and push only the search feature files, and leave the docs typo fix uncommitted.",
            "push the search feature files",
        ] {
            assert_eq!(capture_topic(q).as_deref(), Some("search"), "{}", q);
        }
    }

    #[test]
    fn test_missing_argument_message_reads_as_a_sentence() {
        let err = required(&mut |_| Ok(String::new()), ArgKind::Topic)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Couldn't find the feature/topic to push in your request — nothing was done."
        );
    }

    #[test]
    fn test_conflicting_op_catches_a_topic_kev_ignored() {
        // The fine-tune's one confident miss: "push all api changes" → push @0.96.
        for q in [
            "push all api changes",
            "ship the api fixes",
            "Push the Login changes",
        ] {
            assert_eq!(conflicting_op("push", q), Some("scoped_push"), "{}", q);
        }
        assert_eq!(
            conflicting_op("add_commit", "commit and push only the login work"),
            Some("scoped_push")
        );
        // No topic → no conflict; other ops are never second-guessed.
        for q in [
            "push",
            "push my commits",
            "push all changes",
            "push everything",
        ] {
            assert_eq!(conflicting_op("push", q), None, "{}", q);
        }
        assert_eq!(conflicting_op("scoped_push", "push all api changes"), None);
        assert_eq!(conflicting_op("log", "push all api changes"), None);
    }

    #[test]
    fn test_commit_push_stages_then_commits_then_pushes() {
        let it = build("commit_push", "commit everything and push it");
        assert!(
            matches!(
                it,
                IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
                    message: CommitMessage::Draft(_),
                    stage_first: Some(FileSelection::All),
                })
            ),
            "{:?}",
            it
        );
        // A quoted message is captured, never drafted.
        let it = build("commit_push", "commit all with message 'ship it' and push");
        assert!(matches!(
            it,
            IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
                message: CommitMessage::Provided(ref m), ..
            }) if m == "ship it"
        ));
        // The bare phrase is a fast-path canonical.
        assert!(matches!(
            try_canonical("commit and push"),
            Some(IntentType::FeaturePipeline(FeaturePipelineOp::CommitPush {
                stage_first: Some(FileSelection::All),
                ..
            }))
        ));
    }

    #[test]
    fn test_conflicting_op_catches_commit_and_push_split_in_half() {
        // eval_set_v2 S07: "commit n push" → plain push at 0.92 (fine-tune v1).
        assert_eq!(conflicting_op("push", "commit n push"), Some("commit_push"));
        assert_eq!(
            conflicting_op("add_commit", "commit everything and push it to origin"),
            Some("commit_push")
        );
        assert_eq!(
            conflicting_op("commit", "commit and then push"),
            Some("commit_push")
        );
        // A named topic still wins: that is a scoped push.
        assert_eq!(
            conflicting_op("push", "commit and push the billing work"),
            Some("scoped_push")
        );
        // One verb only → no conflict.
        assert_eq!(conflicting_op("push", "push my commits"), None);
        assert_eq!(conflicting_op("add_commit", "commit all my changes"), None);
        assert_eq!(conflicting_op("commit_push", "commit n push"), None);
    }

    #[test]
    fn test_conflicting_op_guards_commit_push_itself() {
        // gitrust-v1 has never seen commit_push; once offered, it picks it for
        // scoped pushes (0.89) and plain commits (0.51) — measured, §12.9.
        assert_eq!(
            conflicting_op("commit_push", "push all schema changes"),
            Some("scoped_push")
        );
        assert_eq!(
            conflicting_op("commit_push", "commit everything"),
            Some("add_commit")
        );
        assert_eq!(
            conflicting_op("commit_push", "commit it all and upload"),
            None
        );
        assert_eq!(
            conflicting_op("commit_push", "commit then send it up"),
            None
        );
        assert_eq!(conflicting_op("commit_push", "commit and push"), None);
    }

    #[test]
    fn test_every_kev_op_builds() {
        // Every routable option must have a builder arm — catches an op added
        // to kev_ops.json without teaching build_from_op (the Kev analogue of
        // the old "ungoverned intent" finding). Arg ops get a canned answer.
        for (op, _) in crate::kev::ops() {
            if op == crate::kev::REJECT {
                continue;
            }
            let r = build_from_op(op, "do it", &kctx(), &mut |k| {
                Ok(match k {
                    ArgKind::SquashCount => "2".into(),
                    ArgKind::RemoteUrl => "https://x.test/r.git".into(),
                    _ => "feature-x".into(),
                })
            });
            assert!(r.is_ok(), "op '{}' has no builder: {:?}", op, r.err());
        }
        for fam in ["rebase", "merge", "cherry_pick", "revert"] {
            let (_, opts) = crate::kev::in_progress_question(fam).unwrap();
            for (op, _) in opts.iter().filter(|(o, _)| o != "something_else") {
                assert!(build_in_progress(op).is_some(), "{}", op);
            }
        }
    }
}
