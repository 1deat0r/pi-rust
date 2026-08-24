//! Length-prefixed framing for protocol payloads — port of
//! `packages/protocol/src/framing.ts`.

use crate::error::FrameError;

pub const FRAME_HEADER_LENGTH: usize = 4;
pub const PAYLOAD_BLOCK_SIZE: usize = 64 * 1024;

/// Default upper bound for one framed CBOR payload.
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct FrameDecoderOptions {
    pub max_frame_length: Option<usize>,
}

fn resolve_max_frame_length(options: &FrameDecoderOptions) -> Result<usize, FrameError> {
    let value = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    if value > u32::MAX as usize {
        return Err(FrameError::new(format!(
            "maxFrameLength must be an integer between 0 and {}",
            u32::MAX
        )));
    }
    Ok(value)
}

/// Prefixes a payload with its unsigned 32-bit big-endian byte length.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > u32::MAX as usize {
        return Err(FrameError::new(
            "Frame payload exceeds the unsigned 32-bit length limit".to_string(),
        ));
    }
    let length = payload.len() as u32;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.push((length >> 24) as u8);
    frame.push((length >> 16) as u8);
    frame.push((length >> 8) as u8);
    frame.push(length as u8);
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Validates that bytes contain exactly one complete frame within the configured limit.
pub fn assert_complete_frame(
    frame: &[u8],
    options: &FrameDecoderOptions,
) -> Result<(), FrameError> {
    if frame.len() < FRAME_HEADER_LENGTH {
        return Err(FrameError::new(
            "Frame does not contain a complete length prefix".to_string(),
        ));
    }
    let length = (frame[0] as u32) * 0x1_000_000
        + (frame[1] as u32) * 0x1_0000
        + (frame[2] as u32) * 0x100
        + frame[3] as u32;
    let max_frame_length = resolve_max_frame_length(options)?;
    if length as usize > max_frame_length {
        return Err(FrameError::new(format!(
            "Frame length {length} exceeds configured limit of {max_frame_length}"
        )));
    }
    if frame.len() != FRAME_HEADER_LENGTH + length as usize {
        return Err(FrameError::new(
            "Frame must contain exactly one complete payload".to_string(),
        ));
    }
    Ok(())
}

enum DecoderState {
    Open,
    Ended,
    Failed,
}

/// Incrementally splits arbitrary byte chunks into length-prefixed payloads.
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_length: usize,
    max_frame_length: usize,
    state: DecoderState,
    payload: Vec<u8>,
    /// Pending payload length when the header has been consumed, None otherwise.
    pending_payload_length: Option<usize>,
}

