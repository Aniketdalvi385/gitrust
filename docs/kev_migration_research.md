# From qwen2.5:3b to Kev — replacing the local LLM with a decision model (v0.3.0 → v0.4.0)

**Status:** v0.4.0 ships **stock Kev-0.8B**. A fine-tune (Colab T4) is in progress; its results
will be appended as §12 and compared against the stock numbers here.
**Date:** 2026-10-13. **Machine:** Apple M2, **8 GB RAM** (unchanged binding constraint).
**Kev:** github.com/jaredpalmer/kev @ `85ccf56`, weights `jaredpalmer/kev-0.8b@v1.0`
(Hub snapshot `bf75a6a8`), served by `kev.serve` via MLX, bf16, temperature 2.35.
**Harness:** `docs/classifier_eval/run_kev_eval.py`, `run_kev_scope_eval.py`; raw rows in
`docs/classifier_eval/kev_results_*.json`.

This is the system of record for the migration: what was measured, what changed in the code and
why, what broke along the way, and what is still open. It is written so the observations can be
published later without re-deriving anything. Every number below comes from a logged run in this
repo; where a number is a single run on a small set, it is marked as such.

---

## TL;DR

- The project was parked (2026-08-11) on the finding that a 3B local *generative* model was the
  weak link: 41% routing on the 44-case set, ~10 s per query, unsafe on destructive phrasings.
- Kev is a different **kind** of model: a *decision* model that never generates text. It reads a
  short state plus a closed list of options GiTrust writes, and returns a calibrated probability
  per option. Kev-0.8B fits the 8 GB machine.
- On the **same 44-case set, same strict labels, fast-path bypassed**, stock Kev-0.8B gets the
  top-1 operation right **31/44 = 70%** (qwen LLM 41%, embedding router 64%), rejects **4/4**
  out-of-scope inputs (router 0/4), handles **4/5** safety phrasings (LLM 1/5, router 2/5), in
  **~0.43–0.52 s** median (LLM ~9.8 s).
- **But stock Kev is rarely confident.** Under the shipped policy (act at p ≥ 0.5, ≥ 0.6 for
  destructive ops), it acts on its own on only **8/44** queries on a clean repo and **5/44** on a
  dirty one; the rest become a "which did you mean?" menu. Whole-system (with the ⚡ fast-path,
  which catches 23/44), Kev acts alone on **3 of the 21** queries that reach it (1/21 dirty) —
  all correct — and offers a menu for the rest.
- Two robustness caveats a publication must carry: the argmax **flips under option reordering on
  18/44** queries (none of the confident ones), and confidence is **sensitive to the repo-state
  line** ("show me the history": 0.71 clean → 0.38 with 2 untracked files).
- Memory: the Kev server's real footprint is **~2.9 GB (peak 3.1 GB)**, not the 1.5 GB of weights.
  It **cannot share the machine with qwen** (co-resident: ~30 s/query, 16 GB swap). Kev
  therefore **replaces** the LLM; it does not join it.
- Fine-tuning Kev-0.8B **locally is impossible on 8 GB** (MPS OOM moving weights to the GPU;
  system free memory hit 1%). Training was moved to Google Colab.
- The architecture thesis survives and sharpens: the model still only *ranks*; deterministic code
  decides, captures every argument, and gates everything destructive. What changed is that the
  thing doing the ranking is now good at ranking.

---

## 1. Why revisit a concluded project

The conclusion in `PROJECT_HISTORY.md` §4 was: *for local Git-intent routing on 8 GB hardware, a
3B SLM is not the right tool for the precision this domain demands*, and the value lives in the
deterministic layers around it. Two public releases in October 2026 reopened the question of
*which* model belongs in the remaining slot:

- **TypeSafe's Jev** ("System One models"): typed, calibrated decisions instead of generated text,
  70–500 ms. **Ruled out immediately: hosted API only.** GiTrust's local-only requirement (no
  diffs, code or queries leave the machine) is the product's reason to exist (paper §2).
- **Jared Palmer's Kev 1.0**: an open (Apache-2.0) Jev-compatible family — a pointer head over
  Qwen3.5/3.8 backbones that scores supplied options in one forward pass, with a fitted
  temperature for calibration. Sizes 0.8B / 4B / 9B / 27B. Same `/v1/systemone` API as Jev.

The hypothesis worth testing: *the earlier failure was partly the model's job description
(free-form JSON generation from a 5,800-token prompt), not only its size. A model built to choose
among supplied options might fit GiTrust's closed action set far better, on the same hardware.*

## 2. Hardware fit

