//! Cross-cutting interaction plumbing: the one way a run ends when the user
//! backs out, and small terminal helpers shared by the gates.
//!
//! ## Cancel semantics (deliberate)
//! Esc / Ctrl-C at any prompt, `q` at the plan, or choosing a "do nothing"
//! option all end the run the same way: `Err(Canceled)`. main() prints one calm
//! line (no `✗`, no "caused by" chain) and exits **1** — nothing was done, and a
//! not-done operation must never read as success to a script (the same rule as
//! refusals and pauses). The REPL prints the line and keeps going.

use std::fmt;

/// The user backed out; nothing was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(crate::copy::CANCELED)
    }
}

impl std::error::Error for Canceled {}

/// `Err(Canceled)` for any return type — the single exit for a backed-out run.
pub fn canceled<T>() -> anyhow::Result<T> {
    Err(Canceled.into())
}

/// True when `e` (or anything in its chain) is a user cancel.
pub fn is_canceled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Canceled>().is_some())
}

/// Map an inquire result: a cancel/interrupt becomes `Canceled`; other prompt
/// failures keep their context.
pub fn prompt_result<T>(r: Result<T, inquire::InquireError>, what: &str) -> anyhow::Result<T> {
    match r {
        Ok(v) => Ok(v),
        Err(inquire::InquireError::OperationCanceled)
        | Err(inquire::InquireError::OperationInterrupted) => canceled(),
        Err(e) => Err(anyhow::Error::new(e).context(format!("{} failed", what))),
    }
}

// =============================================================================
// The plan's one-key prompt
// =============================================================================

/// What a key does at "run this plan?": run, cancel, edit the commit message
/// (`e`, only when the plan commits) or drop a trailing push (`s`, only when
/// the plan ends in one); `None` = ignore (a stray key never runs a plan).
/// Matches the printed legend, `copy::plan_legend`.
pub fn plan_key(
    code: crossterm::event::KeyCode,
    ctrl: bool,
    can_edit_message: bool,
    can_skip_push: bool,
) -> Option<crate::commands::GateResponse> {
    use crate::commands::GateResponse;
    use crossterm::event::KeyCode as K;
    match code {
        K::Char('c') | K::Char('d') if ctrl => Some(GateResponse::Aborted),
        K::Enter | K::Char('y') | K::Char('Y') => Some(GateResponse::Confirmed),
        K::Char('q') | K::Char('Q') | K::Char('n') | K::Char('N') | K::Esc => {
            Some(GateResponse::Aborted)
        }
        K::Char('e') | K::Char('E') if can_edit_message => Some(GateResponse::PlanEditMessage),
        K::Char('s') | K::Char('S') if can_skip_push => Some(GateResponse::PlanSkipPush),
        _ => None,
    }
}

/// Restores cooked mode however the read ends (error, panic, return).
struct RawMode;
impl RawMode {
    fn on() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawMode)
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Wait for one decisive key at the plan prompt. Not a terminal (piped stdin
/// without `--yes`) → a plain y/n line read, so scripts still work.
pub fn run_plan_keypress(
    can_edit_message: bool,
    can_skip_push: bool,
) -> anyhow::Result<crate::commands::GateResponse> {
    use crate::commands::GateResponse;
    use crossterm::event::{read, Event, KeyEventKind, KeyModifiers};
    use std::io::{IsTerminal, Write};

    let legend = crate::copy::plan_legend(can_edit_message, can_skip_push);
    if !std::io::stdin().is_terminal() {
        print!("  {} [y/N] ", crate::copy::PLAN_KEYMAP_LEGEND);
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(if matches!(line.trim(), "y" | "Y" | "yes") {
            GateResponse::Confirmed
        } else {
            GateResponse::Aborted
        });
    }
    print!("  {}  ", crate::copy::style_keys(&legend));
    std::io::stdout().flush().ok();
    let response = {
        let _raw = RawMode::on()?;
        loop {
            if let Event::Key(k) = read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(r) = plan_key(
                    k.code,
                    k.modifiers.contains(KeyModifiers::CONTROL),
                    can_edit_message,
                    can_skip_push,
                ) {
                    break r;
                }
            }
        }
    };
    println!();
    Ok(response)
}

