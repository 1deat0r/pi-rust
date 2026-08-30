//! Startup timing instrumentation — port of `core/timings.ts`.
//!
//! `PI_TIMING=1` intentionally uses a process-global, insertion-ordered list
//! of namespaces, matching the upstream profiler's output contract. Timing
//! is disabled without the exact value `1`, so ordinary stdout/stderr and
//! startup latency are unaffected.

use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

#[derive(Debug)]
struct TimingNamespace {
    timings: Vec<(String, u128)>,
    last: Instant,
}

static TIMING_NAMESPACES: OnceLock<Mutex<Vec<(String, TimingNamespace)>>> = OnceLock::new();
static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    // The upstream module captures this switch once when it is loaded. Keep
    // later environment mutations from changing an already-started process.
    enabled_value(std::env::var("PI_TIMING").ok().as_deref())
});

fn enabled_value(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Whether the upstream exact-on switch is enabled.
pub fn enabled() -> bool {
    *ENABLED
}

fn namespaces() -> &'static Mutex<Vec<(String, TimingNamespace)>> {
    TIMING_NAMESPACES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Start a fresh timing namespace.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn reset_timings(namespace: &str) {
    if !enabled() {
        return;
    }
    let mut namespaces = namespaces().lock().expect("timing mutex poisoned");
    if let Some((_, state)) = namespaces.iter_mut().find(|(name, _)| name == namespace) {
        state.timings.clear();
        state.last = Instant::now();
    } else {
        namespaces.push((
            namespace.to_string(),
            TimingNamespace {
                timings: Vec::new(),
                last: Instant::now(),
            },
        ));
    }
}

/// Record one elapsed startup segment in a namespace.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn time(label: impl Into<String>, namespace: &str) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let mut namespaces = namespaces().lock().expect("timing mutex poisoned");
    let state = if let Some((_, state)) = namespaces.iter_mut().find(|(name, _)| name == namespace)
    {
        state
    } else {
        namespaces.push((
            namespace.to_string(),
            TimingNamespace {
                timings: Vec::new(),
                last: now,
            },
        ));
        &mut namespaces
            .last_mut()
            .expect("timing namespace was just inserted")
            .1
    };
    let elapsed = now.duration_since(state.last).as_millis();
    state.timings.push((label.into(), elapsed));
    state.last = now;
}

fn timing_group_text(title: &str, timings: &[(String, u128)]) -> String {
    if timings.is_empty() {
        return String::new();
    }
    let mut output = format!("\n--- {title} ---\n");
    for (label, elapsed) in timings {
        output.push_str(&format!("  {label}: {elapsed}ms\n"));
    }
    let total: u128 = timings.iter().map(|(_, elapsed)| *elapsed).sum();
    output.push_str(&format!("  TOTAL: {total}ms\n"));
    output.push_str(&format!("{}\n\n", "-".repeat(title.len() + 8)));
    output
}

/// Print all recorded namespaces using the upstream text format.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn print_timings() {
    if !enabled() {
        return;
    }
    let namespaces = namespaces().lock().expect("timing mutex poisoned");
    for (namespace, state) in namespaces.iter() {
        let title = format!("Startup Timings: {namespace}");
        eprint!("{}", timing_group_text(&title, &state.timings));
    }
}

/// Test-only helper for the exact upstream enable gate.
#[cfg(test)]
fn enabled_for_test(value: Option<&str>) -> bool {
    enabled_value(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn matches_upstream_exact_one_gate() {
        assert!(!enabled_for_test(None));
        assert!(!enabled_for_test(Some("0")));
        assert!(!enabled_for_test(Some("true")));
        assert!(enabled_for_test(Some("1")));
    }

    #[test]
    fn timing_output_matches_upstream_shape() {
        let text = timing_group_text(
            "Startup Timings: main",
            &[("parseArgs".to_string(), 3), ("run".to_string(), 2)],
        );
        assert!(text.starts_with("\n--- Startup Timings: main ---\n"));
        assert!(text.contains("  parseArgs: 3ms\n"));
        assert!(text.contains("  TOTAL: 5ms\n"));
        assert!(text.ends_with("\n\n"));
    }
}
