# Intent-classifier redesign — research notes (routing robustness)

**Status:** research only, no code changes. Decision deferred to the user.
**Date:** 2026-08-10 · **Trigger:** the v0.3.0 phrasing/scope variant run
(M-03/04/10/19/20) exposed that the same behavior asked different ways gives
different results.

---

## 1. The problem, stated precisely

The variant run produced 5 FAIL + 2 defect. Every failure shares one signature:

| Engine marker | Result |
|---|---|
| ⚡ fast-path (`try_canonical`) | always correct |
| ◉ LLM path (qwen2.5:3b) | misrouted every failing variant |

The fast-path recognizes exactly **one canonical phrasing** per operation.
Every other phrasing falls through to the 3B model, which misroutes it. This is
**not** "5 missing rules" — it's a structural weakness: the fallback brain is
too weak, and the deterministic layer is a patch over it.

The user's constraint on any fix (verbatim): *"Solving the regex will not be a
good fix as it expects the user to be rigid — it won't adapt to new or unique
inputs."* Correct. Widening `try_canonical` tables (or the prompt's 40+
`IMPORTANT:` lines, which are the same anti-pattern in prose) does not scale to
phrasings nobody enumerated.

### Two DISTINCT problems (do not conflate)

- **A — Routing.** Which of ~30 known operations does the query mean?
  ("push all docs changes" → scoped-push, not bare push; "squash last three" →
  squash, not undo.) Closed, stable candidate set. **This is a classification
  problem.**
- **B — File scoping.** Given the right operation, which *files* match the named
  feature? ("is `api_client.txt` related to the api feature?") Open-ended,
  genuinely fuzzy content judgment. **Separate, harder.** M-04a/c defects live
  here — the same file was judged "related to login" in one variant and
  "unrelated to api" in another.

This doc is mostly about **A** (the 5 FAILs). B is scoped at the end.

---

## 2. Hardware reality (the binding constraint)

Measured on the dev/target machine:

- **Apple M2, 8 GB RAM, 8 cores.**
- Only model pulled: **qwen2.5:3b (~1.9 GB).**
- Ollama `/api/embed` **works with the already-loaded qwen2.5:3b** — embeddings
  cost **no extra model and no extra RAM**.

Implications:
- A 7B model is borderline on 8 GB (heavy swap) — poor UX on the user's laptop.
- Running a second dedicated embedding model alongside the 3B is tight but not
  impossible; reusing the loaded 3B for embeddings avoids the issue entirely.

---

## 3. Options considered

### Option 1 — Embedding nearest-neighbour routing
Represent each operation by a few exemplar phrasings, embed them once, embed the
query at runtime, route to the nearest operation by cosine similarity. New
phrasings work because they land near a known exemplar — no rule needed. Adding
coverage = adding an example sentence.

### Option 2 — Constrained decoding (grammar / logit mask)
Force the model to emit only a valid op token instead of free JSON. Eliminates
malformed output and shrinks the task. Does **not** fix *semantic* misrouting
("reword" still needs the model to know it means amend).

### Option 3 — Bigger / fine-tuned local model
qwen2.5:7b or a fine-tune. Simplest code change; best raw accuracy. **Rejected
for now on hardware:** 7B on 8 GB M2 is too slow/swappy for an interactive CLI.
Revisit only if the user accepts the latency or targets bigger machines.

### Option 4 — Hybrid: embeddings route, LLM/regex extract arguments
Embeddings pick the operation (robust to phrasing); a focused step extracts the
argument (count, branch, message) only when the op needs one.

---

## 4. Experiments actually run (qwen2.5:3b `/api/embed`)

### 4.1 The failing phrasings — does NN routing fix them?

Exemplars: 1–2 canonical phrasings per op. Query embedded, routed to nearest op.

| query (LLM misrouted these live) | expected | NN top-1 | sim | correct |
|---|---|---|---|---|
| push all docs changes | scoped_push | scoped_push | 0.988 | ✅ |
| push all schema changes | scoped_push | scoped_push | 0.978 | ✅ |
| pull main into feature | sync | sync | 0.883 | ✅ |
| squash the last three commits | squash | squash | 0.997 | ✅ |
| combine the last 3 commits into one | squash | squash | 0.980 | ✅ |
| reword the last commit to 'x' | amend | amend | 0.869 | ✅ |
| change my last commit message to 'x' | amend | amend | 0.891 | ✅ |

