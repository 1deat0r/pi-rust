//! Resource diagnostics — port of `packages/coding-agent/src/core/diagnostics.ts`.

/// A name collision between two resources (extension, skill, prompt, theme)
/// that share a name but load from different paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCollision {
    pub resource_type: &'static str,
    /// Skill name / command / tool / flag name / prompt name / theme name.
    pub name: String,
    pub winner_path: String,
    pub loser_path: String,
    /// e.g. `npm:foo`, `git:...`, `local`.
    pub winner_source: Option<String>,
    pub loser_source: Option<String>,
}

/// A diagnostic surfaced while loading resources into the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDiagnostic {
    pub kind: ResourceDiagnosticKind,
    pub message: String,
    pub path: Option<String>,
    pub collision: Option<ResourceCollision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceDiagnosticKind {
    Warning,
    Error,
    Collision,
}

impl ResourceDiagnostic {
    pub fn warning(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            kind: ResourceDiagnosticKind::Warning,
            message: message.into(),
            path: Some(path.into()),
            collision: None,
        }
    }
}
