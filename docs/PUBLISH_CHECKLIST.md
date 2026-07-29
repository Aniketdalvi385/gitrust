# GiTrust — Publish Checklist (GitHub public + package distribution)

Work top to bottom. Steps 1–4 are one session with Claude Code; 5–6 are account actions only you can do.

## 0. Naming — RESOLVED ✅

**Name: GiTrust.** Verified available 2026-07: `gitrust`, `git-trust`, `gi-trust`, `gitrust-cli` all return 404 on crates.io. GitHub has a user handle `gitrust` (unrelated hobby scripts) and a handful of zero-star "build git in Rust" student repos — no product, no crate, no mindshare to collide with. Contrast with the old name: `glit`/`glit-cli`/`glit-core` were all taken by a published OSINT security tool.

Canonical forms, never mixed: `gitrust` (crate, binary, config paths, CLI examples, repo name) · `GiTrust` (prose and docs).

- [x] Rename executed across code, docs, paths, and directories; zero-hit grep verified
- [ ] **Version bump: Cargo.toml still says 0.1.0** — set `version = "0.2.0"` (the binary currently self-IDs as 0.1.0, contradicting CLAUDE.md's "v0.2 COMPLETE"; the tag and any crates.io release depend on this)

## 0b. Commit the pending work (three separate commits)

The rename is sitting uncommitted. Keep these separate so each is reviewable as one mechanical change:

1. `chore: untrack accidental Glit-test gitlink` — the `git rm --cached` + .gitignore rule alone
2. `chore: rename glit → gitrust` — the 34 renames + string/path/doc edits
3. `chore: cargo metadata for v0.2 release` — version bump + the metadata in step 2 below

## 1. Repo hygiene (before anything is public)

- [ ] `LICENSE-MIT` + `LICENSE-APACHE` (dual, the Rust convention) and `license = "MIT OR Apache-2.0"` in Cargo.toml.
- [ ] `.gitignore` covers `gitrust/target/`, `~` editor droppings; verify `git status` is clean noise-free.
- [ ] Secrets sweep of full history: `git log -p | grep -iE "api[_-]?key|secret|token|BEGIN .*PRIVATE"` (or run gitleaks). You never committed credentials, but verify — history is forever once pushed.
- [x] `design_handoff_gitrust_ux/` renamed and its CLAUDE.md link verified. Decide only whether it ships publicly (it's good portfolio material — shows the UX spec behind the tool) or moves to a separate repo.
- [ ] Verify the gitlink fix held: `git ls-files -s | awk '$1 == "160000"'` returns nothing (a committed gitlink with no .gitmodules gives every cloner a broken empty directory — a bad first impression for a git-safety tool)
- [ ] CLAUDE.md staying public is fine (it's good portfolio signal: shows engineering discipline) — but re-read it once with stranger's eyes.

## 2. Cargo.toml metadata (goes in commit 3; crates.io requires most of these)

```toml
[package]
name = "gitrust"
version = "0.2.0"
edition = "2021"
description = "A safety layer for git: natural-language git operations with deterministic guardrails, undo protection, and lost-work recovery. Local LLM via Ollama — nothing leaves your machine."
license = "MIT OR Apache-2.0"
repository = "https://github.com/<you>/<repo>"
readme = "README.md"          # path is relative to gitrust/ — the package can't reference files above its own dir, so copy the README in or point at a gitrust/README.md
keywords = ["git", "cli", "llm", "safety", "recovery"]     # max 5
categories = ["command-line-utilities", "development-tools"]
```

- [ ] `cargo publish --dry-run` from `gitrust/` — fixes packaging issues (paths outside the package, missing readme) before the real thing.

## 3. README — the flagship page (this IS the portfolio)

Structure that sells the identity (safety layer, not agent):

1. One-line pitch + badges (CI, crates.io if applicable, license). Suggested pitch: *"A safety layer for git. Natural-language git operations with deterministic guardrails — the LLM reads your intent, it never runs your commands."* The name does work here: lead with the trust/rust double meaning once, then never explain it again.
2. **Demo GIF** — M-23 ("find my lost work") or M-06 (diverged push). Record with `vhs` (scriptable, reproducible) or asciinema. This is the highest-ROI hour of the whole launch.
3. The origin story, three sentences: the colleague at the desk.
4. **What makes it different** — the invariants, stated as facts with test counts: LLM parses intent only and never executes; every destructive op is gated, snapshotted, and trash-netted (fail-closed); state machine over 12 repo situations; 192 automated tests + a 31-case manual acceptance report (link `docs/TEST_REPORT_v0.2.md`).
5. Install (per step 5/6 decisions) + prerequisite: Ollama + `gitrust setup`.
6. Quickstart: five commands with real output.
7. The scorecard before/after table from docs/scenario_map.md — "what happens when git goes wrong, measured."
8. Architecture sketch (5 layers, link CLAUDE.md/docs for depth). Safety model section: what `--yes` can and cannot bypass.

## 4. CI (do before going public — the badge matters, but green matters more)

- [ ] `.github/workflows/ci.yml`: on push/PR → `cargo fmt --check`, `cargo clippy`, `cargo test` on ubuntu + macos. Tests are Ollama-free by design (mocked classifier) — they'll pass in CI; if any test needs the binary's runtime deps, gate it behind a feature.
- [ ] The ~49 baseline clippy warnings: either burn them down in one cleanup session now (recommended — public code, and `clippy -D warnings` in CI prevents regrowth) or run clippy non-blocking and ticket it.

## 5. GitHub publish

```bash
# from repo root, everything committed, CLAUDE.md truthful, tag exists
gh repo create <name> --public --source . --push        # or create on github.com then:
git remote add origin git@github.com:<you>/<name>.git
git push -u origin <main-branch> --tags
```
- [ ] Repo settings: description = the one-line pitch; topics: `git`, `rust`, `cli`, `llm`, `ollama`, `developer-tools`; disable wikis/projects you won't use.
- [ ] Pin the repo on your profile.
- [ ] Enable issues; add a minimal bug template asking for `git status -sb` + the gitrust command (mirrors the test-report evidence rule).

## 6. Binary distribution (non-Rust users)

- [ ] `dist init` (cargo-dist) from the workspace — generates `release.yml`; on every version tag push it builds the target matrix, creates the GitHub Release, and produces shell + PowerShell installers automatically. Actively maintained.
- [ ] Alternative if dist fights the repo layout: taiki-e/create-gh-release-action + upload-rust-binary-action (simpler, less magic).
- [ ] Caveat for the README install section either way: the binary alone isn't enough — Ollama + model required; `gitrust setup` is the second command after install, always.
- [ ] Homebrew tap / winget: defer to v0.3 unless demand shows up.

## 7. Launch order

1. Version bump + the three commits (step 0/0b)
2. `ollama create gitrust-qwen --file Modelfile` if you use the local-GGUF path
3. **Manual test suite → `docs/TEST_REPORT_v0.2.md`** (fix or file anything red)
4. Steps 1–4 of this checklist (hygiene, metadata, README, CI)
5. Step 5: push public, tag `v0.2.0`
6. Step 6: binaries via cargo-dist
7. Demo GIF (M-23 or M-06) into the README
8. Then — and only then — share it (r/rust, X, Show HN). The post writes itself from README sections 3–4.

**Do not skip 3.** The report is both the last private bug-catch and the README's strongest credential: *"192 automated tests plus a 31-case manual acceptance report"* is a sentence almost no solo project can write.
