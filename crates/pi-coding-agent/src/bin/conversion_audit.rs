//! Cargo-native conversion progress and source audit checks.

use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

const LEDGER: &str = "CONVERSION-LEDGER.md";
const CENSUS: &str = ".unlazy/full-conversion-20260825/audit/export-census.md";

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    checked: usize,
    total: usize,
}

impl Progress {
    fn output(self) -> String {
        format!(
            "Conversion progress: {:.2}% ({}/{}; {} open)",
            self.checked as f64 * 100.0 / self.total as f64,
            self.checked,
            self.total,
            self.total - self.checked
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct Task {
    checked: bool,
}

fn repo_root() -> PathBuf {
    if let Ok(root) = env::var("PI_RUST_ROOT") {
        if Path::new(&root).join(LEDGER).is_file() {
            return PathBuf::from(root);
        }
    }
    let mut current = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if current.join(LEDGER).is_file() && current.join("Cargo.toml").is_file() {
            return current;
        }
        if !current.pop() {
            break;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn expected_ids() -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for id in 1..=100 {
        ids.insert(id.to_string());
    }
    for id in 1..=66 {
        ids.insert(format!("S-{id:03}"));
    }
    ids
}

fn checklist() -> Regex {
    Regex::new(r"^- \[[^\]]*\]").unwrap()
}

fn task_line() -> Regex {
    Regex::new(r"^- \[([ xX])\] (\d+\.|S-\d+)\s+").unwrap()
}

fn parse_tasks(source: &str) -> Result<BTreeMap<String, Task>> {
    let checklist = checklist();
    let task_line = task_line();
    let mut tasks = BTreeMap::new();
    for (index, line) in source.lines().enumerate() {
        if !checklist.is_match(line) {
            continue;
        }
        let captures = task_line
            .captures(line)
            .ok_or_else(|| format!("malformed task checklist at line {}", index + 1))?;
        let raw_id = captures.get(2).unwrap().as_str();
        let id = raw_id.strip_suffix('.').unwrap_or(raw_id).to_string();
        if tasks.contains_key(&id) {
            return Err(format!("duplicate conversion task id: {id}"));
        }
        tasks.insert(
            id,
            Task {
                checked: captures.get(1).unwrap().as_str().eq_ignore_ascii_case("x"),
            },
        );
    }
    if tasks.is_empty() {
        return Err("no conversion tasks found".to_string());
    }
    Ok(tasks)
}

fn validate_universe(tasks: &BTreeMap<String, Task>) -> Result<()> {
    let expected = expected_ids();
    let actual: BTreeSet<_> = tasks.keys().cloned().collect();
    if actual != expected {
        let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
        let unexpected = actual.difference(&expected).cloned().collect::<Vec<_>>();
        return Err(format!(
            "ledger ID universe mismatch: expected 166 IDs; found {}; missing={}; unexpected={}",
            actual.len(),
            if missing.is_empty() {
                "none".into()
            } else {
                missing.join(",")
            },
            if unexpected.is_empty() {
                "none".into()
            } else {
                unexpected.join(",")
            },
        ));
    }
    Ok(())
}

fn progress(source: &str) -> Result<Progress> {
    let tasks = parse_tasks(source)?;
    validate_universe(&tasks)?;
    Ok(Progress {
        checked: tasks.values().filter(|task| task.checked).count(),
        total: tasks.len(),
    })
}

#[derive(Debug)]
struct CensusRow {
    module: String,
    result: String,
    tag: String,
}

fn cells(line: &str) -> Vec<&str> {
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    line.split('|').map(str::trim).collect()
}

fn parse_census(source: &str) -> Result<Vec<CensusRow>> {
    let mut rows = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if !line.starts_with('|') || line.contains("---") {
            continue;
        }
        let row = cells(line);
        if row.len() < 5 || row[0] == "Upstream module" {
            continue;
        }
        if row[0].is_empty() || row[3].is_empty() {
            return Err(format!("malformed census row at line {}", index + 1));
        }
        rows.push(CensusRow {
            module: row[0].replace(char::from(96), ""),
            result: row[3].to_string(),
            tag: row[4].to_string(),
        });
    }
    if rows.is_empty() {
        return Err("census contains no module rows".to_string());
    }
    Ok(rows)
}

fn add_range(ids: &mut BTreeSet<String>, prefix: &str, start: u32, end: Option<u32>) {
    let end = end.unwrap_or(start);
    if start > end {
        return;
    }
    for value in start..=end {
        ids.insert(if prefix.is_empty() {
            value.to_string()
        } else {
            format!("{prefix}{value:03}")
        });
    }
}

fn referenced_ids(text: &str) -> Vec<(String, bool)> {
    let numeric = Regex::new(r"#(\d+)(?:[-–](\d+))?").unwrap();
    let supplemental = Regex::new(r"S-(\d+)(?:[-–]S?-(\d+))?").unwrap();
    let mut result = Vec::new();
    for (regex, prefix) in [(&numeric, ""), (&supplemental, "S-")] {
        for captures in regex.captures_iter(text) {
            let start: u32 = captures.get(1).unwrap().as_str().parse().unwrap();
            let end = captures.get(2).map(|value| value.as_str().parse().unwrap());
            let mut ids = BTreeSet::new();
            add_range(&mut ids, prefix, start, end);
            result.extend(ids.into_iter().map(|id| (id, end.is_some())));
        }
    }
    result
}

fn divergence(row: &CensusRow) -> Option<&'static str> {
    let s027 = Regex::new(r"(?i)\bS-027\b").unwrap();
    if s027.is_match(&row.tag)
        && (row.module.starts_with("coding-agent/src/core/extensions/")
            || row.module.starts_with("coding-agent/src/extensions/"))
    {
        return Some("S-027");
    }
    if row.module == "agent/src/search/scanning.ts"
        || row.module == "session-backends/sqlite-node/src/sqlite/search-backend.ts"
    {
        return Some("S-066");
    }
    None
}

fn divergence_evidence(root: &Path, id: &str) -> bool {
    let (path, marker) = match id {
        "S-027" => (
            "crates/pi-coding-agent/TODO.md",
            "S-027 is complete under the explicit 100%-Rust distribution scope",
        ),
        "S-066" => (
            ".unlazy/full-conversion-current-20260825/audit/S066-current.md",
            "agent/src/search/scanning.ts",
        ),
        _ => return false,
    };
    read(&root.join(path))
        .map(|source| source.contains(marker))
        .unwrap_or(false)
}

fn audit(root: &Path) -> Result<String> {
    let ledger = parse_tasks(&read(&root.join(LEDGER))?)?;
    validate_universe(&ledger)?;
    let rows = parse_census(&read(&root.join(CENSUS))?)?;
    let expected = expected_ids();
    let mut unclassified = 0;
    let mut blockers = 0;
    let mut divergence_counts = BTreeMap::<&str, usize>::new();
    let mut status_counts = BTreeMap::<&str, usize>::new();

    for row in &rows {
        *status_counts.entry(row.result.as_str()).or_default() += 1;
        if let Some(id) = divergence(row) {
            if !ledger.get(id).is_some_and(|task| task.checked) && !divergence_evidence(root, id) {
                blockers += 1;
            }
            *divergence_counts.entry(id).or_default() += 1;
            continue;
        }

        let mut owners = BTreeSet::new();
        if row.tag.contains("P1 foundation") && row.tag.contains("ledger hole") {
            for id in 43..=49 {
                owners.insert(format!("S-{id:03}"));
            }
        }
        for (id, is_range) in referenced_ids(&row.tag) {
            let task = ledger.get(&id);
            if task.is_some_and(|task| task.checked) || !(is_range && expected.contains(&id)) {
                owners.insert(id);
            }
        }
        if owners.is_empty() {
            unclassified += 1;
            blockers += 1;
        }
        for owner in owners {
            if !ledger.contains_key(&owner) {
                blockers += 1;
            }
        }
    }

    Ok(format!(
        "ledger ID universe: {}/166\nhistorical rows inspected: {}\nhistorical status counts: {:?}\nrow-level owner rows: {}/{}\nrow-level unclassified records: {unclassified}\nrow-level owner blockers: {blockers}\nintentional divergence row counts: {:?}\naudit blockers: {blockers}",
        ledger.len(),
        rows.len(),
        status_counts,
        rows.len() - unclassified,
        rows.len(),
        divergence_counts,
    ))
}

fn workspace_sources(directory: &Path) -> io::Result<Vec<PathBuf>> {
    if !directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "workspace root missing",
        ));
    }
    let mut files = Vec::new();
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "upstream_pi")
                ) {
                    continue;
                }
                visit(&path, files)?;
            } else if kind.is_file()
                && matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx")
                )
            {
                files.push(path);
            }
        }
        Ok(())
    }
    visit(directory, &mut files)?;
    files.sort();
    Ok(files)
}

