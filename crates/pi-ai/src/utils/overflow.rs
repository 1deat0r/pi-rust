//! Context-window overflow classification — port of
//! `packages/ai/src/utils/overflow.ts`.
//!
//! Providers do not share an error schema.  Keeping this classifier beside
//! the provider-independent message types lets the coding-agent recovery
//! path make the same decision for native and OpenAI-compatible adaptors.

use std::sync::OnceLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt is too long",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \d+ tokens",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
    r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
];

const NON_OVERFLOW_PATTERNS: &[&str] = &[
    r"^(Throttling error|Service unavailable):",
    r"rate limit",
    r"too many requests",
];

fn patterns() -> &'static (Vec<Regex>, Vec<Regex>) {
    static PATTERNS: OnceLock<(Vec<Regex>, Vec<Regex>)> = OnceLock::new();
    #[allow(clippy::expect_used)] // invariant: static pattern literals compile
    PATTERNS.get_or_init(|| {
        let overflow = OVERFLOW_PATTERNS
            .iter()
            .map(|pattern| Regex::new(&format!("(?i){pattern}")))
            .collect::<Result<Vec<_>, _>>()
            .expect("overflow patterns are valid");
        let non_overflow = NON_OVERFLOW_PATTERNS
            .iter()
            .map(|pattern| Regex::new(&format!("(?i){pattern}")))
            .collect::<Result<Vec<_>, _>>()
            .expect("non-overflow patterns are valid");
        (overflow, non_overflow)
    })
}

fn input_tokens(message: &AssistantMessage) -> Option<u128> {
    let usage = message.usage()?;
    let input = u128::try_from(usage.input).ok()?;
    let cache_read = u128::try_from(usage.cache_read).ok()?;
    Some(input.saturating_add(cache_read))
}

/// Return whether an assistant message represents a context-window overflow.
///
/// `context_window` is optional because explicit provider error text is enough
/// for most APIs.  When supplied it additionally detects providers that accept
/// an oversized request silently or truncate it to a zero-output length stop.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason() == Some(StopReason::Error) {
        if let Some(error) = message.error_message() {
            let (overflow, non_overflow) = patterns();
            if !non_overflow.iter().any(|pattern| pattern.is_match(error))
                && overflow.iter().any(|pattern| pattern.is_match(error))
            {
                return true;
            }
        }
    }

    let Some(context_window) = context_window else {
        return false;
    };
    let Some(input_tokens) = input_tokens(message) else {
        return false;
    };
    let context_window = u128::from(context_window);

    if message.stop_reason() == Some(StopReason::Stop) && input_tokens > context_window {
        return true;
    }

    message.stop_reason() == Some(StopReason::Length)
        && message.usage().is_some_and(|usage| usage.output == 0)
        && input_tokens.saturating_mul(100) >= context_window.saturating_mul(99)
}

/// Whether a length stop ended below the caller's intended output limit and
/// is therefore eligible for one bounded compact-and-retry attempt.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason() == Some(StopReason::Length)
        && desired_max_output > 0
        && message
            .usage()
            .is_some_and(|usage| usage.output >= 0 && (usage.output as u64) < desired_max_output)
}

/// Return the compiled patterns for diagnostics/tests without exposing the
/// mutable global vectors.
pub fn overflow_pattern_count() -> usize {
    patterns().0.len()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::Usage;

    fn error(message: &str) -> AssistantMessage {
        let mut value = AssistantMessage::new();
        value.set_stop_reason(StopReason::Error);
        value.set_error_message(message);
        value
    }

    fn usage_message(
        reason: StopReason,
        input: i64,
        cache_read: i64,
        output: i64,
    ) -> AssistantMessage {
        let mut value = AssistantMessage::new();
        value.set_stop_reason(reason);
        value.set_usage(Usage {
            input,
            cache_read,
            output,
            total_tokens: input.saturating_add(cache_read).saturating_add(output),
            ..Default::default()
        });
        value
    }

    #[test]
    fn recognizes_provider_error_families() {
        let examples = [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "413 {\"error\":{\"type\":\"request_too_large\"}}",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "The input token count (1196265) exceeds the maximum number of tokens allowed",
            "This model's maximum prompt length is 131072",
            "Please reduce the length of the messages",
            "This endpoint's maximum context length is 128000 tokens",
            "The input (12 tokens) is longer than the model's context length 10 tokens.",
            "the request exceeds the available context size",
            "tokens to keep from the initial prompt is greater than the context length",
            "invalid params, context window exceeds limit",
            "Your request exceeded model token limit: 10",
            "Prompt contains 12 tokens and is too large for model with 10 maximum context length",
            "Prompt has 12 tokens, but the configured context size is 10 tokens",
            "model_context_window_exceeded",
            "prompt too long; exceeded max context length",
            "Range of input length should be [1, 10]",
            "context_length_exceeded",
            "too many tokens",
            "token limit exceeded",
            "413 status code (no body)",
        ];
        for example in examples {
            assert!(
                is_context_overflow(&error(example), None),
                "not classified: {example}"
            );
        }
    }

    #[test]
    fn excludes_rate_limits_and_throttling() {
        for example in [
            "Throttling error: too many tokens, please wait",
            "Service unavailable: token limit exceeded",
            "rate limit: too many tokens",
            "too many requests: token limit exceeded",
        ] {
            assert!(
                !is_context_overflow(&error(example), None),
                "misclassified: {example}"
            );
        }
    }

    #[test]
    fn detects_silent_and_length_stop_overflow() {
        let silent = usage_message(StopReason::Stop, 101, 0, 1);
        assert!(is_context_overflow(&silent, Some(100)));
        assert!(!is_context_overflow(&silent, Some(101)));

        let filled = usage_message(StopReason::Length, 99, 0, 0);
        assert!(is_context_overflow(&filled, Some(100)));
        let output = usage_message(StopReason::Length, 99, 0, 1);
        assert!(!is_context_overflow(&output, Some(100)));
        let partial = usage_message(StopReason::Length, 50, 0, 0);
        assert!(!is_context_overflow(&partial, Some(100)));
    }

    #[test]
    fn handles_missing_and_negative_usage_safely() {
        let mut no_usage = AssistantMessage::new();
        no_usage.set_stop_reason(StopReason::Stop);
        assert!(!is_context_overflow(&no_usage, Some(1)));
        let negative = usage_message(StopReason::Stop, -1, 2, 0);
        assert!(!is_context_overflow(&negative, Some(1)));
    }

    #[test]
    fn identifies_recoverable_length_stops() {
        let partial = usage_message(StopReason::Length, 1, 0, 4);
        assert!(is_recoverable_length(&partial, 8));
        assert!(!is_recoverable_length(&partial, 4));
        assert!(!is_recoverable_length(&partial, 0));
        let stop = usage_message(StopReason::Stop, 1, 0, 1);
        assert!(!is_recoverable_length(&stop, 8));
        assert!(overflow_pattern_count() >= 20);
    }
}
