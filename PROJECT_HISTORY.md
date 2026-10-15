# GiTrust — Project History

*How this was built, what it changed at each stage, what it found, why it was
parked — and how it came back. Sections 1–5 were written on 2026-08-11, when the
project was deliberately parked, and are kept as written; section 6 covers the
October 2026 revival (v0.4), which changed the conclusion.*

**Active span:** 2026-07-13 → 2026-08-11 (~4 weeks), then 2026-10-13 → 2026-10-15.
Today: ~23,000 lines of Rust across 24 modules, 287 tests, v0.4.1. In August: one
language-model swap that never happened and one genuinely useful negative result.
In October: the swap that did happen — to a different *kind* of model.

---

## 1. The original thesis

The goal was a **safety layer for Git** driven by a small, **local** language
model. The pitch, in one sentence: *let a local SLM read what a person means, and
wrap every dangerous Git operation in deterministic guardrails with a roll-back
option at each individual step.*

The three commitments that defined it:

1. **Natural language in.** "Push the login changes," not
   `git add … && git commit -m … && git push`.
2. **Local only.** Intent classification on `qwen2.5:3b` via Ollama — no diffs,
   code, or queries leaving the machine. That was a hard product requirement, not
   a convenience.
3. **Safe by construction.** Destructive operations spelled out before they run,
   snapshotted before they execute, recoverable after. The LLM interprets; it
   never executes.

The binding constraint that shaped everything downstream: the target machine is
an **Apple M2 with 8 GB of RAM.** That number is why the model is 3B and not 7B,
and it is why the project ended where it did.

---

## 2. How it grew — the arc, stage by stage

The build ran in three distinct eras.

### Era 1 — The feature build (mid-July, as "GLit")

The project began as a large, already-substantial baseline and grew through a
disciplined phase plan (`docs/Build_phases_v3.md`), one phase per working
session, never advancing until the previous phase's tests passed.

| Stage | What landed | Why it mattered |
|---|---|---|
| **Baseline (phases 1–10)** | The whole **5-layer pipeline** — Context → Intent → Resolve → Validate/gate → Execute — with trait seams at every layer for testing, and crash-recovery snapshotting. | Established the spine: data flows one direction, each layer narrowing ambiguity until the executor gets fully-concrete steps. |
| **Review hardening** | `build_context` **fails closed** instead of silently treating an unknown repo as "not a repo" (which had let the gate allow every write); state-gate refusals return a **nonzero exit code** instead of `Ok`/exit 0. | The first appearance of the theme that would dominate: *a not-done operation must never look like success.* |
| **Phase 11–12 — divergence reconciliation** | The flagship flow: on a diverged branch, fetch → a **rebase-vs-merge ChoiceGate** (each option stating its consequence) → execute → conditional push, with a calm hand-off on conflict. | The scenario that motivated the whole project — "I committed, fetched, and there's a commit I never pulled" — got a safe, guided answer. A mid-pipeline conflict **pauses with a nonzero error**; a half-done rebase is not success. |
| **Phase 13 — the undo family** | All undo language → a single ambiguous `UndoIntent`, resolved by a deterministic **four-option ChoiceGate** (keep-staged / keep-in-files / revert / discard). The **LLM never picks the destructive path.** "Undo the last 3" deliberately **halts** rather than trusting an LLM-extracted count. Rewriting *published* history escalates to `--force-with-lease`. | This is the clearest statement of the core safety idea: the model gets you to the gate; the gate — deterministic code — decides the outcome. |
| **Phase 14 — forensics & the trash-net** | `find_lost_commits` (two-source scan: reflogs **and** dangling objects — the only way to catch a dropped stash); a `RecoveryBrowser` that restores to a fresh `rescue/*` branch **without touching current state**; the **trash-net invariant** — anything deleting content not in Git's object database backs it up first, **fail-closed**. | Closed the "I lost my work" origin story and the last destructive-safety gaps. Untracked deletion is graded *above* hard reset — hard reset has the reflog; untracked files have only the trash-net. |

