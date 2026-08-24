//! Non-interactive run path — the `pi -p` / `pi <message>` flow. Wires the
//! provider (faux for tests; other providers as they are ported), the agent
//! loop, and session persistence.
//!
//! Provider/model resolution order (1:1 with upstream `findInitialModel` for
//! the one-shot path, plus the port's documented env surface):
//!   CLI `--provider`/`--model` → `PI_PROVIDER`/`PI_MODEL` env →
//!   settings.json `defaultProvider`/`defaultModel` (project merged over
//!   global) → hard default `google` / provider default.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi_agent::fs::StdFileSystem;
use pi_agent::harness::compaction::{
    compact, estimate_context_tokens, prepare_compaction, should_compact, CompactionSettings,
};
use pi_agent::harness::SimpleModels;
use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::session::context::{build_session_context, SessionContextBuildOptions};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::types::{Entry, EntryNoStats, SessionMetadata};
use pi_agent::session::{CreateOptions, ForkOptions, JsonlSessionRepo, Session};
use pi_agent::tools::image::{
    detect_supported_image_mime_type, process_image, ProcessImageOptions,
};
use pi_agent::types::AgentMessage;
use pi_ai::types::{ContentBlock, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

/// Provider stream function: `(model, context) -> event stream`.
pub type StreamFn = Arc<
    dyn Fn(&pi_ai::model::Model, &pi_ai::types::Context) -> pi_ai::AssistantMessageEventStream
        + Send
        + Sync,
>;

pub struct RunOutcome {
    pub final_text: String,
    pub session_path: Option<String>,
}

/// Provider resolution for the run path: CLI → env → settings → `google`.
pub fn resolve_run_provider(cli_provider: Option<&str>, settings: &SettingsManager) -> String {
    cli_provider
        .map(|s| s.to_string())
        .or_else(|| config::env(config::ENV_PROVIDER))
        .or_else(|| settings.get_default_provider().map(|s| s.to_string()))
        .unwrap_or_else(|| "google".to_string())
}

/// Model-hint resolution for the run path: CLI → env → settings → None.
///
/// `apply_settings_default` gates the settings stage: upstream pairs
/// settings `defaultProvider`+`defaultModel` as a unit and resolves models
/// from the provider's own scope once an explicit provider source (CLI/env)
/// is present, so the settings default model must not leak into that scope.
pub fn resolve_run_model(
    cli_model: Option<&str>,
    settings: &SettingsManager,
    apply_settings_default: bool,
) -> Option<String> {
    cli_model
        .map(|s| s.to_string())
        .or_else(|| config::env(config::ENV_MODEL))
        .or_else(|| {
            if apply_settings_default {
                settings.get_default_model().map(|s| s.to_string())
            } else {
                None
            }
        })
}

/// True when an explicit provider source (CLI flag or PI_PROVIDER env) is in
/// play; settings defaults then apply only to the model stage at most.
pub fn has_explicit_provider(cli_provider: Option<&str>) -> bool {
    cli_provider.is_some() || config::env(config::ENV_PROVIDER).is_some()
}

/// Read the global `defaultProjectTrust` setting (allow/deny/ask) without
/// loading project settings (which are themselves trust-gated).
fn settings_default_project_trust(agent_dir: &std::path::Path) -> Option<String> {
    let path = agent_dir.join("settings.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let content = crate::core::settings::strip_bom(&raw);
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("defaultProjectTrust")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn project_trust_override(args: &Args) -> Option<bool> {
    if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    }
}

fn prompt_project_trust(
    cwd: &str,
    trust_store: &crate::core::project_trust::ProjectTrustStore,
) -> bool {
    use std::io::Write;

    println!(
        "Trust project folder?\n{cwd}\n\nThis allows pi to load .pi settings and resources, install missing project packages, and execute project extensions."
    );
    print!("Trust project folder? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let trusted = std::io::stdin()
        .read_line(&mut answer)
        .map(|_| matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
        .unwrap_or(false);
    trust_store.set(cwd, Some(trusted));
    trusted
}

/// Create settings for a mode after applying the upstream project-trust
/// precedence: explicit CLI override, saved directory decision, global
/// `defaultProjectTrust`, then an interactive startup prompt when UI exists.
/// Headless modes deliberately treat an unresolved `ask` decision as
/// untrusted, so project resources cannot execute merely because a mode
/// bypassed the normal interactive startup.
pub fn create_settings_with_project_trust(
    cwd: &str,
    agent_dir: &std::path::Path,
    trust_override: Option<bool>,
    has_ui: bool,
) -> SettingsManager {
    let trust_store =
        crate::core::project_trust::ProjectTrustStore::new(&agent_dir.display().to_string());
    let project_trusted = if let Some(override_value) = trust_override {
        override_value
    } else if !crate::core::project_trust::has_trust_requiring_project_resources(cwd) {
        true
    } else if let Some(saved) = trust_store.get(cwd) {
        saved
    } else {
        match settings_default_project_trust(agent_dir).as_deref() {
            Some("always") => true,
            Some("never") => false,
            _ if has_ui => prompt_project_trust(cwd, &trust_store),
            _ => false,
        }
    };
    SettingsManager::create(
        cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions { project_trusted },
    )
}

/// Mode entry points share one trust gate so interactive, JSON, and RPC do
/// not accidentally load project-local resources with their default settings.
pub fn create_mode_settings(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    has_ui: bool,
) -> SettingsManager {
    create_settings_with_project_trust(cwd, agent_dir, project_trust_override(args), has_ui)
}

pub async fn run(args: &Args) -> Result<RunOutcome, String> {
    let cwd = config::cwd();
    let agent_dir = config::get_agent_dir();
    let mut settings = create_mode_settings(args, &cwd, &agent_dir, false);
    // Surface settings load errors as diagnostics (never silently ignore a
    // malformed settings.json that the user expects to take effect).
    let settings_errors = settings.drain_errors();
    if let Some(first) = settings_errors.first() {
        tracing::warn!(
            scope = ?first.scope,
            path = ?first.path,
            error = %first.error,
            "settings load error; continuing with defaults"
        );
    }

    let provider = resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = resolve_run_model(
        args.model.as_deref(),
        &settings,
        !has_explicit_provider(args.provider.as_deref()),
    );

    // Build the model + stream function for the selected provider. Real
    // providers route through the pi-ai Models facade (catalog-backed model
    // resolution + auth application + api dispatch). `faux` keeps its
    // scripted path for tests.
    let mut selected_provider_uses_oauth = false;
    let (model, stream_fn, summary_stream_fn): (pi_ai::model::Model, StreamFn, StreamFn) =
        if provider == "faux" {
            let core = pi_ai::providers::FauxProviderCore::new(
                &pi_ai::providers::RegisterFauxProviderOptions::default(),
            );
            let model = match model_hint.as_deref() {
                Some(hint) => {
                    let id = hint.rsplit('/').next().unwrap_or(hint);
                    core.get_model(Some(id))
                        .cloned()
                        .ok_or_else(|| format!("unknown faux model {id:?}"))?
                }
                None => core
                    .models
                    .first()
                    .cloned()
                    .ok_or_else(|| "no faux model".to_string())?,
            };
            // Queue one scripted faux response per prompt so sequential
            // print-mode turns (one assistant turn per positional message,
            // upstream `runPrintMode`) each pop a reply.
            let prompts: Vec<String> = if args.messages.is_empty() {
                vec!["Hello from pi-rust".to_string()]
            } else {
                args.messages.clone()
            };
            let responses: Vec<pi_ai::providers::FauxResponseStep> = prompts
                .into_iter()
                .map(|text| {
                    pi_ai::providers::FauxResponseStep::Message(
                        pi_ai::providers::faux_assistant_message(
                            vec![pi_ai::types::ContentBlock::text(format!(
                                "faux response to: {text}"
                            ))],
                            pi_ai::providers::FauxAssistantOptions::default(),
                        ),
                    )
                })
                .collect();
            core.set_responses(responses);
            let core = core.clone();
            let stream_fn: StreamFn = Arc::new(move |model, ctx| core.stream(model, ctx, None));
            // Keep compaction completions off the scripted user-response queue.
            // The real provider uses the same stream path for both calls; faux is
            // deliberately split so a summary cannot consume a later print turn.
            let summary_core = pi_ai::providers::FauxProviderCore::new(
                &pi_ai::providers::RegisterFauxProviderOptions::default(),
            );
            let summary_responses = (0..64)
                .map(|_| {
                    pi_ai::providers::FauxResponseStep::Message(
                        pi_ai::providers::faux_assistant_message(
                            vec![pi_ai::types::ContentBlock::text("faux compaction summary")],
                            pi_ai::providers::FauxAssistantOptions::default(),
                        ),
                    )
                })
                .collect();
            summary_core.set_responses(summary_responses);
            let summary_core = summary_core.clone();
            let summary_stream_fn: StreamFn =
                Arc::new(move |model, ctx| summary_core.stream(model, ctx, None));
            (model, stream_fn, summary_stream_fn)
        } else {
            // models.json runtime merge: the registry overlays the bundled
            // catalog with ~/.pi/agent/models.json (upstream applyModelsJson).
            let models = {
                let models = crate::core::model_registry::builtin_models();
                let config = crate::core::model_config::ModelConfig::load(
                    crate::core::model_config::models_json_path().as_deref(),
                );
                let registry = crate::core::model_registry::ModelRegistry::new(models, config);
                registry.into_models()
            };
            if models.get_provider(&provider).is_none() {
                return Err(format!(
                    "provider {provider:?} is not registered in the model registry"
                ));
            }
            selected_provider_uses_oauth = models
                .get_provider(&provider)
                .is_some_and(|registered| registered.auth.oauth.is_some());
            let model = crate::core::model_runtime::resolve_run_model_for_provider(
                &models,
                &provider,
                model_hint.as_deref(),
            )?;
            // Stream options carry the explicit --api-key / PI_KEY (the facade
            // applies env-key auth when absent).
            let api_key = args
                .api_key
                .clone()
                .or_else(|| std::env::var(config::ENV_KEY).ok());
            let stream_options = pi_ai::types::StreamOptions {
                base: pi_ai::types::ProviderRequestOptions {
                    api_key,
                    ..Default::default()
                },
                ..Default::default()
            };
            let models = models.clone();
            let stream_fn: StreamFn =
                Arc::new(move |_model, ctx| models.stream(_model, ctx, Some(&stream_options)));
            let summary_stream_fn = stream_fn.clone();
            (model, stream_fn, summary_stream_fn)
        };

    let system_prompt = assemble_run_system_prompt(args, &cwd, &agent_dir, &settings);

    // Register built-in tools (bash/read/write/edit + ls/find/grep) unless
    // --no-tools.
    let mut tools: Vec<pi_agent::tools::AgentTool> = Vec::new();
    if !args.no_tools {
        tools.push(pi_agent::tools::bash_tool(cwd.clone()));
        tools.push(pi_agent::tools::read_tool_with_options(
            cwd.clone(),
            ProcessImageOptions {
                auto_resize_images: settings.get_image_auto_resize(),
                ..Default::default()
            },
        ));
        tools.push(pi_agent::tools::write_tool(cwd.clone()));
        tools.push(pi_agent::tools::edit_tool(cwd.clone()));
        tools.push(crate::core::tools::ls_tool(cwd.clone()));
        tools.push(crate::core::tools::find_tool(cwd.clone()));
        tools.push(crate::core::tools::grep_tool(cwd.clone()));
    }
    // Resolve the durable session before creating the harness. This is the
    // point where the CLI session selectors become observable: continue and
    // resume open the selected v4 file, fork creates a child whose parent is
    // the selected session, and a normal run creates a fresh file. Legacy
    // files are migrated before inventory so every selector sees one format.
    let (harness_session, durable_session_path) = prepare_run_session(args, &cwd).await?;
    let harness_tools = tools
        .iter()
        .map(HarnessTool::from_agent_tool)
        .collect::<Vec<_>>();
    let mut harness_options = AgentHarnessOptions::new(harness_session, model.clone());
    harness_options.stream_fn = Some(stream_fn);
    harness_options.system_prompt = Some(system_prompt);
    harness_options.block_images = settings.get_block_images();
    harness_options.tools = Some(harness_tools);
    let (mut harness, _suspended) = AgentHarness::create(harness_options)
        .await
        .map_err(|error| format!("create agent harness: {error}"))?;

    // A resumed or forked session must rebuild the provider context before the
    // first new prompt. AgentHarness owns the live Agent state, while the
    // session file remains the source of truth for compaction boundaries and
    // derived model/tool settings.
    let existing_entries = harness
        .transcript()
        .await
        .map_err(|error| format!("read existing session transcript: {error}"))?;
    if !existing_entries.is_empty() {
        let context =
            build_session_context(&existing_entries, &SessionContextBuildOptions::default());
        harness
            .set_agent_messages(context.messages)
            .await
            .map_err(|error| format!("restore session context: {error}"))?;
    }

    // Expand `/template` prompt-template invocations in positional messages
    // (upstream `expandPromptTemplate`).
    let prompt_templates = load_prompt_templates_for_run(args, &cwd, &agent_dir);
    // Print mode prompts each positional message as its own sequential turn
    // (upstream `runPrintMode`: `for (const message of messages) { await
    // session.prompt(message); }`). Each turn's messages fold into the agent
    // context so a later prompt observes earlier turns.
    let mut all_messages: Vec<pi_agent::types::AgentMessage> = Vec::new();
    // The compaction harness consumes full entries (not just provider
    // messages), all sourced from the harness-owned main lane.
    let summarizer = SimpleModels::new({
        let summary_stream_fn = summary_stream_fn.clone();
        move |model, context, _options| {
            let stream = (summary_stream_fn)(model, context);
            Box::pin(async move { stream.collect().await.1 })
        }
    });
    let prepared_files =
        prepare_file_arguments(&args.file_args, &cwd, settings.get_image_auto_resize())?;
    let mut prompts: Vec<(String, Vec<ContentBlock>)> = Vec::new();
    if let Some((file_text, images)) = prepared_files {
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_text = format!("{file_text}{first_message}");
        if !initial_text.is_empty() || !images.is_empty() {
            prompts.push((initial_text, images));
        }
        prompts.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .map(|text| (text.clone(), Vec::new())),
        );
    } else {
        prompts.extend(args.messages.iter().map(|text| (text.clone(), Vec::new())));
    }
    for (text, images) in prompts {
        let expanded =
            crate::core::prompt_templates::expand_prompt_template(&text, &prompt_templates);
        let mut blocks = vec![ContentBlock::text(expanded)];
        blocks.extend(images);
        let prompt = pi_agent::types::AgentMessage::Core(Message::User(UserContent::blocks(
            blocks,
            pi_ai::types::now_ms(),
        )));
        let turn_messages = harness
            .run_prompt(vec![prompt])
            .await
            .map_err(|error| format!("run harness prompt: {error}"))?;
        let mut history_entries = harness
            .transcript()
            .await
            .map_err(|error| format!("read harness transcript: {error}"))?;
        all_messages.extend(turn_messages);

        let mut agent_messages = harness
            .agent_messages()
            .await
            .map_err(|error| format!("read harness messages: {error}"))?;
        if let Some(compaction) = maybe_auto_compact(
            &mut agent_messages,
            &mut history_entries,
            &model,
            &settings,
            &summarizer,
        )
        .await
        {
            harness
                .set_agent_messages(agent_messages)
                .await
                .map_err(|error| format!("set compacted harness messages: {error}"))?;
            harness
                .append_entry(compaction)
                .await
                .map_err(|error| format!("append harness compaction: {error}"))?;
        }
    }

    // The last assistant message drives output (upstream print-mode.ts reads
    // `state.messages[state.messages.length - 1]`).
    let last_assistant = all_messages.iter().rev().find_map(|m| match m {
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => Some(a),
        _ => None,
    });

    // Terminal error/abort: print `errorMessage` or `Request {stopReason}` to
    // stderr and exit nonzero (upstream sets exitCode = 1).
    if let Some(a) = last_assistant {
        if matches!(
            a.stop_reason(),
            Some(pi_ai::types::StopReason::Error) | Some(pi_ai::types::StopReason::Aborted)
        ) {
            let msg = a
                .error_message()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Request {}", a.stop_reason().unwrap().as_str()));
            return Err(crate::core::auth_guidance::format_provider_auth_failure(
                a.provider().unwrap_or(&provider),
                selected_provider_uses_oauth,
                &msg,
            ));
        }
    }

    // Text-mode output: each text content block printed with a trailing newline
    // (upstream `writeRawStdout(`${content.text}\n`)`), so blocks are joined
    // with `\n` rather than concatenated.
    let final_text: String = last_assistant
        .map(|a| {
            a.content()
                .iter()
                .filter_map(|b| match b {
                    pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    Ok(RunOutcome {
        final_text,
        session_path: durable_session_path,
    })
}

/// Select or create the session used by the one-shot CLI path.
///
/// The TypeScript CLI resolves these selectors before `runPrintMode` starts:
/// `--continue` chooses the newest session for the current directory,
/// `--resume` resolves a session target (or the newest available target in
/// non-interactive mode), and `--fork` opens a child created from the selected
/// source. Keeping that decision here means the harness can append directly to
/// the selected durable file instead of replaying a fresh in-memory transcript
/// into a second session at shutdown.
async fn prepare_run_session(
    args: &Args,
    cwd: &str,
) -> Result<(Session<StdFileSystem>, Option<String>), String> {
    let selects_existing =
        args.continue_session || args.resume || args.session.is_some() || args.fork.is_some();

    if args.no_session {
        if selects_existing {
            return Err(
                "--continue, --resume, --session, and --fork require session persistence"
                    .to_string(),
            );
        }
        let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(in_memory_metadata(
            "print-run",
            None,
        ))));
        return Ok((Session::from_in_memory(storage), None));
    }

    let session_root = args
        .session_dir
        .clone()
        .map(|directory| config::expand_tilde_path(&directory))
        .unwrap_or_else(|| config::get_session_dir().to_string_lossy().into_owned());
    let session_root_path = PathBuf::from(&session_root);
    std::fs::create_dir_all(&session_root_path)
        .map_err(|error| format!("create session dir {session_root}: {error}"))?;

    // Migrate all files visible to the normal repository inventory before
    // resolving a selector. An explicit path outside the configured root is
    // migrated below as well.
    crate::core::session_migration::migrate_legacy_sessions_in_root(&session_root_path)
        .map_err(|error| format!("migrate legacy sessions: {error}"))?;

    let fs = StdFileSystem::new(cwd);
    let mut repo = JsonlSessionRepo::new(fs, &session_root);
    let source_selector = args.fork.as_deref().or(args.session.as_deref());
    let source = if let Some(selector) = source_selector {
        let path = PathBuf::from(config::expand_tilde_path(selector));
        if path.is_file() {
            crate::core::session_migration::migrate_legacy_session_file(&path)
                .map_err(|error| format!("migrate selected session: {error}"))?;
        }
        Some(resolve_session_metadata(&repo, selector).await?)
    } else if args.continue_session || args.resume {
        let mut sessions = repo
            .list(Some(cwd))
            .await
            .map_err(|error| format!("list sessions: {error}"))?;
        sessions.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
        Some(sessions.into_iter().next().ok_or_else(|| {
            if args.resume {
                "no sessions found to resume in this directory".to_string()
            } else {
                "no previous session found to continue in this directory".to_string()
            }
        })?)
    } else {
        None
    };

    let mut session = if let Some(source) = source {
        if args.fork.is_some() {
            repo.fork(
                &source,
                CreateOptions {
                    id: args
                        .session_id
                        .clone()
                        .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
                    cwd: cwd.to_string(),
                    parent_session_id: None,
                    metadata: None,
                    fork_options: ForkOptions::Tree,
                },
            )
            .await
            .map_err(|error| format!("fork session {}: {error}", source.id))?
        } else {
            repo.open(&source)
                .await
                .map_err(|error| format!("open session {}: {error}", source.id))?
        }
    } else {
        repo.create(CreateOptions {
            id: args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
            cwd: cwd.to_string(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Tree,
        })
        .await
        .map_err(|error| format!("create session: {error}"))?
    };

    if let Some(name) = &args.name {
        session
            .set_name(Some(name))
            .await
            .map_err(|error| format!("set session name: {error}"))?;
    }
    let path = session.get_metadata().await.path;
    Ok((session, Some(path)))
}

/// Resolve a CLI session selector by path, exact id, or an unambiguous id
/// prefix. Paths may refer to a file outside the configured session root;
/// those files are opened through a metadata projection after migration.
pub(crate) async fn resolve_session_metadata(
    repo: &JsonlSessionRepo<StdFileSystem>,
    selector: &str,
) -> Result<SessionMetadata, String> {
    let expanded = config::expand_tilde_path(selector);
    let requested_path = PathBuf::from(&expanded);
    let path_like = requested_path.is_file()
        || selector.ends_with(".jsonl")
        || selector.contains(std::path::MAIN_SEPARATOR)
        || selector.contains('/')
        || selector.contains('\\');

    let sessions = repo
        .list(None)
        .await
        .map_err(|error| format!("list sessions: {error}"))?;
    let requested_canonical = if requested_path.exists() {
        std::fs::canonicalize(&requested_path).ok()
    } else {
        None
    };

    let mut matches: Vec<SessionMetadata> = sessions
        .into_iter()
        .filter(|metadata| {
            if path_like {
                if metadata.path == expanded {
                    return true;
                }
                return requested_canonical.as_ref().is_some_and(|requested| {
                    std::fs::canonicalize(&metadata.path)
                        .ok()
                        .is_some_and(|candidate| candidate == *requested)
                });
            }
            metadata.id == selector || metadata.id.starts_with(selector)
        })
        .collect();

    if matches.is_empty() && path_like && requested_path.is_file() {
        return metadata_from_session_path(&requested_path);
    }
    if matches.is_empty() {
        return Err(format!("session not found: {selector}"));
    }
    if matches.len() > 1 {
        matches.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
        let ids = matches
            .iter()
            .map(|metadata| metadata.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("session selector {selector:?} is ambiguous: {ids}"));
    }

    Ok(matches.remove(0))
}

/// Read the v4 header for an explicit session file that is not in the
/// configured repository root. The repository only needs this metadata to
/// validate/open the file; entries remain decoded by `JsonlSessionRepo::open`.
pub(crate) fn metadata_from_session_path(path: &Path) -> Result<SessionMetadata, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("read session {}: {error}", path.display()))?;
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| format!("session {} is empty", path.display()))?;
    let header: serde_json::Value = serde_json::from_str(first_line)
        .map_err(|error| format!("parse session header {}: {error}", path.display()))?;
    if header.get("kind").and_then(serde_json::Value::as_str) != Some("header") {
        return Err(format!("session {} is not a v4 JSONL file", path.display()));
    }
    let id = header
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("session {} header is missing id", path.display()))?;
    let modified_at = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Ok(SessionMetadata {
        id: id.to_string(),
        created_at: header
            .get("createdAt")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cwd: header
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        path: path.to_string_lossy().into_owned(),
        modified_at,
        source_format: header
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(4),
        parent_session_id: header
            .get("parentSessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        legacy_parent_session_path: header
            .get("legacyParentSessionPath")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        metadata: header.get("metadata").cloned(),
    })
}

/// Apply one threshold compaction to the print-mode context and return the
/// provisioned JSONL entry to append after the turn's messages.
async fn maybe_auto_compact(
    messages: &mut Vec<AgentMessage>,
    history_entries: &mut Vec<Entry>,
    model: &pi_ai::model::Model,
    settings: &SettingsManager,
    summarizer: &SimpleModels,
) -> Option<EntryNoStats> {
    let (enabled, reserve_tokens, keep_recent_tokens) = settings.get_compaction_settings();
    let compaction_settings = CompactionSettings {
        enabled,
        reserve_tokens,
        keep_recent_tokens,
    };
    let estimate = estimate_context_tokens(messages);
    if !should_compact(estimate.tokens, model.context_window, &compaction_settings) {
        return None;
    }

    let preparation = match prepare_compaction(history_entries, &compaction_settings) {
        Ok(preparation) => preparation,
        Err(error) => {
            tracing::warn!(%error, "automatic print-mode compaction preparation failed");
            return None;
        }
    };
    let Some(preparation) = preparation else {
        return None;
    };
    let result = match compact(
        &preparation,
        summarizer,
        model,
        None,
        None,
        Some("off"),
        None,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "automatic print-mode compaction failed");
            return None;
        }
    };

    let id = pi_agent::session::new_id();
    let seq = history_entries.last().map_or(1, |entry| entry.seq() + 1);
    let parent_id = history_entries.last().map(|entry| entry.id().to_string());
    let timestamp = pi_ai::types::now_ms();
    let details = result.details.as_ref().map(|details| {
        serde_json::json!({
            "readFiles": details.read_files,
            "modifiedFiles": details.modified_files,
        })
    });
    let retained_tail = result.retained_tail.clone();
    let summary = result.summary.clone();
    let tokens_before = result.tokens_before;
    let usage = result.usage.clone();
    history_entries.push(Entry::Compaction {
        id: id.clone(),
        seq,
        parent_id,
        timestamp,
        summary: summary.clone(),
        retained_tail: retained_tail.clone(),
        tokens_before,
        details: details.clone(),
        usage: usage.clone(),
    });
    *messages =
        build_session_context(history_entries, &SessionContextBuildOptions::default()).messages;

    Some(EntryNoStats::Compaction {
        id,
        summary,
        retained_tail,
        tokens_before,
        details,
        usage,
    })
}

/// Process `@file` arguments using the coding-agent image pipeline. Text
/// files become tagged prompt text; image files become model-facing image
/// blocks plus the same `<file>` reference used by upstream.
pub(crate) fn prepare_file_arguments(
    file_args: &[String],
    cwd: &str,
    auto_resize_images: bool,
) -> Result<Option<(String, Vec<ContentBlock>)>, String> {
    if file_args.is_empty() {
        return Ok(None);
    }

    let mut text = String::new();
    let mut images = Vec::new();
    for file_arg in file_args {
        let absolute = pi_agent::tools::path_utils::resolve_read_tool_path_existing(cwd, file_arg);
        let metadata = std::fs::metadata(&absolute)
            .map_err(|_| format!("Error: File not found: {absolute}"))?;
        if metadata.len() == 0 {
            continue;
        }
        let bytes = std::fs::read(&absolute)
            .map_err(|error| format!("Error: Could not read file {absolute}: {error}"))?;
        if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
            match process_image(
                &bytes,
                mime_type,
                ProcessImageOptions {
                    auto_resize_images,
                    ..Default::default()
                },
            ) {
                Ok(processed) => {
                    images.push(ContentBlock::image(processed.data, processed.mime_type));
                    if processed.hints.is_empty() {
                        text.push_str(&format!("<file name=\"{absolute}\"></file>\n"));
                    } else {
                        text.push_str(&format!(
                            "<file name=\"{absolute}\">{}</file>\n",
                            processed.hints.join("\n")
                        ));
                    }
                }
                Err(message) => {
                    text.push_str(&format!("<file name=\"{absolute}\">{message}</file>\n"));
                }
            }
        } else {
            let content = String::from_utf8_lossy(&bytes);
            let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
            text.push_str(&format!("<file name=\"{absolute}\">\n{content}\n</file>\n"));
        }
    }
    Ok(Some((text, images)))
}

