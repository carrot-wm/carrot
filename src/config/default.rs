// the embedded default config: written to disk on first run, the startup
// fallback for a broken user file, and the source of Config::default().
// the text is the truth; nothing here is mirrored by hand in Rust.

use std::sync::OnceLock;

pub const DEFAULT_CONFIG: &str = r##"// carrot config - KDL v2 (kdl.dev)
// This file was written by carrot on first run; edit freely.
//
// The three habits worth learning on day one:
//   - reload a running session:      burrow reload
//   - test an edit before reloading: carrot check-config
//   - disable any node or block with the /- "slashdash":
//     /-output "DP-1" { ... }
//
// Commented lines are examples, not defaults. A broken file never
// locks you out: carrot falls back to built-in defaults and keeps
// the parse errors readable via `burrow errors`.
//
// The full schema, and the Lua flavor of this file (carrot.lua):
//   https://carrotwm.org/docs/configuration/
//   https://carrotwm.org/docs/lua-config/

// Configs can span multiple files. Paths resolve against the file
// that names them, and included files may include further files:
// include "monitors.kdl"
// include "binds.kdl"

// -- input --
// docs: https://carrotwm.org/docs/input/

input {
    keyboard {
        xkb {
            // Unset means "whatever the system says"; set to override.
            // layout "us"
            // variant "colemak"
            // options "compose:ralt,caps:escape"
        }
        // Hold a key: wait repeat-delay ms, then repeat-rate per second.
        repeat-delay 600
        repeat-rate 25
        // Turn the numpad on at login:
        // numlock
    }
    touchpad {
        // natural-scroll
        // accel-profile "flat"
        // accel-speed 0.2
    }
    mouse {
        // accel-profile "flat"
        // accel-speed 0.0
    }
    // Per-device overrides, matched by name substring:
    // device "razer viper" { accel-speed -0.5; dpi 1600 }

    // The Mod in every chord below; "super" or "alt". Test-driving
    // carrot nested inside another desktop? "alt" keeps the host's
    // super binds out of the way.
    // mod-key "super"
}

// -- outputs --
// Names are connectors ("DP-1") or a "make model serial" string;
// `burrow monitors` lists yours. Monitors you don't mention pick
// their preferred mode and line up left to right.
// docs: https://carrotwm.org/docs/workspaces/

// output "DP-1" {
//     mode "2560x1440@240"
//     scale 1
//     position x=0 y=0
//     variable-refresh-rate on-demand=#true
//     // allow-tearing
//     // off
// }

// -- layout --
// docs: https://carrotwm.org/docs/tiling/

layout {
    // "dwindle" splits the focused window in two; "scrolling" lays
    // columns on an endless strip. Swap the two lines to change, or
    // flip one workspace at runtime with Mod+T.
    mode "dwindle"
    // mode "scrolling"
    gaps-in 5
    gaps-out 10
    border {
        width 2
        active-color "#89b4fa"
        inactive-color "#585b70"
    }
    // Keep floating windows above a fullscreen one:
    // float-above-fullscreen

    // Which way the workspace stack runs on dwindle: "horizontal"
    // or "vertical". A scrolling workspace always stacks vertically
    // - its strip owns the horizontal axis.
    // workspace-axis "horizontal"

    // Tuning for the scrolling mode:
    // scrolling {
    //     // the widths cycle-column-width steps through
    //     preset-widths 0.333 0.5 0.667
    //     default-width 0.5
    //     // "never", "always" or "on-overflow"
    //     center-focus "never"
    // }
}

// -- decoration --
// The render candy. Everything here can be overridden per window
// in the window-rule blocks further down.

decoration {
    rounding 10
    shadow {
        size 20
        color "#00000070"
        offset 0 4
    }
    // Dim everything except the focused window:
    // dim-inactive 0.1
    // Backdrop blur for windows and layers that opt in via rules;
    // costs real GPU time on large screens.
    // blur {
    //     passes 2
    //     size 3
    //     noise 0.01
    // }
}

// -- animations --
// Per-kind motion is a spring or an ease; kinds you leave unset
// inherit the section's default spring. styles: windows/layers take
// popin, fade, slide; workspace-switch takes slide, slidevert,
// fade, slidefade, slidefadevert.
// docs: https://carrotwm.org/docs/animations/

