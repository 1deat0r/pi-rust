//! Unified multi-provider LLM API — port of `@earendil-works/pi-ai`.
//!
//! Current port coverage (see TODO.md):
//! - `types.rs`: core message/content/model/stream types (data contract).
//! - `event_stream.rs`: `AssistantMessageEventStream` push stream.
//! - `partial_json.rs`: tolerant incremental JSON parser (streaming tool args).
//! - `sse.rs`: Server-Sent Events parser.
//! - `model.rs`: Model/ModelCost, cost accounting, thinking-level helpers.
//! - `providers/faux.rs`: scripted test provider with upstream usage-estimation
//!   and delta-streaming semantics.

pub mod api;
pub mod auth;
pub mod auth_flows;
pub mod oauth;
pub mod event_stream;
pub mod images;
pub mod model;
pub mod model_catalog;
pub mod models;
pub mod partial_json;
pub mod providers;
pub mod sse;
pub mod types;
pub mod utils;

pub use event_stream::{create_error_stream, AssistantMessageEventStream};
pub use model::{calculate_cost, clamp_thinking_level, get_supported_thinking_levels, models_are_equal, Model, ModelCost, ModelCostTier, ModelInput};
pub use types::*;