// =============================================================================
// Interpretation echo — what GiTrust understood, before it acts
// =============================================================================

/// A short, user-facing name for an intent ("Show history", "Push only the
/// “login” changes"). `None` for a refusal, which says everything itself.
pub fn intent_title(intent: &crate::commands::IntentType) -> Option<String> {
    use crate::commands::{
        FeaturePipelineOp as F, IntentType as I, ReadOnlyOp as R, ScopedWriteOp as W,
    };
    let q = |s: &str| format!("“{}”", s);
    Some(match intent {
        I::NeedsClarification(_) => return None,
        I::ReadOnly(op) => match op {
            R::Status => "Show status".into(),
            R::Log { .. } => "Show history".into(),
            R::Diff { path: Some(p) } => format!("Show changes in {}", q(p)),
            R::Diff { path: None } => "Show changes".into(),
            R::Remotes => "List remotes".into(),
            R::Branches => "List branches".into(),
            R::Config => "Show Git settings".into(),
            R::Show { target: Some(t) } => format!("Show commit {}", q(t)),
            R::Show { target: None } => "Show the last commit".into(),
            R::FetchStatus { remote } => format!("Check {}", remote),
            R::MergePreview { remote } => format!("Preview a pull from {}", remote),
        },
        I::ScopedWrite(op) => match op {
            W::Add { .. } => "Stage files".into(),
            W::Commit { .. } => "Commit staged files".into(),
            W::Branch { name } if name.trim().is_empty() => "List branches".into(),
            W::Branch { name } => format!("Create branch {}", q(name)),
            W::Checkout { target } => format!("Switch to {}", q(target)),
            W::Stash => "Stash changes".into(),
            W::StashPop => "Restore stashed changes".into(),
            W::Reset { .. } => "Reset".into(),
            W::Pull { remote } => format!("Pull from {}", remote),
            W::Init => "Start a repository here".into(),
            W::DeleteRepo => "Remove Git from this folder".into(),
            W::AddRemote { name, .. } => format!("Add remote {}", q(name)),
            W::RemoveRemote { name } => format!("Remove remote {}", q(name)),
            W::AddAndCommit { files, .. } => match files {
                crate::commands::FileSelection::Explicit(f) if f.len() == 1 => {
                    format!("Commit {}", q(&f[0]))
                }
                crate::commands::FileSelection::Explicit(f) => format!("Commit {} files", f.len()),
                _ => "Commit everything".into(),
            },
            W::Push { remote } => format!("Push to {}", remote),
            W::Undo | W::UndoIntent => "Undo the last commit".into(),
            W::ForensicRecover => "Find lost work".into(),
            W::CleanupIntent => "Discard uncommitted changes".into(),
            W::Amend {
                message: Some(_), ..
            } => "Reword the last commit".into(),
            W::Amend { .. } => "Amend the last commit".into(),
            W::Revert { oid } => format!("Revert {}", &oid[..oid.len().min(7)]),
            W::Unstage { .. } => "Unstage files".into(),
            W::SetUpstream { remote, branch } => format!("Track {}/{}", remote, branch),
            W::SquashLastN { n } => format!("Squash the last {} commits", n),
            W::PullRebase { .. } => "Pull with rebase".into(),
            W::ForcePush { .. } => "Force push".into(),
            W::DiscardLocal { .. } => "Reset to the remote".into(),
            W::MergeAbort => "Abort the merge".into(),
            W::RebaseContinue => "Continue the rebase".into(),
            W::RebaseSkip => "Skip this commit".into(),
            W::RebaseAbort => "Abort the rebase".into(),
            W::CherryPickContinue => "Continue the cherry-pick".into(),
            W::CherryPickAbort => "Abort the cherry-pick".into(),
            W::RevertContinue => "Continue the revert".into(),
            W::RevertAbort => "Abort the revert".into(),
        },
        I::FeaturePipeline(op) => match op {
            F::AddCommitPush { feature_hint } if feature_hint.is_empty() => {
                "Push only some changes".into()
            }
            F::AddCommitPush { feature_hint } => {
                format!("Push only the {} changes", q(feature_hint))
            }
            F::CommitPush {
                stage_first: Some(_),
                ..
            } => "Commit and push everything".into(),
            F::CommitPush {
                stage_first: None, ..
            } => "Commit and push staged files".into(),
            F::Sync { .. } => "Update from main".into(),
        },
    })
}

