//! Agent runtime and harness — port of `@earendil-works/pi-agent-core`.
//!
//! The crate contains the Rust agent loop, rich lifecycle runtime, harness
//! composition surface, filesystem abstraction, and JSONL v4 session
//! storage. Observable behavior is validated by the crate tests and the
//! root exhaustive parity matrix; harness operations execute through the
//! session-backed runtime or return a concrete tagged/configuration error.

pub mod agent;
pub mod fs;
pub mod harness;
pub mod messages;
pub mod proxy;
pub mod rich_agent;
pub mod search;
pub mod session;
pub mod stream_fn;
pub mod tools;
pub mod types;

pub use agent::{
    run_agent_loop, user_text_prompt, AgentContext, AgentEvent, AgentLoopConfig,
    StreamFnWithOptions,
};
pub use rich_agent::{
    OverflowRecoveryHook, OverflowRecoveryReason, OverflowRecoveryRequest, OverflowRecoveryResult,
    PrepareNextTurnContext, PrepareNextTurnHook, PrepareNextTurnWithContextHook,
    RichAgentLoopConfig, RichAgentLoopTurnUpdate,
};
pub use search::{
    create_scanning_session_search, create_scanning_session_search_with_options,
    create_typed_scanning_session_search, scanning_entries, LazyScanningSessionSearch,
    ScanningReadable, ScanningReadableOptions, ScanningSearchHitCreator, ScanningSearchMatcher,
    ScanningSearchOptions, ScanningSearchTextProjector, ScanningSessionSearch,
    ScanningSourceOptionsFactory, SessionSearchCandidate, SessionSearchHit, SessionSearchOptions,
    TypedScanningSearchOptions, TypedScanningSourceOptionsFactory,
};
pub use types::AgentMessage;
