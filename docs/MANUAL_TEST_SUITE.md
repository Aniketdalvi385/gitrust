# GiTrust — Manual Acceptance Test Suite (v0.4)

Run these against the release binary, one by one, in a throwaway sandbox. Record results in the report table at the bottom. This suite is the human-eyes complement to the 287 automated tests and the PTY screen harness (`docs/ux/ux_drive.py`): it verifies the *experience* — gates render right, copy reads right, refusals feel right — which no `cargo test` can.

**Legend:** each test lists whether it needs the Kev model server (`Model: yes/no` — exact phrases like "status", "undo", "abort rebase", "clean up my repo", "find my lost work" bypass the model entirely; `gitrust help` lists them). Tests marked ★ are the flagship demos worth screen-recording for the README.

**Since v0.2:** the model is Kev (`AniketDalvi385/gitrust-kev-0.8b@v2` by default), not qwen via Ollama; GiTrust starts it on the first query that needs it and stops it after 15 idle minutes. "Stop the model" below means `pkill -f kev.serve` (with `kev_autostart = false` in `~/.gitrust.toml`, so it isn't restarted). The first line of every run names what was understood: ⚡ exact or learned phrase, ✦ Kev (with its time), ◇ your menu pick. Every back-out (Esc, `q`, a declined plan) prints `↪ Canceled — nothing was done.` and exits 1. Copy changed in the v0.4 UX pass — where an expectation below quotes a message, the meaning is what's tested, not the exact words; current screens are in `docs/ux/screens/after/`.

---

## Environment record (fill in once)

| Field | Value |
|---|---|
| Date | |
| OS / arch | |
| git version (`git --version`) | |
| GiTrust commit (`git rev-parse --short HEAD`) | |
| `gitrust --version` (must match the tag, e.g. 0.4.1) | |
| Build (`cargo build --release`) | |
| Model (`kev_run`, default `AniketDalvi385/gitrust-kev-0.8b@v2`) | |

---

## Harness — paste once per terminal session

```bash
GITRUST=~/path/to/repo/gitrust/target/release/gitrust   # ← adjust
TESTROOT=~/gitrust-manual-tests
mk_sandbox() {                                  # fresh bare remote + working clone
  local name=$1
  rm -rf $TESTROOT/$name && mkdir -p $TESTROOT/$name && cd $TESTROOT/$name
  git init --bare remote.git -q
  git clone -q remote.git work 2>/dev/null; cd work
  git config user.email you@test.com && git config user.name you
  echo "base" > app.txt && git add . && git commit -qm "base" 
  BR=$(git symbolic-ref --short HEAD)           # main or master, whichever your git defaults to
  git push -q -u origin $BR
}
mk_teammate() {                                 # second clone that pushes a commit you never pull
  ( cd $TESTROOT/$1 && git clone -q remote.git teammate && cd teammate \
    && git config user.email t@test.com && git config user.name teammate \
    && echo "teammate hotfix" >> notes.txt && git add . && git commit -qm "teammate: hotfix" && git push -q )
}
```

Every test below assumes you start with `mk_sandbox <test-id>` unless it says otherwise, and runs `$GITRUST` from inside `work/`.

---

## Part 0 — Setup

### M-00 · Build, version, setup · Model: yes
**Steps:** `cargo build --release` from `gitrust/`; `$GITRUST --version`; `$GITRUST setup`.
**Expect:** build succeeds; **version matches the release tag** — if not, the Cargo.toml bump was missed. Setup installs Kev with uv, starts it (the first start downloads ~1.5 GB) and smoke-tests it; friendly confirmation. Record everything in the environment table.

---

## Part 1 — Everyday operations

### M-01 · Fast-path status · Model: **no** (run with the model stopped to prove it)
**Steps:** `mk_sandbox m01`; stop the model; `$GITRUST "status"`.
**Expect:** instant response starting `⚡  Show status` — no model call, no error about the model; status shows how old the remote info is ("Remote state as of the last fetch, …").

### M-02 · Model classification · Model: yes
**Steps:** `$GITRUST "anything new in my repo?"`
**Expect:** a spinner while the model works, then `✦  Show history · 0.9s` (or status) — read-only, correct summary. Note the time (~0.5–1 s warm, ~25 s if the model had to start).

### M-03 · Commit: the message is yours · Model: yes
**Steps:** `echo change >> app.txt`; `$GITRUST "commit this"`; type a message at the prompt; at the plan press `e`, change the message, then Enter.
**Expect:** GiTrust never drafts a message — it asks for one; the plan lists the staged file; `e` edits the message in place; the edited text is the commit message (`git log -1 --format=%s`). Then `$GITRUST --yes "commit this"; echo $?` on a new change → "A commit message is required — nothing was committed", exit **1**.

### M-04 · Scoped feature pipeline ★ · Model: yes
**Setup:** `mk_sandbox m04`, then:
```bash
echo "login page" > login.txt; echo "login helper" > auth.txt; echo "unrelated" > readme.txt
```
**Steps:** `$GITRUST "push the login changes"`
**Expect:** `login.txt` pre-selected ("name matches “login”"), `auth.txt` pre-selected or shown as "your call", `readme.txt` not selected (or at most "your call", never pre-selected); each row shows `+N −M`; `→`/`←` select all/none; after confirm, only the selected files are committed and pushed (`git show --stat origin/$BR`).

### M-05 · Branch create + switch · Model: no (canonical)
**Steps:** `$GITRUST "create branch feature-x"`; then dirty the tree (`echo x >> app.txt`) and `$GITRUST "checkout $BR"`.
**Expect:** branch created and confirmed; the dirty switch is **blocked with a clear message** (auto-stash SafeSwitch is a known Tier-2 backlog item — the block, not a crash, is the pass condition).

---

## Part 2 — Divergence: the flagship ★

### M-06 · Diverged push → rebase → clean ★ · Model: yes
**Setup:** `mk_sandbox m06; mk_teammate m06`, then in `work/`:
```bash
echo "my feature" >> app.txt && git commit -qam "feat: mine" && git fetch -q
git status -sb    # confirm: ahead 1, behind 1
```
**Steps:** `$GITRUST "push"` → ChoiceGate appears → read both consequence sentences aloud (do they explain rebase vs merge to a non-expert?) → choose **Rebase**.
**Expect:** rebase runs, push completes in the same pipeline; `git rev-list --left-right --count origin/$BR...HEAD` → `0 0`; `git log --oneline` shows teammate's commit *below* yours; both `app.txt` and `notes.txt` contents present. **This is the origin-story scenario — record it.**

### M-07 · Diverged → merge path · Model: yes
**Setup:** same as M-06 in a fresh sandbox.
**Steps:** `$GITRUST "push"` → choose **Merge**.
**Expect:** merge commit created (`git log -1 --format=%P | wc -w` → 2); pushed; `0 0`; both changes present.

### M-08 · Conflicting rebase → calm pause → continue · Model: partial
**Setup:** `mk_sandbox m08; mk_teammate m08` **but** make the teammate edit `app.txt` line 1; locally also edit `app.txt` line 1 and commit; fetch.
**Steps:** `$GITRUST "push"` → Rebase → conflict expected.
**Expect:** the calm handoff — names the conflicted file, says nothing is lost, names `gitrust "continue rebase"` / `gitrust "abort rebase"`; exit code nonzero (`echo $?`); **no** stack-trace spew. Then: fix `app.txt`, `git add app.txt`, `$GITRUST "continue rebase"` (Model: no) → rebase completes; push if offered; history linear.

### M-09 · Abort restores exactly · Model: no
**Setup:** capture the pre-rebase tip (`PRE=$(git rev-parse HEAD)`) **before** invoking gitrust, then reproduce the M-08 conflict state.
**Steps:** `$GITRUST "abort rebase"`.
**Expect:** HEAD returns to exactly `$PRE`; working tree clean; situation back to normal (`$GITRUST "status"` shows no rebase language).

### M-10 · Sync with main via origin/HEAD · Model: yes
**Setup:** `mk_sandbox m10`; `git remote set-head origin --auto`; create `git checkout -b feature`; meanwhile advance the default branch via the teammate helper; fetch.
**Steps:** `$GITRUST "sync with main"`.
**Expect:** target resolves to the default branch automatically; ChoiceGate offers rebase/merge; after **Rebase** on this *unpublished* branch: no auto force-push drama (branch has no upstream); feature now contains main's commit.

---

## Part 3 — Stuck states (the misdiagnosis class, verified dead)

### M-11 · Mid-rebase never says "checkout a branch" · Model: yes
**Setup:** reproduce the M-08 conflict pause (mid-rebase).
**Steps:** `$GITRUST "commit my changes"`.
**Expect:** the block names the **rebase** ("A rebase is in progress…") and offers the continue/abort verbs. FAIL instantly if the words "checkout a branch" appear — that's the resurrected S12 bug.

### M-12 · Merge-aware commit: two parents · Model: yes
**Setup:** `mk_sandbox m12`; create a conflicting merge:
```bash
git checkout -qb side && echo A > app.txt && git commit -qam side
git checkout -q $BR && echo B > app.txt && git commit -qam mainline
git merge side   # conflicts
echo RESOLVED > app.txt && git add app.txt   # resolved, NOT committed — MERGE_HEAD present
```
**Steps:** `$GITRUST "commit the merge"`.
**Expect:** commit succeeds with **exactly 2 parents** (`git log -1 --format=%P | wc -w` → 2); merge state cleaned (`git status` shows no merge language). This was the silent-merge-destruction bug — two parents is the whole test.

### M-13 · Detached write → rescue · Model: yes
**Setup:** `git checkout -q $(git rev-parse HEAD)` (detaches HEAD); `echo x >> app.txt`.
**Steps:** `$GITRUST "commit this"`.
**Expect:** not a dead-end block — the interactive rescue offers **branch-here** (and switch-back); choosing branch-here creates the branch and proceeds sanely.

### M-14 · Stranded-commit guard ★ · Model: no
**Setup:** detach as in M-13, then make a commit **while detached** (`echo y >> app.txt && git commit -qam "detached work"`).
**Steps:** `$GITRUST "checkout $BR"`.
**Expect:** the guard refuses the naive switch-away and offers branch-here **first**, telling you commits would be stranded; taking branch-here preserves the commit on a new branch; nothing lost.

---

## Part 4 — The undo family (P6, defused)

### M-15 · Undo gate: four options, names the commit · Model: no ("undo" is canonical)
**Setup:** `mk_sandbox m15`; `echo v2 >> app.txt && git commit -qam "feat: the mistake"`.
**Steps:** `$GITRUST "undo"`.
**Expect:** ChoiceGate with **exactly four options** (keep staged / keep in files / inverse commit / discard), question names the short oid + "feat: the mistake". Choose **keep staged** → `git status` shows changes staged, `git log` shows the commit gone.

### M-16 · Undo → discard: TypeToConfirm + trash-net · Model: no
**Setup:** as M-15 (fresh), note the content of the modified file.
**Steps:** `$GITRUST "undo"` → choose **Discard completely**.
**Expect:** TypeToConfirm gate before anything happens; after confirming, the success line names the trash backup ("…backup of the discarded changes is in ~/.gitrust/trash/<ts>/ for 7 days"); verify: `ls ~/.gitrust/trash/` has a new timestamped dir; the backed-up file's content matches what you noted (`diff`).

### M-17 · `--yes` cannot undo · Model: no
**Steps:** in a repo with a commit to undo: `$GITRUST --yes "undo"; echo $?`.
**Expect:** refusal naming the reason (can't auto-choose), exit code **1**, HEAD untouched.

### M-18 · Multi-count undo halts with guidance · Model: yes
**Setup:** three commits in a row.
**Steps:** `$GITRUST "undo the last 3 commits"`.
**Expect:** no gate, no action — guidance message offering `squash last 3` / revert-specific / undo-most-recent; HEAD unchanged. The count must never act (exit 1).

### M-19 · Squash last 3: tree-identical · Model: yes
**Setup:** three commits editing different files; `TIP_TREE=$(git rev-parse HEAD^{tree})`.
**Steps:** `$GITRUST "squash the last 3 commits"` → confirm, edit the message.
**Expect:** `git rev-list --count HEAD` dropped by 2; `git rev-parse HEAD^{tree}` equals `$TIP_TREE` (identical content, one commit).

### M-20 · Amend-on-pushed: escalation + lease ★ · Model: yes
**Setup:** `mk_sandbox m20`; `echo v2 >> app.txt && git commit -qam "typo mesage" && git push -q`.
**Steps:** `$GITRUST "fix the last commit message to 'typo message fixed'"`.
**Expect:** GiTrust detects the commit is published → escalation explains a force-push is required → TypeToConfirm on the branch name → after confirm, remote advances to the amended tip (`git rev-parse origin/$BR` == local HEAD) and the message is fixed on the remote. No bare `--force` anywhere in the output.

---

## Part 5 — Hook parity (the S47 family)

### M-21 · Failing pre-commit blocks (via system git) · Model: yes
**Setup:** `mk_sandbox m21`, then:
```bash
cat > .git/hooks/pre-commit << 'EOF'
#!/bin/sh
echo "LINT FAILED"; exit 1
EOF
chmod +x .git/hooks/pre-commit; echo x >> app.txt
```
**Steps:** `$GITRUST "commit this"`.
**Expect:** an info line that hooks were detected (system git routing); the commit is **blocked**; the hook's own output ("LINT FAILED") appears in the failure message; `git rev-list --count HEAD` unchanged.

### M-22 · Revert parity: succeeds despite the same hook · Model: no ("revert last commit" canonical)
**Setup:** keep M-21's failing hook; ensure there's a commit to revert.
**Steps:** `$GITRUST "revert last commit"`.
**Expect:** the revert **succeeds** (inverse commit exists) — matching real git, where revert doesn't run pre-commit. This asymmetry with M-21 is intentional; both passing together is the parity proof.

---

## Part 6 — Forensics & trash-net (the origin story) ★

### M-23 · Deleted branch → find my lost work ★★ · Model: no (phrase-triggered)
**Setup:** `mk_sandbox m23`, then:
```bash
git checkout -qb experiment && echo "precious work" > precious.txt && git add . && git commit -qm "the work I'll lose"
LOST=$(git rev-parse HEAD); git checkout -q $BR && git branch -D experiment
HEAD_BEFORE=$(git rev-parse HEAD)
```
**Steps:** `$GITRUST "find my lost work"` → RecoveryBrowser lists candidates → pick "the work I'll lose".
**Expect:** a `rescue/<date>-<oid>` branch created **at exactly `$LOST`** (`git rev-parse rescue/...` == `$LOST`); current HEAD untouched (`git rev-parse HEAD` == `$HEAD_BEFORE`); working tree untouched. **This is the desk-visit, automated — record it for the README.**

### M-24 · Dropped stash surfaces · Model: no
**Setup:** `echo wip >> app.txt; git stash push -m "important wip"; git stash drop`.
**Steps:** `$GITRUST "find my lost work"`.
**Expect:** the browser lists a stash-shaped candidate (WIP message); restoring it creates a rescue branch containing the wip change.

### M-25 · Empty-scan honesty · Model: no
**Setup:** fresh sandbox, nothing lost.
**Steps:** `$GITRUST "find my lost work"`.
**Expect:** the empty-result message includes the honest caveat about never-committed or garbage-collected work being possibly unrecoverable — not a bare "nothing found".

### M-26 · Clean-up gate + trash backup, nested dirs · Model: no
**Setup:** `echo mod >> app.txt` (tracked change) and `mkdir -p junk/inner && echo tmp > junk/inner/file.txt` (nested untracked).
**Steps:** `$GITRUST "clean up my repo"` → read the gate: it must state the tracked/untracked **counts** and name the trash-net → choose delete-untracked.
**Expect:** `junk/` gone from the tree; `~/.gitrust/trash/<ts>/junk/inner/file.txt` exists and `diff`s identical (the nested-dir hole, verified by hand); the tracked modification untouched.

### M-27 · Fail-closed trash-net · Model: no
**Setup:** untracked file present; `mkdir -p ~/.gitrust/trash && chmod 000 ~/.gitrust/trash`.
**Steps:** `$GITRUST "clean up my repo"` → choose delete-untracked; `echo $?`.
**Expect:** the operation **refuses** (nonzero, error names the backup failure); the untracked file **still exists**. Nothing was deleted without a backup. **Cleanup: `chmod 755 ~/.gitrust/trash`.**

### M-28 · Recover precedence: sideways pointer · Model: no
**Setup:** fresh sandbox, no interrupted pipeline.
**Steps:** `$GITRUST --recover`.
**Expect:** "no interrupted pipeline" + the pointer to `gitrust "find my lost work"` — the two recovery worlds introduce each other.

---

## Part 7 — Meta

### M-29 · Model down: graceful degrade · Model: —
**Setup:** stop the model (`pkill -f kev.serve`, `kev_autostart = false`).
**Steps:** `$GITRUST "what changed in the auth files?"` (needs the model), then the same with `--yes`, then `echo q | $GITRUST "what changed"` (no terminal), then `$GITRUST "status"`.
**Expect:** a warning naming the fix (`gitrust setup` / `kev_autostart`), then a menu of every operation by title — picking one runs it through the normal gates; with `--yes` a refusal listing the choices, exit 1; with no terminal "This needs an answer, and there's no terminal to ask in…", exit 1; `status` still works instantly (⚡). No panic, no raw error chain anywhere.

### M-30 · Not a repo · Model: no
**Steps:** `cd /tmp && $GITRUST "status"`.
**Expect:** friendly not-a-repo message, no stack trace, nonzero exit.

---

## Test report

Copy this file to `docs/TEST_REPORT_v<version>.md`, fill the environment table, and mark each row. A FAIL needs: what you saw, exact command, and `git status -sb` at failure — enough to reproduce. File each FAIL as a GitHub issue after publishing; the issue links become part of the report.

| ID | Title | Result (PASS / FAIL / BLOCKED / SKIP) | Notes / evidence |
|---|---|---|---|
| M-00 | Build, version, setup | | |
| M-01 | Fast-path status (model off) | | |
| M-02 | Model classification | | |
| M-03 | Editable commit message | | |
| M-04 | Scoped feature pipeline ★ | | |
| M-05 | Branch + dirty-switch block | | |
| M-06 | Diverged → rebase → 0/0 ★ | | |
| M-07 | Diverged → merge (2 parents) | | |
| M-08 | Conflict pause → continue | | |
| M-09 | Abort restores exact tip | | |
| M-10 | Sync via origin/HEAD | | |
| M-11 | Mid-rebase message (no "checkout a branch") | | |
| M-12 | Merge-aware commit (2 parents) | | |
| M-13 | Detached rescue | | |
| M-14 | Stranded-commit guard ★ | | |
| M-15 | Undo gate: 4 options + names commit | | |
| M-16 | Discard: TypeToConfirm + trash | | |
| M-17 | --yes undo refused (exit 1) | | |
| M-18 | Multi-count undo halts | | |
| M-19 | Squash: tree-identical | | |
| M-20 | Amend-on-pushed: lease escalation ★ | | |
| M-21 | Failing pre-commit blocks | | |
| M-22 | Revert hook parity | | |
| M-23 | Lost branch → rescue ★★ | | |
| M-24 | Dropped stash found | | |
| M-25 | Empty-scan honesty | | |
| M-26 | Clean-up gate + nested trash | | |
| M-27 | Fail-closed trash-net | | |
| M-28 | Recover sideways pointer | | |
| M-29 | Model-down degrade | | |
| M-30 | Not a repo | | |

**Summary:** ___ / 31 PASS · Blockers: ___ · Filed issues: ___
