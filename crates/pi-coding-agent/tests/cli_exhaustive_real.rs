#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Permanent process-boundary coverage for the CLI parser.
//!
//! These tests deliberately launch the real `pi` binary. Provider turns use
//! the repository's deterministic local `faux` provider only where a
//! successful run is needed; parser/error cases stop before provider setup.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-cli-exhaustive-real-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        let home = root.join("home");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &sessions, &project] {
            fs::create_dir_all(path).expect("create isolated CLI directory");
        }
        Self {
            root,
            home,
            sessions,
            project,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .env_clear()
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("spawn real pi process")
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command();
        for (name, value) in env {
            command.env(name, value);
        }
        command.args(args).output().expect("spawn real pi process")
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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
    files
}

#[test]
fn every_inline_value_form_is_rejected_at_the_real_process_boundary() {
    let sandbox = Sandbox::new("inline-values");
    let output = sandbox.run(&[
        "--provider=faux",
        "--model=faux-1",
        "--mode=json",
        "--print",
        "hello",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(Sandbox::stdout(&output), "\n");
    assert_eq!(
        Sandbox::stderr(&output),
        "Unknown flag: --provider=faux, --model=faux-1, --mode=json\n"
    );

    let output = sandbox.run(&["-xt=bash", "--print", "hello"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(Sandbox::stdout(&output).is_empty());
    assert_eq!(
        Sandbox::stderr(&output),
        "Error: Unknown option: -xt=bash\n"
    );

    // Exercise every value-taking spelling at the process boundary. Long
    // inline forms are unknown flags; short inline forms are unknown-option
    // diagnostics. In both cases the parser must not reinterpret the value as
    // a supported option.
    for flag in [
        "--provider=faux",
        "--model=faux-1",
        "--api-key=secret",
        "--system-prompt=system",
        "--append-system-prompt=append",
        "--session=path",
        "--session-id=id",
        "--fork=source",
        "--session-dir=/tmp/sessions",
        "--name=name",
        "--tools=read",
        "--exclude-tools=bash",
        "--models=faux-1",
        "--extension=extension.ts",
        "--skill=skill.md",
        "--prompt-template=prompt.md",
        "--theme=theme.json",
        "--thinking=high",
        "--mode=json",
        "--export=export.html",
        "--use-theme=solarized",
        "--tui-mode=fullscreen",
        "--list-models=faux",
        "--print=true",
    ] {
        let output = sandbox.run(&[flag, "--print", "hello"]);
        assert_eq!(output.status.code(), Some(1), "{flag}");
        assert_eq!(Sandbox::stdout(&output), "\n", "{flag}");
        assert_eq!(
            Sandbox::stderr(&output),
            format!("Unknown flag: {flag}\n"),
            "{flag}"
        );
    }

    for flag in ["-n=name", "-t=read", "-xt=bash", "-e=extension.ts"] {
        let output = sandbox.run(&[flag, "--print", "hello"]);
        assert_eq!(output.status.code(), Some(1), "{flag}");
        assert!(Sandbox::stdout(&output).is_empty(), "{flag}");
        assert_eq!(
            Sandbox::stderr(&output),
            format!("Error: Unknown option: {flag}\n"),
            "{flag}"
        );
    }
}

#[test]
fn separate_value_forms_short_xt_and_mode_drive_a_real_faux_turn() {
    let sandbox = Sandbox::new("separate-values");
    let output = sandbox.run(&[
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "-xt",
        "bash",
        "--no-tools",
        "--no-session",
        "--print",
        "hello",
    ]);

    assert!(
        output.status.success(),
        "stderr: {}",
        Sandbox::stderr(&output)
    );
    assert_eq!(Sandbox::stdout(&output), "faux response to: hello\n");
    assert!(Sandbox::stderr(&output).is_empty());
    assert!(jsonl_files(&sandbox.sessions).is_empty());

    let output = sandbox.run(&[
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-session",
        "--print",
        "---literal",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        Sandbox::stderr(&output)
    );
    assert_eq!(Sandbox::stdout(&output), "faux response to: ---literal\n");
    assert!(Sandbox::stderr(&output).is_empty());
}

#[test]
fn verbose_print_mode_preserves_the_stdout_and_stderr_contract() {
    let sandbox = Sandbox::new("verbose-print");
    let output = sandbox.run(&[
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--print",
        "hello",
        "--verbose",
    ]);

    assert!(
        output.status.success(),
        "stderr: {}",
        Sandbox::stderr(&output)
    );
    assert_eq!(Sandbox::stdout(&output), "faux response to: hello\n");
    // `--verbose` controls interactive startup presentation only. The pinned
    // print mode does not add a session-path diagnostic to stderr.
    assert!(Sandbox::stderr(&output).is_empty());
    assert_eq!(jsonl_files(&sandbox.sessions).len(), 1);
}

#[test]
fn invalid_mode_and_missing_optional_values_follow_upstream_process_behavior() {
    let sandbox = Sandbox::new("invalid-and-missing");
    let output = sandbox.run(&[
        "--mode",
        "not-a-mode",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-session",
        "--print",
        "invalid mode is ignored",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        Sandbox::stderr(&output)
    );
    assert_eq!(
        Sandbox::stdout(&output),
        "faux response to: invalid mode is ignored\n"
    );
    assert!(Sandbox::stderr(&output).is_empty());

    // All ordinary value flags are optional at EOF in the pinned parser. The
    // environment still supplies the provider/model used by the real turn.
    let output = sandbox.run_with_env(
        &["--print", "environment fallback", "--provider"],
        &[("PI_PROVIDER", "faux"), ("PI_MODEL", "faux-1")],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        Sandbox::stderr(&output)
    );
    assert_eq!(
        Sandbox::stdout(&output),
        "faux response to: environment fallback\n"
    );
    assert!(!Sandbox::stderr(&output).contains("requires a value"));

    let output = sandbox.run_with_env(
        &["--print", "thinking fallback", "--thinking"],
        &[("PI_PROVIDER", "faux"), ("PI_MODEL", "faux-1")],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        Sandbox::stderr(&output)
    );
    assert_eq!(
        Sandbox::stdout(&output),
        "faux response to: thinking fallback\n"
    );
    assert!(!Sandbox::stderr(&output).contains("Invalid thinking level"));

    // Verify every ordinary value-taking flag is silent when it is the final
    // token, matching the pinned parser's `i + 1 < args.length` guards.
    for flag in [
        "--provider",
        "--model",
        "--api-key",
        "--system-prompt",
        "--append-system-prompt",
        "--session",
        "--session-id",
        "--fork",
        "--session-dir",
        "--tools",
        "-t",
        "--exclude-tools",
        "-xt",
        "--models",
        "--extension",
        "-e",
        "--skill",
        "--prompt-template",
        "--theme",
        "--thinking",
        "--mode",
        "--export",
    ] {
        let output = sandbox.run_with_env(
            &["--print", "missing value fallback", "--no-session", flag],
            &[("PI_PROVIDER", "faux"), ("PI_MODEL", "faux-1")],
        );
        assert!(
            output.status.success(),
            "{flag} stderr: {}",
            Sandbox::stderr(&output)
        );
        assert_eq!(
            Sandbox::stdout(&output),
            "faux response to: missing value fallback\n",
            "{flag}"
        );
        assert!(Sandbox::stderr(&output).is_empty(), "{flag}");
    }
}

#[test]
fn required_and_special_value_diagnostics_have_real_exit_and_output_contracts() {
    let sandbox = Sandbox::new("diagnostics");

    let output = sandbox.run(&["--name"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(Sandbox::stdout(&output).is_empty());
    assert_eq!(Sandbox::stderr(&output), "Error: --name requires a value\n");

    for value in ["", "   "] {
        let output = sandbox.run(&["--name", value]);
        assert_eq!(output.status.code(), Some(1), "name={value:?}");
        assert!(Sandbox::stdout(&output).is_empty(), "name={value:?}");
        assert_eq!(
            Sandbox::stderr(&output),
            "Error: --name requires a non-empty value\n",
            "name={value:?}"
        );
    }

    let output = sandbox.run(&["--use-theme", "--print", "hello"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(Sandbox::stdout(&output).is_empty());
    assert_eq!(
        Sandbox::stderr(&output),
        "Error: --use-theme requires a theme name\n"
    );

    let output = sandbox.run(&["--tui-mode"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(Sandbox::stdout(&output).is_empty());
    assert_eq!(
        Sandbox::stderr(&output),
        "Error: --tui-mode requires regular or fullscreen\n"
    );
}

#[test]
fn help_version_and_lone_dash_are_stable_process_controls() {
    let sandbox = Sandbox::new("controls");

    let output = sandbox.run(&["--help", "--version"]);
    assert!(output.status.success());
    assert!(Sandbox::stdout(&output).starts_with("pi "));
    assert!(Sandbox::stderr(&output).is_empty());

    let output = sandbox.run(&["--version", "--help"]);
    assert!(output.status.success());
    assert!(Sandbox::stdout(&output).starts_with("pi "));
    assert!(Sandbox::stderr(&output).is_empty());

    let output = sandbox.run(&["-", "--version"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(Sandbox::stdout(&output).is_empty());
    assert_eq!(Sandbox::stderr(&output), "Error: Unknown option: -\n");
}
