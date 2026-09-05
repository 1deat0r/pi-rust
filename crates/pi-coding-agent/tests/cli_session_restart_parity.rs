#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused real-boundary coverage for clean startup, durable storage, and
//! session restart behavior.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pi_coding_agent::core::auth_storage::{AuthOperationOptions, AuthStorage, Credential};
use pi_coding_agent::core::settings::{
    FileSettingsStorage, SettingsManager, SettingsManagerCreateOptions, SettingsScope,
    SettingsStorage,
};
use serde_json::{json, Value};

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
            "pi-cli-session-restart-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &agent_dir, &sessions, &project] {
            fs::create_dir_all(path).expect("create clean process tree");
        }
        Self {
            root,
            home,
            agent_dir,
            sessions,
            project,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("RUST_BACKTRACE", "1")
            .env("LC_ALL", "C");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("spawn pi {}: {error}", test_binary().display()))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(jsonl_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn tree_snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, current: &Path, output: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        let entries = fs::read_dir(current).expect("read clean process tree");
        for entry in entries {
            let entry = entry.expect("read clean process entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("entry is below snapshot root")
                .to_path_buf();
            if path.is_dir() {
                output.push((relative.clone(), None));
                visit(root, &path, output);
            } else {
                output.push((relative, Some(fs::read(&path).expect("read snapshot file"))));
            }
        }
    }

    let mut output = Vec::new();
    visit(root, root, &mut output);
    output.sort_by(|left, right| left.0.cmp(&right.0));
    output
}

fn api_key(key: &str) -> Credential {
    serde_json::from_value(json!({
        "type": "api_key",
        "key": key,
    }))
    .expect("deserialize test credential")
}

