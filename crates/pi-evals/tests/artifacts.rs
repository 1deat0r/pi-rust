//! Port of `packages/evals/test/vitest-evals/artifacts.test.ts`.

use pi_evals::artifacts::{
    persist_eval_artifact_references, record_eval_session_artifact, Attachment, EvalArtifact,
};

#[test]
fn records_session_and_source_artifacts() {
    let mut artifacts = std::collections::BTreeMap::new();
    artifacts.insert("runId".to_string(), serde_json::json!("run-1"));
    artifacts.insert(
        "piSessionJsonl".to_string(),
        serde_json::json!("{\"type\":\"session\"}\n"),
    );

    let session = record_eval_session_artifact(&artifacts)
        .unwrap()
        .expect("session artifact");
    let source = EvalArtifact::Source {
        run_id: "run-1".to_string(),
        attachments: vec![Attachment {
            name: "hello.ts".to_string(),
            content_type: "text/typescript".to_string(),
            body: "export default function () {}\n".to_string(),
            body_encoding: "utf-8".to_string(),
        }],
    };

    assert_eq!(
        session.type_name(),
        Some("@earendil-works/pi-evals:session")
    );
    assert_eq!(session.attachments()[0].name, "session.jsonl");
    assert_eq!(session.attachments()[0].body, "{\"type\":\"session\"}\n");
    assert_eq!(session.attachments()[0].content_type, "application/jsonl");
    assert_eq!(source.attachments()[0].name, "hello.ts");
}

#[test]
fn persists_and_selects_attachments_belonging_to_the_reported_run() {
    let root = std::env::temp_dir().join(format!(
        "pi-eval-artifact-report-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let artifacts = vec![
        EvalArtifact::Session {
            run_id: "run-1".to_string(),
            attachments: vec![Attachment {
                name: "session.jsonl".to_string(),
                content_type: "application/jsonl".to_string(),
                body: "{\"type\":\"session\"}\n".to_string(),
                body_encoding: "utf-8".to_string(),
            }],
        },
        EvalArtifact::Session {
            run_id: "run-2".to_string(),
            attachments: vec![],
        },
        EvalArtifact::Source {
            run_id: "run-1".to_string(),
            attachments: vec![Attachment {
                name: "hello.ts".to_string(),
                content_type: "text/typescript".to_string(),
                body: "export default function () {}\n".to_string(),
                body_encoding: "utf-8".to_string(),
            }],
        },
    ];

    let references = persist_eval_artifact_references(&artifacts, "run-1", &root).unwrap();
    assert_eq!(references.len(), 2);
    assert!(
        references[0].path.starts_with("sessions/"),
        "got: {:?}",
        references
    );
    assert!(
        references[0].path.ends_with("/session.jsonl"),
        "got: {:?}",
        references
    );
    assert!(
        references[1].path.starts_with("sources/"),
        "got: {:?}",
        references
    );
    assert!(
        references[1].path.ends_with("/hello.ts"),
        "got: {:?}",
        references
    );
    for reference in &references {
        let expected = if reference.name == "session.jsonl" {
            "{\"type\":\"session\"}\n"
        } else {
            "export default function () {}\n"
        };
        assert_eq!(
            std::fs::read_to_string(root.join(&reference.path)).unwrap(),
            expected
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
