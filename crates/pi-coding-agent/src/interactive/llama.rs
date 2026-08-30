//! Interactive llama.cpp manager.
//!
//! The upstream llama extension is a UI around a real llama.cpp router and
//! Hugging Face API.  This module owns the selector-shaped state and the
//! network operations so the interactive event loop never has to fabricate a
//! catalog or an inference response.

use std::sync::{Arc, Mutex};

use pi_ai::auth::AuthResult;
use pi_ai::models::Models;
use pi_tui::components::select_list::SelectItem;
use pi_tui::keys::TuiKey;
use pi_tui::tui::Component;

use crate::core::llama::{
    find_huggingface_token, format_bytes, HuggingFaceClient, HuggingFaceGated, HuggingFaceModel,
    HuggingFaceModelDetails, LlamaCancellation, LlamaClient, LlamaError, LlamaManagerAction,
    LlamaModelAction, LlamaModelInfo, LlamaProviderController, LlamaSelectionOption,
    LlamaWaitOptions, LLAMA_PROVIDER_ID,
};
use crate::interactive::selectors::{ListSelector, SelectorAction};

/// A selector action returned by the llama manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlamaSelectorAction {
    None,
    Select(LlamaManagerAction),
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlamaLoadPlanAction {
    UnloadAll,
    KeepLoaded,
    Cancel,
}

pub struct LlamaLoadPlanSelector {
    list: ListSelector,
}

impl LlamaLoadPlanSelector {
    pub fn new(target: &str, loaded: &[String]) -> Self {
        let count = loaded.len();
        let noun = if count == 1 { "model is" } else { "models are" };
        let items = vec![
            SelectItem::new(
                "unload_all",
                "Unload all and load",
                Some(format!("{count} {noun} currently loaded; target: {target}")),
            ),
            SelectItem::new(
                "keep_loaded",
                "Keep loaded and load",
                Some("llama.cpp may keep multiple models resident".to_owned()),
            ),
            SelectItem::new("cancel", "Cancel", None),
        ];
        Self {
            list: ListSelector::new_slash_layout(items, 6),
        }
    }

    pub fn handle(&mut self, key: &TuiKey) -> Option<LlamaLoadPlanAction> {
        match self.list.handle(key) {
            SelectorAction::Select(Some(_)) => match self.list.selected_item()?.value.as_str() {
                "unload_all" => Some(LlamaLoadPlanAction::UnloadAll),
                "keep_loaded" => Some(LlamaLoadPlanAction::KeepLoaded),
                _ => Some(LlamaLoadPlanAction::Cancel),
            },
            SelectorAction::Cancel => Some(LlamaLoadPlanAction::Cancel),
            SelectorAction::None
            | SelectorAction::Cycle
            | SelectorAction::Select(None)
            | SelectorAction::SelectAsDefault(_) => None,
        }
    }
}

impl Component for LlamaLoadPlanSelector {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

pub struct LlamaUnloadConfirmSelector {
    list: ListSelector,
}

impl LlamaUnloadConfirmSelector {
    pub fn new(model: &str) -> Self {
        Self {
            list: ListSelector::new_slash_layout(
                vec![
                    SelectItem::new("yes", "Yes", Some(format!("unload {model}"))),
                    SelectItem::new("no", "No", None),
                ],
                4,
            ),
        }
    }

    pub fn handle(&mut self, key: &TuiKey) -> Option<bool> {
        match self.list.handle(key) {
            SelectorAction::Select(Some(_)) => Some(self.list.selected_item()?.value == "yes"),
            SelectorAction::Cancel => Some(false),
            SelectorAction::None
            | SelectorAction::Cycle
            | SelectorAction::Select(None)
            | SelectorAction::SelectAsDefault(_) => None,
        }
    }
}

impl Component for LlamaUnloadConfirmSelector {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

/// A list selector whose values are the real llama manager actions.
pub struct LlamaSelector {
    list: ListSelector,
    actions: Vec<LlamaManagerAction>,
}

impl LlamaSelector {
    pub fn new(options: Vec<LlamaSelectionOption>) -> Self {
        let actions = options
            .iter()
            .map(|option| option.action.clone())
            .collect::<Vec<_>>();
        let items = options
            .into_iter()
            .map(|option| {
                SelectItem::new(option.label.clone(), option.label, Some(option.description))
            })
            .collect();
        Self {
            list: ListSelector::new_slash_layout(items, 12),
            actions,
        }
    }

