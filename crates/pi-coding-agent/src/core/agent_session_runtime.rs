//! Runtime-backed session replacement for the public coding-agent SDK.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_agent::session::state::ForkPosition;

use super::sdk::{
    create_agent_session, AgentSession, CreateAgentSessionOptions, CreateAgentSessionResult,
    SessionManager,
};
use super::session_cwd::assert_session_cwd_exists;

pub type RuntimeFuture =
    Pin<Box<dyn Future<Output = Result<CreateAgentSessionResult, String>> + Send>>;

/// Inputs supplied to the runtime factory for each replacement.
#[derive(Debug)]
pub struct CreateAgentSessionRuntimeOptions {
    pub cwd: String,
    pub agent_dir: String,
    pub session_manager: SessionManager,
    pub session: Option<pi_agent::session::Session<pi_agent::fs::StdFileSystem>>,
    pub session_start_reason: String,
    pub previous_session_file: Option<String>,
}

pub type CreateAgentSessionRuntimeFactory =
    Arc<dyn Fn(CreateAgentSessionRuntimeOptions) -> RuntimeFuture + Send + Sync>;
pub type RebindSessionCallback = Arc<
    dyn Fn(&AgentSession) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync,
>;
pub type BeforeSessionInvalidateCallback = Arc<dyn Fn() + Send + Sync>;

pub struct AgentSessionRuntime {
    session: AgentSession,
    factory: CreateAgentSessionRuntimeFactory,
    diagnostics: Vec<super::sdk::AgentSessionDiagnostic>,
    model_fallback_message: Option<String>,
    rebind_session: Option<RebindSessionCallback>,
    before_session_invalidate: Option<BeforeSessionInvalidateCallback>,
    disposed: bool,
}

impl std::fmt::Debug for AgentSessionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSessionRuntime")
            .field("session", &self.session)
            .field("diagnostics", &self.diagnostics)
            .field("disposed", &self.disposed)
            .finish()
    }
}

impl AgentSessionRuntime {
    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn services(&self) -> &super::sdk::AgentSessionServices {
        &self.session.services
    }

    pub fn cwd(&self) -> &str {
        self.session.cwd()
    }

    pub fn diagnostics(&self) -> &[super::sdk::AgentSessionDiagnostic] {
        &self.diagnostics
    }

    pub fn model_fallback_message(&self) -> Option<&str> {
        self.model_fallback_message.as_deref()
    }

    pub fn set_rebind_session(&mut self, callback: Option<RebindSessionCallback>) {
        self.rebind_session = callback;
    }

    pub fn set_before_session_invalidate(
        &mut self,
        callback: Option<BeforeSessionInvalidateCallback>,
    ) {
        self.before_session_invalidate = callback;
    }

    fn before_switch(&self, reason: &str, target: Option<&str>) -> Result<bool, String> {
        self.session
            .extension_runner()
            .emit_session_before_switch(reason, target)
            .map_err(|errors| format_extension_errors(&errors))
    }

    fn before_fork(&self, entry_id: &str, position: ForkPosition) -> Result<bool, String> {
        self.session
            .extension_runner()
            .emit_session_before_fork(
                entry_id,
                match position {
                    ForkPosition::Before => "before",
                    ForkPosition::At => "at",
                },
            )
            .map_err(|errors| format_extension_errors(&errors))
    }

    async fn teardown(&mut self, reason: &str, target: Option<&str>) {
        self.session
            .shutdown(reason, target, self.before_session_invalidate.as_deref())
            .await;
        self.disposed = true;
    }

    async fn replace(
        &mut self,
        mut request: CreateAgentSessionRuntimeOptions,
        reason: &str,
        target: Option<&str>,
    ) -> Result<(), String> {
        if request.previous_session_file.is_none() {
            request.previous_session_file = self.session.session_file.clone();
        }
        self.teardown(reason, target).await;
        let result = (self.factory)(request).await?;
        self.diagnostics = result.session.services.diagnostics.clone();
        self.model_fallback_message = result.model_fallback_message.clone();
        self.session = result.session;
        self.disposed = false;
        if let Some(callback) = &self.rebind_session {
            callback(&self.session).await?;
        }
        Ok(())
    }

