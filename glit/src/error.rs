use std::fmt;

/// Core error type for GLit. Every git2 error is classified by error code
/// into one of three categories with human-readable context.
#[derive(Debug)]
pub enum GlitError {
    /// Error the user can act on — includes actionable suggestions.
    Recoverable {
        message: String,
        suggestions: Vec<String>,
    },
    /// Error that halts the pipeline — repo may be in a partial state.
    Fatal {
        message: String,
        repo_state: String,
        next_steps: Vec<String>,
    },
    /// Internal/unexpected error — wrapped with anyhow context.
    Internal(anyhow::Error),
}

impl fmt::Display for GlitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlitError::Recoverable {
                message,
                suggestions,
            } => {
                write!(f, "Error: {}", message)?;
                for s in suggestions {
                    write!(f, "\n  → {}", s)?;
                }
                Ok(())
            }
            GlitError::Fatal {
                message,
                repo_state,
                next_steps,
            } => {
                write!(f, "Fatal: {}", message)?;
                write!(f, "\n  Repo state: {}", repo_state)?;
                for step in next_steps {
                    write!(f, "\n  → {}", step)?;
                }
                Ok(())
            }
            GlitError::Internal(e) => write!(f, "Internal error: {:#}", e),
        }
    }
}

impl std::error::Error for GlitError {}

impl From<anyhow::Error> for GlitError {
    fn from(e: anyhow::Error) -> Self {
        GlitError::Internal(e)
    }
}

impl From<git2::Error> for GlitError {
    fn from(e: git2::Error) -> Self {
        match e.code() {
            git2::ErrorCode::Auth => GlitError::Recoverable {
                message: "Authentication failed".into(),
                suggestions: vec![
                    "SSH: check ssh-add -l".into(),
                    "HTTPS: verify your token has repo write access".into(),
                ],
            },
            git2::ErrorCode::NotFastForward => GlitError::Recoverable {
                message: "Push rejected — remote has commits you don't have".into(),
                suggestions: vec!["Run: glit pull  then retry".into()],
            },
            git2::ErrorCode::NotFound => GlitError::Recoverable {
                message: format!("Not found: {}", e.message()),
                suggestions: vec!["Run: git remote -v".into()],
            },
            git2::ErrorCode::MergeConflict => GlitError::Fatal {
                message: "Merge conflicts detected — GLit cannot resolve these".into(),
                repo_state: "index contains conflicts — no changes were lost".into(),
                next_steps: vec![
                    "Resolve conflicts manually".into(),
                    "Then run: glit add  and  glit commit".into(),
                ],
            },
            _ => GlitError::Internal(e.into()),
        }
    }
}
