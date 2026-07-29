# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

GiTrust is an agentic Git CLI: a single Rust binary that turns natural-language queries ("push login changes") into safe, confirmed Git operations. Intent classification runs on a **local** LLM via Ollama (default `qwen2.5:3b`) — no network calls to hosted models. The binary lives in the `gitrust/` subdirectory of the repo root; the Cargo project root is `gitrust/`.

## Commands

All commands run from `gitrust/` (where `Cargo.toml` lives).

```bash
cargo build                 # debug build
cargo build --release       # optimized (opt-level=z, lto, stripped — see [profile.release])
cargo test                  # run all 192 unit tests
cargo test <name>           # single test by name substring, e.g. cargo test test_default_config
cargo test --package gitrust intent::   # tests in one module
cargo run -- "show status"  # run a one-shot query
cargo run                   # run the interactive REPL
cargo run -- setup          # verify Ollama + pull the model
cargo clippy                # lint
cargo fmt                   # format
```

Runtime prerequisite: Ollama must be installed and running, with the model pulled (`gitrust setup` handles both). Config lives at `~/.gitrust.toml`; runtime state (snapshots, logs) at `~/.gitrust/`.

## Architecture

The core is a **5-layer pipeline**, orchestrated in [src/agent.rs](src/agent.rs) (`run_agent`). Data flows one direction, each layer narrowing ambiguity until the executor receives fully-concrete steps:

- **Layer 0 — Context** ([src/context.rs](src/context.rs)): `build_context` collects repo state (branch, ahead/behind, modified files) into a `RepoContext`, fed to every LLM call. `detect_situation(repo) -> RepoSituation` is the single deterministic classification of repo state (Normal / EmptyRepo / DetachedHead / MidMerge / MidRebase / MidCherryPick / MidRevert / MidBisect / Diverged / RemoteRewritten / StaleLock), stored on `RepoContext.situation` and rendered into the classifier prompt by `describe_situation`.
- **Layer 1 — Intent** ([src/intent.rs](src/intent.rs)): classifies the query into an `IntentType`. Two engines:
    - **Fast path** — `try_canonical` matches a whitelist of exact phrases (after trim+lowercase, never substring) and bypasses the LLM entirely (⚡ instant).
    - **LLM path** — `OllamaClassifier` calls Ollama with prompts from [src/prompts.rs](src/prompts.rs), parses JSON into an `IntentType` (◉). A `clarification_loop` re-classifies up to `max_iters` times when the model returns `NeedsClarification`.
- **Layer 1.5 — Situation gate** ([src/validation.rs](src/validation.rs)): `situation_gate(&RepoSituation, &IntentType) -> SituationVerdict` enforces the §13 matrix as a first-class pipeline stage, called immediately after classification and **before** resolution, so a state-invalid intent never reaches the resolver. Verdicts are **Allowed / Blocked{message} / Redirect**, where `Redirect` carries gate-driven rescue flows (`DetachedHeadRescue`, `ReconcileDiverged{ahead,behind}`) — the gate, not `agent.rs`, drives the way out. This is the single source of state-based blocking — do not reintroduce parallel `repo.state()` checks in `run_pre_checks`. Block messages live in `copy.rs`.
- **Layer 2 — Resolve** ([src/resolver.rs](src/resolver.rs), [src/manifest.rs](src/manifest.rs)): `TwoPassResolver` turns a fuzzy `FileSelection::Pattern` (e.g. "login page") into concrete file paths, classifying each modified file as related / uncertain / unrelated (`ClassificationResult`) using file diffs.
- **Layer 3 — Validate & gate** ([src/validation.rs](src/validation.rs), [src/confirm.rs](src/confirm.rs), [src/danger.rs](src/danger.rs)): pre-checks (`PreCheckResult`: Ok/Warn/Block) run before a step; a `ConfirmationGate` shows the user a `GateType` — file multi-select, editable commit message, `TypeToConfirm` for destructive ops, `ChoiceGate` for ambiguous intents (undo, reconcile, cleanup), and `RecoveryBrowser` for lost-commit restore. Danger level is graded by **blast radius**, not just reversibility. **The `--yes` refusal family is load-bearing:** `AutoConfirmGate` refuses `ChoiceGate`, `RecoveryBrowser`, and `TypeToConfirm` — `--yes` never silently picks a strategy, a commit to restore, or clears a destructive confirmation. Post-checks verify success.
- **Layer 4 — Execute** ([src/executor.rs](src/executor.rs), [src/git_ops.rs](src/git_ops.rs)): `DefaultExecutor` runs `PipelineStep`s against `Git2Operations`. Every field in a `PipelineStep` is concrete — no `Pattern`, `Current`, or `Draft` survives to this layer.

### Type vocabulary — the seam between layers

[src/commands.rs](src/commands.rs) defines the core types and is the best file to read first. Note the deliberate split:

- **`IntentType`** (`ReadOnly` / `ScopedWrite` / `FeaturePipeline` / `NeedsClarification`) is Layer 1 output — may still contain ambiguity (`FileSelection::Pattern`, `CommitMessage::Draft`).
- **`PipelineStep`** is Layer 4 input — fully resolved. The resolver/validation layers exist to convert one into the other.

