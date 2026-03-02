use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;

/// Default poll interval (in seconds) for refocusing panes to pick up title
/// changes. Zellij's PaneUpdate event doesn't fire on title-only changes,
/// so we periodically refocus to trigger a fresh PaneUpdate.
/// Override via plugin configuration: `poll_interval_secs "5.0"`.
const DEFAULT_POLL_INTERVAL_SECS: f64 = 2.0;

struct State {
    tabs: Vec<TabInfo>,
    pane_manifest: PaneManifest,
    permissions_granted: bool,
    /// Cache: tab_position -> last_set_name (avoid redundant rename calls)
    last_set_names: HashMap<usize, String>,
    /// Track focused terminal pane IDs per tab for timer-based refocus
    focused_pane_ids: HashMap<usize, u32>,
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
        self.poll_interval_secs = configuration
            .get("poll_interval_secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);

        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);
        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::PermissionRequestResult,
            EventType::Timer,
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
                self.focused_pane_ids = Self::extract_focused_pane_ids(&manifest);
                self.pane_manifest = manifest;
                self.rename_tabs();
            }
            Event::Timer(_elapsed) => {
                // Refocus the pane in the active tab to trigger a fresh PaneUpdate.
                // This is needed because Zellij doesn't fire PaneUpdate when only
                // a pane's terminal title changes (e.g., via OSC escape sequences).
                // Only refocus the active tab to avoid switching tabs as a side effect.
                if let Some(pane_id) = self.active_tab_pane_to_refocus() {
                    focus_terminal_pane(pane_id, false);
                }
                set_timeout(self.poll_interval_secs);
            }
            _ => {}
        }
        false // no rendering needed
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        // Required by ZellijPlugin trait but never called (update() returns false).
    }
}

impl State {
    /// Remove cache entries for tabs that no longer exist.
    fn prune_stale_cache_entries(&mut self) {
        let active_positions: HashSet<usize> = self.tabs.iter().map(|t| t.position).collect();
        self.last_set_names
            .retain(|pos, _| active_positions.contains(pos));
        self.focused_pane_ids
            .retain(|pos, _| active_positions.contains(pos));
    }

    /// Apply computed renames via host API and update the cache.
    fn rename_tabs(&mut self) {
        let renames = self.compute_renames();
        for (pos_1_indexed, name) in renames {
            rename_tab(pos_1_indexed, &name);
            self.last_set_names
                .insert((pos_1_indexed - 1) as usize, name);
        }
    }

    /// Compute which tabs need renaming.
    /// Returns a vec of (1-indexed tab position, desired name).
    fn compute_renames(&self) -> Vec<(u32, String)> {
        if !self.permissions_granted {
            return vec![];
        }

        // Debug: dump PaneManifest keys vs TabInfo positions
        let mut manifest_keys: Vec<usize> = self.pane_manifest.panes.keys().copied().collect();
        manifest_keys.sort();
        let tab_positions: Vec<usize> = self.tabs.iter().map(|t| t.position).collect();
        eprintln!(
            "[zjtabname] manifest keys: {:?}, tab positions: {:?}",
            manifest_keys, tab_positions
        );
        for tab in &self.tabs {
            let pane_title = self.find_focused_pane_title(tab.position);
            eprintln!(
                "[zjtabname] tab pos={} name={:?} -> manifest lookup({}) = {:?}",
                tab.position, tab.name, tab.position, pane_title
            );
        }

        let mut renames = Vec::new();
        for tab in &self.tabs {
            if let Some(desired_name) = self.find_focused_pane_title(tab.position) {
                if desired_name.is_empty() {
                    continue;
                }

                let already_set = self
                    .last_set_names
                    .get(&tab.position)
                    .map(|n| n == &desired_name)
                    .unwrap_or(false);

                if !already_set && tab.name != desired_name {
                    // rename_tab() takes a 1-indexed position; TabInfo.position is 0-indexed.
                    renames.push(((tab.position + 1) as u32, desired_name));
                }
            }
        }

        if !renames.is_empty() {
            eprintln!("[zjtabname] renames: {:?}", renames);
        }

        renames
    }

    /// Return the pane ID to refocus for title polling (only the active tab).
    fn active_tab_pane_to_refocus(&self) -> Option<u32> {
        let active_tab = self.tabs.iter().find(|t| t.active)?;
        self.focused_pane_ids.get(&active_tab.position).copied()
    }

    /// Extract focused terminal pane IDs from a pane manifest.
    fn extract_focused_pane_ids(manifest: &PaneManifest) -> HashMap<usize, u32> {
        let mut focused = HashMap::new();
        for (tab_pos, panes) in &manifest.panes {
            for p in panes {
                if p.is_focused && !p.is_plugin && !p.is_suppressed {
                    focused.insert(*tab_pos, p.id);
                }
            }
        }
        focused
    }