fn auth_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[test]
fn clean_child_restart_honors_session_dir_precedence_and_reopens_file() {
    // PI_SESSION_FILE is declared by config but resolved in run.rs, outside
    // this lane's owned paths. The CLI --session file selector is covered
    // below; the environment selector remains an explicit follow-up.
    let sandbox = Sandbox::new("precedence");
    let cli_sessions = sandbox.root.join("cli-sessions");
    let env_sessions = sandbox.root.join("env-sessions");
    let settings_sessions = sandbox.root.join("settings-sessions");

    let mut first = sandbox.command();
    first
        .env("PI_CODING_AGENT_SESSION_DIR", &env_sessions)
        .args([
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "--session-dir",
        ])
        .arg(&cli_sessions)
        .args(["--session-id", "restart-session", "--print", "first prompt"]);
    let first = first.output().expect("run first clean child");
    assert!(first.status.success(), "first stderr: {}", stderr(&first));
    assert!(stdout(&first).contains("faux response to: first prompt"));
    assert_eq!(jsonl_files(&cli_sessions).len(), 1);
    assert!(
        jsonl_files(&env_sessions).is_empty(),
        "CLI must beat env session dir"
    );

    let session_file = jsonl_files(&cli_sessions)
        .pop()
        .expect("first session file");
    let before = fs::metadata(&session_file)
        .expect("stat first session")
        .len();
    let mut second = sandbox.command();
    second
        .env("PI_CODING_AGENT_SESSION_DIR", &env_sessions)
        .args([
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "--session-dir",
        ])
        .arg(&cli_sessions)
        .args(["--session"])
        .arg(&session_file)
        .args(["--print", "second prompt"]);
    let second = second.output().expect("run restart child");
    assert!(
        second.status.success(),
        "restart stderr: {}",
        stderr(&second)
    );
    assert!(stdout(&second).contains("faux response to: second prompt"));
    assert!(
        fs::metadata(&session_file)
            .expect("stat restarted session")
            .len()
            > before
    );
    let contents = fs::read_to_string(&session_file).expect("read restarted session");
    assert!(contents.contains("first prompt"));
    assert!(contents.contains("second prompt"));

    fs::create_dir_all(&sandbox.agent_dir).expect("create agent settings dir");
    fs::write(
        sandbox.agent_dir.join("settings.json"),
        serde_json::to_vec_pretty(&json!({
            "sessionDir": settings_sessions,
        }))
        .expect("serialize session-dir setting"),
    )
    .expect("seed settings session dir");
    let mut settings_run = sandbox.command();
    settings_run
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .args([
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "--session-id",
            "settings-session",
            "--print",
            "settings prompt",
        ]);
    let settings_run = settings_run.output().expect("run settings session child");
    assert!(
        settings_run.status.success(),
        "settings session stderr: {}",
        stderr(&settings_run)
    );
    assert_eq!(jsonl_files(&settings_sessions).len(), 1);
    assert!(jsonl_files(&env_sessions).is_empty());
}
#[test]
fn continue_variants_cover_wrong_cwd_no_session_and_malformed_file() {
    let sandbox = Sandbox::new("continue-variants");
    let continue_args = [
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "--continue",
    ];

    // Wrong cwd: the session belongs to another project, so lookup must
    // miss and the run must fail with the no-previous-session diagnostic.
    let other_project = sandbox.root.join("other-project");
    fs::create_dir_all(&other_project).expect("create other project");
    let wrong_cwd = sandbox
        .command()
        .current_dir(&other_project)
        .args(continue_args)
        .arg("--print")
        .arg("wrong cwd")
        .output()
        .expect("run wrong-cwd continue");
    assert!(
        !wrong_cwd.status.success(),
        "wrong-cwd continue must fail: {}",
        stderr(&wrong_cwd)
    );
    assert!(
        stderr(&wrong_cwd).contains("no previous session found to continue in this directory"),
        "expected no-previous-session diagnostic, got: {}",
        stderr(&wrong_cwd)
    );

    // No session at all in the right cwd: same fail-closed diagnostic.
    let no_session = sandbox
        .command()
        .args(continue_args)
        .arg("--print")
        .arg("no session")
        .output()
        .expect("run no-session continue");
    assert!(
        !no_session.status.success(),
        "no-session continue must fail: {}",
        stderr(&no_session)
    );
    assert!(
        stderr(&no_session).contains("no previous session found to continue in this directory"),
        "expected no-previous-session diagnostic, got: {}",
        stderr(&no_session)
    );

    // Seed a real session, then corrupt it to a malformed header. Discovery
    // skips unreadable headers (upstream readSessionHeaderForDiscovery),
    // so --continue must keep failing closed with the same diagnostic
    // instead of opening a broken file.
    let seed = sandbox
        .command()
        .args([
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "--session-id",
            "malformed-target",
            "--print",
            "seed prompt",
        ])
        .output()
        .expect("run seed session");
    assert!(seed.status.success(), "seed stderr: {}", stderr(&seed));
    let session_file = jsonl_files(&sandbox.sessions)
        .pop()
        .expect("seed session file");
    let original = fs::read_to_string(&session_file).expect("read seed session");
    fs::write(&session_file, b"{ corrupted jsonl\n").expect("corrupt session file");

    let malformed = sandbox
        .command()
        .args(continue_args)
        .arg("--print")
        .arg("malformed")
        .output()
        .expect("run malformed continue");
    assert!(
        !malformed.status.success(),
        "malformed continue must fail: {}",
        stderr(&malformed)
    );
    assert!(
        stderr(&malformed).contains("no previous session found to continue in this directory"),
        "expected no-previous-session diagnostic, got: {}",
        stderr(&malformed)
    );

    // Recovery: restoring the bytes makes --continue work again and append.
    fs::write(&session_file, original).expect("restore session file");
    let recovered = sandbox
        .command()
        .args(continue_args)
        .arg("--print")
        .arg("recovered prompt")
        .output()
        .expect("run recovered continue");
    assert!(
        recovered.status.success(),
        "recovered stderr: {}",
        stderr(&recovered)
    );
    assert!(stdout(&recovered).contains("faux response to: recovered prompt"));
    let contents = fs::read_to_string(&session_file).expect("read recovered session");
    assert!(contents.contains("seed prompt"));
    assert!(contents.contains("recovered prompt"));
}