/// The one line printed after classification: engine glyph + what GiTrust
/// understood (+ the model's latency). The model's probability only appears
/// under GITRUST_DEBUG. `plain` = no color/unicode → ASCII glyph.
pub fn engine_line(
    r: &crate::commands::IntentResult,
    elapsed: std::time::Duration,
    plain: bool,
    debug: bool,
) -> Option<String> {
    let title = intent_title(&r.intent)?;
    Some(echo_line(
        &title,
        r.engine,
        elapsed,
        Some(r.confidence),
        plain,
        debug,
    ))
}

/// Format one echo line for a title (shared by the early echo printed before
/// an argument question, which only knows the op's title).
pub fn echo_line(
    title: &str,
    engine: crate::commands::Engine,
    elapsed: std::time::Duration,
    confidence: Option<f32>,
    plain: bool,
    debug: bool,
) -> String {
    use crate::commands::Engine;
    use crate::copy::*;
    let (glyph, suffix) = match engine {
        Engine::Fast => (
            if plain {
                ENGINE_FAST_ASCII
            } else {
                ENGINE_FAST_GLYPH
            },
            String::new(),
        ),
        Engine::Kev => (
            if plain {
                ENGINE_KEV_ASCII
            } else {
                ENGINE_KEV_GLYPH
            },
            format!(" · {:.1}s", elapsed.as_secs_f32()),
        ),
        Engine::User => (
            if plain {
                ENGINE_USER_ASCII
            } else {
                ENGINE_USER_GLYPH
            },
            " · your choice".to_string(),
        ),
    };
    let dbg = match confidence {
        Some(p) if debug && engine == Engine::Kev => format!("  (p={:.2})", p),
        _ => String::new(),
    };
    format!("{}  {}{}{}", glyph, title, suffix, dbg)
}

// =============================================================================
// Spinner — feedback while the local model works (design handoff §M2)
// =============================================================================

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The label of the spinner currently on screen, if any — so a deeper layer
/// (e.g. a cold start inside a model call) can say what's really happening.
static ACTIVE: Mutex<Option<Arc<Mutex<String>>>> = Mutex::new(None);

/// A one-line spinner on stderr: "⠋ Understanding your request… · 4s". Only on
/// a terminal (a no-op when piped or under tests). Dropping it erases the line,
/// so a prompt never shares a line with it. Elapsed seconds appear after 2 s.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(label: &str) -> Spinner {
        use std::io::IsTerminal;
        let stop = Arc::new(AtomicBool::new(false));
        if !std::io::stderr().is_terminal() || ACTIVE.lock().map(|a| a.is_some()).unwrap_or(true) {
            return Spinner { stop, handle: None };
        }
        let text = Arc::new(Mutex::new(label.to_string()));
        if let Ok(mut a) = ACTIVE.lock() {
            *a = Some(text.clone());
        }
        let plain = !crate::danger::use_color();
        let frames: &[&str] = if plain {
            &["|", "/", "-", "\\"]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        };
        let stop2 = stop.clone();
        let started = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            use std::io::Write;
            let mut i = 0usize;
            // Don't flash a spinner for a fast answer.
            std::thread::sleep(std::time::Duration::from_millis(150));
            while !stop2.load(Ordering::Relaxed) {
                let label = text.lock().map(|t| t.clone()).unwrap_or_default();
                let secs = started.elapsed().as_secs();
                let elapsed = if secs >= 2 {
                    format!(" · {}s", secs)
                } else {
                    String::new()
                };
                let line = format!("{} {}{}", frames[i % frames.len()], label, elapsed);
                let mut err = std::io::stderr();
                if plain {
                    let _ = write!(err, "\r\x1b[2K{}", line);
                } else {
                    let _ = write!(err, "\r\x1b[2K\x1b[2m{}\x1b[0m", line);
                }
                let _ = err.flush();
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        });
        Spinner {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
            if let Ok(mut a) = ACTIVE.lock() {
                *a = None;
            }
        }
    }
}

