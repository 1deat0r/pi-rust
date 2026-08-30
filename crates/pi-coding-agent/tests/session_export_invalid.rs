#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Invalid-session boundary for the standalone HTML exporter.

#[test]
fn nonempty_headerless_session_is_rejected_instead_of_exported_as_empty() {
    let root = std::env::temp_dir().join(format!(
        "pi-export-invalid-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("headerless.jsonl");
    std::fs::write(&input, "not a pi session\n").unwrap();

    let error = pi_coding_agent::core::export_html::export_session_file(
        input.to_str().unwrap(),
        None,
        None,
    )
    .expect_err("a non-empty headerless session must fail export");
    assert_eq!(
        error.to_string(),
        format!(
            "Session file is not a valid pi session: {}",
            input.display()
        )
    );

    assert!(!root.join("pi-session-headerless.html").exists());
    std::fs::remove_dir_all(root).unwrap();
}