fn zero_js_ts(root: &Path) -> Result<()> {
    let files = workspace_sources(root).map_err(|error| error.to_string())?;
    println!("workspace JS/TS source files: {}", files.len());
    if files.is_empty() {
        println!("hard zero-JS/TS scripts census passed");
        Ok(())
    } else {
        for file in files {
            eprintln!("source file: {}", file.display());
        }
        Err("workspace contains JavaScript/TypeScript source files".to_string())
    }
}

fn run() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: conversion-audit [all|progress|source-audit|zero-js-ts]");
        return Ok(());
    }
    let command = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("all");
    let root = repo_root();
    match command {
        "progress" => println!("{}", progress(&read(&root.join(LEDGER))?)?.output()),
        "source-audit" => {
            let output = audit(&root)?;
            println!("{output}");
            if output.lines().last() != Some("audit blockers: 0") {
                return Err("source audit found blockers".to_string());
            }
        }
        "zero-js-ts" => zero_js_ts(&root)?,
        "all" => {
            println!("{}", progress(&read(&root.join(LEDGER))?)?.output());
            let output = audit(&root)?;
            println!("{output}");
            if output.lines().last() != Some("audit blockers: 0") {
                return Err("source audit found blockers".to_string());
            }
            zero_js_ts(&root)?;
            println!("Rust conversion tooling checks passed");
        }
        other => return Err(format!("unknown command: {other}")),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("conversion-audit: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ledger() -> String {
        let mut lines = Vec::new();
        for id in 1..=100 {
            lines.push(format!("- [x] {id}. task"));
        }
        for id in 1..=66 {
            lines.push(format!("- [ ] S-{id:03} task"));
        }
        lines.join("\n")
    }

    #[test]
    fn progress_enforces_the_exact_universe() {
        assert_eq!(
            progress(&fixture_ledger()).unwrap().output(),
            "Conversion progress: 60.24% (100/166; 66 open)"
        );
    }

    #[test]
    fn malformed_and_duplicate_tasks_are_rejected() {
        assert!(parse_tasks("- [?] 1. bad\n").is_err());
        assert!(parse_tasks("- [x] 1. a\n- [ ] 1. b\n").is_err());
    }

    #[test]
    fn references_expand_ranges() {
        assert_eq!(
            referenced_ids("#1-3 S-002–S-004")
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec!["1", "2", "3", "S-002", "S-003", "S-004"]
        );
    }

    #[test]
    fn live_source_audit_has_rows_and_no_owner_blockers() {
        let output = audit(&repo_root()).unwrap();
        assert!(output.contains("ledger ID universe: 166/166"));
        assert!(output.contains("historical rows inspected: 483"));
        assert!(output.ends_with("audit blockers: 0"));
    }

    #[test]
    fn live_workspace_census_is_empty() {
        assert!(workspace_sources(&repo_root()).unwrap().is_empty());
    }
}
