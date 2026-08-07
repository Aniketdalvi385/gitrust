# Small-model intent classification — design notes

GiTrust classifies natural-language intent on a **local 3B model** (qwen2.5:3b via
Ollama). This note records why the classification architecture is
*deterministic-first with an LLM fallback*, backed by both direct measurement and
the SLM literature — so the pattern is a deliberate design choice, not incidental.

## The problem, measured

Several intents carry a **regular, rule-shaped distinction** the 3B model cannot
reliably make, no matter the prompt:

- "push the login changes" (topic filter → scope files) vs "push my work" (plain push)
- "sync with main" (a rebase/merge **write**) vs a read-only status
- "fix the last commit message to 'X'" (amend, extract X) vs a generic fix request

Probing the live model (temperature 0) showed **identical reasoning across repeated
runs** — e.g. "push the login changes" → *"which implies a push operation"* every
time — and adding prompt examples / negative examples did **not** move it. The model
anchors on the salient verb and cannot weight a subtle syntactic cue ("the … changes"
vs "my … work").

## What the research says

- **Few-shot prompting helps little on small models.** *"Smaller models show less
  reliable improvements from few-shot examples and are more prone to biases
  introduced by the examples."* Below ~7B, piling more examples into the prompt is
  close to useless for fine-grained disambiguation.
  ([arXiv 2305.07157](https://arxiv.org/pdf/2305.07157))
- **Deterministic-first, LLM-fallback is the recommended production architecture.**
  *"Build the deterministic version first… add an LLM only to the specific steps
  where deterministic logic breaks down."* *"Regex handles 95–98% of structured
  cases with LLM for the messy remainder."*
  ([hybrid intent guide](https://medium.com/data-science-collective/intent-driven-natural-language-interface-a-hybrid-llm-intent-classification-approach-e1d96ad6f35d),
  [rule+LLM two-stage](https://ranjankumar.in/llm-powered-chatbots-a-practical-guide-to-user-input-classification-and-intent-handling))
- **Constrained decoding / schema enforcement is essential for SLMs.** *"+0.35
  quality… small language models often fail without formal runtime constraints."*
  GiTrust already forces the classifier into a reasoning-first JSON schema.
  ([intent classification techniques](https://labelyourdata.com/articles/machine-learning/intent-classification))
- **Task decomposition lets small models beat large ones** — split one hard
  classification into narrower sub-decisions and aggregate.
  ([arXiv 2509.12423](https://arxiv.org/abs/2509.12423))
- **Fine-tuning is the accuracy ceiling** (even 1B → ~96% schema accuracy) but needs
  a trained/hosted custom model. Out of scope for a zero-infra local CLI.

## GiTrust's architecture (the decision)

1. **Deterministic fast-path first** (`intent.rs::try_canonical` +
   `try_canonical_parameterized`): exact canonical phrases and *regular parameterized
   forms* — `create branch <name>`, `checkout <name>`, `squash the last <N>`,
   `push the <topic> changes`, `sync with <branch>`, `amend … message to '<msg>'` —
   are captured in code. Instant, zero model cost, no lost slot. This is where every
   rule-shaped distinction lives.
2. **LLM only for genuinely semantic calls**: which *files* are "login-related"
   (the resolver), free-form commit-message drafting, and novel phrasings the
   fast-path doesn't cover. This is what the model is actually good at.
3. **Constrained by a JSON schema**, reasoning-first, temperature 0 — already in
   place and consistent with the SLM guidance.

**Rule of thumb for adding an intent:** if the distinction is *syntactic/regular*
(a keyword + a slot), capture it deterministically; reserve the LLM for
*semantic* judgement. A prompt example is the last resort, not the first — on a 3B
model it rarely moves behaviour.

## Not yet applied (available if fast-path + prompt prove insufficient)

- **Confidence-based routing**: the classifier already returns `confidence`; low
  confidence could route to the clarification gate instead of guessing.
- **Two-step decomposition**: read-vs-write, then which-write.
- **Fine-tuning** a small local model on GiTrust's own intent set.
