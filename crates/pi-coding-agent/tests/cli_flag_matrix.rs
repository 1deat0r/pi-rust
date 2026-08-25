//! Binary-level flag-matrix test (T3 #47): fire every flag in the upstream
//! `args.ts` surface and assert (a) recognized flags parse without an "unknown
//! flags" diagnostic, (b) `--help` lists them, (c) error-valued diagnostics
//! exit nonzero with an `Error:` line on stderr. This guards the CLI-surface
//! fidelity contract from PLAN.md §2.1.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-flag-matrix-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        Self {
            root,
            home,
            agent_dir,
        }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
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

/// The full upstream flag surface declared in `packages/coding-agent/src/cli/args.ts`
/// (and PLAN.md §2.1). Each entry is the arg form we expect the binary to accept.
const FLAG_SURFACE: &[&str] = &[
    "--provider",
    "--model",
    "--api-key",
    "--system-prompt",
    "--append-system-prompt",
    "--print",
    "-p",
    "--continue",
    "-c",
    "--resume",
    "-r",
    "--session",
    "--session-id",
    "--fork",
    "--session-dir",
    "--no-session",
    "--name",
    "-n",
    "--models",
    "--no-tools",
    "-nt",
    "--no-builtin-tools",
    "-nbt",
    "--tools",
    "-t",
    "--exclude-tools",
    "-xt",
    "--thinking",
    "--extension",
    "-e",
    "--no-extensions",
    "-ne",
    "--skill",
    "--no-skills",
    "-ns",
    "--prompt-template",
    "--no-prompt-templates",
    "-np",
    "--theme",
    "--use-theme",
    "--no-themes",
    "--no-context-files",
    "-nc",
    "--approve",
    "-a",
    "--no-approve",
    "-na",
    "--offline",
    "--list-models",
    "--tui-mode",
    "--mode",
    "--export",
];

#[test]
fn help_lists_full_flag_surface() {
    let sandbox = Sandbox::new("help");
    let out = sandbox.pi(&sandbox.root, &["--help"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let help = sandbox.stdout(&out);
    // Flag tokens that have no dedicated help line (bare flag names appear in
    // usage lines; every flag-form we ship should appear somewhere in --help).
    for flag in FLAG_SURFACE {
        let token = flag.trim_start_matches('-');
        assert!(
            help.contains(flag) || help.contains(&format!("--{token}")),
            "--help is missing surface flag {flag}"
        );
    }
    // Spot-check a few newly-added help lines specifically.
    for needle in [
        "--no-builtin-tools, -nbt",
        "--extension, -e",
        "--use-theme <name[/name]>",
        "--no-context-files, -nc",
        "--fork <path|id>",
    ] {
        assert!(help.contains(needle), "--help missing '{needle}'");
    }
}

#[test]
fn newly_added_boolean_flags_are_not_unknown() {
    let sandbox = Sandbox::new("booleans");
    // Previously (before T3 #46) all of these fell through to "unknown flags".
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "-p",
            "-nbt",
            "-ne",
            "-ns",
            "-np",
            "--no-themes",
            "-nc",
            "-a",
            "-na",
            "--offline",
            "hi",
        ],
    );
    let stderr = sandbox.stderr(&out);
    assert!(
        !stderr.contains("unknown flags"),
        "expected no unknown-flag diagnostic, got: {stderr}"
    );
    assert!(out.status.success(), "stderr: {stderr}");
    // The run completed (faux reply present).
    assert!(
        sandbox.stdout(&out).contains("faux response to: hi"),
        "no faux reply"
    );
}

#[test]
fn newly_added_value_flags_are_not_unknown() {
    let sandbox = Sandbox::new("values");
    // Value flags that used to be rejected as unknown. Values are dummy paths/
    // ids: parsing is what we verify here (run-path honoring is T6/T7).
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "-p",
            "--fork",
            "abc123",
            "-e",
            "/tmp/ext.ts",
            "--skill",
            "/tmp/skill",
            "--prompt-template",
            "/tmp/tpl",
            "--theme",
            "/tmp/theme",
            "--use-theme",
            "solarized",
            "--append-system-prompt",
            "extra",
            "--models",
            "anthropic,openai",
            "hi",
        ],
    );
    let stderr = sandbox.stderr(&out);
    assert!(
        !stderr.contains("unknown flags"),
        "expected no unknown-flag diagnostic, got: {stderr}"
    );
    // `--fork` is now wired through the session resolver, so this deliberately
    // nonexistent dummy target reaches a semantic error after parsing. The
    // matrix still proves that the value flag is recognized rather than
    // reported as unknown.
    assert!(
        stderr.contains("session not found: abc123"),
        "expected fork target diagnostic, got: {stderr}"
    );
    assert!(!stderr.contains("unknown flags"));
}

#[test]
fn error_diagnostic_exits_nonzero() {
    let sandbox = Sandbox::new("error-diag");
    // --use-theme followed by a flag is an error diagnostic (upstream main.ts
    // prints "Error:" and exits 1).
    let out = sandbox.pi(&sandbox.root, &["--use-theme", "--print", "hi"]);
    assert!(!out.status.success(), "expected nonzero exit");
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Error: --use-theme requires a theme name"),
        "stderr: {stderr}"
    );
}

#[test]
fn thinking_warning_does_not_exit_nonzero() {
    let sandbox = Sandbox::new("thinking-warning");
    // Invalid --thinking is a *warning* diagnostic: it prints "Warning:" but
    // the run continues (upstream behavior).
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "-p",
            "--thinking",
            "bogus",
            "hi",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Warning: Invalid thinking level"),
        "expected warning diagnostic, got: {stderr}"
    );
    assert!(
        sandbox.stdout(&out).contains("faux response to: hi"),
        "no faux reply"
    );
}

#[test]
fn rpc_rejects_file_arguments_before_starting_the_protocol() {
    let sandbox = Sandbox::new("rpc-file-boundary");
    let out = sandbox.pi(&sandbox.root, &["--mode", "rpc", "@prompt.txt"]);
    assert!(!out.status.success(), "RPC @file input must be rejected");
    assert_eq!(
        sandbox.stderr(&out),
        "Error: @file arguments are not supported in RPC mode\n"
    );
    assert!(
        sandbox.stdout(&out).is_empty(),
        "protocol output must not start after a CLI boundary error"
    );
}
