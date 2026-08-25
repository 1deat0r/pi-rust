//! Binary-level tests for `pi --export <file> [output]` (CLI wiring of the
//! export-html pipeline; the HTML parity itself is covered by
//! export_html_parity.rs against the oracle goldens).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-cli-export-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        Self { root, home }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", self.home.join(".pi").join("agent"))
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn stdout(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }
    fn stderr(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn export_with_explicit_output() {
    let sandbox = Sandbox::new("explicit");
    let cwd = sandbox.root.clone();
    let out_path = cwd.join("out.html");
    let out = sandbox.pi(
        &cwd,
        &[
            "--export",
            fixture("export_session.jsonl").to_str().unwrap(),
            out_path.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert_eq!(
        sandbox.stdout(&out).trim(),
        format!("Exported to: {}", out_path.display())
    );
    let html = fs::read_to_string(&out_path).unwrap();
    assert!(html.contains("<html"), "expected html document in output");
    assert!(
        html.contains("hello"),
        "expected user text in rendered HTML"
    );
    assert!(
        html.contains("hi there"),
        "expected assistant text in rendered HTML"
    );
    assert!(
        !html.contains("<script"),
        "expected static HTML without scripts"
    );
}

#[test]
fn export_default_output_name() {
    let sandbox = Sandbox::new("default-name");
    let cwd = sandbox.root.clone();
    let out = sandbox.pi(
        &cwd,
        &[
            "--export",
            fixture("export_session.jsonl").to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    let trimmed = stdout.trim();
    assert!(trimmed.starts_with("Exported to: "), "got: {trimmed}");
    let printed = trimmed.trim_start_matches("Exported to: ").trim();
    assert!(
        printed.ends_with("-session-export_session.html"),
        "got: {printed}"
    );
    // The binary runs with cwd = sandbox root, so a relative output path
    // lands there.
    let html = fs::read_to_string(cwd.join(printed)).unwrap();
    assert!(html.contains("<html"));
}

#[test]
fn export_missing_file_is_error() {
    let sandbox = Sandbox::new("missing");
    let cwd = sandbox.root.clone();
    let out = sandbox.pi(&cwd, &["--export", "/nonexistent/session.jsonl"]);
    assert!(!out.status.success(), "expected failure");
    assert!(
        sandbox.stderr(&out).contains("Error: File not found"),
        "stderr: {}",
        sandbox.stderr(&out)
    );
}
