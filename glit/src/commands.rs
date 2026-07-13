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
    DetachedHead { stranded_commits: u32 },
    /// Mid-merge: `MERGE_HEAD` present. `resolved` = index has no conflicts
    /// (the merge-aware commit from Phase 9.2 is the way out). S16/S17.
    MidMerge { conflicts: Vec<String>, resolved: bool },
    /// Mid-rebase (S12/S18). `current`/`total` from the rebase state dir.
    MidRebase { current: u32, total: u32, conflicted: Vec<String> },
    MidCherryPick { conflicted: Vec<String> }, // S19
    MidRevert { conflicted: Vec<String> },
    MidBisect, // S23 — read-only + refer to `git bisect`
    /// Local and upstream have both advanced (ahead > 0 AND behind > 0). S9.
    Diverged { ahead: u32, behind: u32 },
    /// The tracking ref's history was rewritten under us (old tip is not an
    /// ancestor of the new tip). S14.
    RemoteRewritten { old_tip: git2::Oid, new_tip: git2::Oid },
    /// A stale `.git/index.lock` (older than the threshold) is blocking writes. S22.
    StaleLock { age_secs: u64 },
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
    Log { count: u8 },
    Diff { path: Option<String> },
    Remotes,
    Branches,
    Config,
    /// Show full metadata for a single commit. `target` defaults to HEAD when None.
    Show { target: Option<String> },
    /// Run `git fetch` against a remote, then show fresh status. Network operation.
    /// Used for "am I up to date" / "check remote" queries — answers them honestly
    /// instead of returning stale ahead/behind based on the last fetch.
    FetchStatus { remote: String },
    /// Fetch + simulate a 3-way merge in memory. Tells the user whether a pull
    /// would conflict, BEFORE they pull. Network operation, but never modifies
    /// the working tree.
    MergePreview { remote: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScopedWriteOp {
    Add { files: FileSelection },
    Commit { message: CommitMessage },
    Branch { name: String },
    Checkout { target: String },
    Stash,
    StashPop,
    Reset { mode: ResetMode },
    Pull { remote: String },
    Init,
    DeleteRepo,
    AddRemote { name: String, url: String },
    RemoveRemote { name: String },
    /// "commit all my files" / "commit the readme" — stage then commit in one step.
    /// `files` controls what gets staged: All = everything, Explicit = named files.
    AddAndCommit { message: CommitMessage, files: FileSelection },
    /// "publish current branch" / "push" — push existing commits without staging.
    /// `set_upstream` is decided at build time based on whether the branch is tracking.
    Push { remote: String },
    /// "undo last commit" — soft-reset HEAD to its parent, files stay staged.
    Undo,
    /// `git pull --rebase` — replays local commits on top of remote.
    PullRebase { remote: String },
    /// `git push --force-with-lease` — overwrites the remote branch
    /// (safe variant: refuses if remote moved since last fetch).
    ForcePush { remote: String },
    /// `git fetch && git reset --hard origin/<branch>` — discards local commits
    /// in favor of the remote. Highly destructive.
    DiscardLocal { remote: String },
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
    Add { files: Vec<String> },
    Commit { message: String },
    Push { remote: String, branch: String, set_upstream: bool },
    Pull { remote: String },
    Branch { name: String },
    Checkout { target: String },
    Stash,
    StashPop,
    Reset { mode: ResetMode },
    Log { count: u8 },
    Status,
    Diff { path: Option<String> },
    AddRemote { name: String, url: String },
    RemoveRemote { name: String },
    /// Soft-reset HEAD to `to_oid` (parent of the commit being undone). Files stay staged.
    /// Both `undone_summary` and `undone_sha` are captured at build time for the summary line.
    /// `to_oid` is the parent commit's full SHA (40-char hex).
    Undo {
        to_oid: String,
        undone_summary: String,
        undone_sha: String,
    },
    /// Run `git pull --rebase <remote> <branch>` as a subprocess.
    PullRebase { remote: String, branch: String },
    /// Run `git push --force-with-lease <remote> <branch>` as a subprocess.
    ForcePush { remote: String, branch: String },
    /// Run `git fetch <remote>` then `git reset --hard <remote>/<branch>` as a subprocess.
    DiscardLocal { remote: String, branch: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
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
    Choose { message: String, options: Vec<String> },
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
