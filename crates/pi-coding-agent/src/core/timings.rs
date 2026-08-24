//! Startup timing compatibility surface.
//!
//! Upstream exposes a `PI_TIMING=1` namespace profiler. The Rust distribution
//! does not have an equivalent startup namespace, so it makes that deliberate
//! non-port visible and points users at the supported process-level fallback.

pub const PI_TIMING_FALLBACK_NOTICE: &str =
    "PI_TIMING=1 is not supported by the Rust distribution; use /usr/bin/time -p with the pi command for process-level startup timing.";

/// Return the user-facing compatibility notice for the upstream timing flag.
/// The upstream gate is exact: only the string `1` enables timing output.
pub fn unsupported_notice(value: Option<&str>) -> Option<&'static str> {
    (value == Some("1")).then_some(PI_TIMING_FALLBACK_NOTICE)
}

/// Read `PI_TIMING` from the process environment and return the fallback
/// notice when the unsupported upstream profiler was requested.
pub fn startup_notice() -> Option<&'static str> {
    unsupported_notice(std::env::var("PI_TIMING").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_upstream_exact_one_gate_and_fallback_text() {
        assert_eq!(unsupported_notice(None), None);
        assert_eq!(unsupported_notice(Some("0")), None);
        assert_eq!(unsupported_notice(Some("true")), None);
        assert_eq!(
            unsupported_notice(Some("1")),
            Some(PI_TIMING_FALLBACK_NOTICE)
        );
        assert!(PI_TIMING_FALLBACK_NOTICE.contains("/usr/bin/time -p"));
    }
}