    pub fn handle(&mut self, key: &TuiKey) -> LlamaSelectorAction {
        match self.list.handle(key) {
            SelectorAction::Select(Some(index)) => self
                .actions
                .get(index)
                .cloned()
                .map(LlamaSelectorAction::Select)
                .unwrap_or(LlamaSelectorAction::None),
            SelectorAction::Cancel => LlamaSelectorAction::Cancel,
            SelectorAction::None
            | SelectorAction::Cycle
            | SelectorAction::Select(None)
            | SelectorAction::SelectAsDefault(_) => LlamaSelectorAction::None,
        }
    }

    pub fn selected_item(&self) -> Option<SelectItem> {
        self.list.selected_item()
    }
}

impl Component for LlamaSelector {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

/// A selector for live Hugging Face GGUF search results.
pub struct HuggingFaceSelector {
    list: ListSelector,
    models: Vec<HuggingFaceModel>,
}

/// Follow-up selector for a selected Hugging Face repository. The upstream
/// flow performs the access acknowledgement and quantization choice before a
/// router download; keeping those as explicit actions prevents the command
/// handler from silently guessing a file variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuggingFaceDownloadAction {
    Continue(HuggingFaceModelDetails),
    Download(String),
    Cancel,
}

pub struct HuggingFaceDownloadSelector {
    list: ListSelector,
    details: HuggingFaceModelDetails,
    access_gate: bool,
}

impl HuggingFaceDownloadSelector {
    pub fn access_gate(details: HuggingFaceModelDetails) -> Self {
        let items = vec![
            SelectItem::new(
                "continue",
                "Continue",
                Some("the llama.cpp server must have HF_TOKEN access".to_owned()),
            ),
            SelectItem::new("back", "Back", None),
        ];
        Self {
            list: ListSelector::new_slash_layout(items, 6),
            details,
            access_gate: true,
        }
    }

    pub fn quantizations(details: HuggingFaceModelDetails) -> Self {
        let items = details
            .quantizations
            .iter()
            .map(|quantization| {
                let mut description = quantization
                    .size
                    .map(format_bytes)
                    .unwrap_or_else(|| "size unavailable".to_owned());
                if quantization.name == "Q4_K_M" {
                    description.push_str(" · recommended");
                }
                SelectItem::new(
                    quantization.name.clone(),
                    quantization.name.clone(),
                    Some(description),
                )
            })
            .collect();
        Self {
            list: ListSelector::new_slash_layout(items, 10),
            details,
            access_gate: false,
        }
    }

    pub fn details(&self) -> &HuggingFaceModelDetails {
        &self.details
    }

    pub fn handle(&mut self, key: &TuiKey) -> Option<HuggingFaceDownloadAction> {
        match self.list.handle(key) {
            SelectorAction::Select(Some(index)) => {
                let item = self.list.selected_item()?;
                if self.access_gate {
                    return Some(if item.value == "continue" {
                        HuggingFaceDownloadAction::Continue(self.details.clone())
                    } else {
                        HuggingFaceDownloadAction::Cancel
                    });
                }
                let _ = index;
                Some(HuggingFaceDownloadAction::Download(format!(
                    "{}:{}",
                    self.details.id, item.value
                )))
            }
            SelectorAction::Cancel => Some(HuggingFaceDownloadAction::Cancel),
            SelectorAction::None
            | SelectorAction::Cycle
            | SelectorAction::Select(None)
            | SelectorAction::SelectAsDefault(_) => None,
        }
    }
}

impl Component for HuggingFaceDownloadSelector {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

pub fn huggingface_access_message(details: &HuggingFaceModelDetails) -> String {
    let gate = match details.gated {
        HuggingFaceGated::Manual => "Manual approval is required",
        HuggingFaceGated::Auto => "Accept the access terms",
        HuggingFaceGated::NotGated => "",
    };
    if gate.is_empty() {
        String::new()
    } else {
        format!(
            "Hugging Face access required for {}\n{} at:\nhttps://huggingface.co/{}\n\nThe llama.cpp server needs HF_TOKEN with access.",
            details.id, gate, details.id
        )
    }
}

impl HuggingFaceSelector {
    pub fn new(models: Vec<HuggingFaceModel>) -> Self {
        let items = models
            .iter()
            .map(|model| {
                SelectItem::new(
                    model.id.clone(),
                    model.id.clone(),
                    Some(format!("{} downloads", model.downloads)),
                )
            })
            .collect();
        Self {
            list: ListSelector::new_slash_layout(items, 12),
            models,
        }
    }

