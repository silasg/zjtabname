# zjtabname

A [Zellij](https://zellij.dev/) plugin that automatically sets each tab's title to the title of its currently focused pane.

**Requires Zellij ≥ 0.44.0.**

Tab titles update when:
- Focus moves to a different pane within a tab
- The focused pane's title changes (e.g., a shell sets its terminal title via OSC escape sequences)

## Install

### Using mise (recommended)

[mise](https://mise.jdx.dev/) handles Rust installation and the WASM target automatically:

```bash
mise run build
mise run install
```

### Using cargo

Requires Rust with the `wasm32-wasip1` target:

```bash
rustup target add wasm32-wasip1
cargo build --release
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
| `poll_interval_secs` | `2.0` | How often (in seconds) to poll for pane title changes. Zellij doesn't fire events when only a pane's terminal title changes (e.g., when editors or monitors set their title via OSC escape sequences), so the plugin periodically refocuses the active pane to detect updates. Shell directory changes are detected instantly via `CwdChanged` events. |

Example (in a layout pane):

```kdl
pane {
    plugin location="file:~/.config/zellij/plugins/zjtabname.wasm" {
        poll_interval_secs "3.0"
    }
}
```

> **Note:** `load_plugins` in `config.kdl` does not support passing configuration parameters — only plain plugin URLs are accepted. To pass configuration, load the plugin via a layout pane block or the command line (`zellij action launch-or-focus-plugin --configuration "key=value"`).

## How it works

The plugin runs as a headless background pane (`set_selectable(false)`) and listens for `TabUpdate`, `PaneUpdate`, and `CwdChanged` events. On each event, it iterates all tabs, finds the focused non-plugin, non-suppressed pane in each tab, and renames the tab to that pane's title via `rename_tab_with_id()`.

Tabs are identified by their stable `tab_id` (not position), so renames remain correct even after tabs are closed or reordered.

Shell directory changes (e.g., `cd`) are detected instantly via `CwdChanged` events. For programs that set their own terminal title without a CWD change (e.g., vim, htop, ssh), the plugin uses a timer to periodically refocus the active pane, which triggers a fresh `PaneUpdate` with the latest title.

Plugin configuration changes are picked up at runtime via `PluginConfigurationChanged` events — no restart needed.

## Permissions

The plugin requests permissions on first load:
- **ReadApplicationState** — to receive tab, pane, and CWD update events
- **ChangeApplicationState** — to rename tabs