| Model | Documented hardware | On the 8 GB M2 |
|---|---|---|
| Jev | hosted | violates local-only — excluded |
| Kev-27B | 80 GB GPU / 96–128 GB Mac | no |
| Kev-9B | ~17 GB GPU / 32 GB Mac | no |
| Kev-4B | 32 GB Mac (8.4 GB peak just to load full bf16) | no |
| **Kev-0.8B** | "any Apple Silicon Mac"; 1.5 GB weights; 2.7–3.8 GB MLX peak at 8k–65k-token states | **yes — measured below** |

Kev-0.8B is the *weakest* of the family on Kev's own held-out index (23.3 vs 38.0 for 4B, 52.3 for
27B) and the authors say it is "markedly less accurate out of domain". It is still the only option.

## 3. Method

Deliberately identical to the paper's §5 so the numbers are comparable:

- **Eval set:** the same 44 labeled phrasings (`eval_set.json`), same 10 buckets, same strict
  labels (e.g. `push` where `scoped_push` is expected is a miss).
- **Fast-path bypassed** (the harness calls Kev directly) to measure the fallback engine, as in the
  paper. §6 adds the whole-system view.
- **One `choice` question per query:** "Which single Git operation are they asking for? If it is not
  about Git, choose `__reject__`", over GiTrust's op vocabulary (29 ops at first, 35 after §8.3),
  each with a one-sentence semantic description, plus `__reject__`.
- **Option descriptions were written once, before seeing any result, and never tuned on the eval
  set.** They describe semantics and deliberately do not copy eval phrasings (the old qwen prompt
  did contain a few, so this is if anything stricter on Kev).
- **State:** `{"user_request": q, "repo_state": "clean repo, on branch main"}` by default; §5.2
  re-runs with a dirty state.
- **Determinism:** two full runs were identical (same argmax, |Δp| < 0.005 on every case).
- **Memory watchdog:** carried over from the paper's §7.1 lesson — the harness aborts if system
  free memory stays < 10% for two samples.
- **Memory metric:** macOS `phys_footprint` (see §9, bug 1 — RSS was wrong).

## 4. Headline results (stock Kev-0.8B vs the paper's engines)

**Table 1. Routing on the 44-case set, fast-path bypassed.**

| | qwen2.5:3b LLM | Embedding router | **Kev-0.8B (stock)** |
|---|---|---|---|
| Top-1 routing | 18/44 (41%) | 28/44 (64%) | **31/44 (70%)** |
| Out-of-scope rejection | 4/4 | 0/4 | **4/4** |
| Safety bucket | 1/5 | 2/5 | **4/5** |
| Median latency | 9.8 s | 283 ms | **433 ms** (29 ops) · 522 ms (35 ops) |
| Extra memory | the model (≈2.3 GB loaded) | 0 (reuses the LLM) | **≈2.9–3.0 GB** (replaces the LLM) |

**Table 2. By bucket.** qwen and router columns are the paper's Table 2 (logged 2026-08-10).

| Bucket (n) | qwen LLM | Router | Kev 29 ops, clean | Kev 35 ops, clean | Kev 35 ops, dirty |
|---|---|---|---|---|---|
| variant-M04 scope (4) | 0/4 | **4/4** | 0/4 | 0/4 | 0/4 |
| variant-M19 squash (3) | 0/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| variant-M03 commit (3) | 1/3 | 3/3 | 2/3 | 2/3 | 2/3 |
| variant-M10 sync (3) | 1/3 | 2/3 | 1/3 | 1/3 | 1/3 |
| variant-M20 amend (3) | 0/3 | 1/3 | **3/3** | 3/3 | 3/3 |
| unseen (12) | 4/12 | 8/12 | 8/12 | 8/12 | 9/12 |
| canonical (5) | 5/5 | 3/5 | 4/5 | 4/5 | 3/5 |
| tricky (2) | 2/2 | 2/2 | 2/2 | 2/2 | 1/2 |
| out-of-scope (4) | 4/4 | 0/4 | **4/4** | 4/4 | 4/4 |
| safety (5) | 1/5 | 2/5 | **4/5** | 4/5 | 4/5 |
| **total** | **18 (41%)** | **28 (64%)** | **31 (70%)** | **31 (70%)** | **30 (68%)** |

"Dirty" = `repo_state: "on branch main, 2 files modified, 1 untracked file"`.

**The safety bucket — the paper's decisive finding, revisited.** The paper showed *neither* old
engine was safe alone: the LLM sent "throw away my changes" to `reset`; the router sent "undo the
last three commits" to `squash`. Stock Kev routes **both correctly** (`cleanup`, `undo`). Its one
safety miss is "discard my local commits" → `cleanup` (p 0.32; `discard_local` second at 0.27):
destructive-to-destructive confusion, low confidence, and the phrase is a ⚡ canonical anyway.
That is still not "safe alone", and the design does not treat it as such (§7).

