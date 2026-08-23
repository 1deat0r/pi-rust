//! Project trust — port of `packages/coding-agent/src/core/project-trust.ts`
//! + `trust-manager.ts`.
//!
//! A `trust.json` store next to the agent dir records per-directory trust
//! decisions (nearest-ancestor lookup). `has_trust_requiring_project_resources`
//! gates project-local resources (`.pi/settings.json`, `.pi/extensions`,
//! `.pi/skills`, `.pi/prompts`, `.pi/themes`, `SYSTEM.md`, `APPEND_SYSTEM.md`,
//! and `.agents/skills` in cwd or ancestors). `resolve_project_trusted`
//! applies the CLI override, stored decision, or settings default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Trust-requiring entries under `<cwd>/.pi` (upstream
/// `TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES`).
const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 7] = [
    "settings.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// Canonicalize a path (upstream `canonicalizePath(resolvePath(cwd))`).
fn normalize_cwd(cwd: &str) -> String {
    let expanded = crate::config::expand_tilde_path(cwd);
    std::fs::canonicalize(&expanded)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| expanded)
}

/// True when cwd has project-local resources that must be gated by project
/// trust (upstream `hasTrustRequiringProjectResources`).
pub fn has_trust_requiring_project_resources(cwd: &str) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let user_agents_skills = Path::new(&home).join(".agents").join("skills");
    let mut current = PathBuf::from(normalize_cwd(cwd));

    let config_dir = current.join(crate::config::CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }

    loop {
        let agents_skills = current.join(".agents").join("skills");
        if agents_skills != user_agents_skills && agents_skills.exists() {
            return true;
        }
        let parent = current.parent();
        match parent {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return false,
        }
    }
}

/// One trust decision for a directory.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTrustStoreEntry {
    pub path: String,
    pub decision: bool,
}

/// A proposed trust update (upstream `ProjectTrustUpdate`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTrustUpdate {
    pub path: String,
    pub decision: Option<bool>,
}

/// One selectable trust option (upstream `ProjectTrustOption`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
    pub saved_path: Option<String>,
}

/// The trust store: `trust.json` next to the agent dir.
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &str) -> Self {
        Self {
            trust_path: Path::new(agent_dir).join("trust.json"),
        }
    }

    pub fn trust_path(&self) -> &Path {
        &self.trust_path
    }

    /// Nearest-ancestor decision for cwd (upstream `findNearestTrustEntry`).
    pub fn get(&self, cwd: &str) -> Option<bool> {
        self.get_entry(cwd).map(|e| e.decision)
    }

    pub fn get_entry(&self, cwd: &str) -> Option<ProjectTrustStoreEntry> {
        let data = read_trust_file(&self.trust_path).ok()?;
        let mut current = normalize_cwd(cwd);
        loop {
            if let Some(value) = data.get(&current) {
                if let Some(decision) = value {
                    return Some(ProjectTrustStoreEntry {
                        path: current.clone(),
                        decision: *decision,
                    });
                }
            }
            let parent = Path::new(&current)
                .parent()
                .map(|p| p.to_string_lossy().into_owned());
            match parent {
                Some(parent) if parent != current => current = parent,
                _ => return None,
            }
        }
    }

    pub fn set(&self, cwd: &str, decision: Option<bool>) {
        self.set_many(&[ProjectTrustUpdate {
            path: cwd.to_string(),
            decision,
        }]);
    }

    pub fn set_many(&self, updates: &[ProjectTrustUpdate]) {
        let mut data = read_trust_file(&self.trust_path).unwrap_or_default();
        for update in updates {
            let key = normalize_cwd(&update.path);
            match update.decision {
                Some(decision) => {
                    data.insert(key, Some(decision));
                }
                None => {
                    data.remove(&key);
                }
            }
        }
        write_trust_file(&self.trust_path, &data);
    }
}

/// Trust options for a cwd (upstream `getProjectTrustOptions`).
pub fn get_project_trust_options(cwd: &str, include_session_only: bool) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_string(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(true),
        }],
        saved_path: Some(trust_path.clone()),
    }];
    let parent = Path::new(&trust_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned());
    if let Some(parent) = parent {
        if parent != trust_path {
            options.push(ProjectTrustOption {
                label: format!("Trust parent folder ({parent})"),
                trusted: true,
                updates: vec![
                    ProjectTrustUpdate {
                        path: parent.clone(),
                        decision: Some(true),
                    },
                    ProjectTrustUpdate {
                        path: trust_path.clone(),
                        decision: None,
                    },
                ],
                saved_path: Some(parent),
            });
        }
    }
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_string(),
            trusted: true,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_string(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(false),
        }],
        saved_path: Some(trust_path),
    });
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_string(),
            trusted: false,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options
}

/// Resolve whether the project is trusted (upstream `resolveProjectTrusted`).
/// `trust_override` is the `-a/--approve` / `-na/--no-approve` CLI flag.
pub fn resolve_project_trusted(
    cwd: &str,
    trust_store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: Option<&str>,
) -> bool {
    if let Some(override_value) = trust_override {
        return override_value;
    }
    if !has_trust_requiring_project_resources(cwd) {
        return true;
    }
    if let Some(decision) = trust_store.get(cwd) {
        return decision;
    }
    match default_project_trust.unwrap_or("ask") {
        "always" => true,
        "never" => false,
        _ => false, // "ask" without a UI: not trusted (upstream hasUI check).
    }
}

