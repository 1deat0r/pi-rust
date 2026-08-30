#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

#[test]
fn client_codec_roundtrip() {
    let hello = pi_protocol::ClientMessage::Hello {
        version: pi_protocol::PROTOCOL_VERSION,
    };
    // encode_client_message returns a complete length-prefixed frame.
    let frame = pi_protocol::encode_client_message(&hello, &Default::default()).unwrap();
    let mut decoder = pi_protocol::ClientMessageDecoder::new(&Default::default()).unwrap();
    let msgs = decoder.push(&frame).unwrap();
    assert_eq!(msgs.len(), 1, "frame={}", frame.len());
    assert!(matches!(msgs[0], pi_protocol::ClientMessage::Hello { .. }));
}