/// Change what the on-screen spinner says (no-op when none is running).
/// Returns false when there is no spinner, so the caller can print instead.
pub fn spinner_relabel(label: &str) -> bool {
    match ACTIVE.lock().ok().and_then(|a| a.clone()) {
        Some(t) => {
            if let Ok(mut t) = t.lock() {
                *t = label.to_string();
            }
            true
        }
        None => false,
    }
}

// =============================================================================
// Pick-one menu with direct keys (design handoff: "[a][b][c] — not just arrow
// keys"). Rows are numbered; 1–9 choose immediately, ↑↓/j/k move, Enter
// chooses, Esc/q/Ctrl-C cancel. Used for menus of up to 9 options; longer
// lists keep inquire's type-to-filter.
// =============================================================================

/// What a key does in the pick-one menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    Focus(usize),
    Choose(usize),
    Cancel,
    Ignore,
}

pub fn picker_key(
    code: crossterm::event::KeyCode,
    ctrl: bool,
    n: usize,
    focus: usize,
) -> PickerAction {
    use crossterm::event::KeyCode as K;
    if n == 0 {
        return PickerAction::Cancel;
    }
    match code {
        K::Char('c') | K::Char('d') if ctrl => PickerAction::Cancel,
        K::Esc | K::Char('q') => PickerAction::Cancel,
        K::Enter => PickerAction::Choose(focus),
        K::Down | K::Char('j') | K::Tab => PickerAction::Focus((focus + 1) % n),
        K::Up | K::Char('k') | K::BackTab => PickerAction::Focus((focus + n - 1) % n),
        K::Char(c) if c.is_ascii_digit() => match c.to_digit(10) {
            Some(d) if d >= 1 && (d as usize) <= n => PickerAction::Choose(d as usize - 1),
            _ => PickerAction::Ignore,
        },
        _ => PickerAction::Ignore,
    }
}

/// The rows of the menu: "> [2] Push  — upload all your commits", each cut to
/// `width` columns (a long hint ends in "…"; never wraps, so redraws stay
/// exact). `opts` = (label, hint); labels may carry ANSI-colored markers.
pub fn picker_rows(
    opts: &[(String, String)],
    focus: usize,
    width: usize,
    color: bool,
) -> Vec<String> {
    opts.iter()
        .enumerate()
        .map(|(i, (label, hint))| {
            let cursor = if i == focus { ">" } else { " " };
            let head = format!("{} [{}] {}", cursor, i + 1, label);
            let head_w = display_width(&head);
            let mut row = head.clone();
            if !hint.is_empty() && head_w + 4 < width {
                let room = width - head_w - 4; // "  — "
                let mut h: String = hint.clone();
                if h.chars().count() > room {
                    h = h.chars().take(room.saturating_sub(1)).collect::<String>();
                    h = h.trim_end().to_string();
                    h.push('…');
                }
                if color {
                    row.push_str(&format!("  \x1b[2m— {}\x1b[0m", h));
                } else {
                    row.push_str(&format!("  — {}", h));
                }
            } else if head_w > width {
                let plain: String = strip_ansi(&head)
                    .chars()
                    .take(width.saturating_sub(1))
                    .collect();
                row = format!("{}…", plain);
            }
            if color && i == focus {
                row = format!("\x1b[36m{}\x1b[0m{}", cursor, &row[cursor.len()..]);
            }
            row
        })
        .collect()
}

