//! Extension system for lifecycle events and custom tools — port of
//! `packages/coding-agent/src/core/extensions/index.ts`.

pub mod loader;
pub mod runner;
pub mod types;
pub mod wrapper;

pub use loader::{
    create_extension_runtime, discover_and_load_extensions, discover_extensions_in_dir,
    load_bundled_extension, load_extension, load_extension_from_factory, load_extensions,
    resolve_extension_entries, ExtensionApi,
};
pub use runner::{
    emit_project_trust_event, ExtensionRunner, KeybindingsConfig, ProjectTrustResult,
    ResourceDiagnostic, ResourceDiscovery,
};
pub use types::{
    EntryRenderer, Extension, ExtensionContext, ExtensionError, ExtensionFlag, ExtensionLoadError,
    ExtensionRuntime, ExtensionShortcut, FlagType, HandlerFn, InputAction, InputEventResult,
    LoadExtensionsResult, MarkdownTransformContext, MarkdownTransformer, MessageRenderer,
    RegisteredCommand, RegisteredTool, RegistrationKind, RegistrationRecord, ResolvedCommand,
    SourceInfo, NOT_INITIALIZED_MESSAGE, STALE_MESSAGE,
};
pub use wrapper::{
    wrap_registered_tool, wrap_registered_tools, WrappedTool, WrappedToolCall, WrappedToolResult,
};
