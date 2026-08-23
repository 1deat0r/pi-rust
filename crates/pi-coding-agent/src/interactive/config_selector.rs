//! ConfigSelector resource model — port of
//! `packages/coding-agent/src/modes/interactive/components/config-selector.ts`
//! (the data layer: resolved-resource model + group/subgroup/item construction
//! and ordering). The interactive `render`/`handleInput` surfaces land with the
//! full TUI component wiring; this module owns the behavioral content logic.

/// Origin scope of a resource's `PathMetadata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

impl SourceScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceScope::User => "user",
            SourceScope::Project => "project",
            SourceScope::Temporary => "temporary",
        }
    }
}

/// Origin of a resolved resource: shipped in a package vs a top-level dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOrigin {
    Package,
    TopLevel,
}

impl ResourceOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceOrigin::Package => "package",
            ResourceOrigin::TopLevel => "top-level",
        }
    }
}

/// `PathMetadata` — where a resolved resource came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMetadata {
    pub source: String,
    pub scope: SourceScope,
    pub origin: ResourceOrigin,
    pub base_dir: Option<String>,
}

impl PathMetadata {
    pub fn synthetic(
        source: &str,
        scope: SourceScope,
        origin: ResourceOrigin,
        base_dir: Option<String>,
    ) -> Self {
        Self {
            source: source.to_string(),
            scope,
            origin,
            base_dir,
        }
    }
}

/// `ResolvedResource` — one discovered resource with its enabled state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    pub path: String,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

/// `ResolvedPaths` — the per-type resource collections the selector shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

/// One of the four configurable resource types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ResourceType::Extensions => "Extensions",
            ResourceType::Skills => "Skills",
            ResourceType::Prompts => "Prompts",
            ResourceType::Themes => "Themes",
        }
    }

    /// Stable ordering used to sort subgroups (upstream `typeOrder`).
    pub fn order(&self) -> u8 {
        match self {
            ResourceType::Extensions => 0,
            ResourceType::Skills => 1,
            ResourceType::Prompts => 2,
            ResourceType::Themes => 3,
        }
    }
}

/// A single selectable resource row within a subgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceItem {
    pub path: String,
    pub enabled: bool,
    pub metadata: PathMetadata,
    pub resource_type: ResourceType,
    pub display_name: String,
}

/// A per-type collection of items inside a group (e.g. "Extensions").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSubgroup {
    pub resource_type: ResourceType,
    pub label: String,
    pub items: Vec<ResourceItem>,
}

/// A group of resources sharing `origin:scope:source:baseDir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceGroup {
    pub key: String,
    pub label: String,
    pub scope: SourceScope,
    pub origin: ResourceOrigin,
    pub source: String,
    pub subgroups: Vec<ResourceSubgroup>,
}

impl ResourceGroup {
    /// Whether this group's scope is `User` (drives "inherited global"
    /// dimming in project mode).
    pub fn is_user(&self) -> bool {
        self.scope == SourceScope::User
    }
}

/// Format a base dir for display: home-relative with `~` and a trailing slash
/// (upstream `formatBaseDir`).
pub fn format_base_dir(base_dir: &str, home: Option<&str>) -> String {
    let normalized = base_dir.replace('\\', "/");
    let display = match home {
        Some(home) if base_dir == home => "~".to_string(),
        Some(home) if base_dir.starts_with(home) => {
            format!("~{}", &normalized[home.len()..])
        }
        _ => normalized,
    };
    if display.ends_with('/') {
        display
    } else {
        format!("{display}/")
    }
}

