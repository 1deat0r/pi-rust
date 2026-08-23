// P2-D regression: a producer panic (integer overflow in the faux token-chunk
// LCG used to panic under debug overflow-checks inside the spawned task) used
// to hang consumers forever because collect() holds its own live sender.
// Fixed by wrapping the producer in catch_unwind -> terminal Error. Long text
// forces many split calls per stream; the stream must terminate in bounded
// time with a terminal event, and a forced panic path must surface as Error.

use pi_ai::providers::faux::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pi_ai::types::{AssistantMessageEvent, ContentBlock, Context};
use std::time::Duration;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn long_text_stream_terminates_in_bounded_time() {
    let rt = rt();
    rt.block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        // ~30 token chunks -> well past the old overflow threshold (seed 3).
        let long = "x".repeat(400);
        core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text(&long)],
            FauxAssistantOptions::default(),
        ))]);
        let model = core.models.first().unwrap().clone();
        let context = Context::default();
        let stream = core.stream(&model, &context, None);
        let res = tokio::time::timeout(Duration::from_secs(5), async {
            let (events, msg) = stream.collect().await;
            (events, msg)
        })
        .await;
        let (events, msg) = res
            .expect("stream must terminate; a producer panic must emit a terminal event, not hang");
        assert!(events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Done { .. })));
        assert_eq!(msg.stop_reason(), Some(pi_ai::types::StopReason::Stop));
        // The content must arrive complete: this is the corruption check too.
        let text: String = msg
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, long);
    });
}

#[test]
fn producer_panic_surfaces_as_terminal_error_not_hang() {
    let rt = rt();
    rt.block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        core.set_responses(vec![FauxResponseStep::Factory(Box::new(|_, _, _, _| {
            panic!("synthetic producer panic")
        }))]);
        let model = core.models.first().unwrap().clone();
        let context = Context::default();
        let stream = core.stream(&model, &context, None);
        let res = tokio::time::timeout(Duration::from_secs(5), async {
            let (events, msg) = stream.collect().await;
            (events, msg)
        })
        .await;
        let (events, msg) = res.expect("panic must produce a terminal Error event, never a hang");
        assert!(events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Error { .. })));
        assert_eq!(msg.stop_reason(), Some(pi_ai::types::StopReason::Error));
        let err = msg.error_message().map(str::to_owned).unwrap_or_default();
        assert!(
            err.contains("synthetic producer panic"),
            "unexpected error message: {err}"
        );
    });
}
