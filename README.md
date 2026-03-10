# zjtabname

A [Zellij](https://zellij.dev/) plugin that automatically sets each tab's title to the title of its currently focused pane.

Tab titles update when:
- Focus moves to a different pane within a tab
- The focused pane's title changes (e.g., a shell sets its terminal title via OSC escape sequences)

## Install

Requires Rust with the `wasm32-wasip1` target:

```bash
rustup target add wasm32-wasip1
```

Build the plugin:

```bash
cargo build --release
```

Copy the WASM binary to Zellij's plugin directory:

```bash
mkdir -p ~/.config/zellij/plugins
cp target/wasm32-wasip1/release/zjtabname.wasm ~/.config/zellij/plugins/
```

## Usage

### In a layout file

Add zjtabname as a background pane in your layout:

```kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        children
        pane {
            plugin location="file:~/.config/zellij/plugins/zjtabname.wasm"
        }
    }
}
```

### Via `load_plugins` (Zellij 0.41+)

Load the plugin globally in your Zellij config so it runs in every session without needing a layout pane:

```kdl
load_plugins {
    zjtabname location="file:~/.config/zellij/plugins/zjtabname.wasm"
}
```

### At runtime

```bash
zellij action start-or-reload-plugin file:~/.config/zellij/plugins/zjtabname.wasm
```

## Configuration

Configuration is passed via the plugin block in KDL. Currently supported options:

| Option | Default | Description |
|--------|---------|-------------|
| `poll_interval_secs` | `2.0` | How often (in seconds) to poll for pane title changes. Zellij doesn't fire events when only a pane's terminal title changes, so the plugin periodically refocuses the active pane to detect updates. |
| `rename_via_cli_workaround` | `true` | Use `zellij action rename-tab` (via CLI) to rename only the active tab, sidestepping [Zellij bug #3535](#tab-renames-target-wrong-tabs-after-closing-a-tab). When `"false"`, all tabs are renamed via the plugin API's `rename_tab()` (works correctly as long as no tabs have been closed during the session). |

Example (in a layout pane):

```kdl
pane {
    plugin location="file:~/.config/zellij/plugins/zjtabname.wasm" {
        poll_interval_secs "5.0"
        rename_via_cli_workaround "false"
    }
}
```

> **Note:** `load_plugins` in `config.kdl` does not support passing configuration parameters — only plain plugin URLs are accepted. To pass configuration, load the plugin via a layout pane block or the command line (`zellij action launch-or-focus-plugin --configuration "key=value"`).

## How it works

The plugin runs as a headless background pane (`set_selectable(false)`) and listens for `TabUpdate` and `PaneUpdate` events. On each event, it iterates all tabs, finds the focused non-plugin, non-suppressed pane in each tab, and renames the tab to that pane's title via `rename_tab()`.

Because Zellij doesn't fire `PaneUpdate` when only a pane's terminal title changes (e.g., via OSC escape sequences), the plugin also uses a timer to periodically refocus the active pane, which triggers a fresh `PaneUpdate` with the latest title.

## Known Issues

### Tab renames target wrong tabs after closing a tab

Zellij has an upstream bug ([#3535](https://github.com/zellij-org/zellij/issues/3535)) where `rename_tab()` misidentifies tabs after a tab has been closed. The plugin API parameter is documented as a tab *position* (visual order, renumbered after close), but Zellij's server internally treats it as a tab *index* (a stable internal ID that keeps gaps when tabs are closed). After closing a tab, positions and indices diverge, causing renames to hit the wrong tab.

All known Zellij tab-renaming plugins ([zellij-attention](https://github.com/KiryuuLight/zellij-attention), [zellij-tabula](https://github.com/bezbac/zellij-tabula), [zellij-tab-name](https://github.com/Cynary/zellij-tab-name)) are affected by this same issue. A fix ([PR #4179](https://github.com/zellij-org/zellij/pull/4179)) exists upstream but is not yet merged.

**Workaround:** The plugin defaults to `rename_via_cli_workaround "true"`. Instead of using the buggy plugin API, it shells out to `zellij action rename-tab` which renames the current tab without a position argument, completely sidestepping the bug. The trade-off is that only the active tab is renamed — background tabs keep whatever name they had when you last focused them, rather than updating live.

### Rare race condition with CLI rename workaround

When `rename_via_cli_workaround` is enabled (the default), there is a small window between the plugin issuing `zellij action rename-tab` and Zellij processing it. If you switch tabs during that window, the rename may land on the newly active tab instead of the intended one. In practice this is very unlikely since the window is tiny, but it can result in a tab briefly showing another tab's name. Switching back and forth between tabs will correct it.

## Permissions

The plugin requests permissions on first load:
- **ReadApplicationState** — to receive tab and pane update events
- **ChangeApplicationState** — to rename tabs
- **RunCommands** — to execute `zellij action rename-tab` (only when `rename_via_cli_workaround` is enabled, which is the default)