fn get_group_label(
    metadata: &PathMetadata,
    agent_dir: &str,
    config_dir_name: &str,
    home: Option<&str>,
) -> String {
    match metadata.origin {
        ResourceOrigin::Package => format!("{} ({})", metadata.source, metadata.scope.as_str()),
        ResourceOrigin::TopLevel => {
            if metadata.source == "auto" {
                match metadata.base_dir {
                    Some(ref base) => match metadata.scope {
                        SourceScope::User => format!("User ({})", format_base_dir(base, home)),
                        SourceScope::Project => {
                            format!("Project ({})", format_base_dir(base, home))
                        }
                        SourceScope::Temporary => "Temporary".to_string(),
                    },
                    None => match metadata.scope {
                        SourceScope::User => format!("User ({})", format_base_dir(agent_dir, home)),
                        SourceScope::Project => format!("Project ({config_dir_name}/)"),
                        SourceScope::Temporary => "Temporary".to_string(),
                    },
                }
            } else {
                match metadata.scope {
                    SourceScope::User => "User settings".to_string(),
                    SourceScope::Project => "Project settings".to_string(),
                    SourceScope::Temporary => "Temporary".to_string(),
                }
            }
        }
    }
}

fn display_name_for(resource_type: ResourceType, path: &str) -> String {
    let file_name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent_folder = std::path::Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match resource_type {
        ResourceType::Extensions if parent_folder != "extensions" => {
            format!("{parent_folder}/{file_name}")
        }
        ResourceType::Skills if file_name == "SKILL.md" => parent_folder,
        _ => file_name,
    }
}

fn add_group_resources(
    groups: &mut Vec<ResourceGroup>,
    resources: &[ResolvedResource],
    resource_type: ResourceType,
    agent_dir: &str,
    config_dir_name: &str,
    home: Option<&str>,
) {
    for res in resources {
        let m = &res.metadata;
        let group_key = format!(
            "{}:{}:{}:{}",
            m.origin.as_str(),
            m.scope.as_str(),
            m.source,
            m.base_dir.as_deref().unwrap_or(""),
        );
        let group_idx = if let Some(idx) = groups.iter().position(|g| g.key == group_key) {
            idx
        } else {
            let label = get_group_label(m, agent_dir, config_dir_name, home);
            groups.push(ResourceGroup {
                key: group_key,
                label,
                scope: m.scope,
                origin: m.origin,
                source: m.source.clone(),
                subgroups: Vec::new(),
            });
            groups.len() - 1
        };

        let subgroup_idx = if let Some(idx) = groups[group_idx]
            .subgroups
            .iter()
            .position(|sg| sg.resource_type == resource_type)
        {
            idx
        } else {
            groups[group_idx].subgroups.push(ResourceSubgroup {
                resource_type,
                label: resource_type.label().to_string(),
                items: Vec::new(),
            });
            groups[group_idx].subgroups.len() - 1
        };

        groups[group_idx].subgroups[subgroup_idx]
            .items
            .push(ResourceItem {
                path: res.path.clone(),
                enabled: res.enabled,
                metadata: m.clone(),
                resource_type,
                display_name: display_name_for(resource_type, &res.path),
            });
    }
}

