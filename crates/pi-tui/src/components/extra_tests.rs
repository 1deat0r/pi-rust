#[cfg(test)]
mod components_extra_tests {
    use crate::components::alt_screen::{find_alt_screen_search_matches, search_match_key};
    use crate::components::image::{Image, ImageOptions, ImageTheme};
    use crate::components::settings_list::{
        plain_settings_theme, SettingItem, SettingsList, SettingsListOptions,
    };
    use crate::keys::TuiKey;
    use crate::terminal_image::{
        get_gif_dimensions, get_png_dimensions, set_capabilities, ImageProtocol,
        TerminalCapabilities,
    };
    use crate::tui::Component;

    /// Serializes tests that mutate the global terminal-image capabilities so
    /// parallel executions cannot race on the shared state.
    fn cap_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    #[test]
    fn alt_screen_search_finds_matches_and_merges_segments() {
        let lines = vec![
            "hello world".to_string(),
            "the quick brown fox".to_string(),
            "jumps over hello again".to_string(),
        ];
        let matches = find_alt_screen_search_matches(&lines, "hello");
        assert_eq!(matches.len(), 2, "should find two \"hello\" matches");
        // All segments map to real rows.
        let rows: Vec<usize> = matches
            .iter()
            .flat_map(|m| m.segments.iter().map(|s| s.row))
            .collect();
        assert!(rows.contains(&0));
        assert!(rows.contains(&2));
        // Match keys are unique per occurrence.
        let key0 = search_match_key(&matches[0]);
        let key1 = search_match_key(&matches[1]);
        assert_ne!(key0, key1);
    }

    #[test]
    fn alt_screen_search_normalizes_whitespace() {
        let lines = vec!["foo    bar baz".to_string()];
        let matches = find_alt_screen_search_matches(&lines, "foo  bar");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn image_fallback_without_capabilities() {
        let _guard = cap_lock();
        set_capabilities(TerminalCapabilities {
            images: None,
            true_color: false,
            hyperlinks: false,
        });
        let theme = ImageTheme {
            fallback_color: Box::new(|s| s.to_string()),
        };
        let image = Image::new(
            "not-real-base64",
            "image/png",
            theme,
            ImageOptions {
                filename: Some("/tmp/pic.png".to_string()),
                ..Default::default()
            },
        );
        let lines = image.render(60);
        assert!(lines[0].contains("[Image:"));
        assert!(lines[0].contains("/tmp/pic.png"));
    }

    #[test]
    fn png_dimensions_parse() {
        // Minimal 1x1 PNG (base64).
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let dims = get_png_dimensions(png);
        assert!(dims.is_some());
        let (w, h) = dims.unwrap();
        assert_eq!(w, 1);
        assert_eq!(h, 1);
    }

    #[test]
    fn gif_dimensions_parse() {
        // GIF89a 2x3 placeholder.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GIF89a");
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        let b64 = base64_encode(&bytes);
        let dims = get_gif_dimensions(&b64);
        assert_eq!(dims, Some((2, 3)));
    }

    #[test]
    fn kitty_capability_renders_image_protocol() {
        let _guard = cap_lock();
        set_capabilities(TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        });
        let theme = ImageTheme {
            fallback_color: Box::new(|s| s.to_string()),
        };
        let image = Image::new(
            "SGVsbG8=",
            "image/png",
            theme,
            ImageOptions {
                max_width_cells: Some(40),
                ..Default::default()
            },
        );
        let lines = image.render(80);
        assert!(lines
            .first()
            .map(|l| l.starts_with("\x1b_G"))
            .unwrap_or(false));
    }

    #[test]
    fn settings_list_cycles_values() {
        let items = vec![SettingItem::new(
            "theme",
            "Theme",
            "dark",
            vec!["dark".to_string(), "light".to_string()],
        )];
        let mut list = SettingsList::new(
            items,
            10,
            plain_settings_theme(),
            SettingsListOptions::default(),
        );
        list.handle_input(&TuiKey::simple("enter"));
        assert_eq!(list.selected_id().as_deref(), Some("theme"));
        // After cycling, the item value should be "light".
        if let Some(item) = list.visible_items().first() {
            assert_eq!(item.current_value, "light");
        }
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let mut val = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            for i in 0..4 {
                if i * 6 + 6 > 24 {
                    out.push('=');
                    continue;
                }
                let idx = ((val >> 18) & 0x3f) as usize;
                out.push(TABLE[idx] as char);
                val <<= 6;
            }
        }
        out
    }
}
