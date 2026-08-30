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
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

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
    let resolved = crate::core::settings::resolve_path(cwd);
    std::fs::canonicalize(&resolved)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved.to_string_lossy().into_owned())
}

/// True when cwd has project-local resources that must be gated by project
/// trust (upstream `hasTrustRequiringProjectResources`).
pub fn has_trust_requiring_project_resources(cwd: &str) -> bool {
    let home = crate::config::home_dir().unwrap_or_default();
    let home = PathBuf::from(normalize_cwd(&home.to_string_lossy()));
    let user_agents_skills = home.join(".agents").join("skills");
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
        let normalized_agents_skills =
            std::fs::canonicalize(&agents_skills).unwrap_or_else(|_| agents_skills.clone());
        if normalized_agents_skills != user_agents_skills && agents_skills.exists() {
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

struct TrustFileLock {
    path: PathBuf,
}

impl Drop for TrustFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Acquire the same sidecar lock used by the upstream trust manager. Trust
/// decisions are written by more than one process in normal use (for example,
/// a foreground CLI and an interactive session), so a read/modify/write must
/// not allow one decision to erase another.
fn try_acquire_trust_file_lock(path: &Path) -> Result<TrustFileLock, String> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Failed to create trust store directory {parent:?}: {error}")
        })?;
    }
    for _ in 0..100 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(TrustFileLock { path: lock_path }),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(format!(
                    "Failed to acquire project-trust lock {lock_path:?}: {error}"
                ));
            }
        }
    }
    Err(format!(
        "Timed out acquiring project-trust lock {lock_path:?}"
    ))
}

