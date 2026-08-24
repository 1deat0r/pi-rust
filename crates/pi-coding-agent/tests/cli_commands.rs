//! Binary-level tests for the pi CLI package/config/auth commands
//! (`pi install/remove/uninstall/update/list`, `pi config`, `pi auth`).
//!
//! Each test runs the real `pi` binary with a sandboxed $HOME and
//! PI_CODING_AGENT_DIR so no real home directory or setting file is touched.
//! A fake `npm` on PATH (via the settings `npmCommand` array) lets install/
//! remove/update exercise the full flow without network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pi-cli-cmds-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        Self {
            root,
            home,
            agent_dir,
        }
    }

    fn write_global_settings(&self, v: serde_json::Value) {
        fs::write(self.agent_dir.join("settings.json"), v.to_string()).unwrap();
    }

    fn read_global_settings(&self) -> serde_json::Value {
        let content = fs::read_to_string(self.agent_dir.join("settings.json")).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    fn write_auth(&self, v: serde_json::Value) {
        fs::write(self.agent_dir.join("auth.json"), v.to_string()).unwrap();
    }

    fn write_fake_npm(&self) -> PathBuf {
        let bin = self.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("fake-npm");
        fs::write(
            &fake,
            r#"#!/bin/sh
# Fake npm for CLI tests: handles install/uninstall/view.
mode=""; spec=""; root=""
for a in "$@"; do
  case "$a" in
    install|uninstall|view) mode="$a";;
    --prefix) ;;
    -*) ;;
    *) if [ -z "$spec" ]; then spec="$a"; else root="$a"; fi;;
  esac
done
if [ "$mode" = "view" ]; then
  echo "\"9.9.9\""
  exit 0
fi
if [ -z "$root" ]; then
  echo "fake-npm: missing --prefix" >&2
  exit 1
fi
if [ "$mode" = "install" ]; then
  mkdir -p "$root/node_modules/$spec"
  printf '{"name":"%s","version":"9.9.9"}' "$spec" > "$root/node_modules/$spec/package.json"
  exit 0
fi
if [ "$mode" = "uninstall" ]; then
  rm -rf "$root/node_modules/$spec"
  exit 0
