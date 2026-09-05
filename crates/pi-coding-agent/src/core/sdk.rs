//! Public coding-agent SDK built on the real Rust agent and session runtime.
//!
//! This is the Rust counterpart of `core/sdk.ts`.  It deliberately keeps the
//! storage, model, extension, and harness objects visible instead of wrapping
//! them in an inert compatibility object.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_agent::fs::StdFileSystem;
use pi_agent::harness::agent_harness::{
    AbortResultValue, AgentHarness, AgentHarnessOptions, AgentLane, HarnessError, HarnessTool,
    QueueMode, Resources, RunResultValue,
};
use pi_agent::session::jsonl::repo::{CreateOptions, JsonlSessionRepo};
use pi_agent::session::state::{ForkOptions, ForkPosition};
use pi_agent::session::{EntryNoStats, Session};
use pi_agent::types::AgentMessage;
use pi_ai::model::Model;
use pi_ai::types::{Context, ModelThinkingLevel, SimpleStreamOptions};

use crate::config;
use crate::core::extensions::{
    register_loaded_native_providers, LoadedExtensions, ResourceDiscovery,
};
use crate::core::model_registry::builtin_models;
use crate::core::model_resolver::{find_exact_model_reference_match, DEFAULT_THINKING_LEVEL};
use crate::core::model_runtime::ModelRuntime;
use crate::core::settings::{SettingsManager, SettingsManagerCreateOptions};

/// A concrete session manager backed by the existing JSONL repository.
///
/// The manager owns repository access while each opened session owns its
/// storage handle.  This makes session replacement safe: a new session is
/// fully opened before the old harness is torn down.
#[derive(Clone)]
pub struct SessionManager {
    cwd: String,
    session_dir: String,
    repo: Arc<tokio::sync::Mutex<JsonlSessionRepo<StdFileSystem>>>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("cwd", &self.cwd)
            .field("session_dir", &self.session_dir)
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    pub fn new(cwd: impl Into<String>, session_dir: impl Into<String>) -> Self {
        let cwd = absolute_path(&cwd.into());
        let session_dir = absolute_path(&session_dir.into());
        Self {
            repo: Arc::new(tokio::sync::Mutex::new(JsonlSessionRepo::new(
                StdFileSystem::new(&cwd),
                session_dir.clone(),
            ))),
            cwd,
            session_dir,
        }
    }

