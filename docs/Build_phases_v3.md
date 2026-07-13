# GLit Build Phases v3 — Phases 9–14

Continues the blueprint's Section 14 (phases 1–8, complete). Source of truth for _why_: `docs/scenario_map.md` (S-numbers and P-numbers below refer to it). Same iron rule as v2: **do not start a phase until the previous phase's verification passes.**

Three findings driving Phase 9 were empirically verified against live git/libgit2 behavior; they are bugs, not features to consider.

---

## How to run these phases with Claude Code (read once, then follow per phase)

1. **Place the docs in the repo.** `docs/scenario_map.md` and this file at `docs/BUILD_PHASES_V3.md` (repo root, next to CLAUDE.md). Commit them before starting.
2. **Anchor CLAUDE.md.** Add the short block below — CLAUDE.md is read at the start of every session, so this keeps every session pointed at the plan without bloating context:

    ```markdown
    ## Current work — v0.2 (scenario-map driven)

    - Plan: docs/BUILD_PHASES_V3.md (phases 9–14). Background: docs/scenario_map.md.
    - One phase per session. Never start phase N+1 until phase N verification passes.
    - Bug-fix work is test-first: write the failing test, watch it fail, then fix.
    - Current phase: 9
    ```

3. **One phase = one session.** Start each phase with a fresh context (`/clear`). Long sessions degrade; the ~20k-token cost of a fresh start is cheaper than a confused context.
4. **Plan before code, every phase.** Enter plan mode (Shift+Tab to cycle into it, or `/plan`), have Claude read the phase section + the referenced scenario-map sections + the files it will touch, and produce a step plan. Review it — edit directly with Ctrl+G if needed — then approve and switch to implementation. If a phase goes sideways twice, stop correcting: `/clear`, restate the phase goal plus what you learned, start again.
5. **Verify, review, commit, advance.** Run the phase's verification block. Then ask for a fresh-context review of the diff (the bundled `/code-review`, or: "use a subagent to review the phase 9 diff against docs/BUILD_PHASES_V3.md phase 9 and report gaps"). Commit with the phase name. Update `Current phase:` in CLAUDE.md. `/clear`. Next phase.
6. **Optional but recommended:** a pre-commit hook that runs `cargo test` — CLAUDE.md instructions are advisory; hooks are deterministic. (After Phase 9.3 lands, GLit itself will respect that hook. Enjoy the irony.)

---

## Phase 9 — Correctness hotfixes (Tier 0; P1/P3/P4; S12 ⚗️, S17 ⚗️, S47 ⚗️)

Three verified bugs. Strictly test-first: each sub-item begins by writing the failing integration test (git2 `Repository::init()` in tempdir, the Phase-3 pattern), confirming it fails for the right reason, then fixing.

**9.1 — Layer 0 reads `repo.state()`; detached-HEAD advice gated on it (S12)**

- Add `state` to `RepoContext` wrapping `git2::RepositoryState` (Clean / Merge / Rebase / RebaseMerge / RebaseInteractive / CherryPick / Revert / Bisect / …).
- The `DetachedHead` pre-check message "checkout a branch first" fires **only when `state == Clean`**. For any in-progress state, the interim block message names the operation: "A rebase is in progress. GLit can't act safely mid-rebase yet — finish with `git rebase --continue` or back out with `git rebase --abort`." (Real steps arrive in Phase 11; wrong advice stops today.)
- Include one line in `to_prompt_string()`: `State: rebase in progress (conflicts in 2 files)` — the classifier must see it.
- Failing test first: tempdir repo, start a conflicting rebase, build context → assert state is Rebase\* and the commit pre-check message does NOT contain "checkout a branch".

**9.2 — Merge-aware `commit()` (S17)**

- In `git_ops::commit()`: when `repo.state() == Merge`, read `MERGE_HEAD`, commit with parents `[HEAD, MERGE_HEAD]`, seed the message draft from `.git/MERGE_MSG`, call `repo.cleanup_state()` on success.
- Post-check for Commit gains: if the pipeline began mid-merge, assert the new commit has exactly 2 parents and `MERGE_HEAD` is gone.
- Failing test first: tempdir repo with a real conflicted merge, resolve, stage, commit via GLit's path → assert `commit.parent_count() == 2` and state returns to Clean. (Today this test produces 1 parent — that's the bug.)