/// Show the menu and wait for a decision: `Some(index)`, or `None` if the
/// user backed out. Errors only if the terminal can't be driven.
pub fn pick_one(question: &str, opts: &[(String, String)]) -> anyhow::Result<Option<usize>> {
    use crossterm::event::{read, Event, KeyEventKind, KeyModifiers};
    use std::io::Write;
    let width = term_width();
    let color = crate::danger::use_color();
    let q_lines = wrap_hanging("? ", question, width);
    let help = crate::copy::PICKER_HELP;
    let mut out = std::io::stdout();
    let draw = |out: &mut std::io::Stdout, focus: usize| -> std::io::Result<()> {
        for r in picker_rows(opts, focus, width, color) {
            write!(out, "{}\r\n", r)?;
        }
        if color {
            write!(out, "\x1b[2m[{}]\x1b[0m", help)?;
        } else {
            write!(out, "[{}]", help)?;
        }
        out.flush()
    };
    // Lines below the question that a redraw replaces: the rows + the help line.
    let block = opts.len() + 1;
    let mut focus = 0usize;
    let result = {
        let _raw = RawMode::on()?;
        for l in &q_lines {
            write!(out, "{}\r\n", l)?;
        }
        draw(&mut out, focus)?;
        loop {
            let Event::Key(k) = read()? else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match picker_key(
                k.code,
                k.modifiers.contains(KeyModifiers::CONTROL),
                opts.len(),
                focus,
            ) {
                PickerAction::Focus(f) => {
                    focus = f;
                    // Back to the first row, clear to the end, draw again.
                    write!(out, "\r\x1b[{}A\x1b[J", block - 1)?;
                    draw(&mut out, focus)?;
                }
                PickerAction::Choose(i) => break Some(i),
                PickerAction::Cancel => break None,
                PickerAction::Ignore => {}
            }
        }
    };
    // Replace the whole menu with a one-line echo, like inquire does.
    let up = q_lines.len() + block - 1;
    write!(out, "\r\x1b[{}A\x1b[J", up)?;
    match result {
        Some(i) => println!("> {} {}", question, strip_ansi(&opts[i].0)),
        None => println!("? {} <canceled>", question),
    }
    out.flush()?;
    Ok(result)
}

/// Wrap `text` to `width` columns: the first line starts with `lead`, the rest
/// are indented to line up under the text. Words longer than the width are
/// kept whole.
pub fn wrap_hanging(lead: &str, text: &str, width: usize) -> Vec<String> {
    let indent = " ".repeat(display_width(lead));
    let mut lines = Vec::new();
    let mut cur = lead.to_string();
    let mut cur_len = display_width(lead);
    let mut empty = true;
    for word in text.split_whitespace() {
        let w = display_width(word);
        if !empty && cur_len + 1 + w > width {
            lines.push(cur);
            cur = indent.clone();
            cur_len = indent.chars().count();
            empty = true;
        }
        if !empty {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(word);
        cur_len += w;
        empty = false;
    }
    lines.push(cur);
    lines
}

/// Columns a string takes on screen: ANSI codes take none; emoji-style
/// symbols (⛔, 🔒, …) take two. Good enough for our own copy.
pub fn display_width(s: &str) -> usize {
    strip_ansi(s)
        .chars()
        .map(|c| match c as u32 {
            0x26D4 | 0x2705 | 0x274C | 0x1F000..=0x1FAFF => 2,
            _ => 1,
        })
        .sum()
}

/// Terminal width for wrapping: the real width, capped at 100, 80 if unknown.
pub fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .clamp(40, 100)
}

