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

Example:

```kdl
pane {
    plugin location="file:~/.config/zellij/plugins/zjtabname.wasm" {
        poll_interval_secs "5.0"
    }
}
```

## How it works

The plugin runs as a headless background pane (`set_selectable(false)`) and listens for `TabUpdate` and `PaneUpdate` events. On each event, it iterates all tabs, finds the focused non-plugin, non-suppressed pane in each tab, and renames the tab to that pane's title via `rename_tab()`.

Because Zellij doesn't fire `PaneUpdate` when only a pane's terminal title changes (e.g., via OSC escape sequences), the plugin also uses a timer to periodically refocus the active pane, which triggers a fresh `PaneUpdate` with the latest title.

## Permissions

The plugin requests two permissions on first load:
- **ReadApplicationState** — to receive tab and pane update events
- **ChangeApplicationState** — to rename tabs