animations {
    // off
    // Global speed dial; 2.0 runs everything at half speed.
    slowdown 1.0
    // Name a bezier once, use it as any ease's curve=:
    // curve "overshot" 0.05 0.9 0.1 1.05
    spring damping-ratio=1.0 stiffness=800 epsilon=0.0001
    window-open { ease duration-ms=150 curve="ease-out-expo"; style "popin" perc=80 }
    window-close { ease duration-ms=150 curve="ease-out-quad"; style "popin" perc=80 }
    workspace-switch { spring damping-ratio=1.0 stiffness=1000 epsilon=0.0001; style "slide" }
    border-color { ease duration-ms=200 curve="ease-out-quad" }
    // The remaining kinds: window-move, window-resize,
    // view-movement, layer-open, layer-close. e.g.
    // window-move { spring damping-ratio=0.85 stiffness=600 epsilon=0.0001 }
}

// -- cursor --

cursor {
    // xcursor-theme "Adwaita"
    // xcursor-size 24
    // If the cursor ever renders wrong, force software cursors:
    // software
}

// -- misc --

// Ask apps to drop their own titlebars; carrot draws the borders.
prefer-no-csd

// Programs that start with the session:
// spawn-at-startup "quickshell"
// spawn-sh-at-startup "wl-paste --watch cliphist store"

// Session environment; #null unsets a variable:
// environment {
//     QT_QPA_PLATFORM "wayland"
//     DISPLAY #null
// }

// The screenshare picker is any dmenu-ish list command:
// docs: https://carrotwm.org/docs/screensharing/
// screencast {
//     picker "fuzzel-pick"
// }

// -- rules --
// Matchers are regexes. Properties inside one match AND together,
// several match lines OR, excludes veto.
// docs: https://carrotwm.org/docs/window-rules/

// window-rule {
//     match app-id=#"^discord$"#
//     open-floating #true
//     // screenshares, recordings and screenshots see a black box:
//     no-capture
// }
// window-rule {
//     match title=#"(?i)picture.in.picture"#
//     open-floating #true
//     open-centered
//     default-size 640 360
// }
// The full rule vocabulary: open-floating, open-on-workspace,
// open-centered, default-size, opacity, allow-tearing, rounding,
// shadow, dim, blur, no-anim, no-capture, animation.

// layer-rule {
//     match namespace=#"^launcher$"#
//     blur #true
//     // backdrop only where the surface's own alpha reaches the gate;
//     // argb surfaces only (xrgb reads as fully opaque)
//     ignore-alpha 0.1
//     // shells that remap layers on state changes: skip open/close styles
//     no-anim
// }

// Per-window key remaps, for games that hardcode their keys:
// remap "example-game" {
//     match app-id="steam_app_250900"
//     map "Alt_R" "Left"
// }

// -- binds --
// Chords are Mod+Key; Mod resolves through input.mod-key above.
// Extras ride the chord as properties:
//   repeat=#true             hold to repeat
//   on="release"             fire on key-up
//   cooldown-ms=200          rate-limit
//   allow-when-locked=#true  works on the lock screen
//   title="..."              label for shell hotkey overlays
// docs: https://carrotwm.org/docs/binds/

