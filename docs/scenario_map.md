# GiTrust Scenario Map v1

**Coverage audit, verified potholes, and the build path to "the teammate who fixes git"**

> **Status note (2026-10, v0.4.1).** This audit was written in July 2026 against the qwen-era
> design and is kept as written. What has changed since:
> - **Layer 1 is no longer an LLM.** Wherever this map says "the LLM path" or "qwen2.5:3b",
>   read: ⚡ exact or learned phrases, then **Kev**, a local decision model that ranks GiTrust's
>   36 operations and never generates text, then a menu when it isn't confident
>   (`docs/kev_migration_research.md`). The invariant holds more strictly: the model no longer
>   supplies arguments or drafts commit messages either (S-rows that discuss "draft quality"
>   no longer apply — the user writes the message).
> - **S58 (model down):** a warning plus a menu of every operation, through the normal gates;
>   `--yes` and runs without a terminal refuse with exit 1.
> - **File scoping (S3/S5):** pass 2 asks Kev one yes/no question per file and bands the answer
>   (related / your call / unrelated), instead of one LLM call over all diffs.
> - **Tier-2 progress:** #11 interpretation-echo — **done** (the first line of every run names
>   what was understood, v0.4 UX pass); #15 SquashLastN — **done**; #19 force-with-lease default
>   — **done**; #12 RemoteRewritten and #20 index.lock — **detected** as repo situations, the
>   full response flows are still open; the rest are open (see `gitrust/CLAUDE.md`, Status).

This document maps the full space of git situations GiTrust should address, grades the current design (blueprint v2 + CLAUDE.md) against each, and specifies concrete fixes. Three findings were **empirically verified** in a live test environment (marked ⚗️): they are facts about git/libgit2 behavior, not opinions.

---

## 0. How to read this map

Each scenario has:

- **Situation** — the actual repo state / event
- **You'd hear** — how a colleague phrases it at your desk (this is what the classifier must handle)
- **Today** — verdict + honest trace through GiTrust's 5 layers as designed
- **Fix** — the concrete change: which enum, which layer, which check or gate

**Verdicts:**

| Mark | Meaning                                                                                                       |
| ---- | ------------------------------------------------------------------------------------------------------------- |
| ✅   | Handled well by current design                                                                                |
| 🟡   | Partially: detects but can't fix, or fix exists but spec/UX is thin                                           |
| ❌   | Gap: would fail, refuse unhelpfully, or (worst) misdiagnose and give wrong advice                             |
| ⛔   | Out of scope **by design** — correct call, but GiTrust must _diagnose and refer gracefully_, never fail silently |
| ★    | Differentiator opportunity: nothing else on the market does this                                              |

**Scoring lens:** each scenario is tagged `[freq / pain]`. Frequency: daily · weekly · rare. Pain: annoy · blocked · data-loss. `rare/data-loss` outranks `daily/annoy` — GiTrust's identity is the bad day, not the good one.

**Epistemic note:** verdicts are based on the blueprint v2 and CLAUDE.md. Per CLAUDE.md, the implementation already has `PullRebase`, `ForcePush`, and `DiscardLocal` as shell-outs — ahead of the blueprint. Where they differ I grade both. Confirm against source before acting on any single verdict.

---

## 1. Scorecard

| Category                      | Scenarios | Handled today | Read                                       |
| ----------------------------- | --------- | ------------- | ------------------------------------------ |
| A. Everyday operations        | 8         | ~7 / 8        | **Home turf. Strong.**                     |
| B. Divergence & sync          | 7         | ~2 / 7        | Detection strong, reconciliation missing   |
| C. Stuck / in-progress states | 8         | ~1.5 / 8      | **Worst category — includes misdiagnosis** |
| D. Lost work & forensics      | 7         | ~1.5 / 7      | Inward recovery excellent, outward absent  |
| E. Wrong-place mistakes       | 10        | ~1.5 / 10     | The fix-vocabulary gap                     |
| F. Remotes, auth, policy      | 7         | ~4 / 7        | Solid, needs fork + policy awareness       |
| G. Environment pathologies    | 10        | ~5 / 10       | Mostly fine, one verified hook bug         |
| H. GiTrust meta / LLM layer      | 8         | ~6.5 / 8      | The invariant pays off here                |

The shape of this table **is** the identity finding: GiTrust is strongest exactly where its competitors also work (A) and weakest exactly where its origin story lives (C, D, E) — the moments people walk to your desk.

---

## 2. Category A — Everyday operations (the blueprint's home turf)

### S1 — Read-only queries `[daily / annoy]` ✅

**Situation:** status, log, diff. **You'd hear:** "what's changed", "show me the last few commits".
**Today:** Fast-path canonical phrases skip the LLM (⚡); LLM path handles variants. No gates for ReadOnly by design. Correct.
**Fix:** none. Keep the canonical whitelist exact-match-only and small — substring matching would create false-positive intent, and the blueprint already forbids it. Good instinct.

### S2 — Stage named files, commit, push ✅

**Situation:** user names concrete files. **You'd hear:** "commit src/auth.tsx with message 'fix token refresh'".
**Today:** `validate_explicit_files` rejects path traversal and non-modified files, warns on binary. EditableText gate on every message. Post-checks verify index/HEAD. Solid.
**Fix:** minor — validation should also match against **untracked** files (a new file the user names is not "modified" in the statuses filter depending on flags; ensure `include_untracked` in status options or new files get a confusing `NotInRepo` error).

### S3 — Feature pipeline: "push login changes" ✅ ★