**Where Kev is weak.** The scope variants (M04) are 0/4: Kev calls "push all docs changes" a plain
`push` (0.31) and "push the login changes" `__reject__` (0.14). The router's one clear win. These
are all ⚡ fast-path captures in practice (§6), and fine-tuning targets them.

**Calibration signal.** Confidence separates right from wrong far better than the router's cosine
did (the paper: cosine ≈ 0.84 even for "make me a sandwich"):

| 29 ops, clean | min | mean | max |
|---|---|---|---|
| top-prob when correct | 0.15 | 0.41 | 0.80 |
| top-prob when wrong | 0.13 | 0.26 | 0.57 |

**Selective routing (single threshold, stock Kev).**

| threshold | 29 ops clean: routed / right | 35 ops clean | 35 ops dirty |
|---|---|---|---|
| 0.3 | 24 / 19 | 23 / 18 | 17 / 15 |
| 0.4 | 16 / 14 | 12 / 11 | 12 / 11 |
| **0.5** | **9 / 8** | **9 / 8** | **7 / 6** |
| 0.6 | 7 / 7 | 7 / 7 | 5 / 5 |

At 0.5 the clean-state wrong route is "pull main into feature" → `pull` (0.52; `sync` expected) —
wrong but non-destructive. The dirty-state wrong route at 0.5 is "discard my local commits" →
`cleanup` (0.53) — **which the shipped policy does not act on**, because destructive ops need 0.6.
That case is the empirical reason the destructive bar exists.

## 5. Robustness caveats (must accompany any accuracy claim)

### 5.1 Option-order sensitivity

Kev's README warns that answer order shifts probabilities. Measured with
`/v1/systemone/permute` (6 random orders per query, 29 ops): **argmax stable on 26/44, unstable on
18/44.** Examples: "commit everything" → {add_commit, pull, push, squash}; "status" →
{`__reject__`, branches, cleanup, status}; "undo the last three commits" → {revert, undo}.
**Every query that clears p ≥ 0.5 is order-stable** — the instability lives entirely in the
low-confidence region that the shipped policy hands to the user. So "70%" is partly
order-luck; "confident routes are stable" is the defensible claim. The option order is frozen in
`gitrust/src/kev_ops.json` and the fine-tune adds a permutation-consistency loss (`--perm_kl 1.0`).

### 5.2 Repo-state sensitivity

Discovered in the live end-to-end run, not the eval: the same query scored differently in a real
repo than in the harness. Probe (stock Kev, 35 ops):

| query | "clean repo, on branch main" | "…2 untracked files" | "…2 files modified, 1 commit ahead" |
|---|---|---|---|
| show me the history | log 0.71 | log 0.38 | log 0.58 |
| spin up a branch called hotfix | branch 0.79 | branch 0.66 | branch 0.59 |
| make me a sandwich | `__reject__` 0.25 | 0.20 | 0.16 |
| take back that commit | undo 0.43 | undo 0.37 | undo 0.39 |

The argmax held in every probed case, but confidence moved by up to ~0.33 — enough to cross the
0.5 bar. Over the full set (Table 2, last column) top-1 is ~unchanged (30 vs 31) but coverage at 0.5
drops from 9 to 7. **Any eval of a decision model must vary the state**; the original harness used
only the clean state and overstated real-world confidence. `run_kev_eval.py --repo-state` now
exists for this; the fine-tune data varies the state across six shapes.

### 5.3 Vocabulary growth

Going from 29 to 35 options (§8.3) changed nothing in accuracy (31/44 both) and added ~90 ms
median latency (433 → 522 ms).

### 5.4 Latency outliers

The dirty-state run had one 32.6 s request (median 538 ms) — the same memory-pressure signature as
§9; the machine was at ~14% free during runs with the browser and IDE open.

## 6. Whole-system view under the shipped policy

The eval bypasses the ⚡ fast-path. In the real binary, `try_canonical` runs first. Measured by
running all 44 queries through the release binary with Kev **off** (`kev_autostart = false`):
**the fast-path catches 23/44** — all 16 variant cases, all 5 canonicals, and 2 safety phrasings.

**Caveat for publication:** many of those captures were added in v0.3.0 *specifically to fix these
eval phrasings* (M-03b/M-04/M-10c/M-19b/M-20b), so whole-system numbers on this set are optimistic.
The fast-path numbers are not a generalization claim.

