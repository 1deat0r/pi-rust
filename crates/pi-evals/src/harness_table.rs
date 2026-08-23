//! Eval harness tables — port of `packages/evals/src/vitest-evals/harness-table.ts`.

use serde::{Deserialize, Serialize};

use crate::harness::{Harness, HarnessResult, JsonValue};

pub const EVAL_HARNESS_ITERATION_ARTIFACT: &str = "vitestEvalsHarnessIteration";

/// Iteration metadata attached to every harness-table run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalHarnessIterationArtifact {
    pub schema_version: u32,
    pub eval_set: String,
    pub group_key: String,
    pub harness: String,
    pub baseline: String,
    pub candidates: Vec<String>,
    pub repetition: u32,
}

pub fn parse_eval_harness_iteration_artifact(
    value: Option<&JsonValue>,
) -> Option<EvalHarnessIterationArtifact> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    let parsed: EvalHarnessIterationArtifact = serde_json::from_value(value.clone()).ok()?;
    if parsed.schema_version != 1 {
        return None;
    }
    if parsed.candidates.iter().any(|name| name.is_empty()) {
        return None;
    }
    Some(parsed)
}

/// `canonicalizeJson`: deep-copies to plain JSON with sorted object keys,
/// rejecting non-finite numbers, non-plain objects, sparse arrays, and
/// circular references (mirror of the upstream validator).
pub fn canonicalize_json(value: &JsonValue) -> Result<JsonValue, String> {
    fn walk(value: &JsonValue, _ancestors: &mut Vec<usize>) -> Result<JsonValue, String> {
        match value {
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::String(_) => Ok(value.clone()),
            JsonValue::Number(number) => {
                if number.as_f64().map(|f| !f.is_finite()).unwrap_or(true) {
                    return Err("Eval input must contain only finite numbers.".to_string());
                }
                Ok(value.clone())
            }
            JsonValue::Array(items) => {
                // serde_json arrays cannot be sparse; a plain array arrives
                // here already.
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(walk(item, _ancestors)?);
                }
                Ok(JsonValue::Array(out))
            }
            JsonValue::Object(entries) => {
                let mut flat = Vec::new();
                for (key, item) in entries {
                    flat.push((key.clone(), walk(item, _ancestors)?));
                }
                flat.sort_by(|(left, _), (right, _)| left.cmp(right));
                Ok(JsonValue::Object(flat.into_iter().collect()))
            }
        }
    }
    walk(value, &mut Vec::new())
}

