// the embedded default config: written to disk on first run, the startup
// fallback for a broken user file, and the source of Config::default().
// the text is the truth; nothing here is mirrored by hand in Rust.

use std::sync::OnceLock;

pub const DEFAULT_CONFIG: &str = r##"// carrot config - KDL v2 (kdl.dev)
// This file was written by carrot on first run; edit freely.
// Commented lines are examples, not defaults. Disable any whole block
// with the /- "slashdash": /-output "DP-1" { ... }
// Reload with: burrow reload

input {
    keyboard {
        xkb {
            // layout "us"
            // variant "colemak"
            // options "compose:ralt"
        }
        repeat-delay 600
        repeat-rate 25
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
    // per-device overrides, matched by name substring:
    // device "razer viper" { accel-speed -0.5; dpi 1600 }
    // the Mod used in binds below; "super" or "alt"
    // mod-key "super"
}

// output "DP-1" {
//     mode "2560x1440@240"
//     scale 1
//     position x=0 y=0
//     variable-refresh-rate on-demand=#true
//     // allow-tearing
//     // off
// }

layout {
    mode "dwindle"
    gaps-in 5
    gaps-out 10
    border {
        width 2
        active-color "#89b4fa"
        inactive-color "#585b70"
    }
    // float-above-fullscreen
}

cursor {
    // xcursor-theme "Adwaita"
    // xcursor-size 24
    // software
}

// per-kind motion is a spring or an ease; unset kinds inherit the
// section's default spring. styles: windows/layers take popin, fade,
// slide; workspace-switch takes slide, slidevert, fade, slidefade,
// slidefadevert
animations {
    // off
    slowdown 1.0
    // curve "overshot" 0.05 0.9 0.1 1.05
    spring damping-ratio=1.0 stiffness=800 epsilon=0.0001
    window-open { ease duration-ms=150 curve="ease-out-expo"; style "popin" perc=80 }
    window-close { ease duration-ms=150 curve="ease-out-quad"; style "popin" perc=80 }
    workspace-switch { spring damping-ratio=1.0 stiffness=1000 epsilon=0.0001; style "slide" }
    border-color { ease duration-ms=200 curve="ease-out-quad" }
}

// prefer-no-csd

// spawn-at-startup "quickshell"
// spawn-sh-at-startup "wl-paste --watch cliphist store"

// environment {
//     DISPLAY #null
// }

// screencast {
//     picker "fuzzel-pick"
// }

// window-rule {
//     match app-id=#"^org\.keepassxc\."#
//     open-floating #true
// }

// layer-rule {
//     match namespace=#"^launcher$"#
// }

// remap "example-game" {
//     match app-id="steam_app_250900"
//     map "Alt_R" "Left"
// }

binds {
    Mod+Return { spawn "foot"; }
    Mod+Q { close-window; }
    Mod+F { toggle-fullscreen; }
    Mod+Space { toggle-floating; }
    Mod+Shift+E { quit; }

    Mod+Left { focus-left; }
    Mod+Right { focus-right; }
    Mod+Up { focus-up; }
    Mod+Down { focus-down; }
    Mod+Shift+Left { swap-left; }
    Mod+Shift+Right { swap-right; }
    Mod+Shift+Up { swap-up; }
    Mod+Shift+Down { swap-down; }

    Mod+1 { focus-workspace 1; }
    Mod+2 { focus-workspace 2; }
    Mod+3 { focus-workspace 3; }
    Mod+4 { focus-workspace 4; }
    Mod+5 { focus-workspace 5; }
    Mod+6 { focus-workspace 6; }
    Mod+7 { focus-workspace 7; }
    Mod+8 { focus-workspace 8; }
    Mod+9 { focus-workspace 9; }
    Mod+Shift+1 { move-to-workspace 1; }
    Mod+Shift+2 { move-to-workspace 2; }
    Mod+Shift+3 { move-to-workspace 3; }
    Mod+Shift+4 { move-to-workspace 4; }
    Mod+Shift+5 { move-to-workspace 5; }
    Mod+Shift+6 { move-to-workspace 6; }
    Mod+Shift+7 { move-to-workspace 7; }
    Mod+Shift+8 { move-to-workspace 8; }
    Mod+Shift+9 { move-to-workspace 9; }

    XF86AudioRaiseVolume allow-when-locked=#true { spawn "wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%+"; }
    XF86AudioLowerVolume allow-when-locked=#true { spawn "wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%-"; }
    XF86AudioMute allow-when-locked=#true { spawn "wpctl" "set-mute" "@DEFAULT_AUDIO_SINK@" "toggle"; }
}

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
    }

    #[test]
    fn default_config_is_the_embedded_one() {
        assert_eq!(&super::super::Config::default(), embedded());
    }
}
