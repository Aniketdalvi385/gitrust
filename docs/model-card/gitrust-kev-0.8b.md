---
language: en
license: apache-2.0
library_name: peft
base_model: Qwen/Qwen3.5-0.8B-Base
base_model_relation: adapter
pipeline_tag: text-classification
tags:
  - decision-model
  - lora
  - multiple-choice
  - git
  - cli
  - kev
metrics:
  - accuracy
---

# GiTrust Kev-0.8B (gitrust-v2)

A fine-tune of [Kev-0.8B](https://huggingface.co/jaredpalmer/kev-0.8b) that routes a plain-English Git
request ("commit n push", "undo my last commit", "push only the login changes") to one of the 36
operations of [GiTrust](https://github.com/Aniketdalvi385/gitrust), a Git CLI that keeps the model out
of everything except choosing the operation.

Kev is a *decision model*: it is given a request and a fixed list of options and returns a calibrated
probability for each option. It never writes commands, arguments or commit messages, so it cannot
invent a Git command. GiTrust acts on its answer only when the probability clears a bar (0.5, or 0.6
for operations that discard or rewrite work), asks the user with a menu otherwise, and puts every
destructive operation behind its own confirmation.

## Use with GiTrust

```toml
# ~/.gitrust.toml
kev_run = "AniketDalvi385/gitrust-kev-0.8b@v2"
```

GiTrust starts the Kev server itself; the weights are downloaded to the Hugging Face cache on first
use. Directly with a [Kev](https://github.com/jaredpalmer/kev) checkout:

```bash
uv run --extra serve python -m kev.serve --run AniketDalvi385/gitrust-kev-0.8b@v2 --port 8009
```

**The options must be GiTrust's, verbatim and in order.** The model was trained on the exact option
wording and order in [`kev_ops.json`](kev_ops.json) (also in this repo). Rewording or reordering them
puts it off its training distribution; it is not a general Git classifier for arbitrary option lists.

## Training

| | |
|---|---|
| Initialised from | `jaredpalmer/kev-0.8b@v1.0` (LoRA r = 16 and the pointer head, warm-started) |
| Data | 1,481 synthetic requests over 36 options, written for GiTrust (`make_kev_train.py --round 2`): paraphrases, typos, casual wording, out-of-scope requests, near-miss pairs |
| Recipe | `kev.train`, 2 epochs, lr 2e-5 (adapter and head), permutation-consistency loss (perm_kl 1.0 on 30% of rows), bf16, one Colab T4 |
| Calibration | temperature 4.0, fitted on the 259-row dev set and stored in `head.pt` |
| Hold-out | every evaluation phrasing (and anything within Jaccard 0.6 of one) removed from the training data; the evaluation's slot values (topics, branch names, counts, messages) banned |

## Evaluation

All numbers are on author-labelled requests that were never trained on. They are small sets; read
them as "this works for GiTrust's requests", not as a benchmark.

| | stock Kev-0.8B | **this model** |
|---|---|---|
| Dev set (259 rows, held out from training) | 186 (71.8%) | **241 (93.1%)** |
| 44 held-out requests, 36 options, clean / dirty repo | — | **41 / 41** |
| Out-of-scope and safety items | — | 4/4 · 5/5 |

Inside GiTrust (thresholds, menus and gates included), on 68 requests written by the owner: 48 acted
on correctly, 8–9 answered with a menu, 1 wrong *read-only* action, **0 wrong actions that change
anything**, in both a clean and a dirty repository.

Known weak spots: "psuh" (typo of push) is read as not-Git; "roll back that last commit i just made"
leans to `revert` instead of `undo` (below GiTrust's destructive bar, so it becomes a menu); terse
requests such as "whats goin on" get a menu instead of `status`. The full write-up, per-case results
and the training notebook are in GiTrust's
[`docs/kev_migration_research.md`](https://github.com/Aniketdalvi385/gitrust/blob/main/docs/kev_migration_research.md)
(§12) and [`docs/classifier_eval/`](https://github.com/Aniketdalvi385/gitrust/tree/main/docs/classifier_eval).

## Limitations

- English only; tuned for one developer's phrasing plus synthetic variants.
- Not safe to act on alone. Its value in GiTrust comes from the deterministic layers around it: exact
  phrases first, confidence bars, a re-ask with the options reversed for destructive routes, and
  per-operation confirmations.

## License

Apache-2.0, like Kev and the Qwen3.5 base. Kev's own training data is described in its
[model card](https://huggingface.co/jaredpalmer/kev-0.8b).