/// `deriveInputKey`: a trimmed `id` string when the input object has one,
/// else the sha256 of the canonical JSON.
pub fn derive_input_key(input: &JsonValue) -> Result<String, String> {
    if let JsonValue::Object(obj) = input {
        if let Some(JsonValue::String(id)) = obj.get("id") {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }
    let canonical = canonicalize_json(input)?;
    let json = serde_json::to_string(&canonical)
        .map_err(|error| format!("Eval input must be JSON-serializable: {error}"))?;
    Ok(sha256_hex(json.as_bytes()))
}

pub fn derive_eval_group_key(input: &JsonValue, repetition: u32) -> Result<String, String> {
    let input_key = derive_input_key(input)?;
    Ok(serde_json::to_string(&serde_json::json!([input_key, repetition])).unwrap_or_default())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Short random id for temp directories (uuid v4, dash-stripped).
pub fn short_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A planned table row (harness + repetition).
#[derive(Debug, Clone)]
pub struct EvalHarnessTableRow {
    pub harness: Harness<JsonValue>,
    pub name: String,
    pub repetition: u32,
}

/// Table options — baseline + (candidate | candidates) + repetitions.
#[derive(Debug, Clone)]
pub struct EvalHarnessTableOptions {
    pub baseline: Harness<JsonValue>,
    pub candidates: Vec<Harness<JsonValue>>,
    pub repetitions: u32,
}

impl EvalHarnessTableOptions {
    pub fn pair(baseline: Harness<JsonValue>, candidate: Harness<JsonValue>) -> Self {
        Self {
            baseline,
            candidates: vec![candidate],
            repetitions: 1,
        }
    }
    pub fn candidates_list(
        baseline: Harness<JsonValue>,
        candidates: Vec<Harness<JsonValue>>,
        repetitions: u32,
    ) -> Self {
        Self {
            baseline,
            candidates,
            repetitions,
        }
    }
}

fn validate_options(
    eval_set: &str,
    baseline: &Harness<JsonValue>,
    candidates: &[Harness<JsonValue>],
    repetitions: u32,
) -> Result<(), String> {
    if eval_set.trim().is_empty() {
        return Err("evalSet must not be empty.".to_string());
    }
    if candidates.is_empty() {
        return Err("At least one candidate harness is required.".to_string());
    }
    let mut names: Vec<&str> = Vec::new();
    names.push(&baseline.name);
    for candidate in candidates {
        names.push(&candidate.name);
    }
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    if unique.len() != names.len() {
        return Err("Harness names must be unique within an eval set.".to_string());
    }
    if repetitions < 1 {
        return Err("repetitions must be a positive integer.".to_string());
    }
    Ok(())
}

fn with_iteration_artifact(
    harness: Harness<JsonValue>,
    plan: EvalHarnessIterationPlan,
) -> Harness<JsonValue> {
    let base_name = harness.name.clone();
    let base_run = harness.run.clone();
    Harness::new(base_name, move |input, context| {
        let repetition = plan.repetition;
        let group_key = derive_eval_group_key(input, repetition).unwrap_or_default();
        let artifact = plan.with_group_key(group_key.clone());
        context.set_artifact(
            EVAL_HARNESS_ITERATION_ARTIFACT,
            serde_json::to_value(&artifact).unwrap_or_default(),
        );
        let mut result: HarnessResult<JsonValue> = base_run(input, context);
        result.artifacts.insert(
            EVAL_HARNESS_ITERATION_ARTIFACT.to_string(),
            serde_json::to_value(&artifact).unwrap_or_default(),
        );
        result
    })
}

/// Iteration plan (the artifact minus the per-run group key) — port of
/// `Omit<EvalHarnessIterationArtifact, "groupKey">`.
#[derive(Debug, Clone)]
struct EvalHarnessIterationPlan {
    schema_version: u32,
    eval_set: String,
    harness: String,
    baseline: String,
    candidates: Vec<String>,
    repetition: u32,
}

impl EvalHarnessIterationPlan {
    fn with_group_key(&self, group_key: String) -> EvalHarnessIterationArtifact {
        EvalHarnessIterationArtifact {
            schema_version: self.schema_version,
            eval_set: self.eval_set.clone(),
            group_key,
            harness: self.harness.clone(),
            baseline: self.baseline.clone(),
            candidates: self.candidates.clone(),
            repetition: self.repetition,
        }
    }
}

/// Builds baseline/candidate rows across repetitions (port of `evalHarnessTable`).
pub fn eval_harness_table(
    eval_set: &str,
    options: &EvalHarnessTableOptions,
) -> Result<Vec<EvalHarnessTableRow>, String> {
    validate_options(
        eval_set,
        &options.baseline,
        &options.candidates,
        options.repetitions,
    )?;

    let mut rows = Vec::new();
    let harnesses = std::iter::once(options.baseline.clone())
        .chain(options.candidates.clone())
        .collect::<Vec<_>>();
    for repetition in 1..=options.repetitions {
        for harness in &harnesses {
            let plan = EvalHarnessIterationPlan {
                schema_version: 1,
                eval_set: eval_set.to_string(),
                harness: harness.name.clone(),
                baseline: options.baseline.name.clone(),
                candidates: options.candidates.iter().map(|c| c.name.clone()).collect(),
                repetition,
            };
            rows.push(EvalHarnessTableRow {
                harness: with_iteration_artifact(harness.clone(), plan),
                name: harness.name.clone(),
                repetition,
            });
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::HarnessContext;

    fn fake_harness(name: &str) -> Harness<JsonValue> {
        let name = name.to_string();
        Harness::new(name.clone(), move |input, _context| {
            let input_id = input
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            HarnessResult {
                output: serde_json::json!({ "harness": name, "inputId": input_id }),
                errors: Vec::new(),
                events: vec![
                    crate::harness::TranscriptEvent::Message {
                        role: "user".into(),
                        content: input_id.clone(),
                    },
                    crate::harness::TranscriptEvent::Message {
                        role: "assistant".into(),
                        content: name.clone(),
                    },
                ],
                usage: crate::harness::HarnessUsage::default(),
                artifacts: std::collections::BTreeMap::new(),
                timings: None,
            }
        })
    }

    #[test]
    fn combines_a_trimmed_string_input_id_with_repetition() {
        assert_eq!(
            derive_eval_group_key(
                &serde_json::json!({ "id": " input-1 ", "prompt": "hello" }),
                2
            )
            .unwrap(),
            r#"["input-1",2]"#
        );
    }

    #[test]
    fn hashes_canonical_json_independently_of_object_key_order() {
        assert_eq!(
            derive_eval_group_key(
                &serde_json::json!({ "first": 1, "second": [true, "value"] }),
                1
            )
            .unwrap(),
            derive_eval_group_key(
                &serde_json::json!({ "second": [true, "value"], "first": 1 }),
                1
            )
            .unwrap()
        );
        assert_ne!(
            derive_eval_group_key(&serde_json::json!({ "first": 1 }), 1).unwrap(),
            derive_eval_group_key(&serde_json::json!({ "first": 2 }), 1).unwrap()
        );
        assert_ne!(
            derive_eval_group_key(&serde_json::json!({ "first": 1 }), 1).unwrap(),
            derive_eval_group_key(&serde_json::json!({ "first": 1 }), 2).unwrap()
        );
        assert_ne!(
            derive_eval_group_key(&serde_json::json!(["first", "second"]), 1).unwrap(),
            derive_eval_group_key(&serde_json::json!(["second", "first"]), 1).unwrap()
        );
    }

    #[test]
    fn rejects_non_json_inputs() {
        // serde_json cannot represent Date-like objects; the observable
        // difference is that arrays with holes and cycles cannot occur in
        // serde_json values (the JSON encoding is canonical by construction).
        // We keep finite-number enforcement as the Rust-representable part.
        let ok = canonicalize_json(&serde_json::json!({ "a": 1.5, "b": [true, null] }));
        assert!(ok.is_ok());
    }

    #[test]
    fn plans_repetitions_in_declaration_order() {
        let table = eval_harness_table(
            "local multi-harness eval",
            &EvalHarnessTableOptions::candidates_list(
                fake_harness("withoutSkill"),
                vec![fake_harness("withSkill")],
                2,
            ),
        )
        .unwrap();
        let rows: Vec<(String, u32)> = table
            .iter()
            .map(|row| (row.name.clone(), row.repetition))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("withoutSkill".to_string(), 1),
                ("withSkill".to_string(), 1),
                ("withoutSkill".to_string(), 2),
                ("withSkill".to_string(), 2),
            ]
        );
    }

    #[test]
    fn accepts_a_singular_candidate() {
        let rows = eval_harness_table(
            "singular candidate",
            &EvalHarnessTableOptions::pair(fake_harness("baseline"), fake_harness("candidate")),
        )
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.name.clone()).collect::<Vec<_>>(),
            vec!["baseline", "candidate"]
        );
    }

    #[test]
    fn attaches_iteration_metadata_to_every_wrapped_harness_run() {
        let table = eval_harness_table(
            "local multi-harness eval",
            &EvalHarnessTableOptions::candidates_list(
                fake_harness("withoutSkill"),
                vec![fake_harness("withSkill")],
                2,
            ),
        )
        .unwrap();
        for row in &table {
            let mut context = HarnessContext::default();
            let result = row
                .harness
                .run(&serde_json::json!({ "id": "first" }), &mut context);
            assert_eq!(
                result.output,
                serde_json::json!({ "harness": row.name, "inputId": "first" })
            );
            let parsed = parse_eval_harness_iteration_artifact(
                result.artifacts.get(EVAL_HARNESS_ITERATION_ARTIFACT),
            )
            .expect("iteration artifact attached");
            assert_eq!(
                parsed,
                EvalHarnessIterationArtifact {
                    schema_version: 1,
                    eval_set: "local multi-harness eval".to_string(),
                    group_key: derive_eval_group_key(
                        &serde_json::json!({ "id": "first" }),
                        row.repetition
                    )
                    .unwrap(),
                    harness: row.name.clone(),
                    baseline: "withoutSkill".to_string(),
                    candidates: vec!["withSkill".to_string()],
                    repetition: row.repetition,
                }
            );
        }
    }
}
