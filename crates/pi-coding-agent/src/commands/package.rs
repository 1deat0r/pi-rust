//! Package commands — port of `packages/coding-agent/src/package-manager-cli.ts`
//! (parse + dispatch for `pi install/remove/uninstall/update/list`).
//!
//! Output/exit-code parity with upstream:
//! - `Installed <source>` / `Removed <source>` / `Updated <source>`
//! - `No matching package found for <source>` (remove) exits 1
//! - list format (`User packages:` / `Project packages:` sections,
//!   two-space indented source lines, dimmed installed path under each)
//! - help text and usage/unknown-option/conflicting-option error messages.
//!
//! Divergence: `pi update --self` cannot self-update a compiled Rust binary;
//! the port prints the upstream-style "cannot self-update" instruction.

use crate::config::{self, APP_NAME, CONFIG_DIR_NAME};
use crate::core::package_manager::PackageManager;
use crate::core::remote_catalog_provider::refresh_catalogs;
use crate::core::settings::SettingsManager;

/// Package command kind (upstream `PackageCommand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageCommandKind {
    Install,
    Remove,
    Update,
    List,
}

/// Update target selection (upstream `UpdateTarget`).
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateTarget {
    All,
    Self_,
    Extensions { source: Option<String> },
    Models,
}

#[derive(Debug, Clone, Default)]
pub struct PackageCommandOptions {
    pub command: Option<PackageCommandKind>,
    pub source: Option<String>,
    pub update_target: Option<UpdateTarget>,
    pub show_extensions_skipped_note: bool,
    pub local: bool,
    pub force: bool,
    pub project_trust_override: Option<bool>,
    pub help: bool,
    pub invalid_option: Option<String>,
    pub invalid_argument: Option<String>,
    pub missing_option_value: Option<String>,
    pub conflicting_options: Option<String>,
}

pub fn get_package_command_usage(command: PackageCommandKind) -> String {
    match command {
        PackageCommandKind::Install => format!("{APP_NAME} install <source> [-l] [--approve|--no-approve]"),
        PackageCommandKind::Remove => format!("{APP_NAME} remove <source> [-l] [--approve|--no-approve]"),
        PackageCommandKind::Update => format!(
            "{APP_NAME} update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
        ),
        PackageCommandKind::List => format!("{APP_NAME} list [--approve|--no-approve]"),
    }
}

pub const CONFIG_COMMAND_USAGE: &str = "pi config [-l] [--approve|--no-approve]";

