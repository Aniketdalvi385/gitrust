# GiTrust

**A safety layer for Git, driven by a local language model.** You type what you
want in plain English (`push the login changes`, `undo my last commit but keep
the files`, `find my lost work`); GiTrust classifies the *intent* on a small
model running entirely on your machine, then executes it through deterministic,
gated, reversible Git operations.

The load-bearing invariant: **the language model reads your intent — it never
runs your commands.** Every operation that could lose work is gated, snapshotted,
and (for anything that deletes content Git can't recover) backed up first.

> **Project status: concluded.** This is a finished portfolio / learning project,
> not a maintained product. It works and is well-tested, but development has
> stopped at a deliberate conclusion. The most interesting outcome is a *negative
> result* about where a small local LLM belongs in a tool like this — see
> [`PROJECT_HISTORY.md`](PROJECT_HISTORY.md), which is the real centerpiece of
> this repo.
>
> **Update (2026-10):** the local LLM (`qwen2.5:3b` via Ollama) and the embedding
> router were replaced by **Kev-0.8B**, a local decision model that only ranks a
> closed option list with calibrated probabilities (70% vs the LLM's 41% on the
> same 44-case set; see `docs/classifier_eval/run_kev_eval.py`). The history
> below describes the original design.

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

- **The LLM parses intent and nothing else.** It never selects a destructive
  strategy and never executes. Undo, "clean up," and reconcile-divergence flows
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
  [Kev](https://github.com/jaredpalmer/kev) (Kev-0.8B) served on 127.0.0.1.
  No diffs, code, or queries leave your machine.
- **226 unit/integration tests** plus a versioned manual acceptance-test record
  (see the companion `GiTrust-test` suite).

## How it works — the 5-layer pipeline

```
your query
   │
   ▼
Layer 0  Context      detect repo situation (12 states), collect branch/ahead-behind/diff
   │
   ▼
Layer 1  Intent       ⚡ exact fast-path  →  ✦ Kev ranks the known ops  →  ◇ you pick
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

- **⚡ Fast-path** — an exact-phrase whitelist; instant, no model call.
- **✦ Kev** — a local *decision* model: it never writes text, it returns a
  calibrated probability for each operation GiTrust lists. GiTrust's own code
  decides: route when confident (a higher bar for anything destructive), say
  "not a Git request" when that's confident, otherwise ask.
- **◇ You pick** — when Kev isn't sure, you choose from its top candidates (each
  shown with what it would do). `--yes` refuses rather than guess.

The model never supplies arguments either: branch names, counts, files and
commit messages are captured from what you typed, or GiTrust asks you. It
never drafts a commit message.

Full architecture, type vocabulary, and the conventions for extending it are in
[`gitrust/CLAUDE.md`](gitrust/CLAUDE.md).

## Requirements

- **Rust** (2021 edition) to build.
- **[uv](https://docs.astral.sh/uv/)** — `gitrust setup` uses it to install
  [Kev](https://github.com/jaredpalmer/kev) (pinned commit) and download
  Kev-0.8B (~1.5 GB).
- Designed and tested on an **Apple M2 with 8 GB RAM**. The Kev server uses
  ~3 GB while running; GiTrust starts it in the background on the first query
  that needs it (~25 s cold start) and it stays up afterwards
  (`pkill -f kev.serve` to stop it).

## Build & run

```bash
cd gitrust
cargo build --release          # optimized binary at target/release/gitrust

cargo run -- setup             # install Kev, start it, smoke-test it
cargo run -- "show status"     # one-shot query
cargo run                      # interactive REPL
cargo test                     # 238 tests, no model server needed (Kev is mocked)
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
```

## The safety model, precisely

- `--yes` (auto-confirm) exists for scripting, but it **cannot** bypass the
  load-bearing gates: it refuses to auto-pick a strategy (ChoiceGate), auto-pick
  a commit to restore (RecoveryBrowser), or auto-clear a type-to-confirm on a
  destructive op.
- Rewriting **published** history escalates: it requires explicit confirmation
  and uses `--force-with-lease`, never a bare `--force`.
- Danger is graded by **blast radius**, not just reversibility — deleting
  untracked files (no reflog; only the trash-net protects them) grades *above* a
  hard reset (which the reflog can recover).

## Repository layout

```
gitrust/                     the Rust crate (binary + 22 modules, ~20k LOC)
  src/                       the 5-layer pipeline
  CLAUDE.md                  authoritative architecture reference
docs/
  PROJECT-adjacent research  scenario map, build phases, SLM classification notes,
                             the classifier-redesign + model-evaluation research
  classifier_eval/           throwaway eval harness (ships nothing)
design_handoff_gitrust_ux/   the UX spec behind the tool (copy, glyphs, state machines)
PROJECT_HISTORY.md           how this was built, what was found, why it concluded
LICENSE                      MIT
```

## License

[MIT](LICENSE) © 2026 Aniket Dalvi.