**7/7 correct.** Every phrasing the 3B LLM misrouted, embedding NN gets right —
on the same model, no extra RAM. Strong signal *for the in-set case*.

### 4.2 Stress test — ambiguous / out-of-scope (the honest half)

| query | NN top-1 | s1 | s2 | margin | what we WANTED |
|---|---|---|---|---|---|
| undo my last commit but keep the files | add_commit | 0.926 | 0.920 | **+0.006** | undo (gate decides) |
| get rid of my changes | scoped_push | 0.957 | 0.946 | +0.012 | cleanup / clarify |
| what's the weather | status | 0.848 | 0.804 | +0.044 | REJECT (out of scope) |
| make a sandwich | branches | 0.841 | 0.835 | +0.006 | REJECT |
| i want to throw away everything… | branches | 0.914 | 0.896 | +0.018 | cleanup / clarify |
| **undo the last three commits** | **squash** | 0.983 | 0.966 | +0.017 | **halt (undo-multi), NOT squash** |
| show me what changed | branches | 0.933 | 0.927 | +0.006 | diff/log/show (fuzzy) |

### 4.3 What the stress test proves

1. **Absolute cosine is not a confidence signal.** 0.84 even for "make a
   sandwich". qwen2.5:3b is a decoder, not a trained embedder — no usable
   absolute threshold for "I don't know".
2. **Margins are thin and can be wrong.** The right answer loses by 0.006 on a
   close case. Naive single-vector NN is **not** robust by itself; §4.1's clean
   sweep was helped by well-separated exemplars.
