# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

GLit is an agentic Git CLI: a single Rust binary that turns natural-language queries ("push login changes") into safe, confirmed Git operations. Intent classification runs on a **local** LLM via Ollama (default `qwen2.5:3b`) — no network calls to hosted models. The binary lives in the `glit/` subdirectory of the repo root; the Cargo project root is `glit/`.

## Commands

All commands run from `glit/` (where `Cargo.toml` lives).

```bash
cargo build                 # debug build
cargo build --release       # optimized (opt-level=z, lto, stripped — see [profile.release])
cargo test                  # run all ~163 unit tests
cargo test <name>           # single test by name substring, e.g. cargo test test_default_config
cargo test --package glit intent::   # tests in one module
cargo run -- "show status"  # run a one-shot query
cargo run                   # run the interactive REPL
cargo run -- setup          # verify Ollama + pull the model
cargo clippy                # lint
cargo fmt                   # format
```

Runtime prerequisite: Ollama must be installed and running, with the model pulled (`glit setup` handles both). Config lives at `~/.glit.toml`; runtime state (snapshots, logs) at `~/.glit/`.

## Architecture

The core is a **5-layer pipeline**, orchestrated in [src/agent.rs](src/agent.rs) (`run_agent`). Data flows one direction, each layer narrowing ambiguity until the executor receives fully-concrete steps:

- **Layer 0 — Context** ([src/context.rs](src/context.rs)): `build_context` collects repo state (branch, ahead/behind, modified files) into a `RepoContext`, fed to every LLM call.
- **Layer 1 — Intent** ([src/intent.rs](src/intent.rs)): classifies the query into an `IntentType`. Two engines:
    - **Fast path** — `try_canonical` matches a whitelist of exact phrases (after trim+lowercase, never substring) and bypasses the LLM entirely (⚡ instant).
    - **LLM path** — `OllamaClassifier` calls Ollama with prompts from [src/prompts.rs](src/prompts.rs), parses JSON into an `IntentType` (◉). A `clarification_loop` re-classifies up to `max_iters` times when the model returns `NeedsClarification`.
- **Layer 2 — Resolve** ([src/resolver.rs](src/resolver.rs), [src/manifest.rs](src/manifest.rs)): `TwoPassResolver` turns a fuzzy `FileSelection::Pattern` (e.g. "login page") into concrete file paths, classifying each modified file as related / uncertain / unrelated (`ClassificationResult`) using file diffs.
- **Layer 3 — Validate & gate** ([src/validation.rs](src/validation.rs), [src/confirm.rs](src/confirm.rs), [src/danger.rs](src/danger.rs)): pre-checks (`PreCheckResult`: Ok/Warn/Block) run before a step; a `ConfirmationGate` shows the user a `GateType` (file multi-select, editable commit message, type-to-confirm for destructive ops). Danger level is graded by **blast radius**, not just reversibility. Post-checks verify success.
- **Layer 4 — Execute** ([src/executor.rs](src/executor.rs), [src/git_ops.rs](src/git_ops.rs)): `DefaultExecutor` runs `PipelineStep`s against `Git2Operations`. Every field in a `PipelineStep` is concrete — no `Pattern`, `Current`, or `Draft` survives to this layer.

### Type vocabulary — the seam between layers

[src/commands.rs](src/commands.rs) defines the core types and is the best file to read first. Note the deliberate split:

- **`IntentType`** (`ReadOnly` / `ScopedWrite` / `FeaturePipeline` / `NeedsClarification`) is Layer 1 output — may still contain ambiguity (`FileSelection::Pattern`, `CommitMessage::Draft`).
- **`PipelineStep`** is Layer 4 input — fully resolved. The resolver/validation layers exist to convert one into the other.

`FeaturePipeline` intents (e.g. `AddCommitPush`) expand into multiple `PipelineStep`s; `ReadOnly` and most `ScopedWrite` map to one.

### Trait-based seams for testing