#[test]
fn explicit_session_id_reopens_the_same_session_across_processes() {
    let sandbox = Sandbox::new("explicit-id-restart");
    let first = sandbox.run(&[
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "--session-id",
        "fixed-session",
        "--print",
        "first prompt",
    ]);
    assert!(first.status.success(), "first stderr: {}", stderr(&first));
    assert!(stderr(&first).contains("creating a new session"));

    let second = sandbox.run(&[
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "--session-id",
        "fixed-session",
        "--print",
        "second prompt",
    ]);
    assert!(
        second.status.success(),
        "second stderr: {}",
        stderr(&second)
    );
    assert!(stderr(&second).is_empty());
    assert_eq!(jsonl_files(&sandbox.sessions).len(), 1);
    let session = fs::read_to_string(
        jsonl_files(&sandbox.sessions)
            .pop()
            .expect("reopened session file"),
    )
    .expect("read reopened session");
    assert!(session.contains("first prompt"));
    assert!(session.contains("second prompt"));
}

#[test]
fn no_session_repeat_and_version_boundary_are_process_stable() {
    let sandbox = Sandbox::new("no-session");
    fs::write(
        sandbox.sessions.join("pre-existing-marker.txt"),
        b"must remain unchanged",
    )
    .expect("seed session marker");
    let before = tree_snapshot(&sandbox.sessions);

    let version = sandbox.run(&["--version"]);
    assert!(
        version.status.success(),
        "version stderr: {}",
        stderr(&version)
    );
    assert_eq!(stdout(&version), "pi 0.84.2\n");
    assert!(!stdout(&version).contains("Update available"));
    assert!(!stderr(&version).contains("Update available"));

    let version_source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/core/version_check.rs"
    ));
    assert!(!version_source.contains("latest-version"));
    assert!(!version_source.contains("reqwest"));
    assert!(!version_source.contains("Update available"));

    for prompt in ["ephemeral one", "ephemeral two"] {
        let output = sandbox.run(&[
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "--no-session",
            prompt,
        ]);
        assert!(
            output.status.success(),
            "no-session stderr: {}",
            stderr(&output)
        );
        assert!(stdout(&output).contains(&format!("faux response to: {prompt}")));
        assert!(!stderr(&output).contains("Update available"));
        assert_eq!(tree_snapshot(&sandbox.sessions), before);
    }
}