**Situation:** fuzzy scope over modified files. **Today:** the TwoPassResolver (filename → single semantic LLM call over all unmatched diffs) with confidence-grouped MultiSelect is the strongest _unique_ thing GiTrust does. Whitespace-ignored 25-line snippets, token budget, `TooManyFiles` guard at 50 — all right calls.
**Fix (quality, not correctness):**

1. Enable **rename detection** (`diff.find_similar()`) before building manifests — a renamed file otherwise appears as delete+add, doubling noise and breaking filename match.
2. The preview must **restate the interpretation in plain words** ("I understood: stage files related to _login_, commit, push to `origin/feat-x`"), not only list steps. This makes every LLM misclassification _visible_ at the gate — the cheapest safety multiplier in the whole design.

### S4 — Create / switch branch 🟡

**Situation:** `branch feat/x`, `checkout main`. **Today:** SimpleConfirm; dirty-tree checkout is Blocked with a suggestion (per scenario matrix). Blocking is safe but unhelpful — the user's next question is always "so what do I do?"
**Fix:** a **SafeSwitch** pipeline: offer `stash (incl. untracked) → checkout → optional pop`. If the pop conflicts, keep the stash entry and say so (git already preserves it on pop-conflict; GiTrust must _explain_ that the work is safe). Turns a dead-end into a one-keystroke path.

### S5 — Stash / stash pop 🟡

**Today:** basic ops exist. Gap: **pop with conflicts** panics users ("my stash disappeared / my files are broken"). Reality: on conflict, git aborts the _drop_, so the stash entry survives.
**Fix:** StashPop post-check detects conflict state → message: "Conflicts while applying. Your stash is still saved — nothing lost. Resolve, then run `stash drop`, or abort with `checkout -- .` to try later." This is exactly the reassurance a desk-visit provides.

### S6 — Simply behind upstream (fast-forward pull) ✅

**Today:** Pull with SimpleConfirm; conflicts → Fatal halt with file list (correct honesty for v1 — see S16 for the follow-through gap).

### S7 — Commit message drafting ✅

**Today:** `CommitMessage::Draft` → EditableText, always, even for `Provided`. First-line ≤72 validation. Right call: the message gate doubles as the last human check on _what_ is being committed.
**Fix:** none structural. Draft quality on qwen2.5:3b will be mediocre; that's acceptable because the human edits — the invariant working as intended.

### S8 — "push everything" / very wide scope ✅

**Today:** `TooManyFiles { count > 50 }` asks the user to narrow. Good guardrail; pairs with FileMultiSelect for the ≤50 case.

**Category A summary:** near-complete. The two cheap upgrades that matter: rename detection (S3) and interpretation-echo in the preview (S3) — the latter improves _every_ scenario in this document.

---

## 3. Category B — Divergence & sync (where S9 was your colleague's desk visit)

### S9 — Diverged: ahead N, behind M `[weekly / blocked]` 🟡 → build target

**Situation:** local commit(s) + unpulled remote commit(s) on the same branch. Verified reproduction: `## master...origin/master [ahead 1, behind 1]`.
**You'd hear:** "I committed, then fetched, and now there's a commit I never pulled and I don't know how to put mine on top."
**Today:** Layer 0 computes ahead/behind (✅ detection is deterministic and already built). `BehindUpstream` pre-check blocks the push _before_ the confusing rejection (✅ better than raw git). But then: the blueprint's fix vocabulary has no `Merge` or `Rebase` PipelineStep; `Sync` is named with one line of spec; CLAUDE.md says a `PullRebase` shell-out exists but there is no _choice_ flow.
**Fix (full spec):**

1. Layer 0 classifies `RepoSituation::Diverged { ahead, behind }` (see §12).
2. Intent "push/sync/pull" while Diverged routes to a **ChoiceGate** — a new gate type — with plain-language options: _"Rebase — replay your {ahead} commit(s) on top of theirs; history stays linear (recommended for unshared branches)"_ vs _"Merge — keep both histories and add a merge commit (safer if others already pulled your commits)"_ vs _abort_.
3. Both paths snapshot first; both are conflict-capable, so they must land in the S17/S18 machinery below.
4. On success, offer the now-clean push in the same pipeline.
   This single flow converts the most common desk-visit into GiTrust's showcase.

### S10 — Push rejected: non-fast-forward ✅→🟡

**Today:** error classification catches `NonFastForward` with "pull first, then push" (✅). But the suggestion is a dead end string.
**Fix:** the recovery _offer_ should invoke the S9 ChoiceGate directly — "Want me to reconcile now?" — closing the loop instead of printing advice.

### S11 — Pull produces merge conflicts 🟡

**Today:** Fatal halt + list files + "resolve manually" (honest, correct for v1). The gap is the _follow-through_: after the user resolves in their editor, they return to GiTrust and... nothing understands the mid-merge state (see S16/S17).

### S12 — Rebase stops on conflict midway ❌ ⚗️

**Verified:** mid-rebase, `git symbolic-ref HEAD` fails — **HEAD is detached** while `.git/rebase-merge/` exists.
**Consequence for GiTrust as designed:** Layer 0 sets `is_detached = true`; the `DetachedHead` pre-check fires and tells the user **"checkout a branch first"** — which mid-rebase is actively harmful advice (it abandons the rebase and strands the replayed commits). GiTrust would misdiagnose a stuck rebase as a detached HEAD. _A wrong confident answer is worse than no tool at all_ — this is the single most identity-damaging pothole in the map.
**Fix:** one line of git2 — `repo.state()` returns `RepositoryState::{Merge, Rebase, RebaseMerge, CherryPick, Revert, Bisect, ...}`. Layer 0 must read it and it must **override** `is_detached` messaging. See P1 in §10.

