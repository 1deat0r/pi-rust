#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! End-to-end test for the SessionHandle surface (pi-client): create/attach a
//! session on a live PiServer over a Unix socket, drive prompt/steer/abort/
//! set_model/set_thinking through the handle, verify snapshot subscription
//! fanout and detach/dispose semantics.
//!
//! Upstream oracle: upstream_pi/packages/server/test/sessions.test.ts and
//! upstream_pi/packages/server/src/testing/service.ts.

use std::sync::{Arc, Mutex};

use pi_client::session_handle::{AcquireSessionOptions, SessionLeaseMode};
use pi_protocol::{ModelRef, ThinkingLevel, UserContent};

fn test_models() -> Vec<pi_protocol::ModelMetadata> {
    vec![pi_protocol::ModelMetadata {
        provider: "faux".into(),
        id: "faux-1".into(),
        name: "Faux Model".into(),
        api: "faux".into(),
        reasoning: false,
        input: vec![
            pi_protocol::ModelInput::Text,
            pi_protocol::ModelInput::Image,
        ],
        context_window: 128_000,
        max_tokens: 16_384,
        cost: pi_protocol::ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        supported_thinking_levels: vec![pi_protocol::ThinkingLevel::Off],
        authenticated: false,
    }]
}

async fn spawn_server(dir_name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("{dir_name}-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let socket_path = dir.join("pi.sock").to_string_lossy().into_owned();
    let service = Box::new(pi_server::service::InMemoryService::new(test_models()));
    let mut server = pi_server::server::PiServer::new(
        service,
        pi_server::types::PiServerOptions {
            listeners: vec![Box::new(
                pi_server::UnixListener::new(socket_path.clone()).unwrap(),
            )],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("e2e-server".into()),
            on_error: None,
        },
    )
    .unwrap();
    server.start().await.unwrap();
    socket_path
}

#[tokio::test]
async fn session_handle_start_prompt_and_commands() {
    let socket_path = spawn_server("pi-client-handle").await;
    let client = Arc::new(pi_client::PiClient::connect(&socket_path).await.unwrap());
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();

    // start_session creates an attached handle.
    let handle = client
        .start_session(
            Some(cwd.clone()),
            Some("handle e2e".into()),
            Some(ModelRef {
                provider: "faux".into(),
                id: "faux-1".into(),
            }),
            None,
            AcquireSessionOptions {
                mode: SessionLeaseMode::Shared,
            },
        )
        .await
        .unwrap();
    assert!(handle.attached());
    assert!(handle.snapshot().is_some());

    // Prompt via the handle.
    let snap = handle.prompt("hello from handle").await.unwrap();
    assert_eq!(snap.id, handle.id());
    assert!(snap.transcript.iter().any(|t| matches!(
        t,
        pi_protocol::TranscriptItem::User(u)
            if u.content.iter().any(|c| matches!(c, UserContent::Text(pi_protocol::TextContent::Text { text }) if text == "hello from handle"))
    )));

    // Steering queues a steer item (upstream semantics: queued, not appended
    // to the transcript until applied by the runtime loop).
    let snap = handle.steer("steer me").await.unwrap();
    assert_eq!(snap.queued_steer_count, 1);

    // set_model / set_thinking return updated snapshots.
    let snap = handle
        .set_model(ModelRef {
            provider: "faux".into(),
            id: "faux-1".into(),
        })
        .await
        .unwrap();
    assert_eq!(snap.model.id, "faux-1");
    let snap = handle.set_thinking(ThinkingLevel::Off).await.unwrap();
    assert_eq!(snap.thinking_level, ThinkingLevel::Off);
    // Phase is preserved (the runtime owns phase transitions, not set_*).
    assert_eq!(snap.phase, pi_protocol::SessionPhase::Turn);

    // abort is accepted on an idle session.
    let _ = handle.abort().await.unwrap();

    // Detach flips the handle to inactive.
    handle.detach().await.unwrap();
    assert!(!handle.attached());

    // Dispose releases listeners.
    handle.dispose().await.unwrap();
    assert!(!handle.active());

    client.close().await.unwrap();
}

#[tokio::test]
async fn session_handle_subscribe_gets_snapshot_events() {
    let socket_path = spawn_server("pi-client-handle-sub").await;
    let client = Arc::new(pi_client::PiClient::connect(&socket_path).await.unwrap());
    let handle = client
        .start_session(
            Some(std::env::temp_dir().to_string_lossy().into_owned()),
            None,
            None,
            None,
            AcquireSessionOptions::default(),
        )
        .await
        .unwrap();

    // Subscribe before prompting; the prompt result snapshot also arrives as
    // a ServerEvent::SessionSnapshot and must fan out to the handle.
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r2 = received.clone();
    let _unsub = handle.subscribe(move |snap| {
        r2.lock().unwrap().push(snap.id.clone());
    });

    // Give the reader task time to deliver the attach snapshot, then prompt
    // and poll for the fanned-out event.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _ = handle.prompt("sub test").await.unwrap();
    for _ in 0..20 {
        if !received.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        received.lock().unwrap().first().cloned(),
        Some(handle.id().to_string())
    );

    client.close().await.unwrap();
}
