# GiTrust

**A safety layer for Git, driven by a local decision model.** You type what you
want in plain English (`push the login changes`, `undo my last commit but keep
the files`, `find my lost work`); GiTrust works out the *intent* with a small
model running entirely on your machine, then executes it through deterministic,
gated, reversible Git operations.

The load-bearing invariant: **the model reads your intent — it never runs your
commands, and never writes them.** Every operation that could lose work is gated, snapshotted,
and (for anything that deletes content Git can't recover) backed up first.

> **Status: v0.4.1 (October 2026).** A portfolio / learning project, not a
> supported product. It was parked in August 2026 on a negative result — a 3B
> local LLM was the wrong place to put the intelligence — and revived in October
> when a different kind of model changed the answer: **Kev-0.8B**, a decision
> model that ranks a fixed list of operations instead of writing text, fine-tuned
> for GiTrust and published as
> [`AniketDalvi385/gitrust-kev-0.8b`](https://huggingface.co/AniketDalvi385/gitrust-kev-0.8b).
> On the same 44 held-out requests: the old LLM 41%, stock Kev 70%, the fine-tune
> **41/44**. The whole story is in [`PROJECT_HISTORY.md`](PROJECT_HISTORY.md).

---

## Why it exists

The origin story is the colleague at the next desk who just ran
`git reset --hard`, or force-pushed over a teammate's commit, or can't find the
work they were sure they committed. Git is unforgiving precisely at the moments
people are most stressed. The idea: put a layer in front of Git that (a) lets you
say what you mean instead of memorizing incantations, and (b) makes the
dangerous operations *safe by construction* — spelled out before they run,
snapshotted before they execute, recoverable after.

## What actually makes it different

These are facts backed by the test suite, not aspirations:

- **The model picks an operation and nothing else.** It never writes a command,
  an argument or a commit message, never selects a destructive strategy and never
  executes. Undo, "clean up," and reconcile-divergence flows
  route the model's output into a deterministic **ChoiceGate** that spells out
  each option's consequence — the model gets you *to* the gate; it never picks
  the outcome.
- **Every destructive op is gated, snapshotted, and trash-netted.** Before a
  write pipeline runs, HEAD/index are snapshotted for `--recover`. Any operation
  that deletes content **not in Git's object database** (hard reset's dirty
  working tree, `clean`'s untracked files) is backed up to `~/.gitrust/trash/`
  *first* — and it's **fail-closed**: if the backup can't be written, the
  destructive op does not proceed.
- **A state machine over 12 repo situations.** GiTrust deterministically
  classifies the repo (detached HEAD, mid-merge, mid-rebase, diverged,
  remote-rewritten, stale lock, …) and a *situation gate* blocks or redirects
  intents that don't make sense in that state — so it never gives you
  reset-shaped advice while you're mid-rebase.
- **Lost-work forensics.** `find my lost work` scans reflogs *and* dangling
  objects (the only way to surface a dropped stash) and restores anything you
  pick to a fresh `rescue/*` branch — **without touching your current state.**
- **Fully local.** Intent classification runs on
  [Kev](https://github.com/jaredpalmer/kev) (Kev-0.8B, fine-tuned for GiTrust) served on 127.0.0.1.
  No diffs, code, or queries leave your machine.
- **287 unit/integration tests** (the model is mocked), a pseudo-terminal
  harness that drives the real binary and records every screen
  ([`docs/ux/`](docs/ux)), and a versioned manual acceptance-test record.

## How it works — the 5-layer pipeline

```
your query
   │
   ▼
Layer 0  Context      detect repo situation (12 states), collect branch/ahead-behind/diff
   │
   ▼
Layer 1  Intent       ⚡ exact / learned phrase  →  ✦ Kev ranks the 36 ops  →  ◇ you pick
   │                   (Kev routes only when confident; otherwise you choose)
   ▼
Layer 1.5 Situation gate   block/redirect intents invalid for the current repo state
   │
   ▼
Layer 2  Resolve      turn "the login changes" into concrete file paths
   │
   ▼
Layer 3  Validate & gate   pre-checks · danger grading by blast radius · confirmation gates
   │                       (snapshot + trash-net happen here)
   ▼
Layer 4  Execute      run fully-concrete steps against git2 / shelled-out git
```

Layer 1, in order:

- **⚡ Fast-path** — an exact-phrase whitelist (`gitrust help` lists it); instant,
  no model call. Phrases you resolved from a menu before are remembered per repo
  and are instant too (`gitrust aliases`).
- **✦ Kev** — a local *decision* model: it never writes text, it returns a
  calibrated probability for each operation GiTrust lists. GiTrust's own code
  decides: route when confident (a higher bar for anything destructive), say
  "not a Git request" when that's confident, otherwise ask. A route is
  double-checked against the request's own words, and destructive routes are
  re-asked with the options in reverse order.
- **◇ You pick** — when Kev isn't sure, you choose from its top candidates (each
  shown with what it would do). `--yes` refuses rather than guess.

The model never supplies arguments either: branch names, counts, files and
commit messages are captured from what you typed, or GiTrust asks you. It
never drafts a commit message.

Full architecture, type vocabulary, and the conventions for extending it are in
[`gitrust/CLAUDE.md`](gitrust/CLAUDE.md).

## The model

GiTrust's default is
[`AniketDalvi385/gitrust-kev-0.8b@v2`](https://huggingface.co/AniketDalvi385/gitrust-kev-0.8b):
[Kev-0.8B](https://github.com/jaredpalmer/kev) with a 64 MB adapter trained on
~1,500 synthetic requests over GiTrust's 36 operations (Apache-2.0; card in
[`docs/model-card/`](docs/model-card/gitrust-kev-0.8b.md)). To use stock Kev
instead, put `kev_run = "jaredpalmer/kev-0.8b@v1.0"` in `~/.gitrust.toml`.
Training data, notebooks and every evaluation are in
[`docs/classifier_eval/`](docs/classifier_eval) and
[`docs/kev_migration_research.md`](docs/kev_migration_research.md).

## Requirements

- **Rust** (2021 edition) to build.
- **[uv](https://docs.astral.sh/uv/)** — `gitrust setup` uses it to install
  [Kev](https://github.com/jaredpalmer/kev) (pinned commit); the first start
  downloads the model (the ~1.5 GB Qwen3.5-0.8B base network Kev is built on,
  plus the 64 MB GiTrust adapter).
- Designed and tested on an **Apple M2 with 8 GB RAM**. The Kev server uses
  ~3 GB while running; GiTrust starts it in the background on the first query
  that needs it (~25 s cold start) and stops it again after 15 idle minutes
  (`kev_idle_minutes`; a server you started yourself is never stopped).

## Build & run

```bash
cd gitrust
cargo build --release          # optimized binary at target/release/gitrust

cargo run -- setup             # install Kev, start it, smoke-test it
cargo run -- "show status"     # one-shot query
cargo run                      # interactive REPL
cargo test                     # 287 tests, no model server needed (Kev is mocked)
```

Config lives at `~/.gitrust.toml`; runtime state (snapshots, trash-net, logs) at
`~/.gitrust/`.

## A few things you can say

```
gitrust "commit everything"
gitrust "push the login changes"          # scoped push — resolves which files
gitrust "sync main into my feature branch"
gitrust "undo my last commit but keep the files"
gitrust "squash the last three commits"
gitrust "clean up my repo"                # spells out what would be deleted, first
gitrust "find my lost work"               # reflog + dangling-object forensics
gitrust --recover                         # restore an interrupted pipeline
gitrust help                              # the phrases that work instantly
```

Before anything changes, GiTrust shows the plan: Enter runs it, `q` cancels,
`e` edits the commit message, `s` keeps a commit local instead of pushing.
Backing out anywhere prints `↪ Canceled — nothing was done.` and exits 1.

## The safety model, precisely

- `--yes` (auto-confirm) exists for scripting, but it **cannot** bypass the
  load-bearing gates: it refuses to auto-pick a strategy (ChoiceGate), auto-pick
  a commit to restore (RecoveryBrowser), auto-clear a type-to-confirm on a
  destructive op, or pick from a "which did you mean?" menu.
- **A run that did nothing never exits 0** — a refusal, a cancel, a blocked
  pre-check or a missing answer all exit 1.
- Rewriting **published** history escalates: it requires explicit confirmation
  and uses `--force-with-lease`, never a bare `--force`.
- Danger is graded by **blast radius**, not just reversibility — deleting
  untracked files (no reflog; only the trash-net protects them) grades *above* a
  hard reset (which the reflog can recover).

## Repository layout

```
gitrust/                     the Rust crate (binary + 24 modules, ~23k LOC)
  src/                       the 5-layer pipeline; kev_ops.json = the 36 operations
  CLAUDE.md                  authoritative architecture reference
docs/
  v0.5.0_fix_plan.md         the v0.4.1 acceptance findings: root causes, fixes, order
  kev_migration_research.md  the move to Kev, the fine-tunes, every measurement
  model-card/                the Hugging Face model card
  classifier_eval/           eval sets, harnesses, training data, Colab notebooks
  ux/                        the UX pass: findings, before/after screens, PTY harness
  (older research)           scenario map, build phases, the qwen-era classifier
                             and model-evaluation research (dated records)
design_handoff_gitrust_ux/   the UX spec behind the tool (copy, glyphs, state machines)
Formula/                     Homebrew formula template (not yet published)
PROJECT_HISTORY.md           how this was built, what was found, why it was parked,
                             and how it came back
LICENSE                      MIT
```

## License

[MIT](LICENSE) © 2026 Aniket Dalvi.
