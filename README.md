# Carrot

A pure Rust tiling Wayland compositor with zero linked C, all the way down to the kernel. Carrot aims to be a feature-complete, all-in-one compositor with native support for blur, animations, multi-GPU, tearing, and more - no scripting or external tools needed.

> [!WARNING]
> Carrot is in early development and is in its first beta. Any contributions, suggestions, or feedback are encouraged!

> [!NOTE]
> This README is a draft, and is not in any way final. Anything described in this document is subject to change. 

## Complete List of Intended Features 

### Tiling
- **Dwindle layout** with directional focus, window swapping, and split ratio control
- **Window groups** (tabbed) with a styled groupbar
- **Floating layer** with mouse drag move/resize, centering, and PiP mode
- Fullscreen (real, bordered, and borderless), pin (visible on all workspaces)
- **Force float/tile per window** via window rules - no need for apps to behave

### Workspaces
- Numbered workspaces with relative navigation
- **Workspace groups** - sets of 10, navigable within and between groups
- **Special workspaces** (named scratchpads) with toggle-and-launch behavior

### Eye Candy
- Multi-pass **Kawase blur** with per-window and per-layer control
- **Drop shadows** with configurable falloff
- **Rounded corners**, per-window opacity, active/inactive borders
- **Animations** - custom bezier curves, spring physics, per-property config
  - Styles: slide, slidefadevert, popin, fade

### Window & Layer Rules
- Match by class, title, fullscreen state, xwayland, float (regex)
- Tons of effects: opacity, float, size, center, workspace assignment, immediate (tearing), idle inhibit, rounding, blur, shadow, dim, pin, keep aspect ratio
- **Focus steal control** - let specific apps actually grab focus when they ask for it, instead of just marking urgent
- **Input redirection** - route specific keys to unfocused/offscreen windows, with redirect (background only) or passthrough (both) modes
- Layer rules: animation overrides, blur, ignore alpha threshold

### Input
- Per-device mouse configuration (accel profile, sensitivity, natural scroll)
- Configurable keyboard repeat, layout, numlock
- Keybind types: press, lock-safe, repeat, release, mouse
- **Submaps** - switchable keybind contexts

### Performance & Gaming
- **Tearing** via `wp_tearing_control_v1` with per-window `immediate` rule
- **VRR** (adaptive sync) - off, automatic, or always, per-output
- **Direct scanout** bypass for fullscreen windows
- **Multi-GPU** - cross-GPU DMA-BUF import, blit and direct import paths, explicit sync, configurable renderer skip on secondary GPUs
- **Per-output GPU assignment** - render specific displays on a selectable GPUs (e.g. iGPU for laptop screen, eGPU for gaming monitor)

### IPC
- Carrot-native JSON protocol over a single unix socket at `$XDG_RUNTIME_DIR/carrot.$WAYLAND_DISPLAY.sock` - one request and one reply per line
- `burrow`, the companion CLI, for queries, dispatch, and a live `subscribe` event stream (ndjson)
- Queries: active window, workspaces, workspace groups, special workspaces, monitors, clients
- Event stream: window open/close, focus, workspace changes, monitor hotplug, fullscreen, urgent
- Dispatcher commands (exec, workspace, focus, move, resize, kill, etc.)
- **Launch-to-workspace** - spawn windows on any workspace without the need of switching to it first

### Integration
- **XWayland** support via an in-house X window manager - no external tools
- `ext-idle-notify-v1` for idle/lock management
- `wlr-layer-shell` for panels, overlays, and lock screens
- `wlr-foreign-toplevel-management` for taskbars and window switchers
- Screencopy for screenshots
- Built-in `xdg-desktop-portal` ScreenCast backend with a pure-Rust PipeWire client - screensharing needs no `xdg-desktop-portal-wlr`
- Clipboard via `zwlr_data_control_v1`
- DPMS wake on input

## Configuration