/// Assemble the run-path system prompt from loaded resources: the base
/// `--system-prompt`, the `<available_skills>` block, the `<project_context>`
/// section (disabled by `-nc`), and `--append-system-prompt` inputs (existing
/// files read verbatim, inline text used as-is).
fn assemble_run_system_prompt(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
) -> String {
    let base = args.system_prompt.clone().unwrap_or_default();
    let skills_block = build_skills_block(args, cwd, agent_dir, settings);
    let mut prompt = format!("{base}\n{skills_block}");
    let trimmed = prompt.trim().to_string();
    prompt = trimmed;
    if !args.no_context_files {
        let context_files = crate::core::context_files::load_project_context_files(
            cwd,
            &agent_dir.display().to_string(),
        );
        prompt = format!(
            "{prompt}\n{}",
            crate::core::context_files::format_project_context(&context_files)
        );
    }
    for append in &args.append_system_prompt {
        let resolved = resolve_prompt_input(append, "append system prompt");
        prompt = format!("{prompt}\n{resolved}");
    }
    prompt
}

/// If `input` is an existing file path, read its contents (stripping a BOM);
/// otherwise return the input verbatim (upstream `resolvePromptInput`).
fn resolve_prompt_input(input: &str, description: &str) -> String {
    let expanded = config::expand_tilde_path(input);
    let path = std::path::Path::new(&expanded);
    if path.is_file() {
        match std::fs::read_to_string(path) {
            Ok(content) => return content.trim_start_matches('\u{feff}').to_string(),
            Err(e) => {
                tracing::warn!("could not read {description} file {input}: {e}");
                return input.to_string();
            }
        }
    }
    input.to_string()
}

