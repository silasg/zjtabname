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
    /// When true (the default), only rename the currently active tab by
    /// shelling out to `zellij action rename-tab` via `run_command()`.
    /// This works around Zellij bug #3535 where the plugin API's
    /// `rename_tab()` misidentifies tabs after a tab is closed, because
    /// the position parameter is treated as an internal tab creation index
    /// (stable, with gaps) rather than the visual tab position.
    /// The CLI's `rename-tab` command operates on the current tab without
    /// a position argument, completely sidestepping the bug.
    /// When false, all tabs are renamed via the plugin API `rename_tab()`
    /// (works correctly as long as no tabs have been closed during the
    /// session).
    /// Override via plugin configuration: `rename_via_cli_workaround "false"`.
    rename_via_cli_workaround: bool,
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
            rename_via_cli_workaround: true,
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

        self.rename_via_cli_workaround = configuration
            .get("rename_via_cli_workaround")
            .map(|v| v == "true")
            .unwrap_or(true);

        let mut permissions = vec![
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ];
        if self.rename_via_cli_workaround {
            permissions.push(PermissionType::RunCommands);
        }
        request_permission(&permissions);
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

    /// Apply computed renames and update the cache.
    /// When `rename_via_cli_workaround` is enabled, uses `zellij action rename-tab`
    /// (which renames the current tab without a position argument) to sidestep
    /// Zellij bug #3535. Otherwise, uses the plugin API `rename_tab()`.
    fn rename_tabs(&mut self) {
        let renames = self.compute_renames();
        for (pos_1_indexed, name) in renames {
            if self.rename_via_cli_workaround {
                run_command(
                    &["zellij", "action", "rename-tab", &name],
                    BTreeMap::new(),
                );
            } else {
                rename_tab(pos_1_indexed, &name);
            }
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

        let tabs_to_check: Vec<&TabInfo> = if self.rename_via_cli_workaround {
            self.tabs.iter().filter(|t| t.active).collect()
        } else {
            self.tabs.iter().collect()
        };

        let mut renames = Vec::new();
        for tab in tabs_to_check {
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
        renames
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

    fn make_active_tab_with_floating(position: usize, name: &str) -> TabInfo {
        TabInfo {
            are_floating_panes_visible: true,
            ..make_active_tab(position, name)
        }
    }

    fn make_manifest(entries: Vec<(usize, Vec<PaneInfo>)>) -> PaneManifest {
        PaneManifest {
            panes: entries.into_iter().collect(),
        }
    }

    fn make_state_with_cli_workaround(rename_via_cli_workaround: bool) -> State {
        State {
            rename_via_cli_workaround,
            ..Default::default()
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
            rename_via_cli_workaround: false,
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
            rename_via_cli_workaround: false,
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
            rename_via_cli_workaround: false,
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
            rename_via_cli_workaround: false,
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
            rename_via_cli_workaround: false,
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

    // ── compute_renames with rename_via_cli_workaround ─────────────────

    #[test]
    fn compute_renames_cli_workaround_renames_only_active_tab() {
        // Arrange
        let state = State {
            permissions_granted: true,
            rename_via_cli_workaround: true,
            tabs: vec![
                make_tab(0, "Tab 1"),
                make_active_tab(1, "Tab 2"),
                make_tab(2, "Tab 3"),
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

        // Assert — only the active tab (position 1) is renamed
        assert_eq!(renames, vec![(2, "htop".to_string())]);
    }

    #[test]
    fn compute_renames_cli_workaround_returns_empty_when_active_tab_already_correct() {
        // Arrange
        let state = State {
            permissions_granted: true,
            rename_via_cli_workaround: true,
            tabs: vec![
                make_tab(0, "Tab 1"),
                make_active_tab(1, "htop"),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "vim", true)]),
                (1, vec![make_pane(2, "htop", true)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert
        assert!(renames.is_empty());
    }

    #[test]
    fn compute_renames_all_tabs_when_cli_workaround_disabled() {
        // Arrange
        let state = State {
            permissions_granted: true,
            rename_via_cli_workaround: false,
            tabs: vec![
                make_tab(0, "Tab 1"),
                make_active_tab(1, "Tab 2"),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "vim", true)]),
                (1, vec![make_pane(2, "htop", true)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();

        // Assert — both tabs renamed
        assert_eq!(
            renames,
            vec![
                (1, "vim".to_string()),
                (2, "htop".to_string()),
            ]
        );
    }

    // ── Zellij bug #3535: rename_tab position vs internal index ─────────
    //
    // These tests document the upstream bug where rename_tab(n) is treated
    // as an internal tab index (creation ID) rather than a visual position.
    // Our plugin computes the correct 1-indexed position, but Zellij's
    // server does `screen.tabs.get_mut(&(n - 1))` where the map key is
    // the creation index (stable, with gaps after tab closures).
    //
    // Scenario reproduced live:
    //   1. Create 4 tabs: keys {0,1,2,3}, positions [0,1,2,3]
    //   2. Close tab at position 1 (key 1)
    //   3. Remaining: keys {0,2,3}, positions [0,1,2]
    //   4. Active tab at position 2 (key 3), plugin calls rename_tab(3)
    //   5. Server looks up tabs[3-1] = tabs[2] → the tab at position 1!
    //   6. Result: the tab BEFORE the active one gets renamed.

    /// Simulates the Zellij server's buggy rename_tab behavior.
    /// Takes the 1-indexed position the plugin passes and the tab map
    /// (creation_index -> tab_name), returns which tab name gets renamed.
    fn zellij_server_rename_tab_buggy<'a>(
        tab_map: &'a std::collections::BTreeMap<usize, &'a str>,
        plugin_arg: u32,
    ) -> Option<&'a str> {
        // This is what Zellij's screen.rs does:
        //   screen.tabs.get_mut(&tab_index.saturating_sub(1))
        let key = (plugin_arg as usize).saturating_sub(1);
        tab_map.get(&key).copied()
    }

    #[test]
    fn zellij_bug_3535_rename_hits_wrong_tab_after_close() {
        // Arrange: 4 tabs created, then tab at key 1 closed.
        // Remaining internal map: {0: "TAB-1", 2: "TAB-3", 3: "TAB-4"}
        // Visual positions after close: TAB-1=0, TAB-3=1, TAB-4=2
        let tab_map: std::collections::BTreeMap<usize, &str> = [
            (0, "TAB-1"),
            (2, "TAB-3"),
            (3, "TAB-4"),
        ].into_iter().collect();

        // Active tab is TAB-4 at visual position 2.
        // Plugin correctly computes rename_tab(3) (position 2 + 1).
        let state = State {
            permissions_granted: true,
            rename_via_cli_workaround: true,
            tabs: vec![
                make_tab(0, "TAB-1"),
                make_tab(1, "TAB-3"),
                make_active_tab(2, "TAB-4"),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "pane-1", true)]),
                (1, vec![make_pane(2, "pane-3", true)]),
                (2, vec![make_pane(3, "new-title", true)]),
            ]),
            ..Default::default()
        };

        // Act: plugin computes the rename
        let renames = state.compute_renames();
        assert_eq!(renames.len(), 1);
        let (plugin_position_arg, desired_name) = &renames[0];

        // The plugin correctly targets position 3 (1-indexed) for TAB-4
        assert_eq!(*plugin_position_arg, 3);
        assert_eq!(desired_name, "new-title");

        // But Zellij's server interprets this as internal key 2, which is TAB-3!
        let actually_renamed = zellij_server_rename_tab_buggy(&tab_map, *plugin_position_arg);
        assert_eq!(
            actually_renamed,
            Some("TAB-3"),
            "BUG: Zellij renames TAB-3 instead of TAB-4 (see issue #3535)"
        );
        // This SHOULD be TAB-4, but the bug causes TAB-3 to be renamed instead.
        assert_ne!(
            actually_renamed,
            Some("TAB-4"),
            "If this fails, the Zellij bug has been fixed upstream!"
        );
    }

    #[test]
    fn zellij_bug_3535_rename_hits_deleted_key_becomes_noop() {
        // Arrange: 3 tabs created, then tab at key 1 closed.
        // Remaining internal map: {0: "TAB-1", 2: "TAB-3"}
        // Visual positions: TAB-1=0, TAB-3=1
        let tab_map: std::collections::BTreeMap<usize, &str> = [
            (0, "TAB-1"),
            (2, "TAB-3"),
        ].into_iter().collect();

        // Active tab is TAB-3 at visual position 1.
        // Plugin computes rename_tab(2) (position 1 + 1).
        let state = State {
            permissions_granted: true,
            rename_via_cli_workaround: true,
            tabs: vec![
                make_tab(0, "TAB-1"),
                make_active_tab(1, "TAB-3"),
            ],
            pane_manifest: make_manifest(vec![
                (0, vec![make_pane(1, "pane-1", true)]),
                (1, vec![make_pane(2, "new-title", true)]),
            ]),
            ..Default::default()
        };

        // Act
        let renames = state.compute_renames();
        assert_eq!(renames.len(), 1);
        let (plugin_position_arg, _) = &renames[0];
        assert_eq!(*plugin_position_arg, 2);

        // Zellij looks up tabs[2-1] = tabs[1], which was deleted → silent no-op
        let actually_renamed = zellij_server_rename_tab_buggy(&tab_map, *plugin_position_arg);
        assert_eq!(
            actually_renamed,
            None,
            "BUG: rename_tab(2) hits deleted key 1, rename silently fails (see issue #3535)"
        );
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

    #[test]
    fn active_tab_pane_to_refocus_returns_none_when_floating_panes_visible() {
        // Arrange — e.g., the help window opened via Ctrl+/
        let state = State {
            tabs: vec![make_active_tab_with_floating(0, "Tab 1")],
            focused_pane_ids: HashMap::from([(0, 10)]),
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
            tabs: vec![make_active_tab(0, "Tab 1")],
            focused_pane_ids: HashMap::from([(0, 10)]),
            ..Default::default()
        };

        // Act
        let result = state.active_tab_pane_to_refocus();

        // Assert
        assert_eq!(result, Some(10));
    }
}