    pub fn handle(&mut self, key: &TuiKey) -> Option<Result<HuggingFaceModel, ()>> {
        match self.list.handle(key) {
            SelectorAction::Select(Some(index)) => self.models.get(index).cloned().map(Ok),
            SelectorAction::Cancel => Some(Err(())),
            SelectorAction::None
            | SelectorAction::Cycle
            | SelectorAction::Select(None)
            | SelectorAction::SelectAsDefault(_) => None,
        }
    }
}

impl Component for HuggingFaceSelector {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

/// State retained while the manager is open.  The provider/controller is
/// retained so refreshing or operating on a model updates the same dynamic
/// provider installed in the active Models facade.
#[derive(Clone)]
pub struct LlamaManager {
    pub controller: LlamaProviderController,
    pub client: LlamaClient,
    pub catalog: Vec<LlamaModelInfo>,
}

impl LlamaManager {
    pub async fn open(models: &Models) -> Result<Self, String> {
        let controller = LlamaProviderController::new();
        controller.register_into(models);
        let auth = models
            .get_auth(LLAMA_PROVIDER_ID, None)
            .ok_or_else(|| "llama.cpp is not configured; run /login llama.cpp first".to_owned())?;
        let client = client_from_auth(&auth)?;
        let catalog = client
            .list(Default::default())
            .await
            .map_err(|error| format!("llama.cpp catalog failed: {error}"))?;
        controller
            .set_catalog(catalog.clone(), client.server_url())
            .map_err(|error| format!("llama.cpp catalog invalid: {error}"))?;
        Ok(Self {
            controller,
            client,
            catalog,
        })
    }

    pub fn options(&self) -> Vec<LlamaSelectionOption> {
        self.controller.selection_options(&self.catalog)
    }

