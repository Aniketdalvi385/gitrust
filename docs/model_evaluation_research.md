# SLM model evaluation — is there a better local model than qwen2.5:3b?

**Status:** research, concluded. **Date:** 2026-08-11.
**Baseline:** qwen2.5:3b (1.9 GB) — the configured/installed model.
**Machine:** Apple M2, **8 GB RAM** (the binding constraint — see
[[glit-runtime-constraints]]).

## Question

Is there an open SLM that would serve GiTrust's intent classifier better than
qwen2.5:3b? (The user's phrasing said "llama2.7 3b"; the actual current model is
**qwen2.5:3b** — clarified before testing.)

## TL;DR — recommendation: STAY on qwen2.5:3b

Not because it's the most accurate model in the abstract, but because on the
**8 GB target** the alternatives are either **not deployable** or **not clearly
better** at a real cost:

- **qwen3:4b** — GiTrust's real classifier prompt is **~5,800 tokens**, needing
  `num_ctx ≥ 2048`. At that context a 4B model **OOM-crashed the 8 GB laptop**
  during the full run, and even under heavy throttling sat at a **steady ~19%
  free memory** (right at the edge). It is also **~1.9× slower** (17.8s vs 9.4s
  median). A local CLI that can crash the user's machine is disqualifying,
  regardless of accuracy.
- **phi4-mini (3.8B)** — same memory class as qwen3:4b; showed JSON reliability
  problems under our strict schema in the throttled run (parse-fails), and no
  accuracy advantage was demonstrable within safe memory limits.
- **Llama 3.2 3B** — rejected from published evidence: ~47–56% JSON parse rate
  at 3B (a *downgrade* for a JSON-emitting classifier).
- **Bigger (7B+)** — swaps hard on 8 GB; unusable interactively.

Crucially, **the model is now only the FALLBACK.** The deterministic fast-path
(`try_canonical` + captures) and the embedding router already handle the common
phrasings model-independently. So the upside of a "smarter" fallback model is
small, while the cost (a 4B model's RAM pressure + ~2× latency on 8 GB) is
concrete. The trade doesn't clear the bar.

## The 8 GB hardware wall (the real finding)

| model | ctx needed | steady free RAM | median latency | verdict on 8 GB |
|---|---|---|---|---|
| qwen2.5:3b | 2048 | ~23% | **9.4 s** | fine (current) |
| qwen3:4b | 2048 | **~19% (edge)** | 17.8 s (~1.9×) | **crashed at full load; unsafe** |
| phi4-mini | 2048 | ~19% | slower | JSON-reliability issues; no gain shown |

The classifier prompt is ~5.8 K tokens, so you cannot shrink `num_ctx` to make a
4B model fit — that truncates the prompt (see §Methodology). 2048 ctx + 4B is the
combination that OOM-crashed the machine. There is no safe operating point for a
4B model with GiTrust's real prompt on 8 GB.

## Methodology (and three bugs it caught — the honest part)

The comparison harness (`docs/classifier_eval/compare_models.py`) runs the REAL
`prompts.rs` classifier prompt over the labeled eval set per model, mapping JSON
→ our label vocabulary, timed. Getting a *valid* run took fixing three
methodology bugs — recorded because each produced convincing-but-false numbers:

1. **No unload between models.** Ollama keeps a model resident ~5 min; on 8 GB
   two 2.5 GB models co-resident → swap → inflated latency for whichever ran
   later. Fix: `keep_alive:0` + settle pause between models.
2. **qwen3 reasoning mode.** qwen3 defaults to emitting a hidden `<think>`
   block that consumed the whole token budget → **empty** responses → a fake
   "0% / 44 parse-fails / 2059 s load". Fix: send `think:false` (ignored by
   non-thinking models, so safe for all).
3. **Context truncation.** To reduce memory I set `num_ctx=1024`, but the prompt
   is ~5.8 K tokens → ~80% silently truncated → a fake "10% / 5% / 0%" across
   ALL models (qwen2.5 "dropped" from its real 40% — the tell that the config,
   not the models, was broken).

None of those intermediate numbers are model-quality signals and none are
reported as findings. The lesson: on constrained hardware, benchmark
methodology bugs masquerade as model-quality results — always sanity-check a
dramatic number against a known baseline (qwen2.5's 40%).

Also: the full 44-case run at 2048 ctx **crashed the host** (OOM). Subsequent
runs used a memory watchdog that kills the benchmark if free RAM drops below 8%
twice consecutively — so measurement can never again take down the machine.

## Valid data points (from safe runs)

- qwen2.5:3b, full 44-case @ 2048 ctx (clean, pre-crash baseline): **40% (18/44),
  0 parse-fails, ~10 s median.**
- qwen3:4b @ 2048 ctx: **~1.9× slower**, steady ~19% free RAM (edge), 0
  parse-fails on the small safe subset — but a representative accuracy sample
  could not be taken without risking the machine.
- A strict-label caveat: the harness scores the LLM's `push`→`scoped_push` and
  `commit`→`add_commit` as misses, so raw LLM accuracy understates "reasonable"
  behavior. This does not change the deployment verdict.

## Where this leaves it (portfolio-relevant)

The right answer to "is there a better model" turned out to be **"the question
is moot on this hardware, and the model matters less than it used to."** The
fast-path + embedding router carry the common cases; the 3B fallback is
adequate; a 4B upgrade is unsafe on 8 GB and only ~1.9× slower for an unproven
gain. If GiTrust ever targets 16 GB+ machines, re-run this harness (bugs already
fixed) with qwen3:4b at 2048 ctx and the full eval set — that's the one context
where the upgrade could pay off.