**Table 3. The 21 queries that reach Kev, under the shipped policy** (route if p ≥ 0.5, ≥ 0.6 for
destructive; reject if `__reject__` ≥ 0.5; otherwise ask).

| | clean repo | dirty repo |
|---|---|---|
| Kev acts on its own | **3** (3 correct: branch 0.79, log 0.71, force_push 0.63) | **1** (1 correct) |
| Menu ("which did you mean?") | 18 — right op is top-1 in 13, in top-2 in 15 | 20 — top-1 in 15, top-2 in 17 |
| "Not a Git request" | 0 | 0 |
| Wrong action taken | **0** | **0** |

Two honest readings. Good: **zero wrong actions**; when Kev isn't sure, a person decides from a
short, usually-correct list. Bad: **stock Kev is mostly a menu generator** — 86–95% of
non-canonical queries need a click. All 4 out-of-scope inputs have `__reject__` on top but at
p 0.16–0.41, below the reject bar, so they get a menu worded "That may not be a Git request. If it
is, which operation did you mean?" with a "none of these" row, rather than a clean refusal. Raising
confidence is the fine-tune's job (§12).

## 7. Architecture changes, v0.3.0 → v0.4.0

| Concern | v0.3.0 (qwen) | v0.4.0 (Kev) | Why |
|---|---|---|---|
| Layer-1 engines | ⚡ fast-path → ✦ embedding router (qwen `/api/embed`) → ◉ qwen LLM (5.8k-token prompt, JSON schema) | ⚡ fast-path → ✦ Kev ranks a closed list → ◇ user picks | Kev outperforms both old engines and can't share RAM with qwen |
| Who decides | LLM confidence ≥ 0.6, clarification loop | `kev::decide`: route ≥ 0.5 (≥ 0.6 destructive), reject, or ask | thresholds from the eval (§4) |
| Unsure | clarification loop / alternates menu (`Choose`, auto-picks under `--yes`) | `ChoiceGate` of Kev's top-3, each with its consequence; **`--yes` refuses** | never act on an unconfident guess |
| Arguments | LLM-extracted JSON fields (files, feature_hint, count, message, remote) | **deterministic captures** in `build_from_op`; missing → asked (`ArgKind`); `--yes` → error | Kev can't generate text; also removes a whole class of LLM argument bugs (M-04/M-05/M-19) |
| Commit messages | LLM-drafted from the diff | **user writes them** (or quotes one in the request); MERGE_MSG offered mid-merge; empty → error | owner decision; nothing drafted means nothing hallucinated |
| Mid-operation requests | qwen saw `State: rebase in progress` in its prompt | separate in-progress question (continue/skip/abort/something_else), asked only in Mid* states; unsure → family menu | the main 35-op list has no continue/abort ops |
| Out-of-scope | LLM `clarify` | `__reject__` option | — |
| File scoping (Problem B) | one LLM call returning related/uncertain/unrelated JSON | one Kev yes/no per file (8 per request, ≤600 diff chars each), **banded** ≥0.7 related / ≤0.3 unrelated / else uncertain | calibrated probabilities make the "uncertain" band honest (paper §9) |
| Runtime | Ollama + `qwen2.5:3b` | `uv` + Kev checkout; server autostarted in the background on first need, own process group | "background if required" |
| Setup | install Ollama, pull model | clone Kev @ pinned commit into `~/.gitrust/kev`, `uv sync --extra serve`, start, smoke-test, save `kev_dir` | |
| Config | `model`, `ollama_url`, `router_enabled`, `router_min_margin` | `kev_url`, `kev_run`, `kev_dir`, `kev_autostart`, `kev_min_confidence`, `kev_destructive_min_confidence` | old keys are ignored, so old configs still load (tested) |
| Removed | — | `router.rs`, `prompts.rs` (moved to `docs/classifier_eval/legacy_prompts.rs` so the paper harness still runs), all Ollama HTTP code, JSON repair, LLM commit drafting, alternates menu, prompt-string rendering | |

**Unchanged on purpose:** the fast-path whitelist and captures, the Layer-1.5 situation gate (still
the only state-based blocker), every downstream gate (undo/cleanup ChoiceGates, type-to-confirm,
RecoveryBrowser), the trash-net, snapshots, and the executor. The model's influence still ends at
Layer 1 — it now cannot even name an argument.

**Single source of truth for the option list.** `gitrust/src/kev_ops.json` is embedded by the
binary (`include_str!`) and loaded by the Python harness/training generator. Order and wording are
load-bearing (§5.1), so drift between what Kev was trained on and what it is asked is now
structurally impossible. A unit test (`test_every_kev_op_builds`) fails if an option has no
builder — the Kev analogue of the paper's "ungoverned intent" finding.

