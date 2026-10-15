# GiTrust — Publish & release checklist

State as of **v0.4.1 (2026-10-15)**. The July version of this file planned the first public
release; most of it is done, and what remains is listed under "Open".

## Done

- [x] **Name: GiTrust** (`gitrust` for the crate, binary, config paths and repo; `GiTrust` in prose).
  Chosen 2026-07 because `glit` collided with a published OSINT tool.
- [x] Public repo: [github.com/Aniketdalvi385/gitrust](https://github.com/Aniketdalvi385/gitrust),
  MIT license, issues on.
- [x] Releases with notes: v0.3.0 (2026-08-11, the qwen era), **v0.4.0** (Kev, the fine-tune, the
  UX pass) and **v0.4.1** (latest). Tags `vX.Y.Z` match `version` in `gitrust/Cargo.toml`.
- [x] Model published: [`AniketDalvi385/gitrust-kev-0.8b`](https://huggingface.co/AniketDalvi385/gitrust-kev-0.8b),
  tag `v2`, Apache-2.0, card from [`model-card/gitrust-kev-0.8b.md`](model-card/gitrust-kev-0.8b.md),
  with `kev_ops.json` alongside. It is GiTrust's default `kev_run`.
- [x] README, `PROJECT_HISTORY.md`, `gitrust/CLAUDE.md` describe v0.4.

## Cutting a release (the steps v0.4.0/v0.4.1 used)

1. `cargo test` green; for anything a user sees, re-run `docs/ux/ux_drive.py` and read the screens.
2. Bump `version` in `gitrust/Cargo.toml`, `cargo build --release`, check `gitrust --version`.
3. Commit, `git tag -a vX.Y.Z -m "…"`, push the branch and the tag.
4. `gh release create vX.Y.Z --notes-file <notes> --latest` — a tag alone does not show up as a
   release on GitHub.
5. If the model changed: `kev.publish --run <run> --repo AniketDalvi385/gitrust-kev-0.8b --card
   docs/model-card/gitrust-kev-0.8b.md --tag vN` from the Kev checkout, upload
   `gitrust/src/kev_ops.json` if the options changed, then serve `<repo>@vN` from a clean cache and
   re-run `docs/classifier_eval/run_kev_eval_v2.py` before changing `config::default_kev_run`.

## Open

- [ ] **CI** — no `.github/workflows` yet. `cargo fmt --check`, `cargo clippy`, `cargo test` on
  ubuntu + macOS; the tests need no model server (Kev is mocked).
- [ ] **Clippy** — 37 warnings (mostly dead code left from the qwen era). Burn down, then
  `-D warnings` in CI.
- [ ] **Binaries** — releases carry no binaries. `cargo-dist` (`dist init`) or
  taiki-e/upload-rust-binary-action on tag push. Whatever installs the binary must also say that
  `gitrust setup` (uv + Kev + a ~1.5 GB model download) is the second step.
- [ ] **Homebrew** — [`Formula/gitrust.rb`](../Formula/gitrust.rb) is a template: `YOUR_ORG`, placeholder
  sha256s and download URLs for binaries that don't exist yet. Needs the binaries first.
- [ ] **crates.io** — `readme = "README.md"` in Cargo.toml points at `gitrust/README.md`, which
  doesn't exist (the README is at the repo root); fix before `cargo publish --dry-run`.
- [ ] **Demo GIF** in the README — "find my lost work" or a scoped push, recorded with `vhs`.
- [ ] **Problem B** — file scoping has only a 16-file smoke test; a realistic multi-feature diff
  is untested (see `kev_migration_research.md` §14).