/// Load skills (user + project + `--skill`) and render the `<available_skills>`
/// system-prompt block, marking `-ns` disabled. Surfaces load diagnostics as
/// warnings.
fn build_skills_block(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
) -> String {
    if args.no_skills {
        return String::new();
    }
    // Settings `skills` key provides additional custom skill dirs (upstream
    // `settings.skills` → skill paths).
    let mut skill_paths: Vec<String> = settings.get_skill_paths();
    skill_paths.extend(args.skills.iter().cloned());
    let result = crate::core::skills::load_skills(crate::core::skills::LoadSkillsOptions {
        cwd: cwd.to_string(),
        agent_dir: agent_dir.display().to_string(),
        skill_paths,
    });
    for diagnostic in &result.1 {
        tracing::warn!(
            path = ?diagnostic.path,
            message = %diagnostic.message,
            "skill load diagnostic"
        );
    }
    crate::core::skills::format_skills_for_prompt(&result.0)
}

/// Load prompt templates (user + project + `--prompt-template`) for run-path
/// expansion, marking `-np` / `-npt` disabled.
fn load_prompt_templates_for_run(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
) -> Vec<crate::core::prompt_templates::PromptTemplate> {
    if args.no_prompt_templates {
        return Vec::new();
    }
    let (templates, diagnostics) = crate::core::prompt_templates::load_prompt_templates(
        cwd,
        &agent_dir.display().to_string(),
        &args.prompt_templates,
        true,
        args.no_prompt_templates,
    );
    for diagnostic in &diagnostics {
        tracing::warn!(
            path = ?diagnostic.path,
            message = %diagnostic.message,
            "prompt template load diagnostic"
        );
    }
    templates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::SettingsMap;
    use serde_json::json;

    fn manager() -> SettingsManager {
        SettingsManager::in_memory(
            serde_json::from_value(json!({
                "defaultProvider": "faux",
                "defaultModel": "faux-1"
            }))
            .unwrap(),
        )
    }

    fn clear_env_provider_model() {
        unsafe {
            std::env::remove_var(config::ENV_PROVIDER);
            std::env::remove_var(config::ENV_MODEL);
        }
    }

    #[test]
    fn resolve_provider_cli_beats_settings() {
        clear_env_provider_model();
        let settings = SettingsManager::in_memory(
            serde_json::from_value(json!({
                "defaultProvider": "faux"
            }))
            .unwrap(),
        );
        let provider = resolve_run_provider(Some("anthropic"), &settings);
        assert_eq!(provider, "anthropic");
        let provider = resolve_run_provider(None, &settings);
        assert_eq!(provider, "faux");
        let provider = resolve_run_provider(None, &SettingsManager::in_memory(SettingsMap::new()));
        assert_eq!(provider, "google");
    }

    #[test]
    fn resolve_model_settings_applies_when_no_explicit_provider() {
        clear_env_provider_model();
        let settings = manager();
        let model = resolve_run_model(None, &settings, true);
        assert_eq!(model.as_deref(), Some("faux-1"));
        let model = resolve_run_model(Some("faux-2"), &settings, true);
        assert_eq!(model.as_deref(), Some("faux-2"));
    }

    #[test]
    fn resolve_model_settings_gated_off_with_explicit_provider() {
        clear_env_provider_model();
        let settings = manager();
        // Upstream pairs defaultProvider+defaultModel; an explicit provider
        // source means the settings default model must not leak in.
        let model = resolve_run_model(None, &settings, false);
        assert_eq!(model, None);
        let model = resolve_run_model(Some("faux-2"), &settings, false);
        assert_eq!(model.as_deref(), Some("faux-2"));
    }

    #[test]
    fn build_skills_block_lists_loaded_skills() {
        let root = std::env::temp_dir().join(format!("pi-run-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let agent = root.join("agent");
        std::fs::create_dir_all(agent.join("skills/my-skill")).unwrap();
        std::fs::write(
            agent.join("skills/my-skill/SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill\n---\nbody",
        )
        .unwrap();
        let cwd = root.to_string_lossy().into_owned();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args::default();
        let block = build_skills_block(&args, &cwd, &agent, &settings);
        assert!(block.contains("<available_skills>"));
        assert!(block.contains("<name>my-skill</name>"));
        assert!(!block.contains("disabled"), "no disabled skill");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_skills_block_empty_on_no_skills() {
        let root = std::env::temp_dir().join(format!("pi-run-noskills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let mut args = Args::default();
        args.no_skills = true;
        let block = build_skills_block(&args, &root.to_string_lossy(), &root, &settings);
        assert_eq!(block, "");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_prompt_input_reads_file_or_passes_through() {
        let root = std::env::temp_dir().join(format!("pi-run-promptin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("append.md");
        std::fs::write(&file, "appended content").unwrap();
        assert_eq!(
            resolve_prompt_input(&file.to_string_lossy(), "append system prompt"),
            "appended content"
        );
        assert_eq!(
            resolve_prompt_input("inline text", "append system prompt"),
            "inline text"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn assemble_system_prompt_injects_context_and_skips_on_nc() {
        let root = std::env::temp_dir().join(format!("pi-run-assemble-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let agent = root.join("agent");
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "project ctx line").unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());

        let mut args = Args::default();
        args.append_system_prompt = vec!["tail".to_string()];
        let prompt = assemble_run_system_prompt(&args, &cwd.to_string_lossy(), &agent, &settings);
        assert!(prompt.contains("<project_instructions"));
        assert!(prompt.contains("project ctx line"));
        assert!(prompt.ends_with("tail"), "append prompt is last");

        let mut args_nc = Args::default();
        args_nc.no_context_files = true;
        let prompt_nc =
            assemble_run_system_prompt(&args_nc, &cwd.to_string_lossy(), &agent, &settings);
        assert!(
            !prompt_nc.contains("<project_instructions"),
            "-nc must skip context files"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_arguments_attach_images_and_tag_text_references() {
        let root = std::env::temp_dir().join(format!("pi-run-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("prompt.md"), "inspect this").unwrap();

        let mut bmp = vec![0u8; 58];
        bmp[0..2].copy_from_slice(b"BM");
        let bmp_len = bmp.len() as u32;
        bmp[2..6].copy_from_slice(&bmp_len.to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        bmp[34..38].copy_from_slice(&4u32.to_le_bytes());
        bmp[56] = 0xff;
        std::fs::write(root.join("pixel.bmp"), bmp).unwrap();

        let cwd = root.to_string_lossy().to_string();
        let files = vec!["prompt.md".to_string(), "pixel.bmp".to_string()];
        let (text, images) = prepare_file_arguments(&files, &cwd, false)
            .unwrap()
            .expect("file arguments should produce an initial prompt");
        assert!(text.contains("<file name=\"") && text.contains("inspect this"));
        assert!(text.contains("pixel.bmp"));
        assert!(matches!(
            images.as_slice(),
            [ContentBlock::Image { mime_type, .. }] if mime_type == "image/png"
        ));
        std::fs::remove_dir_all(&root).ok();
    }
}

/// Build the faux model for the scripted test provider (shared by the run
/// and RPC paths).
pub fn build_faux_model(model_hint: Option<&str>) -> Result<pi_ai::model::Model, String> {
    let core = pi_ai::providers::FauxProviderCore::new(
        &pi_ai::providers::RegisterFauxProviderOptions::default(),
    );
    match model_hint {
        Some(hint) => {
            let id = hint.rsplit('/').next().unwrap_or(hint);
            core.get_model(Some(id))
                .cloned()
                .ok_or_else(|| format!("unknown faux model {id:?}"))
        }
        None => core
            .models
            .first()
            .cloned()
            .ok_or_else(|| "no faux model".to_string()),
    }
}