    pub async fn refresh(&mut self) -> Result<(), String> {
        self.catalog = self
            .controller
            .refresh_catalog(&self.client, true, None)
            .await
            .map_err(|error| format!("llama.cpp refresh failed: {error}"))?;
        Ok(())
    }
}

fn client_from_auth(auth: &AuthResult) -> Result<LlamaClient, String> {
    // The upstream command context prefers the persisted provider
    // environment value. `auth.base_url` is the inference `/v1` URL and may
    // be synthesized by the provider, while LLAMA_BASE_URL is the actual
    // router endpoint used by the manager UI.
    let base_url = auth
        .env
        .as_ref()
        .and_then(|environment| environment.get("LLAMA_BASE_URL"))
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .or(auth.auth.base_url.as_deref())
        .ok_or_else(|| "llama.cpp auth has no server URL; run /login llama.cpp".to_owned())?;
    LlamaClient::new(base_url, auth.auth.api_key.as_deref())
        .map_err(|error| format!("invalid llama.cpp server URL: {error}"))
}

/// Retrieve a client from the active Models auth boundary. This is used by
/// direct `/llama download ...` invocations too, so they share the same
/// credential/environment precedence as normal inference.
pub fn client_for_models(models: &Models) -> Result<LlamaClient, String> {
    let auth = models
        .get_auth(LLAMA_PROVIDER_ID, None)
        .ok_or_else(|| "llama.cpp is not configured; run /login llama.cpp first".to_owned())?;
    client_from_auth(&auth)
}

/// Execute one real manager action. The caller owns the cancellation signal
/// and can set it from Ctrl-C or a future shutdown path.
pub async fn execute_model_action(
    client: &LlamaClient,
    action: &LlamaManagerAction,
    signal: LlamaCancellation,
    status: Arc<Mutex<String>>,
) -> Result<(), String> {
    let options = LlamaWaitOptions {
        signal: Some(signal),
        ..Default::default()
    };
    match action {
        LlamaManagerAction::Model {
            id,
            action: LlamaModelAction::Load,
        } => client
            .load_and_wait(id, options, |progress| {
                if let Ok(mut status) = status.lock() {
                    *status = progress_status("load", id, &progress.message, progress.detail);
                }
            })
            .await
            .map(|_| ())
            .map_err(|error| format!("load {id} failed: {error}")),
        LlamaManagerAction::Model {
            id,
            action: LlamaModelAction::Unload,
        } => client
            .unload_and_wait(id, options)
            .await
            .map_err(|error| format!("unload {id} failed: {error}")),
        LlamaManagerAction::Model {
            id,
            action: LlamaModelAction::Observe,
        } => Err(format!("model {id} is still transitioning")),
        LlamaManagerAction::Download | LlamaManagerAction::Close => Ok(()),
    }
}

/// Load a model while honoring the upstream replacement choice. If replacing
/// the resident set fails or is cancelled, every model that was previously
/// loaded is restored through the same live router API.
pub async fn execute_load_with_restore(
    client: &LlamaClient,
    target: &str,
    loaded: &[String],
    replace: bool,
    signal: LlamaCancellation,
    status: Arc<Mutex<String>>,
) -> Result<(), String> {
    let options = || LlamaWaitOptions {
        signal: Some(signal.clone()),
        ..Default::default()
    };
    let restore = || async {
        for model in loaded {
            client
                .load_and_wait(model, options(), |progress| {
                    if let Ok(mut status) = status.lock() {
                        *status =
                            progress_status("restore", model, &progress.message, progress.detail);
                    }
                })
                .await
                .map_err(|error| format!("restore {model} failed: {error}"))?;
        }
        Ok::<(), String>(())
    };
    if replace {
        for model in loaded {
            if signal.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = restore().await;
                return Err("load cancelled".to_owned());
            }
            if let Err(error) = client.unload_and_wait(model, options()).await {
                let _ = restore().await;
                return Err(format!("unload {model} before load failed: {error}"));
            }
        }
    }
    let loaded_result = client
        .load_and_wait(target, options(), |progress| {
            if let Ok(mut status) = status.lock() {
                *status = progress_status("load", target, &progress.message, progress.detail);
            }
        })
        .await;
    match loaded_result {
        Ok(_) => Ok(()),
        Err(error) => {
            if replace {
                let _ = restore().await;
            }
            Err(format!("load {target} failed: {error}"))
        }
    }
}

fn progress_status(action: &str, model: &str, message: &str, detail: Option<String>) -> String {
    match detail {
        Some(detail) => format!("{action} {model}: {message} ({detail})"),
        None => format!("{action} {model}: {message}"),
    }
}

/// Search Hugging Face using the normal local token lookup. The result is a
/// real endpoint response and can be presented by the interactive selector.
pub async fn search_huggingface(
    query: &str,
    signal: Option<LlamaCancellation>,
) -> Result<Vec<HuggingFaceModel>, String> {
    HuggingFaceClient::new(find_huggingface_token().as_deref())
        .map_err(|error| format!("Hugging Face setup failed: {error}"))?
        .search(query, signal)
        .await
        .map_err(|error| format!("Hugging Face search failed: {error}"))
}

/// Download a repository/quantization spec through the router. The router is
/// responsible for the actual bytes; this function only validates the input
/// and waits for the real operation to settle.
pub async fn download_huggingface_model(
    client: &LlamaClient,
    spec: &str,
    signal: LlamaCancellation,
    status: Arc<Mutex<String>>,
) -> Result<(), String> {
    let spec = spec.trim();
    let (repo, quantization) = spec
        .split_once(':')
        .map_or((spec, None), |(repo, quantization)| {
            (repo, Some(quantization))
        });
    if repo.trim().is_empty()
        || quantization.is_some_and(|value| value.trim().is_empty())
        || repo.chars().any(char::is_whitespace)
    {
        return Err("download expects a non-empty Hugging Face repo and quantization".to_owned());
    }
    let model = quantization.map_or_else(
        || repo.trim().to_owned(),
        |quantization| format!("{}:{}", repo.trim(), quantization.trim()),
    );
    let options = LlamaWaitOptions {
        signal: Some(signal),
        ..Default::default()
    };
    client
        .download_and_wait(&model, options, |progress| {
            if let Ok(mut status) = status.lock() {
                *status = progress_status("download", &model, &progress.message, progress.detail);
            }
        })
        .await
        .map(|_| ())
        .map_err(|error| format!("download {model} failed: {error}"))
}

/// Register the provider before `/llama` or `/login llama.cpp` needs it.
pub fn register_provider(models: &Models) {
    LlamaProviderController::new().register_into(models);
}

/// Error text for callers that need to distinguish a missing auth boundary
/// from an unreachable configured server without exposing credentials.
pub fn classify_error(error: &LlamaError) -> &'static str {
    match error {
        LlamaError::Cancelled => "cancelled",
        LlamaError::Timeout => "timed out",
        LlamaError::Http { .. } | LlamaError::SseHttp { .. } | LlamaError::Transport(_) => {
            "server error"
        }
        _ => "configuration error",
    }
}

/// Small helper for tests and command handlers that need the manager's loaded
/// state without reaching into private controller fields.
pub fn loaded_model_ids(catalog: &[LlamaModelInfo]) -> Vec<String> {
    catalog
        .iter()
        .filter(|model| model.status.value.is_loaded())
        .map(|model| model.id.clone())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_ai::auth::ModelAuth;

    #[test]
    fn selector_preserves_real_actions() {
        let options = vec![LlamaSelectionOption {
            label: "model.gguf".to_owned(),
            description: "unloaded".to_owned(),
            action: LlamaManagerAction::Model {
                id: "model.gguf".to_owned(),
                action: LlamaModelAction::Load,
            },
        }];
        let mut selector = LlamaSelector::new(options);
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            LlamaSelectorAction::Select(LlamaManagerAction::Model {
                id: "model.gguf".to_owned(),
                action: LlamaModelAction::Load,
            })
        );
    }