/// Build the selector's groups from resolved paths (upstream `buildGroups`):
/// group by origin/scope/source/baseDir, subgroup by resource type, item by
/// display name, with exact ordering (packages first, user before project,
/// then by source; subgroups by type; items by name).
pub fn build_groups(
    resolved: &ResolvedPaths,
    agent_dir: &str,
    config_dir_name: &str,
    home: Option<&str>,
) -> Vec<ResourceGroup> {
    let mut groups: Vec<ResourceGroup> = Vec::new();

    add_group_resources(
        &mut groups,
        &resolved.extensions,
        ResourceType::Extensions,
        agent_dir,
        config_dir_name,
        home,
    );
    add_group_resources(
        &mut groups,
        &resolved.skills,
        ResourceType::Skills,
        agent_dir,
        config_dir_name,
        home,
    );
    add_group_resources(
        &mut groups,
        &resolved.prompts,
        ResourceType::Prompts,
        agent_dir,
        config_dir_name,
        home,
    );
    add_group_resources(
        &mut groups,
        &resolved.themes,
        ResourceType::Themes,
        agent_dir,
        config_dir_name,
        home,
    );

    // Sort groups: packages first, then user before project, then by source.
    groups.sort_by(|a, b| {
        if a.origin != b.origin {
            return if a.origin == ResourceOrigin::Package {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        if a.scope != b.scope {
            return if a.scope == SourceScope::User {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.source.cmp(&b.source)
    });

    // Sort subgroups by type order, items by display name.
    for group in &mut groups {
        group
            .subgroups
            .sort_by(|a, b| a.resource_type.order().cmp(&b.resource_type.order()));
        for subgroup in &mut group.subgroups {
            subgroup
                .items
                .sort_by(|a, b| a.display_name.cmp(&b.display_name));
        }
    }

    groups
}

/// Interactive resource selector used by `pi config` when attached to a
/// terminal. The data model above remains independently testable; this
/// component owns only cursor navigation, scope switching, rendering, and
/// persistence of the selected enable/disable pattern.
pub struct ConfigSelectorComponent {
    global_groups: Vec<ResourceGroup>,
    project_groups: Vec<ResourceGroup>,
    write_scope: ConfigWriteScope,
    selected: usize,
    closed: bool,
    settings: crate::core::settings::SettingsManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigWriteScope {
    Global,
    Project,
}

impl ConfigSelectorComponent {
    pub fn new(
        global: ResolvedPaths,
        project: ResolvedPaths,
        settings: crate::core::settings::SettingsManager,
        _cwd: String,
        agent_dir: String,
        write_scope: &str,
    ) -> Self {
        let home = dirs::home_dir().map(|p| p.to_string_lossy().into_owned());
        let global_groups = build_groups(
            &global,
            &agent_dir,
            crate::config::CONFIG_DIR_NAME,
            home.as_deref(),
        );
        let project_groups = build_groups(
            &project,
            &agent_dir,
            crate::config::CONFIG_DIR_NAME,
            home.as_deref(),
        );
        Self {
            global_groups,
            project_groups,
            write_scope: if write_scope == "project" {
                ConfigWriteScope::Project
            } else {
                ConfigWriteScope::Global
            },
            selected: 0,
            closed: false,
            settings,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn groups(&self) -> &[ResourceGroup] {
        match self.write_scope {
            ConfigWriteScope::Global => &self.global_groups,
            ConfigWriteScope::Project => &self.project_groups,
        }
    }

    fn item_locations(&self) -> Vec<(usize, usize, usize)> {
        let mut locations = Vec::new();
        for (group_idx, group) in self.groups().iter().enumerate() {
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                for item_idx in 0..subgroup.items.len() {
                    locations.push((group_idx, subgroup_idx, item_idx));
                }
            }
        }
        locations
    }

    fn selected_location(&self) -> Option<(usize, usize, usize)> {
        let locations = self.item_locations();
        locations
            .get(self.selected.min(locations.len().saturating_sub(1)))
            .copied()
    }

    fn toggle_selected(&mut self) {
        let Some((group_idx, subgroup_idx, item_idx)) = self.selected_location() else {
            return;
        };
        let Some(item) = self
            .groups()
            .get(group_idx)
            .and_then(|group| group.subgroups.get(subgroup_idx))
            .and_then(|subgroup| subgroup.items.get(item_idx))
            .cloned()
        else {
            return;
        };

        let inherited = self
            .global_groups
            .iter()
            .flat_map(|group| group.subgroups.iter())
            .flat_map(|subgroup| subgroup.items.iter())
            .find(|candidate| {
                candidate.resource_type == item.resource_type && candidate.path == item.path
            })
            .map(|candidate| candidate.enabled)
            .unwrap_or(item.enabled);
        let enabled = match self.write_scope {
            ConfigWriteScope::Global => !item.enabled,
            ConfigWriteScope::Project => !item.enabled,
        };
        let pattern = resource_pattern(&item);
        let prefix = if enabled { '+' } else { '-' };
        let override_pattern = format!("{prefix}{pattern}");

        match item.metadata.origin {
            ResourceOrigin::TopLevel => {
                let paths = self.resource_paths(item.resource_type, self.write_scope);
                let mut updated = paths
                    .into_iter()
                    .filter(|entry| pattern_target(entry) != pattern)
                    .collect::<Vec<_>>();
                if self.write_scope == ConfigWriteScope::Project
                    && inherited != item.enabled
                    && pattern_target(&item.path) != pattern
                {
                    updated.push(item.path.clone());
                }
                updated.push(override_pattern);
                self.set_resource_paths(item.resource_type, self.write_scope, updated);
            }
            ResourceOrigin::Package => self.toggle_package_resource(&item, &override_pattern),
        }

        if let Some(group) = self.groups_mut().get_mut(group_idx) {
            if let Some(subgroup) = group.subgroups.get_mut(subgroup_idx) {
                if let Some(item) = subgroup.items.get_mut(item_idx) {
                    item.enabled = enabled;
                }
            }
        }
    }

    fn groups_mut(&mut self) -> &mut Vec<ResourceGroup> {
        match self.write_scope {
            ConfigWriteScope::Global => &mut self.global_groups,
            ConfigWriteScope::Project => &mut self.project_groups,
        }
    }

    fn resource_paths(&self, resource_type: ResourceType, scope: ConfigWriteScope) -> Vec<String> {
        let settings = match scope {
            ConfigWriteScope::Global => self.settings.get_global_settings(),
            ConfigWriteScope::Project => self.settings.get_project_settings(),
        };
        let key = resource_type.as_str();
        settings
            .get(key)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }

    fn set_resource_paths(
        &mut self,
        resource_type: ResourceType,
        scope: ConfigWriteScope,
        paths: Vec<String>,
    ) {
        match (scope, resource_type) {
            (ConfigWriteScope::Global, ResourceType::Extensions) => {
                self.settings.set_extension_paths(paths)
            }
            (ConfigWriteScope::Global, ResourceType::Skills) => {
                self.settings.set_skill_paths(paths)
            }
            (ConfigWriteScope::Global, ResourceType::Prompts) => {
                self.settings.set_prompt_template_paths(paths)
            }
            (ConfigWriteScope::Global, ResourceType::Themes) => {
                self.settings.set_theme_paths(paths)
            }
            (ConfigWriteScope::Project, ResourceType::Extensions) => {
                self.settings.set_project_extension_paths(paths)
            }
            (ConfigWriteScope::Project, ResourceType::Skills) => {
                self.settings.set_project_skill_paths(paths)
            }
            (ConfigWriteScope::Project, ResourceType::Prompts) => {
                self.settings.set_project_prompt_template_paths(paths)
            }
            (ConfigWriteScope::Project, ResourceType::Themes) => {
                self.settings.set_project_theme_paths(paths)
            }
        }
    }

    fn toggle_package_resource(&mut self, item: &ResourceItem, override_pattern: &str) {
        let packages = match self.write_scope {
            ConfigWriteScope::Global => self.settings.get_packages(),
            ConfigWriteScope::Project => self.settings.get_project_packages(),
        };
        let mut packages = packages;
        let Some(package) = packages
            .iter_mut()
            .find(|package| package_source(package) == item.metadata.source)
        else {
            return;
        };
        let package = match package {
            crate::core::settings::PackageSource::Str(source) => {
                let source = source.clone();
                *package = crate::core::settings::PackageSource::Obj(
                    crate::core::settings::PackageSourceObj {
                        source,
                        autoload: None,
                        ..Default::default()
                    },
                );
                package
            }
            crate::core::settings::PackageSource::Obj(_) => package,
        };
        let crate::core::settings::PackageSource::Obj(package) = package else {
            return;
        };
        let entries = match item.resource_type {
            ResourceType::Extensions => &mut package.extensions,
            ResourceType::Skills => &mut package.skills,
            ResourceType::Prompts => &mut package.prompts,
            ResourceType::Themes => &mut package.themes,
        };
        let pattern = resource_pattern(item);
        entries
            .get_or_insert_with(Vec::new)
            .retain(|entry| pattern_target(entry) != pattern);
        entries
            .get_or_insert_with(Vec::new)
            .push(override_pattern.to_string());
        match self.write_scope {
            ConfigWriteScope::Global => self.settings.set_packages(packages),
            ConfigWriteScope::Project => self.settings.set_project_packages(packages),
        }
    }
}

impl pi_tui::Component for ConfigSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let title = match self.write_scope {
            ConfigWriteScope::Global => "Global Resources",
            ConfigWriteScope::Project => "Project Local Resources",
        };
        let scope_path = match self.write_scope {
            ConfigWriteScope::Global => {
                format!("~/{}/agent/settings.json", crate::config::CONFIG_DIR_NAME)
            }
            ConfigWriteScope::Project => {
                format!("{}/settings.json", crate::config::CONFIG_DIR_NAME)
            }
        };
        let mut lines = vec![title.to_string(), scope_path, String::new()];
        let selected = self.selected_location();
        for (group_idx, group) in self.groups().iter().enumerate() {
            let inherited = self.write_scope == ConfigWriteScope::Project && group.is_user();
            lines.push(format!(
                "  {}{}",
                group.label,
                if inherited {
                    " · inherited global"
                } else {
                    ""
                }
            ));
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                lines.push(format!("    {}", subgroup.label));
                for (item_idx, item) in subgroup.items.iter().enumerate() {
                    let is_selected = selected == Some((group_idx, subgroup_idx, item_idx));
                    let cursor = if is_selected { ">" } else { " " };
                    let marker = if item.enabled { "[x]" } else { "[ ]" };
                    lines.push(format!("{cursor}       {marker} {}", item.display_name));
                }
            }
        }
        lines.push(String::new());
        lines.push("↑/↓ select · Space toggle · Tab switch scope · Esc close".to_string());
        lines
            .into_iter()
            .map(|line| pi_tui::utils::truncate_to_width(&line, width, ""))
            .collect()
    }

    fn handle_input(&mut self, key: &pi_tui::keys::TuiKey) {
        let count = self.item_locations().len();
        match key.base.as_str() {
            "up" => self.selected = self.selected.saturating_sub(1),
            "down" => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            "pageup" => self.selected = self.selected.saturating_sub(8),
            "pagedown" => self.selected = (self.selected + 8).min(count.saturating_sub(1)),
            "tab" if !self.project_groups.is_empty() => {
                self.write_scope = match self.write_scope {
                    ConfigWriteScope::Global => ConfigWriteScope::Project,
                    ConfigWriteScope::Project => ConfigWriteScope::Global,
                };
                self.selected = self
                    .selected
                    .min(self.item_locations().len().saturating_sub(1));
            }
            "esc" | "escape" | "q" => self.closed = true,
            "enter" if !key.ctrl && !key.alt => self.toggle_selected(),
            " " => self.toggle_selected(),
            _ => {}
        }
    }
}

fn pattern_target(value: &str) -> &str {
    value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .or_else(|| value.strip_prefix('!'))
        .unwrap_or(value)
}

fn resource_pattern(item: &ResourceItem) -> String {
    if item.metadata.origin == ResourceOrigin::Package {
        let base = item.metadata.base_dir.as_deref().unwrap_or_else(|| {
            std::path::Path::new(&item.path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
        });
        std::path::Path::new(&item.path)
            .strip_prefix(base)
            .ok()
            .and_then(|p| p.to_str())
            .unwrap_or(&item.path)
            .replace('\\', "/")
    } else {
        item.path.clone()
    }
}

fn package_source(source: &crate::core::settings::PackageSource) -> &str {
    match source {
        crate::core::settings::PackageSource::Str(source) => source,
        crate::core::settings::PackageSource::Obj(source) => &source.source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_pkg(path: &str, enabled: bool, source: &str, base: Option<&str>) -> ResolvedResource {
        ResolvedResource {
            path: path.to_string(),
            enabled,
            metadata: PathMetadata::synthetic(
                source,
                SourceScope::User,
                ResourceOrigin::Package,
                base.map(String::from),
            ),
        }
    }

    fn user_top(path: &str, enabled: bool) -> ResolvedResource {
        ResolvedResource {
            path: path.to_string(),
            enabled,
            metadata: PathMetadata::synthetic(
                "auto",
                SourceScope::User,
                ResourceOrigin::TopLevel,
                None,
            ),
        }
    }

    #[test]
    fn build_groups_sorts_packages_before_top_level_user_before_project() {
        let resolved = ResolvedPaths {
            extensions: vec![
                user_top("/home/u/.pi/agent/extensions/hook.md", true),
                user_pkg(
                    "/home/u/.pi/agent/extensions/pkg/ext.md",
                    true,
                    "npm:x",
                    Some("/home/u/.pi/agent/extensions/pkg".into()),
                ),
                ResolvedResource {
                    path: "/home/u/.pi/proj/.pi/extensions/proj.md".into(),
                    enabled: false,
                    metadata: PathMetadata::synthetic(
                        "auto",
                        SourceScope::Project,
                        ResourceOrigin::TopLevel,
                        None,
                    ),
                },
            ],
            ..Default::default()
        };
        let groups = build_groups(&resolved, "/home/u/.pi/agent", ".pi", Some("/home/u"));
        // Package group first, then user top-level, then project top-level.
        assert_eq!(groups[0].origin, ResourceOrigin::Package);
        assert_eq!(groups[1].origin, ResourceOrigin::TopLevel);
        assert_eq!(groups[1].scope, SourceScope::User);
        assert_eq!(groups[2].scope, SourceScope::Project);
    }

    #[test]
    fn group_label_for_package_and_top_level() {
        let resolved = ResolvedPaths {
            extensions: vec![
                user_pkg("/p/ext.md", true, "npm:foo", Some("/p".into())),
                user_top("/home/u/.pi/agent/extensions/a.md", true),
            ],
            ..Default::default()
        };
        let groups = build_groups(&resolved, "/home/u/.pi/agent", ".pi", Some("/home/u"));
        assert_eq!(groups[0].label, "npm:foo (user)");
        assert_eq!(groups[1].label, "User (~/.pi/agent/)");
    }

    #[test]
    fn display_names_for_extensions_and_skill_dir() {
        let resolved = ResolvedPaths {
            extensions: vec![user_top("/p/extensions/plain.md", true)],
            skills: vec![ResolvedResource {
                path: "/p/skills/my-skill/SKILL.md".into(),
                enabled: true,
                metadata: PathMetadata::synthetic(
                    "auto",
                    SourceScope::User,
                    ResourceOrigin::TopLevel,
                    None,
                ),
            }],
            ..Default::default()
        };
        let groups = build_groups(&resolved, "/p", ".pi", None);
        // extensions child of a dir named "extensions" -> file name only.
        let ext_item = &groups[0].subgroups[0].items[0];
        assert_eq!(ext_item.display_name, "plain.md");
        // skills with SKILL.md -> parent dir name.
        let skill_item = &groups[0].subgroups[1].items[0];
        assert_eq!(skill_item.display_name, "my-skill");
    }

    #[test]
    fn subgroups_ordered_by_type_and_items_by_name() {
        let resolved = ResolvedPaths {
            extensions: vec![
                user_top("/p/extensions/ext.md", true),
                user_top("/p/extensions/zext.md", true),
            ],
            themes: vec![user_top("/p/themes/theme.json", true)],
            skills: vec![user_top("/p/skills/sk.md", true)],
            prompts: vec![user_top("/p/prompts/pr.md", true)],
            ..Default::default()
        };
        let groups = build_groups(&resolved, "/p", ".pi", None);
        assert_eq!(groups.len(), 1);
        let kinds: Vec<&str> = groups[0]
            .subgroups
            .iter()
            .map(|s| s.resource_type.as_str())
            .collect();
        assert_eq!(kinds, vec!["extensions", "skills", "prompts", "themes"]);
        let ext_names: Vec<&str> = groups[0].subgroups[0]
            .items
            .iter()
            .map(|i| i.display_name.as_str())
            .collect();
        assert_eq!(ext_names, vec!["ext.md", "zext.md"]);
    }

    #[test]
    fn format_base_dir_home_relative() {
        assert_eq!(
            format_base_dir("/home/u/.pi/agent", Some("/home/u")),
            "~/.pi/agent/"
        );
        assert_eq!(format_base_dir("/home/u", Some("/home/u")), "~/");
        assert_eq!(format_base_dir("/opt/x", Some("/home/u")), "/opt/x/");
        assert_eq!(format_base_dir("/p", None), "/p/");
    }
}
