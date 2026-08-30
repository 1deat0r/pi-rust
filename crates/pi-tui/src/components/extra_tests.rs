#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test module
#[cfg(test)]
mod components_extra_tests {
    use crate::components::alt_screen::{
        find_alt_screen_search_matches, search_match_key, SearchMatch, SearchSegment,
    };
    use crate::components::image::{Image, ImageOptions, ImageTheme};
    use crate::components::settings_list::{
        plain_settings_theme, SettingItem, SettingsList, SettingsListOptions,
    };
    use crate::keys::TuiKey;
    use crate::terminal_image::{
        get_capabilities, get_cell_dimensions, get_gif_dimensions, get_png_dimensions,
        set_capabilities, set_cell_dimensions, ImageProtocol, TerminalCapabilities,
    };
    use crate::tui::Component;

    /// Serializes tests that mutate the global terminal-image capabilities so
    /// parallel executions cannot race on the shared state.
    fn cap_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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
    fn alt_screen_search_matches_case_insensitively_across_rows() {
        let lines = vec!["alpha QUICK".to_string(), "brown fox".to_string()];
        let matches = find_alt_screen_search_matches(&lines, "quick brown");

        assert_eq!(
            matches,
            vec![SearchMatch {
                segments: vec![
                    SearchSegment {
                        row: 0,
                        start_col: 6,
                        end_col: 11,
                    },
                    SearchSegment {
                        row: 1,
                        start_col: 0,
                        end_col: 5,
                    },
                ],
            }]
        );
    }