Each layer's behavior sits behind a trait so tests can swap in mocks: `IntentClassifier`, `FileResolver`, `Validator`, `ConfirmationGate` (with `AutoConfirmGate` for non-interactive runs), `StepExecutor`, `GitOperations`, `RecoveryManager`. When adding behavior, implement against the trait rather than the concrete type. `--yes` swaps `InquireGate` for `AutoConfirmGate` but does **not** bypass type-to-confirm safety gates.

### Crash recovery

[src/recovery.rs](src/recovery.rs) + [src/log.rs](src/log.rs): before a write pipeline runs, `FileRecoveryManager` snapshots HEAD/index to `~/.glit/snapshots/<id>.json` and logs START/END per step. On next run, `check_for_interrupted` detects a pipeline that never finished; `glit --recover` (or the `recover` subcommand) restores pre-pipeline state. Destructive git operations must capture a snapshot before executing.

## Conventions

- **Adding a new operation**: add a variant to the relevant `IntentType` enum in `commands.rs`, teach the classifier prompt in `prompts.rs` (and a `try_canonical` entry if it has an unambiguous phrase), add resolution/validation if it touches files or is destructive, add the `PipelineStep` variant + `GitOperations` method, and grade its `DangerLevel` in `danger.rs`.
- **User-facing strings**: new/touched copy goes in [src/copy.rs](src/copy.rs) (glyphs, ANSI tiers, messages) — a single source of truth being migrated to incrementally.
- **Model constraints**: the target model is small (qwen2.5:3b). Prompt and flow design must account for its limits — see the UX handoff below.
- Prefer git2 (`Git2Operations`) for git actions; a few destructive ops (`PullRebase`, `ForcePush`, `DiscardLocal`) shell out to the `git` subprocess by design.

## Design reference

[../design_handoff_glit_ux/README.md](../design_handoff_glit_ux/README.md) is the authoritative UX spec (copy strings, keymaps, glyphs, ANSI color tiers, state machines for the clarification/ambiguity/recovery flows). It is high-fidelity for copy/keymaps/glyphs, medium for layout. The `.html`/`.jsx` files there are visual references, not production code.

## Current work — v0.2 (scenario-map driven)

- Plan: docs/BUILD_PHASES_V3.md (phases 9–14). Background: docs/scenario_map.md.
- One phase per session. Never start phase N+1 until phase N verification passes.
- Bug-fix work is test-first: write the failing test, watch it fail, then fix.
- Current phase: 13 (Phases 9–12 complete).

### Layer 0/3 state machine (Phase 10)

`context::detect_situation(repo) -> commands::RepoSituation` is the single, deterministic classification of repository state (Normal / EmptyRepo / DetachedHead / MidMerge / MidRebase / MidCherryPick / MidRevert / MidBisect / Diverged / RemoteRewritten / StaleLock). It is stored on `RepoContext.situation` and rendered into the classifier prompt by `describe_situation`. `validation::situation_gate(&RepoSituation, &IntentType) -> SituationVerdict` (Allowed / Blocked{message} / Redirect) enforces the §13 matrix; `agent::run_agent` calls it immediately after classification and **before** resolution, so a state-invalid intent never reaches the resolver. This is the single source of state-based blocking — do not reintroduce parallel `repo.state()` checks in `run_pre_checks`. Block messages live in `copy.rs`.

**Phase 12 — divergence reconciliation.** `SituationVerdict::Redirect(Redirect)` now carries gate-driven rescue flows (`DetachedHeadRescue`, `ReconcileDiverged{ahead,behind}`) — the gate, not `agent.rs`, drives the way out (the old detached-HEAD special-case is gone). `agent::reconcile_flow` implements the flagship flow (`fetch → GateType::ChoiceGate` rebase-vs-merge with per-option consequence sentences → run it → conditional push); `AutoConfirmGate` refuses `ChoiceGate` (no silent strategy default under `--yes`). A mid-pipeline rebase/merge conflict pauses into the Phase 11 verbs and returns a nonzero `Err` while preserving the snapshot (`--recover` defers to git). Sync resolves its target via `origin/HEAD` and never auto-pushes a rebased branch (force-with-lease is Phase 13).
