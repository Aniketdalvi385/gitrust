//! Layer 1.25 — the embedding router.
//!
//! Sits between the exact fast-path (`try_canonical`) and the LLM. It routes a
//! paraphrase to a known operation by nearest-exemplar cosine similarity over
//! the *already-loaded* model's `/api/embed` (no second model — verified on the
//! 8 GB target machine).
//!
//! ## Why it exists
//! The v0.3.0 variant run proved the 3B LLM misroutes paraphrases it wasn't
//! prompted for ("push all docs changes", "squash the last three", "reword the
//! last commit"). The eval harness (docs/classifier_eval) measured embedding
//! nearest-neighbour at ~64% vs the LLM's ~41% on the same set, and ~30× faster.
//!
//! ## Why it is DELIBERATELY conservative (the load-bearing part)
//! The same eval proved embeddings alone are NOT a safe classifier: they cannot
//! reject out-of-scope input (0/4), and they blur safety-critical distinctions
//! (e.g. "undo the last three commits" embeds nearest to `squash`, which would
//! destroy work).
//!
//! So the router only ACTS when ALL of these hold, else it abstains to the LLM.
//! (a) The query is not empty / trivially short. (b) The top match is in
//! [`SAFE_ROUTABLE_OPS`] — a hand-picked allowlist of NON-destructive,
//! ARGUMENT-FREE operations. Destructive ops (undo, cleanup, discard_local,
//! force_push, reset, squash) and argument-bearing ops (scoped_push, checkout,
//! branch, amend-with-message, sync) are NEVER routed here — they need the
//! deterministic captures in `try_canonical` (already tried before us) or the
//! LLM/gate. (c) The top1−top2 margin clears `config.router_min_margin`.
//!
//! Abstaining is always safe: it degrades to exactly today's LLM path.

use crate::commands::{IntentType, ReadOnlyOp, ScopedWriteOp};
use crate::config::Config;

/// Exemplar phrasings per operation. Kept small and human-editable — adding
/// coverage for a new phrasing is adding a sentence here, NOT a regex. Mirrors
/// docs/classifier_eval/exemplars.json (the eval harness), restricted to the
/// safe-routable set plus a few decoy ops so nearest-neighbour has real
/// competition (a query near a decoy op correctly fails the allowlist check and
/// abstains rather than mis-firing).
const EXEMPLARS: &[(&str, &[&str])] = &[
    // ---- SAFE, argument-free (these MAY route directly; see SAFE_ROUTABLE_OPS)
    (
        "status",
        &["show status", "what is the repo status", "is my tree clean"],
    ),
    (
        "log",
        &["show commit history", "show the log", "list recent commits"],
    ),
    (
        "branches",
        &[
            "list branches",
            "what branch am i on",
            "show all branches",
            "where am i right now",
        ],
    ),
    (
        "config",
        &["what is my git user and email", "show git config"],
    ),
    (
        "fetch",
        &[
            "fetch from origin",
            "check the remote for new commits",
            "am i behind origin",
            "am i up to date with the remote",
        ],
    ),
    (
        "merge_preview",
        &[
            "will a pull cause conflicts",
            "preview merge conflicts",
            "is it safe to pull",
        ],
    ),
    (
        "push",
        &[
            "push",
            "publish my current branch",
            "put my work up on github",
            "push my commits to origin",
        ],
    ),
    (
        "pull",
        &[
            "pull the latest changes",
            "get the latest from origin",
            "pull from origin",
        ],
    ),
    // ---- DECOY ops (present ONLY so NN has competition; NOT in the allowlist,
    //      so a top-1 here forces an abstain, never a wrong direct route).
    (
        "add_commit",
        &[
            "commit all my files",
            "stage everything and commit",
            "save my changes",
        ],
    ),
    (
        "scoped_push",
        &["push the login changes", "push all api changes"],
    ),
    (
        "amend",
        &["fix the last commit message", "reword the last commit"],
    ),
    (
        "squash",
        &[
            "squash the last commits",
            "combine my last commits into one",
        ],
    ),
    (
        "undo",
        &["undo the last commit", "take back my last commit"],
    ),
    ("revert", &["revert that commit", "make an inverse commit"]),
    (
        "checkout",
        &["switch to another branch", "go to the dev branch"],
    ),
    (
        "cleanup",
        &[
            "clean up my repo",
            "throw away all my changes",
            "start fresh",
        ],
    ),
    (
        "discard_local",
        &["discard my local commits", "reset hard to origin"],
    ),
    (
        "force_push",
        &["force push with lease", "overwrite the remote"],
    ),
    ("sync", &["sync with main", "update my branch from main"]),
];

