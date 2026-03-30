use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TabId(usize);

/// Default poll interval (in seconds) for refocusing panes to pick up title
/// changes. Zellij's PaneUpdate event doesn't fire on title-only changes,
/// so we periodically refocus to trigger a fresh PaneUpdate.
/// The CwdChanged event handles the most common case (shell directory changes),
/// so this timer is primarily a fallback for programs that set their own title
/// (e.g., vim, htop) without a CWD change.
/// Override via plugin configuration: `poll_interval_secs "2.0"`.
const DEFAULT_POLL_INTERVAL_SECS: f64 = 2.0;

struct State {
    tabs: Vec<TabInfo>,
    pane_manifest: PaneManifest,
    permissions_granted: bool,
    /// Cache: tab_id -> last_set_name (avoid redundant rename calls)
    last_set_names: HashMap<TabId, String>,
    /// Track focused terminal pane IDs per tab_id for timer-based refocus
    focused_pane_ids: HashMap<TabId, u32>,
    /// Configurable poll interval (seconds) for timer-based refocus
    poll_interval_secs: f64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            pane_manifest: PaneManifest::default(),
            permissions_granted: false,
            last_set_names: HashMap::new(),
            focused_pane_ids: HashMap::new(),
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
        }
    }
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.apply_configuration(&configuration);

        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);
        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::PermissionRequestResult,
            EventType::Timer,
            EventType::CwdChanged,
            EventType::PluginConfigurationChanged,
        ]);
        set_selectable(false);
        set_timeout(self.poll_interval_secs);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(status) => {
                self.permissions_granted = status == PermissionStatus::Granted;
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                self.prune_stale_cache_entries();
                self.rename_tabs();
            }
            Event::PaneUpdate(manifest) => {
                self.focused_pane_ids = Self::extract_focused_pane_ids(&manifest, &self.tabs);
                self.pane_manifest = manifest;
                self.rename_tabs();
            }
            Event::CwdChanged(_pane_id, _path, _client_ids) => {
                self.refocus_active_pane();
            }
            Event::PluginConfigurationChanged(config) => {
                self.apply_configuration(&config);
            }
            Event::Timer(_elapsed) => {
                self.refocus_active_pane();
                set_timeout(self.poll_interval_secs);
            }
            _ => {}
        }
        false
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        // Required by ZellijPlugin trait but never called (update() returns false).
    }
}

fn is_focused_terminal_pane(pane: &PaneInfo) -> bool {
    pane.is_focused && !pane.is_plugin && !pane.is_suppressed
}

