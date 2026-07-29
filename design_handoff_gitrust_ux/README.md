# Handoff: GiTrust UX redesign

## Overview

This handoff documents a UX pass on **GiTrust**, an agentic CLI that wraps Git in natural language (Rust, single binary, qwen2.5:3b via Ollama for intent classification). The package covers:

- **6 cross-cutting principles** for the entire CLI surface
- **5 selection patterns** (replacing the current flat 4-option picker)
- **8 model-aware patterns** designed around qwen2.5:3b's hard limits
- **Per-flow rewrites** for merge-conflict prediction, plan-execution, status, reset, detached-HEAD, branch-name sanitization, and recovery snapshots
- **An interactive prototype** of the plan-execution flow with 3 keymap variants

The goal is concrete, drop-in changes — exact copy strings, exact keymaps, exact ANSI/glyph mappings — not abstract principles.

---

## About the design files

The files in this bundle are **design references created in HTML** that visualize how GiTrust's terminal output should look and behave. They are NOT production code.

The actual implementation target is the existing **Rust codebase**. The HTML mockups translate to:

- **ANSI escape codes** for color (mapping table below)
- **Unicode glyphs** for markers (⚠ ⛔ ✓ ✗ ↑ ↓ ⠋ ●)
- **Rust crate calls** for interactive picker / spinner / confirm (`inquire`, `indicatif`, `console`/`dialoguer`)
- **String constants** for copy (collected in the "Copy library" section below)
- **State machines** for the new flows (NeedsClarification, AmbiguousFeature, TooManyFiles, failed-step recovery — specced below)

The React prototype (`prototype.jsx`) is a behavioral reference for the plan-execution flow's three keymap variants. The reducer there shows the exact state transitions to implement in Rust.

---

## Fidelity

**High-fidelity for copy, keymaps, glyphs, and color tiers.** Exact strings to use, exact keybindings, exact markers. Implement these as specified.

**Medium-fidelity for layout.** Terminal layout is necessarily approximate (depends on terminal width). The mockups show the *information hierarchy* — what goes on which line, what's dim, what's emphasized — but the developer should adapt to the user's actual `COLUMNS`.

**Out of scope (no design here, use Rust judgment):**
- Error wrapping past 80 chars
- Pagination of long output
- Windows-specific color/glyph fallbacks (a spec is below but implementation is yours)
- Performance of the LLM pipeline itself

---

## Implementation priority

Do these in order — earlier items unlock or de-risk later ones.

| # | Pattern | Why first |
|---|---------|-----------|
| 1 | **Pin `inquire::Select` cursor to index 0** | One-line fix; the screenshot bug. Resolves Q4. |
| 2 | **Engine indicator (`⚡` vs `◉`)** | Tiny visual change with outsized trust impact; touches every command. |
| 3 | **Bucketed file picker (P5)** with per-file reasons | Surfaces existing `ClassificationResult` data; biggest single UX win. |
| 4 | **Tightened plan-execution box (06½) + Variant C keymap** | Replaces the current `╔═╗ GiTrust plan ╚═╝` chrome; the prototype is the spec. |
| 5 | **Stage labels during LLM call (M2)** | Replaces blank-cursor latency with intent + budgets. |
| 6 | **Predict + confirm picker (P1)** for merge-conflict resolution | Turns the 4-option flat list into a 1-keystroke confirmation. |
| 7 | **`.git/gitrust/aliases.toml` (M8)** + correction recording flow | The flywheel — each user's repo gets smarter the more they use it. |
| 8 | **Polish flows for NeedsClarification (M3), AmbiguousFeature (M4), TooManyFiles (M5)** | Existing failure modes that deserve first-class UX. |
| 9 | Everything else in the doc | Status line tightening, reset preview, detached-HEAD rescue, branch-name sanitization. |

---

## Six cross-cutting principles

These apply everywhere — bake them into every output surface.

### 1. Speak GiTrust, not Git

In any list of resolution options or commands, all options use `gitrust "…"`, never raw `git …`.

**Bad:**
```
1. Merge:        gitrust "pull"
2. Rebase:       git pull --rebase
3. Force push:   git push --force-with-lease
```

**Good:**
```
1. Merge        gitrust "pull"
2. Rebase       gitrust "rebase onto origin"
3. Force push   gitrust "force push"
```

Underlying git command can be shown dim on the same line if useful. Never as the primary action.