/// Operations the router is allowed to emit DIRECTLY. Deliberately small: only
/// non-destructive, argument-free reads/writes whose meaning is fully carried by
/// the op alone. Anything that (a) can lose work or (b) needs a free-form
/// argument is excluded on purpose — see the module docs.
///
/// `branches` and `config` are DELIBERATELY excluded despite being harmless
/// reads: the binary eval showed generic non-git questions ("what time is it",
/// "make me a sandwich") embed nearest to "where am i / what branch am i on" and
/// cleared the margin — routing nonsense to `git branch` instead of letting the
/// LLM reject it. The out-of-scope-rejection weakness (research §4.3) makes the
/// "where/what am i" family too absorbent, so we drop it here and let those two
/// ops fall to the LLM (which rejects nonsense correctly).
const SAFE_ROUTABLE_OPS: &[&str] = &[
    "status",
    "log",
    "fetch",
    "merge_preview",
    "push", // plain push to origin — no scope, no force
    "pull",
];

/// Operations that carry a NON-destructive argument (a scope topic, a squash
/// count, a new commit message). These caused the v0.3.0 variant failures —
/// paraphrases the deterministic captures didn't recognise fell to the LLM,
/// which misrouted them. The router adds robustness by picking the OP via
/// embeddings, then extracting the ARGUMENT with the same deterministic capture
/// the fast-path uses. If embeddings pick the op but the argument can't be
/// extracted, the router ABSTAINS (→ LLM) — it never emits an argument-bearing
/// op with a guessed/missing argument.
///
/// Excluded on purpose: undo, cleanup, discard_local, force_push, reset,
/// checkout/branch (a wrong branch name is disruptive) — destructive or
/// context-sensitive enough that a gate or the LLM should own them.
const SAFE_ARG_OPS: &[&str] = &["scoped_push", "squash", "amend"];

/// Cosine similarity of two equal-length vectors.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    dot / (na.sqrt() * nb.sqrt() + 1e-9)
}

/// Map a safe-routable op name to its concrete `IntentType`. Only the
/// argument-free ops in [`SAFE_ROUTABLE_OPS`] are handled; anything else returns
/// `None` (defensive — the caller already gates on the allowlist).
fn op_to_intent(op: &str) -> Option<IntentType> {
    let it = match op {
        "status" => IntentType::ReadOnly(ReadOnlyOp::Status),
        "log" => IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }),
        "branches" => IntentType::ReadOnly(ReadOnlyOp::Branches),
        "config" => IntentType::ReadOnly(ReadOnlyOp::Config),
        "fetch" => IntentType::ReadOnly(ReadOnlyOp::FetchStatus {
            remote: "origin".to_string(),
        }),
        "merge_preview" => IntentType::ReadOnly(ReadOnlyOp::MergePreview {
            remote: "origin".to_string(),
        }),
        "push" => IntentType::ScopedWrite(ScopedWriteOp::Push {
            remote: "origin".to_string(),
        }),
        "pull" => IntentType::ScopedWrite(ScopedWriteOp::Pull {
            remote: "origin".to_string(),
        }),
        _ => return None,
    };
    Some(it)
}