#[test]
fn settings_round_trip_is_clean_startup_atomic_and_concurrent() {
    let sandbox = Sandbox::new("settings");
    let agent_dir = sandbox.root.join("fresh-agent");
    let project = sandbox.root.join("fresh-project");
    fs::create_dir_all(&project).expect("create fresh project");
    let manager = SettingsManager::create(
        &project.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        SettingsManagerCreateOptions::default(),
    );
    assert!(!agent_dir.exists(), "clean settings read created agent dir");
    assert!(
        !project.join(".pi").exists(),
        "clean settings read created project dir"
    );
    drop(manager);

    let mut manager = SettingsManager::create(
        &project.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        SettingsManagerCreateOptions::default(),
    );
    manager.set_theme("dark".to_string());
    manager.set_default_provider("faux".to_string());
    manager.flush_sync();
    assert!(manager.drain_errors().is_empty());
    let settings_path = agent_dir.join("settings.json");
    assert_eq!(fs::metadata(&settings_path).unwrap().mode() & 0o777, 0o600);
    drop(manager);

    let reloaded = SettingsManager::create(
        &project.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(reloaded.get_theme_setting(), Some("dark"));
    assert_eq!(reloaded.get_default_provider(), Some("faux"));
    drop(reloaded);

    let storage = Arc::new(FileSettingsStorage::new(
        &project.to_string_lossy(),
        &agent_dir.to_string_lossy(),
    ));
    let writers = 8;
    let ready = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for index in 0..writers {
        let storage = Arc::clone(&storage);
        let ready = Arc::clone(&ready);
        handles.push(thread::spawn(move || {
            ready.wait();
            storage.with_lock(SettingsScope::Global, &mut |current| {
                let mut map = current
                    .map(|content| serde_json::from_str::<Value>(content).expect("valid settings"))
                    .unwrap_or_else(|| json!({}));
                map.as_object_mut()
                    .expect("settings root object")
                    .insert(format!("writer-{index}"), json!(index));
                thread::sleep(Duration::from_millis(3));
                Some(serde_json::to_string_pretty(&map).expect("serialize settings"))
            });
        }));
    }
    for handle in handles {
        handle.join().expect("concurrent settings writer");
    }
    let persisted: Value = serde_json::from_str(
        &fs::read_to_string(&settings_path).expect("read concurrent settings"),
    )
    .expect("atomic settings file is valid JSON");
    for index in 0..writers {
        let key = format!("writer-{index}");
        assert_eq!(persisted.get(key.as_str()), Some(&json!(index)));
    }
}

#[test]
fn auth_round_trip_concurrency_logout_and_atomic_readers_are_real() {
    let sandbox = Sandbox::new("auth");
    let auth_path = sandbox.root.join("auth").join("auth.json");
    let runtime = tokio::runtime::Runtime::new().expect("create auth runtime");
    let store = AuthStorage::create(auth_path.clone());
    let options = AuthOperationOptions::default();
    runtime
        .block_on(store.modify(
            "alpha",
            move |_| {
                let credential = api_key("alpha-secret");
                Box::pin(async move { Ok(Some(credential)) })
            },
            &options,
        ))
        .expect("persist alpha credential");
    runtime
        .block_on(store.modify(
            "beta",
            move |_| {
                let credential = api_key("beta-secret");
                Box::pin(async move { Ok(Some(credential)) })
            },
            &options,
        ))
        .expect("persist beta credential");
    assert_eq!(fs::metadata(&auth_path).unwrap().mode() & 0o777, 0o600);

    let fresh = AuthStorage::create(auth_path.clone());
    assert!(runtime
        .block_on(fresh.read("alpha", &options))
        .expect("read alpha after restart")
        .is_some());
    assert!(runtime
        .block_on(fresh.read("beta", &options))
        .expect("read beta after restart")
        .is_some());
    runtime
        .block_on(fresh.delete("alpha", &options))
        .expect("logout alpha");
    let after_logout = AuthStorage::create(auth_path.clone());
    assert!(runtime
        .block_on(after_logout.read("alpha", &options))
        .expect("read logged-out alpha")
        .is_none());
    assert!(runtime
        .block_on(after_logout.read("beta", &options))
        .expect("read retained beta")
        .is_some());

    let writer_count = 8;
    let ready = Arc::new(Barrier::new(writer_count));
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop_reader);
    let reader_path = auth_path.clone();
    let reader = thread::spawn(move || {
        while !reader_stop.load(Ordering::SeqCst) {
            if let Ok(mut file) = fs::File::open(&reader_path) {
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .expect("read auth snapshot");
                serde_json::from_str::<Value>(&content).expect("atomic auth snapshot JSON");
            }
        }
    });
    let mut handles = Vec::new();
    for index in 0..writer_count {
        let path = auth_path.clone();
        let ready = Arc::clone(&ready);
        handles.push(thread::spawn(move || {
            let store = AuthStorage::create(path);
            let runtime = tokio::runtime::Runtime::new().expect("create writer runtime");
            ready.wait();
            let options = AuthOperationOptions::default();
            runtime
                .block_on(store.modify(
                    &format!("provider-{index}"),
                    move |_| {
                        let credential = api_key(&format!("secret-{index}"));
                        Box::pin(async move {
                            tokio::time::sleep(Duration::from_millis(3)).await;
                            Ok(Some(credential))
                        })
                    },
                    &options,
                ))
                .expect("concurrent auth writer");
        }));
    }
    for handle in handles {
        handle.join().expect("join concurrent auth writer");
    }
    stop_reader.store(true, Ordering::SeqCst);
    reader.join().expect("join auth reader");

    let final_store = AuthStorage::create(auth_path);
    let entries = runtime
        .block_on(final_store.list(&options))
        .expect("list concurrent auth entries");
    let providers = entries
        .into_iter()
        .map(|entry| entry.provider_id)
        .collect::<std::collections::BTreeSet<_>>();
    for index in 0..writer_count {
        assert!(providers.contains(&format!("provider-{index}")));
    }
}