### 2. State facts confidently

Once `git fetch` is complete, you *know*. Drop hedging language.

| Replace | With |
|---------|------|
| `Merge conflicts predicted` | `Pull will conflict` |
| `Probably modified` | `Modified` |
| `Likely your remote is ahead` | `1 ahead, 3 behind on main` |

### 3. Pick a default for every choice; demote the others

Cursor parked on the safest option, single-key shortcuts for the rest, escape hatch on `q`/`Esc` never as a list item.

### 4. Severity reflects the branch, not the command

| Command | Marker if on own feature branch | Marker if on shared/protected branch |
|---------|----------------------------------|---------------------------------------|
| `commit` | `⚠` | `⚠` |
| `push` | (none) | `⚠` |
| `force-push` | `⚠` | `⛔` |
| `reset --hard` | `⚠⚠` | `⛔` |

Detection: branch name match (`main`/`master`/`develop`/`trunk`) — free, on every classification. See M-section below for the full latency-tiered approach.

### 5. Cut unsolicited explanations

If the user didn't ask "why isn't stash working," don't explain it. Put explanations behind `[?]`.

**Cut:**
> Manual fix needed: Merge/rebase will pause with conflict markers (<<<<<<<, =======, >>>>>>>) inside the affected files. Edit each file to keep the right version, remove the markers, then `git add` it and continue.
>
> Both sides committed conflicting changes. Stash will NOT help — it only captures UNCOMMITTED changes, and you have none. Real options:

**Replace with:**
> ⚠ Pull will conflict · README.md (~3 lines)
> [Press `?` for what conflict resolution looks like]

### 6. Recovery mentioned BEFORE the scary choice

Inline the safety net next to the loss, not in a header above the input prompt.

```
⚠⚠ gitrust "reset --hard HEAD~3"
   loses 3 commits, 99 uncommitted lines
   recoverable for 30 days via:  gitrust undo
```

---

## Per-flow specs

### Flow 01 · Merge-conflict prediction

**Replace the current 4-option flat picker (the screenshot) with this:**

```
$ gitrust "do I have conflicts if I pull"
  dbg → read-only · merge_preview on main      [shown only if GITRUST_DEBUG=1, dim]

Fetched origin/main · 1 ahead, 1 behind

⚠ Pull will conflict
  README.md  both sides edited the intro · ~3 lines overlap

Suggested  Merge — keep both histories, resolve once
  [Enter]  gitrust "pull"           merge commit · resolve markers in README.md

Other options
  [r]  gitrust "rebase"         replay your work on top · resolve per commit
  [f]  gitrust "force push"     ⚠   rewrites 1 remote commit · you own this branch
  [d]  gitrust "discard mine"   ⚠   drops 1 local commit · gitrust undo restores it for 30d

  [?] explain conflict markers   [why] why these 4   [q] quit
▸ _
```

**Key changes from current:**
- Demote the LLM reasoning line: dim, indented, prefixed with `dbg →`, only shown with `GITRUST_DEBUG=1`
- Replace "Merge conflicts predicted" with "Pull will conflict" + status line
- Annotate the file with WHAT's conflicting ("both sides edited the intro · ~3 lines overlap")
- One promoted option (Merge), three with single-key shortcuts (`r` `f` `d`)
- ALL options use `gitrust "…"`, not raw git
- Severity reflects branch reality: force-push on own branch = `⚠`, not `⚠⚠`
- Recovery promise inline: "gitrust undo restores it for 30d"
- Wall-of-text (`?`) and "why these 4" (`why`) hidden behind keys

### Flow 06½ · Plan-execution box (commit pipeline)

**Replace the current `╔═╗ GiTrust plan ╚═╝` box.** Use the timeline visual from `prototype.jsx` (Variant C keymap is the recommendation).

```
$ gitrust "commit my login changes"
⚡ understanding intent…       ~900ms
⚡ matching files (3 of 12)    ~700ms
⚡ drafting message            ~500ms

Plan  · 3 steps · snapshot taken at aa44620

  ●  stage    README.md                            +4 −1
  │
  ●  commit   "fix(README): add conflicting thing"          ⚠
  │           generated from diff · press e to edit
  │
  ●  push     → origin/main                                  ⚠

▸ [Enter] run all · [↑↓] focus · [e] edit focused · [s] skip focused · [q] cancel
```

