use serde::{Deserialize, Serialize};

// =============================================================================
// Repository situation (Layer 0) — the deterministic state machine that gates
// which intents are valid (scenario map §12/§13, P1). Computed by
// `context::detect_situation`; enforced by `validation::situation_gate`.
// No serde (matches `RecoveryAction`) — `git2::Oid` has no serde impl and this
// is runtime-only state.
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSituation {
    Normal,
    EmptyRepo,
    /// Not inside a git repository. Determined before opening a repo, so
    /// `detect_situation` never returns this — the not-a-repo branch in
    /// `agent::run_agent` handles it (and only allows `init`).
    NotARepo,
    /// Detached HEAD in an otherwise-clean repo. `stranded_commits` counts
    /// commits reachable from HEAD but from no local branch (S20/S21).
    DetachedHead {
        stranded_commits: u32,
    },
    /// Mid-merge: `MERGE_HEAD` present. `resolved` = index has no conflicts
    /// (the merge-aware commit from Phase 9.2 is the way out). S16/S17.
    MidMerge {
        conflicts: Vec<String>,
        resolved: bool,
    },
    /// Mid-rebase (S12/S18). `current`/`total` from the rebase state dir.
    MidRebase {
        current: u32,
        total: u32,
        conflicted: Vec<String>,
    },
    MidCherryPick {
        conflicted: Vec<String>,
    }, // S19
    MidRevert {
        conflicted: Vec<String>,
    },
    MidBisect, // S23 — read-only + refer to `git bisect`
    /// Local and upstream have both advanced (ahead > 0 AND behind > 0). S9.
    Diverged {
        ahead: u32,
        behind: u32,
    },
    /// The tracking ref's history was rewritten under us (old tip is not an
    /// ancestor of the new tip). S14.
    RemoteRewritten {
        old_tip: git2::Oid,
        new_tip: git2::Oid,
    },
    /// A stale `.git/index.lock` (older than the threshold) is blocking writes. S22.
    StaleLock {
        age_secs: u64,
    },
}

// =============================================================================
// Intent types — what the LLM returns (Layer 1 output)
// =============================================================================