`FeaturePipeline` intents (e.g. `AddCommitPush`) expand into multiple `PipelineStep`s; `ReadOnly` and most `ScopedWrite` map to one.

### Trait-based seams for testing

Each layer's behavior sits behind a trait so tests can swap in mocks: `IntentClassifier`, `FileResolver`, `Validator`, `ConfirmationGate` (with `AutoConfirmGate` for non-interactive runs), `StepExecutor`, `GitOperations`, `RecoveryManager`. When adding behavior, implement against the trait rather than the concrete type. `--yes` swaps `InquireGate` for `AutoConfirmGate` but does **not** bypass type-to-confirm safety gates.

### Crash recovery

[src/recovery.rs](src/recovery.rs) + [src/log.rs](src/log.rs): before a write pipeline runs, `FileRecoveryManager` snapshots HEAD/index to `~/.gitrust/snapshots/<id>.json` and logs START/END per step. On next run, `check_for_interrupted` detects a pipeline that never finished; `gitrust --recover` (or the `recover` subcommand) restores pre-pipeline state. Destructive git operations must capture a snapshot before executing — **except pure-backout pipelines, which deliberately don't** (the revert/backout commit *is* the recovery, so there is nothing to roll back to).

- **`recovery_plan` defers to git**: when a snapshot's pipeline left git mid-operation (a rebase/merge conflict), recovery does not fight the sequencer — it surfaces the git state and lets the Phase 11 verbs (`git rebase --abort` etc.) finish it, rather than force-resetting over an in-progress operation.
- **`repo_workdir` scoping (S56)**: `PipelineSnapshot.repo_workdir` scopes `check_for_interrupted` to the current repo, so a crash in worktree/repo A never prompts recovery in B.
- **Trash-net invariant** (sibling to "the LLM never executes"): any `GitOperations` method deleting content **not in the object database** backs it up to `~/.gitrust/trash/<ts>/` *before* the destructive git call, and is **fail-closed** — if the backup fails, the method returns `Err` and nothing is deleted.

## Conventions

- **Adding a new operation** — the full checklist (skipping a step usually surfaces later as a non-exhaustive-match compile error or, worse, an *ungoverned* intent):
    1. Add a variant to the relevant `IntentType` enum in `commands.rs`; if it reaches the executor, add the `PipelineStep` variant + `GitOperations` method too.
    2. Teach the classifier prompt in `prompts.rs`, plus a `try_canonical` entry if it has an unambiguous phrase.
    3. Add resolution/validation if it touches files.
    4. **Add a `test_situation_gate_matrix` row.** The §13 matrix is only law for *named* intents — an operation added without a matrix row is **ungoverned** (finding #12): no situation ever blocks or redirects it. Every new intent needs an explicit row.
    5. **Grade danger in BOTH `intent_danger` and `step_danger`** (`danger.rs`) — they are separate functions; grading one leaves the other returning a default. Grade by blast radius, not reversibility.
    6. **Add the exhaustive-match arms the compiler will demand**: `explain.rs` (the `Explanation` arm) and `format_step_label` in `validation.rs`, plus `summarize_step`/`step_name` in `agent.rs`.
    7. **If it commits, decide hook-routing** (hardest-won lesson): does it need the `*_system` shell-out for hook/sequencer parity? `Commit`/`Amend`/`SquashLastN` route through the shell when commit hooks are present; `Revert` shells out for sequencer parity. git2 won't run hooks — a pure-git2 commit path silently skips `pre-commit`.
    8. **If it deletes content not in the odb, honor the trash-net convention** (hardest-won lesson): back up *exactly what the op destroys* inside the `GitOperations` method, fail-closed, before the destructive call — see `reset`/`clean`. The trait carries a convention comment so this is inherited by pattern, not memory.
- **User-facing strings**: new/touched copy goes in [src/copy.rs](src/copy.rs) (glyphs, ANSI tiers, messages) — a single source of truth being migrated to incrementally.
- **Model constraints**: the target model is small (qwen2.5:3b). Prompt and flow design must account for its limits — see the UX handoff below.
- **git2 vs. shell-out**: use git2 (`Git2Operations`) for reads and simple writes; shell out to the `git` subprocess where hooks, sequencer state, or credentials demand parity with real git — see the executor routing (commit family, backout family, revert, and push, which always shells out for credentials).
- **test-first**: bug-fix and behavior-change work writes the failing test first, watches it fail, then fixes.

## Design reference

[../design_handoff_gitrust_ux/README.md](../design_handoff_gitrust_ux/README.md) is the authoritative UX spec (copy strings, keymaps, glyphs, ANSI color tiers, state machines for the clarification/ambiguity/recovery flows). It is high-fidelity for copy/keymaps/glyphs, medium for layout. The `.html`/`.jsx` files there are visual references, not production code.

## Status

**v0.2 (phases 9–14) complete** — history and rationale in [../docs/Build_phases_v3.md](../docs/Build_phases_v3.md) and [../docs/scenario_map.md](../docs/scenario_map.md). Further work is the Tier-2 backlog in the scenario map (interpretation-echo, RemoteRewritten response flow, SafeSwitch, cross-branch pickaxe, multi-restore RecoveryBrowser).