**Mid-execution (Variant C, after Enter):**
- Node 1 fills green with `✓` (stage step done)
- Node 2 pulses yellow (commit step running)
- Node 3 stays empty (queued)
- A status line below the timeline: `Ctrl-C abort · staged work preserved · snapshot @ aa44620`

**On completion:**
- All nodes green
- Delta lines: `committed  aa44620 → 0de4a0d`, `pushed  origin/main now at 0de4a0d`
- Footer: `Done · 1 commit on main, ahead of origin by 0. Next: gitrust "status" · undo with gitrust undo`

**Specifically drop:**
- The `╔═══╗` double-line border + centered "GiTrust plan" title
- `⚠` on `git add` (staging is reversible — don't dilute the marker)
- Raw `git add` / `git commit` verbs — speak GiTrust: `stage`, `commit`, `push`
- "(all files)" without filenames — show what's being staged
- "Pipeline complete" → just "Done"

**Implementation: see `GiTrust Plan Prototype.html` + `prototype.jsx` for the three keymap variants. Recommended is Variant C.**

### Flow 03 · Reset preview + type-to-confirm

```
$ gitrust "reset 3 commits back hard"

⚠⚠ Hard reset · feat/session-refresh → HEAD~3

  Commits dropped (3)
    a3f9c12  feat: session refresh        ← HEAD, 12 min ago, you
    8b21e44  wip: token plumbing            47 min ago, you
    c0d4a89  refactor: extract auth utils   2 hr ago, you

  Working tree reverts
    src/auth/login.rs    −87 +12   (undoes today's work)
    src/auth/session.rs  deleted    (file goes away)

  ↻ Snapshot saved · gitrust undo → restores all of the above for 30 days

Type the branch name (so you know what you're resetting):
  feat/session-refresh
▸ _
```

**Key:**
- Type-to-confirm is the **branch name**, not `CONFIRM`. Same trick GitHub uses for repo deletion — forces eye contact with the thing being modified.
- Author + age per commit (`12 min ago, you`)
- Plain-English annotation per file (`undoes today's work`, `file goes away`)
- "Snapshot saved" — past tense; take it first.

### Flow 04 · Detached HEAD rescue

**Never say "detached HEAD" first.** Lead with what's at stake.

```
⚠ You're not on a branch right now.
  You're looking at commit a3f9c12 directly (the tag v1.4.0).
  Reading code here is fine. Any new commits you make will be lost unless
  you save them to a branch first.

What do you want to do?

  [Enter]  Save my current spot as a new branch     recommended
              name? explore-v1.4.0 (you can change it)
  [m]      Go back to main                          you have no new commits here yet, safe
  [s]      Stay here, I'm just reading
  [?]      Why am I in this state?

▸ _
```

**Key:**
- Pre-fill the branch name based on context (tag name → `explore-<tag>`, SHA → `explore-<sha>`)
- Anchor the SHA to something recognizable (tag, branch, "2 commits before main")
- If user has no new commits, mark "Go back to main" as safe; if they have N commits, demote it with `⚠` and promote the save option
- "Detached HEAD" explainer goes behind `[?]`

### Flow 05 · Branch-name sanitization

**Don't lecture.**

```
$ gitrust "make a branch called feat/Login Page (v2)"

Git won't allow spaces or parens in branch names.

  Suggested   feat/login-page-v2
  or type your own:  _

[Enter] use suggested · [Esc] cancel
```

- No "Error:" — nothing has failed yet
- Show only the rules that matter for THIS input ("spaces or parens"); full rules behind `[?]`
- Cursor in the "type your own" field, pre-filled with the suggestion — backspace edits, Enter accepts

### Flow 06¾ · Status command + branch list

Mostly working already. Three tightenings:

1. **Surface the silent typo-rescue** with one dim breadcrumb line:
   ```
   $ gitrust 'statsus'
     ↳ ran status
   ```
2. **Quantify "last fetched"** with a relative timestamp, color yellow past ~10min:
   ```
   Fetched: 8 min ago  ·  [r] refresh
   ```
3. **Turn "Diverged from upstream" into an inline action keymap** so the user doesn't have to retype:
   ```
   ⚠ Diverged from origin/main — you and someone else both moved on.
     [Enter]  gitrust "pull"           merge their commit in
     [r]      gitrust "rebase"         replay your commit on top
     [w]      keep working          you can resolve later
   ```

Normalize on `↑1 ↓1` arrows everywhere (the branch list already uses this; the status line uses `1 ahead · 1 behind` — pick the arrows, they're scannable and language-independent).

### Flow snapshot listing

Rename `gitrust snapshots` → `gitrust undo`. Both work; lead with `undo` because that's the panic-time command.

```
$ gitrust undo

What would you like to undo?  last 7 days, newest first

  [1]  2 min ago     before reset --hard           3 commits, 2 files
       restoring will bring back: feat: session refresh + 2 more
  [2]  1 hr ago      before force push             1 commit rewritten
       restoring will: revert origin/feat/login to the pre-force state
  [3]  yesterday     before discard mine           1 commit dropped
  [4]  3 days ago    pre-merge fix/auth → main     conflict-resolution state

  [a] show all (12 in last 30 days)   [q] quit
▸ _
```

**Key:**
- Relative timestamps, not ISO
- Describe what restoring will **do**, not just the snapshot name
- Surface `gitrust undo` by name on every `⚠⚠` / `⛔` screen

---

## Five selection patterns

When the user has to pick from a set, use these in this priority order:

### P1 · Predict + confirm (PRIMARY — replaces most pickers)

GiTrust can compute Rust-side: branch ownership (who authored recent commits), repo's merge history (last 20 merges = X rebases, Y merges), conflict size, etc. Use those signals to predict the most likely correct action and present it with one-keystroke confirmation. The full picker hides behind `[?]`.

```
⚠ Pull will conflict on README.md (~3 lines)

Suggested · Rebase  (this repo uses rebase 18/20 times; you're solo on this branch)

  [Enter]  do it           gitrust "rebase"
  [m]      merge instead   creates a merge commit
  [?]      all options     incl. force-push, discard
  [q]      back

▸ _
```

- The reasoning line ("this repo uses rebase 18/20 times") is **Rust-computed from real git data**, not LLM output. It cannot hallucinate.
- Promote the second-most-likely option to a single key (`[m]` here).
- Hide the long tail behind `[?]`.
- Fallback: if prediction confidence is low, skip the suggestion and open the full picker (P2).

### P2 · Outcome-first picker with ASCII graphs (when picker IS needed)

When the user needs to compare, reframe options around the resulting commit graph, not the verb. ASCII art renders everywhere.

```
How should the histories combine?  your branch · feat/session-refresh

  [a]  Merge           ●─●─● main                              keeps both histories
                            ╲   ╲
                             ●───◆ (merge commit)

  [b]  Rebase          ●─●─●─●─●   main                       linear · your work on top
                                    your commit, replayed

  [c]  Force-push      ●─●─●   main     ⚠⚠                    rewrites origin
                            ✗ remote commit deleted

  [d]  Discard mine    ●─●─●   origin/main   ⚠⚠               takes remote as-is
                            ✗ your local commit deleted

[Tab] preview files affected  ·  [Enter] pick  ·  [?] explain a choice
▸ _
```

- Direct-key shortcuts `[a][b][c][d]` — single keystroke, screen-reader friendly
- `Tab` previews without committing; `Enter` commits. **Never auto-commit on hover.**
- Severity rendered as deletion in the graph (`✗ remote commit deleted`) — more visceral than `⚠⚠` alone

### P3 · Inline expand (accordion) for rich descriptions

```
? Pick a resolution strategy

  ▶ Merge          creates a merge commit
  ▼ Rebase         replays your work on top
       ─────────────────────────────────────────────────────────
       Runs:        git pull --rebase origin main
       Affects:     README.md (3 lines conflict)
       Result:      linear history · your commit becomes a3f9c12'
       If it goes wrong:  gitrust undo restores to before pull
                          (snapshot taken automatically)
       [Enter] do this    [↓] next option
  ▶ Force-push     overwrites remote ⚠⚠
  ▶ Discard mine   loses your commit ⚠⚠

[↑↓ navigate · space/→ expand · enter to do · q to quit]
```

Detail block has a fixed shape per option: **Runs / Affects / Result / If it goes wrong**.

### P4 · Wizard mode (opt-in for beginners)

```
⚠ Pull will conflict.  Two questions to figure out the right move:

  1. Is anyone else committing to main right now?
        [y] yes, others are working here
        [n] no, it's just me           (default · based on last 30d of commits)
        ▸ n

  2. Do you want the merge to show up as a separate event in history?
        [y] yes, preserve it as a merge commit
        [n] no, keep history linear     (default · this repo prefers linear)
        ▸ n

  → Rebase fits.  [Enter] do it  ·  [?] show me the other options anyway
```

- Two questions max. Each has a smart default GiTrust pre-fills.
- Questions are about **intent**, not implementation
- Opt-in via `gitrust config verbosity beginner`; off by default

### P5 · Bucketed file multi-select

Uses the existing `ClassificationResult { related, uncertain, unrelated }` from the two-pass resolver. **No new classifier work needed** — just surface what's there.

```
$ gitrust "commit my login changes"

Select files to stage  ·  matched on "login"  ·  4 of 12 changed files selected

  ▼ Related (3 · pre-checked)                                [1] toggle group
       [x]  src/auth/login.rs            +87 −12   named in query
       [x]  src/auth/session.rs          +new      imports login::*
       [x]  tests/login_test.rs          +34       covers login.rs

  ▼ Uncertain (2 · visible, not pre-checked)                  [2]
       [x]  src/db/users.rs              +4 −2    touched in same edit session
       [ ]  README.md                    +12 −3   mentions "login" in changelog

  ▶ Unrelated (7 · hidden · [a] to show)

  Total:  4 files  ·  +125 −12  ·  about 1 commit's worth

[space] toggle file  [1-2] toggle group  [a] show skipped  [n] none  [Enter] stage
```

**Implementation notes:**
- Lift per-file reason (currently `--show-all`-only) into the default view as a dim trailing note
- Promote `--show-all` from flag → inline `[a] show skipped` keystroke
- Toggle a whole bucket with one key (`[1]`, `[2]`)
- Unrelated collapsed by default; `[a]` reveals
- Running total updates live as user toggles

---

## Eight model-aware patterns

These exist *because* of qwen2.5:3b's hard limits (60% fast-path bypass, 92% intent accuracy on the 40% that hit LLM, no streaming, 400ms–2s latency, output drift past ~300 tokens).

### M1 · Engine indicator

Single glyph per response shows which engine answered:

| Glyph | Engine | Implication |
|-------|--------|-------------|
| `⚡` | Fast-path (canonical phrase, ~25 ops) | Instant, 100% reliable |
| `◉` | LLM (qwen2.5:3b) | 400ms–2s, ~92% accurate |

```
$ gitrust "push"
  ⚡ pushed origin/main · 0de4a0d → 1a3f4b8       (fast-path · 12ms)

$ gitrust "ship my login work to prod"
  ◉ understanding…                                (llm · ~1s)
```

Builds power-user behavior naturally — users learn which phrasings hit fast-path.

### M2 · Stage labels (replaces token-streaming)

Ollama's `format: json` mode doesn't stream, so surface **stages** of the pipeline with their typical latency budgets.

```
$ gitrust "ship my login work to prod"
  ⠋ understanding intent…       ~1s        [spinner, current stage]
  ✓ matched intent: push                    [done; shows actual time]
  ⠋ matching files (12 changed)  ~700ms
  ✓ 3 files selected
  ⠋ rendering plan
```

If a stage exceeds its budget by 2×, dim the spinner and offer `[a] keep waiting · [b] cancel · [c] try simpler phrasing`.

The 60% of queries that hit fast-path get NO spinner — just the `⚡` glyph confirming "instant."

### M3 · NeedsClarification (3% of queries)

Currently surfaced as an editable prompt. Upgrade: also offer GiTrust's top 2-3 guesses as one-key picks.

```
$ gitrust "the auth thing"
  ◉ not sure what you want. My best guesses:
    [a]  commit your auth changes        5 modified files in src/auth/
    [b]  switch to branch feat/oauth     last touched 12m ago
    [c]  show recent auth commits

  ▸ or refine: the auth thing _
```

### M4 · AmbiguousFeature (5% of queries)

Reuse the P5 bucketed picker — items are matched features instead of files. The type prefix (`files`, `branch`, `recent`) tells the user what kind of match it is.

```
$ gitrust "commit the auth thing"
  ◉ 3 things match "auth" in this repo:
    [1]  files  ·  src/auth/* + tests/auth_test.rs    5 files, +120
    [2]  branch ·  feat/oauth-refresh                  2 ahead
    [3]  recent ·  auth_middleware.rs                  you, 12m ago

  [Enter] pick highlighted  ·  [r] refine query
```

### M5 · TooManyFiles gate (> 50 modified)

Don't flat-refuse — narrow.

```
$ gitrust "commit everything"
  ⚠ 87 modified files — too many to reason about safely.

  Narrow by:
    [1]  recent edits (last hour)    32 files
    [2]  current directory            14 files
    [3]  type a hint to filter        e.g. "login", "tests"
    [a]  proceed anyway · group by dir  ⚠
    [q]  cancel
```

### M6 · Closed operation set (trust narrative)

Because the LLM picks from a fixed ~25-op enum (not free text), there is no "the AI made up a destructive command" failure mode. Say this out loud in `gitrust help` or first-run copy:

> GiTrust always picks from a known list of ~25 git operations. It will never invent a command.

Differentiates GiTrust from cloud LLM CLI wrappers where this fear is legitimate.

### M7 · Reasoning lines are Rust, not LLM

Any "why" text in the UI (e.g. "this repo uses rebase 18/20 times; you're solo on this branch") is **computed in Rust over real git data** — NEVER LLM-generated. Safety-critical text (warnings, severity tier reasons, recovery advice) is never the model's job.

Surface this trust in copy somewhere:

> GiTrust's reasoning lines are computed from your actual repo — the AI never writes warnings.

### M8 · Corrections grow the fast-path

When the user redirects a misclassification (the `[w]rong?` flow below), do NOT try to fine-tune qwen. Write the correction to a per-repo aliases file consulted **before** the LLM:

```toml
# .git/gitrust/aliases.toml
[aliases]
"changes today"        = { op = "log", since = "1d" }
"my work-in-progress"  = { op = "diff", scope = "uncommitted" }
"ship to staging"      = { op = "push", target = "staging" }

# Aliases are checked before the LLM. Edit by hand or via:
#   gitrust alias "<phrasing>" → <op>
```

Three corrections on the same phrasing make it a 100%-reliable instant response. File is human-readable + diffable; teams can commit it to share dialect.

---

## Universal keymap conventions

These apply to every picker/prompt GiTrust shows:

| Key | Meaning |
|-----|---------|
| `Enter` | Pick the highlighted / suggested option |
| `Esc` / `q` | Quit / cancel — **never** as a list item |
| `Tab` / `space` | Preview without committing (where applicable) |
| `?` | Show explanation/help for the current screen |
| `[a]`, `[b]`, ... | Direct-key shortcuts (preferred over arrow keys) |
| `↑↓` | Move focus when there's a focus concept |
| `Ctrl-C` | Always aborts; mid-execution it's safe (snapshot already taken) |

**Three universal rules:**
1. Direct-key shortcuts (`[a][b][c]`) — NOT just arrow keys
2. `Tab`/`space` to preview, `Enter` to commit — NEVER auto-commit on hover
3. The escape hatch is always `q` or `Esc` — **never** as a list item

---

## ANSI color + glyph spec

### Severity tier markers

| Tier | Marker | ANSI fallback | Hex (in HTML mock) | When |
|------|--------|---------------|---------------------|------|
| Safe | (none) | (none) | — | `status`, `log`, `diff` |
| Caution | `⚠` (yellow) | `!` | `#e3b341` | `commit`, `push`, `stash`, `branch` |
| Destructive | `⚠⚠` (red) | `!!` | `#ff7b72` | `reset --hard`, `discard local`, branch delete |
| Irreversible | `⛔` (bold red) | `XX` | `#ff7b72` bold | force-push to shared branch, repo delete |

Detection: if `TERM` is unset, dumb, or `gitrust --no-unicode`, fall back to ASCII column.

### Colors (semantic)

| Role | Hex (HTML) | ANSI 256 | Use |
|------|------------|----------|-----|
| primary fg | `#e8eaed` | default | body text |
| dim | `#6e7681` | 244 | secondary, captions, debug |
| dim-2 | `#4a525c` | 240 | placeholders, struck-through |
| green | `#56d364` | 78 | success, additions, "done" |
| yellow / caution | `#e3b341` | 178 | warnings, ahead/behind counts |
| orange / caution-light | `#ff9d4a` | 215 | mid-tier warnings |
| red | `#ff7b72` | 203 | destructive markers, failures |
| user-2 (cyan) | `#a5d6ff` | 117 | user input echo, command names |
| magenta | `#d2a8ff` | 183 | scoped values (commit message strings, branch names in flow) |
| accent yellow | `#ffd24a` | 220 | keymap key labels, suggestion highlights |

### Glyphs

| Glyph | Use | ASCII fallback |
|-------|-----|----------------|
| `●` | timeline node | `*` |
| `○` | pending node | `o` |
| `⠋ ⠙ ⠹ ⠸` | spinner frames | `\|/-\` |
| `✓` | success | `OK` |
| `✗` | failure | `X` |
| `↑` `↓` | ahead/behind | `^` `v` |
| `→` | flow / next | `->` |
| `↳` | quiet breadcrumb | `>` |
| `⚡` | fast-path engine | `*` |
| `◉` | LLM engine | `@` |
| `▶` `▼` | accordion collapsed/expanded | `>` `v` |
| `⚠` `⛔` | severity (see above) | `!` `XX` |

---

## State machines for the four "designed-around" failure modes

### NeedsClarification

```
classifier returns NeedsClarification
  ↓
generate top 3 candidate intents (Rust-side: scan recent ops + file matches)
  ↓
render:
  - quoted original query
  - 3 candidates as [a]/[b]/[c] with one-line evidence per
  - editable refinement field below
  ↓
on [a/b/c]: dispatch chosen intent, continue
on text input + Enter: re-run classifier with new query
on q/Esc: cancel
```

### AmbiguousFeature

```
resolver returns AmbiguousFeature { matches: Vec<Match> }
  ↓
render as P5-style picker with type prefix on each row
  ↓
on [1-N]: dispatch with chosen match
on [r]: open refinement field
on q/Esc: cancel
```

### TooManyFiles (>50 modified)

```
status returns > 50 modified files
  ↓
DO NOT call LLM (model truncates silently past 50)
  ↓
render narrowing menu:
  [1] recent edits (last hour) — runs `git status` with `--porcelain` + mtime filter
  [2] current directory — restricts scope to cwd
  [3] type a hint — opens text input, re-runs classifier with hint-filtered list
  [a] proceed anyway — partition by top-level directory, batch as N separate plans
  [q] cancel
```

### Misclassification recovery

```
LLM returns intent I; pipeline runs
  ↓
after execution, append:
  ↳ ran <op>    [w]rong? · [d]iff instead · [s]tatus instead · ...
                            ^ GiTrust's top 2-3 alternative intents
  ↓
if user presses [w]:
  prompt: "what should I have done?"
  show full op list (P3 accordion of all ~25 ops)
on selection of new intent:
  - run that intent (NOT auto-undo — user might want to keep both)
  - write the original query → new intent mapping into .git/gitrust/aliases.toml
  - notify: "→ '<query>' now mapped to <op>. Next time it'll be instant."
on [d]/[s]/etc (one of the alternatives):
  - run that intent
  - record the mapping (same as above)
```

### Failed-step recovery (mid-pipeline)

See `prototype.jsx` — the reducer's `step-failed` → `phase: failed` transition shows the exact state-machine. Keymap:

```
on step failure:
  - mark step's node red with ✗
  - render failure block inline:
      ⚠ Step N failed · <verb>
      <reason>
      [u] undo this run · revert to <snapshot-sha>
      [a] abandon, keep what succeeded
      [q] quit
  - footer reminder: "Snapshot at <sha> means undo is non-destructive."
on [u]: reset to snapshot, return to idle
on [a]: stop, leave intermediate state as-is (e.g. commit made but push failed)
on [q]/Esc: same as [a] for now
```

---

## Copy library (string constants)

Group these in a `copy.rs` or similar — single source of truth.

```rust
// Engine indicators
const ENGINE_FAST: &str = "⚡";
const ENGINE_LLM: &str = "◉";

// Stage labels
const STAGE_INTENT: &str = "understanding intent";
const STAGE_FILES: &str = "matching files";
const STAGE_DRAFT_MSG: &str = "drafting message";

// Stage budgets (ms)
const BUDGET_INTENT: u64 = 1000;
const BUDGET_FILES: u64 = 700;
const BUDGET_DRAFT_MSG: u64 = 500;

// Recovery
const SNAPSHOT_PROMISE: &str = "gitrust undo → restores all of the above for 30 days";
const SNAPSHOT_TAKEN: &str = "Snapshot saved · Working tree was never touched.";

// Conflict
const CONFLICT_HEADER: &str = "Pull will conflict";   // NOT "predicted"
const CONFLICT_HELP_HINT: &str = "[?] explain conflict markers   [why] why these 4   [q] quit";

// Plan
const PLAN_HEADER: &str = "Plan";   // NOT a boxed title
const PLAN_DONE_FOOTER: &str = "Done · {n_commits} commit(s) on {branch}, ahead of origin by {ahead}";
const PLAN_NEXT_HINT: &str = r#"Next: gitrust "status" · undo with gitrust undo"#;

// Status (status command output)
const FETCH_AGE_HINT: &str = "Fetched: {n} {unit} ago  ·  [r] refresh";
const DIVERGED_HEADER: &str = "⚠ Diverged from {upstream} — you and someone else both moved on.";

// Detached HEAD — never lead with "detached HEAD"
const DETACHED_HEADER: &str = "⚠ You're not on a branch right now.";
const DETACHED_RESCUE_OPT: &str = "Save my current spot as a new branch";

// Branch name sanitization
const BRANCH_INVALID_INTRO: &str = "Git won't allow {chars} in branch names.";
// NOT "Error: invalid branch name"

// First-run / help
const TRUST_NARRATIVE: &str = "GiTrust always picks from a known list of ~25 git operations. It will never invent a command.";
const RUST_REASONING: &str = "GiTrust's reasoning lines are computed from your actual repo — the AI never writes warnings.";

// Resolution suggestions — these phrases are sniffable into Rust;
// repo-merge-history heuristic generates "this repo uses {strategy} {x}/{y} times"
const SUGGEST_FMT: &str = "Suggested · {strategy} — {short_rationale}";
```

---

## Suggested Rust crates

| Need | Crate | Note |
|------|-------|------|
| Interactive picker (with arrow keys, type-to-filter) | `inquire` | Already used. Pin starting cursor to 0. |
| Spinner + multi-line progress | `indicatif` | For stage labels (M2) |
| ANSI color + glyph fallback detection | `console` | Has `Term::stdout().features()` for unicode detection |
| TOML for aliases.toml | `toml` + `serde` | Standard |
| Parsing git output / repo state | `git2` | Already used |
| `--json` output mode (M-section, scriptability) | `serde_json` | For piped/CI use |
| TTY detection | `atty` (or `IsTerminal` in std 1.70+) | Switch to non-color, no-picker mode when piped |

---

## Files in this bundle

| File | What it is |
|------|------------|
| `README.md` | This document — comprehensive implementation guide |
| `GiTrust UX Critique.html` | Full critique doc — visual reference for every pattern, with before/after terminal mockups and rationale per change |
| `GiTrust Plan Prototype.html` | Interactive prototype — opens in a browser, three keymap variants for the plan-execution flow with real keyboard input |
| `prototype.jsx` | React source for the prototype — the reducer is the spec for state transitions to implement in Rust |

**How to use them together:**

1. Read this README front-to-back once (you need the context).
2. Open `GiTrust UX Critique.html` in a browser — keep it open while implementing. Each "Flow rewrite" section in the critique has full before/after terminal mockups; visual reference is much faster than re-reading copy from this README.
3. Open `GiTrust Plan Prototype.html` and walk through each keymap variant with the keyboard. The recommendation is **Variant C** (Hybrid: Enter runs all, ↑↓ focus, e edits, s skips). Variant A and B are there for comparison.
4. Use `prototype.jsx`'s reducer (lines for `step-running`, `step-done`, `step-failed`, etc.) as the spec for the plan-execution state machine in Rust.

---

## Open questions for the implementing dev

These were left open at handoff time — decide based on codebase context:

1. **Variant A vs B vs C** for the plan-execution keymap — recommendation is C (Hybrid) but worth a team poll after using all three for a day.
2. **`Tab`-to-save** on the commit message edit modal — currently `⌘/Ctrl+Enter` saves. If team prefers Tab-out semantics, the change is one line.
3. **`r` retry on failed-step recovery** — currently only `u` undo / `a` abandon / `q` quit. Retry-with-fresh-attempt could be useful for transient failures (network, hook race conditions).
4. **The wizard (P4) toggle** — what triggers it? Config setting? First-N-uses auto-on? Explicit `gitrust help me`?
5. **Severity-by-context implementation tier** — start free (branch-name match), then add `~50ms` author-walk for `has_foreign_commits()`, gate `git ls-remote` behind force-push confirm only. Skip GitHub branch protection rules (host-specific).
