//! Layer 1 / Layer 2 model client — Kev, a local "System One" decision model
//! (github.com/jaredpalmer/kev, Apache-2.0) served by `python -m kev.serve` on
//! localhost. Replaces the Ollama LLM and the embedding router.
//!
//! ## Why a decision model, not a generator
//! Kev never writes text. It reads a short state and returns a CALIBRATED
//! probability for each option we supply. That fits GiTrust's invariant better
//! than a generator did: the model can only ever pick from a closed list we
//! wrote, and it says how sure it is, so the code — not the model — decides
//! whether to act, ask, or refuse. Measured on the 44-case eval
//! (docs/classifier_eval/run_kev_eval.py): 70% top-1 vs the old LLM's 41% and
//! the router's 64%, out-of-scope rejection 4/4, ~0.45 s on the 8 GB M2.
//!
//! ## What the model does NOT do (moved to deterministic code)
//! * Arguments (branch names, counts, files, messages) — Kev can't extract text.
//!   `intent::build_from_op` captures them; if one is missing the user is ASKED.
//! * Commit messages — no drafting. The user writes them.
//!
//! The option list lives in `kev_ops.json`, shared with the training/eval
//! harness. Its order and wording are load-bearing (see that file).

use std::time::Duration;

use anyhow::Context;
use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::config::Config;

/// The `__reject__` option: "not a Git request at all".
pub const REJECT: &str = "__reject__";

#[derive(Deserialize)]
struct OpsDoc {
    instructions: String,
    ops: Vec<(String, String)>,
    in_progress: InProgressDoc,
}

#[derive(Deserialize)]
struct InProgressDoc {
    instructions: String,
    families: std::collections::HashMap<String, Vec<(String, String)>>,
    other: (String, String),
}

static OPS_DOC: Lazy<OpsDoc> = Lazy::new(|| {
    serde_json::from_str(include_str!("kev_ops.json")).expect("kev_ops.json is valid")
});

/// The routing options, in the exact order Kev was trained on: (op, description).
pub fn ops() -> &'static [(String, String)] {
    &OPS_DOC.ops
}

/// The routing question's instructions, verbatim from training.
pub fn instructions() -> &'static str {
    &OPS_DOC.instructions
}

/// The in-progress control question for one family ("rebase", "merge",
/// "cherry_pick", "revert"): (instructions, options incl. `something_else` last).
pub fn in_progress_question(family: &str) -> Option<(&'static str, Vec<(String, String)>)> {
    let doc = &OPS_DOC.in_progress;
    let mut opts = doc.families.get(family)?.clone();
    opts.push(doc.other.clone());
    Some((doc.instructions.as_str(), opts))
}

/// The "none of these" option of the in-progress question.
#[cfg(test)]
pub fn in_progress_other() -> &'static str {
    &OPS_DOC.in_progress.other.0
}

/// Description of one op (for menus: the consequence line).
pub fn describe(op: &str) -> &'static str {
    ops()
        .iter()
        .chain(OPS_DOC.in_progress.families.values().flatten())
        .find(|(o, _)| o == op)
        .map(|(_, d)| d.as_str())
        .unwrap_or("")
}

/// Ops whose wrong selection can rewrite or discard work. They need MORE
/// confidence than a read before Kev routes them without asking — and every one
/// of them still lands on a gate (ChoiceGate / TypeToConfirm) downstream.
pub const DESTRUCTIVE_OPS: &[&str] = &[
    "undo",
    "cleanup",
    "discard_local",
    "force_push",
    "squash",
    "amend",
    "revert",
    "delete_repo",
    // in-progress control: skipping drops a commit, aborting discards the
    // resolution work done so far
    "rebase_skip",
    "rebase_abort",
    "merge_abort",
    "cherry_pick_abort",
    "revert_abort",
];

// =============================================================================
// Routing decision — pure, unit-tested without a server
// =============================================================================

/// What the code does with Kev's distribution. The model never acts; this does.
#[derive(Debug, Clone, PartialEq)]
pub enum KevDecision {
    /// Confident enough to route to `op`.
    Route { op: String, p: f32 },
    /// Confidently not a Git request.
    Reject { p: f32 },
    /// Not confident: show the user these candidates (best first) and let them pick.
    Ask { candidates: Vec<(String, f32)> },
}

