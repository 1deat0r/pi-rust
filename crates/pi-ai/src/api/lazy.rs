//! Lazy provider API dispatch — port of `packages/ai/src/api/lazy.ts`.
//!
//! A lazy API keeps the provider module out of the startup path while still
//! exposing exactly the capabilities advertised by the caller. In
//! particular, deferred fetch/cancel methods are not synthesized unless the
//! capability flag requests them; a provider that lies about a flag receives
//! the same diagnostic as the upstream implementation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::Model;
use crate::models::{DeferredFetchOptions, ProviderStreams};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, DeferredHandle, SimpleStreamOptions,
    StreamOptions,
};

/// Async loader used by [`lazy_api`]. The loader is called once per operation;
/// callers can rely on the host runtime's module cache to deduplicate actual
/// imports.
pub type ProviderStreamsLoader = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<ProviderStreams, String>> + Send>> + Send + Sync,
>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LazyApiCapabilities {
    pub fetch_deferred: bool,
    pub cancel_deferred: bool,
}

fn setup_error_message(model: &Model, error: impl Into<String>) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_stop_reason(crate::types::StopReason::Error);
    let AssistantMessage::Assistant { error_message, .. } = &mut message;
    *error_message = Some(error.into());
    message
}

fn lazy_stream(
    model: Model,
    setup: impl Future<Output = Result<AssistantMessageEventStream, String>> + Send + 'static,
) -> AssistantMessageEventStream {
    let outer = AssistantMessageEventStream::new();
    let Some(tx) = outer.sender() else {
        return outer;
    };
    tokio::spawn(async move {
        let mut sink = crate::event_stream::StreamSinkAdapter::new(tx);
        match setup.await {
            Ok(inner) => {
                let final_message = inner.for_each(|event| sink.push(event)).await;
                sink.end(Some(final_message));
            }
            Err(error) => {
                let message = setup_error_message(&model, error);
                sink.push(AssistantMessageEvent::Error {
                    reason: crate::types::ErrorReason::Error,
                    error_message: message.clone(),
                });
                sink.end(Some(message));
            }
        }
    });
    outer
}

