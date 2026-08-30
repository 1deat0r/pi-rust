//! Cargo-native acceptance-index checks for the exhaustive behavioral parity
//! campaign. This deliberately does not reuse the conversion ledger: source
//! reconciliation and observable product behavior are different contracts.

use regex::Regex;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, String>;

const INVENTORY: &str = "docs/EXHAUSTIVE-PARITY-INVENTORY.md";
const TUI_STATUS: &str = "docs/TUI-PARITY-STATUS.md";
const NON_TUI_STATUS: &str = "docs/NON-TUI-PARITY-STATUS.md";
const DASHBOARD: &str = "docs/PARITY-DASHBOARD.md";
const LEDGER: &str = "CONVERSION-LEDGER.md";
const ROOT_GATES: &str = ".unlazy/parity-20260827/GATES.md";
const DEFAULT_SCOPE: &str = ".unlazy/parity-20260827";
const INVENTORY_TOTAL: usize = 318;
const TUI_TOTAL: usize = 52;
const NON_TUI_TOTAL: usize = INVENTORY_TOTAL - TUI_TOTAL;
const UPSTREAM_COMMIT: &str = "5cd93f688aaab89dbb6dfa4aca535f21796ae185";
const SCRIPT_EXTENSIONS: [&str; 6] = ["js", "jsx", "mjs", "cjs", "ts", "tsx"];