binds {
    Mod+Return title="Open a terminal" { spawn "foot"; }
    Mod+D title="App launcher" { spawn "fuzzel"; }
    Mod+Q title="Close window" { close-window; }
    Mod+F title="Fullscreen" { toggle-fullscreen; }
    Mod+Space title="Float / tile" { toggle-floating; }
    Mod+Shift+E title="Quit carrot" { quit; }

    // Focus moves by direction; arrows and home row both work.
    Mod+Left { focus-left; }
    Mod+Right { focus-right; }
    Mod+Up { focus-up; }
    Mod+Down { focus-down; }
    Mod+H { focus-left; }
    Mod+J { focus-down; }
    Mod+K { focus-up; }
    Mod+L { focus-right; }
    // Same directions with Shift trade the two windows' slots.
    Mod+Shift+Left { swap-left; }
    Mod+Shift+Right { swap-right; }
    Mod+Shift+Up { swap-up; }
    Mod+Shift+Down { swap-down; }
    Mod+Shift+H { swap-left; }
    Mod+Shift+J { swap-down; }
    Mod+Shift+K { swap-up; }
    Mod+Shift+L { swap-right; }
    // Cycle through the workspace in map order.
    Mod+Tab { focus-next; }
    Mod+Shift+Tab { focus-prev; }

    // Lean on the split the focused window sits in.
    Mod+Minus repeat=#true { adjust-split-ratio "-0.05"; }
    Mod+Equal repeat=#true { adjust-split-ratio "+0.05"; }

    // Hold the chord and move the mouse; release ends the grab.
    Mod+MouseLeft { pointer-move; }
    Mod+MouseRight { pointer-resize; }

    // Column verbs for the scrolling layout, ready to uncomment;
    // they sit idle on a dwindle workspace.
    Mod+T title="Toggle layout" { set-layout "toggle"; }
    // consume-or-expel pulls the neighbor window into the focused
    // column - two windows stacked - or pops the focused one back
    // out into its own; focus-up/down walks the stack.
    // Mod+BracketLeft { consume-or-expel-left; }
    // Mod+BracketRight { consume-or-expel-right; }
    // Whole columns trade places along the strip:
    // Mod+Ctrl+Left { move-column-left; }
    // Mod+Ctrl+Right { move-column-right; }
    // Mod+R { cycle-column-width; }
    // Mod+Shift+R { cycle-column-width-back; }
    // Mod+W { toggle-full-width; }
    // Mod+C { center-column; }

    Mod+1 { focus-workspace 1; }
    Mod+2 { focus-workspace 2; }
    Mod+3 { focus-workspace 3; }
    Mod+4 { focus-workspace 4; }
    Mod+5 { focus-workspace 5; }
    Mod+6 { focus-workspace 6; }
    Mod+7 { focus-workspace 7; }
    Mod+8 { focus-workspace 8; }
    Mod+9 { focus-workspace 9; }
    // Moving a window brings focus along; focus=#false leaves it.
    Mod+Shift+1 { move-to-workspace 1; }
    Mod+Shift+2 { move-to-workspace 2; }
    Mod+Shift+3 { move-to-workspace 3; }
    Mod+Shift+4 { move-to-workspace 4; }
    Mod+Shift+5 { move-to-workspace 5; }
    Mod+Shift+6 { move-to-workspace 6; }
    Mod+Shift+7 { move-to-workspace 7; }
    Mod+Shift+8 { move-to-workspace 8; }
    Mod+Shift+9 { move-to-workspace 9; }
    // "+N"/"-N" jump relative to the active workspace, wrapping.
    Mod+PageDown { focus-workspace "+1"; }
    Mod+PageUp { focus-workspace "-1"; }

    // Screenshots want grim; the region shot also slurp + wl-clipboard.
    Print title="Screenshot" { spawn-sh "mkdir -p ~/Pictures/Screenshots && grim ~/Pictures/Screenshots/$(date +%F_%H-%M-%S).png"; }
    Mod+Shift+S title="Region to clipboard" { spawn-sh #"grim -g "$(slurp)" - | wl-copy"#; }

    XF86AudioRaiseVolume allow-when-locked=#true { spawn "wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%+"; }
    XF86AudioLowerVolume allow-when-locked=#true { spawn "wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%-"; }
    XF86AudioMute allow-when-locked=#true { spawn "wpctl" "set-mute" "@DEFAULT_AUDIO_SINK@" "toggle"; }
    XF86AudioMicMute allow-when-locked=#true { spawn "wpctl" "set-mute" "@DEFAULT_AUDIO_SOURCE@" "toggle"; }
    XF86AudioPlay allow-when-locked=#true { spawn "playerctl" "play-pause"; }
    XF86AudioNext allow-when-locked=#true { spawn "playerctl" "next"; }
    XF86AudioPrev allow-when-locked=#true { spawn "playerctl" "previous"; }
    XF86MonBrightnessUp allow-when-locked=#true { spawn "brightnessctl" "set" "5%+"; }
    XF86MonBrightnessDown allow-when-locked=#true { spawn "brightnessctl" "set" "5%-"; }
}

// -- debug --

// debug {
//     render-drm-device "/dev/dri/card0"
//     ignore-drm-device "card1"
// }
"##;

pub fn embedded() -> &'static super::Config {
    static ONCE: OnceLock<super::Config> = OnceLock::new();
    ONCE.get_or_init(|| {
        super::kdl::parse_bare(DEFAULT_CONFIG)
            .unwrap_or_else(|e| panic!("embedded default config must parse clean: {e:?}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_parses_clean() {
        let c = embedded();
        assert_eq!(c.layout.gaps_in, 5);
        assert_eq!(c.layout.gaps_out, 10);
        assert_eq!(c.layout.border.width, 2);
        assert_eq!(c.input.keyboard.repeat_delay, 600);
        assert_eq!(c.input.keyboard.repeat_rate, 25);
        assert!(c.binds.len() > 25, "the default is not bind-less");
        assert!(c.outputs.is_empty(), "outputs are examples only");
        assert!(c.rules.is_empty() && c.remaps.is_empty());
        assert!(c.spawns.is_empty(), "autostart is the user's call");
        assert!(c.prefer_no_csd, "carrot draws the borders");
        assert_eq!(c.decoration.rounding, 10);
        assert!(c.decoration.shadow.is_some());
        assert!(c.decoration.blur.is_none(), "blur is opt-in");
    }

    #[test]
    fn default_config_is_the_embedded_one() {
        assert_eq!(&super::super::Config::default(), embedded());
    }
}