fn read_trust_file(path: &Path) -> Result<BTreeMap<String, Option<bool>>, String> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read trust store {path:?}: {e}"))?;
    let content = crate::core::settings::strip_bom(&content);
    let parsed: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to read trust store {path:?}: {e}"))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| format!("Invalid trust store {path:?}: expected an object"))?;
    let mut data = BTreeMap::new();
    for (key, value) in obj {
        match value {
            serde_json::Value::Bool(b) => {
                data.insert(key.clone(), Some(*b));
            }
            serde_json::Value::Null => {
                data.insert(key.clone(), None);
            }
            _ => {
                return Err(format!(
                    "Invalid trust store {path:?}: value for {key:?} must be true, false, or null"
                ));
            }
        }
    }
    Ok(data)
}

fn write_trust_file(path: &Path, data: &BTreeMap<String, Option<bool>>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sorted: BTreeMap<&String, &Option<bool>> = data.iter().collect();
    let obj: serde_json::Map<String, serde_json::Value> = sorted
        .iter()
        .map(|(k, v)| match v {
            Some(true) => (k.to_string(), serde_json::Value::Bool(true)),
            Some(false) => (k.to_string(), serde_json::Value::Bool(false)),
            None => (k.to_string(), serde_json::Value::Null),
        })
        .collect();
    let content = format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap_or_default()
    );
    let _ = std::fs::write(path, content);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-trust-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn store_round_trips_decisions() {
        let root = sandbox("roundtrip");
        let store = ProjectTrustStore::new(root.to_str().unwrap());
        let cwd = root.join("project").to_string_lossy().into_owned();
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(store.get(&cwd), None);
        store.set(&cwd, Some(true));
        assert_eq!(store.get(&cwd), Some(true));
        // Nearest-ancestor lookup from a subdirectory.
        let sub = root
            .join("project")
            .join("sub")
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(store.get(&sub), Some(true));
        // Clearing removes the entry.
        store.set(&cwd, None);
        assert_eq!(store.get(&cwd), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trust_file_format_matches_upstream() {
        let root = sandbox("format");
        let store = ProjectTrustStore::new(root.to_str().unwrap());
        let cwd = root.join("p").to_string_lossy().into_owned();
        std::fs::create_dir_all(&cwd).unwrap();
        store.set(&cwd, Some(true));
        let content = std::fs::read_to_string(store.trust_path()).unwrap();
        assert!(content.contains("true"), "content: {content}");
        assert!(content.trim_end().ends_with('}'), "pretty-printed object");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resources_detection_gates_project_config() {
        let root = sandbox("resources");
        let cwd = root.join("proj").to_string_lossy().into_owned();
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(!has_trust_requiring_project_resources(&cwd));
        // .pi/settings.json triggers trust.
        std::fs::create_dir_all(Path::new(&cwd).join(".pi")).unwrap();
        std::fs::write(Path::new(&cwd).join(".pi").join("settings.json"), "{}").unwrap();
        assert!(has_trust_requiring_project_resources(&cwd));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_applies_override_then_store_then_default() {
        let root = sandbox("resolve");
        let store = ProjectTrustStore::new(root.to_str().unwrap());
        let cwd = root.join("proj").to_string_lossy().into_owned();
        std::fs::create_dir_all(Path::new(&cwd).join(".pi")).unwrap();
        std::fs::write(Path::new(&cwd).join(".pi").join("settings.json"), "{}").unwrap();
        // Override wins.
        assert!(resolve_project_trusted(&cwd, &store, Some(true), None));
        assert!(!resolve_project_trusted(&cwd, &store, Some(false), None));
        // Stored decision wins over default.
        store.set(&cwd, Some(true));
        assert!(resolve_project_trusted(&cwd, &store, None, Some("never")));
        // Default applies when no store entry.
        let cwd2 = root.join("proj2").to_string_lossy().into_owned();
        std::fs::create_dir_all(Path::new(&cwd2).join(".pi")).unwrap();
        std::fs::write(Path::new(&cwd2).join(".pi").join("settings.json"), "{}").unwrap();
        assert!(resolve_project_trusted(&cwd2, &store, None, Some("always")));
        assert!(!resolve_project_trusted(&cwd2, &store, None, Some("never")));
        // No resources -> trusted regardless.
        let plain = root.join("plain").to_string_lossy().into_owned();
        std::fs::create_dir_all(&plain).unwrap();
        assert!(resolve_project_trusted(&plain, &store, None, Some("never")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trust_options_shape() {
        let root = sandbox("options");
        let cwd = root.join("proj").to_string_lossy().into_owned();
        std::fs::create_dir_all(&cwd).unwrap();
        let options = get_project_trust_options(&cwd, true);
        assert_eq!(options.len(), 5);
        assert_eq!(options[0].label, "Trust");
        assert!(options[0].trusted);
        assert_eq!(options[3].label, "Do not trust");
        assert!(!options[3].trusted);
        let _ = std::fs::remove_dir_all(&root);
    }
}