impl FrameDecoder {
    pub fn new(options: &FrameDecoderOptions) -> Result<Self, FrameError> {
        Ok(Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_length: 0,
            max_frame_length: resolve_max_frame_length(options)?,
            state: DecoderState::Open,
            payload: Vec::new(),
            pending_payload_length: None,
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        match self.state {
            DecoderState::Failed => {
                return Err(FrameError::new("Frame decoder has failed".to_string()));
            }
            DecoderState::Ended => {
                return Err(FrameError::new("Frame decoder has ended".to_string()));
            }
            DecoderState::Open => {}
        }
        let mut frames = Vec::new();
        let mut offset = 0usize;
        while offset < chunk.len() {
            if self.pending_payload_length.is_none() {
                let header_bytes =
                    (FRAME_HEADER_LENGTH - self.header_length).min(chunk.len() - offset);
                self.header[self.header_length..self.header_length + header_bytes]
                    .copy_from_slice(&chunk[offset..offset + header_bytes]);
                self.header_length += header_bytes;
                offset += header_bytes;
                if self.header_length < FRAME_HEADER_LENGTH {
                    break;
                }
                let length = (self.header[0] as usize) * 0x1_000_000
                    + (self.header[1] as usize) * 0x1_0000
                    + (self.header[2] as usize) * 0x100
                    + self.header[3] as usize;
                self.header_length = 0;
                if length > self.max_frame_length {
                    self.state = DecoderState::Failed;
                    return Err(FrameError::new(format!(
                        "Frame length {length} exceeds configured limit of {}",
                        self.max_frame_length
                    )));
                }
                if length == 0 {
                    frames.push(Vec::new());
                    continue;
                }
                self.pending_payload_length = Some(length);
                self.payload = Vec::with_capacity(length.min(PAYLOAD_BLOCK_SIZE));
            }

            let expected = self.pending_payload_length.expect("payload length set");
            while offset < chunk.len() && self.payload.len() < expected {
                let payload_bytes = (expected - self.payload.len()).min(chunk.len() - offset);
                self.payload
                    .extend_from_slice(&chunk[offset..offset + payload_bytes]);
                offset += payload_bytes;
            }
            if self.payload.len() == expected {
                let frame = std::mem::take(&mut self.payload);
                self.pending_payload_length = None;
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    /// Called after the final chunk. Errors if a partial frame is pending.
    pub fn end(&mut self) -> Result<(), FrameError> {
        match self.state {
            DecoderState::Failed => {
                return Err(FrameError::new("Frame decoder has failed".to_string()))
            }
            DecoderState::Ended => return Ok(()),
            DecoderState::Open => {}
        }
        if self.header_length != 0
            || self.pending_payload_length.is_some()
            || !self.payload.is_empty()
        {
            self.state = DecoderState::Failed;
            return Err(FrameError::new(
                "Frame decoder ended with a partial frame".to_string(),
            ));
        }
        self.state = DecoderState::Ended;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_and_assert_round_trip() {
        let payload = vec![1u8, 2, 3, 4];
        let frame = encode_frame(&payload).unwrap();
        assert_eq!(&frame[..4], &[0, 0, 0, 4]);
        assert_eq!(&frame[4..], &payload[..]);
        assert!(assert_complete_frame(&frame, &FrameDecoderOptions::default()).is_ok());
    }

    #[test]
    fn assert_rejects_truncated_and_oversized() {
        let payload = vec![7u8; 4];
        let frame = encode_frame(&payload).unwrap();
        assert!(assert_complete_frame(&frame[..5], &FrameDecoderOptions::default()).is_err());
        // Frame claims 100 bytes but only carries 2.
        let bad = [0, 0, 0, 100, 1, 2];
        assert!(assert_complete_frame(&bad, &FrameDecoderOptions::default()).is_err());
        // Frame length exceeds configured max.
        let opts = FrameDecoderOptions {
            max_frame_length: Some(2),
        };
        assert!(assert_complete_frame(&frame, &opts).is_err());
    }

    #[test]
    fn incremental_decode_splits_chunks() {
        let payloads: Vec<Vec<u8>> = ["aa", "bb", "cc"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let frames: Vec<Vec<u8>> = payloads.iter().map(|p| encode_frame(p).unwrap()).collect();
        let mut all = Vec::new();
        for f in &frames {
            all.extend_from_slice(f);
        }
        let mut decoder = FrameDecoder::new(&FrameDecoderOptions::default()).unwrap();
        // Feed one byte at a time across two frames, then the whole third.
        let mut decoded: Vec<Vec<u8>> = Vec::new();
        for &b in &all[..frames[0].len() + frames[1].len()] {
            decoded.extend(decoder.push(&[b]).unwrap());
        }
        decoded.extend(decoder.push(&frames[2]).unwrap());
        assert_eq!(decoded, payloads);
        decoder.end().unwrap();
    }

    #[test]
    fn rejects_oversized_frame_length() {
        let mut decoder = FrameDecoder::new(&FrameDecoderOptions {
            max_frame_length: Some(8),
        })
        .unwrap();
        let err = decoder.push(&[0, 0, 0, 9]).unwrap_err();
        assert!(err.0.contains("exceeds configured limit"));
    }

    #[test]
    fn end_fails_on_partial_frame() {
        let frame = encode_frame(b"hello world").unwrap();
        let mut decoder = FrameDecoder::new(&FrameDecoderOptions::default()).unwrap();
        decoder.push(&frame[..6]).unwrap();
        let err = decoder.end().unwrap_err();
        assert!(err.0.contains("end"));
    }
}
