//! Session-picker helpers for interactive mode — builds picker items from
//! repo metadata without depending on the session crate's full surface.

use pi_agent::session::types::SessionMetadata;

/// Minimal metadata carried by the /resume picker (label + id + full metadata).
#[derive(Debug, Clone)]
pub struct SessionMetaForPicker {
    pub id: String,
    pub label: String,
    pub metadata: SessionMetadata,
}

/// Sort sessions newest-first and render picker labels from file names.
pub fn session_picker_items(sessions: Vec<SessionMetadata>) -> Vec<SessionMetaForPicker> {
    let mut sessions = sessions;
    sessions.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    sessions
        .into_iter()
        .map(|metadata| {
            let label = std::path::Path::new(&metadata.path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| metadata.id.clone());
            SessionMetaForPicker { id: metadata.id.clone(), label, metadata }
        })
        .collect()
}

/// Build SelectItems for the picker UI.
pub fn picker_select_items(items: &[SessionMetaForPicker]) -> Vec<pi_tui::components::select_list::SelectItem> {
    items
        .iter()
        .map(|item| {
            pi_tui::components::select_list::SelectItem::new(
                item.id.clone(),
                item.label.clone(),
                Some(item.metadata.cwd.clone()),
            )
        })
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::session::types::SessionMetadata;

    fn meta(id: &str, modified: u64) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            created_at: 1,
            cwd: "/tmp/proj".to_string(),
            path: format!("/tmp/proj/sessions/2026-01-01T00-00-00_{id}.jsonl"),
            modified_at: modified,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }

    #[test]
    fn picker_sorts_newest_first() {
        let items = session_picker_items(vec![meta("old", 10), meta("new", 30), meta("mid", 20)]);
        assert_eq!(items[0].id, "new");
        assert_eq!(items[1].id, "mid");
        assert_eq!(items[2].id, "old");
    }

    #[test]
    fn picker_labels_use_file_names() {
        let items = session_picker_items(vec![meta("abc123", 10)]);
        assert_eq!(items[0].label, "2026-01-01T00-00-00_abc123.jsonl");
        let select = picker_select_items(&items);
        assert_eq!(select.len(), 1);
        assert_eq!(select[0].value, "abc123");
        assert_eq!(select[0].description.as_deref(), Some("/tmp/proj"));
    }
}