/// Wrap a provider stream implementation in lazy setup and capability-checked
/// deferred dispatch.
pub fn lazy_api(load: ProviderStreamsLoader, capabilities: LazyApiCapabilities) -> ProviderStreams {
    let stream_load = load.clone();
    let stream = Arc::new(
        move |model: &Model, context: &Context, options: Option<&StreamOptions>| {
            let model = model.clone();
            let context = context.clone();
            let options = options.cloned();
            let load = stream_load.clone();
            let setup_model = model.clone();
            lazy_stream(model, async move {
                let implementation = (load)().await?;
                Ok((implementation.stream)(
                    &setup_model,
                    &context,
                    options.as_ref(),
                ))
            })
        },
    );

    let simple_load = load.clone();
    let stream_simple = Arc::new(
        move |model: &Model, context: &Context, options: Option<&SimpleStreamOptions>| {
            let model = model.clone();
            let context = context.clone();
            let options = options.cloned();
            let load = simple_load.clone();
            let setup_model = model.clone();
            lazy_stream(model, async move {
                let implementation = (load)().await?;
                Ok((implementation.stream_simple)(
                    &setup_model,
                    &context,
                    options.as_ref(),
                ))
            })
        },
    );

    let fetch_deferred = capabilities.fetch_deferred.then(|| {
        let fetch_load = load.clone();
        Arc::new(
            move |model: &Model, handle: &DeferredHandle, options: &DeferredFetchOptions| {
                let model = model.clone();
                let handle = handle.clone();
                let options = options.clone();
                let load = fetch_load.clone();
                let setup_model = model.clone();
                lazy_stream(model, async move {
                    let implementation = (load)().await?;
                    let fetch = implementation
                        .fetch_deferred
                        .ok_or_else(|| "API does not support deferred responses".to_string())?;
                    Ok(fetch(&setup_model, &handle, &options))
                })
            },
        ) as crate::models::DeferredStreamFn
    });

    let cancel_deferred = capabilities.cancel_deferred.then(|| {
        let cancel_load = load;
        Arc::new(
            move |model: &Model,
                  handle: &DeferredHandle,
                  options: &crate::models::DeferredCancelOptions| {
                let model = model.clone();
                let handle = handle.clone();
                let options = options.clone();
                let load = cancel_load.clone();
                Box::pin(async move {
                    let implementation = (load)().await?;
                    let cancel = implementation
                        .cancel_deferred
                        .ok_or_else(|| "API cannot cancel deferred responses".to_string())?;
                    cancel(&model, &handle, &options).await
                }) as crate::models::DeferredCancelFuture
            },
        ) as crate::models::DeferredCancelFn
    });

    ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred,
        cancel_deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DeferredCancelOptions, ProviderStreams};
    use crate::types::{
        AssistantMessage, AssistantMessageEvent, Context, ErrorReason, SimpleStreamOptions,
        StreamOptions,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn model() -> Model {
        Model::new("model", "Model", "test-api", "test-provider")
    }

    fn stopped_stream(model: &Model) -> AssistantMessageEventStream {
        let mut stream = AssistantMessageEventStream::new();
        let mut message = AssistantMessage::new();
        message.set_api_provider_model(&model.api, &model.provider, &model.id);
        message.set_stop_reason(crate::types::StopReason::Stop);
        stream.push(AssistantMessageEvent::Done {
            reason: crate::types::DoneReason::Stop,
            message,
        });
        stream
    }

    fn implementation() -> ProviderStreams {
        let stream: crate::models::StreamFn = Arc::new(
            |model: &Model, _context: &Context, _options: Option<&StreamOptions>| {
                stopped_stream(model)
            },
        );
        let stream_simple: crate::models::SimpleStreamFn = Arc::new(
            |model: &Model, _context: &Context, _options: Option<&SimpleStreamOptions>| {
                stopped_stream(model)
            },
        );
        let fetch_deferred: crate::models::DeferredStreamFn = Arc::new(
            |model: &Model, _handle: &DeferredHandle, _options: &DeferredFetchOptions| {
                stopped_stream(model)
            },
        );
        let cancel_deferred: crate::models::DeferredCancelFn = Arc::new(
            |_model: &Model, _handle: &DeferredHandle, _options: &DeferredCancelOptions| {
                Box::pin(async { Ok(()) }) as crate::models::DeferredCancelFuture
            },
        );
        ProviderStreams {
            stream,
            stream_simple,
            fetch_deferred: Some(fetch_deferred),
            cancel_deferred: Some(cancel_deferred),
        }
    }

    #[tokio::test]
    async fn lazy_api_loads_on_first_call_and_only_exposes_declared_capabilities() {
        let loads = Arc::new(AtomicUsize::new(0));
        let load_count = loads.clone();
        let api = lazy_api(
            Arc::new(move || {
                load_count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(implementation()) })
            }),
            LazyApiCapabilities {
                fetch_deferred: true,
                cancel_deferred: false,
            },
        );
        assert!(api.fetch_deferred.is_some());
        assert!(api.cancel_deferred.is_none());
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        let handle = DeferredHandle {
            provider: "test-provider".to_string(),
            model_id: "model".to_string(),
            api: "test-api".to_string(),
            id: "response".to_string(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        };
        let message = api.fetch_deferred.as_ref().unwrap()(&model(), &handle, &Default::default())
            .for_each(|_| {})
            .await;
        assert_eq!(message.stop_reason(), Some(crate::types::StopReason::Stop));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lazy_api_reports_missing_implementation_capabilities() {
        let api = lazy_api(
            Arc::new(|| {
                Box::pin(async {
                    let mut implementation = implementation();
                    implementation.fetch_deferred = None;
                    implementation.cancel_deferred = None;
                    Ok(implementation)
                })
            }),
            LazyApiCapabilities {
                fetch_deferred: true,
                cancel_deferred: true,
            },
        );
        let handle = DeferredHandle {
            provider: "test-provider".to_string(),
            model_id: "model".to_string(),
            api: "test-api".to_string(),
            id: "response".to_string(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        };
        let message = api.fetch_deferred.as_ref().unwrap()(&model(), &handle, &Default::default())
            .for_each(|_| {})
            .await;
        assert_eq!(
            message.error_message(),
            Some("API does not support deferred responses")
        );
        let error = api.cancel_deferred.as_ref().unwrap()(&model(), &handle, &Default::default())
            .await
            .unwrap_err();
        assert_eq!(error, "API cannot cancel deferred responses");
        let _ = ErrorReason::Error;
    }
}
