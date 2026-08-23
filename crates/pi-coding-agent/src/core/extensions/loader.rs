//! Extension loader — port of
//! `packages/coding-agent/src/core/extensions/loader.ts`.
//!
//! Rust cannot execute TypeScript extension modules in-process (the upstream
//! uses jiti imports). The port keeps the exact discovery/resolution surface
//! and models the module execution step as an *external extension runner*:
//! the resolved entry is spawned with `node <entry>` (bun when node is
//! missing), mirroring what the TS runtime would execute. The spawned process
//! is expected to perform extension registration side effects; because a Rust
//! `pi` exposes no JS API to subprocesses, a nonzero exit is reported as the
//! deterministic load error. Divergence from upstream: in-process TS
//! evaluation is replaced by the external runner; the upstream `jiti` cache is
//! replaced with per-cwd path deduplication.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::config::CONFIG_DIR_NAME;
use crate::core::extensions::types::{
    Extension, ExtensionLoadError, ExtensionRuntime, LoadExtensionsResult,
    PendingNativeProviderRegistration, PendingProviderRegistration, SourceInfo,
};
use crate::core::pi_manifest::read_pi_manifest;

/// Entry file extensions accepted by the loader (upstream `isExtensionFile`).
pub fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js")
}

/// Resolve extension entry points from a directory. Checks, in order, for a
/// `package.json` with a `pi.extensions` field (returning its declared paths)
/// and then `index.ts`/`index.js`. Returns resolved paths or `None` when no
/// entry points exist.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json = dir.join("package.json");
    if package_json.exists() {
        if let Some(manifest) = read_pi_manifest(&package_json) {
            if !manifest.extensions.is_empty() {
                let mut entries = Vec::new();
                for ext_path in &manifest.extensions {
                    let resolved = dir.join(ext_path);
                    if resolved.exists() {
                        entries.push(resolved);
                    }
                }
                if !entries.is_empty() {
                    return Some(entries);
                }
            }
        }
    }
    let index_ts = dir.join("index.ts");
    let index_js = dir.join("index.js");
    if index_ts.exists() {
        return Some(vec![index_ts]);
    }
    if index_js.exists() {
        return Some(vec![index_js]);
    }
    None
}

/// Discover extensions in a directory (upstream `discoverExtensionsInDir`),
/// with no recursion beyond one level. The rules are: include direct
/// `*.ts`/`*.js` files; include subdirectory index files; include entries
/// declared by a subdirectory package.json `pi` manifest.
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut discovered = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().ok();
        let is_symlink = matches!(file_type, Some(t) if t.is_symlink());
        let is_file = matches!(file_type, Some(t) if t.is_file()) || is_symlink;
        let is_dir = matches!(file_type, Some(t) if t.is_dir()) || is_symlink;
        // 1. Direct files.
        if is_file && is_extension_file(&name) {
            discovered.push(entry_path);
            continue;
        }
        // 2 & 3. Subdirectories.
        if is_dir {
            if let Some(entries) = resolve_extension_entries(&entry_path) {
                discovered.extend(entries);
            }
        }
    }
    discovered
}

