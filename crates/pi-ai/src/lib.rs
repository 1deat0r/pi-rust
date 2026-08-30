//! Unified multi-provider LLM API — port of `@earendil-works/pi-ai`.
//!
//! The crate includes the core message/content/model/stream contract, event
//! streams, incremental JSON and SSE parsers, model/catalog/auth stores,
//! OAuth, retry/proxy helpers, all bundled provider adaptors, the native
//! Radius protocol, and the deterministic faux provider used only by tests.

pub mod api;
pub mod auth;
pub mod auth_flows;
pub mod error;
pub mod event_stream;
pub mod images;
pub mod model;
pub mod model_catalog;
pub mod models;
pub mod oauth;
pub mod partial_json;
pub mod providers;
pub mod sse;
pub mod types;
pub mod utils;

pub use event_stream::{create_error_stream, AssistantMessageEventStream};
pub use model::{
    calculate_cost, clamp_thinking_level, get_supported_thinking_levels, models_are_equal, Model,
    ModelCost, ModelCostTier, ModelInput,
};
pub use types::*;
