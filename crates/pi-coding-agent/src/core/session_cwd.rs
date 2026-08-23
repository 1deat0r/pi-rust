//! Missing-session-cwd handling — port of
//! `packages/coding-agent/src/core/session-cwd.ts`.
//!
//! When a stored session's working directory no longer exists on disk,
//! upstream refuses/asks before resuming. This module owns the detection,
//! error, and the human-readable error/prompt strings.

use std::path::Path;

/// The missing-cwd problem for a session, with the fallback the caller
/// intends to use instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCwdIssue {
    pub session_file: Option<String>,
    pub session_cwd: String,
    pub fallback_cwd: String,
}

/// Detect whether a stored session cwd is missing. Returns `None` when there
/// is no session file, no stored cwd, or the stored cwd still exists.
pub fn get_missing_session_cwd_issue(
    session_file: Option<&str>,
    session_cwd: &str,
    fallback_cwd: &str,
) -> Option<SessionCwdIssue> {
    if session_file.is_none() {
        return None;
    }
    if session_cwd.is_empty() || Path::new(session_cwd).exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: session_file.map(|s| s.to_string()),
        session_cwd: session_cwd.to_string(),
        fallback_cwd: fallback_cwd.to_string(),
    })
}

/// Upstream `formatMissingSessionCwdError`.
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map(|f| format!("\nSession file: {f}"))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{session_file}\nCurrent working directory: {}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// Upstream `formatMissingSessionCwdPrompt` (interactive selector label).
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// Error thrown when a missing stored session cwd must abort the resume.
#[derive(Debug, Clone)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

impl MissingSessionCwdError {
    pub fn new(issue: SessionCwdIssue) -> Self {
        Self { issue }
    }
}

impl std::fmt::Display for MissingSessionCwdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", format_missing_session_cwd_error(&self.issue))
    }
}

impl std::error::Error for MissingSessionCwdError {}

/// Throw a [`MissingSessionCwdError`] when the stored session cwd is gone.
pub fn assert_session_cwd_exists(
    session_file: Option<&str>,
    session_cwd: &str,
    fallback_cwd: &str,
) -> Result<(), MissingSessionCwdError> {
    if let Some(issue) = get_missing_session_cwd_issue(session_file, session_cwd, fallback_cwd) {
        return Err(MissingSessionCwdError::new(issue));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_session_file_returns_no_issue() {
        assert_eq!(
            get_missing_session_cwd_issue(None, "/nonexistent", "/cwd"),
            None
        );
    }

    #[test]
    fn existing_cwd_returns_no_issue() {
        let existing = String::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            get_missing_session_cwd_issue(Some("/s/1.jsonl"), &existing, "/cwd"),
            None
        );
    }

    #[test]
    fn missing_cwd_reports_issue() {
        let issue =
            get_missing_session_cwd_issue(Some("/s/1.jsonl"), "/definitely-not-here", "/current");
        let issue = issue.expect("should detect missing cwd");
        assert_eq!(issue.session_cwd, "/definitely-not-here");
        assert_eq!(issue.fallback_cwd, "/current");
        assert_eq!(issue.session_file.as_deref(), Some("/s/1.jsonl"));
    }

    #[test]
    fn error_format_lists_both_dirs() {
        let issue = SessionCwdIssue {
            session_file: Some("/s/1.jsonl".into()),
            session_cwd: "/gone".into(),
            fallback_cwd: "/here".into(),
        };
        let msg = format_missing_session_cwd_error(&issue);
        assert!(msg.contains("Stored session working directory does not exist: /gone"));
        assert!(msg.contains("Session file: /s/1.jsonl"));
        assert!(msg.contains("Current working directory: /here"));
    }

    #[test]
    fn prompt_format_lists_choice() {
        let issue = SessionCwdIssue {
            session_file: None,
            session_cwd: "/gone".into(),
            fallback_cwd: "/here".into(),
        };
        let msg = format_missing_session_cwd_prompt(&issue);
        assert!(msg.contains("cwd from session file does not exist"));
        assert!(msg.contains("/gone"));
        assert!(msg.contains("continue in current cwd\n/here"));
    }

    #[test]
    fn assert_returns_err_on_missing_and_ok_otherwise() {
        assert!(assert_session_cwd_exists(Some("f"), "/nope", "/cwd").is_err());
        assert!(assert_session_cwd_exists(Some("f"), env!("CARGO_MANIFEST_DIR"), "/cwd").is_ok());
        assert!(assert_session_cwd_exists(None, "/nope", "/cwd").is_ok());
    }
}
