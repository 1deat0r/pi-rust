use pi_client::{PiClientError, PiClientErrorKind};

#[test]
fn client_errors_expose_recovery_class_without_parsing_at_call_sites() {
    let cases = [
        ("PiClient is disposed", PiClientErrorKind::Disposed),
        ("client is disconnected", PiClientErrorKind::Disconnected),
        (
            "Session demo already has an active lease",
            PiClientErrorKind::SessionOwnership,
        ),
        (
            "Session demo is detached",
            PiClientErrorKind::SessionDetached,
        ),
        ("handshake timed out after 5ms", PiClientErrorKind::Timeout),
        ("connect: no such socket", PiClientErrorKind::Transport),
        (
            "protocol error: unexpected property",
            PiClientErrorKind::Protocol,
        ),
        ("invalid request", PiClientErrorKind::InvalidRequest),
        ("something else failed", PiClientErrorKind::Other),
    ];

    for (message, expected) in cases {
        let error = PiClientError {
            message: message.to_string(),
        };
        assert_eq!(error.kind(), expected, "{message}");
    }
}

#[test]
fn convenience_predicates_are_consistent() {
    let disconnected = PiClientError {
        message: "client is disconnected".into(),
    };
    assert!(disconnected.is_disconnected());
    assert!(!disconnected.is_disposed());

    let disposed = PiClientError {
        message: "PiClient is disposed".into(),
    };
    assert!(disposed.is_disposed());
    assert!(!disposed.is_disconnected());
}
