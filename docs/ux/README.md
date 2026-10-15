# GiTrust UX pass (v0.4.0, 2026-10-15)

A full pass over every prompt, menu and message a user sees, done by driving the real binary in a
pseudo-terminal and reading the rendered screen — not by reading the code. The harness is
[`ux_drive.py`](ux_drive.py) (pyte terminal emulator, 26 scenarios); raw screens are in
[`screens/before/`](screens/before) and [`screens/after/`](screens/after). Design reference:
[`../../design_handoff_gitrust_ux/README.md`](../../design_handoff_gitrust_ux/README.md) (written for
the qwen era; the Kev-era menus it doesn't cover follow its principles).

Five commits on `v0.4.0-kev`: `7b947ca` (wrong things), `ac5f0f9` (menus), `b281665` (spinner),
`a3c1123` (file picker + plan), and the help/docs commit. Tests 244 → 270; every behaviour change was
written test-first, and each new test was seen failing first.

## 1. What was wrong (correctness of what the user is told)

| Found | Before | After |
|---|---|---|
| `status` mid-rebase | "HEAD detached at HEAD · Nothing to commit, working tree clean · Try: gitrust "publish"" — with a conflicted file waiting | "⚠ Rebase in progress — stopped at commit 1 of 2 · 1 conflicted file … · Then: gitrust "continue rebase" · or go back: gitrust "abort rebase"", plus a `Conflicted:` list |
| "whats conflicting" mid-rebase | a fetch, then "No remote-tracking ref found" | status (deterministic rule, instant) |
| The plan's `[Enter] run · [q] cancel` | printed above an inquire `Execute this plan? [Y/n] (Y/n)`; typing `q` hung the prompt | one-key prompt that does what it says; stray keys and type-ahead are ignored |
| Backing out | Esc → `✗ Choice prompt failed / caused by: Operation was canceled by the user`; ~25 "Aborted." / "Nothing changed." sites exited **0** | one calm `↪ Canceled — nothing was done.`, exit **1** everywhere |
| Hard-reset warning | "This cannot be undone (except via git reflog)" — false: the trash-net backs it up, and the reflog never holds uncommitted work | "…backed up to ~/.gitrust/trash first; any dropped commit stays recoverable from git's reflog" |
| Done footer | "undo with `gitrust recover`" — does nothing after a successful run | `Next: gitrust "push" · changed your mind? gitrust "undo"` (or "reverts it — it's pushed") |
| History | same-second commits out of order ("Initial commit" second from the top) | parents before children (`TOPOLOGICAL \| TIME`) |
| "commit everything" on a clean tree | asked for a commit message, then failed | "Nothing to commit — the working tree is clean." before any prompt |
| Invalid branch name typed at the prompt | `✗ 'feat/Login Page (v2)' is not a valid branch name.` | "Git won't allow spaces or parentheses in branch names." + the prompt pre-filled with `feat/Login-Page-v2` |
| Empty branch name | "# Commit message cannot be empty." | "This can't be empty — type a value, or press Esc to cancel." |
| Interactive mode | `exit` was sent to the model ("That may not be a Git request…") | `exit` / `quit` / `q` leave; `help` prints the phrase list |
| `--help` | examples run together on one line | verbatim block, examples that actually work |
| `gitrust help` | listed `commit <file>`, `commit`, `init`, `add remote <url>` as instant — none were | a test checks every listed phrase is a fast-path phrase; `init` and `add remote [name] <url>` became real ones |
| Kev unreachable | menu of raw `git …` commands; "git push" ran commit+push; "git reset → hard" bypassed every gate; engine shown as ⚡ | the same operation list as everywhere (plain titles) through the normal gate + builder: arguments asked, destructive ops keep their gates, `--yes` refuses |

## 2. Menus in the user's language

Kev's "which did you mean?" menus showed the model's internals:

```
before
? I'm not sure which operation you meant. Which one?
> scoped push  (23%) — Commit and push only the files belonging to one named feature or topic (for e
xample the files about one area of the code).
  add commit  (23%) — Stage all changes (or the named files) and create a commit.
  push  (13%) — Upload the current branch's commits to the remote, everything, no filtering.
  none of these — Do nothing — rephrase your request.

after
? I'm not sure which operation you meant. Which one?
> Push only some changes  — pick one topic's files, commit, push
  Commit everything  — stage all changes and commit — no push
  Push  — upload all your commits
[↑↓ move · Enter choose · Esc cancel]
```

- Titles and hints live in `copy::op_label` — user copy only. The model is still asked with
  `kev_ops.json`'s descriptions, whose wording is load-bearing; a test keeps every option labelled.
- Probabilities and op names appear only with `GITRUST_DEBUG=1`. ⚠ marks options that rewrite or
  discard work.
- No "none of these" / "Abort" / "do nothing" rows anywhere (design handoff: the escape hatch is never
  a list item); every menu's help line says `Esc cancel`. After a pick, the echo is the label only.

**What GiTrust understood** is now the first line of every run (design handoff M1, plus the
interpretation echo from the scenario-map backlog):

```
before   ✦  (kev · p=0.99 · 1.1s)          ⚡  (instant)          ◇  (you picked)
after    ✦  Show history · 0.6s            ⚡  Show status        ◇  Push only the “login” changes · your choice
```

**Undo gate** — labels distinct from consequences, the "it's pushed" reason said once:

```
before
? Undo f2410be "Update README" — what should happen to its changes?
> Make an inverse commit (revert) — This commit is already on the remote, so revert is the safe defa
ult: add an inverse commit (revert) — history is preserved; safe even after pushing.
  Keep them staged — keep them staged (soft reset) — the commit is removed, files stay ready to re-c
ommit.
  …
after
? Undo 3b468fb "Update README"? It's already pushed, so reverting is the safe choice.
> Undo with a revert commit  — a new commit reverses it; history is kept — safe after pushing
  Undo, keep the changes staged  — the commit goes; its changes stay staged, ready to re-commit
  Undo, keep the changes in my files  — the commit goes; its changes stay in your files, unstaged
  Undo and throw the changes away  ⚠⚠  — both go (git keeps them ~90 days) · you'll type to confirm
```

**`--yes` refusals** name the choices: `--yes won't choose for you — I'm not sure which operation you
meant. / Choices: Push only some changes · Commit everything · Push / Run it interactively (without
--yes) to pick one, or say more precisely what you want.`

## 3. Feedback while the model works

The terminal sat blank for 0.5–13 s per model call (13 s on a busy 8 GB machine) and ~25 s on a cold
start. Now: `⠋ Understanding your request… · 4s`, `Matching files to your request…`, and — relabelling
the same line — `Starting the local model (about 25 s after it has been idle)…`. stderr, terminal
only, erased before any output or prompt.

## 4. Plan and file picker

```
before                                              after
Plan  · 2 steps · snapshot taken at fa22c4c         Plan  · 2 steps · snapshot taken at af31a0f
  ●  stage     all modified files  ⚠                  ●  stage     all modified files
  │                                                  │     + README.md
  ●  commit    "Add logout"  ⚠                       │     + src/auth.rs …
  [Enter] run · [q] cancel                           ●  commit    "Add logout"  ⚠
? Execute this plan? [Y/n] (Y/n)                    [Enter] run · [q] cancel
  ●  stage     all modified files  ⚠  ●              ✓  stage     all modified files
  ●  commit    "Add logout"  ⚠  ●                    ✓  commit    "Add logout"
  ✓ Done · 1 commit on main, ↑1 ↓0                  ✓ Done · 1 commit on main, ↑1 ↓0
  Next: … undo with `gitrust recover`               Next: gitrust "push" · changed your mind? gitrust "undo"
```

- "stage all" lists the files; staging carries no ⚠ (design handoff: don't dilute the marker); push
  is ⚠ only on a shared branch (main/master/develop/trunk or someone else's commits).
- Clean-up/undo steps say what happens to your work: `discard  uncommitted changes to tracked files`
  (was `reset  hard (discards working tree)`).

```
before                                              after
Select files to stage                               Pick the files for this commit · 2 of 4 changed files pre-selected
  Related   (2, pre-checked)                        ? Stage which files?
  Uncertain (2, visible, not pre-checked)           > [x] src/login.rs  edited · name matches “login”
? Select files to stage (Space to toggle, …):         [x] src/auth.rs   edited · likely part of “login”
> [x] [related]   src/login.rs  filename match        [ ] notes.txt     new · might be part of “login” — your call
  [x] [related]   src/auth.rs  kev p=0.77             [ ] README.md     edited · might be part of “login” — your call
  [ ] [uncertain] notes.txt  kev p=0.30             [↑↓ move · Space toggle · → all · ← none · Enter stage · Esc cancel]
```

## 5. Deliberate decisions (call them out if you disagree)

- **Every back-out exits 1.** Esc, Ctrl-C, `q`, declining a plan, an empty file selection: nothing was
  done, and a not-done operation must not read as success to a script (the same rule already applied
  to refusals and pauses). The message is calm, not an error.
- **No escape rows in menus**, Esc only — per the design handoff. The help line under every menu says so.
- **Type-ahead never confirms a plan.** An Enter pressed before the plan prompt exists arrives as
  Ctrl-J and is ignored; you confirm a plan you have seen.
- **The model's probability is debug-only.** Users see what GiTrust understood, not how sure the model was.
- **`init` and `add remote [name] <url>` are now exact phrases** (they were advertised as instant).

## 6. Not done (worth doing next)

- Direct-key shortcuts (`[a] [b] [c]`) and group toggles in the file picker — inquire's type-to-filter
  conflicts with letter keys; needs a small custom picker.
- Per-file `+N −M` in the picker (needs the manifest's line counts plumbed through the classification).
- When an argument is asked (e.g. a branch name), the prompt appears before the "✦ Create branch …"
  line, because the line names the argument.
- Misclassification recovery (`[w]rong?`) + per-repo aliases (handoff M8) — also the natural source of
  real phrasings for a future training round.
- Plan focus/edit/skip keys (handoff Variant C); "fetched N min ago" in status; the force-push
  explanation block is long and wraps at 100 columns.

## Reproduce

```bash
cd docs/classifier_eval && KEV_DIR=<kev checkout> ./kevctl.sh start runs/gitrust-v2
cd ../ux && uv run --with pyte python ux_drive.py          # → screens/after/*.txt
```
