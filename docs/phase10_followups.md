# Phase 10 follow-ups (code-review backlog)

Findings from the Phase 10 code review (state machine: `RepoSituation`,
`detect_situation`, `situation_gate`). Fixed in the review pass: fail-open on
`build_context` error, exit-0 on state refusal, and the matrix-test coverage gap
(see `validation::test_situation_gate_matrix`). The items below are ticketed, not
yet fixed. Ordered by severity.

Most are pre-existing design limits that the new Phase 10 code inherited or now
newly *depends on* (the `origin` hardcode; reflog-only rewrite detection) rather
than fresh logic errors — but they degrade the very detection Phase 10 added.

---

## P10-1 — Diverged / RemoteRewritten only work for a remote literally named `origin`
- **Where:** `context.rs:283` (`detect_remote_rewritten`), `context.rs:489` (`get_upstream_info`)
- **What:** Both resolve the upstream as a hardcoded `refs/remotes/origin/<local-branch>`.
- **Why it matters:** A branch tracking a non-origin remote (fork `upstream`, or a renamed default) is never classified `Diverged`/`RemoteRewritten`, so `situation_gate` allows a plain push the remote will reject non-fast-forward — the exact case Phase 10 exists to catch.
- **Fix:** Resolve the real upstream via `branch.<name>.remote` / `branch.<name>.merge` (libgit2 `Branch::upstream`) instead of assuming `origin/<same-name>`. One shared helper (see P10-11).

## P10-2 — Detached-HEAD stranded-checkout: gate's stranded message is dead, rescue can orphan commits
- **Where:** `agent.rs:134`
- **What:** The reroute intercepts *every* blocked write in a `DetachedHead` situation (`matches DetachedHead && is_write_intent`) and calls `handle_detached_head_rescue`, discarding the gate's `message`. So `checkout <target>` with stranded commits never shows `detached_stranded_block(n)` and drops the requested target.
- **Why it matters:** If the user picks "switch back to previous branch" in the rescue, HEAD moves to main/master (not `<target>`) and the stranded commits are orphaned — the outcome the block message + §13 checkout row meant to prevent.
- **Fix:** Route to the rescue only for genuine write-and-lose intents (commit-like); for `checkout` with `stranded > 0`, surface the stranded block (or a dedicated branch-here ChoiceGate — S21, deferred to Phase 14).

## P10-3 — `RemoteRewritten` persists as a false positive after the user reconciles
- **Where:** `context.rs:286` (`detect_remote_rewritten`)
- **What:** Decision is made purely from the tracking ref's most-recent reflog entry (`id_old` vs current tip); it never checks the local branch's relationship to the new tip.
- **Why it matters:** After a force-push A→B and a full local reconcile (`reset --hard origin/main`, local == B), the reflog still shows the non-ff A→B, so `RemoteRewritten` keeps firing for the reflog entry's lifetime (~90 days) and the gate keeps blocking pull/push/reconcile on an already-clean repo.
- **Fix:** Only report `RemoteRewritten` when the local HEAD is *behind or divergent from* the new tip in a way the rewrite actually affects (e.g. old tip is an ancestor of local HEAD but not of new tip), not merely because the tracking ref moved non-ff at some point.

## P10-4 — Resolved-merge commit with an empty net diff is rejected by the empty-commit pre-check
- **Where:** `validation.rs:282` (pre-check 5b, `index_is_empty_vs_head`)
- **What:** `situation_gate` allows `MidMerge{resolved:true}` + commit-like, but 5b diffs index-tree vs HEAD-tree with no `MERGE_HEAD`/`was_merging` exception.
- **Why it matters:** A merge resolved by taking "ours" for every file (index tree == HEAD tree) is a legitimate two-parent merge commit, but 5b blocks it as "Nothing staged to commit," stranding the user mid-merge. (`was_merging` is consulted only in the post-check at `validation.rs:620`.)
- **Fix:** Skip the 5b empty-commit block when `repo.state() == Merge` / `MERGE_HEAD` is present.

## P10-5 — `RemoteRewritten` missed when the old remote tip has been GC'd locally
- **Where:** `context.rs:290`
- **What:** `graph_descendant_of(new_tip, old_tip)` returning `Err` (old commit no longer present) is mapped to `None` = "not rewritten."
- **Why it matters:** After a force-push + local prune/GC, the dangerous state is silently treated as safe and a blind pull/rebase onto rewritten history is allowed.
- **Fix:** Treat the `Err`/missing-ancestor case as "rewritten" (fail closed), or fetch/verify before deciding.