fi
exit 0
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn pi_offline(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_OFFLINE", "1")
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

fn project(sandbox: &Sandbox, name: &str) -> PathBuf {
    let dir = sandbox.root.join(name);
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// pi list
// ---------------------------------------------------------------------------

#[test]
fn list_no_packages() {
    let sandbox = Sandbox::new("list-empty");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["list"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert_eq!(sandbox.stdout(&out).trim(), "No packages installed.");
}

#[test]
fn list_user_and_project_packages() {
    let sandbox = Sandbox::new("list");
    // User package whose install dir exists, and project package missing.
    fs::create_dir_all(
        sandbox
            .agent_dir
            .join("git")
            .join("github.com")
            .join("u")
            .join("r"),
    )
    .unwrap();
    sandbox.write_global_settings(json!({
        "packages": ["git:github.com/u/r"]
    }));
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(cwd.join(".pi")).unwrap();
    fs::write(
        cwd.join(".pi").join("settings.json"),
        json!({ "packages": ["npm:missing"] }).to_string(),
    )
    .unwrap();

    let out = sandbox.pi(&cwd, &["list", "--approve"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("User packages:"), "{stdout}");
    assert!(stdout.contains("git:github.com/u/r"), "{stdout}");
    assert!(
        stdout.contains(&sandbox.agent_dir.display().to_string()),
        "{stdout}"
    );
    assert!(stdout.contains("Project packages:"), "{stdout}");
    assert!(stdout.contains("npm:missing"), "{stdout}");
}

// ---------------------------------------------------------------------------
// pi install / remove
// ---------------------------------------------------------------------------

#[test]
fn install_local_package_persists_settings() {
    let sandbox = Sandbox::new("install-local");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(cwd.join("pkg").join("extensions")).unwrap();

    let out = sandbox.pi(&cwd, &["install", "./pkg"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("Installing ./pkg..."), "{stdout}");
    assert!(stdout.contains("Installed ./pkg"), "{stdout}");

    let settings = sandbox.read_global_settings();
    let packages = settings["packages"].as_array().expect("packages array");
    assert_eq!(packages.len(), 1);
    assert!(
        packages[0].as_str().unwrap().contains("pkg"),
        "{}",
        packages[0]
    );
}

#[test]
fn install_local_missing_path_errors() {
    let sandbox = Sandbox::new("install-missing");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["install", "./does-not-exist"]);
    assert!(!out.status.success());
    let stderr = sandbox.stderr(&out);
    assert!(stderr.contains("Path does not exist"), "{stderr}");
}

#[test]
fn install_npm_with_fake_npm() {
    let sandbox = Sandbox::new("install-npm");
    let fake = sandbox.write_fake_npm();
    sandbox.write_global_settings(json!({ "npmCommand": [fake.to_string_lossy()] }));
    let cwd = project(&sandbox, "work");

    let out = sandbox.pi(&cwd, &["install", "npm:demo-pkg"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("Installed npm:demo-pkg"), "{stdout}");

    // The managed layout appeared under the agent dir npm root.
    let installed = sandbox
        .agent_dir
        .join("npm")
        .join("node_modules")
        .join("demo-pkg")
        .join("package.json");
    assert!(
        installed.exists(),
        "fake npm must create the managed install layout"
    );

    // The settings entry was persisted.
    let settings = sandbox.read_global_settings();
    assert!(settings["packages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "npm:demo-pkg"));
}

#[test]
fn remove_npm_package_updates_settings_and_layout() {
    let sandbox = Sandbox::new("remove-npm");
    let fake = sandbox.write_fake_npm();
    sandbox.write_global_settings(
        json!({ "npmCommand": [fake.to_string_lossy()], "packages": ["npm:demo-pkg"] }),
    );
    fs::create_dir_all(
        sandbox
            .agent_dir
            .join("npm")
            .join("node_modules")
            .join("demo-pkg"),
    )
    .unwrap();
    let cwd = project(&sandbox, "work");

    let out = sandbox.pi(&cwd, &["remove", "npm:demo-pkg"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("Removed npm:demo-pkg"), "{stdout}");
    let settings = sandbox.read_global_settings();
    assert!(settings
        .get("packages")
        .map(|p| p.as_array().map(|a| a.is_empty()).unwrap_or(true))
        .unwrap_or(true));
    assert!(!sandbox
        .agent_dir
        .join("npm")
        .join("node_modules")
        .join("demo-pkg")
        .exists());
}

#[test]
fn remove_unmatched_package_errors() {
    let sandbox = Sandbox::new("remove-miss");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["remove", "npm:ghost"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("No matching package found for npm:ghost"),
        "{stderr}"
    );
}

#[test]
fn uninstall_alias_works() {
    let sandbox = Sandbox::new("uninstall");
    sandbox.write_global_settings(json!({ "packages": ["npm:ghost"] }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["uninstall", "npm:ghost"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("Removed npm:ghost"), "{stdout}");
}

#[test]
fn install_without_source_errors() {
    let sandbox = Sandbox::new("install-none");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["install"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(stderr.contains("Missing install source."), "{stderr}");
}

#[test]
fn install_unknown_flag_errors() {
    let sandbox = Sandbox::new("install-flag");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["install", "--wat", "npm:x"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Unknown option --wat for \"install\""),
        "{stderr}"
    );
}

#[test]
fn install_help_prints_usage() {
    let sandbox = Sandbox::new("install-help");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["install", "--help"]);
    assert!(out.status.success());
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("pi install <source> [-l] [--approve|--no-approve]"),
        "{stdout}"
    );
}

// ---------------------------------------------------------------------------
// pi update
// ---------------------------------------------------------------------------

#[test]
fn update_reports_extensions_skipped_for_default_self() {
    let sandbox = Sandbox::new("update-self");
    sandbox.write_global_settings(json!({ "packages": [] }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(&cwd, &["update"]);
    // Offline update still resolves the self-update plan first, matching the
    // upstream fetch failure instead of pretending the binary was updated.
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("Extensions are skipped. Run pi update --extensions to update extensions."),
        "{stdout}"
    );
    assert!(!out.status.success());
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Could not determine latest pi version"),
        "{stderr}"
    );
}

#[test]
fn update_extension_with_fake_npm() {
    let sandbox = Sandbox::new("update-ext");
    let fake = sandbox.write_fake_npm();
    fs::create_dir_all(
        sandbox
            .agent_dir
            .join("npm")
            .join("node_modules")
            .join("demo-pkg"),
    )
    .unwrap();
    fs::write(
        sandbox
            .agent_dir
            .join("npm")
            .join("node_modules")
            .join("demo-pkg")
            .join("package.json"),
        r#"{ "name": "demo-pkg", "version": "1.0.0" }"#,
    )
    .unwrap();
    sandbox.write_global_settings(
        json!({ "npmCommand": [fake.to_string_lossy()], "packages": ["npm:demo-pkg"] }),
    );
    let cwd = project(&sandbox, "work");

    let out = sandbox.pi(&cwd, &["update", "--extensions"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("Updated packages"), "{stdout}");
}

#[test]
fn update_single_extension_reports_updated_source() {
    let sandbox = Sandbox::new("update-ext-one");
    let fake = sandbox.write_fake_npm();
    fs::create_dir_all(
        sandbox
            .agent_dir
            .join("npm")
            .join("node_modules")
            .join("demo-pkg"),
    )
    .unwrap();
    fs::write(
        sandbox
            .agent_dir
            .join("npm")
            .join("node_modules")
            .join("demo-pkg")
            .join("package.json"),
        r#"{ "name": "demo-pkg", "version": "1.0.0" }"#,
    )
    .unwrap();
    sandbox.write_global_settings(
        json!({ "npmCommand": [fake.to_string_lossy()], "packages": ["npm:demo-pkg"] }),
    );
    let cwd = project(&sandbox, "work");

    let out = sandbox.pi(&cwd, &["update", "npm:demo-pkg"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("Updated npm:demo-pkg"), "{stdout}");
}

// ---------------------------------------------------------------------------
// pi config
// ---------------------------------------------------------------------------

#[test]
fn config_help_prints_usage() {
    let sandbox = Sandbox::new("config-help");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["config", "--help"]);
    assert!(out.status.success());
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("pi config [-l] [--approve|--no-approve]"),
        "{stdout}"
    );
}

#[test]
fn config_local_requires_trust() {
    let sandbox = Sandbox::new("config-trust");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(cwd.join(".pi")).unwrap();
    fs::write(cwd.join(".pi").join("settings.json"), json!({}).to_string()).unwrap();
    let out = sandbox.pi(&cwd, &["config", "-l"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Project is not trusted. Use --approve to modify local resource config."),
        "{stderr}"
    );
}

#[test]
fn config_unknown_option_errors() {
    let sandbox = Sandbox::new("config-opt");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["config", "--bogus"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Unknown option --bogus for \"config\""),
        "{stderr}"
    );
}

// ---------------------------------------------------------------------------
// pi auth
// ---------------------------------------------------------------------------

#[test]
fn auth_help_lists_commands() {
    let sandbox = Sandbox::new("auth-help");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth"]);
    assert!(out.status.success());
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("pi auth print-api-key"), "{stdout}");
    assert!(stdout.contains("pi auth print-bearer-token"), "{stdout}");
    assert!(stdout.contains("pi auth check"), "{stdout}");
}

#[test]
fn auth_check_without_flags_errors() {
    let sandbox = Sandbox::new("auth-check-none");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "check"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Auth checks require --provider <provider> or --model <model>"),
        "{stderr}"
    );
}

#[test]
fn auth_check_no_credentials_not_ready() {
    let sandbox = Sandbox::new("auth-check-notready");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "check", "--provider", "faux"]);
    assert_eq!(out.status.code(), Some(1));
    // faux is not a built-in provider -> provider_not_found.
    let stdout = sandbox.stdout(&out);
    assert_eq!(stdout.trim(), "not_ready");
}

#[test]
fn auth_check_ready_with_stored_api_key() {
    let sandbox = Sandbox::new("auth-check-ready");
    sandbox.write_global_settings(json!({}));
    sandbox.write_auth(json!({ "google": { "type": "api_key", "key": "test-key" } }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "check", "--provider", "google"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        sandbox.stderr(&out)
    );
    assert_eq!(sandbox.stdout(&out).trim(), "ready");
}

#[test]
fn auth_check_json_ready() {
    let sandbox = Sandbox::new("auth-check-json");
    sandbox.write_global_settings(json!({}));
    sandbox.write_auth(json!({ "google": { "type": "api_key", "key": "test-key" } }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "check", "--provider", "google", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        sandbox.stderr(&out)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(sandbox.stdout(&out).trim()).expect("json");
    assert_eq!(parsed["status"], "ready");
    assert_eq!(parsed["provider"], "google");
    assert_eq!(parsed["authType"], "api_key");
}

#[test]
fn auth_check_json_with_credentials() {
    let sandbox = Sandbox::new("auth-check-cred");
    sandbox.write_global_settings(json!({}));
    sandbox.write_auth(json!({ "google": { "type": "api_key", "key": "secret-value" } }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(
        &cwd,
        &[
            "auth",
            "check",
            "--provider",
            "google",
            "--credentials",
            "--json",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let parsed: serde_json::Value =
        serde_json::from_str(sandbox.stdout(&out).trim()).expect("json");
    assert_eq!(parsed["credentials"], "secret-value");
}

#[test]
fn auth_print_api_key() {
    let sandbox = Sandbox::new("auth-key");
    sandbox.write_global_settings(json!({}));
    sandbox.write_auth(json!({ "google": { "type": "api_key", "key": "my-api-key" } }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "print-api-key", "--provider", "google"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert_eq!(sandbox.stdout(&out).trim(), "my-api-key");
}

#[test]
fn auth_print_bearer_token_oauth() {
    let sandbox = Sandbox::new("auth-bearer");
    sandbox.write_global_settings(json!({}));
    sandbox.write_auth(json!({ "google": { "type": "oauth", "access": "access-tok", "refresh": "r", "expires": 1 } }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(
        &cwd,
        &["auth", "print-bearer-token", "--provider", "google"],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert_eq!(sandbox.stdout(&out).trim(), "access-tok");
}

#[test]
fn auth_print_api_key_for_oauth_provider_errors() {
    let sandbox = Sandbox::new("auth-key-oauth");
    sandbox.write_global_settings(json!({}));
    sandbox.write_auth(
        json!({ "google": { "type": "oauth", "access": "a", "refresh": "r", "expires": 1 } }),
    );
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "print-api-key", "--provider", "google"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("configured with OAuth, not an API key"),
        "{stderr}"
    );
}

#[test]
fn auth_print_api_key_no_credentials_errors() {
    let sandbox = Sandbox::new("auth-key-none");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["auth", "print-api-key", "--provider", "google"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("No usable API key is configured"),
        "{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Subcommand dispatch does not shadow normal run mode
// ---------------------------------------------------------------------------

#[test]
fn normal_run_still_works_after_dispatch() {
    let sandbox = Sandbox::new("run-after-dispatch");
    sandbox.write_global_settings(json!({
        "defaultProvider": "faux",
        "defaultModel": "faux-1"
    }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["-p", "hello from cli commands"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("faux response to: hello from cli commands"),
        "{stdout}"
    );
}