By mid-July the v0.2 feature set was complete and merged to `main`.

### Era 2 — The rename (late July)

A single, deliberately mechanical commit renamed the project **GLit → GiTrust**
(the old name collided with a published OSINT tool; the new one was verified
available). History-preserving `git mv` across 39 files, every identifier, path,
config location, and user-facing string. No alias, no fallback — a clean total
rename, with the version bump split into its own commit to keep the rename a pure
rename.

### Era 3 — The acceptance-fix campaign (early August) — where the real story is

This is where the project's central lesson emerged. A **manual acceptance test
suite** (human-eyes testing of the release binary — codes `M-xx`) was run against
v0.2, and it exposed a pattern the unit tests hadn't: the tool behaved correctly
for the *one canonical phrasing* of each operation, and misrouted almost
everything else.

The fixes, in order:

- **The phantom reset (a P0 data-integrity bug).** `reset` always reset to the
  *current* HEAD — a silent no-op — because the pipeline step carried no target
  oid. It reported success while HEAD never moved. Fixed by threading an explicit
  target through the step.
- **Exit-code and argument-extraction fixes.** Refusals that returned exit 0;
  targets (branch names, squash counts) dropped on the way to the executor. Both
  patched with deterministic captures and honest exit codes.
- **The two "passed-with-defects."** Two cases that superficially passed but hid
  latent bugs — a staging omission on a detached-HEAD rescue, and a hook-blocked
  commit whose failure was being swallowed into exit 0.

Then the campaign hit the question that ended up defining the project.

---

## 3. The central finding: where a small local LLM actually belongs

### The observation

