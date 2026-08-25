//! Extension system for lifecycle events and custom tools — port of
//! `packages/coding-agent/src/core/extensions/index.ts`.

pub mod integration;
pub mod loader;
pub mod runner;
pub mod types;
pub mod wrapper;

pub use integration::{
    install_tools, load_for_mode, load_for_mode_with_reason, load_for_mode_with_reason_and_flags,
    load_for_mode_with_reason_and_flags_and_previous, register_loaded_native_providers,
    register_native_provider, ExtensionHostState, LoadedExtensions,
};
pub use loader::{
    create_extension_runtime, discover_and_load_extensions, discover_extensions_in_dir,
    load_bundled_extension, load_extension, load_extension_from_factory, load_extensions,
    load_extensions_with_host_actions, resolve_extension_entries, ExtensionApi,
};
pub use runner::{
    emit_project_trust_event, ExtensionRunner, KeybindingsConfig, ProjectTrustResult,
    ResourceDiagnostic, ResourceDiscovery,
};
pub use types::{
    EntryRenderer, Extension, ExtensionContext, ExtensionError, ExtensionFlag, ExtensionHostAction,
    ExtensionHostActions, ExtensionLoadError, ExtensionRuntime, ExtensionShortcut, FlagType,
    HandlerFn, InputAction, InputEventResult, LoadExtensionsResult, MarkdownTransformContext,
    MarkdownTransformer, MessageRenderer, RegisteredCommand, RegisteredTool, RegistrationKind,
    RegistrationRecord, ResolvedCommand, SourceInfo, ToolExecuteFn, ToolExecutionRequest,
    NOT_INITIALIZED_MESSAGE, STALE_MESSAGE,
};
pub use wrapper::{
    wrap_registered_tool, wrap_registered_tools, WrappedTool, WrappedToolCall, WrappedToolResult,
};