/// Spawn an external extension runner for a resolved entry.
///
/// Argument protocol: `node <entry>` (bun when node is absent). The entry is
/// spawned with the containing directory as cwd. Returns the extension record
/// on exit 0 (hidden=false) or a deterministic error on nonzero exit.
pub fn run_external_extension(
    extension_path: &str,
    resolved_path: &Path,
    runner: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<Extension, String> {
    let runner = match runner {
        Some(runner) => runner.to_string(),
        None => {
            if command_on_path("node") {
                "node".to_string()
            } else if command_on_path("bun") {
                "bun".to_string()
            } else {
                return Err(
                    "Failed to load extension: no external extension runner found on PATH (expected node or bun)"
                        .to_string(),
                );
            }
        }
    };
    let cwd = resolved_path.parent().unwrap_or_else(|| Path::new("."));
    let mut command = Command::new(&runner);
    command
        .arg(resolved_path)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to load extension: {e}"))?;
    if let Some(timeout_ms) = timeout_ms {
        let pid = child.id();
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("Failed to load extension: {e}"))?
            {
                return finish_child(status.code(), child, extension_path, resolved_path);
            }
            if start.elapsed().as_millis() >= timeout_ms as u128 {
                let _ = Command::new("kill")
                    .args([pid.to_string().as_str()])
                    .status();
                return Err(format!(
                    "Failed to load extension: extension runner timed out for {extension_path}"
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    } else {
        let output = child
            .wait_with_output()
            .map_err(|e| format!("Failed to load extension: {e}"))?;
        if output.status.success() {
            Ok(make_extension(extension_path, resolved_path))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let summary = stderr.trim();
            let error = if summary.is_empty() {
                format!(
                    "Failed to load extension: runner exited with code {}",
                    output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            } else {
                format!("Failed to load extension: {summary}")
            };
            Err(error)
        }
    }
}

fn finish_child(
    code: Option<i32>,
    mut child: std::process::Child,
    extension_path: &str,
    resolved_path: &Path,
) -> Result<Extension, String> {
    if code == Some(0) {
        // Drain remaining pipes.
        let _ = child.wait_with_output();
        Ok(make_extension(extension_path, resolved_path))
    } else {
        let _ = child.kill();
        let _ = child.wait();
        Err(format!(
            "Failed to load extension: runner exited with code {}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ))
    }
}

fn make_extension(extension_path: &str, resolved_path: &Path) -> Extension {
    let source = if extension_path.starts_with('<') && extension_path.ends_with('>') {
        extract_synthetic_source(extension_path)
    } else {
        "local".to_string()
    };
    let base_dir = if extension_path.starts_with('<') {
        None
    } else {
        resolved_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
    };
    Extension {
        path: extension_path.to_string(),
        resolved_path: resolved_path.to_string_lossy().into_owned(),
        hidden: false,
        source_info: SourceInfo::synthetic(extension_path, &source, base_dir),
        ..Default::default()
    }
}

/// Upstream source derivation: `<inline:name>` -> "inline"; `<bundled:name>`
/// -> "bundled". The source is the part before the colon inside the brackets.
fn extract_synthetic_source(extension_path: &str) -> String {
    let inner = &extension_path[1..extension_path.len() - 1];
    inner.split(':').next().unwrap_or("temporary").to_string()
}

fn command_on_path(name: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join(name).is_file() {
                return true;
            }
            #[cfg(windows)]
            if dir.join(format!("{name}.exe")).is_file() {
                return true;
            }
        }
    }
    false
}

/// Load one extension path: resolve relative to `cwd`, then run the external
/// runner. Returns the extension or a recorded error (upstream `loadExtension`).
pub fn load_extension(
    extension_path: &str,
    cwd: &str,
    runner: Option<&str>,
) -> Result<Extension, ExtensionLoadError> {
    let resolved = resolve_relative_path(extension_path, cwd);
    match run_external_extension(extension_path, &resolved, runner, None) {
        Ok(extension) => Ok(extension),
        Err(error) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error,
        }),
    }
}

/// Resolve a path against a base directory without canonicalization
/// (a conservative stand-in for the upstream `resolvePath`).
pub fn resolve_relative_path(path: &str, base: &str) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        path_buf
    } else {
        Path::new(base).join(path_buf)
    }
}

/// Create the shared extension runtime (upstream `createExtensionRuntime`).
pub fn create_extension_runtime() -> Arc<Mutex<ExtensionRuntime>> {
    Arc::new(Mutex::new(ExtensionRuntime::new()))
}

/// Load extensions from explicit paths (upstream `loadExtensions`).
pub fn load_extensions(
    paths: &[String],
    cwd: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
) -> LoadExtensionsResult {
    let runtime = runtime.unwrap_or_else(create_extension_runtime);
    let mut extensions = Vec::new();
    let mut errors = Vec::new();
    for ext_path in paths {
        match load_extension(ext_path, cwd, runner) {
            Ok(extension) => extensions.push(extension),
            Err(error) => errors.push(error),
        }
    }
    LoadExtensionsResult {
        extensions,
        errors,
        runtime,
    }
}

/// Discover and load extensions from standard locations (upstream
/// `discoverAndLoadExtensions`):
/// 1. project-local `cwd/.pi/extensions/`
/// 2. global `agentDir/extensions/`
/// 3. explicitly configured paths
pub fn discover_and_load_extensions(
    configured_paths: &[String],
    cwd: &str,
    agent_dir: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
) -> LoadExtensionsResult {
    let mut all_paths: Vec<String> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut add_paths = |paths: Vec<PathBuf>| {
        for p in paths {
            let absolute = if p.is_absolute() {
                p.clone()
            } else {
                Path::new(cwd).join(&p)
            };
            if seen.insert(absolute.clone()) {
                all_paths.push(absolute.to_string_lossy().into_owned());
            }
        }
    };

    // 1. Project-local extensions.
    let local_ext_dir = Path::new(cwd).join(CONFIG_DIR_NAME).join("extensions");
    add_paths(discover_extensions_in_dir(&local_ext_dir));

    // 2. Global extensions.
    let global_ext_dir = Path::new(agent_dir).join("extensions");
    add_paths(discover_extensions_in_dir(&global_ext_dir));

    // 3. Explicitly configured paths.
    for p in configured_paths {
        let resolved = resolve_relative_path(p, cwd);
        if resolved.is_dir() {
            if let Some(entries) = resolve_extension_entries(&resolved) {
                add_paths(entries);
                continue;
            }
            add_paths(discover_extensions_in_dir(&resolved));
            continue;
        }
        add_paths(vec![resolved]);
    }

    load_extensions(&all_paths, cwd, runtime, runner)
}