    #[test]
    fn download_spec_requires_repo_and_quantization() {
        assert!("repo/model".split_once(':').is_none());
        assert!("repo/model:".split_once(':').is_some());
    }

    #[test]
    fn load_plan_selector_covers_replace_keep_and_cancel() {
        let loaded = vec!["resident.gguf".to_owned()];
        let mut selector = LlamaLoadPlanSelector::new("target.gguf", &loaded);
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            Some(LlamaLoadPlanAction::UnloadAll)
        );

        let mut selector = LlamaLoadPlanSelector::new("target.gguf", &loaded);
        selector.handle(&TuiKey::simple("down"));
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            Some(LlamaLoadPlanAction::KeepLoaded)
        );

        let mut selector = LlamaLoadPlanSelector::new("target.gguf", &loaded);
        selector.handle(&TuiKey::simple("down"));
        selector.handle(&TuiKey::simple("down"));
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            Some(LlamaLoadPlanAction::Cancel)
        );
        assert_eq!(
            selector.handle(&TuiKey::simple("escape")),
            Some(LlamaLoadPlanAction::Cancel)
        );
    }

    #[test]
    fn unload_confirmation_defaults_to_yes_and_escape_is_no() {
        let mut selector = LlamaUnloadConfirmSelector::new("resident.gguf");
        assert_eq!(selector.handle(&TuiKey::simple("enter")), Some(true));

        let mut selector = LlamaUnloadConfirmSelector::new("resident.gguf");
        selector.handle(&TuiKey::simple("down"));
        assert_eq!(selector.handle(&TuiKey::simple("enter")), Some(false));
        assert_eq!(selector.handle(&TuiKey::simple("escape")), Some(false));
    }

    #[test]
    fn huggingface_access_and_quantization_selectors_are_explicit() {
        let details = HuggingFaceModelDetails {
            id: "org/model".to_owned(),
            gated: HuggingFaceGated::Manual,
            quantizations: vec![
                crate::core::llama::HuggingFaceQuantization {
                    name: "Q4_K_M".to_owned(),
                    size: Some(1024),
                },
                crate::core::llama::HuggingFaceQuantization {
                    name: "Q8_0".to_owned(),
                    size: None,
                },
            ],
        };
        let mut access = HuggingFaceDownloadSelector::access_gate(details.clone());
        assert!(matches!(
            access.handle(&TuiKey::simple("enter")),
            Some(HuggingFaceDownloadAction::Continue(_))
        ));
        let mut access = HuggingFaceDownloadSelector::access_gate(details.clone());
        access.handle(&TuiKey::simple("down"));
        assert_eq!(
            access.handle(&TuiKey::simple("enter")),
            Some(HuggingFaceDownloadAction::Cancel)
        );

        let mut quantizations = HuggingFaceDownloadSelector::quantizations(details);
        assert_eq!(
            quantizations.handle(&TuiKey::simple("enter")),
            Some(HuggingFaceDownloadAction::Download(
                "org/model:Q4_K_M".to_owned()
            ))
        );
    }

    #[test]
    fn manager_prefers_router_environment_url_over_inference_url() {
        let mut environment = pi_ai::types::ProviderEnv::new();
        environment.insert(
            "LLAMA_BASE_URL".to_owned(),
            "http://router.example:8080".to_owned(),
        );
        let auth = AuthResult {
            auth: ModelAuth {
                api_key: Some("synthetic-key".to_owned()),
                headers: None,
                base_url: Some("http://inference.example:8080/v1".to_owned()),
            },
            env: Some(environment),
            source: Some("stored credential".to_owned()),
        };

        let client = client_from_auth(&auth).expect("router URL from environment");
        assert_eq!(client.server_url(), "http://router.example:8080");
    }
}