/// Port of `parsePackageCommand(args)`.
pub fn parse_package_command(args: &[String]) -> Option<PackageCommandOptions> {
    if args.is_empty() {
        return None;
    }
    let raw_command = &args[0];
    let command = if raw_command == "uninstall" {
        Some(PackageCommandKind::Remove)
    } else if matches!(
        raw_command.as_str(),
        "install" | "remove" | "update" | "list"
    ) {
        match raw_command.as_str() {
            "install" => Some(PackageCommandKind::Install),
            "remove" => Some(PackageCommandKind::Remove),
            "update" => Some(PackageCommandKind::Update),
            _ => Some(PackageCommandKind::List),
        }
    } else {
        None
    };
    let command = command?;

    let mut options = PackageCommandOptions {
        command: Some(command),
        ..Default::default()
    };
    let mut self_flag = false;
    let mut extensions_flag = false;
    let mut models_flag = false;
    let mut all_flag = false;
    let mut extension_flag_source: Option<String> = None;
    let rest = &args[1..];
    let mut index = 0usize;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "-h" || arg == "--help" {
            options.help = true;
            index += 1;
            continue;
        }
        if arg == "-l" || arg == "--local" {
            if matches!(
                command,
                PackageCommandKind::Install | PackageCommandKind::Remove
            ) {
                options.local = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.clone());
            }
            index += 1;
            continue;
        }
        if arg == "--self" {
            if command == PackageCommandKind::Update {
                self_flag = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.clone());
            }
            index += 1;
            continue;
        }
        if arg == "--extensions" {
            if command == PackageCommandKind::Update {
                extensions_flag = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.clone());
            }
            index += 1;
            continue;
        }
        if arg == "--models" {
            if command == PackageCommandKind::Update {
                models_flag = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.clone());
            }
            index += 1;
            continue;
        }
        if arg == "--all" {
            if command == PackageCommandKind::Update {
                all_flag = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.clone());
            }
            index += 1;
            continue;
        }
        if arg == "--approve" || arg == "-a" {
            options.project_trust_override = Some(true);
            index += 1;
            continue;
        }
        if arg == "--no-approve" || arg == "-na" {
            options.project_trust_override = Some(false);
            index += 1;
            continue;
        }
        if arg == "--force" {
            if command == PackageCommandKind::Update {
                options.force = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.clone());
            }
            index += 1;
            continue;
        }
        if arg == "--extension" {
            if command != PackageCommandKind::Update {
                options.invalid_option.get_or_insert_with(|| arg.clone());
                index += 1;
                continue;
            }
            let value = rest.get(index + 1).cloned();
            match value {
                Some(value) if !value.starts_with('-') => {
                    if extension_flag_source.is_some() {
                        options.conflicting_options.get_or_insert_with(|| {
                            "--extension can only be provided once".to_string()
                        });
                    } else {
                        extension_flag_source = Some(value);
                    }
                    index += 2;
                }
                _ => {
                    options
                        .missing_option_value
                        .get_or_insert_with(|| arg.clone());
                    index += 1;
                }
            }
            continue;
        }
        if arg.starts_with('-') {
            options.invalid_option.get_or_insert_with(|| arg.clone());
            index += 1;
            continue;
        }
        if options.source.is_none() {
            options.source = Some(arg.clone());
        } else {
            options.invalid_argument.get_or_insert_with(|| arg.clone());
        }
        index += 1;
    }

    // Resolve the final update target with upstream conflict rules.
    if command == PackageCommandKind::Update {
        let extension_source = extension_flag_source;

        if all_flag && (self_flag || extensions_flag || models_flag || extension_source.is_some()) {
            options.conflicting_options.get_or_insert_with(|| {
                "--all cannot be combined with --self, --extensions, --models, or --extension"
                    .to_string()
            });
        }
        if all_flag && options.source.is_some() {
            options.conflicting_options.get_or_insert_with(|| {
                "--all cannot be combined with a positional source".to_string()
            });
        }

        let update_target: Option<UpdateTarget> = if models_flag {
            if self_flag || extensions_flag || all_flag || extension_source.is_some() {
                options
                    .conflicting_options
                    .get_or_insert_with(|| "--models cannot be combined with --self, --extensions, --all, or --extension".to_string());
            }
            if options.source.is_some() {
                options.conflicting_options.get_or_insert_with(|| {
                    "--models cannot be combined with a positional source".to_string()
                });
            }
            Some(UpdateTarget::Models)
        } else if let Some(source) = extension_source {
            if self_flag || extensions_flag || all_flag {
                options.conflicting_options.get_or_insert_with(|| {
                    "--extension cannot be combined with --self, --extensions, or --all".to_string()
                });
            }
            if options.source.is_some() {
                options.conflicting_options.get_or_insert_with(|| {
                    "--extension cannot be combined with a positional source".to_string()
                });
            }
            Some(UpdateTarget::Extensions {
                source: Some(source),
            })
        } else if let Some(source) = options.source.clone() {
            let source_is_self = source == "self" || source == "pi";
            if source_is_self {
                if extensions_flag {
                    options.update_target = Some(UpdateTarget::All);
                } else {
                    options.update_target = Some(UpdateTarget::Self_);
                }
            } else {
                if extensions_flag || self_flag || all_flag {
                    options
                        .conflicting_options
                        .get_or_insert_with(|| "positional update targets cannot be combined with --self, --extensions, or --all".to_string());
                }
                options.update_target = Some(UpdateTarget::Extensions {
                    source: Some(source),
                });
            }
            None
        } else {
            // No positional source; resolve flags.
            if all_flag || (self_flag && extensions_flag) {
                Some(UpdateTarget::All)
            } else if self_flag {
                Some(UpdateTarget::Self_)
            } else if extensions_flag {
                Some(UpdateTarget::Extensions { source: None })
            } else {
                options.show_extensions_skipped_note = true;
                Some(UpdateTarget::Self_)
            }
        };
        if update_target.is_some() {
            options.update_target = update_target;
        }
    }

    Some(options)
}

