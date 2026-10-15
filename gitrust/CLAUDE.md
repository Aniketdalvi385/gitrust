# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

GiTrust is an agentic Git CLI: a single Rust binary that turns natural-language queries ("push login changes") into safe, confirmed Git operations. Intent classification runs on **Kev**, a small *local* decision model (github.com/jaredpalmer/kev, Kev-0.8B by default) served on 127.0.0.1 — it never generates text, only returns calibrated probabilities over a closed option list GiTrust supplies. No network calls to hosted models. (Until 2026-10 this was `qwen2.5:3b` via Ollama plus an embedding router; see "History" below.) The binary lives in the `gitrust/` subdirectory of the repo root; the Cargo project root is `gitrust/`.

## Commands

All commands run from `gitrust/` (where `Cargo.toml` lives).

```bash
cargo build                 # debug build
cargo build --release       # optimized (opt-level=z, lto, stripped — see [profile.release])
cargo test                  # run all unit tests (no Kev server needed — the model is mocked)
cargo test <name>           # single test by name substring, e.g. cargo test test_default_config
cargo test --package gitrust intent::   # tests in one module
cargo run -- "show status"  # run a one-shot query
cargo run                   # run the interactive REPL
cargo run -- setup          # clone/install Kev, start it, smoke-test it
cargo clippy                # lint
cargo fmt                   # format
```

Runtime prerequisite: `uv` and a Kev checkout (`gitrust setup` clones it at a pinned commit into `~/.gitrust/kev`, runs `uv sync --extra serve`, and records `kev_dir`). With `kev_dir` set and `kev_autostart = true` (default), the first query that needs the model starts `kev.serve` in the background (~25 s cold start on the 8 GB M2; it then stays resident at ~3 GB; log at `~/.gitrust/serve.log`). The server is owned by a hidden `gitrust kev-supervise` process (same process group) that stops it after `kev_idle_minutes` (default 15, 0 = never) without a request — requests touch `~/.gitrust/kev.last_used`. A server you started yourself is never stopped. Exact fast-path phrases never need the server. Config lives at `~/.gitrust.toml`; runtime state (snapshots, logs) at `~/.gitrust/`.

## Architecture

The core is a **5-layer pipeline**, orchestrated in [src/agent.rs](src/agent.rs) (`run_agent`). Data flows one direction, each layer narrowing ambiguity until the executor receives fully-concrete steps:

- **Layer 0 — Context** ([src/context.rs](src/context.rs)): `build_context` collects repo state (branch, ahead/behind, modified files) into a `RepoContext`. `detect_situation(repo) -> RepoSituation` is the single deterministic classification of repo state (Normal / EmptyRepo / DetachedHead / MidMerge / MidRebase / MidCherryPick / MidRevert / MidBisect / Diverged / RemoteRewritten / StaleLock), stored on `RepoContext.situation`. Kev sees only a one-line summary of it (`intent::repo_state_line`, e.g. "on branch main, 2 files modified, rebase in progress (2/5)") — the same short shape it was fine-tuned on.
- **Layer 1 — Intent** ([src/intent.rs](src/intent.rs), [src/kev.rs](src/kev.rs), `classify_query` in [src/agent.rs](src/agent.rs)):
    - **Fast path** — `try_canonical` matches a whitelist of exact phrases (after trim+lowercase, never substring) and bypasses the model entirely (⚡ instant, works with the server down).
    - **Learned phrases** ([src/aliases.rs](src/aliases.rs)) — a pick from a Kev or manual menu is saved in `.git/gitrust/aliases.toml` and consulted next (⚡ "· learned"), before the model; never for `kev::DESTRUCTIVE_OPS`, not while an operation is in progress, and the op is rebuilt by `build_from_op` (arguments are never stored). `gitrust aliases` lists/forgets.
    - **In-progress question** — only when the repo is MidRebase/MidMerge/MidCherryPick/MidRevert: Kev ranks that family's control ops (continue / skip / abort) plus `something_else`. Confident → route; unsure but a control op on top → the family menu (a `ChoiceGate`); otherwise fall through.
    - **Kev routing** — `IntentClassifier::rank` (impl `KevClassifier`) asks Kev to rank every op in [src/kev_ops.json](src/kev_ops.json). `kev::decide` — deterministic — then **routes** (top op ≥ `kev_min_confidence`, or ≥ `kev_destructive_min_confidence` for `kev::DESTRUCTIVE_OPS`), **rejects** (`__reject__` ≥ `kev_reject_min_confidence`, 0.9 → "not a Git request"; below it the user gets a menu of Git ops), or **asks** (◇ a `ChoiceGate` of Kev's top-3, shown with user-facing titles and hints from `copy::op_label` — never the model's op names, its probabilities or its model-facing descriptions). The model never acts on a guess it isn't confident about, and `--yes` refuses the ask-menu.
    - **Second opinions on a route** (they can only turn "act" into "ask"): `intent::conflicting_op` — the request's own words contradict Kev's op (a topic named, but plain `push`/`add_commit`/`commit_push`; "commit" and "push" both said, but only one of them; `commit_push` with no push verb) → menu; and a route to a `DESTRUCTIVE_OPS` member is re-asked with the options reversed (`kev::reversed_keep_last`, sentinel kept last) — a different top op, or a failed call, → menu. Calibration can't see a *confident* confusion; these can.
    - **Arguments are never generated.** `build_from_op` captures them from the raw query and the repo context (topic, branch names, counts, files matched against changed paths, quoted commit messages, URLs, shas); a missing one is **asked for** via `ArgKind` (`--yes` → error, never a guessed default).
    - **Commit messages are never drafted.** A `CommitMessage::Draft` means "ask the user": `resolve_or_generate_commit_message` prompts for it (offering git's MERGE_MSG mid-merge); an empty submission is an error, so `--yes` without a quoted message commits nothing.
    - If Kev is unreachable, Layer 1 offers `manual_menu`: every op by title, through the same `ChoiceGate` and `build_from_op`, so arguments are asked and destructive ops keep their own gates (`--yes` refuses it).
- **Layer 1.5 — Situation gate** ([src/validation.rs](src/validation.rs)): `situation_gate(&RepoSituation, &IntentType) -> SituationVerdict` enforces the §13 matrix as a first-class pipeline stage, called immediately after classification and **before** resolution, so a state-invalid intent never reaches the resolver. Verdicts are **Allowed / Blocked{message} / Redirect**, where `Redirect` carries gate-driven rescue flows (`DetachedHeadRescue`, `ReconcileDiverged{ahead,behind}`) — the gate, not `agent.rs`, drives the way out. This is the single source of state-based blocking — do not reintroduce parallel `repo.state()` checks in `run_pre_checks`. Block messages live in `copy.rs`.
- **Layer 2 — Resolve** ([src/resolver.rs](src/resolver.rs), [src/manifest.rs](src/manifest.rs)): `TwoPassResolver` turns a fuzzy `FileSelection::Pattern` (e.g. "login page") into concrete file paths: pass 1 is a filename match; pass 2 asks Kev one yes/no question per remaining file ("should `<path>` be in a commit of only the '<topic>' changes?", diff snippets as state, 8 files per request) and **bands** the calibrated P(yes): ≥ `RELATED_AT` (0.7) related, ≤ `UNRELATED_AT` (0.3) unrelated, otherwise uncertain — the user decides the middle band in the file picker.
- **Layer 3 — Validate & gate** ([src/validation.rs](src/validation.rs), [src/confirm.rs](src/confirm.rs), [src/danger.rs](src/danger.rs)): pre-checks (`PreCheckResult`: Ok/Warn/Block) run before a step; a `ConfirmationGate` shows the user a `GateType` — file multi-select, editable commit message, `TypeToConfirm` for destructive ops, `ChoiceGate` for ambiguous intents (undo, reconcile, cleanup), and `RecoveryBrowser` for lost-commit restore. Danger level is graded by **blast radius**, not just reversibility. **The `--yes` refusal family is load-bearing:** `AutoConfirmGate` refuses `ChoiceGate`, `RecoveryBrowser`, and `TypeToConfirm` — `--yes` never silently picks a strategy, a commit to restore, or clears a destructive confirmation. Post-checks verify success.
- **Layer 4 — Execute** ([src/executor.rs](src/executor.rs), [src/git_ops.rs](src/git_ops.rs)): `DefaultExecutor` runs `PipelineStep`s against `Git2Operations`. Every field in a `PipelineStep` is concrete — no `Pattern`, `Current`, or `Draft` survives to this layer.

### Type vocabulary — the seam between layers

[src/commands.rs](src/commands.rs) defines the core types and is the best file to read first. Note the deliberate split:

- **`IntentType`** (`ReadOnly` / `ScopedWrite` / `FeaturePipeline` / `NeedsClarification`) is Layer 1 output — may still contain ambiguity (`FileSelection::Pattern`, `CommitMessage::Draft`).
- **`PipelineStep`** is Layer 4 input — fully resolved. The resolver/validation layers exist to convert one into the other.

`FeaturePipeline` intents (e.g. `AddCommitPush`) expand into multiple `PipelineStep`s; `ReadOnly` and most `ScopedWrite` map to one.

### Trait-based seams for testing

Each layer's behavior sits behind a trait so tests can swap in mocks: `IntentClassifier` (a fixed-distribution mock drives `classify_query` in `kev_layer1_tests`), `FileResolver`, `Validator`, `ConfirmationGate` (with `AutoConfirmGate` for non-interactive runs), `StepExecutor`, `GitOperations`, `RecoveryManager`. When adding behavior, implement against the trait rather than the concrete type. `--yes` swaps `InquireGate` for `AutoConfirmGate` but does **not** bypass type-to-confirm safety gates.

### Crash recovery

[src/recovery.rs](src/recovery.rs) + [src/log.rs](src/log.rs): before a write pipeline runs, `FileRecoveryManager` snapshots HEAD/index to `~/.gitrust/snapshots/<id>.json` and logs START/END per step. On next run, `check_for_interrupted` detects a pipeline that never finished; `gitrust --recover` (or the `recover` subcommand) restores pre-pipeline state. Destructive git operations must capture a snapshot before executing — **except pure-backout pipelines, which deliberately don't** (the revert/backout commit *is* the recovery, so there is nothing to roll back to).

- **`recovery_plan` defers to git**: when a snapshot's pipeline left git mid-operation (a rebase/merge conflict), recovery does not fight the sequencer — it surfaces the git state and lets the Phase 11 verbs (`git rebase --abort` etc.) finish it, rather than force-resetting over an in-progress operation.
- **`repo_workdir` scoping (S56)**: `PipelineSnapshot.repo_workdir` scopes `check_for_interrupted` to the current repo, so a crash in worktree/repo A never prompts recovery in B.
- **Trash-net invariant** (sibling to "the model never executes"): any `GitOperations` method deleting content **not in the object database** backs it up to `~/.gitrust/trash/<ts>/` *before* the destructive git call, and is **fail-closed** — if the backup fails, the method returns `Err` and nothing is deleted.

## Conventions

- **Adding a new operation** — the full checklist (skipping a step usually surfaces later as a non-exhaustive-match compile error or, worse, an *ungoverned* intent):
    1. Add a variant to the relevant `IntentType` enum in `commands.rs`; if it reaches the executor, add the `PipelineStep` variant + `GitOperations` method too.
    2. Add it to [src/kev_ops.json](src/kev_ops.json) (before `__reject__`) with a one-sentence description, add a `build_from_op` arm (`test_every_kev_op_builds` fails until you do), add training phrasings in `docs/classifier_eval/make_kev_train.py`, and a `try_canonical` entry if it has an unambiguous phrase. **Changing kev_ops.json changes what Kev sees**: re-run `docs/classifier_eval/run_kev_eval.py` and regenerate the fine-tuning data. Destructive ops also go in `kev::DESTRUCTIVE_OPS`.
    3. Add resolution/validation if it touches files.
    4. **Add a `test_situation_gate_matrix` row.** The §13 matrix is only law for *named* intents — an operation added without a matrix row is **ungoverned** (finding #12): no situation ever blocks or redirects it. Every new intent needs an explicit row.
    5. **Grade danger in BOTH `intent_danger` and `step_danger`** (`danger.rs`) — they are separate functions; grading one leaves the other returning a default. Grade by blast radius, not reversibility.
    6. **Add the exhaustive-match arms the compiler will demand**: `explain.rs` (the `Explanation` arm) and `format_step_label` in `validation.rs`, plus `summarize_step`/`step_name` in `agent.rs`.
    7. **If it commits, decide hook-routing** (hardest-won lesson): does it need the `*_system` shell-out for hook/sequencer parity? `Commit`/`Amend`/`SquashLastN` route through the shell when commit hooks are present; `Revert` shells out for sequencer parity. git2 won't run hooks — a pure-git2 commit path silently skips `pre-commit`.
    8. **If it deletes content not in the odb, honor the trash-net convention** (hardest-won lesson): back up *exactly what the op destroys* inside the `GitOperations` method, fail-closed, before the destructive call — see `reset`/`clean`. The trait carries a convention comment so this is inherited by pattern, not memory.
- **User-facing strings**: new/touched copy goes in [src/copy.rs](src/copy.rs) (glyphs, ANSI tiers, messages) — a single source of truth being migrated to incrementally. Menu names for ops come from `copy::op_label`; `kev_ops.json` descriptions are model-facing (load-bearing) and never shown. `gitrust help` reads `copy::HELP_SECTIONS`, and a test asserts every listed phrase is a fast-path phrase.
- **Interaction conventions** ([src/ux.rs](src/ux.rs); audit and before/after screens in [../docs/ux/README.md](../docs/ux/README.md)):
    - **Every back-out is `ux::canceled()`** — Esc/Ctrl-C at any prompt (`ux::prompt_result` maps inquire's cancel), `q` at the plan, an empty file selection. main() prints one calm `↪ Canceled — nothing was done.` and exits **1** (a not-done operation never exits 0); the REPL prints it and continues. Never `println!("Aborted.")` + `return Ok(..)`.
    - **Menus have no "none of these" / "Abort" / "do nothing" row** — Esc cancels, and `copy::CHOICE_HELP` says so under every menu.
    - **The engine line names what was understood** (`ux::engine_line`: "⚡  Show status", "✦  Show history · 0.6s", "◇  … · your choice"); the model's probability is `GITRUST_DEBUG`-only.
    - **Model calls show `ux::Spinner`** (stderr, terminal only, erased on drop — keep its guard scoped so it is gone before any prompt).
    - **The plan prompt is `GateType::RunPlan`**: one keypress (Enter/y runs; q/n/Esc/Ctrl-C cancels; `e` edits the commit message, `s` drops a trailing push — shown only when they apply); other keys and type-ahead are ignored.
    - **Menus of ≤9 options use `ux::pick_one`** (numbered; a digit chooses at once); longer lists keep inquire's type-to-filter.
    - After changing anything a user sees, re-capture with `docs/ux/ux_drive.py` (PTY + terminal emulator) and read the screens.
- **Model constraints**: Kev-0.8B is small and its confidence drops on unfamiliar state wording (a dirty tree lowered "show me the history" from 0.71 to 0.38). Keep `repo_state_line` short and in the trained shape; keep option descriptions stable (order and wording are load-bearing — Kev also flips its argmax under reordering on ~40% of the eval's queries, though not on confident ones). Every routing threshold change should be justified with `run_kev_eval.py` numbers.
- **git2 vs. shell-out**: use git2 (`Git2Operations`) for reads and simple writes; shell out to the `git` subprocess where hooks, sequencer state, or credentials demand parity with real git — see the executor routing (commit family, backout family, revert, and push, which always shells out for credentials).
- **test-first**: bug-fix and behavior-change work writes the failing test first, watches it fail, then fixes.

## Design reference

[../design_handoff_gitrust_ux/README.md](../design_handoff_gitrust_ux/README.md) is the authoritative UX spec (copy strings, keymaps, glyphs, ANSI color tiers, state machines for the clarification/ambiguity/recovery flows). It is high-fidelity for copy/keymaps/glyphs, medium for layout. The `.html`/`.jsx` files there are visual references, not production code.

## History

Through v0.3.0 Layer 1 was ⚡ fast-path → ✦ an embedding router over `qwen2.5:3b`'s `/api/embed` → ◉ the qwen LLM with a ~5,800-token prompt (Ollama). The paper/blog drafts in `../writing/` describe that design and its measurements (LLM 41%, router 64% on the 44-case set). In 2026-10 both were replaced by Kev (70% top-1 stock, out-of-scope 4/4, ~0.5 s); the old prompt is preserved at `../docs/classifier_eval/legacy_prompts.rs` so the paper's harness still runs. Fine-tuning notebook: `../docs/classifier_eval/kev_finetune_colab.ipynb`.

## Status

**v0.4.0** — Layer 1/2 moved from qwen2.5:3b (Ollama) + embedding router to stock Kev-0.8B; user-written commit messages; deterministic argument capture. Measurements, design rationale and open items: [../docs/kev_migration_research.md](../docs/kev_migration_research.md). Colab fine-tunes are opt-in via `kev_run`: `runs/gitrust-v2` (round 2, 36 options incl. `commit_push`; 41/44 both states) supersedes `runs/gitrust-v1`; the harness around it adds the second-opinion checks above, the 0.9 refusal bar and the idle shutdown (research doc §12.7).


**v0.2 (phases 9–14) complete** — history and rationale in [../docs/Build_phases_v3.md](../docs/Build_phases_v3.md) and [../docs/scenario_map.md](../docs/scenario_map.md). Further work is the Tier-2 backlog in the scenario map (interpretation-echo, RemoteRewritten response flow, SafeSwitch, cross-branch pickaxe, multi-restore RecoveryBrowser).