**9.3 — Hook-aware routing (S47)**

- Verified: libgit2 never executes hooks. Detect executable hooks relevant to the op (`pre-commit`, `commit-msg`, `prepare-commit-msg` for Commit; `pre-push` for Push) under `core.hooksPath` or `.git/hooks`.
- When present, route the operation through the existing shell-out convention (`git commit -m …` / `git push …`) instead of git2, with an info line: "hooks detected — using system git so they run." Config knob `hooks = "auto" | "git2" | "system"` (default auto) in `~/.glit.toml`.
- Fix the `HookExists` pre-check copy: it currently claims the hook "will run" — under git2 it will not. The warn text now depends on the routing decision.
- Failing test first: tempdir repo with an executable `pre-commit` that exits 1 → commit via GLit is blocked and surfaces the hook output as a Recoverable error (this is the blueprint's own test-matrix scenario, which was previously impossible to pass).

**Verification (phase gate):** `cargo test` green including the three new integration tests; `cargo clippy` clean. Manual smoke: in a repo mid-rebase, `glit "commit"` names the rebase and never says "checkout a branch".

---

## Phase 10 — `RepoSituation` + the situation gate (P1; §12–§13 of the map)

The state machine that eliminates the "wrong advice from unrecognized state" failure class.

- `commands.rs` first (house convention): add the `RepoSituation` enum exactly as specified in scenario map §12 — Normal, EmptyRepo, NotARepo, DetachedHead{stranded_commits}, MidMerge{conflicts, resolved}, MidRebase{current, total, conflicted}, MidCherryPick, MidRevert, MidBisect, Diverged{ahead, behind}, RemoteRewritten{old_tip, new_tip}, StaleLock{age_secs}.
- `context.rs`: `detect_situation(repo) -> RepoSituation`, fully deterministic:
    - `repo.state()` covers the Mid\* family; conflict lists from the index; `resolved` = MERGE_HEAD present and index conflict-free.
    - Diverged from the existing ahead/behind when both > 0.
    - DetachedHead stranded count: revwalk from HEAD minus commits reachable from any local branch ref.
    - RemoteRewritten: previous tip of the tracking ref (its own local reflog) not an ancestor of the new tip (`graph_descendant_of`).
    - StaleLock: `.git/index.lock` exists, mtime older than 5 minutes.
- Add the situation to `RepoContext` and one plain-language line to `to_prompt_string()`.
- `validation.rs`: `situation_gate(&RepoSituation, &IntentType) -> Allowed | Blocked{message} | Redirect{suggested_intent}` implementing the §13 matrix. `agent.rs` calls it immediately after classification, **before** resolution — invalid intents never reach the resolver.
- All new user-facing strings go through `copy.rs` (house convention). Every Blocked message must state the situation AND the way out.

**Verification:** table-driven test over the full §13 matrix (each situation × representative intents → expected Allowed/Blocked and message substring). Fixture tests: bare-remote force-push → RemoteRewritten detected; stale lock file → StaleLock; detached with 2 unbranched commits → stranded_commits == 2.

---

## Phase 11 — In-progress operation steps: continue / skip / abort (S16, S18, S19)

- `commands.rs`: PipelineStep variants `MergeAbort`, `RebaseContinue`, `RebaseSkip`, `RebaseAbort`, `CherryPickContinue`, `CherryPickAbort`, `RevertContinue`, `RevertAbort`.
- Execution via the existing shell-out convention (`git rebase --continue`, etc.) — the pragmatic v1 the blueprint already blesses for gnarly ops; git2's rebase API can replace it later behind the same trait.
- Pre-checks: `*Continue` requires zero unresolved conflicts in the index (else Block: "2 files still conflicted: …"). Snapshot before every Continue (a crash mid-continue must be recoverable). Aborts are graded low danger (they restore pre-op state) and are always listed first in gates — the anxious user's exit.
- `prompts.rs`: classifier vocabulary + canonical fast-path entries: "abort rebase", "continue rebase", "abort merge", "abort cherry-pick". Safe to whitelist: with Phase 10's gate, they're only valid in their states.
- Update the §13 matrix rows so Mid\* situations allow exactly these ops + status/diff.

**Verification:** tempdir tests — conflicted rebase → resolve → `RebaseContinue` succeeds and lands on the rebased tip; `RebaseAbort` restores the original HEAD oid; `MergeAbort` restores; Continue with unresolved conflicts is Blocked with the file list. `cargo test` green.

---

## Phase 12 — Divergence reconciliation: the flagship flow (S9, S10, S13; needs 10 & 11)

The scenario that started this whole redesign — "I committed, fetched, and there's a commit I never pulled."

- `confirm.rs`: new `GateType::ChoiceGate { question: String, options: Vec<ChoiceOption { label, consequence }> }` rendered with `inquire::Select`, each option showing its one-sentence consequence. Generic — Phases 13–14 reuse it.
- `commands.rs` + `git_ops.rs`: steps `Fetch { remote, prune: bool }` (git2), `Merge { source: String }` (git2, lands in Phase 9.2's merge-aware commit on conflict-free success; on conflict, exits into `MidMerge`), `Rebase { onto: String }` (shell-out; on conflict, exits into `MidRebase` — Phase 11 owns the rest).
- `agent.rs` flow: when situation is `Diverged` and intent is push/pull/sync → pipeline: Fetch → ChoiceGate:
    - "Rebase — replay your {ahead} commit(s) on top of theirs; history stays linear. Recommended when nobody else has your commits."
    - "Merge — keep both histories, adds a merge commit. Safer if others may have pulled your commits."
    - "Abort — do nothing."
    - On success, offer Push as the final step of the same pipeline. On conflict, the calm handoff message: "Paused on conflicts in N files — nothing is lost. Resolve them, then `glit continue`, or `glit abort` to return to where you started."
- Wire the `NonFastForward` push-failure recovery to _offer this flow directly_ instead of printing "pull first" (S10).
- `Sync { strategy }` intent resolves its target branch via `origin/HEAD` (default-branch detection) and reuses the same ChoiceGate (S13).
- Migrate the Phase 10 `DetachedHead` write-block off its `agent.rs` special-case (`if matches!(…DetachedHead…)` → `handle_detached_head_rescue`) onto `SituationVerdict::Redirect { suggested_intent }`, so the gate—not `agent.rs`—drives the branch-here / switch-back way-out.

**Verification:** integration test reproducing the audited scenario — bare remote + two clones, diverge to `[ahead 1, behind 1]`, run the reconcile pipeline with a mocked gate choosing Rebase → assert final ahead/behind `0 0` on push and **both** changes present in the tree. Second test: Merge path → merge commit with 2 parents. Third: conflicting rebase path → situation transitions to `MidRebase`, pipeline pauses without error spew, snapshot exists.

---

## Phase 13 — Repair vocabulary: the undo family (S32–S34, S38–S40; P2, P6)

- `commands.rs`: steps `Amend { message: Option<String>, include_staged: bool }`, `Revert { oid: String }`, `Unstage { paths: Vec<String> }`, `SetUpstream { remote, branch }`, `SquashLastN { n: u32 }` (implemented as soft reset to `HEAD~n` + single commit — no interactive rebase; preserves working tree).
- New pre-check `PushedCommitRewrite`: target commit reachable from the remote tracking ref → escalate: explain that rewriting published history requires a force push, require TypeToConfirm, and route the eventual push through the shell-out with **`--force-with-lease`** (also switch the existing `ForcePush` shell-out to lease semantics — bare `--force` is retired).
- Undo disambiguation (the P6 fix): classifier maps all undo-family language ("undo", "take back", "that commit was wrong", …) to a single `UndoIntent`. The LLM's job ends there. A deterministic ChoiceGate then asks: "What should happen to the changes from that commit?" → keep them staged (`Reset Soft`) / keep them in files (`Reset Mixed`) / make an inverse commit — the safe choice if pushed (`Revert`) / discard completely (`Reset Hard`, TypeToConfirm). If HEAD is already on the remote, the gate reorders to put Revert first and says why.
- `Unstage` uses index manipulation scoped to paths (restore-staged semantics), never `Reset` modes.
- Danger grading: Amend/Squash inherit Hard-adjacent danger _only_ when `PushedCommitRewrite` fires; otherwise medium.

**Verification:** mocked-gate tests — an undo phrase can never reach `Reset::Hard` without the ChoiceGate having offered all four options (assert on gate invocation); amend on a pushed commit triggers the escalation path; `SquashLastN(3)` turns 3 commits into 1 with an identical tree oid; `Revert` produces a commit whose tree equals the pre-target tree; force path emits `--force-with-lease` (assert on the subprocess args).

---

## Phase 14 — Forensic recovery, trash-net, and guards (S21, S24, S26, S27, S30, S62; P5)

- `recovery.rs`: `find_lost_commits(repo) -> Vec<LostCommit { oid, age, summary, files_touched }>` — walk the reflogs of HEAD and every local branch (including recently deleted ones' surviving reflogs), collect commits unreachable from any current ref, dedupe, sort by age. Include dangling stash-shaped commits ("WIP on …", 2–3 parents) so dropped stashes surface too (S27).
- `confirm.rs`: `GateType::RecoveryBrowser` — MultiSelect-style list: "2 hours ago — 'feat: export csv' — 3 files". Restore action is always **`CreateBranchAt { name: rescue/<date>-<short-oid>, oid }`** — never resets the user's current state. New step in `commands.rs` + `git_ops.rs`.
- Intent: "find my lost work", "I lost a commit", "recover" (when no GLit snapshot is pending) → the forensic flow. Keep `glit --recover` (snapshot path) untouched — this is its outward-facing sibling.
- **Trash-net (new invariant, sibling to "the LLM never executes"):** `backup_to_trash(paths) -> ~/.glit/trash/<timestamp>/` runs before any step that deletes content not in the object database: `Clean`, `DiscardLocal`, `Reset::Hard` (working-tree delta). Config `trash_retention_days` (default 7) with prune-on-startup. The Reset::Hard success message becomes: "Done. A backup of the discarded changes is in ~/.glit/trash/… for 7 days." (S30 — a sentence git itself cannot say.)
- Destructive-phrase routing (S62): a deterministic table maps "clean up", "start fresh", "get rid of / throw away my changes" → a ChoiceGate that spells out what would be deleted (tracked changes vs untracked files, with counts) before any `Clean`/`DiscardLocal` is even constructed. Untracked deletion is graded **above** hard reset in `danger.rs` — hard reset has reflog; untracked has only the trash-net.
- Checkout guard (S21): pre-check `WouldStrandCommits { n }` when leaving a detached HEAD that has commits unreachable from any branch → gate offers `CreateBranchAt` first.
- Housekeeping fix while in this file: persist the repo workdir path inside `PipelineSnapshot` and filter `check_for_interrupted` by it (prevents worktree/multi-repo recovery cross-talk, S56).

**Verification:** tempdir tests — commit on a branch, delete the branch, forensic scan finds the commit, restore creates `rescue/*` at the right oid with current HEAD untouched; dropped stash surfaces in the scan; `Clean` writes the trash copy before deletion (assert file exists in trash and is byte-identical); Reset::Hard backs up the dirty delta; checkout from detached-with-2-commits raises the guard; snapshots from repo A never prompt recovery in repo B.

---

## After Phase 14 — the Tier 2 backlog (no phase spec yet, deliberately)

Interpretation-echo in every preview (S3/S61) · RemoteRewritten _response flow_ (detection shipped in Phase 10; the rescue-branch flow) · SafeSwitch auto-stash (S4/S15) · secret & large-file pre-checks (S35/S37) · protected-branch classification → PR flow (S43) · fork/multi-remote ChoiceGate (S44) · per-step precondition re-verify (S57) · index.lock handler (S22) · rename detection (S53) · cross-branch "find my changes" (S28).

Pick items one per session from `docs/scenario_map.md` §14 Tier 2 when phases 9–14 are green. Each is independent by design.

---

## Release gate for v0.2

Extend the blueprint's Section 15 checklist with the map's §15 test list — every ⚗️ scenario and every phase verification above must pass, plus the original 20 scenarios from v2. Then update CLAUDE.md's architecture section to document `RepoSituation`, the situation gate, the ChoiceGate/RecoveryBrowser gate types, and the trash-net invariant — the next session's Claude (and the next contributor) should learn the new shape of the system from CLAUDE.md, not from git archaeology.
