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

/// What a key does at "run this plan?": `Some(Confirmed)` / `Some(Aborted)`,
/// or `None` to ignore it (any other key — so a stray keystroke never runs a
/// plan). Matches the printed legend `[Enter] run · [q] cancel`.
pub fn plan_key(
    code: crossterm::event::KeyCode,
    ctrl: bool,
) -> Option<crate::commands::GateResponse> {
    use crate::commands::GateResponse;
    use crossterm::event::KeyCode as K;
    match code {
        K::Char('c') | K::Char('d') if ctrl => Some(GateResponse::Aborted),
        K::Enter | K::Char('y') | K::Char('Y') => Some(GateResponse::Confirmed),
        K::Char('q') | K::Char('Q') | K::Char('n') | K::Char('N') | K::Esc => {
            Some(GateResponse::Aborted)
        }
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
pub fn run_plan_keypress() -> anyhow::Result<crate::commands::GateResponse> {
    use crate::commands::GateResponse;
    use crossterm::event::{read, Event, KeyEventKind, KeyModifiers};
    use std::io::{IsTerminal, Write};

    let legend = crate::copy::PLAN_KEYMAP_LEGEND;
    if !std::io::stdin().is_terminal() {
        print!("  {} [y/N] ", legend);
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(if matches!(line.trim(), "y" | "Y" | "yes") {
            GateResponse::Confirmed
        } else {
            GateResponse::Aborted
        });
    }
    print!("  {}  ", crate::copy::style_keys(legend));
    std::io::stdout().flush().ok();
    let response = {
        let _raw = RawMode::on()?;
        loop {
            if let Event::Key(k) = read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(r) = plan_key(k.code, k.modifiers.contains(KeyModifiers::CONTROL)) {
                    break r;
                }
            }
        }
    };
    println!();
    Ok(response)
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
    fn repl_exit_words_leave_instead_of_reaching_the_model() {
        for w in ["exit", "Quit", " q ", ":q", "bye"] {
            assert!(is_repl_exit(w), "{w}");
        }
        for w in ["status", "quit the rebase", "exit the merge"] {
            assert!(!is_repl_exit(w), "{w}");
        }
    }

    #[test]
    fn plan_keys_do_what_the_legend_says() {
        use crate::commands::GateResponse as R;
        use crossterm::event::KeyCode as K;
        assert!(matches!(plan_key(K::Enter, false), Some(R::Confirmed)));
        assert!(matches!(plan_key(K::Char('y'), false), Some(R::Confirmed)));
        for k in [K::Char('q'), K::Char('n'), K::Esc] {
            assert!(matches!(plan_key(k, false), Some(R::Aborted)), "{:?}", k);
        }
        assert!(matches!(plan_key(K::Char('c'), true), Some(R::Aborted)));
        // Anything else is ignored — a stray key never runs a plan.
        for k in [K::Char('x'), K::Char(' '), K::Tab, K::Char('c')] {
            assert!(plan_key(k, false).is_none(), "{:?}", k);
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
