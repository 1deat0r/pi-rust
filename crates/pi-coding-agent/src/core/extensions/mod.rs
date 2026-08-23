//! Extension system for lifecycle events and custom tools — port of
//! `packages/coding-agent/src/core/extensions/index.ts`.

pub mod loader;
pub mod runner;
pub mod types;
pub mod wrapper;

pub use loader::{
    create_extension_runtime, discover_and_load_extensions, discover_extensions_in_dir,
    load_bundled_extension, load_extension, load_extensions, resolve_extension_entries,
};
pub use runner::{ExtensionRunner, KeybindingsConfig, ResourceDiagnostic};
pub use types::{
    Extension, ExtensionContext, ExtensionError, ExtensionFlag, ExtensionLoadError,
    ExtensionRuntime, ExtensionShortcut, FlagType, HandlerFn, LoadExtensionsResult,
    RegisteredCommand, RegisteredTool, ResolvedCommand, SourceInfo,
};
pub use wrapper::{
    wrap_registered_tool, wrap_registered_tools, WrappedTool, WrappedToolCall, WrappedToolResult,
};