#[test]
fn malformed_files_fail_closed_then_recover_and_cancellation_allows_restart() {
    let sandbox = Sandbox::new("malformed");
    let agent_dir = sandbox.root.join("agent");
    let project = sandbox.root.join("project");
    fs::create_dir_all(project.join(".pi")).expect("create project settings dir");
    let global_settings = agent_dir.join("settings.json");
    let project_settings = project.join(".pi/settings.json");
    fs::write(&global_settings, b"{ malformed").expect("write malformed global settings");
    fs::write(&project_settings, b"[ malformed").expect("write malformed project settings");
    let mut manager = SettingsManager::create(
        &project.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(manager.drain_errors().len(), 2);
    manager.set_theme("must-not-overwrite".to_string());
    manager.flush_sync();
    assert_eq!(fs::read_to_string(&global_settings).unwrap(), "{ malformed");

    fs::write(&global_settings, br#"{"theme":"recovered-global"}"#)
        .expect("repair global settings");
    fs::write(&project_settings, br#"{"theme":"recovered-project"}"#)
        .expect("repair project settings");
    tokio::runtime::Runtime::new()
        .expect("create settings runtime")
        .block_on(manager.reload());
    assert_eq!(manager.get_theme_setting(), Some("recovered-project"));

    let auth_path = sandbox.root.join("malformed-auth").join("auth.json");
    fs::create_dir_all(auth_path.parent().unwrap()).expect("create auth dir");
    fs::write(&auth_path, b"{ malformed").expect("write malformed auth");
    let mut auth = AuthStorage::create(auth_path.clone());
    let signal = Arc::new(AtomicBool::new(false));
    let options = AuthOperationOptions {
        signal: Some(Arc::clone(&signal)),
    };
    let runtime = tokio::runtime::Runtime::new().expect("create malformed auth runtime");
    assert!(runtime.block_on(auth.read("provider", &options)).is_err());
    let unchanged = fs::read_to_string(&auth_path).expect("read malformed auth unchanged");
    assert_eq!(unchanged, "{ malformed");
    fs::write(
        &auth_path,
        serde_json::to_vec(&BTreeMap::from([(
            String::from("provider"),
            api_key("recovered"),
        )]))
        .expect("serialize repaired auth"),
    )
    .expect("repair auth");
    auth.reload();
    assert!(runtime
        .block_on(auth.read("provider", &AuthOperationOptions::default()))
        .expect("read repaired auth")
        .is_some());

    let cancel_path = sandbox.root.join("cancel-auth").join("auth.json");
    let cancel_store = AuthStorage::create(cancel_path.clone());
    let lock_path = auth_lock_path(&cancel_path);
    fs::write(&lock_path, format!("{}\n", std::process::id())).expect("hold auth lock");
    let cancel_signal = Arc::new(AtomicBool::new(false));
    let signal_for_thread = Arc::clone(&cancel_signal);
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(40));
        signal_for_thread.store(true, Ordering::SeqCst);
    });
    let cancel_options = AuthOperationOptions {
        signal: Some(cancel_signal),
    };
    let cancelled = runtime.block_on(cancel_store.modify(
        "cancelled",
        move |_| {
            let credential = api_key("should-not-write");
            Box::pin(async move { Ok(Some(credential)) })
        },
        &cancel_options,
    ));
    canceller.join().expect("join cancellation thread");
    assert_eq!(cancelled.unwrap_err().to_string(), "Aborted");
    assert!(
        lock_path.exists(),
        "cancellation removed another process lock"
    );
    assert!(!fs::read_to_string(&cancel_path)
        .expect("read cancelled auth")
        .contains("should-not-write"));
    fs::remove_file(&lock_path).expect("release held auth lock");

    fs::write(&lock_path, b"4294967295\n").expect("seed dead auth lock");
    let restarted = AuthStorage::create(cancel_path);
    runtime
        .block_on(restarted.modify(
            "after-restart",
            move |_| {
                let credential = api_key("restart-success");
                Box::pin(async move { Ok(Some(credential)) })
            },
            &AuthOperationOptions::default(),
        ))
        .expect("restart recovers dead lock");
}
