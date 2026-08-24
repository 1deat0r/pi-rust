//! End-to-end protocol-link test: PiServer over a Unix socket, driven by
//! PiClient (hello handshake → list/create/prompt → snapshot events).

use std::sync::{Arc, Mutex};

use pi_protocol::{Command, CommandResult};
use pi_server::server::PiServer;
use pi_server::service::InMemoryService;
use pi_server::UnixListener;

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

#[tokio::test]
async fn client_server_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pi-server-e2e-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let socket_path = dir.join("pi.sock").to_string_lossy().into_owned();

    let service = Box::new(InMemoryService::new(test_models()));
    let mut server = PiServer::new(
        service,
        pi_server::types::PiServerOptions {
            listeners: vec![Box::new(UnixListener::new(socket_path.clone()).unwrap())],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("e2e-server".into()),
            on_error: None,
        },
    )
    .unwrap();
    server.start().await.unwrap();

    // Client connects + handshakes.
    let client = pi_client::PiClient::connect(&socket_path).await.unwrap();
    let snapshot = client.snapshot().unwrap();
    assert_eq!(snapshot.server_id, "e2e-server");
    assert_eq!(snapshot.sessions.len(), 0);
    assert_eq!(snapshot.models.len(), 1);

    // List sessions.
    let result = client.request(Command::List).await.unwrap();
    if let CommandResult::List { sessions } = result {
        assert!(sessions.is_empty());
    } else {
        panic!("expected List result");
    }

    // Create a session.
    let result = client
        .request(Command::Create {
            cwd: Some(dir.to_string_lossy().into_owned()),
            name: Some("e2e session".into()),
            model: None,
            thinking_level: None,
        })
        .await
        .unwrap();
    let session_id = match &result {
        CommandResult::Create { session } => session.id.clone(),
        _ => panic!("expected Create result"),
    };

    // Prompt the session; the snapshot must carry the user transcript item.
    let result = client
        .request(Command::Prompt {
            session_id: session_id.clone(),
            text: "hello server".into(),
        })
        .await
        .unwrap();
    match result {
        CommandResult::Prompt { session } => {
            assert_eq!(session.id, session_id);
            assert_eq!(session.transcript.len(), 1);
            match &session.transcript[0] {
                pi_protocol::TranscriptItem::User(u) => {
                    assert_eq!(u.role, "user");
                }
                _ => panic!("expected user transcript item"),
            }
        }
        _ => panic!("expected Prompt result"),
    }

    // List now shows the session.
    let result = client.request(Command::List).await.unwrap();
    if let CommandResult::List { sessions } = result {
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);
    } else {
        panic!("expected List result");
    }

    // Set model + thinking on the session.
    let result = client
        .request(Command::SetModel {
            session_id: session_id.clone(),
            model: pi_protocol::ModelRef {
                provider: "faux".into(),
                id: "faux-1".into(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(result, CommandResult::SetModel { .. }));

    let result = client
        .request(Command::SetThinking {
            session_id: session_id.clone(),
            thinking_level: pi_protocol::ThinkingLevel::Off,
        })
        .await
        .unwrap();
    assert!(matches!(result, CommandResult::SetThinking { .. }));

    // Abort is accepted.
    let result = client.request(Command::Abort { session_id }).await.unwrap();
    assert!(matches!(result, CommandResult::Abort { .. }));

    // Snapshot events: subscribe before a detach, then observe.
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let events = events.clone();
        client.subscribe(Arc::new(move |event| {
            if let pi_protocol::ServerEvent::ServerSnapshot { .. } = event {
                events.lock().unwrap().push("snapshot".to_string());
            }
        }));
    }
    let _ = client.request(Command::List).await.unwrap(); // triggers broadcast after response
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !events.lock().unwrap().is_empty(),
        "expected snapshot broadcast events"
    );

    let _ = client.close().await;
    drop(client);
    let _ = server.close().await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn bad_protocol_version_gets_hello_error() {
    let dir = std::env::temp_dir().join(format!("pi-server-ver-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let socket_path = dir.join("pi.sock").to_string_lossy().into_owned();

    let service = Box::new(InMemoryService::new(test_models()));
    let mut server = PiServer::new(
        service,
        pi_server::types::PiServerOptions {
            listeners: vec![Box::new(UnixListener::new(socket_path.clone()).unwrap())],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("e2e-server".into()),
            on_error: None,
        },
    )
    .unwrap();
    server.start().await.unwrap();

    // Send a bad-version hello directly and expect hello_error.
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bad_hello = pi_protocol::ClientMessage::Hello { version: 999 };
    // encode_client_message returns a complete length-prefixed frame.
    let frame = pi_protocol::encode_client_message(&bad_hello, &Default::default()).unwrap();
    write.write_all(&frame).await.unwrap();
    write.flush().await.unwrap();

    // Read the response frame: must be a hello_error.
    let mut header = [0u8; 4];
    let n = read.read_exact(&mut header).await.unwrap_or(0);
    assert_eq!(n, 4, "header bytes: {:02x?}", header);
    let len = u32::from_be_bytes(header) as usize;
    assert!(len <= 16 * 1024 * 1024, "unreasonable frame length {len}");
    let mut body = vec![0u8; len];
    read.read_exact(&mut body).await.unwrap();
    let mut frame = header.to_vec();
    frame.extend_from_slice(&body);
    let messages = pi_protocol::ServerMessageDecoder::new(&Default::default())
        .unwrap()
        .push(&frame)
        .unwrap();
    assert!(matches!(
        messages[0],
        pi_protocol::ServerMessage::HelloError { .. }
    ));

    let _ = read;
    let _ = writer_shutdown(write).await;
    let _ = server.close().await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

async fn writer_shutdown(mut w: tokio::net::unix::OwnedWriteHalf) {
    use tokio::io::AsyncWriteExt;
    let _ = w.shutdown().await;
}
