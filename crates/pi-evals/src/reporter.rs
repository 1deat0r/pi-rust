//! Eval run reporting — port of `packages/evals/src/vitest-evals/reporter.ts`
//! (the `appendHarnessRunReport` half). The Rust port appends a normalized
//! run record to `.eval/<timestamp>_<uuid>/runs.jsonl`.

use std::path::PathBuf;

use crate::artifacts::{persist_eval_artifact_references, EvalArtifact, RunRecord, TestRecord};

pub struct ReporterOptions {
    pub artifact_directory: Option<PathBuf>,
}

impl Default for ReporterOptions {
    fn default() -> Self {
        let dir = std::env::var("PI_EVAL_ARTIFACT_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        Self {
            artifact_directory: dir,
        }
    }
}

/// Appends one harness run record to `runs.jsonl` (port of
/// `appendHarnessRunReport`).
#[allow(clippy::too_many_arguments)]
pub fn append_harness_run_report(
    options: &ReporterOptions,
    run_id: &str,
    test: TestRecord,
    harness: &str,
    usage: &serde_json::Value,
    timings: Option<&serde_json::Value>,
    errors: &[String],
    artifacts: &[EvalArtifact],
    metadata: std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let Some(artifact_directory) = &options.artifact_directory else {
        return Ok(());
    };
    std::fs::create_dir_all(artifact_directory)
        .map_err(|error| format!("Failed to create artifact dir: {error}"))?;
    let references = persist_eval_artifact_references(artifacts, run_id, artifact_directory)?;
    let record = RunRecord {
        schema_version: 1,
        run_id: run_id.to_string(),
        test,
        harness: harness.to_string(),
        usage: usage.clone(),
        timings: timings.cloned(),
        errors: errors.to_vec(),
        artifacts: references,
        metadata,
    };
    let path = artifact_directory.join("runs.jsonl");
    let line = serde_json::to_string(&record)
        .map_err(|error| format!("Failed to serialize run record: {error}"))?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("Failed to open runs.jsonl: {error}"))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| format!("Failed to append run record: {error}"))
}