    pub async fn switch_session(
        &mut self,
        session_path: impl AsRef<std::path::Path>,
        cwd_override: Option<&str>,
    ) -> Result<bool, String> {
        let path = session_path.as_ref().to_string_lossy().into_owned();
        if self.before_switch("resume", Some(&path))? {
            return Ok(true);
        }
        let target_cwd = cwd_override
            .map(str::to_string)
            .unwrap_or_else(|| self.session.session_manager.cwd().to_string());
        let manager = SessionManager::new(&target_cwd, self.session.session_manager.session_dir());
        let session = manager.open_session(&path).await?;
        let stored_cwd = session.get_metadata().await.cwd;
        // Match upstream's resume guard: do not tear down the live session or
        // construct cwd-bound services when the persisted cwd has vanished.
        // A caller may explicitly provide an override, which is the supported
        // recovery path for a deleted project directory.
        let effective_cwd = cwd_override.unwrap_or(&stored_cwd);
        assert_session_cwd_exists(Some(&path), effective_cwd, self.cwd())
            .map_err(|error| error.to_string())?;
        let request = CreateAgentSessionRuntimeOptions {
            cwd: cwd_override.map(str::to_string).unwrap_or_else(|| {
                if stored_cwd.is_empty() {
                    target_cwd
                } else {
                    stored_cwd
                }
            }),
            agent_dir: self.session.services.agent_dir.clone(),
            session_manager: manager,
            session: Some(session),
            session_start_reason: "resume".to_string(),
            previous_session_file: None,
        };
        self.replace(request, "resume", Some(&path)).await?;
        Ok(false)
    }

    pub async fn new_session(&mut self, parent_session_id: Option<String>) -> Result<bool, String> {
        if self.before_switch("new", None)? {
            return Ok(true);
        }
        let manager = SessionManager::new(
            self.session.cwd(),
            self.session.session_manager.session_dir(),
        );
        let session = manager.create_session(None, parent_session_id).await?;
        let target = session.get_metadata().await.path;
        let request = CreateAgentSessionRuntimeOptions {
            cwd: self.session.cwd().to_string(),
            agent_dir: self.session.services.agent_dir.clone(),
            session_manager: manager,
            session: Some(session),
            session_start_reason: "new".to_string(),
            previous_session_file: None,
        };
        self.replace(request, "new", nonempty(&target)).await?;
        Ok(false)
    }

    pub async fn fork(
        &mut self,
        entry_id: Option<&str>,
        position: ForkPosition,
    ) -> Result<bool, String> {
        if let Some(entry_id) = entry_id {
            if self.before_fork(entry_id, position)? {
                return Ok(true);
            }
        } else if self.before_switch("new", None)? {
            return Ok(true);
        }
        let manager = SessionManager::new(
            self.session.cwd(),
            self.session.session_manager.session_dir(),
        );
        let source = self.session.session();
        let session = {
            let source = source.lock().await;
            manager.fork_session(&source, entry_id, position).await?
        };
        let target = session.get_metadata().await.path;
        let request = CreateAgentSessionRuntimeOptions {
            cwd: self.session.cwd().to_string(),
            agent_dir: self.session.services.agent_dir.clone(),
            session_manager: manager,
            session: Some(session),
            session_start_reason: "fork".to_string(),
            previous_session_file: None,
        };
        self.replace(request, "fork", nonempty(&target)).await?;
        Ok(false)
    }

