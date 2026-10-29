# niri-sidebar

A lightweight, external sidebar manager for the [Niri](https://github.com/YaLTeR/niri) window manager.

`niri-sidebar` allows you to toggle any window into a "floating sidebar" stack on the right side of your screen. It automatically handles resizing, positioning, and stacking, keeping your main workspace clean while keeping utility apps (terminals, music players, chats) accessible.

https://github.com/user-attachments/assets/46f51b18-d85b-4d79-9c44-63e63649707a

## Features

- **Toggle Windows:** Instantly move the focused window into the sidebar stack.
- **Auto-Stacking:** Windows automatically stack vertically with a configurable gap.
- **Smart Close:** Closing a sidebar window automatically reorders the remaining windows to fill the gap.
- **Flip & Hide:** Flip the stack to the other side of the screen or hide it completely (peeking mode).
- **State Persistence:** Remembers your sidebar windows and their original sizes even if you restart the tool.

## Installation

### Option 1: Download Binary (Recommended)

1.  Go to the [Releases](https://github.com/Vigintillionn/niri-sidebar/releases) page.
2.  Download the binary matching your architecture (e.g., `niri-sidebar-linux-x86_64`).
3.  Rename it to `niri-sidebar`, make it executable, and move it to your path:

```bash
# Example for x86_64
mv niri-sidebar-linux-x86_64 niri-sidebar
chmod +x niri-sidebar

# Move to a directory in your PATH
sudo mv niri-sidebar /usr/local/bin/
# OR for a local installation:
mkdir -p ~/.local/bin
mv niri-sidebar ~/.local/bin/
```

### Option 2: Build from Source

```bash
git clone https://github.com/Vigintillionn/niri-sidebar
cd niri-sidebar
cargo build --release
cp target/release/niri-sidebar ~/.local/bin/
```

### Option 3: Arch Linux (AUR)

An unofficial AUR package is available:
```bash
yay -S niri-sidebar-git
```
> **Note:** This package is community-maintained on the AUR and is not officially maintained by the niri-sidebar project. Please review the PKGBUILD before installing.

### Option 4: Void Linux (`xbps-src`)

A Void Linux package is available as a template in the official [`void-packages`](https://github.com/void-linux/void-packages) repository.

1. Clone the `void-packages` repository (if you haven’t already):
```bash
git clone https://github.com/void-linux/void-packages.git
cd void-packages
./xbps-src binary-bootstrap
```
2. Build the `niri-sidebar` package
```bash
./xbps-src pkg niri-sidebar
```
3. Install the `niri-sidebar` package
```bash
sudo xbps-install --repository=hostdir/binpkgs niri-sidebar
```
> **Note:** This package is community-maintained in the Void Linux community repository and is not officially maintained by the niri-sidebar project. Please review the template before installing.

## Niri configuration

Add the following bindings to your niri `config.kdl` file.

**Important:** These examples assume you installed the tool to `~/.local/bin`. If you installed it elsewhere, update the paths accordingly.

```kdl
binds {
    // Toggle the focused window into/out of the sidebar
    Mod+S { spawn-sh "~/.local/bin/niri-sidebar toggle-window"; }

    // Toggle sidebar visibility (hide/show)
    Mod+Shift+S { spawn-sh "~/.local/bin/niri-sidebar toggle-visibility"; }

    // Flip the order of the sidebar
    Mod+Ctrl+S { spawn-sh "~/.local/bin/niri-sidebar flip"; }

    // Force reorder (useful if something gets misaligned manually)
    Mod+Alt+R { spawn-sh "~/.local/bin/niri-sidebar reorder"; }
}
```

Tracked windows can form independent sidebars on multiple active monitors. Move a
tracked window with niri's normal workspace or monitor actions (for example,
`niri msg action move-window-to-monitor DP-2`); it remains tracked and joins the
sidebar on its destination workspace. Hover zones are created on every active output
that currently contains tracked sidebar windows.

When a tracked window moves to another workspace or monitor, it is appended to the
end of that destination sidebar's displayed order. With a top-to-bottom stack this
places it at the bottom. `flip` still reverses the displayed order on every sidebar.

In order for your sidebar to stay consistent and gap free, you want to add the following to your startup scripts

```kdl
spawn-at-startup "~/.local/bin/niri-sidebar" "listen"
```

This will spawn a daemon to listen for window close events and reorder the sidebar if the closed window was part of it.

Some applications enforce a minimum window size that is larger than your sidebar configuration, which can cause windows to overlap or look broken. Add this rule to force them to respect the sidebar size:

```kdl
window-rule {
    match is-floating=true
    min-width 100
    min-height 100
}
```

## Configuration

Run `niri-sidebar init` to generate a `config.toml` file located at `~/.config/niri-sidebar`.

#### Default Config

```toml
# niri-sidebar configuration

[geometry]
# Width of the sidebar in pixels
width = 400
# Height of the sidebar windows
height = 335
# Gap between windows in the stack
gap = 10

[margins]
# Margins are default to 0 if left out
# Space from the top of the screen
top = 50
# Space from the right edge of the screen
right = 10
# Space from the left edge of the screen
left = 10
# Space from the bottom of the screen
bottom = 10

[interaction]
# Where to put the sidebar, can be "left", "right", "top" or "bottom"
# Defaults to "right"
position = "right"
# How windows stack inside the sidebar.
# For left/right sidebars use "top_to_bottom" or "bottom_to_top".
# For top/bottom bars use "left_to_right" or "right_to_left".
stack_direction = "bottom_to_top"
# Width of windows when sidebar is hidden in pixels
peek = 10
# Width of window when sidebar is hidden but window is focused in pixels
# set this equal to peek to disable this feature
# set this equal to sidebar_width + offset_right to make focused windows "unhide"
# when it is at least the window width, the shown sidebar uses the same gap
# (focus_peek - width) instead of the margin, so both positions line up
# Optional and defaults to peek if ommitted
focus_peek = 50
# Whether the sidebar should follow if you switch workspaces
sticky = false
# Expand the sidebar when the pointer touches the screen edge it is docked to,
# and collapse it once the pointer leaves (runs inside `niri-sidebar listen`)
hover = false
# "all" shows the whole sidebar when the edge is hovered; "window" reveals only the
# window whose edge strip is hovered (only applies while the sidebar is hidden)
# hover_mode = "all"
# Width of the screen-edge strip that triggers the expand in "all" mode, in pixels
# (defaults to `peek`). In "window" mode each strip covers the part of that window still visible on screen.
# hover_zone_width = 1
```

#### Window Rules

Window rules allow you to customize behavior for specific windows based on their `app_id` or `title`. Rules are evaluated in order, and the first matching rule is applied. If a field is omitted in a rule, the global default configuration is used.

```toml
# Example window rule
# all fields are optional if not given a default from other configs will be used
[[window_rule]]
app_id = "firefox"  # regex, if not set will match all app_id's
title = "^Picture-in-Picture$"  # regex, if not set will match no matter the title
width = 700
height = 400
focus_peek = 710
peek = 10
auto_add = true  # defaults to false
```

## Workflow tips

- **Adding/Removing:** Press `Mod+S` on any window to snap it into the sidebar. Press it again to return it to your normal tiling layout.
- **Hiding:** Press `Mod+Shift+S` to tuck the sidebar away. It will stick out slightly (configured by peek) so you know it's there.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
