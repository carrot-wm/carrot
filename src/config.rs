// kdl v2 config. parse errors are fatal at startup and rejected on reload -
// never silently fall back to defaults. reload parses fresh, diffs, applies;
// each key is hot (apply live) or cold (log that a restart is needed).

use serde::{Deserialize, Serialize};
pub(crate) use ::kdl::{KdlDocument, KdlNode};

pub mod kdl;
pub mod lua;

pub use kdl::parse;

// action names double as the ipc vocabulary; every bind has a wire twin
// by construction
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Workspace(usize),
    /// signed jump from the active workspace, wrapping
    WorkspaceRel(i32),
    SendToWorkspace(usize),
    MoveToWorkspace(usize),
    ToggleFullscreen,
    ToggleFloating,
    CloseWindow,
    FocusNext,
    FocusPrev,
    FocusDir(Dir),
    SwapDir(Dir),
    /// nudge the focused window's parent split; signed fraction of the span
    SplitRatio(f64),
    Spawn(String),
    Quit,
}

// press is the only kind the seat fires today; the rest parse and wait
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum BindKind {
    Press,
    Release,
    Repeat,
    LockSafe,
    Mouse,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bind {
    pub mods: u32,
    pub key: u32,
    pub action: Action,
    pub kind: BindKind,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct InputCfg {
    pub accel_profile: Option<String>,
    pub natural_scroll: bool,
    pub tap: bool,
    pub dwt: bool,
    pub layout: Option<String>,
    pub numlock: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlurCfg {
    pub enabled: bool,
    pub size: i32,
    pub passes: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShadowCfg {
    pub enabled: bool,
    pub range: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecorationCfg {
    pub rounding: i32,
    pub blur: BlurCfg,
    pub shadow: ShadowCfg,
}

impl Default for DecorationCfg {
    fn default() -> DecorationCfg {
        DecorationCfg {
            rounding: 0,
            blur: BlurCfg { enabled: false, size: 8, passes: 2 },
            shadow: ShadowCfg { enabled: false, range: 20 },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnimationCfg {
    pub enabled: bool,
    pub speed: f64,
    pub curve: String,
    // slide, slidefadevert, popin, fade
    pub style: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AnimationsCfg {
    // name -> cubic bezier control points
    pub beziers: Vec<(String, [f64; 4])>,
    // name -> mass, stiffness, damping
    pub springs: Vec<(String, [f64; 3])>,
    pub animations: Vec<(String, AnimationCfg)>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct WindowRule {
    // selectors; class and title are regexes
    pub match_class: Option<String>,
    pub match_title: Option<String>,
    pub match_fullscreen: Option<bool>,
    pub match_xwayland: Option<bool>,
    pub match_floating: Option<bool>,
    // effects
    pub floating: bool,
    pub tile: bool,
    pub workspace: Option<usize>,
    pub immediate: bool,
    pub idle_inhibit: Option<String>,
    pub opacity: Option<f64>,
    pub size: Option<(i32, i32)>,
    pub center: bool,
    pub rounding: Option<i32>,
    pub blur: Option<bool>,
    pub shadow: Option<bool>,
    pub dim: Option<f64>,
    pub pin: bool,
    pub keep_aspect_ratio: bool,
    pub focus_steal: bool,
    // "redirect" (background only) or "passthrough" (both), plus the keys
    pub redirect_mode: Option<String>,
    pub redirect_keys: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct LayerRule {
    pub match_namespace: Option<String>,
    pub animation: Option<String>,
    pub blur: Option<bool>,
    pub ignore_alpha: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputCfg {
    pub name: String,
    pub vrr: Option<String>,
    pub gpu: Option<String>,
    pub scale: Option<f64>,
    /// "2560x1440@240" or "2560x1440"; picks the closest advertised mode
    pub mode: Option<(u32, u32, Option<u32>)>,
}

/// secondary gpus can skip bringing up a renderer (import-only)
#[derive(Clone, Debug, PartialEq)]
pub struct GpuCfg {
    pub name: String,
    pub skip_renderer: bool,
}

/// focus-activated key translations; criteria AND together
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RemapProfile {
    pub name: String,
    pub class: Option<String>,
    pub title: Option<String>,
    /// "x11" or "wayland"
    pub win_type: Option<String>,
    pub pid: Option<i32>,
    /// 1-based, as written in config
    pub workspace: Option<usize>,
    /// evdev from -> to
    pub maps: Vec<(u32, u32)>,
}

/// per-device overrides, matched by normalized name substring
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRule {
    pub name: String,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<String>,
    pub natural_scroll: Option<bool>,
    /// the mouse's real dpi; raw deltas scale to a 1000dpi baseline so
    /// sensitivity stays device-independent
    pub dpi: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub gaps_in: i32,
    pub gaps_out: i32,
    pub border: i32,
    pub border_focused: [f32; 4],
    pub border_unfocused: [f32; 4],
    pub repeat_rate: i32,
    pub repeat_delay: i32,
    pub float_above_fullscreen: bool,
    pub layout: String,
    pub allow_tearing: bool,
    /// composite the cursor instead of using the hardware plane; the
    /// escape hatch for planes that misbehave (joined-pipe modes)
    pub input: InputCfg,
    pub devices: Vec<DeviceRule>,
    pub decoration: DecorationCfg,
    pub animations: AnimationsCfg,
    pub rules: Vec<WindowRule>,
    pub layer_rules: Vec<LayerRule>,
    pub outputs: Vec<OutputCfg>,
    pub gpus: Vec<GpuCfg>,
    pub binds: Vec<Bind>,
    pub remaps: Vec<RemapProfile>,
    pub submaps: Vec<(String, Vec<Bind>)>,
    /// named scratchpads; the command spawns on first toggle
    pub specials: Vec<(String, Option<String>)>,
}

// mod bits match the seat's exact-set matcher
pub const M_SHIFT: u32 = 1 << 0;
pub const M_CTRL: u32 = 1 << 2;
pub const M_ALT: u32 = 1 << 3;
pub const M_SUPER: u32 = 1 << 6;

// neutral zeros only - carrot ships no opinions; every visible choice
// comes from the user's file. kernel repeat timings are the one
// exception (a keyboard that never repeats reads as broken, not unset)
impl Default for Config {
    fn default() -> Config {
        Config {
            gaps_in: 0,
            gaps_out: 0,
            border: 0,
            border_focused: [0.0; 4],
            border_unfocused: [0.0; 4],
            repeat_rate: 25,
            repeat_delay: 600,
            float_above_fullscreen: false,
            layout: "dwindle".to_string(),
            allow_tearing: false,
            input: InputCfg::default(),
            devices: Vec::new(),
            decoration: DecorationCfg::default(),
            animations: AnimationsCfg::default(),
            rules: Vec::new(),
            layer_rules: Vec::new(),
            outputs: Vec::new(),
            gpus: Vec::new(),
            binds: Vec::new(),
            remaps: Vec::new(),
            submaps: Vec::new(),
            specials: Vec::new(),
        }
    }
}

fn config_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("carrot")
}

pub fn config_path() -> std::path::PathBuf {
    // the language is picked by which file exists; kdl wins a tie
    let dir = config_dir();
    let kdl = dir.join("carrot.kdl");
    if kdl.exists() {
        return kdl;
    }
    let lua = dir.join("carrot.lua");
    if lua.exists() { lua } else { kdl }
}

// a missing file means defaults; an unreadable or unparsable one is an error
pub fn load() -> Result<Config, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let parsed = if path.extension().is_some_and(|e| e == "lua") {
        lua::parse(&text)
    } else {
        parse(&text)
    };
    parsed.map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_mode(s: &str) -> Option<(u32, u32, Option<u32>)> {
    let (res, hz) = match s.split_once('@') {
        Some((r, h)) => (r, Some(h.parse().ok()?)),
        None => (s, None),
    };
    let (w, h) = res.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, hz))
}

fn parse_mods(spec: &str) -> Result<u32, String> {
    let mut mods = 0;
    for part in spec.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "shift" => mods |= M_SHIFT,
            "ctrl" | "control" => mods |= M_CTRL,
            "alt" => mods |= M_ALT,
            "super" | "meta" | "mod" | "logo" => mods |= M_SUPER,
            "" => {}
            other => return Err(format!("unknown modifier \"{other}\"")),
        }
    }
    Ok(mods)
}

// the full evdev keyboard map, KEY_* names lowercased, plus the common
// aliases; straight from input-event-codes.h
fn keycode(name: &str) -> Option<u32> {
    let k = match name {
        "esc" | "escape" => 1,
        "1" => 2, "2" => 3, "3" => 4, "4" => 5, "5" => 6,
        "6" => 7, "7" => 8, "8" => 9, "9" => 10, "0" => 11,
        "minus" => 12, "equal" => 13, "backspace" => 14, "tab" => 15,
        "q" => 16, "w" => 17, "e" => 18, "r" => 19, "t" => 20,
        "y" => 21, "u" => 22, "i" => 23, "o" => 24, "p" => 25,
        "leftbrace" | "bracketleft" => 26, "rightbrace" | "bracketright" => 27,
        "enter" | "return" => 28, "leftctrl" | "ctrl_l" => 29,
        "a" => 30, "s" => 31, "d" => 32, "f" => 33, "g" => 34,
        "h" => 35, "j" => 36, "k" => 37, "l" => 38,
        "semicolon" => 39, "apostrophe" => 40, "grave" => 41,
        "leftshift" | "shift_l" => 42, "backslash" => 43,
        "z" => 44, "x" => 45, "c" => 46, "v" => 47, "b" => 48,
        "n" => 49, "m" => 50,
        "comma" => 51, "dot" | "period" => 52, "slash" => 53,
        "rightshift" | "shift_r" => 54, "kpasterisk" => 55, "leftalt" | "alt_l" => 56,
        "space" => 57, "capslock" => 58,
        "f1" => 59, "f2" => 60, "f3" => 61, "f4" => 62, "f5" => 63,
        "f6" => 64, "f7" => 65, "f8" => 66, "f9" => 67, "f10" => 68,
        "numlock" => 69, "scrolllock" => 70,
        "kp7" => 71, "kp8" => 72, "kp9" => 73, "kpminus" => 74,
        "kp4" => 75, "kp5" => 76, "kp6" => 77, "kpplus" => 78,
        "kp1" => 79, "kp2" => 80, "kp3" => 81, "kp0" => 82, "kpdot" => 83,
        "zenkakuhankaku" => 85, "102nd" => 86,
        "f11" => 87, "f12" => 88,
        "ro" => 89, "katakana" => 90, "hiragana" => 91, "henkan" => 92,
        "katakanahiragana" => 93, "muhenkan" => 94, "kpjpcomma" => 95,
        "kpenter" => 96, "rightctrl" | "ctrl_r" => 97, "kpslash" => 98, "sysrq" => 99,
        "rightalt" | "alt_r" => 100, "linefeed" => 101,
        "home" => 102, "up" => 103, "pageup" => 104, "left" => 105,
        "right" => 106, "end" => 107, "down" => 108, "pagedown" => 109,
        "insert" => 110, "delete" => 111, "macro" => 112,
        "mute" => 113, "volumedown" => 114, "volumeup" => 115,
        "power" => 116, "kpequal" => 117, "kpplusminus" => 118,
        "pause" => 119, "scale" => 120, "kpcomma" => 121,
        "hangeul" | "hanguel" => 122, "hanja" => 123, "yen" => 124,
        "leftmeta" | "super_l" => 125, "rightmeta" | "super_r" => 126, "compose" | "menu" => 127,
        "stop" => 128, "again" => 129, "props" => 130, "undo" => 131,
        "front" => 132, "copy" => 133, "open" => 134, "paste" => 135,
        "find" => 136, "cut" => 137, "help" => 138,
        "calc" => 140, "setup" => 141, "sleep" => 142, "wakeup" => 143,
        "file" => 144, "sendfile" => 145, "deletefile" => 146, "xfer" => 147,
        "prog1" => 148, "prog2" => 149, "www" => 150, "msdos" => 151,
        "coffee" | "screenlock" => 152, "rotate_display" => 153,
        "cyclewindows" => 154, "mail" => 155, "bookmarks" => 156,
        "computer" => 157, "back" => 158, "forward" => 159,
        "closecd" => 160, "ejectcd" => 161, "ejectclosecd" => 162,
        "nextsong" => 163, "playpause" => 164, "previoussong" => 165,
        "stopcd" => 166, "record" => 167, "rewind" => 168, "phone" => 169,
        "iso" => 170, "config" => 171, "homepage" => 172, "refresh" => 173,
        "exit" => 174, "move" => 175, "edit" => 176,
        "scrollup" => 177, "scrolldown" => 178,
        "kpleftparen" => 179, "kprightparen" => 180,
        "new" => 181, "redo" => 182,
        "f13" => 183, "f14" => 184, "f15" => 185, "f16" => 186,
        "f17" => 187, "f18" => 188, "f19" => 189, "f20" => 190,
        "f21" => 191, "f22" => 192, "f23" => 193, "f24" => 194,
        "playcd" => 200, "pausecd" => 201, "prog3" => 202, "prog4" => 203,
        "all_applications" | "dashboard" => 204, "suspend" => 205,
        "close" => 206, "play" => 207, "fastforward" => 208,
        "bassboost" => 209, "print" => 210, "hp" => 211, "camera" => 212,
        "sound" => 213, "question" => 214, "email" => 215, "chat" => 216,
        "search" => 217, "connect" => 218, "finance" => 219, "sport" => 220,
        "shop" => 221, "alterase" => 222, "cancel" => 223,
        "brightnessdown" => 224, "brightnessup" => 225, "media" => 226,
        "switchvideomode" => 227, "kbdillumtoggle" => 228,
        "kbdillumdown" => 229, "kbdillumup" => 230,
        "send" => 231, "reply" => 232, "forwardmail" => 233, "save" => 234,
        "documents" => 235, "battery" => 236, "bluetooth" => 237,
        "wlan" => 238, "uwb" => 239,
        "video_next" => 241, "video_prev" => 242,
        "brightness_cycle" => 243, "brightness_auto" | "brightness_zero" => 244,
        "display_off" => 245, "wwan" | "wimax" => 246, "rfkill" => 247,
        "micmute" => 248,
        // mouse buttons, for type="mouse" binds
        "btn_left" | "mouse_left" => 272, "btn_right" | "mouse_right" => 273,
        "btn_middle" | "mouse_middle" => 274,
        "btn_side" => 275, "btn_extra" => 276,
        _ => return None,
    };
    Some(k)
}

/// merged window-rule effects for one window at map time; rules apply
/// in file order, later scalar wins, flags accumulate
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuleFx {
    pub floating: Option<bool>,
    pub workspace: Option<usize>,
    pub immediate: bool,
    pub opacity: Option<f64>,
    pub size: Option<(i32, i32)>,
    pub center: bool,
}

pub fn rule_effects(
    cfg: &Config,
    class: &str,
    title: &str,
    xwayland: bool,
    fullscreen: bool,
) -> RuleFx {
    let mut fx = RuleFx::default();
    let matches_re = |pat: &Option<String>, hay: &str| -> bool {
        match pat {
            None => true,
            // validated at parse time; a stale failure just never matches
            Some(p) => regex_lite::Regex::new(p).is_ok_and(|re| re.is_match(hay)),
        }
    };
    for r in cfg.rules.iter() {
        if !matches_re(&r.match_class, class) || !matches_re(&r.match_title, title) {
            continue;
        }
        if let Some(want) = r.match_xwayland {
            if want != xwayland {
                continue;
            }
        }
        if let Some(want) = r.match_fullscreen {
            if want != fullscreen {
                continue;
            }
        }
        if r.floating {
            fx.floating = Some(true);
        }
        if r.tile {
            fx.floating = Some(false);
        }
        if let Some(ws) = r.workspace {
            fx.workspace = Some(ws);
        }
        fx.immediate |= r.immediate;
        if let Some(o) = r.opacity {
            fx.opacity = Some(o);
        }
        if let Some(sz) = r.size {
            fx.size = Some(sz);
        }
        fx.center |= r.center;
    }
    fx
}

/// the translation for one key under the focused window, if any profile
/// matches. criteria AND; first matching profile wins
pub fn resolve_remap(
    cfg: &Config,
    class: &str,
    title: &str,
    is_x11: bool,
    pid: i32,
    ws_1based: usize,
    key: u32,
) -> Option<u32> {
    for p in cfg.remaps.iter() {
        if let Some(c) = &p.class {
            if c != class {
                continue;
            }
        }
        if let Some(t) = &p.title {
            if !title.contains(t.as_str()) {
                continue;
            }
        }
        if let Some(ty) = &p.win_type {
            let want_x11 = ty == "x11";
            if want_x11 != is_x11 {
                continue;
            }
        }
        if let Some(want) = p.pid {
            if want != pid {
                continue;
            }
        }
        if let Some(want) = p.workspace {
            if want != ws_1based {
                continue;
            }
        }
        for (from, to) in p.maps.iter() {
            if *from == key {
                return Some(*to);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_by_regex_and_merge_in_order() {
        let cfg = parse(
            r##"
            window-rule {
                match class="^steam_app_.*$"
                immediate #true
                opacity 0.9
            }
            window-rule {
                match class="^steam_app_250900$"
                floating #true
                size 800 600
                workspace 3
            }
            "##,
        )
        .unwrap();
        let fx = rule_effects(&cfg, "steam_app_250900", "Isaac", true, false);
        assert!(fx.immediate, "first rule matched by prefix regex");
        assert_eq!(fx.opacity, Some(0.9));
        assert_eq!(fx.floating, Some(true), "second rule stacked");
        assert_eq!(fx.size, Some((800, 600)));
        assert_eq!(fx.workspace, Some(2), "parser stores 0-based");
        // non-matching class gets nothing
        let fx = rule_effects(&cfg, "foot", "shell", false, false);
        assert_eq!(fx, RuleFx::default());
    }

    #[test]
    fn a_bad_rule_regex_fails_the_parse() {
        assert!(parse(r##"window-rule { match class="[unclosed" floating #true }"##).is_err());
    }

    #[test]
    fn remap_parses_and_resolves() {
        let cfg = parse(
            r##"
            remap "binding-of-isaac" {
                class "steam_app_250900"
                map "Alt_R" "Left"
                map "Compose" "Down"
                map "Ctrl_R" "Right"
                map "Slash" "Up"
            }
            "##,
        )
        .unwrap();
        assert_eq!(cfg.remaps.len(), 1);
        let p = &cfg.remaps[0];
        assert_eq!(p.class.as_deref(), Some("steam_app_250900"));
        assert_eq!(p.maps.len(), 4);
        // alt_r(100) -> left(105) under the matching class
        assert_eq!(
            resolve_remap(&cfg, "steam_app_250900", "Isaac", true, 1, 1, 100),
            Some(105)
        );
        // wrong class: untouched
        assert_eq!(resolve_remap(&cfg, "foot", "shell", false, 1, 1, 100), None);
        // unmapped key under the right class: untouched
        assert_eq!(resolve_remap(&cfg, "steam_app_250900", "x", true, 1, 1, 30), None);
        // slash(53) -> up(103)
        assert_eq!(
            resolve_remap(&cfg, "steam_app_250900", "x", true, 1, 1, 53),
            Some(103)
        );
    }

    #[test]
    fn remap_criteria_and_together() {
        let cfg = parse(
            r##"
            remap "narrow" {
                class "foot"
                workspace 3
                type "wayland"
                map "a" "b"
            }
            "##,
        )
        .unwrap();
        let key_a = keycode("a").unwrap();
        let key_b = keycode("b").unwrap();
        assert_eq!(resolve_remap(&cfg, "foot", "", false, 1, 3, key_a), Some(key_b));
        assert_eq!(resolve_remap(&cfg, "foot", "", false, 1, 2, key_a), None);
        assert_eq!(resolve_remap(&cfg, "foot", "", true, 1, 3, key_a), None);
    }

    #[test]
    fn move_and_send_to_workspace_differ() {
        let cfg = parse(
            "bind \"Meta+Shift\" \"1\" \"movetoworkspace\" \"3\"\nbind \"Meta+Ctrl\" \"1\" \"send-to-workspace\" \"3\"\n",
        )
        .unwrap();
        let n = cfg.binds.len();
        assert_eq!(cfg.binds[n - 2].action, Action::MoveToWorkspace(2));
        assert_eq!(cfg.binds[n - 1].action, Action::SendToWorkspace(2));
    }

    #[test]
    fn defaults_are_neutral() {
        // no hardcoded config ships with carrot: no binds, no visible
        // choices; everything comes from the user's file
        let c = Config::default();
        assert_eq!((c.gaps_in, c.gaps_out, c.border), (0, 0, 0));
        assert_eq!(c.layout, "dwindle");
        assert!(!c.allow_tearing);
        assert!(c.binds.is_empty());
        assert!(c.rules.is_empty() && c.outputs.is_empty() && c.devices.is_empty());
        assert!(c.remaps.is_empty());
    }

    #[test]
    fn errors_are_loud_and_positioned() {
        let err = parse("general {\n    gaps-in \"soup\"\n}").unwrap_err();
        assert!(err.contains("2:5"), "{err}");
        assert!(err.contains("integer"), "{err}");
        let err = parse("no-such-key").unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
        let err = parse("bind \"Meta\" \"nope\" \"close\"").unwrap_err();
        assert!(err.contains("unknown key \"nope\""), "{err}");
        let err = parse("general {\n    allow-tearing yes\n}").unwrap_err();
        assert!(err.contains("#true or #false"), "{err}");
        let err = parse("bind \"Meta\" \"q\" \"close\" type=\"sometimes\"").unwrap_err();
        assert!(err.contains("bind type"), "{err}");
    }

    #[test]
    fn actions_roundtrip_as_json() {
        let a = Action::Workspace(3);
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&j).unwrap(), a);
        let j = serde_json::to_string(&Action::ToggleFullscreen).unwrap();
        assert_eq!(j, "\"toggle-fullscreen\"");
        let j = serde_json::to_string(&Action::SplitRatio(-0.1)).unwrap();
        assert_eq!(j, "{\"split-ratio\":-0.1}");
        let j = serde_json::to_string(&Action::WorkspaceRel(-1)).unwrap();
        assert_eq!(j, "{\"workspace-rel\":-1}");
        let j = serde_json::to_string(&Action::FocusDir(Dir::Left)).unwrap();
        assert_eq!(j, "{\"focus-dir\":\"left\"}");
        let j = serde_json::to_string(&Action::SwapDir(Dir::Down)).unwrap();
        assert_eq!(j, "{\"swap-dir\":\"down\"}");
    }

    #[test]
    fn split_ratio_binds_parse_signed_deltas() {
        let cfg = parse(r##"bind "Meta" "r" "split-ratio" "+0.1""##).unwrap();
        assert_eq!(cfg.binds[0].action, Action::SplitRatio(0.1));
        let cfg = parse(r##"bind "Meta" "e" "split-ratio" "-0.1""##).unwrap();
        assert_eq!(cfg.binds[0].action, Action::SplitRatio(-0.1));
        assert!(parse(r##"bind "Meta" "r" "split-ratio""##).is_err());
    }
}
