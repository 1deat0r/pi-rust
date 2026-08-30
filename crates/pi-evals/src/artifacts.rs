//! Eval artifacts — port of `packages/evals/src/vitest-evals/artifacts.ts`.
//!
//! The upstream works against Vitest's `TestArtifactRegistry`; the Rust port
//! keeps the same artifact types and persistence semantics (session JSONL
//! under `sessions/<sha256(runId)>/`, generated sources under
//! `sources/<sha256(runId)>/`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::EvalError;

/// Name of the session-artifact key stored in run artifacts.
pub const PI_SESSION_SNAPSHOT_ARTIFACT: &str = "piSessionJsonl";

pub const SESSION_ARTIFACT_TYPE: &str = "@earendil-works/pi-evals:session";
pub const SOURCE_ARTIFACT_TYPE: &str = "@earendil-works/pi-evals:source";

/// A persisted eval attachment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub name: String,
    pub content_type: String,
    pub body: String,
    pub body_encoding: String,
}

/// Eval artifact union (port of the `PiSessionArtifact | SourceArtifact`
/// pair).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvalArtifact {
    #[serde(rename = "@earendil-works/pi-evals:session")]
    Session {
        run_id: String,
        attachments: Vec<Attachment>,
    },
    #[serde(rename = "@earendil-works/pi-evals:source")]
    Source {
        run_id: String,
        attachments: Vec<Attachment>,
    },
    #[serde(other)]
    Other,
}

impl EvalArtifact {
    pub fn is_session_or_source(&self) -> bool {
        matches!(
            self,
            EvalArtifact::Session { .. } | EvalArtifact::Source { .. }
        )
    }
    pub fn run_id(&self) -> Option<&str> {
        match self {
            EvalArtifact::Session { run_id, .. } | EvalArtifact::Source { run_id, .. } => {
                Some(run_id)
            }
            _ => None,
        }
    }
    pub fn attachments(&self) -> &[Attachment] {
        match self {
            EvalArtifact::Session { attachments, .. }
            | EvalArtifact::Source { attachments, .. } => attachments,
            _ => &[],
        }
    }
    pub fn type_name(&self) -> Option<&'static str> {
        match self {
            EvalArtifact::Session { .. } => Some(SESSION_ARTIFACT_TYPE),
            EvalArtifact::Source { .. } => Some(SOURCE_ARTIFACT_TYPE),
            _ => None,
        }
    }
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

/// Records the session snapshot artifact for a run (port of
/// `recordEvalSessionArtifact`): when the run artifacts contain
/// `piSessionJsonl`, it is attached as `session.jsonl`.
pub fn record_eval_session_artifact(
    run_artifacts: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<Option<EvalArtifact>, EvalError> {
    let Some(session) = run_artifacts.get(PI_SESSION_SNAPSHOT_ARTIFACT) else {
        return Ok(None);
    };
    let run_id = run_artifacts
        .get("runId")
        .and_then(|v| v.as_str())
        .ok_or(EvalError::InvalidSessionArtifact)?;
    let session = session.as_str().ok_or(EvalError::InvalidSessionArtifact)?;
    Ok(Some(EvalArtifact::Session {
        run_id: run_id.to_string(),
        attachments: vec![Attachment {
            name: "session.jsonl".to_string(),
            content_type: "application/jsonl".to_string(),
            body: session.to_string(),
            body_encoding: "utf-8".to_string(),
        }],
    }))
}

/// Records a generated source artifact (port of `recordEvalSourceArtifact`).
pub fn record_eval_source_artifact(run_id: &str, attachment: Attachment) -> EvalArtifact {
    EvalArtifact::Source {
        run_id: run_id.to_string(),
        attachments: vec![attachment],
    }
}

/// A persisted artifact reference relative to the artifact directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub name: String,
    pub path: String,
}

/// Persists session/source attachments that belong to `run_id` under the
/// artifact directory (port of `persistEvalArtifactReferences`).
pub fn persist_eval_artifact_references(
    artifacts: &[EvalArtifact],
    run_id: &str,
    artifact_directory: &Path,
) -> Result<Vec<ArtifactReference>, EvalError> {
    let mut references = Vec::new();
    for artifact in artifacts {
        if !artifact.is_session_or_source() || artifact.run_id() != Some(run_id) {
            continue;
        }
        let category = match artifact {
            EvalArtifact::Session { .. } => "sessions",
            EvalArtifact::Source { .. } => "sources",
            EvalArtifact::Other => continue,
        };
        for attachment in artifact.attachments() {
            let name = attachment
                .name
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&attachment.name);
            if name != attachment.name || name.is_empty() {
                return Err(EvalError::InvalidArtifactName {
                    name: attachment.name.clone(),
                });
            }
            let hash = sha256_hex(run_id.as_bytes());
            let directory = artifact_directory.join(category).join(hash);
            std::fs::create_dir_all(&directory)
                .map_err(|source| EvalError::CreateArtifactDir { source })?;
            restrict_permissions(&directory, 0o700)?;
            let path = directory.join(name);
            std::fs::write(&path, attachment.body.as_bytes())
                .map_err(|source| EvalError::WriteArtifact { source })?;
            restrict_permissions(&path, 0o600)?;
            let relative = path
                .strip_prefix(artifact_directory)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            references.push(ArtifactReference {
                name: name.to_string(),
                path: relative,
            });
        }
    }
    Ok(references)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> Result<(), EvalError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .map_err(|source| EvalError::InspectArtifactPermissions { source })?
        .permissions();
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions)
        .map_err(|source| EvalError::RestrictArtifactPermissions { source })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> Result<(), EvalError> {
    Ok(())
}

/// Result of a recorded run appended to `runs.jsonl` (port of the reporter
/// record shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub schema_version: u32,
    pub run_id: String,
    pub test: TestRecord,
    pub harness: String,
    pub usage: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactReference>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestRecord {
    pub id: String,
    pub file: String,
    pub name: String,
    pub full_name: String,
    pub status: String,
}