### S13 — "Sync my feature branch with main" 🟡

**Situation:** distinct from S9 — cross-branch freshness, not same-branch divergence.
**Today:** `Sync { strategy }` intent exists, unspecified. **Fix:** resolve the _target_ (main/master/default-branch detection via `origin/HEAD`), then the same ChoiceGate: `merge main into feature` vs `rebase feature onto main`, with the shared-branch warning on rebase. Conflict handling lands in S16–S18 machinery.

### S14 — Remote history was rewritten under you ❌ ★ `[rare / data-loss]`

**Situation:** someone force-pushed the shared branch; your local history no longer shares the tip. Looks like S9's "diverged" but rebasing blindly can duplicate or resurrect commits.
**Today:** invisible — ahead/behind alone cannot distinguish it.
**Fix (deterministic, no LLM):** after fetch, check whether the _previous_ tracking-ref tip (from the tracking ref's own local reflog) is an ancestor of the new tip (`graph_descendant_of`). If not → `RepoSituation::RemoteRewritten`. Response: explain in one sentence, snapshot, offer "keep my local commits on a rescue branch, then reset to remote" as the safe default. **No other git tool diagnoses this for you.** Genuine differentiator.

### S15 — Pull would overwrite uncommitted local changes 🟡

**Today:** git/git2 aborts with an overwrite error; GiTrust surfaces it via the generic error path.
**Fix:** pre-check `UncommittedWouldBeOverwritten` before Pull → offer stash-first (the SafeSwitch pattern from S4 reused). One pre-check, big daily-friction win.

---

## 4. Category C — Stuck / in-progress states (the misdiagnosis danger zone)

**The systemic problem:** `RepoContext` has `is_empty_repo` and `is_detached` but no notion of _operation in progress_. git2 exposes it in one call (`repo.state()`); the blueprint never reads it. Every scenario below is a desk-visit moment, and today GiTrust is blind to all of them.

### S16 — Mid-merge, conflicts unresolved 🟡

**Today:** `ConflictsPresent` pre-check blocks all writes and lists files (✅ safe). But there is no forward path: no "abort the merge", no guidance loop.
**Fix:** situation-gated intents: when `MidMerge{resolved:false}`, the _only_ valid ops are `status/diff`, `MergeAbort`, and per-file guidance ("show me both sides of f.txt" → diff with ours/theirs). GiTrust doesn't resolve conflicts (right call, keep it) — but it should be the calmest possible guide through them.

### S17 — Mid-merge, conflicts RESOLVED, needs the merge commit ❌ ⚗️ `[weekly / data-loss]`

**Verified:** while `.git/MERGE_HEAD` exists, a correct merge commit has **2 parents**.
**Consequence for GiTrust as designed:** the blueprint's `commit()` builds parents from `repo.head().peel_to_commit()` **only** — one parent. A user who resolves conflicts and then asks GiTrust to "commit" gets a single-parent commit: the merge relationship is silently destroyed, MERGE_HEAD state lingers, and the branch history is wrong in a way that surfaces much later. This is a **correctness bug**, not a missing feature.
**Fix:** `commit()` must check `repo.state()`; if merging, read `MERGE_HEAD`, include it as second parent, use the prepared `MERGE_MSG` as draft, and call `repo.cleanup_state()` after. ~15 lines. Ship before anything else in this document.

### S18 — Mid-rebase (continue / skip / abort) ❌ ⚗️

The S12 finding, from the stuck side. **You'd hear:** "it says rebase in progress and I don't know if I should continue or abort."
**Fix:** `RebaseContinue / RebaseSkip / RebaseAbort` steps. Note the blueprint's own convention already blesses shell-outs for gnarly ops (`PullRebase` etc.) — `git rebase --continue` via subprocess is the pragmatic v1, with git2's rebase API as a later refinement. Continue's pre-check: all conflicts staged. Abort is the always-safe exit and should be offered first to anxious users.

### S19 — Mid-cherry-pick / mid-revert ❌

Same family, same fix: `repo.state()` detects; `--continue/--abort` steps; situation-gated intents.

### S20 — Detached HEAD, clean, no local commits ✅

**Today:** pre-check blocks commit with "checkout a branch first" — correct _in this state_. The lesson of S12 is that this advice must be gated on `state() == Clean`.

### S21 — Detached HEAD _with_ local commits, user switches away ❌ `[rare / data-loss]`

**Situation:** user committed while detached (post-bisect, post-checkout-of-tag), then checks out main → commits become reflog-only.
**Fix:** Checkout pre-check `WouldStrandCommits { n }`: count commits reachable from HEAD but from no branch ref (one revwalk). Gate: "You have {n} commit(s) not on any branch — create branch `rescue/...` to keep them?" This is a guardian moment no coding agent provides. ★

### S22 — Stale `index.lock` ❌

**Situation:** a crashed git process (often an IDE) left `.git/index.lock`; every write fails with a cryptic error.
**Fix:** on lock-error, deterministic check: lock file exists + no live git process + mtime older than N minutes → explain and offer removal (TypeToConfirm not needed; SimpleConfirm with the age shown). Classic desk-visit, 20-line fix.

### S23 — Bisect in progress ⛔

Detect via `repo.state()`, name it, refer to `git bisect` — and _refuse writes_ while in it. Graceful referral is the win; silence is the failure.

---

## 5. Category D — Lost work & forensics (the origin story's home)

### S24 — Accidental `reset --hard` done OUTSIDE GiTrust ❌→★ `[rare / data-loss]`

**Today:** the blueprint's recovery is **inward-facing** — superb for GiTrust's own interrupted pipelines, but for user-made disasters it only prints "here's the reflog command." The person who needs reflog explained cannot use a printed reflog command.
**Fix — the Forensic Recovery flow (deterministic Rust, zero LLM):**

1. Walk `HEAD` reflog + all branch reflogs; collect commits no longer reachable from any ref.
2. Present a **RecoveryBrowser** gate: candidates with age, message, files touched ("2 hours ago — 'feat: export csv' — 3 files").
3. Restore action is always non-destructive: **create a branch at the chosen commit** (`rescue/<date>`), never reset the user's current state.
   This turns GiTrust's weakest story into its strongest demo: "I lost my work" → 30 seconds → "here it is, on a rescue branch."

### S25 — GiTrust's own pipeline crash ✅✅

Snapshot-before-write, START/END step logging, `gitrust --recover`, working-tree-never-touched rule. **This is genuinely ahead of git's own UX** and ahead of every agentic tool. Protect it; extend it (see S30).

### S26 — Deleted branch with unmerged commits ❌

Same forensic machinery as S24: the branch's reflog persists ~30 days after deletion; candidates found there or via unreachable-commit walk. Restore-to-branch.

### S27 — Dropped / "lost" stash ❌

Dropped stashes are dangling commits with the distinctive "WIP on ..." message and 2–3 parents; findable by an odb scan filtered on that shape. Same RecoveryBrowser.

### S28 — "Where did my changes go?" ❌ ★ `[weekly / blocked]`

**Situation:** the work exists — on another branch, in a stash, in an uncommitted worktree — the user just can't find it. This is maybe the #1 desk question.
**Fix:** a `find` intent: search a string/filename across all branch tips, stashes, and the working tree (revwalk + tree diff; a bounded pickaxe). "Your `export_csv` function is in stash@{1} and on branch `spike/reports`." Ambitious but fully deterministic. Tier-2 differentiator.

### S29 — Discarded uncommitted changes (`checkout -- file` outside GiTrust) ⛔ honest

Never committed, never staged → not in the object database → gone (rare exception: previously staged blobs findable via fsck-style scan — worth attempting, worth being honest about odds). **GiTrust must say "this is likely unrecoverable" plainly.** A recovery tool that overpromises dies on its first failure.

### S30 — `reset --hard` via GiTrust itself ✅→★

**Today:** TypeToConfirm + reflog hint. **Fix (safer-than-git):** before executing _GiTrust's own_ hard reset or `DiscardLocal`, write a lightweight backup of the doomed working-tree changes (a stash-like ref or `~/.gitrust/trash/<ts>/` copy). Then "GiTrust's hard reset is undoable" becomes literally true — a sentence git itself cannot say. This generalizes as the **trash-net principle** (§12): any GiTrust operation that would delete content not in the object database backs it up first.

---

## 6. Category E — Wrong-place mistakes (the fix-vocabulary gap)

**The systemic problem:** the entire "I did X but meant Y" family needs `Amend`, `Revert`, `CherryPick`, `Unstage`, and pushed-state awareness. None exist in the blueprint's `PipelineStep`. For a tool whose identity is _fixing_ git, the fix vocabulary is the product.

### S31 — Committed on the wrong branch (unpushed) ❌ `[weekly / blocked]`

**You'd hear:** "I just committed to main but it was supposed to go on my feature branch."
**Fix:** a `MoveCommits` pipeline — deterministic composition of existing primitives: `branch feat/x at HEAD` → `checkout main` → `reset --hard origin/main` _scoped to the moved commits only_, or cherry-pick for the cross-branch case. Preview shows exactly which commits move where. Snapshot-guarded end to end.

### S32 — Amend: fix message / add forgotten file (unpushed) ❌

**Today:** the classifier has no Amend target. Danger: "fix my commit message" likely misclassifies as `Commit` → creates a _new_ commit — a valid-but-wrong op, the worst failure class.
**Fix:** `Amend { message: Option, include_staged: bool }` step + prompt vocabulary. Pre-check: is HEAD already on the upstream? (see S33).

### S33 — Amend / rewrite after push 🟡

**Fix:** pre-check `PushedCommitRewrite`: if the target commit is reachable from the remote tracking ref, escalate — explain force-push consequences, require TypeToConfirm, and **default the shell-out to `--force-with-lease`, never bare `--force`** (lease aborts if the remote moved — protects teammates; git2 lacks lease semantics, which retroactively justifies the existing ForcePush shell-out).

### S34 — "Undo my last commit" 🟡 → deterministic disambiguation `[weekly / data-loss]`

**The most dangerous word in the input space.** "Undo" can mean: reset --soft (keep staged), reset --mixed (keep unstaged), reset --hard (destroy), revert (new inverse commit), amend. A 3B model must never pick among these from vibes — this is precisely where the invariant ("LLM parses, never decides execution") must bind hardest.
**Fix:** any undo-family phrase classifies only to `UndoIntent`; a **deterministic ChoiceGate** then asks the one question that matters: _"What should happen to the changes from that commit?"_ → keep staged / keep in files / discard completely (TypeToConfirm) / commit was pushed → revert instead. The LLM's job ends at recognizing "undo."

### S35 — Committed a secret (unpushed) ❌ ★

**Fix:** pre-_commit_ check `SecretPattern` — high-precision regexes only (AWS key shapes, `BEGIN ... PRIVATE KEY`, common token prefixes) over the staged diff. Warn-not-block by default; block on private-key headers. Post-hoc: the Amend/reset flow from S32/S34. Deterministic, cheap, and a story competitors don't have.

### S36 — Committed a secret (already pushed) ⛔ honest

Correct response: "rotate the credential first — the secret is public regardless of git history"; then refer to `git filter-repo`/BFG for the rewrite. GiTrust should not attempt history rewriting in v1. Diagnose + sequence the human's actions.

### S37 — Committed a huge file / node_modules ❌

**Fix:** pre-commit `LargeFile { threshold }` warn (also catches "why is my push taking forever"). Unpushed fix = S32/S34 machinery.

### S38 — Undo a _pushed_ commit → Revert ❌

**Fix:** `Revert { oid }` step. Should be the **default public undo** GiTrust steers toward: history-preserving, team-safe, no force. One step + prompt vocabulary.

### S39 — Unstage files 🟡

Blueprint `Reset { mode }` has no pathspec. **Fix:** `Unstage { paths }` (`restore --staged` semantics) — daily-frequency, trivial.

### S40 — "Squash my last 3 commits" ❌ → achievable without interactive rebase ★

Everyone assumes this needs `rebase -i`. It doesn't: `reset --soft HEAD~N` + `commit` is deterministic, snapshot-friendly, and preserves the working tree. **Fix:** `SquashLastN { n }` pipeline; pre-checks: none of the N are merge commits, none pushed (else S33 escalation). Interactive rebase stays ⛔; squash — the 80% use of it — ships anyway.

---

## 7. Category F — Remotes, auth, server policy

### S41 — No upstream configured ✅→🟡

**Today:** `NoPushTarget` blocks with the right suggestion. **Fix:** offer to run it — `SetUpstream` step (`push -u origin <branch>`) behind SimpleConfirm — instead of printing a command for the user to retype.

### S42 — Auth failures ✅

Credential chain (agent → key file → token prompt) + classified `Auth` error with actionable suggestions is well designed. Windows ssh-agent caveat already documented — good honesty.

### S43 — Protected-branch rejection ❌

**Situation:** the push fails not for git reasons but server policy ("remote: protected branch hook declined"). Today it lands in the generic push error and the "pull then retry" suggestion is _wrong_ — retrying can never work.
**Fix:** classify server-side policy rejections (sideband message substring match) as their own failure type → "This branch is protected on the server. The path forward is a branch + pull request: want me to create `feat/...` and push there instead?" Turning a wall into a doorway.

### S44 — Fork workflow: origin + upstream ❌

**Situation:** two remotes; "push" and "sync" become ambiguous. **Today:** config default `remote=origin`; context lists remotes but nothing chooses.
**Fix:** when remotes > 1 and intent involves a remote, a ChoiceGate (remember per-repo answer in `~/.gitrust.toml` scoped config). "Sync my fork" = fetch upstream → merge/rebase onto local → push origin: a named pipeline worth first-class support given how common fork contribution is.

### S45 — Remote branch deleted / stale tracking refs 🟡

**Fix:** detect "upstream gone" (git reports it in status); offer `fetch --prune` + upstream reselect.

### S46 — Hook rejects the commit/push 🟡 (see S47 — the premise is broken)

The blueprint's test matrix expects "pre-commit hook rejects → Recoverable, offer retry/skip/abort." Fine _if hooks run at all_ — see next. Also: "skip" implies `--no-verify`, which bypasses team policy; it must be labeled as such and never the default.

### S47 — Hooks don't run AT ALL under git2 ❌ ⚗️ `[daily for hook-using teams / policy-violation]`

**Verified empirically:** same repo, same failing pre-commit hook — git CLI blocked the commit (`exit 1`, hook evidence written); **pygit2/libgit2 created the commit anyway and the hook never executed.**
**Consequence:** the blueprint contradicts reality in two places — the `HookExists` pre-check says the hook "will run" (it will not), and the test-matrix scenario above cannot occur via the git2 commit path. For teams on husky / pre-commit / lint-staged, GiTrust as designed **silently bypasses their quality gates on every commit** — from the team's perspective, a safety tool that violates policy.
**Fix:** when executable hooks exist for the op, **shell out to `git commit` / `git push` instead of git2** (the blueprint's own shell-out convention already blesses this), or run hooks manually before the git2 call. At minimum, warn loudly: "hooks present — using system git so they run." This is a ship-blocker for credibility with exactly the professional teams GiTrust wants.

---

## 8. Category G — Environment & repo pathologies

### S48 — Not a git repo 🟡

**Fix:** friendly message + walk up for the nearest enclosing repo ("you're in `~/proj/src`; the repo root is `~/proj` — run me there?"). Also: offer `git init`? No — too destructive-adjacent to guess; name the option, require explicit ask.

### S49 — Empty repo (no commits) ✅

First-commit parent handling and `peel_to_commit` guards are specified and in the test matrix. Good.

### S50 — Submodules 🟡 (by design)

Detect + warn + document unsupported. Correct v1 scoping.

### S51 — CRLF / line-ending storm 🟡

**Situation:** every file shows modified; diffs are pure noise. Whitespace-ignored snippets already protect the _resolver_ (good), but status is still noise and users panic.
**Fix:** detect the pattern (many files modified, near-zero non-whitespace diff lines) → name it: "This looks like a line-ending change, not real edits — likely `core.autocrlf`. Want an explanation before committing 200 files?" Diagnosis-as-a-feature again.

### S52 — Permission-bit-only changes 🟡

Same shape as S51, smaller: detect mode-only diffs, say so.

### S53 — Renames ❌→🟡

Covered in S3: `find_similar()` before manifest build. Also affects explicit-file validation (user names the _new_ path).

### S54 — Very large repos ✅→🟡

Status truncation (15/category), TooManyFiles guard, token-budgeted manifests — the scaling story is genuinely thought through. Watch: full-status walks on 100k-file repos are slow in libgit2; consider status pathspec limits if REPL feels laggy there.

### S55 — Git LFS ⛔

git2 doesn't run LFS filters. Detect `.gitattributes` LFS patterns → warn that pushes/pulls of LFS content need system git; consider shelling out for those repos wholesale.

### S56 — Worktrees ⛔→🟡

Partial git2 support. Two real risks: (a) misdiagnosis if state files differ; (b) **snapshot collision** — `~/.gitrust/snapshots` keyed only by pipeline id is fine, but `check_for_interrupted` must match snapshots to the _repo/worktree path_, or worktree A's crash prompts recovery inside worktree B. Store repo path in the snapshot; filter on it.

### S57 — Concurrent modification (TOCTOU) ❌

**Situation:** preview computed at T0; user confirms at T1; meanwhile the IDE auto-fetched, a hook rewrote files, or another terminal committed. Post-checks catch some damage _after_ the fact.
**Fix:** cheap **precondition re-verification** immediately before each step executes: HEAD oid and index tree oid still match what the preview assumed → else halt with "the repo changed since the preview — re-planning." Two oid comparisons; closes the race the gate design otherwise leaves open.

---

## 9. Category H — GiTrust meta / the LLM layer

### S58 — Ollama down ✅

Contextualized error with the exact fix. Also in the test matrix.

### S59 — Malformed model JSON ✅

`format:"json"` + `json_repair()` + temp 0 + the qwen-over-tinyllama correction — this whole risk row is what a real postmortem-driven design looks like.

### S60 — Low confidence → clarification ✅

0.6 threshold, clarification loop with `max_iters` cap and explicit cap message, `fallback_interactive()` when the model fails outright. Solid.

### S61 — _Confident misclassification_ 🟡 → contained, make it visible

The threshold can't catch a wrong-but-confident parse. The containment is architectural: every write passes a preview + gate, so a misparse becomes a _visible wrong plan the human rejects_, not a wrong execution. That is the invariant paying rent. The missing piece is S3's interpretation-echo — the preview must restate _what GiTrust understood_, or a plausible-looking plan for the wrong intent can slip past a skimming user.

### S62 — Innocent phrasing, destructive mapping ❌ `[weekly / data-loss]`

"Clean up my repo", "start fresh", "get rid of these changes" — each plausibly maps to discarding uncommitted or **untracked** work. Untracked deletions have **no reflog, no object-db copy: gone.**
**Fix:** (1) a deterministic destructive-phrase table routes these to a spell-it-out ChoiceGate, never directly to `DiscardLocal`/clean; (2) the **trash-net** (§12): GiTrust backs up whatever a clean/discard would delete to `~/.gitrust/trash/<ts>/` before executing. Danger grading by blast radius should rank _untracked deletion above hard reset_ — hard reset at least has reflog.

### S63 — Non-git requests 🟡

"Delete this folder", "undo my code changes" (meaning editor undo). Classifier needs an explicit out-of-scope bucket with a friendly boundary line, rather than forcing the nearest git op.

### S64 — `--yes` vs safety gates ✅

CLAUDE.md is explicit: `--yes` swaps the gate impl but TypeToConfirm still binds. Exactly right; never weaken for CI convenience.

### S65 — Fast-path phrase collisions ✅

Exact-match-after-normalize only, never substring — the discipline that keeps ⚡ safe. Keep the whitelist boring: status/log/diff class only; no whitelisted writes, ever.

---

## 10. Cross-cutting potholes — the systemic findings

The scenario grades above collapse into seven structural issues. Fixing these fixes whole categories at once.

**P1 — Layer 0 is state-blind (drives all of Category C).** `RepoContext` never reads `repo.state()`, so merge/rebase/cherry-pick/revert/bisect in progress are invisible — and worse, mid-rebase masquerades as detached HEAD (⚗️ S12), turning a safety pre-check into a source of harmful advice. _Principle to adopt: never give advice from an unclassified state._ Fix: the `RepoSituation` enum (§12) + situation-gated intents (§13). This is the highest-leverage change in the document.

**P2 — The fix-vocabulary gap (drives Categories B & E).** No Merge, Rebase{Continue,Skip,Abort}, MergeAbort, CherryPick, Revert, Amend, Unstage, Fetch, SetUpstream, CreateBranchAt in `PipelineStep`. The happy-path vocabulary is complete; the _repair_ vocabulary — the identity — is missing. Each addition is mechanical thanks to the trait seams: enum variant → prompt vocab → danger grade → pre-checks → GitOperations method → tests.

**P3 — `commit()` is not merge-aware (⚗️ S17, correctness bug).** Single-parent commit while `MERGE_HEAD` exists silently destroys the merge. ~15-line fix; ship first.

**P4 — Hooks are silently bypassed by git2 (⚗️ S47, credibility bug).** Verified: libgit2 never executes hooks; the blueprint's `HookExists` text and its hook-rejection test scenario are both built on a false premise. Shell out when hooks exist.

**P5 — Recovery is inward-facing only (drives Category D).** World-class for GiTrust's own crashes; a printed reflog hint for the user's own disasters — which are the origin story. Fix: the deterministic Forensic Recovery flow (S24/26/27) + trash-net (S30/S62). All pure Rust, perfectly on-invariant.

**P6 — "Undo"/"clean" language maps to destruction without deterministic disambiguation (S34, S62).** The one place the small model could route someone into data loss. Fix: undo/clean phrases terminate at ChoiceGates that ask _what to keep_, never at a mode the LLM guessed.

**P7 — TOCTOU between preview and execution (S57).** The gate design assumes the world freezes while the human reads. It doesn't. Fix: per-step precondition oid re-check.

---

## 11. Strengths register — what NOT to break while fixing the above

**ST1 — The invariant is the moat.** "LLM parses intent; execution is deterministic Rust" means every pothole above is an _additive_ fix — new states, new steps, new checks — never architectural surgery. That is rare and valuable; most tools with these gaps would need a rewrite.

**ST2 — Snapshot-before-write + crash recovery (S25)** is ahead of git's own UX and of every agentic competitor. The forensic flow (P5) is its natural extension, not a replacement.

**ST3 — Blast-radius danger grading + TypeToConfirm + `--yes` discipline (S64).** Friction proportional to danger, borrowed from the GitHub repo-delete pattern, and CI mode doesn't weaken it.

**ST4 — Preemption over apology.** `BehindUpstream` blocks the doomed push _before_ the confusing rejection; `RepoIsClean` bails before building a pointless pipeline. Extend this instinct (S15, S21, S35, S37) — it's the tool's personality.

**ST5 — The semantic resolver (S3)** remains the unique happy-path capability: cross-file reasoning in a single call, confidence-grouped selection, honest "uncertain" bucket, whitespace-ignored evidence.

**ST6 — Post-check verification culture.** Re-fetching the remote ref to confirm a push actually landed is diligence almost nobody does. Precondition re-checks (P7) complete the sandwich.

**ST7 — Trait seams + ~145 tests.** Every scenario in this map can become a mocked or tempdir integration test without touching production paths — the map doubles as a test plan (§15).

**ST8 — Local & private.** No diffs, code, or tokens leave the machine — a hard requirement for a real user segment, and a story hosted agents cannot tell.

**ST9 — Error classification with next actions** (`Recoverable{suggestions}` / `Fatal{repo_state, next_steps}`) — most tools stop at the git error string.

---

## 12. Proposed vocabulary v3 — concrete types (feed directly to Claude Code)

```rust
// Layer 0 addition — computed deterministically, one git2 call + cheap checks.
// This REPLACES reliance on bare is_detached for messaging.
pub enum RepoSituation {
    Normal,
    EmptyRepo,
    NotARepo,
    DetachedHead { stranded_commits: u32 },        // S20/S21
    MidMerge     { conflicts: Vec<String>, resolved: bool }, // S16/S17
    MidRebase    { current: u32, total: u32, conflicted: Vec<String> }, // S12/S18
    MidCherryPick{ conflicted: Vec<String> },      // S19
    MidRevert    { conflicted: Vec<String> },
    MidBisect,                                     // S23 (read-only + refer)
    Diverged     { ahead: u32, behind: u32 },      // S9
    RemoteRewritten { old_tip: Oid, new_tip: Oid },// S14 ★
    StaleLock    { age_secs: u64 },                // S22
}
// Detection: repo.state() covers Merge/Rebase*/CherryPick/Revert/Bisect;
// Diverged from existing ahead/behind; RemoteRewritten via tracking-ref
// reflog ancestor test; stranded_commits via revwalk(HEAD) minus branch refs.

// PipelineStep additions (each: prompt vocab + danger grade + checks + method + tests)
Fetch { remote: String, prune: bool },
Merge { source: String },            MergeAbort,
Rebase { onto: String },             RebaseContinue, RebaseSkip, RebaseAbort, // shell-out v1
CherryPick { oid: String },          Revert { oid: String },                  // S38: default public undo
Amend { message: Option<String>, include_staged: bool },                      // S32
Unstage { paths: Vec<String> },                                               // S39
SetUpstream { remote: String, branch: String },                               // S41
CreateBranchAt { name: String, oid: String },    // recovery restore target — always non-destructive
SquashLastN { n: u32 },                          // S40: soft reset + commit, no interactive rebase
SafeSwitch { target: String },                   // S4/S15: stash → checkout → optional pop
Clean { trash_net: true },                       // S62: trash_net is not optional

// New pre-checks
WouldStrandCommits { n: u32 }        // on Checkout, S21
PushedCommitRewrite                  // on Amend/Squash/ForcePush, S33 — escalate + force-with-lease
SecretPattern { matches: Vec<..> }   // on Commit, S35 — warn; block on private-key headers
LargeFile { paths: Vec<..> }         // on Commit, S37
UncommittedWouldBeOverwritten        // on Pull/Checkout, S15
RemoteRewrittenCheck                 // post-Fetch, S14
LockFilePresent { age }              // on any write, S22
HooksPresentUseSystemGit             // on Commit/Push, S47 ⚗️ — route to shell-out

// New gates
ChoiceGate { question, options: Vec<(label, consequence_sentence)> }
    // S9 rebase-vs-merge; S34 undo semantics; S44 remote choice; S62 destructive phrasing
RecoveryBrowser { candidates: Vec<LostCommit { oid, age, summary, files }> }
    // S24/26/27 — restore always = CreateBranchAt
    // v1 SHIPPED single-select (one rescue/* branch per invocation): every
    // ancestor is reachable from that one branch, and re-running is cheap.
    // "MultiSelect-style" multi-restore → Tier-2 backlog. (Phase 14. Same
    // "specs bend to reality in writing" treatment as S47's hook finding.)
    // NOTE (Phase 14, verified): candidates come from TWO sources — HEAD +
    // live-branch reflogs (carries deleted-branch recovery) AND a bounded
    // odb.foreach unreachable-commit scan (the only path that finds dropped
    // stashes; deleted-branch reflogs are REMOVED by `git branch -D`).

// Trash-net principle (new invariant, sibling to "LLM never executes"):
// Any GiTrust operation that would delete content NOT in the object database
// (untracked files via clean/discard; working-tree changes via hard reset)
// first copies it to ~/.gitrust/trash/<timestamp>/ with a pruning policy.
// Result: "GiTrust's destructive ops are undoable" — a sentence git cannot say.
```

---

## 13. Situation → allowed-intents matrix (the state machine Layer 3 enforces)

| RepoSituation             | Allowed                                                            | Blocked (with the _right_ message)                                                           |
| ------------------------- | ------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| Normal                    | everything                                                         | —                                                                                            |
| EmptyRepo                 | status, add, first commit, branch                                  | push/pull/log-heavy → "no commits yet"                                                       |
| DetachedHead {0 stranded} | status/log/diff, checkout, branch-here                             | commit → "checkout a branch first" (S20 — only safe here)                                    |
| DetachedHead {n>0}        | branch-here (offered first!), checkout via WouldStrandCommits gate | plain checkout without the gate                                                              |
| MidMerge {unresolved}     | status/diff, MergeAbort, per-file guidance                         | add/commit/push/pull/checkout → "merge in progress"                                          |
| MidMerge {resolved}       | **merge-aware commit** (2 parents), MergeAbort                     | everything else                                                                              |
| MidRebase                 | RebaseContinue/Skip/Abort, status/diff                             | commit/checkout/pull → "rebase in progress — continue or abort" (never "checkout a branch"!) |
| MidCherryPick / MidRevert | continue/abort, status/diff                                        | writes                                                                                       |
| MidBisect                 | status/log; refer to `git bisect`                                  | all writes                                                                                   |
| Diverged                  | ChoiceGate(rebase/merge), fetch, status/diff                       | plain push (today's BehindUpstream, kept)                                                    |
| RemoteRewritten           | rescue-branch + reset-to-remote flow, status                       | plain pull/rebase → "remote history changed — don't reconcile blindly"                       |
| StaleLock                 | lock-removal offer                                                 | all writes until cleared                                                                     |

This table is small, deterministic, and eliminates the entire "wrong advice from unrecognized state" failure class in one structure.

---

## 14. Priority roadmap

**Tier 0 — correctness, ship immediately**

1. Merge-aware `commit()` (P3 ⚗️) — data-integrity bug.
2. Hook-aware commit/push routing to system git (P4 ⚗️) — credibility bug.
3. `repo.state()` into Layer 0 with detached-HEAD messaging gated on `Clean` (P1/S12 ⚗️) — stop the harmful advice even before full situations ship.

**Tier 1 — the identity (turns the origin story into the product)** 4. Full `RepoSituation` + situation-gated intents (§12/§13). 5. Continue/Skip/Abort steps for merge/rebase/cherry-pick/revert (S16–S19). 6. Diverged ChoiceGate: rebase vs merge, then push (S9/S10/S13) — the flagship flow. 7. Undo disambiguation gate (S34) + `Revert` (S38) + `Amend` (S32) + `Unstage` (S39). 8. Forensic Recovery: reflog/dangling browser + restore-to-branch (S24/S26/S27). 9. Trash-net + destructive-phrase routing (S62/S30). 10. Stranded-commit checkout guard (S21).

**Tier 2 — the polish that compounds** 11. Interpretation-echo in every preview (S3/S61). 12. RemoteRewritten detection (S14 ★). 13. SafeSwitch + overwrite pre-check (S4/S15). 14. Secret & large-file pre-checks (S35/S37). 15. SquashLastN (S40). 16. Protected-branch classification → PR flow (S43). 17. Fork/multi-remote ChoiceGate (S44). 18. Precondition re-verify per step (S57). 19. force-with-lease default (S33). 20. index.lock handler (S22). 21. Rename detection (S53). 22. Cross-branch/stash "find my changes" (S28 ★).

**Tier 3 — graceful referral (diagnose, name, hand off)**
Interactive rebase beyond squash · filter-repo/BFG history rewriting (S36) · bisect (S23) · LFS (S55) · worktree full support (S56, but fix snapshot path-scoping now) · submodules (S50).

**Sequencing logic:** Tier 0 is three small diffs fixing verified wrong behavior. Tier 1 is one coherent release — "GiTrust v0.2: it doesn't just do git, it gets you _out_ of git" — and every item rides the existing trait seams. Tier 2 items are independent and parallelizable. Tier 3 is about never being silently useless.

---

## 15. Test-matrix extension

The blueprint's release checklist culture is a strength — extend it with this map. Every ⚗️ finding and every Tier 0/1 scenario becomes a `git2::Repository::init()` tempdir integration test (the Phase-3 pattern you already use):

mid-rebase state reported as Rebase not DetachedHead · commit during MidMerge produces exactly 2 parents and clears state · commit with executable failing pre-commit hook is blocked (shell-out path) · diverged repo triggers ChoiceGate, both branches end `0 0` ahead/behind with both changes present (reproduced in this audit) · remote-rewritten fixture detected and blind rebase refused · undo phrase never reaches Reset{Hard} without the keep-what gate · clean/discard writes trash-net copy first · checkout with stranded commits raises the guard · snapshot recovery filtered by repo path (worktrees) · force-push emits `--force-with-lease` · preview→execute halts when HEAD oid moved between them.

---

_Generated 2026-07-02 from blueprint v2 + CLAUDE.md, with three findings verified against live git 2.x and libgit2 (pygit2) behavior. Reproduction scripts available on request._
