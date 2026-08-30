#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused component-level parity fixtures for session, tree, auth, and
//! clipboard interactions. These tests stay below the interactive-mode
//! integration boundary and exercise the owned components directly.

use std::collections::HashMap;

use pi_agent::session::types::Entry;
use pi_agent::types::AgentMessage;
use pi_ai::types::{AssistantMessage, Message, StopReason, UserContent};
use pi_coding_agent::interactive::auth::AuthSurfaceAction;
use pi_coding_agent::interactive::clipboard::{
    is_wayland_session, read_clipboard_image_sync, read_clipboard_text_with_env,
};
use pi_coding_agent::interactive::session_meta::{
    match_session, parse_session_search_query, SessionPickerRecord,
};
use pi_coding_agent::interactive::tree_selector::{TreeFilterMode, TreeSelector};
use pi_tui::keys::TuiKey;

fn session_record() -> SessionPickerRecord {
    SessionPickerRecord {
        id: "session-1".to_owned(),
        path: "/tmp/project/session-1.jsonl".to_owned(),
        cwd: "/tmp/project".to_owned(),
        name: None,
        created_at: 1,
        modified_at: 2,
        message_count: 1,
        first_message: "first message only".to_owned(),
        all_messages_text: String::new(),
        parent_session_id: None,
        parent_session_path: None,
    }
}

fn user_entry(id: &str, parent_id: Option<&str>, text: &str, seq: u64) -> Entry {
    Entry::Message {
        id: id.to_owned(),
        seq,
        parent_id: parent_id.map(str::to_owned),
        timestamp: seq,
        message: AgentMessage::Core(Message::User(UserContent::string(text, seq))),
        terminate: None,
    }
}

fn assistant_entry(id: &str, parent_id: Option<&str>, stop_reason: StopReason, seq: u64) -> Entry {
    let mut message = AssistantMessage::new().with_timestamp(seq);
    message.set_stop_reason(stop_reason);
    Entry::Message {
        id: id.to_owned(),
        seq,
        parent_id: parent_id.map(str::to_owned),
        timestamp: seq,
        message: AgentMessage::Core(Message::Assistant(message)),
        terminate: None,
    }
}

#[test]
fn session_search_uses_all_messages_text_without_first_message_fallback() {
    let session = session_record();

    assert!(!match_session(&session, &parse_session_search_query("first")).matches);
    assert!(match_session(&session, &parse_session_search_query("session-1")).matches);
}

#[test]
fn tree_selection_walks_to_visible_ancestor_and_preserves_empty_filter_selection() {
    let entries = vec![
        user_entry("root", None, "root prompt", 1),
        Entry::ModelChange {
            id: "settings".to_owned(),
            seq: 2,
            parent_id: Some("root".to_owned()),
            timestamp: 2,
            provider: "anthropic".to_owned(),
            model_id: "claude".to_owned(),
        },
        user_entry("branch", Some("root"), "branch prompt", 3),
    ];
    let mut selector = TreeSelector::new(entries, HashMap::new(), Some("settings".into()), 30);

    assert_eq!(selector.selected_entry_id().as_deref(), Some("root"));
    selector.handle(&TuiKey::simple("branch"));
    assert_eq!(selector.search_query(), "branch");
    assert_eq!(selector.count(), 1);
    assert_eq!(selector.selected_entry_id().as_deref(), Some("branch"));
    selector.handle(&TuiKey::simple("backspace"));
    selector.handle(&TuiKey::simple("escape"));
    assert_eq!(selector.search_query(), "");
    assert_eq!(selector.selected_entry_id().as_deref(), Some("branch"));
}

#[test]
fn tree_filter_controls_and_non_stop_assistant_rows_match_upstream() {
    let entries = vec![
        user_entry("root", None, "root prompt", 1),
        assistant_entry("stopped", Some("root"), StopReason::Stop, 2),
        assistant_entry("length", Some("root"), StopReason::Length, 3),
    ];
    let mut selector = TreeSelector::new(entries, HashMap::new(), None, 30);

    assert_eq!(selector.count(), 2);
    selector.handle(&TuiKey::ctrl("u"));
    assert_eq!(selector.filter_mode(), TreeFilterMode::UserOnly);
    assert_eq!(selector.count(), 1);
    selector.handle(&TuiKey::ctrl("o"));
    assert_eq!(selector.filter_mode(), TreeFilterMode::LabeledOnly);
    selector.handle(&TuiKey {
        base: "o".to_owned(),
        ctrl: true,
        shift: true,
        alt: false,
        super_key: false,
    });
    assert_eq!(selector.filter_mode(), TreeFilterMode::UserOnly);
}

#[test]
fn auth_prompt_uses_grapheme_safe_editing_and_upstream_paste_cleanup() {
    let mut surface = pi_coding_agent::interactive::auth::AuthSurfaceState::dialog("Login");
    surface.set_prompt(&pi_ai::auth::AuthPrompt::ManualCode {
        message: "Paste code".to_owned(),
        placeholder: None,
    });
    surface.handle_raw("\x1b[200~line\r\nend\t\x1b[201~");
    surface.handle_raw("\x1b[200~e\u{301}🙂\x1b[201~");
    assert_eq!(
        surface.handle(&TuiKey::simple("backspace")),
        AuthSurfaceAction::None
    );
    assert_eq!(
        surface.handle(&TuiKey::simple("backspace")),
        AuthSurfaceAction::None
    );
    assert_eq!(
        surface.handle(&TuiKey::simple("enter")),
        AuthSurfaceAction::Submit("lineend    ".to_owned())
    );
}

#[cfg(unix)]
mod unix_clipboard_fixtures {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScriptDirectory(PathBuf);

    impl ScriptDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "pi-clipboard-parity-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create fixture directory");
            Self(path)
        }

        fn command(&self, name: &str, body: &str) {
            let path = self.0.join(name);
            fs::write(&path, body).expect("write fixture command");
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                .expect("make fixture command executable");
        }

        fn env(&self) -> Vec<(String, String)> {
            vec![
                ("PATH".to_owned(), self.0.display().to_string()),
                ("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned()),
                ("DISPLAY".to_owned(), ":0".to_owned()),
            ]
        }
    }

    impl Drop for ScriptDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn empty_wayland_clipboard_does_not_fall_back_to_stale_x11_text() {
        let fixture = ScriptDirectory::new();
        fixture.command("wl-paste", "#!/bin/sh\nexit 0\n");
        fixture.command("xclip", "#!/bin/sh\nprintf stale-x11\n");
        let env = fixture.env();

        assert!(is_wayland_session(&env));
        assert_eq!(read_clipboard_text_with_env(&env, "linux"), None);
    }

    #[test]
    fn image_clipboard_trims_mime_target_before_second_probe() {
        let fixture = ScriptDirectory::new();
        fixture.command(
            "wl-paste",
            "#!/bin/sh\nif [ \"$1\" = \"--list-types\" ]; then printf ' text/plain\\n image/png \\n'; else if [ \"$2\" = \"image/png\" ]; then printf '\\211PNG\\r\\n\\032\\n'; fi; fi\n",
        );
        let image = read_clipboard_image_sync(&fixture.env(), "linux").expect("fixture image");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.bytes, b"\x89PNG\r\n\x1a\n");
    }
}
