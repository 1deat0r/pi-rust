//! Native modifier key state — port of `packages/tui/src/native-modifiers.ts`.
//!
//! Upstream loads a native helper to detect whether the physical Shift /
//! Command / Control / Option key is currently pressed (used on macOS and
//! Windows). This port has no native helper on supported Unix terminals, so
//! `is_native_modifier_pressed` always returns `false` (matching upstream's
//! behavior when the native module cannot be loaded).

pub type ModifierKey = &'static str;

pub const MODIFIER_SHIFT: &str = "shift";
pub const MODIFIER_COMMAND: &str = "command";
pub const MODIFIER_CONTROL: &str = "control";
pub const MODIFIER_OPTION: &str = "option";

/// Whether the given physical modifier key is currently pressed.
/// Always `false` in this port (no native helper).
pub fn is_native_modifier_pressed(_key: ModifierKey) -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn native_modifiers_are_not_pressed() {
        assert!(!is_native_modifier_pressed(MODIFIER_SHIFT));
        assert!(!is_native_modifier_pressed(MODIFIER_COMMAND));
        assert!(!is_native_modifier_pressed(MODIFIER_CONTROL));
        assert!(!is_native_modifier_pressed(MODIFIER_OPTION));
    }
}