## P10-6 — `StaleLock` missed when the lock's mtime is in the future
- **Where:** `context.rs:302` (`stale_lock_age`)
- **What:** `SystemTime::now().duration_since(mtime).ok()?` collapses to `None` on a future mtime (clock skew / NFS).
- **Why it matters:** The friendly stale-lock remedy never fires; the write instead fails on the raw git lock with no guidance.
- **Fix:** On `duration_since` error, treat the age as effectively large (lock exists → stale) rather than `None`.

## P10-7 — Priority order masks `StaleLock` behind `DetachedHead`
- **Where:** `context.rs:196` (detached check is step 3, stale-lock is step 4)
- **What:** A repo that is both detached and holds a stale lock is classified `DetachedHead`.
- **Why it matters:** glit guides the user around detached HEAD while every write still fails on the abandoned lock, and the lock remedy is never surfaced. A stale lock blocks *all* writes, so it is arguably the more urgent classification.
- **Fix:** Consider moving the stale-lock check ahead of detached HEAD (or surface both).

## P10-8 — Non-UTF8 / unreadable conflict paths misclassify an unresolved merge as resolved
- **Where:** `context.rs:224` / `context.rs:220` (`conflicted_paths`)
- **What:** A non-UTF8 conflict path (or an index read error) yields an empty list, so `detect_situation` sets `MidMerge{resolved:true}`.
- **Why it matters:** Wrong situation → wrong prompt/status text and a misleading gate decision. **Not** data loss: the `run_pre_checks` `has_conflicts` backstop (`validation.rs:216`) uses libgit2's UTF-8-agnostic `index.has_conflicts()` and still blocks the commit.
- **Fix:** Base `resolved` on `index.has_conflicts()` (a bool) rather than on whether any conflict *paths* were decodable.

## P10-9 — Diverged gate doesn't block destructive `DiscardLocal`/`Reset`/`Undo`
- **Where:** `validation.rs:130` (Diverged arm blocks only `is_push_like`)
- **What:** §13's Diverged allow-set is reconcile-choice + read-only, but the gate lets destructive fall-through/reconcile ops through.
- **Why it matters:** `DiscardLocal` on a diverged branch discards the ahead commit. **Not silent** — danger.rs type-to-confirm still guards it — but the situation gate adds no protection and the matrix intends only the rebase/merge ChoiceGate here. (Encoded as current behavior in the widened matrix test so a future tightening trips the row.)
- **Fix:** Decide the intended Diverged policy for destructive ops when the Phase 12 ChoiceGate lands.

## P10-10 — `MidCherryPick.conflicted` / `MidRevert.conflicted` are dead state
- **Where:** `context.rs:176` / `context.rs:181` (populated), `commands.rs` (declared)
- **What:** Both fields are filled by a full `conflicted_paths` index walk but never read (`describe_situation` and `situation_gate` match `{ .. }`).
- **Why it matters:** An unnecessary index walk per cherry-pick/revert detection + two fields that falsely imply usable data (unlike `MidRebase.conflicted` / `MidMerge.conflicts`, which are read).
- **Fix:** Drop the field from these two variants, or wire it into the messaging.

## P10-11 — `detect_situation` recomputes ahead/behind (perf)
- **Where:** `context.rs:205`
- **What:** `build_context` already computes ahead/behind (stored on `RepoContext`), then calls `detect_situation`, which calls `get_upstream_info` again for the same branch.
- **Why it matters:** Two identical upstream lookups + `graph_ahead_behind` walks on every context build (every LLM call).
- **Fix:** Pass the already-computed ahead/behind into `detect_situation`. Also consolidate the duplicated upstream-derivation logic shared with `validation.rs:494` `get_ahead_behind` into one helper (ties into P10-1).

## P10-12 — `describe_situation` copy lives in `context.rs`, not `copy.rs`
- **Where:** `context.rs:106`
- **What:** New plain-language state strings ("merge in progress (conflicts in N file(s))", "diverged (ahead a, behind b)", …) are built inline, whereas the parallel `*_BLOCK` constants correctly live in `copy.rs`.
- **Why it matters:** `CLAUDE.md` convention: "User-facing strings: new/touched copy goes in `src/copy.rs`." Currently only feeds the LLM prompt (not rendered to the user), so low severity, but the copy belongs in `copy.rs` for consistency and future status-header reuse.
- **Fix:** Move the strings to `copy.rs`.