A **multi-variant test run** — the same operation phrased many different ways
("commit the login changes," "push all the docs changes," "push all schema
changes," "combine the last three commits") — made the pattern undeniable:

| Layer-1 engine | Result on paraphrases |
|---|---|
| ⚡ fast-path (exact phrase) | always correct |
| ◉ LLM path (`qwen2.5:3b`) | misrouted nearly every variant |

The obvious "fix" was to widen the exact-match tables with more phrasings. The
project's owner rejected that explicitly, and correctly:

> *"Solving the regex will not be a good fix as it expects the user to be rigid —
> it won't adapt to new or unique inputs."*

Widening rule tables (or piling `IMPORTANT:` lines into the prompt, which is the
same anti-pattern in prose) does not scale to phrasings nobody enumerated. The
weakness was structural: **the fallback brain was too weak, and the deterministic
layer was a patch over it.**

### The experiment (measured, not guessed)

Rather than guess, a throwaway **evaluation harness** was built
(`docs/classifier_eval/`, ships nothing): a 44-case labeled phrasing set across
buckets (canonical, unseen paraphrases, scope variants, safety-critical,
out-of-scope), run through two engines on the same model — the real classifier
prompt vs. embedding nearest-neighbour routing. The headline, with the fast-path
deliberately bypassed so it measured the *fallback*:

| | LLM (pure model) | Embedding router |
|---|---|---|
| Routing accuracy | **41%** | **64%** |
| Median latency | ~10 s | **~270 ms** (~36× faster) |
| Out-of-scope rejection | **4/4** | **0/4** |

But the decisive finding was in the *safety* bucket: **neither engine was safe
alone on destructive phrasings.** The LLM routed "throw away my changes" straight
to `reset`. Embeddings routed "undo the last three commits" to `squash` — a
data-loss class. This confirmed that whatever routes, **the safety gate is the
load-bearing part, not polish.**

### What shipped as a result

A new **Layer 1.25 — the embedding router** (`gitrust/src/router.rs`). It embeds
the query via the *already-loaded* `qwen2.5:3b` (no second model — critical on
8 GB), ranks against hand-written op exemplars by cosine similarity, and:

- routes **argument-free, safe** operations directly when the top-1/top-2 margin
  clears a calibrated threshold;
- for a few argument-bearing safe ops, picks the operation by embedding but
  extracts the argument with deterministic capture, **abstaining if it can't**;
- **abstains** — degrading to exactly the unchanged LLM path — on thin margins,
  out-of-scope input, or *any* destructive/context-sensitive operation.

Abstaining is always safe; an embedding error is a silent abstain, never a hard
failure. A real regression the eval caught and fixed: with "branches"/"config" in
the safe-routable set, "what time is it" embedded near "what branch am I on" and
confidently routed nonsense to `git branch` — so those were dropped from the
allowlist (kept only as decoy exemplars that *cause* an abstain). The research's
core rule, in action: **erring toward abstain is always safe.**

The remaining paraphrase failures were then closed by *widening the deterministic
captures safely* (now that the router reliably identified the operation) — e.g.
"push all docs changes," "squash the last three commits," "reword the last commit
to X," "pull main into feature." All resolve correctly now: mostly via ⚡
fast-path, the rest via ✦ router, with the LLM as the safe fallback.

### The model-swap question — a clean negative result

The last investigation asked: *is there a better local model than qwen2.5:3b?*
The answer, measured on the 8 GB target, was **no — stay put:**

- GiTrust's real classifier prompt is **~5,800 tokens**, so it needs a 2048-token
  context. A **4B model at that context OOM-crashed the machine** during a full
  run, and sat at the memory edge (~19% free) even heavily throttled — while
  being **~1.9× slower**. A local CLI that can crash your laptop is disqualifying,
  regardless of accuracy.
- Smaller alternatives (Llama 3.2 3B) had worse JSON reliability — a *downgrade*
  for a JSON-emitting classifier.

This benchmark also produced *fake* numbers three times before producing a real
one — each a methodology bug caught and documented (models not unloaded between
runs skewing latency; a reasoning model's hidden `<think>` block emptying the
output; a too-small context silently truncating the prompt). The tell each time
was the baseline model "dropping" from its known accuracy. That cautionary tale
is preserved in `docs/model_evaluation_research.md`, because on constrained
hardware, benchmark bugs masquerade convincingly as model-quality results.

---

## 4. Why the project was parked here (August 2026)

The tool works, it's well-tested, and every safety invariant it set out to build
was built. But the honest assessment at conclusion is this:

**The original thesis assumed the SLM would be the intelligent core. The build
proved the SLM is the *weak* link, and the value lives in the layers around it.**

- The **safety machinery** — the gates, the fail-closed trash-net, per-step
  snapshots, the situation state machine, the reflog/dangling-object forensics —
  works, and works *independently of how smart the model is.* That part delivered
  the thesis.
- The **SLM** landed at ~40% on hard phrasings. To make routing reliable, more
  and more distinction had to be pulled into deterministic captures and the
  embedding router. The model ended up doing only coarse intent classification —
  the fallback, not the brain.

So the project answered its own question: **for local Git-intent routing on 8 GB
hardware, a 3B SLM is not the right tool for the precision this domain demands.**
The interesting engineering ended up being the deterministic-first hybrid — and
once that was true, a "smarter" model bought little while costing real RAM and
latency. There was no compelling next thing to build that would change that
conclusion, so the project is parked here, deliberately, as a finished piece with
a finding.

### What it would take to revisit

*(As written in August. What actually brought it back was none of these — see §6.)*

- **A bigger memory target (16 GB+).** The one context where a 4B (or 7B) model
  could pay off. The eval harness is built and the methodology bugs are fixed —
  re-run it there and the conclusion could change.
- **Problem B (file scoping), still open.** "Which files belong to the login
  feature?" is genuine fuzzy content judgment the 3B model answers
  inconsistently. Options were sketched (embed diffs and rank; or be honest and
  widen the "uncertain" bucket) but never chosen. This is the one genuinely hard
  problem the project left on the table.

---

## 5. What this project is good evidence of

Read as a portfolio piece, the story is *"I set out to build an LLM-driven tool,
measured that the LLM was the wrong place to put the intelligence, and
re-architected around that finding"* — which is a stronger engineering narrative
than "the model worked great." Specifically it demonstrates:

- **Deterministic-first design under a hard constraint.** Rule-shaped
  distinctions live in code; the model is reserved for genuine semantic judgment.
- **Safety treated as load-bearing and testable.** Fail-closed context,
  honest exit codes, gates that keep the model out of destructive decisions, a
  trash-net verified byte-for-byte, a router that abstains rather than guesses.
- **Measurement over intuition.** A negative result reached with a real eval
  harness, and the discipline to *not report* three sets of numbers that turned
  out to be methodology artifacts.
- **Knowing when to stop.** Concluding a project on a finding, rather than
  polishing past the point of value.
- **Knowing when to start again** (§6): coming back when a genuinely different
  option appeared, and measuring it with the same harness before believing it.

---

## 6. The revival — a decision model instead of a language model (October 2026)

### What changed the question

The August conclusion was "a 3B model is the wrong tool on 8 GB, and a bigger one
doesn't fit." What it never considered was a model that doesn't *generate* at all.
[Kev](https://github.com/jaredpalmer/kev) is a **decision model**: given a request
and a fixed list of options it returns a calibrated probability for each — no
text, no JSON to parse, no prompt engineering. Kev-0.8B fits the 8 GB machine
(~3 GB while serving).

### What was measured (same 44-case harness, fast-path bypassed)

| | qwen2.5:3b LLM | embedding router | Kev-0.8B stock | **Kev-0.8B fine-tuned** |
|---|---|---|---|---|
| Routing accuracy | 41% | 64% | 70% | **41/44 (93%)** |
| Out-of-scope rejection | 4/4 | 0/4 | 4/4 | 4/4 |
| Median latency | ~10 s | ~270 ms | ~0.5 s | ~0.5 s |

The fine-tune (`gitrust-v2`) took two Colab runs on a free T4: ~1,500 synthetic
requests over GiTrust's 36 operations, with every evaluation phrasing (and
anything close to one) kept out of training. On a second held-out set of 68
requests written by the owner, run through the whole binary, it acted correctly
48 times, asked with a menu for the rest, and made **no wrong action that changes
anything**. It is published as
[`AniketDalvi385/gitrust-kev-0.8b`](https://huggingface.co/AniketDalvi385/gitrust-kev-0.8b)
and is the default. Full record: `docs/kev_migration_research.md`.

### What changed in the code (v0.4.0)

- **Ollama, `qwen2.5:3b`, the 5,800-token prompt and the embedding router are
  gone.** Layer 1 is now ⚡ exact phrases → learned phrases → ✦ Kev → ◇ a menu.
- **The model chooses an operation and nothing else.** Arguments (branch names,
  counts, files, URLs) are captured from the request or asked for; commit
  messages are written by the user, never drafted.
- **Kev's answer is checked deterministically:** it acts only above a confidence
  bar (higher for anything that discards or rewrites work); the request's own
  words can veto it ("push the login changes" is never a plain push); destructive
  routes are re-asked with the options reversed. Calibration can't see a
  *confident* confusion — these checks can.
- **Problem B has an approach, not yet a proof:** file scoping asks Kev one
  yes/no question per changed file and bands the probability — confident yes
  pre-selected, confident no left out, the middle shown to the user as "your
  call". It has only been checked on a 16-file smoke test (12–13 right, 0 wrong);
  a realistic multi-feature diff is still untested.
- **A UX pass**, done by driving the binary in a pseudo-terminal and reading the
  screen (`docs/ux/README.md`): plain-language menus, a line saying what was
  understood, a spinner, a one-key plan prompt, every cancel exiting 1, phrases
  learned from menu picks, and a dozen messages that had been wrong.

### How it changes the conclusion

The August finding holds and gets sharper: **the intelligence belongs in the
deterministic layers around the model**, and the safety machinery is still what
makes the tool safe — Kev is not safe alone either. What was wrong was the
implied corollary that no local model could do better. **Model type mattered
more than size**: a 0.8B model that ranks a closed list beat a 3B model that
writes text, at a twentieth of the latency on the same laptop, and a cheap
fine-tune made it good enough that most requests no longer need a menu.

---

## Appendix — the commits

| # | Commit | Date | What it did |
|---|---|---|---|
| 1 | `6bae784` phases 1–10 baseline | 2026-07-13 | Entire initial import — the 5-layer pipeline, trait seams, crash recovery. |
| 2 | `fea0e50` fmt + Phase-10 review fixes | 2026-07-13 | fmt; clap help crash; **fail-closed context**; **exit-1 on refusal**. |
| 3 | `c9eaa9b` Phase 12 (+ lands 11) | 2026-07-14 | Divergence reconciliation ChoiceGate; continue/skip/abort verbs. |
| 4 | `6d73fc2` Phase 13 undo family | 2026-07-16 | UndoIntent → 4-option gate; Amend/Revert/Unstage/Squash; force-with-lease. |
| 5 | `0a67c72` Phase 14 forensics/trash-net | 2026-07-16 | Two-source lost-commit scan; RecoveryBrowser; fail-closed trash-net. |
| 6 | `88b2d0d` merge → main | 2026-07-16 | v0.2 feature set complete. |
| 7 | `f58e354` CLAUDE.md rewrite | 2026-07-16 | Docs to v0.2-complete state. |
| 8 | `cc35784` untrack stray gitlink | 2026-07-29 | Hygiene. |
| 9 | `b26c44e` **rename glit → gitrust** | 2026-07-29 | Total mechanical rename, history-preserving. |
| 10 | `7f0f9d7` version 0.2.0 | 2026-07-29 | Version bump. |
| 11 | `6a33eb7` v0.3.0 fixes batch | 2026-08-05 | **Phantom-reset P0**; exit codes; arg extraction; redirect resume. |
| 12 | `2d92d8f` deterministic fast-paths | 2026-08-07 | scoped-push/sync/amend off the LLM (measured the 3B can't do them). |
| 13 | `9b5c00e` version 0.3.0 | 2026-08-07 | Version bump. |
| 14 | `209f964` fix 2 passed-with-defects | 2026-08-07 | M-13 staging; M-21 failure-masked-as-success. |
| 15 | `bc46cb1` **embedding router** | 2026-08-10 | Layer 1.25 + eval harness + research doc. The architectural turn. |
| 16 | `ff98ebe` final paraphrase captures | 2026-08-10 | Last 3 variant misroutes closed deterministically. |
| 17 | `5f56525` public-readiness docs | 2026-08-11 | README, this history, license; v0.3.0 released and parked. |
| 18 | `92931df` **qwen → Kev** | 2026-10-13 | Ollama, the LLM prompt and the router replaced by the Kev decision model. |
| 19 | `249d605` harden Layer 1 | 2026-10-13 | Second opinions, the 0.9 refusal bar, idle shutdown of the model server. |
| 20 | `b9dbad9` `commit_push` | 2026-10-14 | 36th operation; round-2 fine-tune data and notebook. |
| 21 | `077967b` gitrust-v2 results | 2026-10-15 | Round-2 fine-tune measured: 41/44, 0 harmful wrong actions. |
| 22 | `7b947ca`…`85b7b8a` UX pass | 2026-10-15 | Nine commits: wrong messages fixed, plain menus, spinner, plan keys, learned phrases. |
| 23 | `d07390e` fine-tune is the default | 2026-10-15 | Published to Hugging Face; v0.4.0 released. |
| 24 | `2f3de3f` no-terminal message | 2026-10-15 | v0.4.1. |

Deeper references, all in-repo: `gitrust/CLAUDE.md` (architecture),
`docs/Build_phases_v3.md` (the phase plan), `docs/scenario_map.md` (the *why*
behind each scenario), `docs/classifier_redesign_research.md` (the router
research + eval), `docs/model_evaluation_research.md` (the model-swap negative
result), `docs/slm_classification_notes.md` (the 3B-can't-do-this measurement),
`docs/kev_migration_research.md` (the move to Kev, the fine-tunes, every
measurement since), `docs/ux/README.md` (the UX pass).