impl State {
    /// Parse plugin configuration from the KDL config block.
    fn apply_configuration(&mut self, configuration: &BTreeMap<String, String>) {
        self.poll_interval_secs = configuration
            .get("poll_interval_secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    }

    fn prune_stale_cache_entries(&mut self) {
        let active_tab_ids: HashSet<TabId> = self.tabs.iter().map(|t| TabId(t.tab_id)).collect();
        self.last_set_names
            .retain(|id, _| active_tab_ids.contains(id));
        self.focused_pane_ids
            .retain(|id, _| active_tab_ids.contains(id));
    }

    fn rename_tabs(&mut self) {
        let renames = self.compute_renames();
        for (tab_id, name) in renames {
            rename_tab_with_id(tab_id.0 as u64, &name);
            self.last_set_names.insert(tab_id, name);
        }
    }

    /// Compute which tabs need renaming.
    /// Returns a vec of (tab_id, desired name).
    fn compute_renames(&self) -> Vec<(TabId, String)> {
        if !self.permissions_granted {
            return vec![];
        }

        let mut renames = Vec::new();
        for tab in &self.tabs {
            if let Some(desired_name) = self.find_focused_pane_title(tab.position) {
                if desired_name.is_empty() {
                    continue;
                }

                let id = TabId(tab.tab_id);
                let already_set = self
                    .last_set_names
                    .get(&id)
                    .is_some_and(|n| n == &desired_name);

                if !already_set && tab.name != desired_name {
                    renames.push((id, desired_name));
                }
            }
        }
        renames
    }

    /// Refocus the active tab's pane to trigger a fresh PaneUpdate.
    /// Used by both CwdChanged (shell directory change) and Timer (fallback for
    /// programs like vim/htop that set their own title without a CWD change).
    fn refocus_active_pane(&self) {
        if let Some(pane_id) = self.active_tab_pane_to_refocus() {
            focus_terminal_pane(pane_id, false, false);
        }
    }

    /// Return the pane ID to refocus for title polling (only the active tab).
    /// Returns `None` when floating panes are visible (e.g., the help window
    /// opened via Ctrl+/) so that `focus_terminal_pane` doesn't steal focus
    /// and close them.
    fn active_tab_pane_to_refocus(&self) -> Option<u32> {
        let active_tab = self.tabs.iter().find(|t| t.active)?;
        if active_tab.are_floating_panes_visible {
            return None;
        }
        self.focused_pane_ids.get(&TabId(active_tab.tab_id)).copied()
    }

    /// Extract focused terminal pane IDs from a pane manifest, keyed by tab_id.
    fn extract_focused_pane_ids(manifest: &PaneManifest, tabs: &[TabInfo]) -> HashMap<TabId, u32> {
        let pos_to_id: HashMap<usize, TabId> = tabs.iter().map(|t| (t.position, TabId(t.tab_id))).collect();

        manifest
            .panes
            .iter()
            .flat_map(|(tab_pos, panes)| {
                let tab_id = pos_to_id.get(tab_pos).copied();
                panes
                    .iter()
                    .filter(|p| is_focused_terminal_pane(p))
                    .filter_map(move |p| tab_id.map(|id| (id, p.id)))
            })
            .collect()
    }

    /// Find the title of the focused non-plugin, non-suppressed pane in a given tab.
    fn find_focused_pane_title(&self, tab_position: usize) -> Option<String> {
        let panes = self.pane_manifest.panes.get(&tab_position)?;
        panes
            .iter()
            .find(|p| is_focused_terminal_pane(p))
            .map(|p| p.title.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pane(id: u32, title: &str, is_focused: bool) -> PaneInfo {
        PaneInfo {
            id,
            title: title.to_string(),
            is_focused,
            ..Default::default()
        }
    }

    fn make_plugin_pane(id: u32, title: &str, is_focused: bool) -> PaneInfo {
        PaneInfo {
            is_plugin: true,
            ..make_pane(id, title, is_focused)
        }
    }

    fn make_suppressed_pane(id: u32, title: &str, is_focused: bool) -> PaneInfo {
        PaneInfo {
            is_suppressed: true,
            ..make_pane(id, title, is_focused)
        }
    }

    fn make_tab(position: usize, name: &str, tab_id: usize) -> TabInfo {
        TabInfo {
            position,
            name: name.to_string(),
            tab_id,
            ..Default::default()
        }
    }

    fn make_active_tab(position: usize, name: &str, tab_id: usize) -> TabInfo {
        TabInfo {
            active: true,
            ..make_tab(position, name, tab_id)
        }
    }

    fn make_active_tab_with_floating(position: usize, name: &str, tab_id: usize) -> TabInfo {
        TabInfo {
            are_floating_panes_visible: true,
            ..make_active_tab(position, name, tab_id)
        }
    }

    fn make_manifest(entries: Vec<(usize, Vec<PaneInfo>)>) -> PaneManifest {
        PaneManifest {
            panes: entries.into_iter().collect(),
        }
    }

    // ── apply_configuration ─────────────────────────────────────────────

    #[test]
    fn apply_configuration_sets_poll_interval_from_valid_float() {
        // Arrange
        let mut state = State::default();
        let config = BTreeMap::from([("poll_interval_secs".to_string(), "3.5".to_string())]);

        // Act
        state.apply_configuration(&config);

        // Assert
        assert_eq!(state.poll_interval_secs, 3.5);
    }

    #[test]
    fn apply_configuration_uses_default_when_key_missing() {
        // Arrange
        let mut state = State::default();
        let config = BTreeMap::new();

        // Act
        state.apply_configuration(&config);

        // Assert
        assert_eq!(state.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
    }

    #[test]
    fn apply_configuration_uses_default_for_non_numeric_value() {
        // Arrange
        let mut state = State::default();
        let config = BTreeMap::from([("poll_interval_secs".to_string(), "not-a-number".to_string())]);

        // Act
        state.apply_configuration(&config);

        // Assert
        assert_eq!(state.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
    }

    #[test]
    fn apply_configuration_uses_default_for_empty_value() {
        // Arrange
        let mut state = State::default();
        let config = BTreeMap::from([("poll_interval_secs".to_string(), "".to_string())]);

        // Act
        state.apply_configuration(&config);

        // Assert
        assert_eq!(state.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
    }

    #[test]
    fn apply_configuration_ignores_unrelated_keys() {
        // Arrange
        let mut state = State::default();
        let config = BTreeMap::from([("unrelated_key".to_string(), "42".to_string())]);

        // Act
        state.apply_configuration(&config);

        // Assert
        assert_eq!(state.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
    }

    // ── find_focused_pane_title ──────────────────────────────────────────

    #[test]
    fn find_focused_pane_title_returns_none_for_unknown_tab() {
        // Arrange
        let state = State {
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "shell", true)])]),
            ..Default::default()
        };

        // Act
        let result = state.find_focused_pane_title(99);

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn find_focused_pane_title_returns_none_when_no_pane_is_focused() {
        // Arrange
        let state = State {
            pane_manifest: make_manifest(vec![(
                0,
                vec![
                    make_pane(1, "pane-a", false),
                    make_pane(2, "pane-b", false),
                ],
            )]),
            ..Default::default()
        };

        // Act
        let result = state.find_focused_pane_title(0);

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn find_focused_pane_title_returns_title_of_focused_pane() {
        // Arrange
        let state = State {
            pane_manifest: make_manifest(vec![(
                0,
                vec![make_pane(1, "unfocused", false), make_pane(2, "vim", true)],
            )]),
            ..Default::default()
        };

        // Act
        let result = state.find_focused_pane_title(0);

        // Assert
        assert_eq!(result, Some("vim".to_string()));
    }

    #[test]
    fn find_focused_pane_title_ignores_plugin_panes() {
        // Arrange
        let state = State {
            pane_manifest: make_manifest(vec![(
                0,
                vec![
                    make_plugin_pane(1, "status-bar", true),
                    make_pane(2, "shell", false),
                ],
            )]),
            ..Default::default()
        };

        // Act
        let result = state.find_focused_pane_title(0);

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn find_focused_pane_title_ignores_suppressed_panes() {
        // Arrange
        let state = State {
            pane_manifest: make_manifest(vec![(
                0,
                vec![
                    make_suppressed_pane(1, "hidden", true),
                    make_pane(2, "shell", false),
                ],
            )]),
            ..Default::default()
        };

        // Act
        let result = state.find_focused_pane_title(0);

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn find_focused_pane_title_picks_terminal_pane_over_plugin() {
        // Arrange
        let state = State {
            pane_manifest: make_manifest(vec![(
                0,
                vec![
                    make_plugin_pane(1, "tab-bar", true),
                    make_pane(2, "htop", true),
                    make_plugin_pane(3, "zjtabname", false),
                ],
            )]),
            ..Default::default()
        };

        // Act
        let result = state.find_focused_pane_title(0);

        // Assert
        assert_eq!(result, Some("htop".to_string()));
    }

    // ── compute_renames ──────────────────────────────────────────────────

    #[test]
    fn compute_renames_returns_empty_when_permissions_not_granted() {
        // Arrange
        let state = State {
            permissions_granted: false,
            tabs: vec![make_tab(0, "Tab 1", 100)],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "shell", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert!(renames.is_empty());
    }

    #[test]
    fn compute_renames_renames_tab_to_focused_pane_title() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "Tab 1", 100)],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "my-project", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(TabId(100), "my-project".to_string())]);
    }

    #[test]
    fn compute_renames_uses_tab_id_not_position() {
        // Arrange — position=2 but tab_id=200, verify rename uses tab_id
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(2, "Tab 3", 200)],
            pane_manifest: make_manifest(vec![(2, vec![make_pane(5, "nvim", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert — key is tab_id (200), not position (2)
        assert_eq!(renames, vec![(TabId(200), "nvim".to_string())]);
    }

    #[test]
    fn compute_renames_skips_tabs_with_empty_pane_title() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "Tab 1", 100)],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert!(renames.is_empty());
    }

    #[test]
    fn compute_renames_skips_when_already_cached() {
        // Arrange — cache keyed by tab_id (100)
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "old-name", 100)],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "shell", true)])]),
            last_set_names: HashMap::from([(TabId(100), "shell".to_string())]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert!(renames.is_empty());
    }

    #[test]
    fn compute_renames_skips_when_tab_name_already_matches() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "shell", 100)],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "shell", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert!(renames.is_empty());
    }

    #[test]
    fn compute_renames_renames_when_cache_differs_from_desired() {
        // Arrange — cache keyed by tab_id (100)
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "old-title", 100)],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "new-title", true)])]),
            last_set_names: HashMap::from([(TabId(100), "old-title".to_string())]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(TabId(100), "new-title".to_string())]);
    }

    #[test]
    fn compute_renames_handles_multiple_tabs() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![
                make_tab(0, "Tab 1", 100),
                make_tab(1, "already-correct", 101),
                make_tab(2, "Tab 3", 102),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "vim", true)]),
                (1, vec![make_pane(2, "already-correct", true)]),
                (2, vec![make_pane(3, "htop", true)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(
            renames,
            vec![
                (TabId(100), "vim".to_string()),
                (TabId(102), "htop".to_string()),
            ]
        );
    }

    #[test]
    fn compute_renames_skips_tabs_with_no_focused_pane() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "Tab 1", 100), make_tab(1, "Tab 2", 101)],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "shell", true)]),
                (1, vec![make_pane(2, "unfocused", false)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(TabId(100), "shell".to_string())]);
    }

    #[test]
    fn compute_renames_renames_all_tabs_not_just_active() {
        // Arrange — both active and inactive tabs should be renamed
        let state = State {
            permissions_granted: true,
            tabs: vec![
                make_tab(0, "Tab 1", 100),
                make_active_tab(1, "Tab 2", 101),
                make_tab(2, "Tab 3", 102),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "vim", true)]),
                (1, vec![make_pane(2, "htop", true)]),
                (2, vec![make_pane(3, "cargo", true)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert — all three tabs renamed, not just the active one
        assert_eq!(
            renames,
            vec![
                (TabId(100), "vim".to_string()),
                (TabId(101), "htop".to_string()),
                (TabId(102), "cargo".to_string()),
            ]
        );
    }

    #[test]
    fn compute_renames_survives_tab_close_with_stable_ids() {
        // Arrange: 3 tabs created (tab_ids 100, 101, 102), tab 101 closed.
        // Remaining: tab_id 100 at position 0, tab_id 102 at position 1.
        // Cache has entry for closed tab_id 101 — should not interfere.
        let state = State {
            permissions_granted: true,
            tabs: vec![
                make_tab(0, "Tab 1", 100),
                make_tab(1, "Tab 3", 102),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "shell", true)]),
                (1, vec![make_pane(3, "vim", true)]),
            ]),
            last_set_names: HashMap::from([
                (TabId(100), "shell".to_string()),
                (TabId(101), "old-closed-tab".to_string()),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert — tab_id 100 is already cached as "shell" so skipped;
        // tab_id 102 needs renaming to "vim"
        assert_eq!(renames, vec![(TabId(102), "vim".to_string())]);
    }

    // ── prune_stale_cache_entries ────────────────────────────────────────

    #[test]
    fn prune_stale_cache_entries_removes_closed_tab_entries() {
        // Arrange — cache keyed by tab_id; only tab_id 101 survives
        let mut state = State {
            tabs: vec![make_tab(0, "Tab 2", 101)],
            last_set_names: HashMap::from([
                (TabId(100), "old-shell".to_string()),
                (TabId(101), "vim".to_string()),
                (TabId(102), "htop".to_string()),
            ]),
            focused_pane_ids: HashMap::from([(TabId(100), 10), (TabId(101), 20), (TabId(102), 30)]),
            ..Default::default()
        };

        // Act
        state.prune_stale_cache_entries();

        // Assert
        assert_eq!(state.last_set_names.len(), 1);
        assert_eq!(state.last_set_names.get(&TabId(101)), Some(&"vim".to_string()));
        assert_eq!(state.focused_pane_ids.len(), 1);
        assert_eq!(state.focused_pane_ids.get(&TabId(101)), Some(&20));
    }

    #[test]
    fn prune_stale_cache_entries_keeps_all_when_tabs_match() {
        // Arrange
        let mut state = State {
            tabs: vec![make_tab(0, "Tab 1", 100), make_tab(1, "Tab 2", 101)],
            last_set_names: HashMap::from([
                (TabId(100), "shell".to_string()),
                (TabId(101), "vim".to_string()),
            ]),
            focused_pane_ids: HashMap::from([(TabId(100), 10), (TabId(101), 20)]),
            ..Default::default()
        };

        // Act
        state.prune_stale_cache_entries();

        // Assert
        assert_eq!(state.last_set_names.len(), 2);
        assert_eq!(state.focused_pane_ids.len(), 2);
    }

    #[test]
    fn prune_stale_cache_entries_clears_all_when_no_tabs() {
        // Arrange
        let mut state = State {
            tabs: vec![],
            last_set_names: HashMap::from([(TabId(100), "shell".to_string())]),
            focused_pane_ids: HashMap::from([(TabId(100), 10)]),
            ..Default::default()
        };

        // Act
        state.prune_stale_cache_entries();

        // Assert
        assert!(state.last_set_names.is_empty());
        assert!(state.focused_pane_ids.is_empty());
    }

    // ── extract_focused_pane_ids ─────────────────────────────────────────

    #[test]
    fn extract_focused_pane_ids_returns_empty_for_empty_manifest() {
        // Arrange
        let manifest = PaneManifest::default();
        let tabs: Vec<TabInfo> = vec![];

        // Act
        let focused = State::extract_focused_pane_ids(&manifest, &tabs);

        // Assert
        assert!(focused.is_empty());
    }

    #[test]
    fn extract_focused_pane_ids_tracks_focused_terminal_pane() {
        // Arrange
        let tabs = vec![make_tab(0, "Tab 1", 100)];
        let manifest = make_manifest(vec![(
            0,
            vec![make_pane(1, "unfocused", false), make_pane(2, "focused", true)],
        )]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest, &tabs);

        // Assert — keyed by tab_id (100), not position (0)
        assert_eq!(focused.get(&TabId(100)), Some(&2));
    }

    #[test]
    fn extract_focused_pane_ids_ignores_plugin_panes() {
        // Arrange
        let tabs = vec![make_tab(0, "Tab 1", 100)];
        let manifest = make_manifest(vec![(
            0,
            vec![make_plugin_pane(1, "tab-bar", true), make_pane(2, "shell", false)],
        )]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest, &tabs);

        // Assert
        assert!(focused.is_empty());
    }

    #[test]
    fn extract_focused_pane_ids_ignores_suppressed_panes() {
        // Arrange
        let tabs = vec![make_tab(0, "Tab 1", 100)];
        let manifest = make_manifest(vec![(
            0,
            vec![make_suppressed_pane(1, "hidden", true)],
        )]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest, &tabs);

        // Assert
        assert!(focused.is_empty());
    }

    #[test]
    fn extract_focused_pane_ids_tracks_across_multiple_tabs() {
        // Arrange
        let tabs = vec![
            make_tab(0, "Tab 1", 100),
            make_tab(1, "Tab 2", 101),
            make_tab(2, "Tab 3", 102),
        ];
        let manifest = make_manifest(vec![
            (0, vec![make_pane(1, "shell-1", true)]),
            (1, vec![make_pane(2, "vim", false), make_pane(3, "shell-2", true)]),
            (2, vec![make_pane(4, "htop", true)]),
        ]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest, &tabs);

        // Assert — keyed by tab_id
        assert_eq!(focused.len(), 3);
        assert_eq!(focused.get(&TabId(100)), Some(&1));
        assert_eq!(focused.get(&TabId(101)), Some(&3));
        assert_eq!(focused.get(&TabId(102)), Some(&4));
    }

    // ── active_tab_pane_to_refocus ───────────────────────────────────────

    #[test]
    fn active_tab_pane_to_refocus_returns_none_when_no_tabs() {
        // Arrange
        let state = State::default();

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn active_tab_pane_to_refocus_returns_none_when_no_active_tab() {
        // Arrange
        let state = State {
            tabs: vec![make_tab(0, "Tab 1", 100), make_tab(1, "Tab 2", 101)],
            focused_pane_ids: HashMap::from([(TabId(100), 1), (TabId(101), 2)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn active_tab_pane_to_refocus_returns_pane_id_of_active_tab() {
        // Arrange
        let state = State {
            tabs: vec![make_tab(0, "Tab 1", 100), make_active_tab(1, "Tab 2", 101)],
            focused_pane_ids: HashMap::from([(TabId(100), 10), (TabId(101), 20)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, Some(20));
    }

    #[test]
    fn active_tab_pane_to_refocus_returns_none_when_active_tab_has_no_tracked_pane() {
        // Arrange
        let state = State {
            tabs: vec![make_tab(0, "Tab 1", 100), make_active_tab(1, "Tab 2", 101)],
            focused_pane_ids: HashMap::from([(TabId(100), 10)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn active_tab_pane_to_refocus_ignores_inactive_tabs() {
        // Arrange
        let state = State {
            tabs: vec![
                make_tab(0, "Tab 1", 100),
                make_active_tab(1, "Tab 2", 101),
                make_tab(2, "Tab 3", 102),
            ],
            focused_pane_ids: HashMap::from([(TabId(100), 10), (TabId(101), 20), (TabId(102), 30)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, Some(20));
    }

    #[test]
    fn active_tab_pane_to_refocus_returns_none_when_floating_panes_visible() {
        // Arrange — e.g., the help window opened via Ctrl+/
        let state = State {
            tabs: vec![make_active_tab_with_floating(0, "Tab 1", 100)],
            focused_pane_ids: HashMap::from([(TabId(100), 10)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, None);
    }

    #[test]
    fn active_tab_pane_to_refocus_resumes_after_floating_panes_closed() {
        // Arrange — floating panes were visible but are now closed
        let state = State {
            tabs: vec![make_active_tab(0, "Tab 1", 100)],
            focused_pane_ids: HashMap::from([(TabId(100), 10)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, Some(10));
    }
}
