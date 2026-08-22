//! Minimal `Models` seam for the summarization path.
//!
//! Upstream compaction helpers take `packages/ai`'s `Models` facade which
//! owns provider dispatch and auth resolution. The Rust port does not have
//! the full `Models` implementation yet (P4 model-runtime), so the harness
//! defines the smallest object-safe surface it needs: `complete_simple`,
//! injected as a boxed async function. The port records this as a known
//! divergence — summarization callers that need auth/provider routing will
//! migrate to the real `Models` when it lands.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::model::Model;
use pi_ai::types::{AssistantMessage, Context, SimpleStreamOptions};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type CompleteSimpleFn =
    Arc<dyn Fn(&Model, &Context, &SimpleStreamOptions) -> BoxFuture<'static, AssistantMessage> + Send + Sync>;

/// `Models.completeSimple(model, context, options)` bound to an injected
/// async function.
#[derive(Clone)]
pub struct SimpleModels {
    pub complete_simple_fn: CompleteSimpleFn,
}

impl std::fmt::Debug for SimpleModels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleModels").finish_non_exhaustive()
    }
}

impl SimpleModels {
    pub fn new(
        complete_simple_fn: impl Fn(&Model, &Context, &SimpleStreamOptions) -> BoxFuture<'static, AssistantMessage>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self { complete_simple_fn: Arc::new(complete_simple_fn) }
    }

    pub fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> BoxFuture<'static, AssistantMessage> {
        (self.complete_simple_fn)(model, context, options)
    }
}