/// Remove ANSI color sequences (labels carry colored markers on a TTY).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Words that leave the interactive prompt (besides an empty line / Ctrl-C /
/// Ctrl-D). Without this, "exit" went to the model as a Git request.
pub fn is_repl_exit(input: &str) -> bool {
    matches!(
        input.trim().to_lowercase().as_str(),
        "exit" | "quit" | "q" | ":q" | "bye" | "logout"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_line_says_what_was_understood() {
        use crate::commands::{Engine, FeaturePipelineOp, IntentResult, IntentType, ReadOnlyOp};
        let r = |intent, engine, confidence| IntentResult {
            intent,
            confidence,
            raw_query: "q".into(),
            engine,
            echoed: false,
        };
        let ms = std::time::Duration::from_millis(600);
        let fast = engine_line(
            &r(IntentType::ReadOnly(ReadOnlyOp::Status), Engine::Fast, 1.0),
            ms,
            false,
            false,
        );
        assert_eq!(fast.as_deref(), Some("⚡  Show status"));
        let kev = engine_line(
            &r(
                IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }),
                Engine::Kev,
                0.99,
            ),
            ms,
            false,
            false,
        );
        assert_eq!(kev.as_deref(), Some("✦  Show history · 0.6s"));
        // The model's probability is a debugging aid, not user copy.
        let dbg = engine_line(
            &r(
                IntentType::ReadOnly(ReadOnlyOp::Log { count: 10 }),
                Engine::Kev,
                0.99,
            ),
            ms,
            false,
            true,
        );
        assert!(dbg.unwrap().contains("p=0.99"));
        let user = engine_line(
            &r(
                IntentType::FeaturePipeline(FeaturePipelineOp::AddCommitPush {
                    feature_hint: "login".into(),
                }),
                Engine::User,
                1.0,
            ),
            ms,
            false,
            false,
        );
        assert_eq!(
            user.as_deref(),
            Some("◇  Push only the “login” changes · your choice")
        );
        // A refusal has nothing to echo — its own message says it.
        assert!(engine_line(
            &r(IntentType::NeedsClarification("x".into()), Engine::Kev, 0.9),
            ms,
            false,
            false
        )
        .is_none());
        // ASCII fallback when color/unicode is off.
        let plain = engine_line(
            &r(IntentType::ReadOnly(ReadOnlyOp::Status), Engine::Fast, 1.0),
            ms,
            true,
            false,
        );
        assert_eq!(plain.as_deref(), Some("*  Show status"));
    }

    #[test]
    fn repl_exit_words_leave_instead_of_reaching_the_model() {
        for w in ["exit", "Quit", " q ", ":q", "bye"] {
            assert!(is_repl_exit(w), "{w}");
        }
        for w in ["status", "quit the rebase", "exit the merge"] {
            assert!(!is_repl_exit(w), "{w}");
        }
    }

    #[test]
    fn wrap_hangs_continuation_lines_under_the_text() {
        let lines = wrap_hanging("     Risks:    ", "Anyone who fetched the old version will need to reset. Their commits made on top are abandoned.", 60);
        assert_eq!(
            lines[0],
            "     Risks:    Anyone who fetched the old version will need"
        );
        assert!(
            lines[1].starts_with("               to reset."),
            "{:?}",
            lines
        );
        assert!(lines.iter().all(|l| l.chars().count() <= 60), "{:?}", lines);
        // ⛔ is two columns wide: the lead counts it as such.
        assert_eq!(display_width("  ⛔ x"), 6);
        assert_eq!(display_width("\x1b[31m⚠\x1b[0m"), 1);
        // A word longer than the width is kept whole, not split.
        let long = wrap_hanging(
            "  ",
            "see ~/.gitrust/trash/20261015-123839/some/very/long/path",
            20,
        );
        assert!(long
            .iter()
            .any(|l| l.contains("~/.gitrust/trash/20261015-123839/some/very/long/path")));
    }

    #[test]
    fn picker_keys_choose_directly() {
        use crossterm::event::KeyCode as K;
        // 3 options, focus on row 0.
        assert_eq!(
            picker_key(K::Char('2'), false, 3, 0),
            PickerAction::Choose(1)
        );
        assert_eq!(
            picker_key(K::Char('4'), false, 3, 0),
            PickerAction::Ignore,
            "no row 4"
        );
        assert_eq!(picker_key(K::Down, false, 3, 0), PickerAction::Focus(1));
        assert_eq!(
            picker_key(K::Down, false, 3, 2),
            PickerAction::Focus(0),
            "wraps"
        );
        assert_eq!(
            picker_key(K::Up, false, 3, 0),
            PickerAction::Focus(2),
            "wraps"
        );
        assert_eq!(
            picker_key(K::Char('j'), false, 3, 0),
            PickerAction::Focus(1)
        );
        assert_eq!(picker_key(K::Enter, false, 3, 2), PickerAction::Choose(2));
        for k in [K::Esc, K::Char('q')] {
            assert_eq!(picker_key(k, false, 3, 1), PickerAction::Cancel);
        }
        assert_eq!(picker_key(K::Char('c'), true, 3, 1), PickerAction::Cancel);
        assert_eq!(picker_key(K::Char('x'), false, 3, 1), PickerAction::Ignore);
    }

    #[test]
    fn picker_rows_number_and_fit_the_width() {
        let opts = vec![
            (
                "Push only some changes".to_string(),
                "pick one topic's files, commit, push".to_string(),
            ),
            ("Push".to_string(), "upload all your commits".to_string()),
        ];
        let rows = picker_rows(&opts, 1, 40, false);
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0].starts_with("  [1] Push only some changes"),
            "{:?}",
            rows
        );
        assert!(rows[1].starts_with("> [2] Push"), "{:?}", rows);
        assert!(rows.iter().all(|r| display_width(r) <= 40), "{:?}", rows);
        assert!(
            rows[0].ends_with('…'),
            "a long hint is trimmed, not wrapped: {:?}",
            rows
        );
    }

    #[test]
    fn spinner_is_silent_off_a_terminal() {
        // cargo test's stderr is not a TTY: no thread, nothing printed, and a
        // relabel with no spinner says so (the caller then prints instead).
        let s = Spinner::start("Understanding your request…");
        assert!(s.handle.is_none());
        drop(s);
        assert!(!spinner_relabel("Starting the local model…"));
    }

    #[test]
    fn plan_keys_do_what_the_legend_says() {
        use crate::commands::GateResponse as R;
        use crossterm::event::KeyCode as K;
        assert!(matches!(
            plan_key(K::Enter, false, false, false),
            Some(R::Confirmed)
        ));
        assert!(matches!(
            plan_key(K::Char('y'), false, false, false),
            Some(R::Confirmed)
        ));
        for k in [K::Char('q'), K::Char('n'), K::Esc] {
            assert!(
                matches!(plan_key(k, false, false, false), Some(R::Aborted)),
                "{:?}",
                k
            );
        }
        assert!(matches!(
            plan_key(K::Char('c'), true, false, false),
            Some(R::Aborted)
        ));
        // Anything else is ignored — a stray key never runs a plan.
        for k in [K::Char('x'), K::Char(' '), K::Tab, K::Char('c')] {
            assert!(plan_key(k, false, false, false).is_none(), "{:?}", k);
        }
    }

    #[test]
    fn canceled_is_detected_through_context() {
        let e: anyhow::Error = anyhow::Error::new(Canceled).context("while picking files");
        assert!(is_canceled(&e));
        assert!(!is_canceled(&anyhow::anyhow!("push rejected")));
        assert_eq!(Canceled.to_string(), crate::copy::CANCELED);
    }

    #[test]
    fn inquire_cancel_and_interrupt_become_canceled() {
        let r: anyhow::Result<()> = prompt_result(
            Err(inquire::InquireError::OperationCanceled),
            "Choice prompt",
        );
        assert!(is_canceled(&r.unwrap_err()));
        let r: anyhow::Result<()> = prompt_result(
            Err(inquire::InquireError::OperationInterrupted),
            "Choice prompt",
        );
        assert!(is_canceled(&r.unwrap_err()));
        let r: anyhow::Result<()> =
            prompt_result(Err(inquire::InquireError::NotTTY), "Choice prompt");
        let e = r.unwrap_err();
        assert!(!is_canceled(&e));
        assert!(e.to_string().contains("Choice prompt failed"));
    }
}