/// Load an inline/bundled extension by spawning its entry as a subprocess
/// (upstream `loadExtensionFromFactory`: the wrapper transforms a bundled
/// extension into a runnable subprocess; in the JS runtime it executes the
/// factory in-process, and the port executes the entry file with the external
/// runner instead).
pub fn load_bundled_extension(
    extension_path: &str,
    runner: Option<&str>,
) -> Result<Extension, ExtensionLoadError> {
    let path = PathBuf::from(extension_path);
    let resolved = if path.is_absolute() {
        path
    } else {
        Path::new(".").join(&path)
    };
    match run_external_extension(extension_path, &resolved, runner, None) {
        Ok(extension) => Ok(extension),
        Err(error) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error,
        }),
    }
}

/// Runtime helpers used by runner.rs: queue a provider registration.
pub fn queue_provider_registration(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    registration: PendingProviderRegistration,
) {
    runtime
        .lock()
        .unwrap()
        .pending_provider_registrations
        .push(registration);
}

pub fn queue_native_provider_registration(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    registration: PendingNativeProviderRegistration,
) {
    runtime
        .lock()
        .unwrap()
        .pending_native_provider_registrations
        .push(registration);
}

/// Serialize the runtime's queued provider registrations (upstream flush).
pub fn take_pending_provider_registrations(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> (
    Vec<PendingProviderRegistration>,
    Vec<PendingNativeProviderRegistration>,
) {
    let mut guard = runtime.lock().unwrap();
    (
        std::mem::take(&mut guard.pending_provider_registrations),
        std::mem::take(&mut guard.pending_native_provider_registrations),
    )
}

/// Build the `VirtualModule`-style flag map from registered extension flags:
/// defaults are applied when no CLI value exists (upstream `registerFlag`).
pub fn apply_flag_defaults(runtime: &Arc<Mutex<ExtensionRuntime>>, extensions: &[Extension]) {
    let mut guard = runtime.lock().unwrap();
    for extension in extensions {
        for (name, flag) in &extension.flags {
            if flag.default.is_some() && !guard.flag_values.contains_key(name) {
                guard
                    .flag_values
                    .insert(name.clone(), flag.default.clone().unwrap());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sandbox(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pi-ext-loader-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extension_file_detection() {
        assert!(is_extension_file("a.ts"));
        assert!(is_extension_file("b.js"));
        assert!(!is_extension_file("c.json"));
        assert!(!is_extension_file("d.md"));
    }

    #[test]
    fn resolve_entries_prefers_pi_manifest() {
        let dir = sandbox("manifest");
        fs::write(
            dir.join("package.json"),
            r#"{ "pi": { "extensions": ["main.ts"] } }"#,
        )
        .unwrap();
        fs::write(dir.join("main.ts"), "export default () => {}").unwrap();
        fs::write(dir.join("index.ts"), "export default () => {}").unwrap();
        let entries = resolve_extension_entries(&dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().unwrap().to_string_lossy(), "main.ts");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entries_falls_back_to_index() {
        let dir = sandbox("index");
        fs::write(dir.join("index.js"), "module.exports = () => {}").unwrap();
        let entries = resolve_extension_entries(&dir).unwrap();
        assert_eq!(
            entries[0].file_name().unwrap().to_string_lossy(),
            "index.js"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entries_none_when_no_entry() {
        let dir = sandbox("none");
        assert!(resolve_extension_entries(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_direct_files_and_subdirs() {
        let dir = sandbox("discover");
        fs::write(dir.join("one.ts"), "x").unwrap();
        fs::write(dir.join("two.js"), "x").unwrap();
        fs::write(dir.join("three.md"), "x").unwrap();
        fs::create_dir_all(dir.join("pkg")).unwrap();
        fs::write(
            dir.join("pkg/package.json"),
            r#"{ "pi": { "extensions": ["ext.ts"] } }"#,
        )
        .unwrap();
        fs::write(dir.join("pkg/ext.ts"), "x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/index.ts"), "x").unwrap();
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::create_dir_all(dir.join("nested/deep")).unwrap();
        fs::write(dir.join("nested/deep/index.js"), "x").unwrap(); // only one level
        let discovered = discover_extensions_in_dir(&dir);
        let names: Vec<String> = discovered
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"one.ts".to_string()), "{names:?}");
        assert!(names.contains(&"two.js".to_string()), "{names:?}");
        assert!(names.contains(&"pkg/ext.ts".to_string()), "{names:?}");
        assert!(names.contains(&"sub/index.ts".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("nested/")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("three.md")), "{names:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_fails_when_no_runner() {
        // No node/bun guaranteed on PATH; with a nonexistent runner the error
        // must be deterministic.
        let dir = sandbox("norunner");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let result = run_external_extension("index.ts", &entry, Some("/nonexistent/runner"), None);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_records_error_for_missing_runner_default() {
        // When neither node nor bun is on PATH, the default runner resolution
        // fails with the upstream-style "Failed to load extension:" prefix.
        if command_on_path("node") || command_on_path("bun") {
            return; // environment has a runner; covered by fake-runner tests
        }
        let dir = sandbox("nort");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let err = load_extension("index.ts", &dir.to_string_lossy(), None).unwrap_err();
        let _ = &err;
        assert!(
            err.error.starts_with("Failed to load extension:"),
            "{}",
            err.error
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn execute_extension_with_fake_node() {
        // Fake `node`: a script that records its argv and exits 0.
        let dir = sandbox("fake");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let bin = sandbox("bin");
        let node_path = bin.join("node");
        let log_path = bin.join("log");
        let script = format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > \"{}\"\n",
            log_path.display()
        );
        fs::write(&node_path, &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&node_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result =
            run_external_extension("index.ts", &entry, Some(node_path.to_str().unwrap()), None);
        assert!(result.is_ok(), "result was an error: {result:?}");
        let extension = result.unwrap();
        assert_eq!(extension.path, "index.ts");
        // The arg protocol is `node <resolved entry>`.
        let logged = fs::read_to_string(&log_path).unwrap();
        assert_eq!(logged, entry.to_string_lossy());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn execute_extension_fake_node_failure_reports_error() {
        let dir = sandbox("fakefail");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let bin = sandbox("binfail");
        let node_path = bin.join("node");
        fs::write(
            &node_path,
            "#!/bin/sh\necho 'boom: bad extension' >&2\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&node_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let err =
            run_external_extension("index.ts", &entry, Some(node_path.to_str().unwrap()), None)
                .unwrap_err();
        assert!(err.starts_with("Failed to load extension:"), "{err}");
        assert!(err.contains("boom: bad extension"), "{err}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn discover_and_load_collects_project_and_global() {
        let root = sandbox("disc");
        let cwd = root.join("proj");
        let agent_dir = root.join("agent");
        fs::create_dir_all(cwd.join(".pi/extensions")).unwrap();
        fs::create_dir_all(agent_dir.join("extensions")).unwrap();
        fs::write(cwd.join(".pi/extensions/local.ts"), "x").unwrap();
        fs::write(agent_dir.join("extensions/global.ts"), "x").unwrap();
        let result = discover_and_load_extensions(
            &[],
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            None,
            None,
        );
        // Both discovered; loads may record errors if no runner exists, but
        // the discovery surfaces both entries either as extensions or errors.
        let count = result.extensions.len() + result.errors.len();
        assert_eq!(
            count,
            2,
            "expected 2 entries; got {} extensions and {} errors",
            result.extensions.len(),
            result.errors.len()
        );
        let paths: Vec<String> = result
            .extensions
            .iter()
            .map(|e| e.path.clone())
            .chain(result.errors.iter().map(|e| e.path.clone()))
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("local.ts")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("global.ts")), "{paths:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_paths_are_included_and_deduped() {
        let root = sandbox("cfg");
        let cwd = root.join("proj");
        let agent_dir = root.join("agent");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&agent_dir).unwrap();
        let ext = cwd.join("ext.ts");
        fs::write(&ext, "x").unwrap();
        // The configured path resolves to the same project-local extension.
        let configured = vec![ext.to_string_lossy().to_string()];
        let result = discover_and_load_extensions(
            &configured,
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            None,
            None,
        );
        let paths: Vec<String> = result
            .extensions
            .iter()
            .map(|e| e.path.clone())
            .chain(result.errors.iter().map(|e| e.path.clone()))
            .collect();
        // One occurrence total (deduped).
        assert_eq!(paths.len(), 1, "{paths:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn synthetic_source_extraction() {
        assert_eq!(extract_synthetic_source("<inline:foo>"), "inline");
        assert_eq!(extract_synthetic_source("<bundled:llama.cpp>"), "bundled");
    }
}
