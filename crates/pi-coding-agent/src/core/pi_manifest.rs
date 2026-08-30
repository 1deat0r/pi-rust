//! Pi package manifest — port of
//! `packages/coding-agent/src/core/pi-manifest.ts`.
//!
//! Reads the `pi` field of a `package.json` (extension/skill/prompt/theme
//! entry points). Used by the extension loader and the package manager.

use std::path::Path;

use crate::core::settings::strip_bom;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PiManifest {
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
}

pub const RESOURCE_FIELDS: [&str; 4] = ["extensions", "skills", "prompts", "themes"];

/// Read the `pi` manifest from a package.json path. Returns `None` when the
/// file is missing/unparseable or has no valid `pi` object.
pub fn read_pi_manifest(package_json_path: &Path) -> Option<PiManifest> {
    let content = std::fs::read_to_string(package_json_path).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(strip_bom(&content)).ok()?;
    let pi = pkg.get("pi")?;
    if !pi.is_object() {
        return None;
    }
    let mut manifest = PiManifest::default();
    for (field, target) in [
        ("extensions", &mut manifest.extensions),
        ("skills", &mut manifest.skills),
        ("prompts", &mut manifest.prompts),
        ("themes", &mut manifest.themes),
    ] {
        if let Some(entries) = pi.get(field) {
            if let Some(list) = entries.as_array() {
                if list.iter().all(|entry| entry.is_string()) {
                    *target = list
                        .iter()
                        .filter_map(|entry| entry.as_str().map(|s| s.to_string()))
                        .collect();
                }
            }
        }
    }
    Some(manifest)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write_pkg(dir: &Path, content: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("package.json");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn reads_extension_entries() {
        let dir = std::env::temp_dir().join(format!("pi-manifest-{}", uuid::Uuid::new_v4()));
        let path = write_pkg(
            &dir,
            r#"{ "name": "ext", "pi": { "extensions": ["index.ts", "extra.js"] } }"#,
        );
        let manifest = read_pi_manifest(&path).unwrap();
        assert_eq!(manifest.extensions, vec!["index.ts", "extra.js"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_pi_field_returns_none() {
        let dir = std::env::temp_dir().join(format!("pi-manifest-{}", uuid::Uuid::new_v4()));
        let path = write_pkg(&dir, r#"{ "name": "ext" }"#);
        assert!(read_pi_manifest(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_string_entries_ignored() {
        let dir = std::env::temp_dir().join(format!("pi-manifest-{}", uuid::Uuid::new_v4()));
        let path = write_pkg(&dir, r#"{ "pi": { "extensions": ["ok.ts", 42] } }"#);
        // Upstream requires every entry to be a string; a mixed array is
        // rejected wholesale for that field.
        let manifest = read_pi_manifest(&path).unwrap();
        assert!(manifest.extensions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_returns_none() {
        assert!(read_pi_manifest(Path::new("/nonexistent/package.json")).is_none());
    }

    #[test]
    fn all_resource_fields_parsed() {
        let dir = std::env::temp_dir().join(format!("pi-manifest-{}", uuid::Uuid::new_v4()));
        let path = write_pkg(
            &dir,
            r#"{ "pi": { "extensions": ["i.ts"], "skills": ["s.md"], "prompts": ["p.md"], "themes": ["t.json"] } }"#,
        );
        let manifest = read_pi_manifest(&path).unwrap();
        assert_eq!(manifest.skills, vec!["s.md"]);
        assert_eq!(manifest.prompts, vec!["p.md"]);
        assert_eq!(manifest.themes, vec!["t.json"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