fn try_with_trust_file_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _lock = try_acquire_trust_file_lock(path)?;
    operation()
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &str) -> Self {
        let agent_dir = crate::core::settings::resolve_path(agent_dir);
        Self {
            trust_path: agent_dir.join("trust.json"),
        }
    }

    pub fn trust_path(&self) -> &Path {
        &self.trust_path
    }

    /// Nearest-ancestor decision for cwd (upstream `findNearestTrustEntry`).
    pub fn get(&self, cwd: &str) -> Option<bool> {
        self.get_entry(cwd).map(|e| e.decision)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn get_entry(&self, cwd: &str) -> Option<ProjectTrustStoreEntry> {
        self.try_get_entry(cwd)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    /// Read a trust decision without converting malformed or inaccessible
    /// storage into a process panic. Interactive callers use this form so a
    /// failed `/trust` open can return to the editor with a truthful error.
    pub fn try_get(&self, cwd: &str) -> Result<Option<bool>, String> {
        self.try_get_entry(cwd)
            .map(|entry| entry.map(|entry| entry.decision))
    }

    pub fn try_get_entry(&self, cwd: &str) -> Result<Option<ProjectTrustStoreEntry>, String> {
        try_with_trust_file_lock(&self.trust_path, || {
            let data = read_trust_file(&self.trust_path)?;
            let mut current = normalize_cwd(cwd);
            loop {
                if let Some(Some(decision)) = data.get(&current) {
                    return Ok(Some(ProjectTrustStoreEntry {
                        path: current.clone(),
                        decision: *decision,
                    }));
                }
                let parent = Path::new(&current)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned());
                match parent {
                    Some(parent) if parent != current => current = parent,
                    _ => return Ok(None),
                }
            }
        })
    }

    pub fn set(&self, cwd: &str, decision: Option<bool>) {
        self.set_many(&[ProjectTrustUpdate {
            path: cwd.to_string(),
            decision,
        }]);
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn set_many(&self, updates: &[ProjectTrustUpdate]) {
        self.try_set_many(updates)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    /// Persist trust updates while returning storage/locking diagnostics to a
    /// UI caller. The legacy `set_many` wrapper retains its historical panic
    /// contract for non-interactive callers that cannot surface a Result.
    pub fn try_set_many(&self, updates: &[ProjectTrustUpdate]) -> Result<(), String> {
        try_with_trust_file_lock(&self.trust_path, || {
            let mut data = read_trust_file(&self.trust_path)?;
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
            write_trust_file(&self.trust_path, &data)
        })
    }

    pub fn try_set(&self, cwd: &str, decision: Option<bool>) -> Result<(), String> {
        self.try_set_many(&[ProjectTrustUpdate {
            path: cwd.to_string(),
            decision,
        }])
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
    match trust_store.try_get(cwd) {
        Ok(Some(decision)) => return decision,
        Ok(None) => {}
        Err(_) => return false,
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
    let parsed: serde_json::Value = serde_json::from_str(content)
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

fn write_trust_file(path: &Path, data: &BTreeMap<String, Option<bool>>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("Failed to create trust store directory {parent:?}: {error}")
        })?;
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
    let content = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
        .map(|content| format!("{content}\n"))
        .map_err(|error| format!("Failed to encode trust store {path:?}: {error}"))?;
    std::fs::write(path, content)
        .map_err(|error| format!("Failed to write trust store {path:?}: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn every_project_resource_marker_requires_trust() {
        let root = sandbox("resource-matrix");
        let cwd = root.join("proj");
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        for resource in [
            "settings.json",
            "extensions",
            "skills",
            "prompts",
            "themes",
            "SYSTEM.md",
            "APPEND_SYSTEM.md",
        ] {
            let path = cwd.join(".pi").join(resource);
            if resource.ends_with("s") {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::write(&path, "").unwrap();
            }
            assert!(
                has_trust_requiring_project_resources(cwd.to_str().unwrap()),
                "resource marker {resource:?} was not gated"
            );
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }

        let ancestor = root.join("ancestor");
        std::fs::create_dir_all(ancestor.join(".agents").join("skills")).unwrap();
        let nested = ancestor.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(has_trust_requiring_project_resources(
            nested.to_str().unwrap()
        ));
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

    #[test]
    fn concurrent_store_writes_preserve_decisions() {
        let root = sandbox("concurrent");
        let agent_dir = root.join("agent");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let agent = agent_dir.to_string_lossy().into_owned();
        let first_path = first.to_string_lossy().into_owned();
        let second_path = second.to_string_lossy().into_owned();
        let left = std::thread::spawn({
            let agent = agent.clone();
            move || ProjectTrustStore::new(&agent).set(&first_path, Some(true))
        });
        let right = std::thread::spawn({
            let agent = agent.clone();
            move || ProjectTrustStore::new(&agent).set(&second_path, Some(false))
        });
        left.join().unwrap();
        right.join().unwrap();

        let store = ProjectTrustStore::new(&agent);
        assert_eq!(store.get(&first.to_string_lossy()), Some(true));
        assert_eq!(store.get(&second.to_string_lossy()), Some(false));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn try_store_reports_malformed_data_without_panicking() {
        let root = sandbox("malformed");
        let store = ProjectTrustStore::new(root.to_str().unwrap());
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(store.trust_path(), "{not-json").unwrap();

        let read_error = store.try_get_entry(cwd.to_str().unwrap()).unwrap_err();
        assert!(
            read_error.contains("Failed to read trust store"),
            "{read_error}"
        );
        let write_error = store
            .try_set(cwd.to_str().unwrap(), Some(true))
            .unwrap_err();
        assert!(
            write_error.contains("Failed to read trust store"),
            "{write_error}"
        );
        assert_eq!(
            std::fs::read_to_string(store.trust_path()).unwrap(),
            "{not-json"
        );
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        std::fs::write(cwd.join(".pi").join("settings.json"), "{}").unwrap();
        assert!(!resolve_project_trusted(
            cwd.to_str().unwrap(),
            &store,
            None,
            Some("always")
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn try_store_reports_directory_trust_path_without_panicking() {
        let root = sandbox("directory-path");
        let store = ProjectTrustStore::new(root.to_str().unwrap());
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir(store.trust_path()).unwrap();

        let error = store
            .try_set(cwd.to_str().unwrap(), Some(true))
            .unwrap_err();
        assert!(error.contains("Failed to read trust store"), "{error}");
        assert!(store.trust_path().is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn trust_store_canonicalizes_symlinked_cwds() {
        use std::os::unix::fs::symlink;

        let root = sandbox("symlink");
        let real = root.join("real");
        let link = root.join("link");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();
        let store = ProjectTrustStore::new(root.to_str().unwrap());

        store.try_set(link.to_str().unwrap(), Some(true)).unwrap();

        assert_eq!(store.try_get(link.to_str().unwrap()).unwrap(), Some(true));
        assert_eq!(store.try_get(real.to_str().unwrap()).unwrap(), Some(true));
        let content = std::fs::read_to_string(store.trust_path()).unwrap();
        assert!(content.contains(&real.to_string_lossy().to_string()));
        assert!(!content.contains(&link.to_string_lossy().to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }
}
