//! Shared provider-error body normalization.
//!
//! Mirrors `packages/ai/src/utils/error-body.ts` for the Rust adaptors that
//! receive a raw HTTP response rather than an SDK error object.

pub(crate) const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4_000;

pub(crate) fn truncate_provider_error_text(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_PROVIDER_ERROR_BODY_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX_PROVIDER_ERROR_BODY_CHARS).collect();
    format!(
        "{truncated}... [truncated {} chars]",
        count - MAX_PROVIDER_ERROR_BODY_CHARS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_unicode_scalar_with_the_upstream_suffix() {
        let exact = "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS);
        assert_eq!(truncate_provider_error_text(&exact), exact);

        let oversized = format!("{}🦀tail", "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS));
        let truncated = truncate_provider_error_text(&oversized);
        assert!(truncated.starts_with(&"x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS)));
        assert!(truncated.ends_with("... [truncated 5 chars]"));
        assert!(!truncated.contains('🦀'));
    }
}
