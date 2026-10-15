use inquire::{Confirm, MultiSelect, Select, Text};

use crate::ux::prompt_result;

use crate::commands::{ClassificationResult, GateResponse, GateType};

/// Trait for user confirmation — enables swapping interactive prompts
/// with auto-confirm for CI or test mocks.
pub trait ConfirmationGate {
    fn prompt(&self, gate: &GateType) -> anyhow::Result<GateResponse>;
}

/// Default gate using inquire for interactive prompts.
pub struct InquireGate;

impl ConfirmationGate for InquireGate {
    fn prompt(&self, gate: &GateType) -> anyhow::Result<GateResponse> {
        match gate {
            GateType::None => Ok(GateResponse::Confirmed),

            GateType::RunPlan => crate::ux::run_plan_keypress(),

            GateType::SimpleConfirm { message } => {
                let confirmed = prompt_result(
                    Confirm::new(message).with_default(true).prompt(),
                    "Confirmation prompt",
                )?;

                if confirmed {
                    Ok(GateResponse::Confirmed)
                } else {
                    Ok(GateResponse::Aborted)
                }
            }

            GateType::FileMultiSelect {
                classification,
                show_all,
            } => prompt_file_multiselect(classification, *show_all),

            GateType::EditableText { initial, label } => {
                let result = prompt_result(
                    Text::new(label)
                        .with_initial_value(initial)
                        .with_help_message(crate::copy::TEXT_PROMPT_HELP)
                        .with_validator(|input: &str| {
                            if input.trim().is_empty() {
                                Ok(inquire::validator::Validation::Invalid(
                                    crate::copy::TEXT_PROMPT_EMPTY.into(),
                                ))
                            } else {
                                Ok(inquire::validator::Validation::Valid)
                            }
                        })
                        .prompt(),
                    "Text prompt",
                )?;

                Ok(GateResponse::TextProvided(result))
            }

            GateType::TypeToConfirm {
                expected,
                message,
                explanation,
                danger,
            } => {
                if let Some(exp) = explanation {
                    print_explanation_block(*danger, exp);
                }
                prompt_type_to_confirm(expected, message)
            }

            GateType::Choose { message, options } => {
                if options.is_empty() {
                    return Ok(GateResponse::Aborted);
                }
                let labels: Vec<String> = options.clone();
                let selected = prompt_result(
                    Select::new(message, labels.clone())
                        .with_starting_cursor(0)
                        .with_help_message(crate::copy::CHOICE_HELP)
                        .prompt(),
                    "Choice prompt",
                )?;
                match labels.iter().position(|l| l == &selected) {
                    Some(idx) => Ok(GateResponse::Chose(idx)),
                    None => Ok(GateResponse::Aborted),
                }
            }

            GateType::ChoiceGate { question, options } => {
                if options.is_empty() {
                    return Ok(GateResponse::Aborted);
                }
                // Show each option's one-sentence consequence (dimmed) so the
                // choice is made with eyes open. The label→index mapping is by
                // position, so the displayed row stays paired with its option.
                let use_color = crate::danger::use_color();
                let dim = |s: &str| -> String {
                    if use_color {
                        format!("\x1b[2m{}\x1b[0m", s)
                    } else {
                        s.to_string()
                    }
                };
                let labels: Vec<String> = options
                    .iter()
                    .map(|o| {
                        if o.consequence.is_empty() {
                            o.label.clone()
                        } else {
                            format!("{}  {}", o.label, dim(&format!("— {}", o.consequence)))
                        }
                    })
                    .collect();
                // After a pick, echo just the chosen label — not the wrapped
                // consequence sentence.
                let echo = |o: inquire::list_option::ListOption<&String>| {
                    options
                        .get(o.index)
                        .map(|c| crate::ux::strip_ansi(&c.label))
                        .unwrap_or_default()
                };
                let selected = prompt_result(
                    Select::new(question, labels.clone())
                        .with_starting_cursor(0)
                        .with_help_message(crate::copy::CHOICE_HELP)
                        .with_formatter(&echo)
                        .prompt(),
                    "Choice prompt",
                )?;
                match labels.iter().position(|l| l == &selected) {
                    Some(idx) => Ok(GateResponse::Chose(idx)),
                    None => Ok(GateResponse::Aborted),
                }
            }

            GateType::RecoveryBrowser { candidates } => {
                if candidates.is_empty() {
                    println!("{}", crate::copy::FORENSIC_NONE_FOUND);
                    return Ok(GateResponse::Aborted);
                }
                println!();
                println!("{}", crate::copy::FORENSIC_BROWSER_HEADER);
                let labels: Vec<String> = candidates.iter().map(format_lost_commit).collect();
                let selected = prompt_result(
                    Select::new("Recover which commit?", labels.clone())
                        .with_starting_cursor(0)
                        .with_help_message(crate::copy::CHOICE_HELP)
                        .prompt(),
                    "Recovery browser prompt",
                )?;
                match labels.iter().position(|l| l == &selected) {
                    Some(idx) => Ok(GateResponse::Chose(idx)),
                    None => Ok(GateResponse::Aborted),
                }
            }
        }
    }
}