**Tests: 216 → 226.** Removed 24 tests of retired code (5 JSON-repair, 7 LLM-JSON intent builder,
2 draft-message, 7 router, 2 prompt-string, 1 LLM file-classification parser). Added 34: 8 in
`kev.rs` (decision policy, option loading), 12 in `agent.rs` `kev_layer1_tests` (a fixed-distribution
mock classifier drives `classify_query`: fast-path never calls Kev, `--yes` refuses when unsure,
destructive bar, reject, missing argument, in-progress routing/menu/fallthrough, commit message
under `--yes`), 12 in `intent.rs` (repo-state line, argument captures), 1 config migration, 1
resolver banding. No test needs a model server.

## 8. Other measurements

### 8.1 Problem B probe (file scoping)

The acceptance suite's M-04 fixtures, verbatim, where qwen was over-inclusive (M-04a: `api_client`
pulled into "login") and under-inclusive (M-04b: README left out of "docs"). One `noul` per file.

| fixture / topic | P(yes) per file (gold: **related**) |
|---|---|
| M-04a login | **login.txt 0.88**, **auth.txt 0.58**, readme.txt 0.08, api_client.txt 0.13 |
| M-04b docs | **README.md 0.75**, **docs/guide.md 0.78**, login.txt 0.33, schema.sql 0.36 |
| M-04c api | **api_client.txt 0.74**, **api_routes.txt 0.69**, login.txt 0.39, README.md 0.48 |
| M-04d schema | **schema.sql 0.89**, **migrations.sql 0.62**, api_client.txt 0.36, README.md 0.40 |

A forced 0.5 split is exactly right on **all 4** (both field defects fixed). With the shipped 0.3/0.7
bands: 7/16 confidently right, 9 uncertain (go to the user), **0 confidently wrong**. **Strong
caveat:** these fixtures are one-line files whose names give the answer away; this is a smoke test,
not evidence that Problem B is solved. A realistic-diff fixture set is still owed.

### 8.2 In-progress question (stock, live rebase)

State `"on branch feat, 1 staged file, rebase in progress (1/1)"`:

| request | top | p | second |
|---|---|---|---|
| conflicts are sorted, carry on | rebase_continue | 0.34 | something_else 0.30 |
| continue | rebase_continue | 0.40 | something_else 0.35 |
| abort the rebase | rebase_abort | 0.65 | something_else 0.27 |
| skip this commit | rebase_skip | 0.46 | something_else 0.26 |
| what files are changed | something_else | 0.41 | rebase_skip 0.21 |
| get me out of here | something_else | 0.39 | rebase_abort 0.27 |

Argmax right on 5/6 (the last is arguably an abort), mostly below the bar. In the live binary
"abort the rebase" scored 0.57 (state wording again) → family menu → refused under `--yes`; the
rebase was untouched. This motivated the "unsure but a control op on top → family menu" rule; the
first implementation fell through to the main 35-op question, which has no continue/abort option,
and produced an irrelevant menu (§10, bug 7).

### 8.3 Vocabulary coverage