3. **No out-of-scope rejection.** Nonsense routes confidently.
4. **A safety-relevant miss:** "undo the last three commits" → `squash`. This
   must halt with guidance (today's M-18 behaviour), never silently squash.

---

## 5. Honest conclusion

- Embeddings on the loaded 3B are **excellent at disambiguating phrasings inside
  the known op set** (the actual v0.3.0 failure) and **cost nothing extra on the
  8 GB machine** — the single most important finding.
- Embeddings alone are **not** a safe drop-in classifier: no rejection, thin
  margins, and they blur safety-critical fine distinctions (undo-multi vs
  squash; undo vs reset-keep-files).

So the shape of a good fix is **hybrid + layered**, not "replace the LLM with
embeddings":

```
query
 └─► try_canonical (exact, instant)           ← unchanged; covers the common exact hits
      └─► EMBEDDING ROUTER (new)                ← picks op by NN over exemplars
           ├─ high margin & in a SAFE op set  → route directly (fast, no LLM)
           ├─ low margin / near a DANGEROUS op → fall to the LLM (or clarify)
           └─ argument still needed (count,     → targeted extract (regex first,
              branch, message, feature)            LLM only if regex misses)
```

Key design rules this data implies:
- **Rank, never threshold on absolute cosine.** Use the *margin* between top-1
  and top-2, and calibrate it (mean-center embeddings first — standard for
  decoder embeddings — then re-measure margins).
- **Safety gate on the router.** A short list of destructive/irreversible ops
  (undo, reset/discard, cleanup, force-push, squash-vs-undo) must NOT be chosen
  on a thin margin — those escalate to the LLM or to a clarify gate. Erring here
  is a data-loss class, not a UX nit.
- **Keep the existing gates.** undo/cleanup already defer the *semantic* to a
  deterministic ChoiceGate — the router only needs to get them *to* that gate,
  which is easier than picking the semantic.
- **Argument extraction stays separate.** "three" → 3 is a normalization
  problem; solve it in the extractor (word-number map), not by praying the LLM
  emits `count:3`.

---

## 6. Open questions to resolve before building

1. **Calibration.** Does mean-centering the 3B embeddings widen the margins
   enough to make a margin threshold trustworthy? (Needs a labelled set — see
   §7.) If not, the router leans more on the LLM for close calls.
2. **Exemplar set + eval harness.** We need a labelled phrasing set (the 16
   variants + a few dozen more, incl. adversarial/out-of-scope) to measure
   router accuracy, not eyeball 7 examples.
3. **Latency budget.** One extra `/api/embed` per query (~exemplars embedded
   once at startup or cached to disk). Measure vs. the current ~10–18 s LLM call
   — embeddings should be *much* faster and could make many queries feel
   instant.
4. **Where the router lives.** New `router.rs` between `try_canonical` and the
   LLM, behind the existing `IntentClassifier` trait so it's swappable/testable.
5. **Fallback contract.** When the router abstains (thin margin / dangerous
   neighbourhood), it must degrade to *exactly today's* LLM path — never worse.

---

## 6b. MEASURED results — the eval harness (44-case labeled set)

Built `docs/classifier_eval/` (throwaway, ships nothing): `eval_set.json` (44
labeled phrasings across 7 buckets), `exemplars.json` (28 ops), `run_eval.py`
(compares embedding-NN vs. the REAL `prompts.rs` classifier on qwen2.5:3b).

**Headline (both engines, fast-path BYPASSED so we measure the fallback brain):**

| | LLM (pure model) | Embedding router |
|---|---|---|
| Routing accuracy | **41%** (18/44) | **64%** (28/44) |
| Latency | ~10 s median | **270 ms** (~36× faster) |
| Out-of-scope rejection | **4/4** | **0/4** |

**By bucket:**

| bucket | LLM | Embeddings | takeaway |
|---|---|---|---|
| variant-M04 (scope) | 0/4 | **4/4** | embeddings fix the v0.3.0 scope failures |
| variant-M19 (squash) | 0/3 | **3/3** | ditto |
| variant-M03 (commit) | 1/3 | **3/3** | ditto |
| unseen paraphrases | 4/12 | **8/12** | embeddings ~2× on unseen phrasing |
| canonical | **5/5** | 3/5 | LLM better on trivial exacts (but fast-path covers these live) |
| out-of-scope | **4/4** | 0/4 | LLM can say "I don't know"; embeddings can't |
| safety | 1/5 | 2/5 | **both bad — see below** |

### The decisive finding: NEITHER engine is safe alone on destructive phrasings

- LLM: "throw away my changes" → `reset`; "discard my local commits" → `reset`
  — routes straight to a **destructive op** instead of the `cleanup` gate.
- Embeddings: "undo the last three commits" → `squash` (must halt, per M-18).

This is a data-loss class, and it **confirms** §5's core claim: the safety gate
on the router is not optional polish — it is the load-bearing part. Whatever
routes, destructive/irreversible neighbourhoods must escalate to a gate, never
route directly on a thin margin.

### Honest caveats on the numbers

1. **41% understates the shipped product.** The harness bypasses
   `try_canonical`, so this is the *pure model* with no fast-path. Live,
   "status/push/undo/squash the last 3" hit the fast-path and are correct. The
   point stands: everything the fast-path misses falls to a ~41% brain.
2. **Strict labels.** `commit`→`add_commit` and `push`→`scoped_push` were scored
   as misses; some are arguably defensible. The dangerous misses (reset for
   cleanup, undo for squash) are not label artifacts — they're real.
3. **Mean-centering HURT** (64% → 57%) — dropped; not the fix here.
4. **Margin is a weak-but-real signal:** correct calls mean margin 0.055 vs
   incorrect 0.018, but both bottom out at 0.001 — heavy overlap. A margin
   threshold catches *some* errors, not a clean separator. It can gate "route
   directly vs. escalate", not serve as a standalone confidence.

### What this means for the decision

- Embeddings **demonstrably fix the exact v0.3.0 paraphrase failures** (M03/04/
  19 buckets) and are **36× faster** — the two tangible wins.
- Embeddings alone are **not** a classifier: 0/4 rejection, 3/5 canonical, unsafe
  on destructive phrasings. A pure swap would trade one failure class for
  another.
- The only defensible design is the **layered hybrid** in §5, with the safety
  gate as a hard requirement — now backed by numbers, not intuition.

## 6c. BUILT — the layered hybrid shipped (v0.3.0-fixes)

The research above was acted on. What shipped:

**New Layer 1.25 — the embedding router** (`gitrust/src/router.rs`). Sits between
`try_canonical` and the LLM. Embeds the query via the already-loaded qwen2.5:3b
`/api/embed` (no extra model), ranks against ~19 op exemplars by cosine, and:
- routes **argument-free safe ops** (status/log/branches/config/fetch/
  merge_preview/push/pull) directly when the top1−top2 margin ≥
  `router_min_margin`;
- for **argument-bearing safe ops** (scoped_push/squash/amend) it picks the op
  by embedding but extracts the ARGUMENT with the existing deterministic
  captures — abstaining if the arg can't be extracted;
- **abstains** (→ unchanged LLM path) on thin margins, out-of-scope, or any
  destructive/context-sensitive op (undo/cleanup/discard_local/force_push/
  reset/checkout/branch). Abstain is always safe.
- Any embedding error is a silent abstain — never a hard failure.
- New `Engine::Router` (`✦`) marker so the user sees when it fired.

**Calibration.** `router_min_margin` default = **0.015**, measured against the
real binary's embeddings (not the harness's): the v0.3.0 target wins land at
margin ≥ ~0.016 while the noise (a status↔fetch confusion at 0.006, "what's the
weather" 0.012) abstains. Config flags: `router_enabled` (default true),
`router_min_margin`.

**The bigger fix the router enabled.** Building the router revealed the v0.3.0
failures were really two-part: the LLM misrouted the OP *and* the deterministic
captures were too rigid on the ARGUMENT. With the router reliably identifying
the op, the captures were widened safely:
- `capture_scoped_push` now strips an "all"/"the" determiner and keys off the
  trailing filter-noun → "push all docs changes" / "push all schema changes"
  now fast-path (M-04b/d).
- `capture_squash_count` now parses spelled-out numbers (two–ten) → "squash the
  last three commits" now fast-paths (M-19b).

Net effect on the v0.3.0 failing phrasings: **all now resolve correctly** —
mostly via ⚡ fast-path (instant), the rest via ✦ router (no LLM), with the LLM
as the safe fallback. Verified live and by unit tests (`router::*`, extended
`test_canonical_parameterized_scoped_push` / `_squash_count`). Full suite 215/0,
clippy/fmt clean.

**One real regression caught by the binary eval (and fixed).** With `branches`/
`config` in the safe-routable set, generic non-git questions ("what time is it",
"make me a sandwich") embedded nearest to the "where am i / what branch am i on"
family and cleared the margin — routing nonsense to `git branch` instead of
letting the LLM reject it. Fix: **drop `branches` and `config` from the
safe-routable set** (they stay as decoy exemplars, so nonsense lands on them and
ABSTAINS). Cost: "where am i right now" now falls to the LLM instead of routing
— an acceptable trade to keep out-of-scope rejection intact. This is the
research's core rule in action: erring toward abstain is always safe.

Final safe-routable set: status, log, fetch, merge_preview, push, pull (+ the
scoped_push/squash/amend arg-ops via deterministic capture).

## 7. Suggested next step (superseded — kept for history)

Build a **throwaway evaluation harness** (script, not shipped): a labelled set
of ~50 phrasings (the 16 variants + hand-written unseen/adversarial ones),
embed-route them, and report:
- routing accuracy vs. the LLM baseline on the same set,
- margin distribution for correct vs. incorrect calls (does a margin threshold
  separate them?),
- effect of mean-centering,
- out-of-scope rejection rate at the chosen threshold.

That turns "7/7 looked great" into a defensible accuracy number and a
calibrated threshold — the evidence needed to commit to (or reject) the hybrid
router before touching `intent.rs`.

---

## 8. Problem B (file scoping) — parked, noted

M-04a/c aren't routing bugs; they're the resolver asking the 3B model "is file
Y related to feature X" over raw diffs — which it answers inconsistently
(`api_client.txt` "related to login" in one run, "unrelated to api" in another).
Options when we get to it: (a) embed the diff and the feature word and rank
(same technique as routing); (b) be *honest* — widen the "uncertain" bucket and
let the user toggle, instead of forcing a confident related/unrelated split the
model can't support. Decide after routing is settled.