fn repo_root() -> PathBuf {
    if let Ok(root) = env::var("PI_RUST_ROOT") {
        if Path::new(&root).join("Cargo.toml").is_file() {
            return PathBuf::from(root);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(root: &Path, relative: &str) -> Result<String> {
    fs::read_to_string(root.join(relative))
        .map_err(|error| format!("{}: {error}", root.join(relative).display()))
}

fn count_files(root: &Path) -> Result<usize> {
    let mut count = 0;
    let entries = fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path)?;
        } else if path.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn upstream_root(root: &Path) -> PathBuf {
    env::var_os("PI_UPSTREAM_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("../pi-rust-s1-audit.KMw0N2/upstream_pi"))
}

fn parity_scope(root: &Path) -> PathBuf {
    env::var_os("PI_PARITY_SCOPE")
        .map(PathBuf::from)
        .map(|scope| {
            if scope.is_absolute() {
                scope
            } else {
                root.join(scope)
            }
        })
        .unwrap_or_else(|| root.join(DEFAULT_SCOPE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InventoryMetrics {
    ids: usize,
    upstream_files: usize,
    rust_files: usize,
}

fn inventory_metrics(root: &Path) -> Result<InventoryMetrics> {
    let source = read(root, INVENTORY)?;
    // Only the first column of a capability table row is an inventory ID.
    // Searching the whole document also counts historical/checkpoint prose
    // such as "CLI-005..CLI-011" and falsely reports duplicate capabilities.
    let ids: std::collections::BTreeSet<_> = inventory_capabilities(&source)?.into_keys().collect();
    if ids.len() < 200 {
        return Err(format!(
            "inventory contains only {} capability IDs; expected at least 200",
            ids.len()
        ));
    }
    for heading in [
        "## A. Product launch",
        "## B. Environment",
        "## C. Authentication",
        "## D. AI transport",
        "## E. Agent loop",
        "## F. Session",
        "## G. Interactive TUI",
        "## H. Text",
        "## I. Extensions",
        "## J. Cross-cutting",
    ] {
        if !source.contains(heading) {
            return Err(format!("inventory is missing required section: {heading}"));
        }
    }
    let upstream = upstream_root(root);
    let upstream_files = count_files(&upstream.join("packages"))?;
    let rust_files = count_files(&root.join("crates"))?;
    if upstream_files < 1000 || rust_files < 100 {
        return Err(format!(
            "source census unexpectedly small: upstream={upstream_files}, rust={rust_files}"
        ));
    }
    Ok(InventoryMetrics {
        ids: ids.len(),
        upstream_files,
        rust_files,
    })
}

fn inventory(root: &Path) -> Result<String> {
    let metrics = inventory_metrics(root)?;
    println!(
        "PARITY_INVENTORY_OK ids={} upstream_files={} rust_files={}",
        metrics.ids, metrics.upstream_files, metrics.rust_files
    );
    Ok("ok".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InventoryCapability {
    domain: &'static str,
    capability: String,
    acceptance: String,
}

fn capability_domain(id: &str) -> Option<&'static str> {
    let (prefix, number) = id.split_once('-')?;
    if number.len() != 3 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match prefix {
        "CLI" => Some("A"),
        "ENV" | "CFG" | "RES" => Some("B"),
        "AUTH" | "MODEL" | "PROV" => Some("C"),
        "AI" => Some("D"),
        "AGENT" | "TOOL" | "TRUST" => Some("E"),
        "SES" => Some("F"),
        "TUI" => Some("G"),
        "MODE" | "RPC" | "PROTO" | "SERVER" | "CLIENT" | "BACKEND" => Some("H"),
        "EXT" | "PKG" | "EVAL" | "DIST" => Some("I"),
        "X" => Some("J"),
        _ => None,
    }
}

fn markdown_row_fields(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return None;
    }
    Some(
        trimmed[1..trimmed.len() - 1]
            .split('|')
            .map(str::trim)
            .collect(),
    )
}

fn inventory_capabilities(
    source: &str,
) -> Result<std::collections::BTreeMap<String, InventoryCapability>> {
    let mut capabilities = std::collections::BTreeMap::new();
    for line in source.lines() {
        let Some(fields) = markdown_row_fields(line) else {
            continue;
        };
        let Some(id) = fields.first() else {
            continue;
        };
        let Some(domain) = capability_domain(id) else {
            continue;
        };
        if fields.len() > 3 {
            return Err(format!(
                "inventory row {id} has {} columns; expected capability and optional acceptance",
                fields.len()
            ));
        }
        let capability = fields
            .get(1)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("inventory row {id} has no capability text"))?;
        let record = InventoryCapability {
            domain,
            capability: (*capability).to_string(),
            acceptance: fields
                .get(2)
                .copied()
                .filter(|value| !value.is_empty())
                .unwrap_or(capability)
                .to_string(),
        };
        if capabilities.insert((*id).to_string(), record).is_some() {
            return Err(format!("inventory contains duplicate capability row: {id}"));
        }
    }
    Ok(capabilities)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceMetrics {
    checked: usize,
    total: usize,
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
fn source_metrics(root: &Path) -> Result<SourceMetrics> {
    let source = read(root, LEDGER)?;
    let checklist = Regex::new(r"^- \[[^\]]*\]").map_err(|error| error.to_string())?;
    let task = Regex::new(r"^- \[([ xX])\] (\d+\.|S-\d+)\s+").map_err(|error| error.to_string())?;
    let mut ids = std::collections::BTreeSet::new();
    let mut checked = 0;
    for (index, line) in source.lines().enumerate() {
        if !checklist.is_match(line) {
            continue;
        }
        let captures = task
            .captures(line)
            .ok_or_else(|| format!("malformed conversion task at line {}", index + 1))?;
        let raw_id = captures.get(2).unwrap().as_str();
        let id = raw_id.strip_suffix('.').unwrap_or(raw_id).to_string();
        if !ids.insert(id.clone()) {
            return Err(format!("duplicate conversion task id: {id}"));
        }
        if captures.get(1).unwrap().as_str().eq_ignore_ascii_case("x") {
            checked += 1;
        }
    }

    let expected: std::collections::BTreeSet<_> = (1..=100)
        .map(|id| id.to_string())
        .chain((1..=66).map(|id| format!("S-{id:03}")))
        .collect();
    if ids != expected {
        let missing = expected.difference(&ids).cloned().collect::<Vec<_>>();
        let unexpected = ids.difference(&expected).cloned().collect::<Vec<_>>();
        return Err(format!(
            "source ledger ID universe mismatch: found {}; missing={}; unexpected={}",
            ids.len(),
            if missing.is_empty() {
                "none".to_string()
            } else {
                missing.join(",")
            },
            if unexpected.is_empty() {
                "none".to_string()
            } else {
                unexpected.join(",")
            }
        ));
    }
    Ok(SourceMetrics {
        checked,
        total: ids.len(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GateMetrics {
    passed: usize,
    total: usize,
}

fn root_gate_metrics(root: &Path) -> Result<GateMetrics> {
    let source = read(root, ROOT_GATES)?;
    let mut passed = 0;
    let mut total = 0;
    for line in source.lines().map(str::trim_start) {
        let Some(rest) = line.strip_prefix("- [") else {
            continue;
        };
        let Some(status) = rest.chars().next() else {
            continue;
        };
        if !matches!(status, ' ' | 'x' | 'X') || !rest[1..].starts_with("] ") {
            continue;
        }
        total += 1;
        passed += usize::from(status.eq_ignore_ascii_case(&'x'));
    }
    if total == 0 {
        return Err(format!("no root acceptance gates found in {ROOT_GATES}"));
    }
    Ok(GateMetrics { passed, total })
}

fn workspace_script_sources(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                // `doc/` is generated Rustdoc output. Its JavaScript search
                // assets are not shipped application source or a runtime.
                if matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "upstream_pi" | "doc")
                ) {
                    continue;
                }
                visit(&path, files)?;
            } else if kind.is_file()
                && SCRIPT_EXTENSIONS.contains(
                    &path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .unwrap_or_default(),
                )
            {
                files.push(path);
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, &mut files).map_err(|error| error.to_string())?;
    files.sort();
    Ok(files)
}

fn rust_distribution_boundary(root: &Path) -> Result<(bool, usize)> {
    let files = workspace_script_sources(root)?;
    Ok((files.is_empty(), files.len()))
}

fn domains(root: &Path) -> Result<String> {
    let scope = parity_scope(root);
    let plan = fs::read_to_string(scope.join("PLAN.md"))
        .map_err(|error| format!("{}: {error}", scope.join("PLAN.md").display()))?;
    let leaves: Vec<_> = plan
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split('|').map(str::trim).collect();
            (fields.len() >= 4 && fields[1].starts_with("leaf-") && fields[1] != "leaf-id")
                .then(|| (fields[1].to_string(), fields[2].to_string()))
        })
        .collect();
    if leaves.is_empty() {
        return Err(format!(
            "no leaf rows found in {}",
            scope.join("PLAN.md").display()
        ));
    }
    if leaves
        .iter()
        .any(|(_, state)| !matches!(state.as_str(), "VERIFIED"))
    {
        let open = leaves
            .iter()
            .filter(|(_, state)| state != "VERIFIED")
            .map(|(id, state)| format!("{id}={state}"))
            .collect::<Vec<_>>();
        return Err(format!("unverified parity leaves: {}", open.join(", ")));
    }

    let mut gate_paths = vec![scope.join("GATES.md")];
    let gate_dir = scope.join("gates");
    for entry in
        fs::read_dir(&gate_dir).map_err(|error| format!("{}: {error}", gate_dir.display()))?
    {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            gate_paths.push(path);
        }
    }
    for path in gate_paths {
        let contents =
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if contents.contains("EVIDENCE: pending") {
            return Err(format!(
                "gate still contains pending evidence: {}",
                path.display()
            ));
        }
        if contents.contains("[ ]") {
            return Err(format!(
                "gate still contains unchecked items: {}",
                path.display()
            ));
        }
    }
    println!(
        "PARITY_DOMAINS_OK scope={} leaves={}",
        scope.display(),
        leaves.len()
    );
    Ok("ok".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParityStatus {
    Pass,
    Partial,
    Open,
}

impl ParityStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "PASS" => Some(Self::Pass),
            "PARTIAL" => Some(Self::Partial),
            "OPEN" => Some(Self::Open),
            _ => None,
        }
    }

    fn record(self, counts: &mut StatusCounts) {
        match self {
            Self::Pass => counts.pass += 1,
            Self::Partial => counts.partial += 1,
            Self::Open => counts.open += 1,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StatusCounts {
    pass: usize,
    partial: usize,
    open: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NonTuiMetrics {
    total: usize,
    implementation: StatusCounts,
    evidence: StatusCounts,
    boundary: StatusCounts,
    overall: usize,
}

fn parse_non_tui_status_rows(
    source: &str,
    inventory: &std::collections::BTreeMap<String, InventoryCapability>,
) -> Result<NonTuiMetrics> {
    let expected: std::collections::BTreeSet<_> = inventory
        .iter()
        .filter(|(id, _)| !id.starts_with("TUI-"))
        .map(|(id, _)| id.as_str())
        .collect();
    if expected.is_empty() {
        return Err("inventory has no non-TUI capability rows".to_string());
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut implementation = StatusCounts::default();
    let mut evidence = StatusCounts::default();
    let mut boundary = StatusCounts::default();
    let mut overall = 0;

    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        let Some(fields) = markdown_row_fields(trimmed) else {
            continue;
        };
        let Some(id) = fields.first() else {
            continue;
        };
        if capability_domain(id).is_none() {
            continue;
        }
        if id.starts_with("TUI-") {
            return Err(format!("non-TUI status register contains TUI row: {id}"));
        }
        let expected_capability = inventory
            .get(*id)
            .ok_or_else(|| format!("non-TUI status register contains unknown ID: {id}"))?;
        if fields.len() != 7 {
            return Err(format!(
                "malformed non-TUI status row (expected 7 columns): {trimmed}"
            ));
        }
        if fields[1] != expected_capability.domain {
            return Err(format!(
                "non-TUI status row {id} has domain {}, expected {}",
                fields[1], expected_capability.domain
            ));
        }
        if fields[2] != expected_capability.capability {
            return Err(format!(
                "non-TUI status row {id} capability differs from inventory"
            ));
        }
        let implementation_status = ParityStatus::parse(fields[3])
            .ok_or_else(|| format!("invalid implementation status for {id}: {}", fields[3]))?;
        let evidence_status = ParityStatus::parse(fields[4])
            .ok_or_else(|| format!("invalid evidence status for {id}: {}", fields[4]))?;
        let boundary_status = ParityStatus::parse(fields[5])
            .ok_or_else(|| format!("invalid boundary status for {id}: {}", fields[5]))?;
        let note = fields[6];
        if note.is_empty() {
            return Err(format!("non-TUI status row has no evidence boundary: {id}"));
        }
        if !note.contains(UPSTREAM_COMMIT) || !note.contains("Required:") {
            return Err(format!(
                "non-TUI status row {id} must name pinned upstream commit and Required boundary"
            ));
        }
        let required_boundary = format!("Required: {}", expected_capability.acceptance);
        if !note.ends_with(&required_boundary) {
            return Err(format!(
                "non-TUI status row {id} does not preserve the inventory acceptance boundary"
            ));
        }
        if !seen.insert(*id) {
            return Err(format!("duplicate non-TUI capability ID: {id}"));
        }

        implementation_status.record(&mut implementation);
        evidence_status.record(&mut evidence);
        boundary_status.record(&mut boundary);
        overall += usize::from(
            implementation_status == ParityStatus::Pass
                && evidence_status == ParityStatus::Pass
                && boundary_status == ParityStatus::Pass,
        );
    }

    let missing: Vec<_> = expected
        .iter()
        .filter(|id| !seen.contains(*id))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "non-TUI status register has {} rows; missing: {}",
            seen.len(),
            missing.join(", ")
        ));
    }

    Ok(NonTuiMetrics {
        total: expected.len(),
        implementation,
        evidence,
        boundary,
        overall,
    })
}

fn parse_non_tui_status(source: &str, inventory_source: &str) -> Result<NonTuiMetrics> {
    let inventory = inventory_capabilities(inventory_source)?;
    if inventory.len() != INVENTORY_TOTAL {
        return Err(format!(
            "inventory capability rows changed: expected {INVENTORY_TOTAL}, found {}",
            inventory.len()
        ));
    }
    let non_tui = inventory
        .keys()
        .filter(|id| !id.starts_with("TUI-"))
        .count();
    if non_tui != NON_TUI_TOTAL {
        return Err(format!(
            "non-TUI inventory rows changed: expected {NON_TUI_TOTAL}, found {non_tui}"
        ));
    }
    parse_non_tui_status_rows(source, &inventory)
}

fn pinned_upstream_revision(root: &Path) -> Result<String> {
    let upstream = upstream_root(root);
    let output = Command::new("git")
        .arg("-C")
        .arg(&upstream)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| format!("read pinned upstream revision: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pinned upstream revision lookup failed: {}",
            output.status
        ));
    }
    let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if revision != UPSTREAM_COMMIT {
        return Err(format!(
            "pinned upstream revision mismatch: expected {UPSTREAM_COMMIT}, found {revision}"
        ));
    }
    Ok(revision)
}

fn non_tui_metric_lines(metrics: NonTuiMetrics) -> [String; 5] {
    let percentage = |passed: usize| -> String {
        format!(
            "{:.2}% ({passed}/{})",
            passed as f64 * 100.0 / metrics.total as f64,
            metrics.total
        )
    };
    let dimension = |label: &str, counts: StatusCounts| {
        format!(
            "{label}: {:.2}% ({}/{} PASS; {} PARTIAL; {} OPEN)",
            counts.pass as f64 * 100.0 / metrics.total as f64,
            counts.pass,
            metrics.total,
            counts.partial,
            counts.open
        )
    };
    [
        format!(
            "Non-TUI normalized register coverage: {:.2}% ({}/{})",
            metrics.total as f64 * 100.0 / NON_TUI_TOTAL as f64,
            metrics.total,
            NON_TUI_TOTAL
        ),
        dimension("Non-TUI implementation parity", metrics.implementation),
        dimension("Non-TUI deterministic evidence parity", metrics.evidence),
        dimension("Non-TUI runtime-boundary parity", metrics.boundary),
        format!("Non-TUI overall parity: {}", percentage(metrics.overall)),
    ]
}

fn register(root: &Path) -> Result<String> {
    let inventory_source = read(root, INVENTORY)?;
    let status_source = read(root, NON_TUI_STATUS)?;
    let metrics = parse_non_tui_status(&status_source, &inventory_source)?;
    let upstream = pinned_upstream_revision(root)?;
    for line in non_tui_metric_lines(metrics) {
        if !status_source
            .lines()
            .any(|candidate| candidate.trim() == line)
        {
            return Err(format!(
                "non-TUI status register has stale or missing generated metric: {line}"
            ));
        }
        println!("{line}");
    }
    println!(
        "PARITY_REGISTER_OK upstream={} rows={} implementation_pass={} implementation_partial={} implementation_open={} evidence_pass={} evidence_partial={} evidence_open={} boundary_pass={} boundary_partial={} boundary_open={} overall={}/{}",
        upstream,
        metrics.total,
        metrics.implementation.pass,
        metrics.implementation.partial,
        metrics.implementation.open,
        metrics.evidence.pass,
        metrics.evidence.partial,
        metrics.evidence.open,
        metrics.boundary.pass,
        metrics.boundary.partial,
        metrics.boundary.open,
        metrics.overall,
        metrics.total,
    );
    Ok("ok".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TuiMetrics {
    total: usize,
    functional: usize,
    evidence: usize,
    visual: usize,
    overall: usize,
}

fn tui_status_passes(value: &str) -> bool {
    matches!(value, "PASS" | "PARTIAL" | "OPEN")
}

fn parse_tui_status(source: &str) -> Result<TuiMetrics> {
    let mut seen = std::collections::BTreeSet::new();
    let mut functional = 0;
    let mut evidence = 0;
    let mut visual = 0;
    let mut overall = 0;

    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("| TUI-") {
            continue;
        }
        let fields: Vec<_> = trimmed.split('|').map(str::trim).collect();
        if fields.len() != 8 {
            return Err(format!(
                "malformed TUI status row (expected 6 columns): {trimmed}"
            ));
        }
        let id = fields[1];
        let number = id
            .strip_prefix("TUI-")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=TUI_TOTAL).contains(value))
            .ok_or_else(|| format!("invalid TUI capability id: {id}"))?;
        if !seen.insert(number) {
            return Err(format!("duplicate TUI capability id: {id}"));
        }
        let functional_status = fields[3];
        let evidence_status = fields[4];
        let visual_status = fields[5];
        if !tui_status_passes(functional_status)
            || !tui_status_passes(evidence_status)
            || !tui_status_passes(visual_status)
        {
            return Err(format!("invalid TUI status row: {trimmed}"));
        }
        if fields[6].is_empty() {
            return Err(format!("TUI status row has no evidence note: {id}"));
        }
        let functional_pass = functional_status == "PASS";
        let evidence_pass = evidence_status == "PASS";
        let visual_pass = visual_status == "PASS";
        functional += usize::from(functional_pass);
        evidence += usize::from(evidence_pass);
        visual += usize::from(visual_pass);
        overall += usize::from(functional_pass && evidence_pass && visual_pass);
    }

    if seen.len() != TUI_TOTAL {
        let missing: Vec<_> = (1..=TUI_TOTAL)
            .filter(|number| !seen.contains(number))
            .map(|number| format!("TUI-{number:03}"))
            .collect();
        return Err(format!(
            "TUI status register has {} rows; missing: {}",
            seen.len(),
            missing.join(", ")
        ));
    }

    Ok(TuiMetrics {
        total: seen.len(),
        functional,
        evidence,
        visual,
        overall,
    })
}

fn tui_metric_lines(metrics: TuiMetrics) -> [String; 4] {
    let percentage = |passed: usize| -> String {
        format!(
            "{:.2}% ({passed}/{})",
            passed as f64 * 100.0 / metrics.total as f64,
            metrics.total
        )
    };
    [
        format!(
            "TUI functional implementation: {}",
            percentage(metrics.functional)
        ),
        format!("TUI test/evidence parity: {}", percentage(metrics.evidence)),
        format!(
            "TUI visual/interaction parity: {}",
            percentage(metrics.visual)
        ),
        format!("TUI overall parity: {}", percentage(metrics.overall)),
    ]
}

fn dashboard_metric_lines(
    source: SourceMetrics,
    inventory: InventoryMetrics,
    gates: GateMetrics,
    tui: TuiMetrics,
    non_tui: NonTuiMetrics,
) -> Vec<String> {
    let percentage = |passed: usize, total: usize| {
        format!(
            "{:.2}% ({passed}/{total})",
            passed as f64 * 100.0 / total as f64
        )
    };
    let scored = tui.total + non_tui.total;
    let whole_product_overall = tui.overall + non_tui.overall;
    let mut lines = vec![
        format!(
            "Source/conversion ledger: {:.2}% ({}/{}; {} open)",
            source.checked as f64 * 100.0 / source.total as f64,
            source.checked,
            source.total,
            source.total - source.checked
        ),
        format!(
            "Acceptance inventory census: {} ({} IDs indexed)",
            percentage(inventory.ids, INVENTORY_TOTAL),
            inventory.ids
        ),
        format!(
            "Acceptance scoring coverage: {} ({} of {} IDs scored)",
            percentage(scored, inventory.ids),
            scored,
            inventory.ids
        ),
        format!(
            "Root acceptance gates: {} ({} passed; {} open)",
            percentage(gates.passed, gates.total),
            gates.passed,
            gates.total - gates.passed
        ),
    ];
    lines.extend(tui_metric_lines(tui));
    lines.extend(non_tui_metric_lines(non_tui).into_iter().skip(1));
    lines.push(format!(
        "Whole-product behavioral parity: {}",
        percentage(whole_product_overall, inventory.ids)
    ));
    lines
}

fn dashboard(root: &Path) -> Result<String> {
    let source = source_metrics(root)?;
    let inventory = inventory_metrics(root)?;
    if inventory.ids != INVENTORY_TOTAL {
        return Err(format!(
            "inventory census changed: expected {INVENTORY_TOTAL} IDs, found {}",
            inventory.ids
        ));
    }
    let inventory_source = read(root, INVENTORY)?;
    let non_tui_source = read(root, NON_TUI_STATUS)?;
    let non_tui = parse_non_tui_status(&non_tui_source, &inventory_source)?;
    let upstream = pinned_upstream_revision(root)?;
    let gates = root_gate_metrics(root)?;
    let tui_source = read(root, TUI_STATUS)?;
    let tui = parse_tui_status(&tui_source)?;
    let (rust_only, script_count) = rust_distribution_boundary(root)?;
    let scored = tui.total + non_tui.total;
    let mut metric_lines = dashboard_metric_lines(source, inventory, gates, tui, non_tui);
    metric_lines.insert(
        4,
        format!(
            "Rust-only distribution boundary: {} ({} JS/TS executable source files; generated Rustdoc excluded)",
            if rust_only { "100.00%" } else { "0.00%" },
            script_count
        ),
    );
    if !rust_only {
        return Err(format!(
            "Rust-only distribution boundary failed: {script_count} JS/TS executable source files"
        ));
    }
    let dashboard_source = read(root, DASHBOARD)?;
    for line in &metric_lines {
        if !dashboard_source
            .lines()
            .any(|candidate| candidate.trim() == line)
        {
            return Err(format!(
                "parity dashboard has stale or missing generated metric: {line}"
            ));
        }
        println!("{line}");
    }
    println!(
        "PARITY_DASHBOARD_OK source={}/{} inventory={}/{} scored={}/{} tui_overall={}/{} non_tui_register={}/{} upstream={} gates={}/{}",
        source.checked,
        source.total,
        inventory.ids,
        INVENTORY_TOTAL,
        scored,
        inventory.ids,
        tui.overall,
        tui.total,
        non_tui.total,
        NON_TUI_TOTAL,
        upstream,
        gates.passed,
        gates.total
    );
    Ok("ok".to_string())
}

fn tui(root: &Path) -> Result<String> {
    let source = read(root, TUI_STATUS)?;
    let metrics = parse_tui_status(&source)?;
    let metric_lines = tui_metric_lines(metrics);
    for line in &metric_lines {
        if !source.lines().any(|candidate| candidate.trim() == line) {
            return Err(format!(
                "TUI status register has stale or missing generated metric: {line}"
            ));
        }
        println!("{line}");
    }
    println!(
        "PARITY_TUI_OK rows={} functional={} evidence={} visual={} overall={}",
        metrics.total, metrics.functional, metrics.evidence, metrics.visual, metrics.overall
    );
    Ok("ok".to_string())
}

fn installed(root: &Path) -> Result<String> {
    let binary = root.join("target/release/pi");
    if !binary.is_file() {
        return Err(format!("release binary is missing: {}", binary.display()));
    }
    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .map_err(|error| format!("run {}: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!(
            "release binary --version failed: {}",
            output.status
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout);
    if !version.trim_start().starts_with("pi ") {
        return Err(format!("unexpected release version output: {version:?}"));
    }
    let release_path = fs::canonicalize(&binary)
        .map_err(|error| format!("canonicalize release binary: {error}"))?;
    let path_for = |command_name: &str| -> Result<PathBuf> {
        let path_output = Command::new("sh")
            .args(["-c", &format!("command -v {command_name}")])
            .output()
            .map_err(|error| format!("resolve PATH {command_name}: {error}"))?;
        if !path_output.status.success() {
            return Err(format!("no {command_name} command is available on PATH"));
        }
        let command_path = String::from_utf8_lossy(&path_output.stdout)
            .trim()
            .to_string();
        fs::canonicalize(&command_path)
            .map_err(|error| format!("canonicalize PATH {command_name}: {error}"))
    };
    let rust_path = path_for("pi-rust")?;
    if rust_path != release_path {
        return Err(format!(
            "PATH pi-rust resolves to {}, expected {}",
            rust_path.display(),
            release_path.display()
        ));
    }
    let mut file =
        fs::File::open(&rust_path).map_err(|error| format!("open PATH pi-rust: {error}"))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .map_err(|error| format!("read PATH pi-rust: {error}"))?;
    if magic != *b"\x7fELF" {
        return Err("PATH pi-rust is not an ELF Rust release binary".to_string());
    }

    let official_path = path_for("pi")?;
    if official_path == release_path {
        return Err(
            "PATH pi is shadowed by the pi-rust release binary; official Pi must remain separate"
                .to_string(),
        );
    }
    let official_output = Command::new(&official_path)
        .arg("--version")
        .output()
        .map_err(|error| format!("run official Pi {}: {error}", official_path.display()))?;
    if !official_output.status.success() {
        return Err(format!(
            "official Pi {} --version failed: {}",
            official_path.display(),
            official_output.status
        ));
    }
    let official_version = String::from_utf8_lossy(&official_output.stdout)
        .trim()
        .to_string();
    if official_version.is_empty() {
        return Err("official Pi --version returned no version".to_string());
    }
    println!(
        "PARITY_INSTALLED_RUST_OK command=pi-rust release_version={} official_pi_version={}",
        version.trim(),
        official_version
    );
    Ok("ok".to_string())
}

fn main() -> Result<()> {
    let command = env::args()
        .nth(1)
        .unwrap_or_else(|| "inventory".to_string());
    let root = repo_root();
    match command.as_str() {
        "inventory" => inventory(&root).map(|_| ()),
        "domains" => domains(&root).map(|_| ()),
        "register" => register(&root).map(|_| ()),
        "tui" => tui(&root).map(|_| ()),
        "dashboard" => dashboard(&root).map(|_| ()),
        "installed" => installed(&root).map(|_| ()),
        other => Err(format!(
            "unknown parity_audit command {other:?}; expected inventory, domains, register, tui, dashboard, or installed"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        dashboard_metric_lines, inventory_capabilities, parse_non_tui_status,
        parse_non_tui_status_rows, parse_tui_status, rust_distribution_boundary, tui_metric_lines,
        GateMetrics, InventoryMetrics, NonTuiMetrics, SourceMetrics, StatusCounts, NON_TUI_TOTAL,
        TUI_TOTAL, UPSTREAM_COMMIT,
    };

    fn row(number: usize, functional: &str, evidence: &str, visual: &str) -> String {
        format!(
            "| TUI-{number:03} | capability | {functional} | {evidence} | {visual} | evidence note |"
        )
    }

    fn valid_register() -> String {
        (1..=TUI_TOTAL)
            .map(|number| row(number, "PASS", "PASS", "OPEN"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn non_tui_row(
        id: &str,
        domain: &str,
        capability: &str,
        implementation: &str,
        evidence: &str,
        boundary: &str,
    ) -> String {
        format!(
            "| {id} | {domain} | {capability} | {implementation} | {evidence} | {boundary} | Pinned upstream commit {UPSTREAM_COMMIT}; Required: {capability} |"
        )
    }

    fn synthetic_non_tui_inventory() -> String {
        [
            "| ID | Capability | Required acceptance |",
            "|---|---|---|",
            "| CLI-001 | cli behavior | cli behavior |",
            "| ENV-001 | environment behavior | environment behavior |",
        ]
        .join("\n")
    }

    #[test]
    fn parses_all_tui_rows_and_calculates_overall_only_from_three_passes() {
        let metrics = parse_tui_status(&valid_register()).expect("valid register");
        assert_eq!(metrics.total, TUI_TOTAL);
        assert_eq!(metrics.functional, TUI_TOTAL);
        assert_eq!(metrics.evidence, TUI_TOTAL);
        assert_eq!(metrics.visual, 0);
        assert_eq!(metrics.overall, 0);
        assert_eq!(
            tui_metric_lines(metrics)[0],
            "TUI functional implementation: 100.00% (52/52)"
        );
    }

    #[test]
    fn rejects_duplicate_and_missing_rows() {
        let mut source = valid_register();
        source.push('\n');
        source.push_str(&row(1, "PASS", "PASS", "PASS"));
        let error = parse_tui_status(&source).expect_err("duplicate must fail");
        assert!(error.contains("duplicate TUI capability id"));
    }

    #[test]
    fn rejects_unknown_status_values() {
        let mut source = valid_register();
        source = source.replacen("| PASS | PASS | OPEN |", "| DONE | PASS | OPEN |", 1);
        let error = parse_tui_status(&source).expect_err("unknown status must fail");
        assert!(error.contains("invalid TUI status row"));
    }

    #[test]
    fn calculates_non_tui_dimensions_without_promoting_partial_work() {
        let inventory = inventory_capabilities(&synthetic_non_tui_inventory()).expect("inventory");
        let source = [
            non_tui_row("CLI-001", "A", "cli behavior", "PASS", "PARTIAL", "OPEN"),
            non_tui_row(
                "ENV-001",
                "B",
                "environment behavior",
                "OPEN",
                "OPEN",
                "OPEN",
            ),
        ]
        .join("\n");
        let metrics = parse_non_tui_status_rows(&source, &inventory).expect("status register");
        assert_eq!(metrics.total, 2);
        assert_eq!(metrics.implementation.pass, 1);
        assert_eq!(metrics.implementation.open, 1);
        assert_eq!(metrics.evidence.partial, 1);
        assert_eq!(metrics.evidence.open, 1);
        assert_eq!(metrics.boundary.open, 2);
        assert_eq!(metrics.overall, 0);
    }

    #[test]
    fn rejects_non_tui_capability_drift_and_missing_residual_boundary() {
        let inventory = inventory_capabilities(&synthetic_non_tui_inventory()).expect("inventory");
        let source = non_tui_row("CLI-001", "A", "changed capability", "OPEN", "OPEN", "OPEN");
        let error = parse_non_tui_status_rows(&source, &inventory).expect_err("drift must fail");
        assert!(error.contains("capability differs from inventory"));

        let source = "| CLI-001 | A | cli behavior | OPEN | OPEN | OPEN | no pinned oracle |";
        let error = parse_non_tui_status_rows(source, &inventory).expect_err("boundary must fail");
        assert!(error.contains("must name pinned upstream commit"));
    }

    #[test]
    fn current_non_tui_register_covers_all_inventory_rows_with_normalized_status() {
        let root = super::repo_root();
        let inventory = super::read(&root, super::INVENTORY).expect("inventory document");
        let register = super::read(&root, super::NON_TUI_STATUS).expect("status register");
        let metrics = parse_non_tui_status(&register, &inventory).expect("current register");
        assert_eq!(metrics.total, NON_TUI_TOTAL);
        assert_eq!(metrics.implementation.pass, 49);
        assert_eq!(metrics.implementation.partial, 194);
        assert_eq!(metrics.implementation.open, 23);
        assert_eq!(metrics.evidence.pass, 36);
        assert_eq!(metrics.evidence.partial, 207);
        assert_eq!(metrics.evidence.open, 23);
        assert_eq!(metrics.boundary.pass, 37);
        assert_eq!(metrics.boundary.partial, 154);
        assert_eq!(metrics.boundary.open, 75);
        assert_eq!(metrics.overall, 30);
    }

    #[test]
    fn dashboard_keeps_scoring_coverage_separate_from_completion() {
        let tui = parse_tui_status(&valid_register()).expect("valid register");
        let lines = dashboard_metric_lines(
            SourceMetrics {
                checked: 166,
                total: 166,
            },
            InventoryMetrics {
                ids: 318,
                upstream_files: 1_310,
                rust_files: 484,
            },
            GateMetrics {
                passed: 7,
                total: 8,
            },
            tui,
            NonTuiMetrics {
                total: NON_TUI_TOTAL,
                implementation: StatusCounts {
                    pass: 0,
                    partial: 0,
                    open: NON_TUI_TOTAL,
                },
                evidence: StatusCounts {
                    pass: 0,
                    partial: 0,
                    open: NON_TUI_TOTAL,
                },
                boundary: StatusCounts {
                    pass: 0,
                    partial: 0,
                    open: NON_TUI_TOTAL,
                },
                overall: 0,
            },
        );
        assert!(lines
            .iter()
            .any(|line| { line == "Source/conversion ledger: 100.00% (166/166; 0 open)" }));
        assert!(lines
            .iter()
            .any(|line| line
                == "Acceptance scoring coverage: 100.00% (318/318) (318 of 318 IDs scored)"));
        assert!(lines
            .iter()
            .any(|line| { line == "Whole-product behavioral parity: 0.00% (0/318)" }));
    }

    #[test]
    fn generated_rustdoc_javascript_is_outside_the_distribution_boundary() {
        let (rust_only, script_count) =
            rust_distribution_boundary(&super::repo_root()).expect("workspace census");
        assert!(rust_only);
        assert_eq!(script_count, 0);
    }
}