    pub async fn import_from_jsonl(
        &mut self,
        input_path: impl AsRef<std::path::Path>,
        cwd_override: Option<&str>,
    ) -> Result<bool, String> {
        let manager = SessionManager::new(
            self.session.cwd(),
            self.session.session_manager.session_dir(),
        );
        let (source, destination) = manager.prepare_import(&input_path)?;
        let imported_path = destination.to_string_lossy().into_owned();
        if self.before_switch("resume", nonempty(&imported_path))? {
            return Ok(true);
        }
        let session = manager
            .import_prepared_session(source, destination, cwd_override)
            .await?;
        let imported_cwd = session.get_metadata().await.cwd;
        assert_session_cwd_exists(Some(&imported_path), &imported_cwd, self.cwd())
            .map_err(|error| error.to_string())?;
        let request = CreateAgentSessionRuntimeOptions {
            cwd: cwd_override
                .map(str::to_string)
                .unwrap_or_else(|| self.session.cwd().to_string()),
            agent_dir: self.session.services.agent_dir.clone(),
            session_manager: manager,
            session: Some(session),
            session_start_reason: "resume".to_string(),
            previous_session_file: None,
        };
        self.replace(request, "resume", nonempty(&imported_path))
            .await?;
        Ok(false)
    }

    pub async fn dispose(&mut self) {
        if !self.disposed {
            self.teardown("quit", None).await;
        }
    }
}

pub async fn create_agent_session_runtime(
    factory: CreateAgentSessionRuntimeFactory,
    options: CreateAgentSessionRuntimeOptions,
) -> Result<AgentSessionRuntime, String> {
    let result = factory(options).await?;
    Ok(AgentSessionRuntime {
        diagnostics: result.session.services.diagnostics.clone(),
        model_fallback_message: result.model_fallback_message.clone(),
        session: result.session,
        factory,
        rebind_session: None,
        before_session_invalidate: None,
        disposed: false,
    })
}