    /// Find the title of the focused non-plugin, non-suppressed pane in a given tab.
    fn find_focused_pane_title(&self, tab_position: usize) -> Option<String> {
        let panes = self.pane_manifest.panes.get(&tab_position)?;
        panes
            .iter()
            .find(|p| p.is_focused && !p.is_plugin && !p.is_suppressed)
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

    fn make_tab(position: usize, name: &str) -> TabInfo {
        TabInfo {
            position,
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn make_active_tab(position: usize, name: &str) -> TabInfo {
        TabInfo {
            active: true,
            ..make_tab(position, name)
        }
    }

    fn make_manifest(entries: Vec<(usize, Vec<PaneInfo>)>) -> PaneManifest {
        PaneManifest {
            panes: entries.into_iter().collect(),
        }
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
            tabs: vec![make_tab(0, "Tab 1")],
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
            tabs: vec![make_tab(0, "Tab 1")],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "my-project", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(1, "my-project".to_string())]);
    }

    #[test]
    fn compute_renames_uses_1_indexed_tab_position() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(2, "Tab 3")],
            pane_manifest: make_manifest(vec![(2, vec![make_pane(5, "nvim", true)])]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(3, "nvim".to_string())]);
    }

    #[test]
    fn compute_renames_skips_tabs_with_empty_pane_title() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "Tab 1")],
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
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "old-name")],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "shell", true)])]),
            last_set_names: HashMap::from([(0, "shell".to_string())]),
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
            tabs: vec![make_tab(0, "shell")],
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
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "old-title")],
            pane_manifest: make_manifest(vec![(0, vec![make_pane(1, "new-title", true)])]),
            last_set_names: HashMap::from([(0, "old-title".to_string())]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(1, "new-title".to_string())]);
    }

    #[test]
    fn compute_renames_handles_multiple_tabs() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![
                make_tab(0, "Tab 1"),
                make_tab(1, "already-correct"),
                make_tab(2, "Tab 3"),
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
                (1, "vim".to_string()),
                (3, "htop".to_string()),
            ]
        );
    }

    #[test]
    fn compute_renames_skips_tabs_with_no_focused_pane() {
        // Arrange
        let state = State {
            permissions_granted: true,
            tabs: vec![make_tab(0, "Tab 1"), make_tab(1, "Tab 2")],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "shell", true)]),
                (1, vec![make_pane(2, "unfocused", false)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert_eq!(renames, vec![(1, "shell".to_string())]);
    }

    // ── prune_stale_cache_entries ────────────────────────────────────────

    #[test]
    fn prune_stale_cache_entries_removes_closed_tab_entries() {
        // Arrange
        let mut state = State {
            tabs: vec![make_tab(1, "Tab 2")],
            last_set_names: HashMap::from([
                (0, "old-shell".to_string()),
                (1, "vim".to_string()),
                (2, "htop".to_string()),
            ]),
            focused_pane_ids: HashMap::from([(0, 10), (1, 20), (2, 30)]),
            ..Default::default()
        };

        // Act
        state.prune_stale_cache_entries();

        // Assert
        assert_eq!(state.last_set_names.len(), 1);
        assert_eq!(state.last_set_names.get(&1), Some(&"vim".to_string()));
        assert_eq!(state.focused_pane_ids.len(), 1);
        assert_eq!(state.focused_pane_ids.get(&1), Some(&20));
    }

    #[test]
    fn prune_stale_cache_entries_keeps_all_when_tabs_match() {
        // Arrange
        let mut state = State {
            tabs: vec![make_tab(0, "Tab 1"), make_tab(1, "Tab 2")],
            last_set_names: HashMap::from([
                (0, "shell".to_string()),
                (1, "vim".to_string()),
            ]),
            focused_pane_ids: HashMap::from([(0, 10), (1, 20)]),
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
            last_set_names: HashMap::from([(0, "shell".to_string())]),
            focused_pane_ids: HashMap::from([(0, 10)]),
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

        // Act
        let focused = State::extract_focused_pane_ids(&manifest);

        // Assert
        assert!(focused.is_empty());
    }

    #[test]
    fn extract_focused_pane_ids_tracks_focused_terminal_pane() {
        // Arrange
        let manifest = make_manifest(vec![(
            0,
            vec![make_pane(1, "unfocused", false), make_pane(2, "focused", true)],
        )]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest);

        // Assert
        assert_eq!(focused.get(&0), Some(&2));
    }

    #[test]
    fn extract_focused_pane_ids_ignores_plugin_panes() {
        // Arrange
        let manifest = make_manifest(vec![(
            0,
            vec![make_plugin_pane(1, "tab-bar", true), make_pane(2, "shell", false)],
        )]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest);

        // Assert
        assert!(focused.is_empty());
    }

    #[test]
    fn extract_focused_pane_ids_ignores_suppressed_panes() {
        // Arrange
        let manifest = make_manifest(vec![(
            0,
            vec![make_suppressed_pane(1, "hidden", true)],
        )]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest);

        // Assert
        assert!(focused.is_empty());
    }

    #[test]
    fn extract_focused_pane_ids_tracks_across_multiple_tabs() {
        // Arrange
        let manifest = make_manifest(vec![
            (0, vec![make_pane(1, "shell-1", true)]),
            (1, vec![make_pane(2, "vim", false), make_pane(3, "shell-2", true)]),
            (2, vec![make_pane(4, "htop", true)]),
        ]);

        // Act
        let focused = State::extract_focused_pane_ids(&manifest);

        // Assert
        assert_eq!(focused.len(), 3);
        assert_eq!(focused.get(&0), Some(&1));
        assert_eq!(focused.get(&1), Some(&3));
        assert_eq!(focused.get(&2), Some(&4));
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
            tabs: vec![make_tab(0, "Tab 1"), make_tab(1, "Tab 2")],
            focused_pane_ids: HashMap::from([(0, 1), (1, 2)]),
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
            tabs: vec![make_tab(0, "Tab 1"), make_active_tab(1, "Tab 2")],
            focused_pane_ids: HashMap::from([(0, 10), (1, 20)]),
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
            tabs: vec![make_tab(0, "Tab 1"), make_active_tab(1, "Tab 2")],
            focused_pane_ids: HashMap::from([(0, 10)]),
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
                make_tab(0, "Tab 1"),
                make_active_tab(1, "Tab 2"),
                make_tab(2, "Tab 3"),
            ],
            focused_pane_ids: HashMap::from([(0, 10), (1, 20), (2, 30)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, Some(20));
    }
}
