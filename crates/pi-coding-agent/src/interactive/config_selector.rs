//! ConfigSelector resource model — port of
//! `packages/coding-agent/src/modes/interactive/components/config-selector.ts`
//! (the resolved-resource model + group/subgroup/item construction and
//! ordering). The component below also ports the interactive search,
//! navigation, scope switching, project override cycling, and persistence
//! behavior used by `pi config`.

use std::path::Path;

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
            .sort_by_key(|subgroup| subgroup.resource_type.order());
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
    search: pi_tui::components::Input,
    cwd: String,
    agent_dir: String,
    settings: crate::core::settings::SettingsManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectOverrideState {
    Inherit,
    Load,
    Unload,
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
        cwd: String,
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
            search: pi_tui::components::Input::new("Search: "),
            cwd,
            agent_dir,
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

    fn item_matches(&self, item: &ResourceItem) -> bool {
        let query = self.search.value.trim().to_lowercase();
        query.is_empty()
            || item.display_name.to_lowercase().contains(&query)
            || item.path.to_lowercase().contains(&query)
            || item.resource_type.as_str().contains(&query)
    }

    fn item_locations(&self) -> Vec<(usize, usize, usize)> {
        let mut locations = Vec::new();
        for (group_idx, group) in self.groups().iter().enumerate() {
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                for (item_idx, item) in subgroup.items.iter().enumerate() {
                    if self.item_matches(item) {
                        locations.push((group_idx, subgroup_idx, item_idx));
                    }
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

    fn selected_item(&self) -> Option<((usize, usize, usize), ResourceItem)> {
        let location = self.selected_location()?;
        let item = self
            .groups()
            .get(location.0)
            .and_then(|group| group.subgroups.get(location.1))
            .and_then(|subgroup| subgroup.items.get(location.2))
            .cloned()?;
        Some((location, item))
    }

    fn inherited_enabled(&self, item: &ResourceItem) -> bool {
        self.global_groups
            .iter()
            .flat_map(|group| group.subgroups.iter())
            .flat_map(|subgroup| subgroup.items.iter())
            .find(|candidate| same_resource(candidate, item))
            .map(|candidate| candidate.enabled)
            .unwrap_or(item.enabled)
    }

    fn is_inherited_global_item(&self, item: &ResourceItem) -> bool {
        item.metadata.scope == SourceScope::User
            || self
                .global_groups
                .iter()
                .flat_map(|group| group.subgroups.iter())
                .flat_map(|subgroup| subgroup.items.iter())
                .any(|candidate| same_resource(candidate, item))
    }

    fn toggle_selected(&mut self) {
        let Some((location, item)) = self.selected_item() else {
            return;
        };

        // Global mode edits global resources; project-local entries are only
        // writable after switching to project mode.
        if self.write_scope == ConfigWriteScope::Global && item.metadata.scope != SourceScope::User
        {
            return;
        }

        let enabled = match self.write_scope {
            ConfigWriteScope::Global => {
                let enabled = !item.enabled;
                if !self.set_resource_state(&item, enabled, ConfigWriteScope::Global) {
                    return;
                }
                enabled
            }
            ConfigWriteScope::Project => {
                let inherited = self.inherited_enabled(&item);
                let state = self.project_override_state(&item);
                let next = next_override_state(state, inherited);
                if !self.set_project_override(&item, next) {
                    return;
                }
                match next {
                    ProjectOverrideState::Inherit => inherited,
                    ProjectOverrideState::Load => true,
                    ProjectOverrideState::Unload => false,
                }
            }
        };

        // Settings setters enqueue writes. The config command can exit as
        // soon as the selector closes, so flush after each toggle as well as
        // on close to preserve the upstream persist-before-exit guarantee.
        self.settings.flush_sync();
        if let Some(group) = self.groups_mut().get_mut(location.0) {
            if let Some(subgroup) = group.subgroups.get_mut(location.1) {
                if let Some(item) = subgroup.items.get_mut(location.2) {
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
        settings
            .get(resource_type.as_str())
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

    fn project_override_state(&self, item: &ResourceItem) -> ProjectOverrideState {
        match item.metadata.origin {
            ResourceOrigin::TopLevel => override_state(
                &self.resource_paths(item.resource_type, ConfigWriteScope::Project),
                &self.top_level_override_patterns(item),
                false,
            ),
            ResourceOrigin::Package => {
                let Some(package) = self
                    .settings
                    .get_project_packages()
                    .into_iter()
                    .find(|package| package_source(package) == item.metadata.source)
                else {
                    return ProjectOverrideState::Inherit;
                };
                let (entries, empty_array_is_unload) = match package {
                    crate::core::settings::PackageSource::Str(_) => (Vec::new(), false),
                    crate::core::settings::PackageSource::Obj(package) => (
                        package_entries(&package, item.resource_type),
                        package.autoload == Some(false),
                    ),
                };
                override_state(
                    &entries,
                    &[resource_pattern_for_package(item)],
                    empty_array_is_unload,
                )
            }
        }
    }

    fn top_level_override_patterns(&self, item: &ResourceItem) -> Vec<String> {
        let project_base = self.top_level_base_dir(ConfigWriteScope::Project);
        vec![
            resource_pattern_for_scope(item, ConfigWriteScope::Project, &self.cwd, &self.agent_dir),
            item.path.clone(),
            relative_path(&project_base, &item.path),
        ]
    }

    fn set_project_override(&mut self, item: &ResourceItem, state: ProjectOverrideState) -> bool {
        match item.metadata.origin {
            ResourceOrigin::TopLevel => {
                let current = self.resource_paths(item.resource_type, ConfigWriteScope::Project);
                let inherited = self.is_inherited_global_item(item);
                let pattern = resource_pattern_for_scope(
                    item,
                    ConfigWriteScope::Project,
                    &self.cwd,
                    &self.agent_dir,
                );
                let patterns = self.top_level_override_patterns(item);
                let mut updated = current
                    .into_iter()
                    .filter(|entry| {
                        let target = pattern_target(entry);
                        let removes_override = (entry.starts_with('+')
                            || entry.starts_with('-')
                            || entry.starts_with('!'))
                            && patterns.iter().any(|candidate| candidate == target);
                        let removes_inherited_path = state == ProjectOverrideState::Inherit
                            && inherited
                            && target == pattern;
                        !(removes_override || removes_inherited_path)
                    })
                    .collect::<Vec<_>>();
                if state != ProjectOverrideState::Inherit {
                    if inherited && !updated.iter().any(|entry| entry == &item.path) {
                        updated.push(item.path.clone());
                    }
                    updated.push(format!("{}{}", state.prefix(), pattern));
                }
                self.set_resource_paths(item.resource_type, ConfigWriteScope::Project, updated);
                true
            }
            ResourceOrigin::Package => {
                self.set_package_state(item, state, ConfigWriteScope::Project)
            }
        }
    }

    fn set_resource_state(
        &mut self,
        item: &ResourceItem,
        enabled: bool,
        scope: ConfigWriteScope,
    ) -> bool {
        let state = if enabled {
            ProjectOverrideState::Load
        } else {
            ProjectOverrideState::Unload
        };
        match item.metadata.origin {
            ResourceOrigin::TopLevel => {
                let pattern = resource_pattern_for_scope(item, scope, &self.cwd, &self.agent_dir);
                let current = self.resource_paths(item.resource_type, scope);
                let updated = current
                    .into_iter()
                    .filter(|entry| pattern_target(entry) != pattern)
                    .chain(std::iter::once(format!("{}{}", state.prefix(), pattern)))
                    .collect();
                self.set_resource_paths(item.resource_type, scope, updated);
                true
            }
            ResourceOrigin::Package => self.set_package_state(item, state, scope),
        }
    }

    fn set_package_state(
        &mut self,
        item: &ResourceItem,
        state: ProjectOverrideState,
        scope: ConfigWriteScope,
    ) -> bool {
        let mut packages = match scope {
            ConfigWriteScope::Global => self.settings.get_packages(),
            ConfigWriteScope::Project => self.settings.get_project_packages(),
        };
        let package_index = packages
            .iter()
            .position(|package| package_source(package) == item.metadata.source);
        let package_index = match package_index {
            Some(index) => index,
            None if scope == ConfigWriteScope::Project
                && state != ProjectOverrideState::Inherit =>
            {
                packages.push(crate::core::settings::PackageSource::Obj(
                    crate::core::settings::PackageSourceObj {
                        source: item.metadata.source.clone(),
                        autoload: Some(false),
                        ..Default::default()
                    },
                ));
                packages.len() - 1
            }
            None => return false,
        };
        let package = match packages.get_mut(package_index) {
            Some(crate::core::settings::PackageSource::Str(source)) => {
                let source = source.clone();
                packages[package_index] = crate::core::settings::PackageSource::Obj(
                    crate::core::settings::PackageSourceObj {
                        source,
                        ..Default::default()
                    },
                );
                packages.get_mut(package_index).expect("package inserted")
            }
            Some(package) => package,
            None => return false,
        };
        let crate::core::settings::PackageSource::Obj(package) = package else {
            return false;
        };
        let pattern = resource_pattern_for_package(item);
        let entries = package_entries_mut(package, item.resource_type);
        entries.retain(|entry| pattern_target(entry) != pattern);
        if state != ProjectOverrideState::Inherit {
            entries.push(format!("{}{}", state.prefix(), pattern));
        }

        if scope == ConfigWriteScope::Global {
            self.settings.set_packages(packages);
        } else {
            self.settings.set_project_packages(packages);
        }
        true
    }

    fn top_level_base_dir(&self, scope: ConfigWriteScope) -> String {
        match scope {
            ConfigWriteScope::Global => self.agent_dir.clone(),
            ConfigWriteScope::Project => Path::new(&self.cwd)
                .join(crate::config::CONFIG_DIR_NAME)
                .to_string_lossy()
                .into_owned(),
        }
    }

    fn render_checkbox(&self, item: &ResourceItem) -> &'static str {
        if self.write_scope != ConfigWriteScope::Project {
            return if item.enabled { "[x]" } else { "[ ]" };
        }
        match self.project_override_state(item) {
            ProjectOverrideState::Load => "[+]",
            ProjectOverrideState::Unload => "[-]",
            ProjectOverrideState::Inherit if item.enabled => "[x]",
            ProjectOverrideState::Inherit => "[ ]",
        }
    }

    fn render_item_suffix(&self, item: &ResourceItem) -> String {
        if self.write_scope != ConfigWriteScope::Project {
            return String::new();
        }
        match self.project_override_state(item) {
            ProjectOverrideState::Load => "  project load".to_string(),
            ProjectOverrideState::Unload => "  project unload".to_string(),
            ProjectOverrideState::Inherit if self.is_inherited_global_item(item) => {
                "  inherited global".to_string()
            }
            ProjectOverrideState::Inherit => String::new(),
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
        let mut lines = vec![title.to_string(), scope_path];
        lines.extend(self.search.render(width));
        lines.push(String::new());
        let selected = self.selected_location();
        let mut content = Vec::new();
        let mut selected_line = None;
        for (group_idx, group) in self.groups().iter().enumerate() {
            let has_matching_item = group
                .subgroups
                .iter()
                .flat_map(|subgroup| subgroup.items.iter())
                .any(|item| self.item_matches(item));
            if !has_matching_item {
                continue;
            }
            let inherited = self.write_scope == ConfigWriteScope::Project && group.is_user();
            content.push(format!(
                "  {}{}",
                group.label,
                if inherited {
                    " · inherited global"
                } else {
                    ""
                }
            ));
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                let matching_items = subgroup
                    .items
                    .iter()
                    .filter(|item| self.item_matches(item))
                    .collect::<Vec<_>>();
                if matching_items.is_empty() {
                    continue;
                }
                content.push(format!("    {}", subgroup.label));
                for (item_idx, item) in subgroup.items.iter().enumerate() {
                    if !self.item_matches(item) {
                        continue;
                    }
                    let is_selected = selected == Some((group_idx, subgroup_idx, item_idx));
                    let cursor = if is_selected { ">" } else { " " };
                    if is_selected {
                        selected_line = Some(content.len());
                    }
                    let marker = self.render_checkbox(item);
                    let suffix = self.render_item_suffix(item);
                    let dimmed = self.write_scope == ConfigWriteScope::Project
                        && self.is_inherited_global_item(item)
                        && self.project_override_state(item) == ProjectOverrideState::Inherit;
                    let name = if dimmed {
                        format!("{} (inherited)", item.display_name)
                    } else {
                        item.display_name.clone()
                    };
                    content.push(format!("{cursor}       {marker} {name}{suffix}"));
                }
            }
        }

        if content.is_empty() {
            content.push("  No resources found".to_string());
        }
        let max_visible = 18;
        let content_len = content.len();
        let start = selected_line
            .unwrap_or(0)
            .saturating_sub(max_visible / 2)
            .min(content_len.saturating_sub(max_visible));
        if start > 0 {
            lines.push(format!("  ↑ more ({}/{})", start, content.len()));
        }
        lines.extend(content.into_iter().skip(start).take(max_visible));
        let visible_end = (start + max_visible).min(content_len);
        if visible_end < content_len {
            lines.push(format!("  ↓ more ({}/{})", visible_end, content_len));
        }
        lines.push(String::new());
        lines.push(
            "↑/↓ select · PgUp/PgDn page · Space toggle · Tab switch scope · Esc close".to_string(),
        );
        lines
            .into_iter()
            .map(|line| pi_tui::utils::truncate_to_width(&line, width, ""))
            .collect()
    }

    fn handle_input(&mut self, key: &pi_tui::keys::TuiKey) {
        let count = self.item_locations().len();
        match key.base.as_str() {
            "up" if count > 0 => {
                self.selected = if self.selected == 0 {
                    count - 1
                } else {
                    self.selected - 1
                };
            }
            "down" if count > 0 => {
                self.selected = (self.selected + 1) % count;
            }
            "pageup" if count > 0 => self.selected = self.selected.saturating_sub(8),
            "pagedown" if count > 0 => self.selected = (self.selected + 8).min(count - 1),
            "tab" if !self.project_groups.is_empty() => {
                self.write_scope = match self.write_scope {
                    ConfigWriteScope::Global => ConfigWriteScope::Project,
                    ConfigWriteScope::Project => ConfigWriteScope::Global,
                };
                self.selected = self
                    .selected
                    .min(self.item_locations().len().saturating_sub(1));
            }
            "esc" | "escape" => {
                self.settings.flush_sync();
                self.closed = true;
            }
            "enter" if !key.ctrl && !key.alt => self.toggle_selected(),
            " " => self.toggle_selected(),
            _ if key.ctrl && key.base == "c" => {
                self.settings.flush_sync();
                self.closed = true;
            }
            _ => {
                let before = self.search.value.clone();
                self.search.handle_input(key);
                if self.search.value != before {
                    self.selected = 0;
                }
            }
        }
    }
}

impl ProjectOverrideState {
    fn prefix(self) -> char {
        match self {
            ProjectOverrideState::Inherit => '+',
            ProjectOverrideState::Load => '+',
            ProjectOverrideState::Unload => '-',
        }
    }
}

fn same_resource(left: &ResourceItem, right: &ResourceItem) -> bool {
    left.resource_type == right.resource_type
        && normalize_path(&left.path) == normalize_path(&right.path)
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn relative_path(base: &str, path: &str) -> String {
    Path::new(path)
        .strip_prefix(base)
        .ok()
        .and_then(|relative| relative.to_str())
        .map(normalize_path)
        .unwrap_or_else(|| normalize_path(path))
}

fn resource_pattern_for_scope(
    item: &ResourceItem,
    target_scope: ConfigWriteScope,
    cwd: &str,
    agent_dir: &str,
) -> String {
    let source_scope = match item.metadata.scope {
        SourceScope::Project => ConfigWriteScope::Project,
        SourceScope::User | SourceScope::Temporary => ConfigWriteScope::Global,
    };
    if source_scope != target_scope {
        return normalize_path(&item.path);
    }
    let fallback = match source_scope {
        ConfigWriteScope::Global => agent_dir.to_string(),
        ConfigWriteScope::Project => Path::new(cwd)
            .join(crate::config::CONFIG_DIR_NAME)
            .to_string_lossy()
            .into_owned(),
    };
    relative_path(
        item.metadata.base_dir.as_deref().unwrap_or(&fallback),
        &item.path,
    )
}

fn resource_pattern_for_package(item: &ResourceItem) -> String {
    let base = item.metadata.base_dir.as_deref().or_else(|| {
        Path::new(&item.path)
            .parent()
            .and_then(|path| path.to_str())
    });
    base.map(|base| relative_path(base, &item.path))
        .unwrap_or_else(|| normalize_path(&item.path))
}

fn package_entries(
    package: &crate::core::settings::PackageSourceObj,
    resource_type: ResourceType,
) -> Vec<String> {
    match resource_type {
        ResourceType::Extensions => package.extensions.clone().unwrap_or_default(),
        ResourceType::Skills => package.skills.clone().unwrap_or_default(),
        ResourceType::Prompts => package.prompts.clone().unwrap_or_default(),
        ResourceType::Themes => package.themes.clone().unwrap_or_default(),
    }
}

fn package_entries_mut(
    package: &mut crate::core::settings::PackageSourceObj,
    resource_type: ResourceType,
) -> &mut Vec<String> {
    match resource_type {
        ResourceType::Extensions => package.extensions.get_or_insert_with(Vec::new),
        ResourceType::Skills => package.skills.get_or_insert_with(Vec::new),
        ResourceType::Prompts => package.prompts.get_or_insert_with(Vec::new),
        ResourceType::Themes => package.themes.get_or_insert_with(Vec::new),
    }
}

fn override_state(
    entries: &[String],
    patterns: &[String],
    empty_array_is_unload: bool,
) -> ProjectOverrideState {
    if entries.is_empty() && empty_array_is_unload {
        return ProjectOverrideState::Unload;
    }
    let mut state = ProjectOverrideState::Inherit;
    for entry in entries {
        if !patterns
            .iter()
            .any(|pattern| pattern == pattern_target(entry))
        {
            continue;
        }
        state = if entry.starts_with('!') || entry.starts_with('-') {
            ProjectOverrideState::Unload
        } else {
            ProjectOverrideState::Load
        };
    }
    state
}

fn next_override_state(
    state: ProjectOverrideState,
    inherited_enabled: bool,
) -> ProjectOverrideState {
    match state {
        ProjectOverrideState::Inherit if inherited_enabled => ProjectOverrideState::Unload,
        ProjectOverrideState::Inherit => ProjectOverrideState::Load,
        ProjectOverrideState::Unload if inherited_enabled => ProjectOverrideState::Load,
        ProjectOverrideState::Unload => ProjectOverrideState::Inherit,
        ProjectOverrideState::Load if inherited_enabled => ProjectOverrideState::Inherit,
        ProjectOverrideState::Load => ProjectOverrideState::Unload,
    }
}

fn pattern_target(value: &str) -> &str {
    value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .or_else(|| value.strip_prefix('!'))
        .unwrap_or(value)
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
    use pi_tui::Component;

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

    fn top_resource(path: &str, scope: SourceScope, enabled: bool) -> ResolvedResource {
        ResolvedResource {
            path: path.to_string(),
            enabled,
            metadata: PathMetadata::synthetic("auto", scope, ResourceOrigin::TopLevel, None),
        }
    }

    fn settings_with_global_extensions(paths: &[&str]) -> crate::core::settings::SettingsManager {
        let mut settings = crate::core::settings::SettingsMap::new();
        settings.insert(
            "extensions".into(),
            serde_json::Value::Array(
                paths
                    .iter()
                    .map(|path| serde_json::Value::String((*path).into()))
                    .collect(),
            ),
        );
        crate::core::settings::SettingsManager::in_memory(settings)
    }

    #[test]
    fn selector_filters_search_and_persists_global_toggle() {
        let mut selector = ConfigSelectorComponent::new(
            ResolvedPaths {
                extensions: vec![
                    top_resource("/agent/extensions/alpha.md", SourceScope::User, true),
                    top_resource("/agent/extensions/beta.md", SourceScope::User, true),
                ],
                ..Default::default()
            },
            ResolvedPaths::default(),
            settings_with_global_extensions(&["extensions/alpha.md", "extensions/beta.md"]),
            "/project".into(),
            "/agent".into(),
            "global",
        );

        let initial = selector.render(100).join("\n");
        assert!(initial.contains("Search:"));
        assert!(initial.contains("alpha.md"));
        assert!(initial.contains("beta.md"));

        selector.handle_input(&pi_tui::TuiKey::simple("b"));
        assert!(selector.render(100).join("\n").contains("beta.md"));
        assert!(!selector.render(100).join("\n").contains("alpha.md"));

        selector.handle_input(&pi_tui::TuiKey::simple(" "));
        let paths = selector.settings.get_global_settings();
        let paths = paths
            .get("extensions")
            .and_then(|value| value.as_array())
            .unwrap();
        assert!(paths.iter().any(|path| path == "-extensions/beta.md"));
    }

    #[test]
    fn project_scope_cycles_inherit_unload_load_and_back() {
        let mut selector = ConfigSelectorComponent::new(
            ResolvedPaths {
                extensions: vec![top_resource(
                    "/agent/extensions/alpha.md",
                    SourceScope::User,
                    true,
                )],
                ..Default::default()
            },
            ResolvedPaths {
                extensions: vec![top_resource(
                    "/agent/extensions/alpha.md",
                    SourceScope::User,
                    true,
                )],
                ..Default::default()
            },
            settings_with_global_extensions(&["extensions/alpha.md"]),
            "/project".into(),
            "/agent".into(),
            "project",
        );

        selector.handle_input(&pi_tui::TuiKey::simple(" "));
        assert!(selector.render(100).join("\n").contains("[-]"));
        selector.handle_input(&pi_tui::TuiKey::simple(" "));
        assert!(selector.render(100).join("\n").contains("[+]"));
        selector.handle_input(&pi_tui::TuiKey::simple(" "));
        assert!(selector.render(100).join("\n").contains("[x]"));

        let project = selector.settings.get_project_settings();
        let paths = project
            .get("extensions")
            .and_then(|value| value.as_array())
            .map_or(&[][..], |value| value.as_slice());
        assert!(paths.is_empty(), "inherit should remove project overrides");
    }

    #[test]
    fn render_snapshots_cover_global_and_project_override_rows() {
        let resource = ResolvedResource {
            path: "/agent/extensions/alpha.md".into(),
            enabled: true,
            metadata: PathMetadata::synthetic(
                "auto",
                SourceScope::User,
                ResourceOrigin::TopLevel,
                Some("/agent".into()),
            ),
        };
        let mut selector = ConfigSelectorComponent::new(
            ResolvedPaths {
                extensions: vec![resource.clone()],
                ..Default::default()
            },
            ResolvedPaths {
                extensions: vec![resource],
                ..Default::default()
            },
            settings_with_global_extensions(&["extensions/alpha.md"]),
            "/project".into(),
            "/agent".into(),
            "global",
        );

        assert_eq!(
            selector.render(100),
            vec![
                "Global Resources",
                "~/.pi/agent/settings.json",
                "Search: ",
                "",
                "  User (/agent/)",
                "    Extensions",
                ">       [x] alpha.md",
                "",
                "↑/↓ select · PgUp/PgDn page · Space toggle · Tab switch scope · Esc close",
            ]
        );

        selector.handle_input(&pi_tui::TuiKey::simple("tab"));
        selector.handle_input(&pi_tui::TuiKey::simple(" "));
        assert_eq!(
            selector.render(100),
            vec![
                "Project Local Resources",
                ".pi/settings.json",
                "Search: ",
                "",
                "  User (/agent/) · inherited global",
                "    Extensions",
                ">       [-] alpha.md  project unload",
                "",
                "↑/↓ select · PgUp/PgDn page · Space toggle · Tab switch scope · Esc close",
            ]
        );
    }
}