    pub fn default_for(cwd: impl Into<String>, agent_dir: impl Into<String>) -> Self {
        let cwd = absolute_path(&cwd.into());
        let agent_dir = absolute_path(&agent_dir.into());
        Self::new(
            cwd.clone(),
            Path::new(&agent_dir)
                .join("sessions")
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn session_dir(&self) -> &str {
        &self.session_dir
    }

    pub async fn create_session(
        &self,
        id: Option<String>,
        parent_session_id: Option<String>,
    ) -> Result<Session<StdFileSystem>, String> {
        std::fs::create_dir_all(&self.session_dir)
            .map_err(|e| format!("create session directory: {e}"))?;
        self.repo
            .lock()
            .await
            .create(CreateOptions {
                id,
                cwd: self.cwd.clone(),
                parent_session_id,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn open_session(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Session<StdFileSystem>, String> {
        let path = absolute_path(&path.as_ref().to_string_lossy());
        let metadata = read_session_metadata(&path)?;
        self.repo
            .lock()
            .await
            .open(&metadata)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn fork_session(
        &self,
        source: &Session<StdFileSystem>,
        entry_id: Option<&str>,
        position: ForkPosition,
    ) -> Result<Session<StdFileSystem>, String> {
        let metadata = source.get_metadata().await;
        let options = CreateOptions {
            id: None,
            cwd: self.cwd.clone(),
            parent_session_id: Some(metadata.id.clone()),
            metadata: None,
            fork_options: match entry_id {
                Some(entry_id) => ForkOptions::Branch {
                    entry_id: Some(entry_id.to_string()),
                    position: Some(position),
                },
                None => ForkOptions::Tree,
            },
        };
        self.repo
            .lock()
            .await
            .fork(&metadata, options)
            .await
            .map_err(|e| e.to_string())
    }

    pub fn prepare_import(
        &self,
        input_path: impl AsRef<Path>,
    ) -> Result<(PathBuf, PathBuf), String> {
        let source = absolute_path(&input_path.as_ref().to_string_lossy());
        if !Path::new(&source).is_file() {
            return Err(format!("File not found: {source}"));
        }
        std::fs::create_dir_all(&self.session_dir)
            .map_err(|e| format!("create session directory: {e}"))?;
        let destination = Path::new(&self.session_dir).join(
            Path::new(&source)
                .file_name()
                .ok_or_else(|| format!("invalid session path: {source}"))?,
        );
        Ok((PathBuf::from(source), destination))
    }

    pub async fn import_prepared_session(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        cwd_override: Option<&str>,
    ) -> Result<Session<StdFileSystem>, String> {
        let source = source.as_ref();
        let destination = destination.as_ref();
        if destination != source {
            std::fs::copy(source, destination)
                .map_err(|e| format!("copy session for import: {e}"))?;
        }
        let mut metadata = read_session_metadata(&destination.to_string_lossy())?;
        if let Some(cwd) = cwd_override {
            metadata.cwd = absolute_path(cwd);
        }
        self.repo
            .lock()
            .await
            .open(&metadata)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn session_from_import(
        &self,
        input_path: impl AsRef<Path>,
        cwd_override: Option<&str>,
    ) -> Result<Session<StdFileSystem>, String> {
        let (source, destination) = self.prepare_import(input_path)?;
        self.import_prepared_session(source, destination, cwd_override)
            .await
    }
}

fn absolute_path(path: &str) -> String {
    let expanded = config::expand_tilde_path(path);
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn read_session_metadata(path: &str) -> Result<pi_agent::session::SessionMetadata, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read session: {e}"))?;
    let header_line = content
        .lines()
        .next()
        .ok_or_else(|| "session file is empty".to_string())?;
    let header = pi_agent::session::jsonl::parse_header(header_line)
        .map_err(|e| format!("invalid session header: {e}"))?;
    let modified_at = std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(header.created_at);
    Ok(pi_agent::session::jsonl::metadata_from_header(
        &header,
        path,
        modified_at,
    ))
}

/// Resource and extension state bound to one cwd.
#[derive(Clone)]
pub struct ResourceLoader {
    pub extensions: LoadedExtensions,
    pub discovery: ResourceDiscovery,
    pub resources: Resources,
}

impl std::fmt::Debug for ResourceLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceLoader")
            .field("extensions", &self.extensions.runner.get_extension_paths())
            .field("discovery", &self.discovery)
            .field("resources", &self.resources)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

pub struct CreateAgentSessionServicesOptions {
    pub cwd: String,
    pub agent_dir: Option<String>,
    pub settings_manager: Option<SettingsManager>,
    pub model_runtime: Option<ModelRuntime>,
    pub extension_args: crate::args::Args,
    pub extension_reason: String,
    pub previous_session_file: Option<String>,
}

impl Default for CreateAgentSessionServicesOptions {
    fn default() -> Self {
        Self {
            cwd: config::cwd(),
            agent_dir: None,
            settings_manager: None,
            model_runtime: None,
            extension_args: crate::args::Args::default(),
            extension_reason: "startup".to_string(),
            previous_session_file: None,
        }
    }
}

pub struct AgentSessionServices {
    pub cwd: String,
    pub agent_dir: String,
    pub model_runtime: ModelRuntime,
    pub settings_manager: SettingsManager,
    pub resource_loader: ResourceLoader,
    pub diagnostics: Vec<AgentSessionDiagnostic>,
}

impl std::fmt::Debug for AgentSessionServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSessionServices")
            .field("cwd", &self.cwd)
            .field("agent_dir", &self.agent_dir)
            .field("resource_loader", &self.resource_loader)
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}

pub async fn create_agent_session_services(
    options: CreateAgentSessionServicesOptions,
) -> Result<AgentSessionServices, String> {
    let cwd = absolute_path(&options.cwd);
    let configured_agent_dir = config::get_agent_dir();
    let agent_dir = absolute_path(
        options
            .agent_dir
            .as_deref()
            .unwrap_or(configured_agent_dir.to_str().unwrap_or(".")),
    );
    let settings_manager = options.settings_manager.unwrap_or_else(|| {
        SettingsManager::create(&cwd, &agent_dir, SettingsManagerCreateOptions::default())
    });
    let model_runtime = options
        .model_runtime
        .unwrap_or_else(|| ModelRuntime::new(builtin_models()));
    let thinking = settings_manager
        .get_default_thinking_level()
        .unwrap_or(DEFAULT_THINKING_LEVEL)
        .to_string();
    let extension_args = options.extension_args;
    let loaded = crate::core::extensions::load_for_mode_with_reason_and_flags_and_previous(
        &extension_args,
        &settings_manager,
        &cwd,
        &agent_dir,
        "sdk",
        false,
        None,
        thinking,
        &options.extension_reason,
        crate::core::extensions::integration::parsed_extension_flag_values(&extension_args),
        options.previous_session_file.as_deref(),
    );
    let mut diagnostics = loaded
        .errors
        .iter()
        .map(|error| AgentSessionDiagnostic {
            level: DiagnosticLevel::Error,
            message: format!("{}: {}", error.path, error.error),
        })
        .collect::<Vec<_>>();
    if let Err(error) = register_loaded_native_providers(&model_runtime.models(), &loaded) {
        diagnostics.push(AgentSessionDiagnostic {
            level: DiagnosticLevel::Error,
            message: format!("register extension provider: {error}"),
        });
    }
    let (pending_json, _) =
        crate::core::extensions::loader::take_pending_provider_registrations(&loaded.runtime);
    for registration in pending_json {
        diagnostics.push(AgentSessionDiagnostic {
            level: DiagnosticLevel::Error,
            message: format!(
                "extension provider {} is not supported by the Rust ModelRuntime registration ABI",
                registration.name
            ),
        });
    }

    let discovery = loaded.resources.clone();
    let mut skill_paths = settings_manager.get_skill_paths();
    skill_paths.extend(extension_args.skills.iter().cloned());
    skill_paths.extend(discovery.resolved_skill_paths(&cwd));
    let (skills, skill_diagnostics) = pi_agent::harness::load_skills(&cwd, &skill_paths);
    for diagnostic in skill_diagnostics {
        diagnostics.push(AgentSessionDiagnostic {
            level: DiagnosticLevel::Warning,
            message: format!("{}: {}", diagnostic.path, diagnostic.message),
        });
    }
    let mut prompt_paths = settings_manager.get_prompt_template_paths();
    prompt_paths.extend(extension_args.prompt_templates.iter().cloned());
    prompt_paths.extend(discovery.resolved_prompt_paths(&cwd));
    let (prompt_templates, prompt_diagnostics) =
        pi_agent::harness::load_prompt_templates(&cwd, &prompt_paths);
    for diagnostic in prompt_diagnostics {
        diagnostics.push(AgentSessionDiagnostic {
            level: DiagnosticLevel::Warning,
            message: format!("{}: {}", diagnostic.path, diagnostic.message),
        });
    }
    Ok(AgentSessionServices {
        cwd,
        agent_dir,
        model_runtime,
        settings_manager,
        resource_loader: ResourceLoader {
            extensions: loaded,
            discovery,
            resources: Resources {
                prompt_templates,
                skills,
            },
        },
        diagnostics,
    })
}

pub struct CreateAgentSessionOptions {
    pub cwd: Option<String>,
    pub agent_dir: Option<String>,
    pub model_runtime: Option<ModelRuntime>,
    pub settings_manager: Option<SettingsManager>,
    pub resource_loader: Option<ResourceLoader>,
    pub session_manager: Option<SessionManager>,
    pub session: Option<Session<StdFileSystem>>,
    pub model: Option<Model>,
    pub thinking_level: Option<ModelThinkingLevel>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: bool,
    pub custom_tools: Vec<HarnessTool>,
    pub system_prompt: Option<String>,
    pub extension_args: crate::args::Args,
    pub extension_reason: String,
    pub previous_session_file: Option<String>,
}

impl Default for CreateAgentSessionOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            agent_dir: None,
            model_runtime: None,
            settings_manager: None,
            resource_loader: None,
            session_manager: None,
            session: None,
            model: None,
            thinking_level: None,
            tools: None,
            exclude_tools: Vec::new(),
            no_tools: false,
            custom_tools: Vec::new(),
            system_prompt: None,
            extension_args: crate::args::Args::default(),
            extension_reason: "startup".to_string(),
            previous_session_file: None,
        }
    }
}

pub struct CreateAgentSessionResult {
    pub session: AgentSession,
    pub extensions_result: LoadedExtensions,
    pub model_fallback_message: Option<String>,
}

pub struct AgentSession {
    pub harness: AgentHarness<StdFileSystem>,
    pub session_manager: SessionManager,
    pub services: AgentSessionServices,
    pub session_file: Option<String>,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("cwd", &self.services.cwd)
            .field("session_file", &self.session_file)
            .field("harness", &self.harness)
            .finish()
    }
}

impl AgentSession {
    pub fn cwd(&self) -> &str {
        &self.services.cwd
    }

    pub fn model_runtime(&self) -> &ModelRuntime {
        &self.services.model_runtime
    }

    pub fn extension_runner(&self) -> Arc<crate::core::extensions::ExtensionRunner> {
        self.services.resource_loader.extensions.runner.clone()
    }

    pub fn session(&self) -> Arc<tokio::sync::Mutex<Session<StdFileSystem>>> {
        self.harness.session()
    }

    pub async fn prompt_text(&self, text: &str) -> Result<RunResultValue, HarnessError> {
        self.harness.prompt_text(text, &[]).await
    }

    pub async fn prompt(&self, message: AgentMessage) -> Result<RunResultValue, HarnessError> {
        self.harness.prompt_messages(&[message]).await
    }

    pub async fn abort(&self) -> Result<AbortResultValue, HarnessError> {
        self.harness.abort().await
    }

    pub async fn wait_for_idle(&self) -> Result<(), HarnessError> {
        self.harness.wait_for_idle().await
    }

    pub(crate) async fn shutdown(
        &mut self,
        reason: &str,
        target: Option<&str>,
        before_invalidate: Option<&(dyn Fn() + Send + Sync)>,
    ) {
        let _ = self.harness.abort().await;
        let _ = self
            .extension_runner()
            .emit_session_shutdown_with_target(reason, target);
        if let Some(callback) = before_invalidate {
            callback();
        }
        self.harness.close().await;
        if let Ok(mut runtime) = self.services.resource_loader.extensions.runtime.lock() {
            runtime.invalidate(Some("session disposed"));
        }
    }

    pub async fn dispose(&mut self) {
        self.shutdown("quit", None, None).await;
    }
}

fn resolve_model(
    runtime: &ModelRuntime,
    settings: &SettingsManager,
    requested: Option<&Model>,
) -> Result<Model, String> {
    if let Some(model) = requested {
        return Ok(model.clone());
    }
    let models = runtime.models().get_models(None);
    let provider = settings.get_default_provider();
    if let Some(reference) = settings.get_default_model() {
        if let Some(model) = find_exact_model_reference_match(reference, &models) {
            return Ok(model);
        }
    }
    if let Some(provider) = provider {
        if let Some(model) = models.iter().find(|model| model.provider == provider) {
            return Ok(model.clone());
        }
    }
    models
        .into_iter()
        .next()
        .ok_or_else(|| "No models available in ModelRuntime".to_string())
}

fn parse_thinking_level(level: &str) -> Option<ModelThinkingLevel> {
    Some(match level {
        "off" => ModelThinkingLevel::Off,
        "minimal" => ModelThinkingLevel::Minimal,
        "low" => ModelThinkingLevel::Low,
        "medium" => ModelThinkingLevel::Medium,
        "high" => ModelThinkingLevel::High,
        "xhigh" => ModelThinkingLevel::Xhigh,
        "max" => ModelThinkingLevel::Max,
        _ => return None,
    })
}

fn make_builtin_tools(
    cwd: &str,
    settings: &SettingsManager,
    loaded: &LoadedExtensions,
) -> Vec<HarnessTool> {
    let image_options = pi_agent::tools::image::ProcessImageOptions {
        auto_resize_images: settings.get_image_auto_resize(),
        ..Default::default()
    };
    let mut tools = [
        pi_agent::tools::read_tool_with_options(cwd.to_string(), image_options),
        pi_agent::tools::bash_tool_with_options(
            cwd.to_string(),
            settings.get_shell_command_prefix().map(str::to_string),
            settings.get_shell_path(),
        ),
        pi_agent::tools::edit_tool(cwd.to_string()),
        pi_agent::tools::write_tool(cwd.to_string()),
        crate::core::tools::ls_tool(cwd.to_string()),
        crate::core::tools::find_tool(cwd.to_string()),
        crate::core::tools::grep_tool(cwd.to_string()),
    ]
    .to_vec();
    crate::core::extensions::install_tools(loaded, &mut tools, true);
    tools.iter().map(HarnessTool::from_agent_tool).collect()
}

pub async fn create_agent_session(
    options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, String> {
    let cwd = absolute_path(options.cwd.as_deref().unwrap_or("."));
    let configured_agent_dir = config::get_agent_dir();
    let agent_dir = absolute_path(
        options
            .agent_dir
            .as_deref()
            .unwrap_or(configured_agent_dir.to_str().unwrap_or(".")),
    );
    let services = if let Some(resource_loader) = options.resource_loader {
        let settings_manager = options.settings_manager.unwrap_or_else(|| {
            SettingsManager::create(&cwd, &agent_dir, SettingsManagerCreateOptions::default())
        });
        let model_runtime = options
            .model_runtime
            .unwrap_or_else(|| ModelRuntime::new(builtin_models()));
        AgentSessionServices {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            model_runtime,
            settings_manager,
            resource_loader,
            diagnostics: Vec::new(),
        }
    } else {
        create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: options.settings_manager,
            model_runtime: options.model_runtime,
            extension_args: options.extension_args,
            extension_reason: options.extension_reason.clone(),
            previous_session_file: options.previous_session_file.clone(),
        })
        .await?
    };
    let session_manager = options
        .session_manager
        .unwrap_or_else(|| SessionManager::default_for(&cwd, &agent_dir));
    let session = match options.session {
        Some(session) => session,
        None => session_manager.create_session(None, None).await?,
    };
    let session_file = session.get_metadata().await.path;
    let session_file = (!session_file.is_empty()).then_some(session_file);
    let model = resolve_model(
        &services.model_runtime,
        &services.settings_manager,
        options.model.as_ref(),
    )?;
    let thinking_level = options
        .thinking_level
        .or_else(|| {
            services
                .settings_manager
                .get_default_thinking_level()
                .and_then(parse_thinking_level)
        })
        .unwrap_or(ModelThinkingLevel::Medium);
    let stream_runtime = services.model_runtime.clone();
    let stream_options = SimpleStreamOptions::default();
    let stream_fn: pi_agent::agent::StreamFn = Arc::new(move |model: &Model, context: &Context| {
        stream_runtime.stream_simple(model, context, Some(&stream_options))
    });
    let mut tools = if options.no_tools {
        Vec::new()
    } else {
        make_builtin_tools(
            &cwd,
            &services.settings_manager,
            &services.resource_loader.extensions,
        )
    };
    tools.extend(options.custom_tools);
    if let Some(allowed) = options.tools {
        tools.retain(|tool| allowed.iter().any(|name| name == tool.name()));
    }
    tools.retain(|tool| !options.exclude_tools.iter().any(|name| name == tool.name()));
    let mut session = session;
    if session
        .find_entries(&pi_agent::session::EntryQuery {
            order: Some(pi_agent::session::EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        session
            .append_entry(
                EntryNoStats::ModelChange {
                    id: pi_agent::session::new_id(),
                    provider: model.provider.clone(),
                    model_id: model.id.clone(),
                },
                "main",
            )
            .await
            .map_err(|error| error.to_string())?;
        session
            .append_entry(
                EntryNoStats::ThinkingLevel {
                    id: pi_agent::session::new_id(),
                    thinking_level: thinking_level.as_str().to_string(),
                },
                "main",
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    let mut harness_options = AgentHarnessOptions::new(session, model);
    harness_options.stream_fn = Some(stream_fn);
    harness_options.system_prompt = options.system_prompt;
    harness_options.block_images = services.settings_manager.get_block_images();
    harness_options.tool_result_image_options = Some(pi_agent::tools::image::ProcessImageOptions {
        auto_resize_images: services.settings_manager.get_image_auto_resize(),
        ..Default::default()
    });
    harness_options.thinking_level = Some(thinking_level);
    harness_options.tools = Some(tools);
    harness_options.resources = Some(services.resource_loader.resources.clone());
    harness_options.steering_mode =
        Some(if services.settings_manager.get_steering_mode() == "all" {
            QueueMode::All
        } else {
            QueueMode::OneAtATime
        });
    harness_options.follow_up_mode =
        Some(if services.settings_manager.get_follow_up_mode() == "all" {
            QueueMode::All
        } else {
            QueueMode::OneAtATime
        });
    let (harness, _suspended) = AgentHarness::create(harness_options)
        .await
        .map_err(|error| error.to_string())?;
    let extensions_result = services.resource_loader.extensions.clone();
    Ok(CreateAgentSessionResult {
        session: AgentSession {
            harness,
            session_manager,
            services,
            session_file,
        },
        extensions_result,
        model_fallback_message: None,
    })
}

/// Create a session from already-created cwd-bound services.
pub async fn create_agent_session_from_services(
    services: AgentSessionServices,
    mut options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, String> {
    let diagnostics = services.diagnostics.clone();
    options.cwd = Some(services.cwd.clone());
    options.agent_dir = Some(services.agent_dir.clone());
    options.model_runtime = Some(services.model_runtime);
    options.settings_manager = Some(services.settings_manager);
    options.resource_loader = Some(services.resource_loader);
    let mut result = create_agent_session(options).await?;
    result.session.services.diagnostics = diagnostics;
    Ok(result)
}