    #[test]
    fn alt_screen_search_uses_terminal_cell_columns_for_wide_graphemes() {
        let lines = vec!["a界b".to_string()];
        let matches = find_alt_screen_search_matches(&lines, "界b");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].segments,
            vec![crate::components::alt_screen::SearchSegment {
                row: 0,
                start_col: 1,
                end_col: 4,
            }]
        );
    }

    #[test]
    fn alt_screen_search_forwards_query_changes_and_focus_to_input() {
        use std::sync::{Arc, Mutex};

        let queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let queries_for_callback = queries.clone();
        let mut search = crate::components::alt_screen::AltScreenSearchComponent::new()
            .with_query_callback(move |query| {
                queries_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(query.to_string());
            });
        search.set_focused(true);
        assert!(search.is_focused());
        search.handle_input(&TuiKey::simple("x"));
        assert_eq!(
            queries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            ["x".to_string()]
        );
        assert!(search
            .render(40)
            .iter()
            .any(|line| line.contains(crate::tui::CURSOR_MARKER)));
    }

    #[test]
    fn alt_screen_search_uses_the_upstream_input_prompt() {
        let mut search = crate::components::alt_screen::AltScreenSearchComponent::new();
        search.set_query("needle");

        let lines = search.render(24);
        assert_eq!(lines.len(), 2);
        assert!(crate::utils::strip_ansi_codes(&lines[1]).starts_with("> needle"));
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
    fn image_recomputes_cached_rows_after_cell_size_changes() {
        let _guard = cap_lock();
        let original_capabilities = get_capabilities();
        let original_cell_dimensions = get_cell_dimensions();
        set_capabilities(TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        });
        set_cell_dimensions(10, 10);
        let image = Image::new(
            "SGVsbG8=",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|s| s.to_string()),
            },
            ImageOptions {
                max_width_cells: Some(2),
                ..Default::default()
            },
        );

        assert_eq!(image.render(10).len(), 2);
        set_cell_dimensions(10, 20);
        assert_eq!(image.render(10).len(), 1);

        set_cell_dimensions(original_cell_dimensions.0, original_cell_dimensions.1);
        set_capabilities(original_capabilities);
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

    #[test]
    fn settings_callbacks_skip_disabled_rows_and_preserve_duplicate_filter_matches() {
        use std::sync::{Arc, Mutex};

        let changes = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let cancels = Arc::new(Mutex::new(0usize));
        let changes_for_callback = changes.clone();
        let cancels_for_callback = cancels.clone();
        let items = vec![
            SettingItem::new(
                "disabled",
                "Duplicate",
                "off",
                vec!["off".into(), "on".into()],
            )
            .with_disabled(true),
            SettingItem::new(
                "enabled-a",
                "Duplicate",
                "one",
                vec!["one".into(), "two".into()],
            ),
            SettingItem::new(
                "enabled-b",
                "Duplicate",
                "red",
                vec!["red".into(), "blue".into()],
            ),
        ];
        let mut list = SettingsList::new_with_callbacks(
            items,
            10,
            plain_settings_theme(),
            move |id, value| {
                changes_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push((id.to_string(), value.to_string()))
            },
            move || {
                *cancels_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) += 1
            },
            SettingsListOptions {
                enable_search: true,
            },
        );

        // Search must retain both rows with the same label rather than mapping
        // both fuzzy results back to the first matching label.
        for character in "Duplicate".chars() {
            list.handle_input(&TuiKey::simple(character.to_string()));
        }
        assert_eq!(
            list.visible_items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["disabled", "enabled-a", "enabled-b"]
        );

        // The initial selection is the first enabled row; Down skips the
        // disabled item and Enter persists through the callback.
        list.handle_input(&TuiKey::simple("down"));
        assert_eq!(list.selected_id().as_deref(), Some("enabled-b"));
        list.handle_input(&TuiKey::simple("enter"));
        assert_eq!(
            changes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            [("enabled-b".to_string(), "blue".to_string())]
        );
        list.handle_input(&TuiKey::simple("esc"));
        list.handle_input(&TuiKey::ctrl("c"));
        assert_eq!(
            *cancels.lock().unwrap_or_else(|error| error.into_inner()),
            2
        );
    }

    #[test]
    fn settings_space_changes_before_search_and_becomes_query_afterward() {
        use std::sync::{Arc, Mutex};

        let changes = Arc::new(Mutex::new(Vec::<String>::new()));
        let changes_for_callback = changes.clone();
        let mut list = SettingsList::new_with_callbacks(
            vec![SettingItem::new(
                "mode",
                "Mode",
                "one",
                vec!["one".into(), "two".into()],
            )],
            5,
            plain_settings_theme(),
            move |_, value| {
                changes_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(value.to_string())
            },
            || {},
            SettingsListOptions {
                enable_search: true,
            },
        );
        list.handle_input(&TuiKey::simple(" "));
        assert_eq!(
            changes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            ["two".to_string()]
        );
        list.handle_input(&TuiKey::simple("m"));
        list.handle_input(&TuiKey::simple(" "));
        assert_eq!(
            changes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            ["two".to_string()]
        );
    }

    #[test]
    fn settings_submenu_done_updates_value_and_closes() {
        use crate::components::settings_list::SettingsSubmenuDoneFn;
        use std::sync::{Arc, Mutex};

        struct DoneComponent {
            done: Option<SettingsSubmenuDoneFn>,
        }
        impl Component for DoneComponent {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["submenu".to_string()]
            }
            fn handle_input(&mut self, key: &TuiKey) {
                if key.base == "enter" {
                    if let Some(done) = self.done.take() {
                        done(Some("selected".to_string()), None);
                    }
                }
            }
        }

        let changes = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let changes_for_callback = changes.clone();
        let item = SettingItem::new("picker", "Picker", "old", Vec::new()).with_submenu_done(
            |current, done| {
                assert_eq!(current, "old");
                Some(Box::new(DoneComponent { done: Some(done) }))
            },
        );
        let mut list = SettingsList::new_with_callbacks(
            vec![item],
            5,
            plain_settings_theme(),
            move |id, value| {
                changes_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push((id.to_string(), value.to_string()));
            },
            || {},
            SettingsListOptions::default(),
        );
        list.handle_input(&TuiKey::simple("enter"));
        assert!(list.is_submenu_open());
        list.handle_input(&TuiKey::simple("enter"));
        assert!(!list.is_submenu_open());
        assert_eq!(list.visible_items()[0].current_value, "selected");
        assert_eq!(
            changes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            [("picker".to_string(), "selected".to_string())]
        );
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
