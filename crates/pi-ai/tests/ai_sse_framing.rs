#![allow(clippy::expect_used)]

use pi_ai::sse::{SseEvent, SseParser};

fn expected_events() -> Vec<SseEvent> {
    vec![
        SseEvent {
            data: "{\"text\":\"héllo 世界\"}\nsecond line".to_string(),
            event: Some("message".to_string()),
            id: Some("evt-1".to_string()),
        },
        SseEvent {
            data: "[DONE]".to_string(),
            event: None,
            id: None,
        },
    ]
}

#[test]
fn every_two_chunk_boundary_matches_whole_payload_parsing() {
    let payload = concat!(
        ": keepalive\r\n",
        "retry: 250\r\n",
        "event: message\r\n",
        "id: evt-1\r\n",
        "data: {\"text\":\"héllo 世界\"}\r\n",
        "data: second line\r\n",
        "\r\n",
        "data: [DONE]\n\n"
    );
    assert_eq!(SseParser::parse_text(payload), expected_events());

    for split in 0..=payload.len() {
        if !payload.is_char_boundary(split) {
            continue;
        }
        let mut parser = SseParser::new();
        let mut events = parser.push_bytes(&payload.as_bytes()[..split]);
        events.extend(parser.push_bytes(&payload.as_bytes()[split..]));
        events.extend(parser.finish());
        assert_eq!(events, expected_events(), "split at byte {split}");
    }

    let mut bytewise = SseParser::new();
    let mut events = Vec::new();
    for byte in payload.as_bytes() {
        events.extend(bytewise.push_bytes(&[*byte]));
    }
    events.extend(bytewise.finish());
    assert_eq!(events, expected_events());
}

#[test]
fn malformed_json_and_unknown_fields_remain_raw_provider_data() {
    let events = SseParser::parse_text(concat!(
        "unknown: ignored\n",
        "data: {\"partial\":\n",
        "data: [1,2\n",
        "\n"
    ));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "{\"partial\":\n[1,2");
}

#[test]
fn data_less_named_event_dispatches_and_bare_data_is_empty() {
    let events = SseParser::parse_text("event: ping\nid: heartbeat\n\ndata\n\n");
    assert_eq!(
        events,
        vec![
            SseEvent {
                data: String::new(),
                event: Some("ping".to_string()),
                id: Some("heartbeat".to_string()),
            },
            SseEvent {
                data: String::new(),
                event: None,
                id: None,
            },
        ]
    );
}

#[test]
fn eof_after_split_cr_delivers_pending_event_exactly_once() {
    let mut parser = SseParser::new();
    assert!(parser.push_bytes(b"data: tail\r").is_empty());
    assert_eq!(
        parser.finish(),
        vec![SseEvent {
            data: "tail".to_string(),
            event: None,
            id: None,
        }]
    );
}
