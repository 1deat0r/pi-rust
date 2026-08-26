//! Typed terminal mouse decoding.
//!
//! `StdinBuffer` deliberately preserves complete escape sequences as strings
//! so existing key consumers remain compatible.  This module is the typed
//! boundary used by the Rust TUI controllers: it decodes both SGR (1006) and
//! legacy X10 mouse reports and exposes a safe component-dispatch primitive.

/// Mouse button, retaining unknown buttons instead of discarding input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other(u8),
}

/// Mouse action after decoding terminal button flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Motion,
    Drag,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

/// Normalized modifier flags.  The values match the xterm/SGR bit layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// A zero-based terminal pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub button: MouseButton,
    pub x: usize,
    pub y: usize,
    pub modifiers: MouseModifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseDecodeError {
    Incomplete,
    Malformed,
}

fn modifiers(bits: u8) -> MouseModifiers {
    MouseModifiers {
        shift: bits & 4 != 0,
        alt: bits & 8 != 0,
        ctrl: bits & 16 != 0,
    }
}

fn button(value: u8) -> MouseButton {
    match value & 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        value => MouseButton::Other(value),
    }
}

fn event_kind(code: u8, release: bool) -> (MouseEventKind, MouseButton) {
    if code & 64 != 0 {
        return (
            match code & 3 {
                0 => MouseEventKind::WheelUp,
                1 => MouseEventKind::WheelDown,
                2 => MouseEventKind::WheelLeft,
                _ => MouseEventKind::WheelRight,
            },
            button(code),
        );
    }
    let pointer = button(code);
    let kind = if release {
        MouseEventKind::Release
    } else if code & 32 != 0 {
        if code & 3 == 3 {
            MouseEventKind::Motion
        } else {
            MouseEventKind::Drag
        }
    } else {
        MouseEventKind::Press
    };
    (kind, pointer)
}

fn parse_coordinate(value: &str) -> Result<usize, MouseDecodeError> {
    let value = value
        .parse::<usize>()
        .map_err(|_| MouseDecodeError::Malformed)?;
    value.checked_sub(1).ok_or(MouseDecodeError::Malformed)
}

/// Whether a string is a complete SGR or legacy X10 mouse report.
pub fn is_mouse_sequence(raw: &str) -> bool {
    matches!(decode_mouse_event(raw), Ok(Some(_)))
}

/// Decode a complete terminal mouse report.
///
/// `Ok(None)` means the input is a non-mouse terminal sequence.  A prefix that
/// clearly starts a mouse report returns `Err(Incomplete)` so callers can keep
/// buffering it rather than accidentally treating it as a key.
pub fn decode_mouse_event(raw: &str) -> Result<Option<MouseEvent>, MouseDecodeError> {
    if let Some(body) = raw.strip_prefix("\x1b[<") {
        let Some(release) = body
            .strip_suffix('m')
            .map(|_| true)
            .or_else(|| body.strip_suffix('M').map(|_| false))
        else {
            return Err(MouseDecodeError::Incomplete);
        };
        let payload = &body[..body.len() - 1];
        let mut fields = payload.split(';');
        let code = fields
            .next()
            .ok_or(MouseDecodeError::Malformed)?
            .parse::<u16>()
            .map_err(|_| MouseDecodeError::Malformed)?;
        let x = parse_coordinate(fields.next().ok_or(MouseDecodeError::Malformed)?)?;
        let y = parse_coordinate(fields.next().ok_or(MouseDecodeError::Malformed)?)?;
        if fields.next().is_some() || code > u8::MAX as u16 {
            return Err(MouseDecodeError::Malformed);
        }
        let code = code as u8;
        let (kind, pointer) = event_kind(code, release);
        return Ok(Some(MouseEvent {
            kind,
            button: pointer,
            x,
            y,
            modifiers: modifiers(code),
        }));
    }

    if let Some(payload) = raw.strip_prefix("\x1b[M") {
        if payload.len() < 3 {
            return Err(MouseDecodeError::Incomplete);
        }
        if payload.len() != 3 {
            return Err(MouseDecodeError::Malformed);
        }
        let bytes = payload.as_bytes();
        let code = bytes[0]
            .checked_sub(32)
            .ok_or(MouseDecodeError::Malformed)?;
        let x = bytes[1]
            .checked_sub(33)
            .ok_or(MouseDecodeError::Malformed)? as usize;
        let y = bytes[2]
            .checked_sub(33)
            .ok_or(MouseDecodeError::Malformed)? as usize;
        let release = code & 3 == 3;
        let (kind, pointer) = event_kind(code, release);
        return Ok(Some(MouseEvent {
            kind,
            button: pointer,
            x,
            y,
            modifiers: modifiers(code),
        }));
    }

    if raw.starts_with("\x1b[<") || raw.starts_with("\x1b[M") {
        return Err(MouseDecodeError::Incomplete);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_sgr_press_release_drag_and_modifiers() {
        assert_eq!(
            decode_mouse_event("\x1b[<0;20;5M"),
            Ok(Some(MouseEvent {
                kind: MouseEventKind::Press,
                button: MouseButton::Left,
                x: 19,
                y: 4,
                modifiers: MouseModifiers::default()
            }))
        );
        assert_eq!(
            decode_mouse_event("\x1b[<60;3;2M"),
            Ok(Some(MouseEvent {
                kind: MouseEventKind::Drag,
                button: MouseButton::Left,
                x: 2,
                y: 1,
                modifiers: MouseModifiers {
                    shift: true,
                    alt: true,
                    ctrl: true
                }
            }))
        );
        assert_eq!(
            decode_mouse_event("\x1b[<0;20;5m").unwrap().unwrap().kind,
            MouseEventKind::Release
        );
        assert_eq!(
            decode_mouse_event("\x1b[<35;4;6M").unwrap().unwrap().kind,
            MouseEventKind::Motion
        );
    }

    #[test]
    fn decodes_wheels_and_legacy_x10() {
        assert_eq!(
            decode_mouse_event("\x1b[<64;1;1M").unwrap().unwrap().kind,
            MouseEventKind::WheelUp
        );
        assert_eq!(
            decode_mouse_event("\x1b[<65;1;1M").unwrap().unwrap().kind,
            MouseEventKind::WheelDown
        );
        assert_eq!(decode_mouse_event("\x1b[M !!").unwrap().unwrap().x, 0);
        assert_eq!(decode_mouse_event("\x1b[M !!").unwrap().unwrap().y, 0);
    }

    #[test]
    fn distinguishes_non_mouse_malformed_and_partial_input() {
        assert_eq!(decode_mouse_event("\x1b[A"), Ok(None));
        assert_eq!(
            decode_mouse_event("\x1b[<0;1"),
            Err(MouseDecodeError::Incomplete)
        );
        assert_eq!(
            decode_mouse_event("\x1b[M "),
            Err(MouseDecodeError::Incomplete)
        );
        assert_eq!(
            decode_mouse_event("\x1b[<a;1;1M"),
            Err(MouseDecodeError::Malformed)
        );
        assert!(!is_mouse_sequence("\x1b[A"));
    }
}