/// Decide from a ranked (descending) distribution. Thresholds come from config
/// (`kev_min_confidence`, `kev_destructive_min_confidence`), calibrated on the
/// eval: at p ≥ 0.5 stock Kev-0.8B routed 9/44 with 8 right, none of them
/// flipping under option reordering, and the 1 miss a harmless pull-for-sync.
pub fn decide(ranked: &[(String, f32)], min_conf: f32, destructive_min_conf: f32) -> KevDecision {
    let Some((top, p)) = ranked.first().map(|(o, p)| (o.as_str(), *p)) else {
        return KevDecision::Ask { candidates: vec![] };
    };
    if top == REJECT && p >= min_conf {
        return KevDecision::Reject { p };
    }
    let needed = if DESTRUCTIVE_OPS.contains(&top) {
        destructive_min_conf
    } else {
        min_conf
    };
    if top != REJECT && p >= needed {
        return KevDecision::Route {
            op: top.to_string(),
            p,
        };
    }
    KevDecision::Ask {
        candidates: ranked
            .iter()
            .filter(|(o, _)| o != REJECT)
            .take(3)
            .cloned()
            .collect(),
    }
}

// =============================================================================
// HTTP client
// =============================================================================

fn client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("building HTTP client")
}

fn endpoint(config: &Config, path: &str) -> String {
    format!("{}{}", config.kev_url.trim_end_matches('/'), path)
}

/// POST one System One request and return the `answers` object.
async fn system_one(
    config: &Config,
    state: serde_json::Value,
    questions: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    ensure_running(config).await?;
    let body = serde_json::json!({ "model": "kev-latest", "state": state, "questions": questions });
    let resp = client(Duration::from_secs(120))?
        .post(endpoint(config, "/v1/systemone"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Kev request to {} failed", config.kev_url))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Kev returned HTTP {}: {}", status, text);
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).context("Kev response was not valid JSON")?;
    v.get("answers")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Kev response missing 'answers'"))
}

/// Ask one `choice` question; returns the options ranked by probability.
pub async fn ask_choice(
    config: &Config,
    state: serde_json::Value,
    instructions: &str,
    criteria: &[(String, String)],
) -> anyhow::Result<Vec<(String, f32)>> {
    // Option ORDER is load-bearing (Kev was trained on this order); serde_json's
    // `preserve_order` feature (Cargo.toml) keeps Map insertion order on the wire.
    let mut crit = serde_json::Map::new();
    for (k, v) in criteria {
        crit.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    let q = serde_json::json!({
        "op": { "type": "choice", "instructions": instructions, "criteria": crit }
    });
    let answers = system_one(config, state, q).await?;
    let probs = answers["op"]["probabilities"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Kev answer missing probabilities"))?;
    let mut ranked: Vec<(String, f32)> = probs
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0) as f32))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(ranked)
}

/// Ask several yes/no questions about one state; returns P(yes) per question,
/// in the order given.
pub async fn ask_nouls(
    config: &Config,
    state: serde_json::Value,
    questions: &[String],
) -> anyhow::Result<Vec<f32>> {
    let mut qs = serde_json::Map::new();
    for (i, instr) in questions.iter().enumerate() {
        qs.insert(
            format!("q{}", i),
            serde_json::json!({ "type": "noul", "instructions": instr }),
        );
    }
    let answers = system_one(config, state, serde_json::Value::Object(qs)).await?;
    (0..questions.len())
        .map(|i| {
            answers[format!("q{}", i)]["noul"]
                .as_f64()
                .map(|p| p as f32)
                .ok_or_else(|| anyhow::anyhow!("Kev answer missing q{}", i))
        })
        .collect()
}

// =============================================================================
// Server lifecycle — "runs in the background when needed"
// =============================================================================

