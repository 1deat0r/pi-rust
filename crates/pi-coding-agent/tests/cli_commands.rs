#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Binary-level tests for the pi CLI package/config/auth commands
//! (`pi install/remove/uninstall/update/list`, `pi config`, `pi auth`).
//!
//! Each test runs the real `pi` binary with a sandboxed $HOME and
//! PI_CODING_AGENT_DIR so no real home directory or setting file is touched.
//! A fake `npm` configured through the settings `npmCommand` array lets the
//! rejection tests prove that the disabled package manager is never invoked.

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

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .env_clear()
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command.args(args);
        command
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        self.command(cwd, args).output().expect("spawn pi")
    }

    fn pi_offline(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        let mut command = self.command(cwd, args);
        command.env("PI_OFFLINE", "1");
        command.output().expect("spawn pi")
    }

    fn pi_with_version_endpoint(
        &self,
        cwd: &Path,
        args: &[&str],
        endpoint: &str,
    ) -> std::process::Output {
        let mut command = self.command(cwd, args);
        command.env("PI_VERSION_CHECK_URL", endpoint);
        command.output().expect("spawn pi")
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

fn assert_npm_rejected(sandbox: &Sandbox, out: &std::process::Output, source: &str) {
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {}",
        sandbox.stdout(out)
    );
    assert_eq!(
        sandbox.stderr(out),
        format!(
            "Error: Rust-native-only package policy: JavaScript/TypeScript package execution is disabled; npm, npx, and bun are not invoked. Register compiled Rust extensions or use local/git skills, prompts, and themes. Unsupported source: {source}\n"
        )
    );
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
fn install_local_package_persists_project_settings_with_approve() {
    let sandbox = Sandbox::new("install-local-project");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(cwd.join("pkg").join("skills")).unwrap();

    let out = sandbox.pi(&cwd, &["install", "--local", "--approve", "./pkg"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(sandbox.stdout(&out).contains("Installed ./pkg"));

    let project_settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cwd.join(".pi").join("settings.json")).unwrap())
            .unwrap();
    let packages = project_settings["packages"]
        .as_array()
        .expect("project packages array");
    assert_eq!(packages.len(), 1);
    let source = packages[0].as_str().expect("project package source");
    assert!(
        source.starts_with(".."),
        "source should be project-relative: {source}"
    );
    assert!(
        source.ends_with("pkg"),
        "source should name the package: {source}"
    );
    assert_eq!(sandbox.read_global_settings(), json!({}));

    let removed = sandbox.pi(&cwd, &["remove", "--local", "--approve", "./pkg"]);
    assert!(
        removed.status.success(),
        "stderr: {}",
        sandbox.stderr(&removed)
    );
    assert!(sandbox.stdout(&removed).contains("Removed ./pkg"));
    let project_settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cwd.join(".pi").join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(project_settings["packages"], json!([]));
}

#[test]
fn local_package_commands_require_trust_and_preserve_project_settings() {
    let sandbox = Sandbox::new("install-local-trust");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(cwd.join("pkg")).unwrap();
    fs::create_dir_all(cwd.join(".pi")).unwrap();
    let original = json!({ "packages": ["./existing"] });
    fs::write(cwd.join(".pi").join("settings.json"), original.to_string()).unwrap();

    let out = sandbox.pi(&cwd, &["install", "--local", "./pkg"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        sandbox.stderr(&out),
        "Project is not trusted. Use --approve to modify local package config.\n"
    );
    let persisted: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cwd.join(".pi").join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(persisted, original);
}

#[test]
fn install_npm_with_fake_npm() {
    let sandbox = Sandbox::new("install-npm");
    let fake = sandbox.write_fake_npm();
    sandbox.write_global_settings(json!({ "npmCommand": [fake.to_string_lossy()] }));
    let cwd = project(&sandbox, "work");

    let out = sandbox.pi(&cwd, &["install", "npm:demo-pkg"]);
    assert_npm_rejected(&sandbox, &out, "npm:demo-pkg");

    // The fake npm was not invoked, so no managed layout appeared.
    let installed = sandbox
        .agent_dir
        .join("npm")
        .join("node_modules")
        .join("demo-pkg")
        .join("package.json");
    assert!(
        !installed.exists(),
        "Rust-native rejection must not invoke fake npm"
    );

    // The settings entry was not persisted.
    let settings = sandbox.read_global_settings();
    assert!(settings.get("packages").is_none());
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
    assert_npm_rejected(&sandbox, &out, "npm:demo-pkg");
    let settings = sandbox.read_global_settings();
    assert_eq!(settings["packages"], json!(["npm:demo-pkg"]));
    assert!(sandbox
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
    assert_npm_rejected(&sandbox, &out, "npm:ghost");
}

#[test]
fn uninstall_alias_works() {
    let sandbox = Sandbox::new("uninstall");
    sandbox.write_global_settings(json!({ "packages": ["npm:ghost"] }));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["uninstall", "npm:ghost"]);
    assert_npm_rejected(&sandbox, &out, "npm:ghost");
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
fn update_self_reports_rust_install_boundary_without_upstream_query() {
    let sandbox = Sandbox::new("update-self");
    sandbox.write_global_settings(json!({ "packages": [] }));
    let cwd = project(&sandbox, "work");
    // An invalid endpoint makes any accidental upstream version request fail
    // immediately, without relying on network access or a live server.
    let out = sandbox.pi_with_version_endpoint(&cwd, &["update"], "http://[::1");
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("Extensions are skipped. Run pi update --extensions to update extensions."),
        "{stdout}"
    );
    assert!(!out.status.success());
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("pi-rust cannot self-update this installation."),
        "{stderr}"
    );
    assert!(
        stderr.contains("pi-rust cannot self-update this compiled Rust installation."),
        "{stderr}"
    );
    assert!(
        stderr.contains("Update pi-rust from its source repository"),
        "{stderr}"
    );
    assert!(!stdout.contains("Update available"), "{stdout}");
    assert!(!stderr.contains("pi.dev"), "{stderr}");
}

#[test]
fn update_help_does_not_claim_upstream_pi_self_update() {
    let sandbox = Sandbox::new("update-help");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi(&cwd, &["update", "--help"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("Explain how to update the pi-rust Rust binary"),
        "{stdout}"
    );
    assert!(!stdout.contains("Update pi only"), "{stdout}");
    assert!(!stdout.contains("npm"), "{stdout}");
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
    assert_npm_rejected(&sandbox, &out, "npm:demo-pkg");
    assert_eq!(
        fs::read_to_string(
            sandbox
                .agent_dir
                .join("npm")
                .join("node_modules")
                .join("demo-pkg")
                .join("package.json")
        )
        .unwrap(),
        r#"{ "name": "demo-pkg", "version": "1.0.0" }"#
    );
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
    assert_npm_rejected(&sandbox, &out, "npm:demo-pkg");
    assert_eq!(
        fs::read_to_string(
            sandbox
                .agent_dir
                .join("npm")
                .join("node_modules")
                .join("demo-pkg")
                .join("package.json")
        )
        .unwrap(),
        r#"{ "name": "demo-pkg", "version": "1.0.0" }"#
    );
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
fn config_local_approved_summary_discovers_resources_and_ignores_source_extensions() {
    let sandbox = Sandbox::new("config-local-summary");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(cwd.join(".pi").join("skills").join("review")).unwrap();
    fs::create_dir_all(cwd.join(".pi").join("prompts")).unwrap();
    fs::create_dir_all(cwd.join(".pi").join("themes")).unwrap();
    fs::create_dir_all(cwd.join(".pi").join("extensions")).unwrap();
    fs::write(
        cwd.join(".pi")
            .join("skills")
            .join("review")
            .join("SKILL.md"),
        "---\nname: review\ndescription: Review files\n---\nbody",
    )
    .unwrap();
    fs::write(
        cwd.join(".pi").join("prompts").join("brief.md"),
        "---\ndescription: Brief\n---\nBrief $@",
    )
    .unwrap();
    fs::write(cwd.join(".pi").join("themes").join("night.json"), "{}").unwrap();
    fs::write(
        cwd.join(".pi").join("extensions").join("unsupported.ts"),
        "export default {}",
    )
    .unwrap();

    let out = sandbox.pi(&cwd, &["config", "--local", "--approve"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("pi config (write scope: project)"),
        "{stdout}"
    );
    assert!(stdout.contains("Project resources:"), "{stdout}");
    assert!(stdout.contains("review"), "{stdout}");
    assert!(stdout.contains("brief"), "{stdout}");
    assert!(stdout.contains("night"), "{stdout}");
    assert!(
        !stdout.contains("unsupported.ts"),
        "source-language extension must not be discoverable: {stdout}"
    );
}

#[test]
fn config_settings_directory_is_a_user_visible_startup_failure() {
    let sandbox = Sandbox::new("config-settings-directory");
    let cwd = project(&sandbox, "work");
    fs::create_dir_all(sandbox.agent_dir.join("settings.json")).unwrap();

    let out = sandbox.pi(&cwd, &["config"]);
    assert_eq!(out.status.code(), Some(101));
    let stderr = sandbox.stderr(&out);
    assert!(stderr.contains("Failed to read settings file"), "{stderr}");
    assert!(
        stderr.contains("Is a directory") || stderr.contains("is a directory"),
        "{stderr}"
    );
    assert!(sandbox.stdout(&out).is_empty());
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

#[test]
fn main_help_lists_mode_option() {
    let sandbox = Sandbox::new("main-help");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(&cwd, &["--help"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(sandbox.stdout(&out).contains("--mode <mode>"));
    assert!(sandbox.stderr(&out).is_empty());
}

#[test]
fn unknown_flag_matches_upstream_diagnostic() {
    let sandbox = Sandbox::new("unknown-flag");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(&cwd, &["--definitely-not-a-real-flag"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(sandbox.stdout(&out), "\n");
    assert_eq!(
        sandbox.stderr(&out),
        "Unknown flag: --definitely-not-a-real-flag\n"
    );
}

#[test]
fn invalid_tui_mode_is_reported_before_startup() {
    let sandbox = Sandbox::new("invalid-tui-mode");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(&cwd, &["--tui-mode", "sideways"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(sandbox.stdout(&out).is_empty());
    assert_eq!(
        sandbox.stderr(&out),
        "Error: Invalid TUI mode \"sideways\". Valid values: regular, fullscreen\n"
    );
}

#[test]
fn update_conflicting_targets_report_a_deterministic_error() {
    let sandbox = Sandbox::new("update-conflict");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(&cwd, &["update", "--all", "--self"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains(
            "--all cannot be combined with --self, --extensions, --models, or --extension"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("pi update"), "{stderr}");
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
fn auth_check_json_unknown_provider_reports_reason() {
    let sandbox = Sandbox::new("auth-check-json-unknown");
    sandbox.write_global_settings(json!({}));
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(
        &cwd,
        &["auth", "check", "--provider", "not-a-provider", "--json"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(sandbox.stderr(&out).is_empty());
    let parsed: serde_json::Value =
        serde_json::from_str(sandbox.stdout(&out).trim()).expect("json");
    assert_eq!(parsed["status"], "not_ready");
    assert_eq!(parsed["provider"], "not-a-provider");
    assert_eq!(parsed["reason"], "provider_not_found");
}

#[test]
fn auth_check_json_malformed_credentials_reports_invalid() {
    let sandbox = Sandbox::new("auth-check-malformed");
    sandbox.write_global_settings(json!({}));
    fs::write(sandbox.agent_dir.join("auth.json"), "{not-json").unwrap();
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(
        &cwd,
        &[
            "auth",
            "check",
            "--provider",
            "google",
            "--no-refresh",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(sandbox.stderr(&out).is_empty());
    let parsed: serde_json::Value =
        serde_json::from_str(sandbox.stdout(&out).trim()).expect("json");
    assert_eq!(parsed["status"], "invalid");
    assert_eq!(parsed["provider"], "google");
    assert_eq!(parsed["reason"], "invalid_state");
}

#[test]
fn auth_check_rejects_unknown_options_before_loading_credentials() {
    let sandbox = Sandbox::new("auth-check-option");
    let cwd = project(&sandbox, "work");
    let out = sandbox.pi_offline(&cwd, &["auth", "check", "--provider", "google", "--bogus"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Unknown option --bogus for \"auth check\"."),
        "{stderr}"
    );
    assert!(stderr.contains("pi auth check"), "{stderr}");
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