fn print_package_command_help(command: PackageCommandKind) {
    match command {
        PackageCommandKind::Install => {
            println!("Usage:");
            println!("  {}", get_package_command_usage(command));
            println!();
            println!("Install a package and add it to settings.");
            println!();
            println!("Options:");
            println!(
                "  -l, --local       Install project-locally ({CONFIG_DIR_NAME}/settings.json)"
            );
            println!("  -a, --approve     Trust project-local files for this command");
            println!("  -na, --no-approve Ignore project-local files for this command");
            println!();
            println!("Examples:");
            println!("  {APP_NAME} install npm:@foo/bar");
            println!("  {APP_NAME} install git:github.com/user/repo");
            println!("  {APP_NAME} install git:git@github.com:user/repo");
            println!("  {APP_NAME} install https://github.com/user/repo");
            println!("  {APP_NAME} install ssh://git@github.com/user/repo");
            println!("  {APP_NAME} install ./local/path");
        }
        PackageCommandKind::Remove => {
            println!("Usage:");
            println!("  {}", get_package_command_usage(command));
            println!();
            println!("Remove a package and its source from settings.");
            println!("Alias: {APP_NAME} uninstall <source> [-l]");
            println!();
            println!("Options:");
            println!("  -l, --local       Remove from project settings ({CONFIG_DIR_NAME}/settings.json)");
            println!("  -a, --approve     Trust project-local files for this command");
            println!("  -na, --no-approve Ignore project-local files for this command");
            println!();
            println!("Examples:");
            println!("  {APP_NAME} remove npm:@foo/bar");
            println!("  {APP_NAME} uninstall npm:@foo/bar");
        }
        PackageCommandKind::Update => {
            println!("Usage:");
            println!("  {}", get_package_command_usage(command));
            println!();
            println!("Update pi, installed packages, or model catalogs.");
            println!();
            println!("Options:");
            println!("  --self                  Update pi only (default when no target is given)");
            println!("  --extensions            Update installed packages only");
            println!("  --models                Refresh model catalogs only");
            println!("  --all                   Update pi and installed packages");
            println!("  --extension <source>    Update one package only");
            println!("  -a, --approve           Trust project-local files for this command");
            println!("  -na, --no-approve       Ignore project-local files for this command");
            println!(
                "  --force                 Reinstall pi even if the current version is latest"
            );
            println!();
            println!("Short forms:");
            println!("  {APP_NAME} update                Update pi only");
            println!("  {APP_NAME} update --all          Update pi and all extensions");
            println!("  {APP_NAME} update --models       Refresh model catalogs only");
            println!("  {APP_NAME} update <source>       Update one package");
            println!(
                "  {APP_NAME} update pi             Update pi only (self works as alias to pi)"
            );
        }
        PackageCommandKind::List => {
            println!("Usage:");
            println!("  {}", get_package_command_usage(command));
            println!();
            println!("List installed packages from user and project settings.");
            println!();
            println!("Options:");
            println!("  -a, --approve      Trust project-local files for this command");
            println!("  -na, --no-approve  Ignore project-local files for this command");
        }
    }
}

fn report_settings_errors(settings_manager: &mut SettingsManager, context: &str) {
    for error in settings_manager.drain_errors() {
        let scope = match error.scope {
            crate::core::settings::SettingsScope::Global => "global",
            crate::core::settings::SettingsScope::Project => "project",
        };
        eprintln!("Warning ({context}, {scope} settings): {}", error.error);
    }
}

