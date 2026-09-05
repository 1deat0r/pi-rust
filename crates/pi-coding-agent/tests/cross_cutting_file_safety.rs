#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Aggregate real-process coverage for malformed persisted inputs and
//! filesystem mutation boundaries. Focused component suites remain the deeper
//! oracle; these tests prove the boundaries compose in the shipped binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-cross-cutting-files-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &agent_dir, &sessions, &project] {
            fs::create_dir_all(path).expect("create isolated test directory");
        }
        Self {
            root,
            home,
            agent_dir,
            sessions,
            project,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_agent_dir(&self.agent_dir, args)
    }

    fn run_with_agent_dir(&self, agent_dir: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(test_binary());
        command
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C")
            .args(args);
        command.output().expect("spawn real pi process")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn malformed_persisted_inputs_fail_closed_and_recover() {
    let sandbox = Sandbox::new("malformed");
    let sentinel = sandbox.root.join("unrelated.bin");
    let sentinel_bytes = b"unrelated\0bytes\xff";
    fs::write(&sentinel, sentinel_bytes).expect("write unrelated sentinel");

    let global_settings = sandbox.agent_dir.join("settings.json");
    let project_settings = sandbox.project.join(".pi/settings.json");
    fs::create_dir_all(project_settings.parent().unwrap()).expect("create project settings root");
    fs::write(&global_settings, b"{ malformed-global").expect("write malformed settings");
    fs::write(&project_settings, b"[ malformed-project").expect("write malformed project settings");
    let settings = sandbox.run(&["config"]);
    assert!(
        settings.status.success(),
        "config warning path failed: {}",
        stderr(&settings)
    );
    let settings_error = stderr(&settings);
    assert!(
        settings_error.contains(global_settings.to_str().unwrap())
            && settings_error.contains("invalid settings JSON"),
        "settings diagnostic was not actionable: {settings_error}"
    );
    let project_config = sandbox.run(&["config", "--local", "--approve"]);
    assert!(
        project_config.status.success(),
        "project config warning path failed: {}",
        stderr(&project_config)
    );
    assert!(
        stderr(&project_config).contains("Warning (config command, project settings):"),
        "project settings diagnostic missing: {}",
        stderr(&project_config)
    );
    assert_eq!(fs::read(&global_settings).unwrap(), b"{ malformed-global");
    assert_eq!(fs::read(&project_settings).unwrap(), b"[ malformed-project");

    fs::write(&global_settings, b"{}").expect("repair global settings");
    fs::write(&project_settings, b"{}").expect("repair project settings");

    let models = sandbox.agent_dir.join("models.json");
    fs::write(&models, b"{\"providers\":").expect("write malformed models");
    let list = sandbox.run(&["--list-models"]);
    assert!(
        list.status.success(),
        "list-models stderr: {}",
        stderr(&list)
    );
    assert!(
        stderr(&list).contains("Warning: errors loading models.json:")
            && stderr(&list).contains("Failed to parse models.json"),
        "models diagnostic missing: {}",
        stderr(&list)
    );
    assert_eq!(fs::read(&models).unwrap(), b"{\"providers\":");

    let auth = sandbox.agent_dir.join("auth.json");
    fs::write(&auth, b"{not-json").expect("write malformed auth");
    let auth_check = sandbox.run(&[
        "auth",
        "check",
        "--provider",
        "google",
        "--no-refresh",
        "--json",
    ]);
    assert_eq!(auth_check.status.code(), Some(2));
    assert!(
        auth_check.stderr.is_empty(),
        "auth stderr: {}",
        stderr(&auth_check)
    );
    let auth_result: serde_json::Value =
        serde_json::from_slice(&auth_check.stdout).expect("auth check JSON response");
    assert_eq!(auth_result["status"], "invalid");
    assert_eq!(auth_result["reason"], "invalid_state");
    assert_eq!(fs::read(&auth).unwrap(), b"{not-json");

    let malformed_session = sandbox.sessions.join("malformed.jsonl");
    let export = sandbox.root.join("must-not-exist.html");
    fs::write(&malformed_session, b"not a pi session\n").expect("write malformed session");
    let export_result = sandbox.run(&[
        "--export",
        malformed_session.to_str().unwrap(),
        export.to_str().unwrap(),
    ]);
    assert!(!export_result.status.success(), "malformed export passed");
    assert!(
        stderr(&export_result).contains("not a valid pi session"),
        "export diagnostic missing: {}",
        stderr(&export_result)
    );
    assert_eq!(fs::read(&malformed_session).unwrap(), b"not a pi session\n");
    assert!(!export.exists(), "failed export created output");

    fs::remove_file(&models).expect("remove malformed models");
    fs::write(&auth, b"{}").expect("repair auth");
    let recovered = sandbox.run(&[
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "--no-session",
        "recovered",
    ]);
    assert!(
        recovered.status.success(),
        "recovered process stderr: {}",
        stderr(&recovered)
    );
    assert_eq!(stdout(&recovered), "faux response to: recovered\n");
    assert_eq!(fs::read(&sentinel).unwrap(), sentinel_bytes);
}

#[cfg(unix)]
#[test]
fn symlinked_and_read_only_agent_roots_preserve_filesystem_boundaries() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let sandbox = Sandbox::new("filesystem");
    let sentinel = sandbox.root.join("unrelated.txt");
    fs::write(&sentinel, b"do-not-touch").expect("write unrelated sentinel");
    let package = sandbox.root.join("local-package");
    fs::create_dir_all(package.join("prompts")).expect("create local package");
    fs::write(package.join("prompts/example.md"), "Example $@").expect("write package prompt");

    let real_agent = sandbox.root.join("real-agent");
    let linked_agent = sandbox.root.join("linked-agent");
    fs::create_dir_all(&real_agent).expect("create real agent root");
    fs::write(real_agent.join("settings.json"), b"{}").expect("write settings");
    symlink(&real_agent, &linked_agent).expect("symlink agent root");
    let install = sandbox.run_with_agent_dir(
        &linked_agent,
        &["install", package.to_str().unwrap(), "--approve"],
    );
    assert!(
        install.status.success(),
        "symlink install: {}",
        stderr(&install)
    );
    assert!(fs::symlink_metadata(&linked_agent)
        .expect("linked agent metadata")
        .file_type()
        .is_symlink());
    let settings: serde_json::Value = serde_json::from_slice(
        &fs::read(real_agent.join("settings.json")).expect("read target settings"),
    )
    .expect("valid target settings JSON");
    let installed_source = settings["packages"]
        .as_array()
        .and_then(|packages| packages.first())
        .and_then(serde_json::Value::as_str)
        .expect("persisted local package source");
    assert_eq!(
        fs::canonicalize(linked_agent.join(installed_source)).unwrap(),
        fs::canonicalize(&package).unwrap(),
        "unexpected symlink target settings: {settings:#}"
    );
    assert_no_staging_files(&real_agent);
    assert_eq!(fs::read(&sentinel).unwrap(), b"do-not-touch");

    let read_only_agent = sandbox.root.join("read-only-agent");
    fs::create_dir_all(&read_only_agent).expect("create read-only agent root");
    let read_only_settings = read_only_agent.join("settings.json");
    fs::write(&read_only_settings, b"{}").expect("write read-only settings");
    fs::set_permissions(&read_only_settings, fs::Permissions::from_mode(0o400))
        .expect("make settings read-only");
    fs::set_permissions(&read_only_agent, fs::Permissions::from_mode(0o500))
        .expect("make agent root read-only");

    let rejected = sandbox.run_with_agent_dir(
        &read_only_agent,
        &["install", package.to_str().unwrap(), "--approve"],
    );

    fs::set_permissions(&read_only_agent, fs::Permissions::from_mode(0o700))
        .expect("restore agent root permissions");
    fs::set_permissions(&read_only_settings, fs::Permissions::from_mode(0o600))
        .expect("restore settings permissions");

    assert!(
        !rejected.status.success(),
        "read-only install unexpectedly passed"
    );
    let error = stderr(&rejected);
    assert!(
        error.contains("settings")
            && (error.contains("Permission denied") || error.contains("permission denied")),
        "read-only diagnostic missing: {error}"
    );
    assert_eq!(fs::read(&read_only_settings).unwrap(), b"{}");
    assert_no_staging_files(&read_only_agent);
    assert_eq!(fs::read(&sentinel).unwrap(), b"do-not-touch");
}

fn assert_no_staging_files(root: &Path) {
    let unexpected = fs::read_dir(root)
        .expect("read agent root")
        .map(|entry| entry.expect("agent root entry").file_name())
        .filter(|name| {
            let name = name.to_string_lossy();
            name.contains(".tmp") || name.ends_with(".lock")
        })
        .collect::<Vec<_>>();
    assert!(unexpected.is_empty(), "staging residue: {unexpected:?}");
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