Carrot uses [KDL](https://kdl.dev) for configuration, with full NixOS and Home Manager module integration.

Lua configuration is also officially supported as an opt-in alternative - KDL stays the default. It's a runtime switch (no rebuild needed), and on NixOS / Home Manager you declare it through the module.

```kdl
general {
    layout "dwindle"
    allow-tearing true
    border-size 3
}

decoration {
    rounding 10
    blur {
        enabled true
        size 8
        passes 2
    }
    shadow {
        enabled true
        range 20
    }
}

animations {
    bezier "standard" 0.2 0.0 0.0 1.0

    animation "windowsIn" {
        enabled true
        speed 5
        curve "standard"
    }
}

window-rule {
    match class="steam_app_.*"
    immediate true
    idle-inhibit "always"
}

bind "Super" "Return" "exec" "foot"
```

<details>
<summary>NixOS / Home Manager</summary>

```nix
# flake.nix inputs
inputs.carrot.url = "github:flammablebunny/carrot";

# NixOS module
programs.carrot.enable = true;

# Home Manager module
wayland.windowManager.carrot = {
  enable = true;
  configFormat = "lua"; # optional, defaults to "kdl"
  settings = {
    general = {
      layout = "dwindle";
      allow_tearing = true;
    };
    decoration = {
      rounding = 10;
      blur = { enabled = true; size = 8; passes = 2; };
    };
  };
};
```

</details>

### Screensharing

Carrot serves the ScreenCast portal itself and draws no chooser of its own,
so the share menu is whatever you want it to be. Every share request passes
one consent step:

- a **restore token** from an earlier share skips straight through,
- otherwise the configured **picker** command runs,
- with no picker configured, the next **left click** picks the window (or
  output) under the cursor - Escape or any other button cancels, and the
  click never reaches the app.

The picker is any program: it receives one JSON candidate per line on stdin
and answers with the chosen `id` on stdout (empty output or exit without an
answer cancels the share).

```json
{"kind":"output","id":"o:DP-1","name":"DP-1","width":2560,"height":1440,"x":0,"y":0}
{"kind":"window","id":"w:42","app_id":"foot","title":"~","workspace":2}
{"kind":"workspace","id":"ws:2","index":2,"output":"DP-1","active":true}
```

Workspace entries are a carrot extra - sharing one follows the workspace
across outputs instead of pinning to a monitor. Theming lives entirely in
the picker program: a dmenu-style script works anywhere,

```kdl
general {
    picker "carrot-share-menu"
}
```

```sh
#!/bin/sh
# carrot-share-menu: candidates in, one id out
jq -r '[.id, .kind, (.name // .app_id // ""), (.title // "")] | @tsv' \
    | fuzzel --dmenu \
    | cut -f1
```

while a quickshell (or any shell) setup can forward the candidate list to
its own styled menu over IPC and print the id it picked. The picker path
never grabs the seat, so shell-drawn menus receive their clicks normally.

## Building

### With Nix

```sh
nix build github:flammablebunny/carrot
```

### With Cargo

```sh
cargo build --release
```

System dependencies: just `vulkan-loader` (dlopened at runtime, never linked) and a Vulkan driver (ICD) for your GPU. Carrot links **zero C** - no `libdrm`, `libinput`, `libseat`, `libxkbcommon`, or `libwayland`; the DRM, input, and session stacks are all hand-rolled over raw syscalls. `kbvm` parses an embedded default US keymap, so carrot boots with no XKB data on disk; it only reads `xkeyboard-config` when you configure a non-default layout, wired up automatically by the Nix build.

## Acknowledgments

Carrot is built from scratch, with no framework or compositor library dependency. References and inspiration:
- [Jay](https://github.com/mahkoh/jay) - proved a from-scratch Vulkan Wayland compositor in Rust is viable for a solo dev
- [Niri](https://github.com/niri-wm/niri) - reference for animation design and window rules
- [ash](https://github.com/ash-rs/ash) - Vulkan bindings for Rust

## License

GPL-3.0 - see [LICENSE](LICENSE) for details.

## Communtity, Dev Discussions, and Support

[Discord Server](https://discord.gg/fQyxq4JHpR)