pub fn default_runtime_factory() -> CreateAgentSessionRuntimeFactory {
    Arc::new(|options| {
        Box::pin(create_agent_session(CreateAgentSessionOptions {
            cwd: Some(options.cwd),
            agent_dir: Some(options.agent_dir),
            session_manager: Some(options.session_manager),
            session: options.session,
            extension_reason: options.session_start_reason,
            previous_session_file: options.previous_session_file,
            ..Default::default()
        }))
    })
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn format_extension_errors(errors: &[crate::core::extensions::ExtensionError]) -> String {
    errors
        .iter()
        .map(|error| format!("{} {}: {}", error.extension_path, error.event, error.error))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::model_runtime::register_faux_provider;
    use crate::core::settings::{SettingsManager, SettingsMap};
    use pi_ai::providers::{
        faux_assistant_message, FauxAssistantOptions, FauxResponseStep, RegisterFauxProviderOptions,
    };
    use pi_ai::types::ContentBlock;

    fn temp_root(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pi-coding-agent-sdk-{label}-{}",
            pi_agent::session::new_id()
        ));
        std::fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn faux_runtime() -> (
        crate::core::model_runtime::ModelRuntime,
        pi_ai::model::Model,
    ) {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let core = register_faux_provider(&models, &RegisterFauxProviderOptions::default());
        core.set_responses(vec![
            FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("runtime-ready-before")],
                FauxAssistantOptions::default(),
            )),
            FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("runtime-ready-after")],
                FauxAssistantOptions::default(),
            )),
        ]);
        let model = core.get_model(None).expect("faux model").clone();
        (crate::core::model_runtime::ModelRuntime::new(models), model)
    }

    fn factory(
        model_runtime: crate::core::model_runtime::ModelRuntime,
        model: pi_ai::model::Model,
    ) -> CreateAgentSessionRuntimeFactory {
        Arc::new(move |options| {
            let model_runtime = model_runtime.clone();
            let model = model.clone();
            Box::pin(create_agent_session(CreateAgentSessionOptions {
                cwd: Some(options.cwd),
                agent_dir: Some(options.agent_dir),
                model_runtime: Some(model_runtime),
                model: Some(model),
                settings_manager: Some(SettingsManager::in_memory(SettingsMap::new())),
                session_manager: Some(options.session_manager),
                session: options.session,
                no_tools: true,
                extension_reason: options.session_start_reason,
                previous_session_file: options.previous_session_file,
                ..Default::default()
            }))
        })
    }

    fn cancelling_factory(
        model_runtime: crate::core::model_runtime::ModelRuntime,
        model: pi_ai::model::Model,
    ) -> CreateAgentSessionRuntimeFactory {
        let base = factory(model_runtime, model);
        Arc::new(move |options| {
            let base = base.clone();
            Box::pin(async move {
                let mut result = base(options).await?;
                let mut extension = crate::core::extensions::Extension {
                    path: "cancel-session-switch".to_string(),
                    ..Default::default()
                };
                let cancel: crate::core::extensions::HandlerFn =
                    Arc::new(|_, _| Ok(Some(serde_json::json!({"cancel": true}))));
                extension
                    .handlers
                    .insert("session_before_switch".to_string(), vec![cancel]);
                let extension_runtime = Arc::new(std::sync::Mutex::new(
                    crate::core::extensions::ExtensionRuntime::new(),
                ));
                let runner = crate::core::extensions::ExtensionRunner::new(
                    vec![extension],
                    extension_runtime.clone(),
                    result.session.cwd().to_string(),
                );
                result.session.services.resource_loader.extensions.runner = Arc::new(runner);
                result.session.services.resource_loader.extensions.runtime = extension_runtime;
                Ok(result)
            })
        })
    }

    #[tokio::test]
    async fn restart_replaces_the_live_session_and_keeps_it_runnable() {
        let root = temp_root("restart");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let manager = SessionManager::new(&cwd, root.to_string_lossy());
        let (model_runtime, model) = faux_runtime();
        let factory = factory(model_runtime, model);
        let first = manager.create_session(None, None).await.unwrap();
        let first_path = first.get_metadata().await.path;
        let mut runtime = create_agent_session_runtime(
            factory,
            CreateAgentSessionRuntimeOptions {
                cwd: cwd.clone(),
                agent_dir: root.to_string_lossy().into_owned(),
                session_manager: manager,
                session: Some(first),
                session_start_reason: "startup".to_string(),
                previous_session_file: None,
            },
        )
        .await
        .unwrap();

        runtime
            .session()
            .prompt_text("before replacement")
            .await
            .unwrap();
        let old_extension_runtime = runtime
            .session()
            .services
            .resource_loader
            .extensions
            .runtime
            .clone();
        assert!(!runtime.new_session(None).await.unwrap());
        let second_path = runtime
            .session()
            .session_file
            .clone()
            .expect("replacement session file");
        assert_ne!(second_path, first_path);
        assert!(
            old_extension_runtime
                .lock()
                .expect("old extension runtime")
                .is_stale(),
            "replacement must invalidate the previous extension runtime"
        );
        runtime
            .session()
            .prompt_text("after replacement")
            .await
            .unwrap();
        runtime.dispose().await;
        assert!(runtime.session().harness.is_closed());
        let error = runtime
            .session()
            .prompt_text("after dispose")
            .await
            .expect_err("disposed runtime must reject later prompts");
        assert!(error.to_string().contains("closed"), "{error}");

        let first_jsonl = std::fs::read_to_string(&first_path).unwrap();
        assert!(first_jsonl.contains("before replacement"), "{first_jsonl}");
        assert!(!first_jsonl.contains("after replacement"), "{first_jsonl}");
        let second_jsonl = std::fs::read_to_string(&second_path).unwrap();
        assert!(second_jsonl.contains("after replacement"), "{second_jsonl}");
        assert!(
            !second_jsonl.contains("before replacement"),
            "{second_jsonl}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn replacement_factory_receives_previous_session_file() {
        let root = temp_root("previous-session");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let manager = SessionManager::new(&cwd, root.to_string_lossy());
        let (model_runtime, model) = faux_runtime();
        let base = factory(model_runtime, model);
        let observed = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let recording: CreateAgentSessionRuntimeFactory = {
            let observed = Arc::clone(&observed);
            Arc::new(move |options| {
                observed
                    .lock()
                    .expect("observation lock")
                    .push(options.previous_session_file.clone());
                base(options)
            })
        };
        let first = manager.create_session(None, None).await.unwrap();
        let first_path = first.get_metadata().await.path;
        let mut runtime = create_agent_session_runtime(
            recording,
            CreateAgentSessionRuntimeOptions {
                cwd: cwd.clone(),
                agent_dir: root.to_string_lossy().into_owned(),
                session_manager: manager,
                session: Some(first),
                session_start_reason: "startup".to_string(),
                previous_session_file: None,
            },
        )
        .await
        .unwrap();

        runtime.new_session(None).await.unwrap();
        {
            let observed = observed.lock().expect("observation lock");
            assert_eq!(observed.first(), Some(&None));
            assert_eq!(
                observed.get(1).and_then(Option::as_deref),
                Some(first_path.as_str())
            );
        }

        runtime.dispose().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_import_is_a_real_failure_and_abort_is_reported_when_idle() {
        let root = temp_root("failure");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let manager = SessionManager::new(&cwd, root.to_string_lossy());
        let (model_runtime, model) = faux_runtime();
        let factory = factory(model_runtime, model);
        let session = manager.create_session(None, None).await.unwrap();
        let mut runtime = create_agent_session_runtime(
            factory,
            CreateAgentSessionRuntimeOptions {
                cwd,
                agent_dir: root.to_string_lossy().into_owned(),
                session_manager: manager,
                session: Some(session),
                session_start_reason: "startup".to_string(),
                previous_session_file: None,
            },
        )
        .await
        .unwrap();
        let error = runtime
            .import_from_jsonl(root.join("missing.jsonl"), None)
            .await
            .unwrap_err();
        assert!(error.contains("File not found"), "{error}");
        let abort_error = runtime.session().abort().await.unwrap_err();
        assert!(abort_error.to_string().contains("active"), "{abort_error}");
        runtime.dispose().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellable_switch_keeps_the_current_session_alive() {
        let root = temp_root("cancel");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let manager = SessionManager::new(&cwd, root.to_string_lossy());
        let (model_runtime, model) = faux_runtime();
        let factory = cancelling_factory(model_runtime, model);
        let first = manager.create_session(None, None).await.unwrap();
        let first_path = first.get_metadata().await.path;
        let mut runtime = create_agent_session_runtime(
            factory,
            CreateAgentSessionRuntimeOptions {
                cwd: cwd.clone(),
                agent_dir: root.to_string_lossy().into_owned(),
                session_manager: manager.clone(),
                session: Some(first),
                session_start_reason: "startup".to_string(),
                previous_session_file: None,
            },
        )
        .await
        .unwrap();

        assert!(runtime.new_session(None).await.unwrap());
        assert_eq!(
            runtime.session().session_file.as_deref(),
            Some(first_path.as_str())
        );
        assert!(!runtime.session().harness.is_closed());
        runtime.dispose().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn switch_rejects_missing_stored_cwd_before_teardown() {
        let root = temp_root("missing-cwd");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let manager = SessionManager::new(&cwd, root.to_string_lossy());
        let (model_runtime, model) = faux_runtime();
        let factory = factory(model_runtime, model);
        let first = manager.create_session(None, None).await.unwrap();
        let first_path = first.get_metadata().await.path;
        let mut runtime = create_agent_session_runtime(
            factory,
            CreateAgentSessionRuntimeOptions {
                cwd: cwd.clone(),
                agent_dir: root.to_string_lossy().into_owned(),
                session_manager: manager,
                session: Some(first),
                session_start_reason: "startup".to_string(),
                previous_session_file: None,
            },
        )
        .await
        .unwrap();

        let target = root.join("missing-cwd.jsonl");
        let missing_cwd = root.join("deleted-project");
        std::fs::write(
            &target,
            format!(
                "{{\"kind\":\"header\",\"version\":4,\"id\":\"target\",\"createdAt\":1,\"cwd\":{}}}\n",
                serde_json::to_string(&missing_cwd.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();

        let error = runtime
            .switch_session(&target, None)
            .await
            .expect_err("missing session cwd must stop before replacement");
        assert!(error.contains("Stored session working directory does not exist"));
        assert_eq!(
            runtime.session().session_file.as_deref(),
            Some(first_path.as_str())
        );
        assert!(!runtime.session().harness.is_closed());

        runtime.dispose().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