/// The result of a router decision, for logging/telemetry and to make the
/// abstain reasons explicit (and testable).
#[derive(Debug, Clone, PartialEq)]
pub enum RouteDecision {
    /// Confidently routed to a safe, argument-free op — build directly.
    Routed { op: String, margin: f32 },
    /// Confidently picked an argument-bearing op — the caller must extract the
    /// argument deterministically (`build_arg_op`) and abstain if it can't.
    RoutedArg { op: String, margin: f32 },
    /// Abstained — the LLM path must run. `why` is a short reason.
    Abstain { why: &'static str },
}

/// Embed the exemplar phrases once. Returns (op, vector) pairs — one per
/// exemplar phrase (an op with 3 phrases contributes 3 rows). Errors propagate
/// so the caller can abstain gracefully if Ollama's embed endpoint is down.
pub async fn embed_exemplars(config: &Config) -> anyhow::Result<Vec<(String, Vec<f32>)>> {
    let mut phrases: Vec<String> = Vec::new();
    let mut owners: Vec<String> = Vec::new();
    for (op, exs) in EXEMPLARS {
        for e in *exs {
            phrases.push((*e).to_string());
            owners.push((*op).to_string());
        }
    }
    let vecs = crate::intent::embed_texts(&phrases, config).await?;
    Ok(owners.into_iter().zip(vecs).collect())
}

/// Core routing decision given a query and pre-embedded exemplars. Pure and
/// synchronous (the embedding call is done by the caller), so it is unit-tested
/// with hand-built vectors — no network.
pub fn decide(
    query: &str,
    query_vec: &[f32],
    exemplars: &[(String, Vec<f32>)],
    min_margin: f32,
) -> RouteDecision {
    // Guard trivially short / empty input — nothing to route.
    if query.trim().len() < 2 || exemplars.is_empty() {
        return RouteDecision::Abstain { why: "empty/short" };
    }

    // Best cosine per op.
    let mut best: Vec<(f32, &str)> = Vec::new();
    for (op, v) in exemplars {
        let s = cosine(query_vec, v);
        match best.iter_mut().find(|(_, o)| *o == op.as_str()) {
            Some(slot) => {
                if s > slot.0 {
                    slot.0 = s;
                }
            }
            None => best.push((s, op.as_str())),
        }
    }
    best.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let (s1, top1) = best[0];
    let s2 = best.get(1).map(|(s, _)| *s).unwrap_or(0.0);
    let margin = s1 - s2;

    // Margin gate first — a thin margin is untrustworthy regardless of which op
    // won (the eval showed correct/incorrect margins overlap in the low range).
    if margin < min_margin {
        return RouteDecision::Abstain { why: "low margin" };
    }
    // Argument-free safe op → route directly.
    if SAFE_ROUTABLE_OPS.contains(&top1) {
        return RouteDecision::Routed {
            op: top1.to_string(),
            margin,
        };
    }
    // Argument-bearing safe op → the caller must extract the arg deterministically.
    if SAFE_ARG_OPS.contains(&top1) {
        return RouteDecision::RoutedArg {
            op: top1.to_string(),
            margin,
        };
    }
    // Anything else (destructive / context-sensitive decoy) must NOT route —
    // abstain so the LLM / deterministic captures / gates handle it.
    RouteDecision::Abstain {
        why: "top op not safe-routable",
    }
}

/// Full router entry point: embed the query, decide, and (if routed) build the
/// concrete `IntentType`. Returns:
///
/// * `Ok(Some(intent))` — routed confidently; skip the LLM.
/// * `Ok(None)` — abstained; the caller runs the LLM path unchanged.
/// * `Err(_)` — embedding call failed; the caller treats it as an abstain
///   (never a hard failure — degrade to LLM).
pub async fn try_route(
    query: &str,
    exemplars: &[(String, Vec<f32>)],
    config: &Config,
) -> anyhow::Result<Option<IntentType>> {
    let qv = crate::intent::embed_texts(&[query.to_string()], config).await?;
    let qv = qv.into_iter().next().unwrap_or_default();
    let debug = std::env::var("GITRUST_DEBUG").is_ok();
    match decide(query, &qv, exemplars, config.router_min_margin) {
        RouteDecision::Routed { op, margin } => {
            if debug {
                eprintln!("  (router: '{}' → {} margin={:.3})", query, op, margin);
            }
            Ok(op_to_intent(&op))
        }
        RouteDecision::RoutedArg { op, margin } => {
            // Embeddings picked the op; the argument MUST come from the
            // deterministic capture. If it can't be extracted, abstain → LLM
            // (never emit an arg-bearing op with a missing argument).
            match crate::intent::build_arg_op(&op, query) {
                Some(intent) => {
                    if debug {
                        eprintln!(
                            "  (router: '{}' → {} (+arg) margin={:.3})",
                            query, op, margin
                        );
                    }
                    Ok(Some(intent))
                }
                None => {
                    if debug {
                        eprintln!("  (router: picked {} but arg capture failed → LLM)", op);
                    }
                    Ok(None)
                }
            }
        }
        RouteDecision::Abstain { why } => {
            if debug {
                eprintln!("  (router abstained: {})", why);
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a tiny synthetic exemplar set with orthogonal-ish vectors so the
    // decision logic is tested WITHOUT any network/embedding.
    fn ex(op: &str, v: Vec<f32>) -> (String, Vec<f32>) {
        (op.to_string(), v)
    }

    #[test]
    fn routes_safe_op_on_high_margin() {
        // query is closest to "status"; margin large.
        let exs = vec![
            ex("status", vec![1.0, 0.0, 0.0]),
            ex("log", vec![0.0, 1.0, 0.0]),
            ex("push", vec![0.0, 0.0, 1.0]),
        ];
        let q = vec![0.98, 0.05, 0.0];
        let d = decide("what's my status", &q, &exs, 0.04);
        assert!(
            matches!(d, RouteDecision::Routed { ref op, .. } if op == "status"),
            "got {:?}",
            d
        );
    }

    #[test]
    fn abstains_when_top_op_is_dangerous_decoy() {
        // query closest to "undo" (a decoy, NOT in the allowlist) → must abstain
        // even though the margin is huge. This is the safety-critical case.
        let exs = vec![
            ex("undo", vec![1.0, 0.0, 0.0]),
            ex("status", vec![0.0, 1.0, 0.0]),
        ];
        let q = vec![0.99, 0.01, 0.0];
        let d = decide("roll everything back", &q, &exs, 0.04);
        assert!(
            matches!(d, RouteDecision::Abstain { why } if why == "top op not safe-routable"),
            "dangerous decoy must never route directly; got {:?}",
            d
        );
    }

    #[test]
    fn routes_arg_op_as_routedarg_not_direct() {
        // query closest to "squash" (an arg-bearing safe op) with good margin →
        // RoutedArg, so the caller extracts the count. NOT a direct Routed.
        let exs = vec![
            ex("squash", vec![1.0, 0.0, 0.0]),
            ex("status", vec![0.0, 1.0, 0.0]),
        ];
        let q = vec![0.98, 0.02, 0.0];
        let d = decide("collapse my last few commits", &q, &exs, 0.04);
        assert!(
            matches!(d, RouteDecision::RoutedArg { ref op, .. } if op == "squash"),
            "arg-bearing op must be RoutedArg; got {:?}",
            d
        );
    }

    #[test]
    fn abstains_on_low_margin_between_safe_ops() {
        // status vs log nearly tied → abstain to the LLM.
        let exs = vec![ex("status", vec![1.0, 0.0]), ex("log", vec![0.99, 0.0])];
        let q = vec![1.0, 0.0];
        let d = decide("show me stuff", &q, &exs, 0.04);
        assert!(
            matches!(d, RouteDecision::Abstain { why } if why == "low margin"),
            "got {:?}",
            d
        );
    }

    #[test]
    fn abstains_on_empty_query() {
        let exs = vec![ex("status", vec![1.0])];
        let d = decide("", &[1.0], &exs, 0.04);
        assert!(matches!(d, RouteDecision::Abstain { why } if why == "empty/short"));
    }

    #[test]
    fn safe_routable_ops_are_all_buildable() {
        // Every op we claim is safe-routable must map to a concrete intent —
        // guards against listing an op in the allowlist but forgetting op_to_intent.
        for op in SAFE_ROUTABLE_OPS {
            assert!(
                op_to_intent(op).is_some(),
                "safe-routable op '{}' has no op_to_intent arm",
                op
            );
        }
    }

    #[test]
    fn every_safe_op_has_exemplars() {
        // A safe-routable op with no exemplars can never win NN — catch that.
        for op in SAFE_ROUTABLE_OPS {
            assert!(
                EXEMPLARS.iter().any(|(o, _)| o == op),
                "safe-routable op '{}' has no exemplars",
                op
            );
        }
    }
}