/// Top-level intent classification from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IntentType {
    ReadOnly(ReadOnlyOp),
    ScopedWrite(ScopedWriteOp),
    FeaturePipeline(FeaturePipelineOp),
    NeedsClarification(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadOnlyOp {
    Status,
    Log {
        count: u8,
    },
    Diff {
        path: Option<String>,
    },
    Remotes,
    Branches,
    Config,
    /// Show full metadata for a single commit. `target` defaults to HEAD when None.
    Show {
        target: Option<String>,
    },
    /// Run `git fetch` against a remote, then show fresh status. Network operation.
    /// Used for "am I up to date" / "check remote" queries — answers them honestly
    /// instead of returning stale ahead/behind based on the last fetch.
    FetchStatus {
        remote: String,
    },
    /// Fetch + simulate a 3-way merge in memory. Tells the user whether a pull
    /// would conflict, BEFORE they pull. Network operation, but never modifies
    /// the working tree.
    MergePreview {
        remote: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScopedWriteOp {
    Add {
        files: FileSelection,
    },
    Commit {
        message: CommitMessage,
    },
    Branch {
        name: String,
    },
    Checkout {
        target: String,
    },
    Stash,
    StashPop,
    Reset {
        mode: ResetMode,
    },
    Pull {
        remote: String,
    },
    Init,
    DeleteRepo,
    AddRemote {
        name: String,
        url: String,
    },
    RemoveRemote {
        name: String,
    },
    /// "commit all my files" / "commit the readme" — stage then commit in one step.
    /// `files` controls what gets staged: All = everything, Explicit = named files.
    AddAndCommit {
        message: CommitMessage,
        files: FileSelection,
    },
    /// "publish current branch" / "push" — push existing commits without staging.
    /// `set_upstream` is decided at build time based on whether the branch is tracking.
    Push {
        remote: String,
    },
    /// "undo last commit" — soft-reset HEAD to its parent, files stay staged.
    /// This is the concrete soft-reset primitive the undo ChoiceGate resolves
    /// to for the "keep staged" choice. The classifier NEVER emits it directly;
    /// all undo-language classifies to `UndoIntent` (P6 — the LLM recognizes
    /// undo, the deterministic gate decides the semantic).
    Undo,
    /// Ambiguous undo-language (S34/P6). The classifier's ONLY undo target. A
    /// deterministic ChoiceGate (`build_undo_intent`) resolves it into one of
    /// {Undo(soft) / Reset Mixed / Revert / Reset Hard}. No undo phrase reaches
    /// a destructive mode without the gate having offered every option.
    UndoIntent,
    /// "find my lost work" / "I lost a commit" / "recover" (Phase 14, S24/26/27).
    /// Runs the forensic scan → RecoveryBrowser → restore-to-rescue-branch. It
    /// only CREATES a ref (never touches current HEAD/index/worktree), so it is
    /// allowed in ANY situation — the situation gate treats it as read-only
    /// against current state. Distinct from `gitrust --recover` (the snapshot path).
    ForensicRecover,
    /// Ambiguous destructive-cleanup language (Phase 14, S62 / P6): "clean up",
    /// "start fresh", "throw away my changes". The classifier's ONLY cleanup
    /// target — like `UndoIntent`, a deterministic ChoiceGate spells out what
    /// would be deleted (tracked vs untracked, with counts) before any
    /// `Clean`/`Reset::Hard` is constructed. The LLM never picks the semantic.
    CleanupIntent,
    /// "amend"/"fix my last commit message"/"add forgotten file" (S32). `message`
    /// = new message when the user gave one, else keep the existing (`--no-edit`).
    /// `include_staged` folds currently-staged changes into the commit.
    Amend {
        message: Option<String>,
        include_staged: bool,
    },
    /// "revert that commit" (S38) — the default *public* undo: a history-
    /// preserving inverse commit, team-safe, no force. `oid` resolves to HEAD at
    /// build time in v1 (empty = HEAD).
    Revert {
        oid: String,
    },
    /// "unstage" / "unstage <file>" (S39) — `restore --staged` semantics, scoped
    /// to paths, NEVER a reset mode. Empty here means "all staged"; the concrete
    /// staged-path list is enumerated at build time (nothing ambiguous reaches
    /// the executor).
    Unstage {
        paths: Vec<String>,
    },
    /// "set upstream" / "track origin/<branch>" — configure the branch's upstream.
    SetUpstream {
        remote: String,
        branch: String,
    },
    /// "squash my last N commits" (S40) — soft reset to HEAD~N + one commit. No
    /// interactive rebase; preserves the working tree and the resulting tree oid.
    SquashLastN {
        n: u32,
    },
    /// `git pull --rebase` — replays local commits on top of remote.
    PullRebase {
        remote: String,
    },
    /// `git push --force-with-lease` — overwrites the remote branch
    /// (safe variant: refuses if remote moved since last fetch).
    ForcePush {
        remote: String,
    },
    /// `git fetch && git reset --hard origin/<branch>` — discards local commits
    /// in favor of the remote. Highly destructive.
    DiscardLocal {
        remote: String,
    },
    // -------------------------------------------------------------------------
    // In-progress operation backout/continue steps (Phase 11 — S16/S18/S19).
    // Field-less: they act on ambient repo state (rebase-merge/MERGE_HEAD/…).
    // Situation-gated — each is only Allowed in its matching Mid* situation.
    // Aborts/skips restore or drop pre-op state (git's ORIG_HEAD is the safety
    // net); continues require a conflict-free index (enforced by the gate).
    // -------------------------------------------------------------------------
    /// `git merge --abort` — back out of a mid-merge, restoring pre-merge state.
    MergeAbort,
    /// `git rebase --continue` — proceed once conflicts are resolved and staged.
    RebaseContinue,
    /// `git rebase --skip` — drop the current conflicted commit and continue.
    RebaseSkip,
    /// `git rebase --abort` — back out, restoring the original HEAD.
    RebaseAbort,
    /// `git cherry-pick --continue` — proceed once conflicts are resolved.
    CherryPickContinue,
    /// `git cherry-pick --abort` — back out of a mid-cherry-pick.
    CherryPickAbort,
    /// `git revert --continue` — proceed once conflicts are resolved.
    RevertContinue,
    /// `git revert --abort` — back out of a mid-revert.
    RevertAbort,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FeaturePipelineOp {
    /// "push login changes" — requires file resolution + commit + push
    AddCommitPush { feature_hint: String },
    /// "commit and push staged" — commit staged files then push
    CommitPush { message: CommitMessage },
    /// "sync with main" — pull from upstream branch
    Sync { strategy: SyncStrategy },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncStrategy {
    Merge,
    Rebase,
}

/// Which engine produced an `IntentResult`. Surfaced as a one-glyph indicator
/// (⚡ for canonical fast-path, ◉ for LLM) so the user learns over time which
/// phrasings hit the instant path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Fast,
    Llm,
}

/// Result of a single LLM classification call.
#[derive(Debug, Clone)]
pub struct IntentResult {
    pub intent: IntentType,
    pub confidence: f32,
    pub raw_query: String,
    pub engine: Engine,
}

// =============================================================================
// Pipeline steps — what the executor receives (Layer 4 input)
// All fields are fully resolved concrete values. No ambiguity.
// =============================================================================

/// A single executable step. By the time this reaches the executor,
/// every field is a concrete, resolved value — no `Current`, `Pattern`, or `Draft`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PipelineStep {
    Add {
        files: Vec<String>,
    },
    Commit {
        message: String,
    },
    Push {
        remote: String,
        branch: String,
        set_upstream: bool,
    },
    Pull {
        remote: String,
    },
    Branch {
        name: String,
    },
    Checkout {
        target: String,
    },
    Stash,
    StashPop,
    /// Reset HEAD to `to_oid` — the *parent* (`HEAD~1`) to DROP a commit
    /// (undo→discard / undo→keep-in-files), or `None` to reset to HEAD itself,
    /// which discards an uncommitted working-tree delta while keeping the commit
    /// (cleanup→tracked). The target is load-bearing: a target-less reset always
    /// peeled HEAD and so silently no-op'd every commit-drop (M-16/M-19).
    Reset {
        mode: ResetMode,
        to_oid: Option<String>,
    },
    Log {
        count: u8,
    },
    Status,
    Diff {
        path: Option<String>,
    },
    AddRemote {
        name: String,
        url: String,
    },
    RemoveRemote {
        name: String,
    },
    /// Soft-reset HEAD to `to_oid` (parent of the commit being undone). Files stay staged.
    /// Both `undone_summary` and `undone_sha` are captured at build time for the summary line.
    /// `to_oid` is the parent commit's full SHA (40-char hex).
    Undo {
        to_oid: String,
        undone_summary: String,
        undone_sha: String,
    },
    /// Run `git pull --rebase <remote> <branch>` as a subprocess.
    PullRebase {
        remote: String,
        branch: String,
    },
    /// Run `git push --force-with-lease[=<branch>:<oid>] <remote> <branch>` as a
    /// subprocess. When `expect_oid` is `Some`, the lease is pinned to that oid
    /// (captured at GiTrust's own fetch) so a background fetch advancing the tracking
    /// ref during a long conflict resolution can't silently satisfy the lease
    /// (TOCTOU hardening). `None` = bare `--force-with-lease` (classifier path).
    ForcePush {
        remote: String,
        branch: String,
        expect_oid: Option<String>,
    },
    /// Amend HEAD (S32). Routed through system git when commit hooks are present
    /// (executor), so it never becomes a hook bypass (9.3/S47). `message = None`
    /// keeps the existing message (`--no-edit`).
    Amend {
        message: Option<String>,
        include_staged: bool,
    },
    /// Inverse commit of `oid` (S38). Shells out to `git revert --no-edit` for
    /// sequencer-state parity + MidRevert conflict handoff. `oid` is a full SHA.
    Revert {
        oid: String,
    },
    /// Unstage the named paths (S39) — `restore --staged` semantics. `paths` is
    /// always concrete (enumerated at build time), never an empty-means-all sentinel.
    Unstage {
        paths: Vec<String>,
    },
    /// Set the branch's upstream to `remote/branch`.
    SetUpstream {
        remote: String,
        branch: String,
    },
    /// Squash the last `n` commits into one (S40) — soft reset to HEAD~n + a
    /// single commit with `message` (resolved at build time). Routes through the
    /// same commit-hook logic as `Commit`/`Amend`.
    SquashLastN {
        n: u32,
        message: String,
    },
    /// Run `git fetch <remote>` then `git reset --hard <remote>/<branch>` as a subprocess.
    DiscardLocal {
        remote: String,
        branch: String,
    },
    // -------------------------------------------------------------------------
    // In-progress operation backout/continue steps (Phase 11 — S16/S18/S19).
    // All shell out to system git (`git rebase --continue`, etc.) — the
    // pragmatic v1 the blueprint blesses for gnarly ops. Field-less: they act
    // on ambient repo state.
    // -------------------------------------------------------------------------
    MergeAbort,
    RebaseContinue,
    RebaseSkip,
    RebaseAbort,
    CherryPickContinue,
    CherryPickAbort,
    RevertContinue,
    RevertAbort,
    // -------------------------------------------------------------------------
    // Divergence reconciliation (Phase 12 — S9/S10/S13). Composed internally by
    // `agent::reconcile_flow`; NOT produced by the classifier. `fetch` runs
    // inline before the ChoiceGate (so it is a `git_ops` method, not a step).
    // -------------------------------------------------------------------------
    /// `git merge <source>` via git2. On conflict-free success it lands the
    /// merge commit (2 parents) via the merge-aware `commit()` using `message`;
    /// on conflict it exits into `MidMerge` (Phase 11 owns the rest). `message`
    /// is resolved at build time so the plan preview can show it.
    Merge {
        source: String,
        message: String,
    },
    /// `git rebase <onto>` (shell-out). On conflict it exits into `MidRebase`.
    Rebase {
        onto: String,
    },
    /// Delete untracked files (`git clean -fd`) — Phase 14 / S62. `trash_net` is
    /// always `true`: the destroyed untracked content is backed up to
    /// `~/.gitrust/trash/` first (it has no reflog — the trash-net is its only
    /// safety). Produced ONLY by the destructive-phrase ChoiceGate, never the
    /// classifier. Graded `Irreversible` (above hard reset) in `danger.rs`.
    Clean {
        trash_net: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl PipelineStep {
    /// True for the abort/skip in-progress ops (Phase 11). These run WITH a
    /// conflicted index, so they must NOT trigger snapshot capture — git2's
    /// `index.write_tree()` errors on unmerged entries — and they bypass the
    /// conflicts backstop. `Continue` is deliberately excluded: by the time it
    /// runs its index is conflict-free (enforced by the situation gate), so it
    /// snapshots and validates like any other write.
    pub fn is_abort_or_skip_backout(&self) -> bool {
        matches!(
            self,
            PipelineStep::MergeAbort
                | PipelineStep::RebaseAbort
                | PipelineStep::RebaseSkip
                | PipelineStep::CherryPickAbort
                | PipelineStep::RevertAbort
        )
    }
}

/// A pipeline consisting ENTIRELY of abort/skip backout steps. Such a pipeline
/// skips snapshot capture (conflicted index) and is exempt from the conflicts
/// backstop. Uses `all()` — NOT `any()` — so a mixed pipeline (a backout plus
/// any other step, as Phase 12 will compose) keeps both safeties. The non-empty
/// guard covers `all()`'s vacuous truth on an empty slice.
pub fn pipeline_is_pure_backout(steps: &[PipelineStep]) -> bool {
    !steps.is_empty() && steps.iter().all(PipelineStep::is_abort_or_skip_backout)
}

// =============================================================================
// File selection and commit message — pre-resolution types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileSelection {
    /// Stage everything tracked.
    All,
    /// User named specific files.
    Explicit(Vec<String>),
    /// "login page" — must be resolved via manifest before reaching executor.
    Pattern(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommitMessage {
    /// LLM-generated draft — user edits in the gate.
    Draft(String),
    /// User provided exact message — still shown for confirmation.
    Provided(String),
}

// =============================================================================
// Resolution and classification types (Layer 2 output)
// =============================================================================

/// Result of the two-pass file resolver.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    /// Files strongly related to the feature — pre-selected in gate.
    pub related: Vec<ClassifiedFile>,
    /// Files that might be related — shown but NOT pre-selected.
    pub uncertain: Vec<ClassifiedFile>,
    /// Files unrelated to the feature — hidden unless --show-all.
    pub unrelated: Vec<ClassifiedFile>,
}

#[derive(Debug, Clone)]
pub struct ClassifiedFile {
    /// Relative path within the repository.
    pub path: String,
    /// Human-readable reason shown in gate, e.g. "registers /login route in app".
    pub reason: String,
}

/// Outcome of the resolution phase.
#[derive(Debug, Clone)]
pub enum ResolutionOutcome {
    /// Files classified successfully.
    Found(ClassificationResult),
    /// No modified files in the repository.
    NothingModified,
    /// Feature hint matched zero files.
    NothingMatched {
        hint: String,
        suggestion: Option<String>,
    },
    /// More than 50 modified files — ask user to narrow scope.
    TooManyFiles { count: usize },
    /// Feature hint is ambiguous — multiple possible interpretations.
    AmbiguousFeature { options: Vec<String> },
}

// =============================================================================
// Validation types (Layer 3)
// =============================================================================

/// Result of a single pre-check.
#[derive(Debug, Clone)]
pub enum PreCheckResult {
    Ok,
    Warn(String),
    Block(String),
}

/// What type of human gate to show for a step.
#[derive(Debug, Clone)]
pub enum GateType {
    /// No confirmation needed (read-only ops).
    None,
    /// Simple Y/n prompt, default Y.
    SimpleConfirm { message: String },
    /// File multi-select grouped by confidence.
    /// When `show_all` is false (default), files classified as Unrelated are
    /// hidden from the picker — the user opts in via the `--show-all` CLI flag.
    FileMultiSelect {
        classification: ClassificationResult,
        show_all: bool,
    },
    /// Editable text field (commit messages).
    EditableText { initial: String, label: String },
    /// Type-to-confirm for destructive ops (must type branch name exactly).
    /// When `explanation` is `Some`, the gate prints a What/Risks/Recovery block
    /// above the input — the long-form beginner-friendly description.
    TypeToConfirm {
        expected: String,
        message: String,
        explanation: Option<&'static crate::explain::Explanation>,
        danger: crate::danger::DangerLevel,
    },
    /// Multi-option select for disambiguation. Each label is shown to the user.
    Choose {
        message: String,
        options: Vec<String>,
    },
    /// Strategy choice with a one-sentence consequence per option (Phase 12 —
    /// S9 rebase-vs-merge). Rendered like `Choose` but each row shows its
    /// consequence. Unlike `Choose`, `AutoConfirmGate` REFUSES this gate (never
    /// silently defaults a strategy — see confirm.rs).
    ChoiceGate {
        question: String,
        options: Vec<ChoiceOption>,
    },
    /// Forensic recovery browser (Phase 14 — S24/S26/S27). Lists lost-commit
    /// candidates; the user picks one to restore. Restore is ALWAYS
    /// non-destructive (a `rescue/*` branch at the chosen oid), so a wrong pick
    /// costs only a spare branch. `--yes` REFUSES this gate (never auto-pick a
    /// commit to restore — see `AutoConfirmGate`). Single-select in v1
    /// (`GateResponse::Chose`); multi-restore is Tier-2.
    RecoveryBrowser {
        candidates: Vec<crate::recovery::LostCommit>,
    },
}

/// One option in a `GateType::ChoiceGate`: a short `label` plus a plain-language
/// `consequence` sentence shown alongside it so the user chooses with eyes open.
#[derive(Debug, Clone)]
pub struct ChoiceOption {
    pub label: String,
    pub consequence: String,
}

/// User's response to a gate.
#[derive(Debug, Clone)]
pub enum GateResponse {
    /// User confirmed (with optional modifications).
    Confirmed,
    /// User selected specific files.
    FilesSelected(Vec<String>),
    /// User provided/edited text.
    TextProvided(String),
    /// User aborted.
    Aborted,
    /// User picked a choice — index into the original options vector.
    Chose(usize),
}

// =============================================================================
// Recovery types
// =============================================================================

/// What to do when a specific step fails.
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    /// Restore index to a known OID (undo staging).
    RestoreIndex {
        to_oid: git2::Oid,
        description: String,
    },
    /// Soft-reset HEAD to undo a commit, keeping files staged.
    SoftReset {
        to_oid: Option<git2::Oid>,
        description: String,
    },
    /// No local change needed — e.g., push failed but commit is safe.
    NoLocalChange {
        description: String,
        suggestion: String,
    },
    /// Only git reflog can help (after reset --hard).
    ReflogOnly { description: String, hint: String },
}

/// Classification of push failure types.
#[derive(Debug, Clone)]
pub enum PushFailureType {
    /// Remote unreachable. Commit safe locally.
    NetworkError,
    /// Remote has commits local doesn't. Pull first.
    NonFastForward,
    /// Credential/auth failure.
    AuthFailure,
    /// Post-check couldn't confirm success.
    VerificationFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Phase 11 quantifier guard: the snapshot-skip and conflicts-backstop
    // exemption must use all()/non-empty, NOT any(). A mixed pipeline keeps both
    // safeties on.
    #[test]
    fn pure_backout_requires_all_steps_backout() {
        // A solitary abort/skip → pure backout.
        assert!(pipeline_is_pure_backout(&[PipelineStep::RebaseAbort]));
        assert!(pipeline_is_pure_backout(&[PipelineStep::MergeAbort]));
        assert!(pipeline_is_pure_backout(&[PipelineStep::RebaseSkip]));
        // Mixed pipeline (backout + anything else) → NOT pure; safeties stay on.
        assert!(!pipeline_is_pure_backout(&[
            PipelineStep::RebaseAbort,
            PipelineStep::Status,
        ]));
        // Empty pipeline → not a backout (guards all()`s vacuous truth).
        assert!(!pipeline_is_pure_backout(&[]));
        // Continue is NOT a backout step — it snapshots and validates normally.
        assert!(!pipeline_is_pure_backout(&[PipelineStep::RebaseContinue]));
        // A plain non-backout step.
        assert!(!pipeline_is_pure_backout(&[PipelineStep::Status]));
    }
}