/// One recovery-browser row: "2 hours ago — 'feat: export csv' — 3 files [stash]".
fn format_lost_commit(c: &crate::recovery::LostCommit) -> String {
    let age = humanize_age(c.age_secs);
    let stash = if c.is_stash_shaped { " [stash]" } else { "" };
    format!(
        "{} — '{}' — {} file{}{}",
        age,
        c.summary,
        c.files_touched,
        if c.files_touched == 1 { "" } else { "s" },
        stash
    )
}

/// Compact relative age: "just now" / "5 minutes ago" / "2 hours ago" / "3 days ago".
fn humanize_age(secs: u64) -> String {
    let (n, unit) = if secs < 60 {
        return "just now".to_string();
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86400 {
        (secs / 3600, "hour")
    } else {
        (secs / 86400, "day")
    };
    format!("{} {}{} ago", n, unit, if n == 1 { "" } else { "s" })
}

/// File multi-select bucketed by classifier confidence:
///   • Related   — pre-checked
///   • Uncertain — visible, not pre-checked
///   • Unrelated — hidden by default (opt in via `--show-all`)
///
/// Each row carries the classifier's per-file reason as dim trailing text so
/// the user can see *why* the file was bucketed without having to ask.
fn prompt_file_multiselect(
    classification: &ClassificationResult,
    show_all: bool,
) -> anyhow::Result<GateResponse> {
    let n_related = classification.related.len();
    let n_uncertain = classification.uncertain.len();
    let n_unrelated = classification.unrelated.len();

    // Bucket header — printed once before inquire takes over.
    println!();
    println!("Select files to stage");
    println!("  Related   ({}, pre-checked)", n_related);
    println!("  Uncertain ({}, visible, not pre-checked)", n_uncertain);
    if n_unrelated > 0 {
        if show_all {
            println!("  Unrelated ({}, shown via --show-all)", n_unrelated);
        } else {
            println!(
                "  Unrelated ({}, hidden · re-run with --show-all to see)",
                n_unrelated
            );
        }
    }
    println!();

    let use_color = crate::danger::use_color();
    let dim = |s: &str| -> String {
        if use_color {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    };

    let mut options: Vec<FileOption> = Vec::new();
    let mut default_selected: Vec<usize> = Vec::new();

    for f in &classification.related {
        let idx = options.len();
        options.push(FileOption {
            path: f.path.clone(),
            label: format!("[related]   {}  {}", f.path, dim(&f.reason)),
        });
        default_selected.push(idx);
    }

    for f in &classification.uncertain {
        options.push(FileOption {
            path: f.path.clone(),
            label: format!("[uncertain] {}  {}", f.path, dim(&f.reason)),
        });
    }

    if show_all {
        for f in &classification.unrelated {
            options.push(FileOption {
                path: f.path.clone(),
                label: format!("[unrelated] {}  {}", f.path, dim(&f.reason)),
            });
        }
    }

    if options.is_empty() {
        println!("  No files to stage.");
        if n_unrelated > 0 && !show_all {
            println!(
                "  {} file(s) hidden as unrelated — re-run with `--show-all` to see them.",
                n_unrelated
            );
        }
        return Ok(GateResponse::FilesSelected(vec![]));
    }

    let display_labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();

    let selected = prompt_result(
        MultiSelect::new(
            "Select files to stage (Space to toggle, Enter to confirm):",
            display_labels,
        )
        .with_default(&default_selected)
        .prompt(),
        "File selection prompt",
    )?;

    let selected_paths: Vec<String> = selected
        .iter()
        .filter_map(|label| {
            options
                .iter()
                .find(|o| &o.label == label)
                .map(|o| o.path.clone())
        })
        .collect();

    if selected_paths.is_empty() {
        if n_unrelated > 0 && !show_all {
            println!(
                "  Nothing staged. {} file(s) hidden as unrelated — re-run with `--show-all` to include them.",
                n_unrelated
            );
        }
        Ok(GateResponse::Aborted)
    } else {
        Ok(GateResponse::FilesSelected(selected_paths))
    }
}

/// Print a beginner-friendly description before a TypeToConfirm prompt.
/// Format:
///   ⚠⚠ Destructive: <what>
///      Risks:     <risks>
///      Recovery:  <recovery>     (when Some)
///      Safety:    <safety>       (when Some)
fn print_explanation_block(danger: crate::danger::DangerLevel, exp: &crate::explain::Explanation) {
    let marker = crate::danger::colored_marker(danger);
    let label_text = crate::danger::label(danger);
    let label_colored = crate::danger::colorize(danger, label_text);
    println!();
    if marker.is_empty() {
        println!("  {}: {}", label_colored, exp.what);
    } else {
        println!("  {} {}: {}", marker, label_colored, exp.what);
    }
    println!("     Risks:    {}", exp.risks);
    if let Some(r) = &exp.recovery {
        println!("     Recovery: {}", r);
    }
    if let Some(s) = &exp.safety {
        println!("     Safety:   {}", s);
    }
    println!();
}

/// TypeToConfirm gate — user must type the expected string exactly.
fn prompt_type_to_confirm(expected: &str, message: &str) -> anyhow::Result<GateResponse> {
    let expected_owned = expected.to_string();
    let result = Text::new(message)
        .with_placeholder(expected)
        .with_validator(move |input: &str| {
            if input == expected_owned {
                Ok(inquire::validator::Validation::Valid)
            } else {
                Ok(inquire::validator::Validation::Invalid(
                    format!("Must type '{}' exactly to confirm.", expected_owned).into(),
                ))
            }
        })
        .prompt();

    match prompt_result(result, "Type-to-confirm prompt")? {
        input if input == expected => Ok(GateResponse::Confirmed),
        _ => Ok(GateResponse::Aborted),
    }
}

/// Internal struct used to keep path + display label together.
struct FileOption {
    path: String,
    label: String,
}

/// Auto-confirm gate for --yes flag and CI environments.
/// Selects all related files, uses initial text for editable fields.
pub struct AutoConfirmGate;

impl ConfirmationGate for AutoConfirmGate {
    fn prompt(&self, gate: &GateType) -> anyhow::Result<GateResponse> {
        match gate {
            GateType::None | GateType::RunPlan => Ok(GateResponse::Confirmed),
            GateType::SimpleConfirm { .. } => Ok(GateResponse::Confirmed),
            GateType::FileMultiSelect { classification, .. } => {
                // Select all related and uncertain files automatically
                let files: Vec<String> = classification
                    .related
                    .iter()
                    .chain(classification.uncertain.iter())
                    .map(|f| f.path.clone())
                    .collect();
                Ok(GateResponse::FilesSelected(files))
            }
            GateType::EditableText { initial, .. } => {
                Ok(GateResponse::TextProvided(initial.clone()))
            }
            GateType::TypeToConfirm { .. } => {
                // --yes flag does NOT bypass TypeToConfirm — safety gate for destructive ops
                // This forces the user to always confirm destructive operations manually
                Err(anyhow::anyhow!(
                    "--yes flag cannot bypass a type-to-confirm gate (destructive operation). \
                     Run interactively and confirm manually."
                ))
            }
            GateType::Choose { options, .. } => {
                if options.is_empty() {
                    Ok(GateResponse::Aborted)
                } else {
                    Ok(GateResponse::Chose(0))
                }
            }
            GateType::ChoiceGate { question, options } => {
                // --yes must NEVER silently pick from a ChoiceGate — an auto-
                // chosen undo mode, cleanup scope, or reconcile strategy could
                // discard or rewrite work the user meant to keep. Refuse,
                // mirroring TypeToConfirm. Echo the actual question (M-17) and
                // the choices, so the refusal says what to rephrase toward.
                let choices: Vec<String> = options
                    .iter()
                    .map(|o| crate::ux::strip_ansi(&o.label).trim().to_string())
                    .collect();
                Err(anyhow::anyhow!(
                    "--yes won't choose for you — {}\n   Choices: {}\n   Run it interactively \
                     (without --yes) to pick one, or say more precisely what you want.",
                    question,
                    choices.join(" · ")
                ))
            }
            GateType::RecoveryBrowser { .. } => {
                // --yes must NEVER auto-pick a commit to restore. Refuse,
                // mirroring ChoiceGate/TypeToConfirm.
                Err(anyhow::anyhow!(
                    "--yes flag cannot pick a commit to recover. \
                     Run interactively and choose from the recovery browser."
                ))
            }
        }
    }
}

/// Mock gate for testing — returns canned responses.
#[cfg(test)]
pub struct MockGate {
    pub response: GateResponse,
}

#[cfg(test)]
impl ConfirmationGate for MockGate {
    fn prompt(&self, _gate: &GateType) -> anyhow::Result<GateResponse> {
        Ok(self.response.clone())
    }
}

/// Scripted gate for multi-gate flows (Phase 12 reconcile): answers a
/// `ChoiceGate`/`Choose` with a fixed index, confirms `SimpleConfirm`, returns
/// the initial text for `EditableText`, auto-selects related+uncertain files —
/// and RECORDS whether a `ChoiceGate` was ever shown, so tests can assert the
/// degenerate-count guards skip the gate (Amendment 2).
#[cfg(test)]
pub struct ScriptedGate {
    pub choice: usize,
    pub choice_shown: std::cell::Cell<bool>,
    /// The number of options in the most-recently-shown `ChoiceGate` — lets the
    /// undo-safety test assert the gate offered all four options (Phase 13).
    pub last_choice_options: std::cell::Cell<usize>,
    /// The labels of the most-recently-shown `ChoiceGate` (for reorder assertions).
    pub last_choice_labels: std::cell::RefCell<Vec<String>>,
    /// True if a `TypeToConfirm` gate was shown (undo-discard requires it).
    pub type_to_confirm_shown: std::cell::Cell<bool>,
}

#[cfg(test)]
impl ScriptedGate {
    pub fn new(choice: usize) -> Self {
        Self {
            choice,
            choice_shown: std::cell::Cell::new(false),
            last_choice_options: std::cell::Cell::new(0),
            last_choice_labels: std::cell::RefCell::new(Vec::new()),
            type_to_confirm_shown: std::cell::Cell::new(false),
        }
    }
}

#[cfg(test)]
impl ConfirmationGate for ScriptedGate {
    fn prompt(&self, gate: &GateType) -> anyhow::Result<GateResponse> {
        match gate {
            GateType::None | GateType::RunPlan | GateType::SimpleConfirm { .. } => {
                Ok(GateResponse::Confirmed)
            }
            GateType::TypeToConfirm { .. } => {
                self.type_to_confirm_shown.set(true);
                Ok(GateResponse::Confirmed)
            }
            GateType::EditableText { initial, .. } => {
                Ok(GateResponse::TextProvided(initial.clone()))
            }
            GateType::FileMultiSelect { classification, .. } => {
                let files: Vec<String> = classification
                    .related
                    .iter()
                    .chain(classification.uncertain.iter())
                    .map(|f| f.path.clone())
                    .collect();
                Ok(GateResponse::FilesSelected(files))
            }
            GateType::Choose { options, .. } => {
                if options.is_empty() {
                    Ok(GateResponse::Aborted)
                } else {
                    Ok(GateResponse::Chose(self.choice.min(options.len() - 1)))
                }
            }
            GateType::ChoiceGate { options, .. } => {
                self.choice_shown.set(true);
                self.last_choice_options.set(options.len());
                *self.last_choice_labels.borrow_mut() =
                    options.iter().map(|o| o.label.clone()).collect();
                if options.is_empty() {
                    Ok(GateResponse::Aborted)
                } else {
                    Ok(GateResponse::Chose(self.choice.min(options.len() - 1)))
                }
            }
            GateType::RecoveryBrowser { candidates } => {
                self.choice_shown.set(true);
                self.last_choice_options.set(candidates.len());
                if candidates.is_empty() {
                    Ok(GateResponse::Aborted)
                } else {
                    Ok(GateResponse::Chose(self.choice.min(candidates.len() - 1)))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{ClassificationResult, ClassifiedFile, GateResponse, GateType};

    #[test]
    fn test_auto_confirm_gate_none() {
        let gate = AutoConfirmGate;
        let result = gate.prompt(&GateType::None).unwrap();
        assert!(matches!(result, GateResponse::Confirmed));
    }

    #[test]
    fn test_auto_confirm_gate_simple() {
        let gate = AutoConfirmGate;
        let result = gate
            .prompt(&GateType::SimpleConfirm {
                message: "Proceed?".to_string(),
            })
            .unwrap();
        assert!(matches!(result, GateResponse::Confirmed));
    }

    #[test]
    fn test_auto_confirm_selects_related_and_uncertain() {
        let gate = AutoConfirmGate;
        let classification = ClassificationResult {
            related: vec![ClassifiedFile {
                path: "src/login.rs".to_string(),
                reason: "login form".to_string(),
            }],
            uncertain: vec![ClassifiedFile {
                path: "src/routes.rs".to_string(),
                reason: "registers route".to_string(),
            }],
            unrelated: vec![ClassifiedFile {
                path: "src/config.rs".to_string(),
                reason: "unrelated config".to_string(),
            }],
        };
        let result = gate
            .prompt(&GateType::FileMultiSelect {
                classification,
                show_all: false,
            })
            .unwrap();
        match result {
            GateResponse::FilesSelected(files) => {
                assert_eq!(files.len(), 2);
                assert!(files.contains(&"src/login.rs".to_string()));
                assert!(files.contains(&"src/routes.rs".to_string()));
                assert!(!files.contains(&"src/config.rs".to_string()));
            }
            _ => panic!("Expected FilesSelected"),
        }
    }

    #[test]
    fn test_auto_confirm_editable_text_returns_initial() {
        let gate = AutoConfirmGate;
        let result = gate
            .prompt(&GateType::EditableText {
                initial: "feat: add login".to_string(),
                label: "Commit message:".to_string(),
            })
            .unwrap();
        match result {
            GateResponse::TextProvided(msg) => {
                assert_eq!(msg, "feat: add login");
            }
            _ => panic!("Expected TextProvided"),
        }
    }

    #[test]
    fn test_auto_confirm_rejects_choice_gate() {
        // --yes must NEVER silently pick a reconciliation strategy (Phase 12).
        let gate = AutoConfirmGate;
        let result = gate.prompt(&GateType::ChoiceGate {
            question: "Rebase or merge?".to_string(),
            options: vec![
                crate::commands::ChoiceOption {
                    label: "Rebase".to_string(),
                    consequence: "linear".to_string(),
                },
                crate::commands::ChoiceOption {
                    label: "Merge".to_string(),
                    consequence: "merge commit".to_string(),
                },
            ],
        });
        assert!(result.is_err(), "--yes must refuse a ChoiceGate");
        let msg = result.unwrap_err().to_string();
        // The refusal echoes the actual question and points at interactive mode,
        // so it reads correctly for ANY ChoiceGate (reconcile, undo, cleanup) —
        // not just the reconcile case (M-17 Defect 2).
        assert!(
            msg.contains("Rebase or merge?") && msg.contains("interactively"),
            "refusal should echo the question and point at interactive mode, got: {msg}"
        );
    }

    #[test]
    fn test_auto_confirm_rejects_type_to_confirm() {
        let gate = AutoConfirmGate;
        let result = gate.prompt(&GateType::TypeToConfirm {
            expected: "main".to_string(),
            message: "Type 'main' to confirm reset --hard:".to_string(),
            explanation: None,
            danger: crate::danger::DangerLevel::Destructive,
        });
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("type-to-confirm") || msg.contains("destructive"));
    }

    #[test]
    fn test_mock_gate_returns_configured_response() {
        let gate = MockGate {
            response: GateResponse::Aborted,
        };
        let result = gate.prompt(&GateType::None).unwrap();
        assert!(matches!(result, GateResponse::Aborted));
    }

    #[test]
    fn test_auto_confirm_selects_empty_classification() {
        let gate = AutoConfirmGate;
        let classification = ClassificationResult {
            related: vec![],
            uncertain: vec![],
            unrelated: vec![],
        };
        let result = gate
            .prompt(&GateType::FileMultiSelect {
                classification,
                show_all: false,
            })
            .unwrap();
        match result {
            GateResponse::FilesSelected(files) => {
                assert!(files.is_empty());
            }
            _ => panic!("Expected FilesSelected"),
        }
    }
}