/// Port of `handlePackageCommand(args)` + the error/help branches. Returns
/// true when the args were a package command (handled).
pub async fn handle_package_command(args: &[String]) -> bool {
    let Some(options) = parse_package_command(args) else {
        return false;
    };
    let command = options.command.unwrap();

    if options.help {
        print_package_command_help(command);
        return true;
    }
    if let Some(option) = &options.invalid_option {
        eprintln!("Unknown option {option} for \"{}\".", command_name(command));
        eprintln!(
            "Use \"{APP_NAME} --help\" or \"{}\".",
            get_package_command_usage(command)
        );
        std::process::exit(1);
    }
    if let Some(option) = &options.missing_option_value {
        eprintln!("Missing value for {option}.");
        eprintln!("Usage: {}", get_package_command_usage(command));
        std::process::exit(1);
    }
    if let Some(argument) = &options.invalid_argument {
        eprintln!("Unexpected argument {argument}.");
        eprintln!("Usage: {}", get_package_command_usage(command));
        std::process::exit(1);
    }
    if let Some(message) = &options.conflicting_options {
        eprintln!("{message}");
        eprintln!("Usage: {}", get_package_command_usage(command));
        std::process::exit(1);
    }

    let source = options.source.clone();
    if matches!(
        command,
        PackageCommandKind::Install | PackageCommandKind::Remove
    ) && source.is_none()
    {
        eprintln!("Missing {} source.", command_name(command));
        eprintln!("Usage: {}", get_package_command_usage(command));
        std::process::exit(1);
    }

    if command == PackageCommandKind::Update
        && matches!(options.update_target, Some(UpdateTarget::Models))
    {
        match refresh_catalogs(&agent_dir_for_catalog(), true).await {
            Ok(updated) => {
                println!("Model catalogs refreshed ({updated} providers)");
                return true;
            }
            Err(error) => {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
        }
    }

    let cwd = config::cwd();
    let agent_dir = config::get_agent_dir();
    let writes_project_package_config = matches!(
        command,
        PackageCommandKind::Install | PackageCommandKind::Remove
    ) && options.local;

    // Package commands use saved project trust only (upstream
    // `useSavedProjectTrustOnly` for update; interactive prompt for
    // install/remove — the port defaults to untrusted without a prompt).
    let settings_trusted = options.project_trust_override.unwrap_or(false);
    let mut settings = SettingsManager::create(
        &cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions {
            project_trusted: settings_trusted,
        },
    );
    if writes_project_package_config && !settings.is_project_trusted() {
        eprintln!("Project is not trusted. Use --approve to modify local package config.");
        std::process::exit(1);
    }
    report_settings_errors(&mut settings, "package command");

    let mut package_manager =
        PackageManager::new(crate::core::package_manager::PackageManagerOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.display().to_string(),
            settings_manager: settings,
        });

    package_manager.set_progress_callback(Some(Box::new(|event| {
        // Upstream streams the dimmed progress line to stdout.
        if event.event_type == "start" {
            if let Some(message) = &event.message {
                println!("{message}");
            }
        }
    })));

    match command {
        PackageCommandKind::Install => {
            if let Err(error) =
                package_manager.install_and_persist(source.as_deref().unwrap_or(""), options.local)
            {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
            println!("Installed {}", source.unwrap_or_default());
        }
        PackageCommandKind::Remove => {
            let removed = match package_manager
                .remove_and_persist(source.as_deref().unwrap_or(""), options.local)
            {
                Ok(removed) => removed,
                Err(error) => {
                    eprintln!("Error: {error}");
                    std::process::exit(1);
                }
            };
            if !removed {
                eprintln!(
                    "No matching package found for {}",
                    source.unwrap_or_default()
                );
                std::process::exit(1);
            }
            println!("Removed {}", source.unwrap_or_default());
        }
        PackageCommandKind::List => {
            let configured = package_manager.list_configured_packages();
            let user_packages: Vec<_> = configured.iter().filter(|p| p.scope == "user").collect();
            let project_packages: Vec<_> =
                configured.iter().filter(|p| p.scope == "project").collect();
            if configured.is_empty() {
                println!("No packages installed.");
                return true;
            }
            if !user_packages.is_empty() {
                println!("User packages:");
                for pkg in &user_packages {
                    let display = if pkg.filtered {
                        format!("{} (filtered)", pkg.source)
                    } else {
                        pkg.source.clone()
                    };
                    println!("  {display}");
                    if let Some(path) = &pkg.installed_path {
                        println!("    {path}");
                    }
                }
            }
            if !project_packages.is_empty() {
                if !user_packages.is_empty() {
                    println!();
                }
                println!("Project packages:");
                for pkg in &project_packages {
                    let display = if pkg.filtered {
                        format!("{} (filtered)", pkg.source)
                    } else {
                        pkg.source.clone()
                    };
                    println!("  {display}");
                    if let Some(path) = &pkg.installed_path {
                        println!("    {path}");
                    }
                }
            }
        }
        PackageCommandKind::Update => {
            let target = options.update_target.clone().unwrap_or(UpdateTarget::Self_);
            if options.show_extensions_skipped_note {
                println!("Extensions are skipped. Run {APP_NAME} update --extensions to update extensions.");
            }
            let includes_extensions =
                matches!(target, UpdateTarget::All | UpdateTarget::Extensions { .. });
            let includes_self = matches!(target, UpdateTarget::All | UpdateTarget::Self_);
            if includes_extensions {
                let update_source = match &target {
                    UpdateTarget::Extensions { source } => source.clone(),
                    _ => None,
                };
                match package_manager.update(update_source.as_deref()) {
                    Ok(_) => {
                        if let Some(source) = &update_source {
                            println!("Updated {source}");
                        } else {
                            println!("Updated packages");
                        }
                    }
                    Err(error) => {
                        eprintln!("Error: {error}");
                        std::process::exit(1);
                    }
                }
            }
            if includes_self {
                // The compiled Rust port cannot self-update; use the
                // upstream-style unavailable instruction (documented
                // divergence).
                eprintln!("error: {APP_NAME} cannot self-update this installation.");
                eprintln!("Update pi using the package manager, wrapper, or source checkout that provides this installation.");
                std::process::exit(1);
            }
        }
    }

    // The upstream process.exit()s package commands so lingering extension
    // handles cannot keep the process alive; the Rust port has no such
    // handles but keeps the exit-code semantics for callers.
    true
}

fn agent_dir_for_catalog() -> std::path::PathBuf {
    config::get_agent_dir()
}

/// The subcommand name without the `uninstall` alias.
pub fn command_name(command: PackageCommandKind) -> &'static str {
    match command {
        PackageCommandKind::Install => "install",
        PackageCommandKind::Remove => "remove",
        PackageCommandKind::Update => "update",
        PackageCommandKind::List => "list",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &[&str]) -> PackageCommandOptions {
        let args: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        parse_package_command(&args).expect("expected package command")
    }

    #[test]
    fn parses_install() {
        let options = parse(&["install", "npm:@foo/bar"]);
        assert_eq!(options.command, Some(PackageCommandKind::Install));
        assert_eq!(options.source.as_deref(), Some("npm:@foo/bar"));
        assert!(!options.local);
    }

    #[test]
    fn parses_install_local_and_approve() {
        let options = parse(&["install", "-l", "-a", "./pkg"]);
        assert!(options.local);
        assert_eq!(options.project_trust_override, Some(true));
    }

    #[test]
    fn parses_uninstall_alias() {
        let options = parse(&["uninstall", "npm:x"]);
        assert_eq!(options.command, Some(PackageCommandKind::Remove));
    }

    #[test]
    fn install_missing_source_is_ok_at_parse() {
        let options = parse(&["install"]);
        assert_eq!(options.command, Some(PackageCommandKind::Install));
        assert!(options.source.is_none());
    }

    #[test]
    fn local_flag_rejected_for_list() {
        let options = parse(&["list", "-l"]);
        assert!(options.invalid_option.is_some());
    }

    #[test]
    fn update_defaults_to_self_with_skip_note() {
        let options = parse(&["update"]);
        assert_eq!(options.update_target, Some(UpdateTarget::Self_));
        assert!(options.show_extensions_skipped_note);
    }

    #[test]
    fn update_source_resolves_to_extension_update() {
        let options = parse(&["update", "npm:a"]);
        assert_eq!(
            options.update_target,
            Some(UpdateTarget::Extensions {
                source: Some("npm:a".into())
            })
        );
        assert!(!options.show_extensions_skipped_note);
    }

    #[test]
    fn update_self_alias() {
        let options = parse(&["update", "pi"]);
        assert_eq!(options.update_target, Some(UpdateTarget::Self_));
    }

    #[test]
    fn update_models_conflicts_with_source() {
        let options = parse(&["update", "--models", "pkg"]);
        assert!(options.conflicting_options.is_some());
    }

    #[test]
    fn update_all_conflicts_with_self() {
        let options = parse(&["update", "--all", "--self"]);
        assert!(options.conflicting_options.is_some());
    }

    #[test]
    fn update_extension_flag() {
        let options = parse(&["update", "--extension", "npm:x"]);
        assert_eq!(
            options.update_target,
            Some(UpdateTarget::Extensions {
                source: Some("npm:x".into())
            })
        );
    }

    #[test]
    fn update_extension_missing_value() {
        let options = parse(&["update", "--extension"]);
        assert!(options.missing_option_value.is_some());
    }

    #[test]
    fn not_a_package_command() {
        let args = vec!["-p".to_string(), "hello".to_string()];
        assert!(parse_package_command(&args).is_none());
        let args = vec!["run".to_string()];
        assert!(parse_package_command(&args).is_none());
    }

    #[test]
    fn multiple_positionals_rejected() {
        let options = parse(&["remove", "a", "b"]);
        assert!(options.invalid_argument.is_some());
    }
}