/// True when the Kev server answers `/v1/models`.
pub async fn is_reachable(config: &Config) -> bool {
    let Ok(c) = client(Duration::from_secs(2)) else {
        return false;
    };
    matches!(
        c.get(endpoint(config, "/v1/models")).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// Make sure a server is up. If not and `kev_autostart` is on (with `kev_dir`
/// pointing at a Kev checkout), start one detached in the background and wait
/// for it — a cold start is ~25 s on the 8 GB M2, after which it stays resident
/// (~2.9 GB) for later queries. Never starts a second server on a busy port.
pub async fn ensure_running(config: &Config) -> anyhow::Result<()> {
    if is_reachable(config).await {
        return Ok(());
    }
    let Some(dir) = config.kev_dir.as_deref().filter(|_| config.kev_autostart) else {
        anyhow::bail!(
            "The Kev model server isn't running at {}.\n  \
             Start it with `gitrust setup`, or set kev_dir + kev_autostart in ~/.gitrust.toml.",
            config.kev_url
        );
    };
    let port = config
        .kev_url
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
        .unwrap_or(8009);
    eprintln!("  Starting the local Kev model in the background (first run takes ~25 s)…");
    spawn_server(dir, &config.kev_run, port)?;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if is_reachable(config).await {
            return Ok(());
        }
    }
    anyhow::bail!(
        "Started Kev but it never became ready at {}. See {}/serve.log.",
        config.kev_url,
        crate::config::gitrust_dir()?.display()
    )
}

/// Spawn `uv run --extra serve python -m kev.serve` detached, logging to
/// ~/.gitrust/serve.log. Binds 127.0.0.1 only (kev.serve's default).
pub fn spawn_server(kev_dir: &str, run: &str, port: u16) -> anyhow::Result<()> {
    let log_dir = crate::config::gitrust_dir()?;
    std::fs::create_dir_all(&log_dir).ok();
    let log = std::fs::File::create(log_dir.join("serve.log")).context("creating serve.log")?;
    let mut cmd = std::process::Command::new("uv");
    cmd.args([
        "run",
        "--extra",
        "serve",
        "python",
        "-m",
        "kev.serve",
        "--run",
        run,
        "--port",
    ])
    .arg(port.to_string())
    .current_dir(kev_dir)
    .stdin(std::process::Stdio::null())
    .stdout(log.try_clone()?)
    .stderr(log);
    // Own process group: a Ctrl-C at the GiTrust prompt must not take the
    // background server down with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .with_context(|| format!("failed to start Kev via `uv` in {}", kev_dir))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(xs: &[(&str, f32)]) -> Vec<(String, f32)> {
        xs.iter().map(|(o, p)| (o.to_string(), *p)).collect()
    }

    #[test]
    fn ops_load_in_trained_order_with_reject_last() {
        let names: Vec<&str> = ops().iter().map(|(o, _)| o.as_str()).collect();
        assert_eq!(names.len(), 35);
        assert_eq!(names[0], "status");
        assert_eq!(*names.last().unwrap(), REJECT);
        assert!(!instructions().is_empty());
    }

    #[test]
    fn in_progress_families_end_with_something_else() {
        for fam in ["rebase", "merge", "cherry_pick", "revert"] {
            let (instr, opts) = in_progress_question(fam).expect(fam);
            assert!(!instr.is_empty());
            assert_eq!(opts.last().unwrap().0, in_progress_other());
            assert!(opts.len() >= 2);
        }
        assert!(in_progress_question("bisect").is_none());
    }

    #[test]
    fn every_destructive_op_is_a_real_option() {
        for op in DESTRUCTIVE_OPS {
            assert!(!describe(op).is_empty(), "{} missing from kev_ops.json", op);
        }
    }

    #[test]
    fn routes_confident_safe_op() {
        let d = decide(&r(&[("log", 0.73), ("status", 0.1)]), 0.5, 0.6);
        assert_eq!(
            d,
            KevDecision::Route {
                op: "log".into(),
                p: 0.73
            }
        );
    }

    #[test]
    fn destructive_op_needs_the_higher_bar() {
        // 0.55 clears the safe bar but not the destructive one → ask, never act.
        let d = decide(&r(&[("cleanup", 0.55), ("stash", 0.2)]), 0.5, 0.6);
        assert!(matches!(d, KevDecision::Ask { .. }), "got {:?}", d);
        let d = decide(&r(&[("cleanup", 0.65), ("stash", 0.2)]), 0.5, 0.6);
        assert!(matches!(d, KevDecision::Route { ref op, .. } if op == "cleanup"));
    }

    #[test]
    fn confident_reject_is_a_reject() {
        let d = decide(&r(&[(REJECT, 0.8), ("status", 0.05)]), 0.5, 0.6);
        assert_eq!(d, KevDecision::Reject { p: 0.8 });
    }

    #[test]
    fn unsure_asks_with_top3_excluding_reject() {
        let d = decide(
            &r(&[
                (REJECT, 0.3),
                ("push", 0.25),
                ("scoped_push", 0.2),
                ("add_commit", 0.1),
                ("log", 0.05),
            ]),
            0.5,
            0.6,
        );
        match d {
            KevDecision::Ask { candidates } => {
                let names: Vec<_> = candidates.iter().map(|(o, _)| o.as_str()).collect();
                assert_eq!(names, vec!["push", "scoped_push", "add_commit"]);
            }
            other => panic!("expected Ask, got {:?}", other),
        }
    }

    #[test]
    fn empty_distribution_asks() {
        assert_eq!(
            decide(&[], 0.5, 0.6),
            KevDecision::Ask { candidates: vec![] }
        );
    }
}