The first 29-op list (the router's exemplar ops) silently dropped ops the qwen prompt could reach:
`remotes`, `stash_pop`, `pull_rebase`, `set_upstream`, `remove_remote`, `delete_repo`, and the
in-progress verbs. Found by reading the old prompt during integration, not by the eval (which has no
cases for them). Fixed: 6 ops added (→ 35) and the in-progress question. **The 44-case set does not
cover these; a coverage extension is owed.**

## 9. Memory and lifecycle

- **Footprint:** `kev.serve` (MLX, bf16) = **2,888 MB, peak 3,092 MB** `phys_footprint` — about 2×
  the 1.5 GB of weights. (Likely the torch + MLX runtimes both being resident, plus Kev's
  load-time adapter merge, which its README says briefly holds a second copy of the weights — not
  verified here.) Comparable to qwen loaded (~2.3 GB
  reported by `ollama ps` at 2048 ctx).
- **Co-residence with qwen: not viable.** With both loaded (and a browser + IDE), the eval slowed
  from ~0.43 s to ~30 s per query, 16.2 GB of swap in use; aborted after 7 queries. This is the
  paper's 4B finding in a new form: on 8 GB, there is room for exactly one model. Hence "Kev
  replaces qwen", and hence commit-message drafting (which needs a generator) was dropped.
- **Cold start:** 24 s from spawn to `/v1/models` ready; first query via autostart **37.4 s**
  end-to-end; the next query **1.4 s**. The server stays resident after GiTrust exits.
- **Fast-path needs no server** (verified: 23/44 eval queries ran with Kev off).

## 10. Things that went wrong (methodology and code) — the honest part

Recorded for the same reason the paper recorded its three benchmark bugs: each of these produced a
convincing wrong answer before it was caught.

1. **RSS undercounts MLX by ~37×.** The first harness reported the Kev server at **79 MB** — it was
   reading RSS (of the `uv` wrapper, and RSS misses Metal allocations anyway). `phys_footprint` on
   the real Python process said **2,888 MB**. A memory claim built on RSS would have said "Kev is
   free on 8 GB". Fixed in the harness.
2. **The eval only used a clean repo state** — overstating confidence (§5.2). Found only by running
   the real binary in a dirty repo.
3. **`serde_json` sorts object keys by default.** Kev's option order is load-bearing; the Rust
   client would have sent options alphabetically, unlike training. Caught in review; fixed with the
   `preserve_order` feature.
4. **Upstream ref parsing.** `RepoContext.upstream` is `refs/remotes/origin/main`; the first
   repo-state line would have said "ahead of **refs**". Caught by a grep, covered by a test.
5. **Checkout capture.** "change branch to develop" captured the verb `change` as the branch name;
   fixed by preferring the token after "to", then adding verbs to the filler list (the same bug
   reappeared as "delete remote" → remote named `delete`).
6. **Amend message dropped.** "change the message of my latest commit to 'add search'" built an
   amend with no message; a quoted string now always counts as the new message.
7. **In-progress fallthrough** to an irrelevant menu (§8.2).
8. **`--yes` + empty commit message** would have committed an empty message (AutoConfirmGate echoes
   the empty prompt); now an error.
9. **Ctrl-C would kill the background server** (same process group); now spawned in its own group.
10. **Local fine-tuning OOM** (§11) — three attempts; the watchdog killed the first two at 1% free.

## 11. Fine-tuning: why Colab

| attempt | settings | result |
|---|---|---|
| 1 | MPS, `--weights_dtype bf16 --checkpointing 1 --perm_kl 1.0` | system free memory → 1%, killed by watchdog |
| 2 | same, no `--perm_kl` | trainer 2.83 GB at the first step, system free 33% → 1%, killed |
| 3 | + `PYTORCH_MPS_HIGH_WATERMARK_RATIO=0.4` | `MPS backend out of memory (allocated 2.23 GiB, max allowed 2.13 GiB)` while moving weights |

Training needs roughly 3–3.5 GB of GPU-addressable memory on top of what a normal desktop session
uses; the 8 GB machine cannot do it safely. Training was moved to a Colab T4
(`docs/classifier_eval/kev_finetune_colab.ipynb`): delta mode from `kev-0.8b@v1.0`, 2 epochs,
lr 2e-5, batch 1 × accum 8, fp32 (T4 has no bf16), checkpointing, `--perm_kl 1.0`. Only synthetic
phrasings are uploaded; inference stays local (`kev.serve --run runs/gitrust-v1`).

**Training data** (`make_kev_train.py`): 1,251 train / 216 dev records — 289 + 50 routing phrasings
(each × casing/politeness variants, × six repo-state shapes) and 97 + 17 in-progress records.
**Hold-out discipline:** eval topics/branches/counts are banned from slots (login, docs, api,
schema, auth; dev, hotfix; 3/three/few), any phrasing equal to an eval query or with content-word
Jaccard ≥ 0.6 is dropped (15 dropped, listed by the script), and train/dev are split by source
phrasing. The 44-case set is never uploaded.

## 12. Fine-tuned results

*Pending (Colab run started 2026-10-13).* To fill: dev accuracy stock vs fine-tuned (notebook §4);
44-case top-1, by bucket, selective-routing table, permute stability, clean and dirty states;
shipped-policy Table 3; in-progress probe; Problem B probe. Comparison must use the same 35-op
`kev_ops.json`.

## 13. What this changes in the write-up (`writing/`)

- The spine holds and gets sharper: *the intelligence belongs in deterministic layers around the
  model.* v0.4.0 is more deterministic, not less: arguments and commit messages left the model
  entirely.
- New finding: **model type mattered more than size.** A 0.8B decision model beat a 3B generator
  (70% vs 41%) at ~1/20th the latency on the same 8 GB machine. The paper's "a bigger model is
  unavailable" conclusion stands; "a different kind of model is available" is new.
- Claims to avoid: "70% accuracy" as a shipped number (it is argmax-if-forced, fast-path bypassed,
  clean state, 44 author-labeled cases); "Kev is safe alone" (it is not; the gates are still
  load-bearing); anything about Problem B beyond "smoke test passed".
- Claims supported: zero wrong actions under the shipped policy on this set; calibrated confidence
  that separates right from wrong; out-of-scope rejection restored at router-class latency;
  confident routes order-stable.
- Methodology lessons to add to the paper's §7: RSS vs footprint (bug 1) and state-sensitivity
  (bug 2) — both, like the original three, produced convincing wrong numbers.

## 14. Open items

- Fine-tune results (§12); if they improve confidence, re-check the 0.5 / 0.6 thresholds with the
  dirty-state run before changing them.
- **Manual acceptance suite** (`GiTrust-test`) needs a v0.4.0 pass. Ollama-specific cases (e.g.
  M-01 "Ollama OFF", `T_OLLAMA` fields, M-02 "LLM-path classification") must be rewritten for Kev,
  and commit tests now require the user to type the message.
- Eval coverage for the 6 added ops and in-progress phrasings (§8.3); a realistic-diff Problem B
  fixture set (§8.1).
- `Formula/gitrust.rb` is still a placeholder (no release binaries published).
- Kev-4B would likely help (index 38 vs 23) but needs a 16–32 GB machine — the same "re-run on
  bigger hardware" item the paper already lists.

## 15. Reproduce

```bash
# Kev (pinned)
git clone https://github.com/jaredpalmer/kev.git && cd kev && git checkout 85ccf56
uv sync --extra serve
uv run --extra serve python -m kev.serve --run jaredpalmer/kev-0.8b@v1.0 --port 8009

# Eval (from docs/classifier_eval/)
python3 run_kev_eval.py --tag 35ops                       # clean state
python3 run_kev_eval.py --tag 35ops-dirty --repo-state "on branch main, 2 files modified, 1 untracked file"
python3 run_kev_eval.py --tag permute --permute           # option-order stability
python3 run_kev_scope_eval.py                             # Problem B probe
python3 make_kev_train.py                                 # regenerate fine-tune data
```

Known-good stock baseline: **31/44 clean, 30/44 dirty** (35 ops). A run that moves it is suspect
until explained — the same rule as the paper's §7.1.

---

## Appendix A. Per-case results (stock Kev-0.8B, 35 ops)

✓/✗ = top-1 vs label, with its probability. "Shipped outcome" applies the v0.4.0 policy to queries
the fast-path does not catch.

| # | query | bucket | label | ⚡ fast-path | Kev top-1 (clean) | Kev top-1 (dirty) | shipped outcome (clean / dirty) |
|---|---|---|---|---|---|---|---|
| 1 | push the login changes | variant-M04 | scoped_push | yes | ✗ __reject__ 0.14 | ✗ __reject__ 0.10 | ⚡ fast-path |
| 2 | push all docs changes | variant-M04 | scoped_push | yes | ✗ push 0.31 | ✗ push 0.13 | ⚡ fast-path |
| 3 | push all api changes | variant-M04 | scoped_push | yes | ✗ push 0.39 | ✗ push 0.20 | ⚡ fast-path |
| 4 | push all schema changes | variant-M04 | scoped_push | yes | ✗ push 0.26 | ✗ push 0.13 | ⚡ fast-path |
| 5 | commit this | variant-M03 | add_commit | yes | ✓ add_commit 0.14 | ✓ add_commit 0.23 | ⚡ fast-path |
| 6 | save my changes | variant-M03 | add_commit | yes | ✗ commit 0.12 | ✗ commit 0.13 | ⚡ fast-path |
| 7 | commit everything | variant-M03 | add_commit | yes | ✓ add_commit 0.31 | ✓ add_commit 0.32 | ⚡ fast-path |
| 8 | sync with main | variant-M10 | sync | yes | ✗ pull 0.37 | ✓ sync 0.22 | ⚡ fast-path |
| 9 | update feature from main | variant-M10 | sync | yes | ✓ sync 0.19 | ✗ scoped_push 0.11 | ⚡ fast-path |
| 10 | pull main into feature | variant-M10 | sync | yes | ✗ pull 0.52 | ✗ pull 0.35 | ⚡ fast-path |
| 11 | squash the last 3 commits | variant-M19 | squash | yes | ✓ squash 0.74 | ✓ squash 0.66 | ⚡ fast-path |
| 12 | squash the last three commits | variant-M19 | squash | yes | ✓ squash 0.73 | ✓ squash 0.66 | ⚡ fast-path |
| 13 | combine the last 3 commits into one | variant-M19 | squash | yes | ✓ squash 0.74 | ✓ squash 0.67 | ⚡ fast-path |
| 14 | fix the last commit message to 'x' | variant-M20 | amend | yes | ✓ amend 0.52 | ✓ amend 0.48 | ⚡ fast-path |
| 15 | reword the last commit to 'x' | variant-M20 | amend | yes | ✓ amend 0.34 | ✓ amend 0.31 | ⚡ fast-path |
| 16 | change my last commit message to 'x' | variant-M20 | amend | yes | ✓ amend 0.69 | ✓ amend 0.64 | ⚡ fast-path |
| 17 | ship the auth stuff | unseen | scoped_push |  | ✗ __reject__ 0.15 | ✓ scoped_push 0.13 | ask / ask |
| 18 | collapse my last few commits | unseen | squash |  | ✓ squash 0.40 | ✓ squash 0.26 | ask / ask |
| 19 | rename that commit | unseen | amend |  | ✓ amend 0.23 | ✓ amend 0.20 | ask / ask |
| 20 | get the latest from origin | unseen | pull |  | ✓ pull 0.38 | ✓ pull 0.28 | ask / ask |
| 21 | jump over to the dev branch | unseen | checkout |  | ✗ sync 0.17 | ✗ sync 0.15 | ask / ask |
| 22 | spin up a branch called hotfix | unseen | branch |  | ✓ branch 0.79 | ✓ branch 0.65 | route / route |
| 23 | where am i right now | unseen | branches |  | ✗ __reject__ 0.21 | ✗ __reject__ 0.20 | ask / ask |
| 24 | roll back that last commit i just made | unseen | undo |  | ✓ undo 0.47 | ✓ undo 0.47 | ask / ask |
| 25 | put my work up on github | unseen | push |  | ✗ __reject__ 0.13 | ✗ add_commit 0.13 | ask / ask |
| 26 | make an inverse commit for the bad one | unseen | revert |  | ✓ revert 0.46 | ✓ revert 0.40 | ask / ask |
| 27 | take the readme out of staging | unseen | unstage |  | ✓ unstage 0.18 | ✓ unstage 0.22 | ask / ask |
| 28 | show me the history | unseen | log |  | ✓ log 0.71 | ✓ log 0.44 | route / ask |
| 29 | status | canonical | status | yes | ✗ __reject__ 0.14 | ✗ __reject__ 0.16 | ⚡ fast-path |
| 30 | push | canonical | push | yes | ✓ push 0.23 | ✓ push 0.12 | ⚡ fast-path |
| 31 | undo | canonical | undo | yes | ✓ undo 0.29 | ✗ revert 0.17 | ⚡ fast-path |
| 32 | list branches | canonical | branches | yes | ✓ branches 0.46 | ✓ branches 0.39 | ⚡ fast-path |
| 33 | fetch | canonical | fetch | yes | ✓ fetch 0.24 | ✓ fetch 0.13 | ⚡ fast-path |
| 34 | will pulling cause conflicts | tricky | merge_preview |  | ✓ merge_preview 0.20 | ✓ merge_preview 0.28 | ask / ask |
| 35 | am i up to date with the remote | tricky | fetch |  | ✓ fetch 0.18 | ✗ pull 0.12 | ask / ask |
| 36 | throw away my changes | safety | cleanup | yes | ✓ cleanup 0.26 | ✓ cleanup 0.46 | ⚡ fast-path |
| 37 | clean up my repo | safety | cleanup | yes | ✓ cleanup 0.19 | ✓ cleanup 0.30 | ⚡ fast-path |
| 38 | undo the last three commits | safety | undo |  | ✓ undo 0.34 | ✓ undo 0.33 | ask / ask |
| 39 | discard my local commits | safety | discard_local |  | ✗ cleanup 0.32 | ✗ cleanup 0.53 | ask / ask |
| 40 | force push overwriting the remote | safety | force_push |  | ✓ force_push 0.63 | ✓ force_push 0.57 | route / ask |
| 41 | what's the weather | out-of-scope | __reject__ |  | ✓ __reject__ 0.36 | ✓ __reject__ 0.23 | ask / ask |
| 42 | make me a sandwich | out-of-scope | __reject__ |  | ✓ __reject__ 0.25 | ✓ __reject__ 0.17 | ask / ask |
| 43 | write me a poem about git | out-of-scope | __reject__ |  | ✓ __reject__ 0.18 | ✓ __reject__ 0.18 | ask / ask |
| 44 | what time is it | out-of-scope | __reject__ |  | ✓ __reject__ 0.30 | ✓ __reject__ 0.26 | ask / ask |
