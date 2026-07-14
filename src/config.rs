// kdl v2 config. the embedded default is the single source of defaults:
// written to disk on first run, the startup fallback when the user file
// fails to parse (every error still prints), and Config::default(). a
// failed reload keeps the running config. each key is hot (apply live)
// or cold (log that a restart is needed).

use serde::{Deserialize, Serialize};
pub(crate) use ::kdl::{KdlDocument, KdlNode};

pub mod default;
pub mod kdl;
pub mod lua;

pub use default::DEFAULT_CONFIG;
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
    FocusWorkspace(usize),
    /// signed jump from the active workspace, wrapping
    FocusWorkspaceRel(i32),
    /// move the window; focus follows unless told otherwise
    MoveToWorkspace(usize),
    SendToWorkspace(usize),
    ToggleFullscreen,
    ToggleFloating,
    CloseWindow,
    FocusNext,
    FocusPrev,
    FocusDir(Dir),
    SwapDir(Dir),
    /// nudge the focused window's parent split; signed fraction of the span
    AdjustSplitRatio(f64),
    // scrolling-layout verbs; harmless no-ops on a dwindle workspace
    ConsumeOrExpelLeft,
    ConsumeOrExpelRight,
    CycleColumnWidth,
    CycleColumnWidthBack,
    ToggleFullWidth,
    CenterColumn,
    SetLayout(SetLayoutArg),
    /// grab the window under the cursor and let the pointer carry it;
    /// bound to a mouse chord it drags, bound to a key it sticks until
    /// the next click
    PointerMove,
    /// same grab, resizing by the quadrant the pointer started in
    PointerResize,
    Spawn(Vec<String>),
    SpawnSh(String),
    Quit,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetLayoutArg {
    Dwindle,
    Scrolling,
    Toggle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bind {
    pub mods: u32,
    /// evdev code; mouse buttons live at 0x110+ in the same space
    pub key: u32,
    pub action: Action,
    pub on_release: bool,
    pub repeat: bool,
    pub allow_when_locked: bool,
    pub cooldown_ms: Option<u32>,
    /// shown by shell-drawn hotkey overlays via the ipc bind list
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct XkbCfg {
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyboardCfg {
    pub xkb: XkbCfg,
    pub repeat_rate: i32,
    pub repeat_delay: i32,
    pub numlock: bool,
}

impl Default for KeyboardCfg {
    fn default() -> KeyboardCfg {
        // kernel-ish repeat timings; a keyboard that never repeats reads
        // as broken, not unset
        KeyboardCfg { xkb: XkbCfg::default(), repeat_rate: 25, repeat_delay: 600, numlock: false }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PointerClassCfg {
    pub accel_profile: Option<String>,
    pub accel_speed: Option<f64>,
    pub natural_scroll: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum ModKey {
    #[default]
    Super,
    Alt,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct InputCfg {
    pub keyboard: KeyboardCfg,
    pub touchpad: PointerClassCfg,
    pub mouse: PointerClassCfg,
    pub devices: Vec<DeviceRule>,
    pub mod_key: ModKey,
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum LayoutMode {
    #[default]
    Dwindle,
    Scrolling,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ColWidthCfg {
    Prop(f64),
    FixedPx(i32),
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum CenterFocus {
    #[default]
    Never,
    Always,
    OnOverflow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollCfg {
    pub preset_widths: Vec<f64>,
    pub default_width: ColWidthCfg,
    pub center_focus: CenterFocus,
}

impl Default for ScrollCfg {
    fn default() -> ScrollCfg {
        ScrollCfg {
            preset_widths: vec![1.0 / 3.0, 0.5, 2.0 / 3.0],
            default_width: ColWidthCfg::Prop(0.5),
            center_focus: CenterFocus::Never,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct BorderCfg {
    pub width: i32,
    pub active: [f32; 4],
    pub inactive: [f32; 4],
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct LayoutCfg {
    pub mode: LayoutMode,
    pub gaps_in: i32,
    pub gaps_out: i32,
    pub border: BorderCfg,
    pub float_above_fullscreen: bool,
    pub scrolling: ScrollCfg,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShadowCfg {
    pub size: i32,
    pub color: [f32; 4],
    pub offset: (i32, i32),
    /// falloff exponent; higher hugs the window tighter
    pub power: f64,
}

impl Default for ShadowCfg {
    fn default() -> ShadowCfg {
        ShadowCfg { size: 20, color: [0.0, 0.0, 0.0, 0.6], offset: (0, 4), power: 2.0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlurCfg {
    pub passes: i32,
    /// per-level tap offset in that level's half-pixels; the dual-kawase
    /// reference wants ~1.5-3, past 6 the taps alias into banding
    pub size: f64,
    pub noise: f64,
    pub contrast: f64,
    pub brightness: f64,
}

impl Default for BlurCfg {
    fn default() -> BlurCfg {
        BlurCfg { passes: 3, size: 2.5, noise: 0.02, contrast: 1.0, brightness: 1.0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecoCfg {
    /// corner radius px; 0 = square
    pub rounding: i32,
    /// parsed and stored; the shader family is circular until a
    /// superellipse need shows up
    pub rounding_power: f64,
    pub dim_inactive: f64,
    pub shadow: Option<ShadowCfg>,
    pub blur: Option<BlurCfg>,
}

impl Default for DecoCfg {
    fn default() -> DecoCfg {
        DecoCfg {
            rounding: 0,
            rounding_power: 2.0,
            dim_inactive: 0.0,
            shadow: None,
            blur: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct CursorCfg {
    pub xcursor_theme: Option<String>,
    pub xcursor_size: Option<u32>,
    /// composite the cursor instead of using the hardware plane; the
    /// escape hatch for planes that misbehave (joined-pipe modes)
    pub software: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ScreencastCfg {
    /// the consent picker command; unset means click-to-select
    pub picker: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SpawnCfg {
    Argv(Vec<String>),
    Sh(String),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct DebugCfg {
    pub render_drm_device: Option<String>,
    /// secondary gpus to leave alone entirely
    pub ignore_drm_devices: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum Vrr {
    #[default]
    Off,
    OnDemand,
    Always,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputCfg {
    /// connector name ("DP-3") or "make model serial" string
    pub name: String,
    pub vrr: Vrr,
    pub scale: Option<f64>,
    /// "2560x1440@240" or "2560x1440"; picks the closest advertised mode
    pub mode: Option<(u32, u32, Option<u32>)>,
    pub position: Option<(i32, i32)>,
    pub off: bool,
    pub allow_tearing: bool,
}

/// focus-activated key translations; criteria AND together
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RemapProfile {
    pub name: String,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub is_xwayland: Option<bool>,
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

/// selectors regex-match; a rule needs at least one match child. AND
/// within a match node, OR across nodes, excludes veto
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RuleMatch {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub is_xwayland: Option<bool>,
    pub is_floating: Option<bool>,
    pub is_fullscreen: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct WindowRule {
    pub matches: Vec<RuleMatch>,
    pub excludes: Vec<RuleMatch>,
    // open-time effects
    pub open_floating: Option<bool>,
    pub open_on_workspace: Option<usize>,
    pub default_size: Option<(i32, i32)>,
    pub open_centered: bool,
    // dynamic effects
    pub opacity: Option<f64>,
    pub allow_tearing: bool,
    pub no_anim: bool,
    /// captures and casts see a black stand-in instead of the content
    pub no_capture: bool,
    /// open/close style override, window-style grammar
    pub animation: Option<Style>,
    pub rounding: Option<i32>,
    pub shadow: Option<bool>,
    pub dim: Option<bool>,
    pub blur: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct LayerRule {
    pub matches: Vec<Pattern>,
    pub blur: bool,
    /// backdrop only where surface alpha >= this; unset covers the whole
    /// rect. argb surfaces only - xrgb views pin alpha to one
    pub ignore_alpha: Option<f64>,
    /// maps and unmaps appear instantly; shells that remap layers on
    /// state changes opt out of the open/close styles
    pub no_anim: bool,
}

// -- animations: per-kind spring/ease motion plus visual styles --

#[derive(Clone, Debug, PartialEq)]
pub enum Motion {
    Spring { damping: f64, stiffness: f64, epsilon: f64 },
    Ease { ms: u32, curve: CurveRef },
}

#[derive(Clone, Debug, PartialEq)]
pub enum CurveRef {
    Linear,
    Quad,
    Cubic,
    Expo,
    Named(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Style {
    /// kind-appropriate builtin (popin for windows, slide for the rest)
    Default,
    /// scale from `perc` of full size about the center, fading in
    Popin { perc: f64 },
    Fade,
    /// None picks the nearest edge (windows/layers) or the axis rule (workspaces)
    Slide { dir: Option<Dir> },
    SlideVert,
    SlideFade { perc: f64 },
    SlideFadeVert { perc: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct KindCfg {
    pub off: bool,
    /// None inherits the section's default motion
    pub motion: Option<Motion>,
    pub style: Style,
}

impl Default for KindCfg {
    fn default() -> KindCfg {
        KindCfg { off: false, motion: None, style: Style::Default }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AnimKind {
    WindowOpen,
    WindowClose,
    WindowMove,
    WindowResize,
    WorkspaceSwitch,
    ViewMovement,
    LayerOpen,
    LayerClose,
    BorderColor,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnimsCfg {
    pub off: bool,
    pub slowdown: f64,
    pub curves: Vec<(String, crate::anim::CubicBezier)>,
    pub default_motion: Motion,
    pub window_open: KindCfg,
    pub window_close: KindCfg,
    pub window_move: KindCfg,
    pub window_resize: KindCfg,
    pub workspace_switch: KindCfg,
    pub view_movement: KindCfg,
    pub layer_open: KindCfg,
    pub layer_close: KindCfg,
    pub border_color: KindCfg,
}

impl Default for AnimsCfg {
    fn default() -> AnimsCfg {
        AnimsCfg {
            off: false,
            slowdown: 1.0,
            curves: Vec::new(),
            default_motion: Motion::Spring { damping: 1.0, stiffness: 800.0, epsilon: 0.0001 },
            window_open: KindCfg {
                off: false,
                motion: Some(Motion::Ease { ms: 150, curve: CurveRef::Expo }),
                style: Style::Popin { perc: 0.8 },
            },
            window_close: KindCfg {
                off: false,
                motion: Some(Motion::Ease { ms: 150, curve: CurveRef::Quad }),
                style: Style::Popin { perc: 0.8 },
            },
            window_move: KindCfg::default(),
            window_resize: KindCfg::default(),
            workspace_switch: KindCfg {
                off: false,
                motion: Some(Motion::Spring { damping: 1.0, stiffness: 1000.0, epsilon: 0.0001 }),
                style: Style::Slide { dir: None },
            },
            view_movement: KindCfg::default(),
            layer_open: KindCfg { off: false, motion: None, style: Style::Slide { dir: None } },
            layer_close: KindCfg { off: false, motion: None, style: Style::Slide { dir: None } },
            border_color: KindCfg {
                off: false,
                motion: Some(Motion::Ease { ms: 200, curve: CurveRef::Quad }),
                style: Style::Default,
            },
        }
    }
}

impl AnimsCfg {
    pub fn kind(&self, k: AnimKind) -> &KindCfg {
        match k {
            AnimKind::WindowOpen => &self.window_open,
            AnimKind::WindowClose => &self.window_close,
            AnimKind::WindowMove => &self.window_move,
            AnimKind::WindowResize => &self.window_resize,
            AnimKind::WorkspaceSwitch => &self.workspace_switch,
            AnimKind::ViewMovement => &self.view_movement,
            AnimKind::LayerOpen => &self.layer_open,
            AnimKind::LayerClose => &self.layer_close,
            AnimKind::BorderColor => &self.border_color,
        }
    }

    /// None = this kind is disabled; global off still resolves (the clock
    /// makes those animations complete instantly)
    pub fn motion(&self, k: AnimKind) -> Option<&Motion> {
        let kc = self.kind(k);
        if kc.off {
            return None;
        }
        Some(kc.motion.as_ref().unwrap_or(&self.default_motion))
    }

    pub fn curve(&self, r: &CurveRef) -> crate::anim::Curve {
        match r {
            CurveRef::Linear => crate::anim::Curve::Linear,
            CurveRef::Quad => crate::anim::Curve::EaseOutQuad,
            CurveRef::Cubic => crate::anim::Curve::EaseOutCubic,
            CurveRef::Expo => crate::anim::Curve::EaseOutExpo,
            // unknown names were rejected at parse; fall back sanely anyway
            CurveRef::Named(n) => self
                .curves
                .iter()
                .find(|(name, _)| name == n)
                .map(|(_, b)| crate::anim::Curve::Bezier(*b))
                .unwrap_or(crate::anim::Curve::Linear),
        }
    }
}

/// which styles an animation kind accepts
#[derive(Copy, Clone, PartialEq)]
pub(crate) enum StyleFamily {
    Win,        // popin, fade, slide
    Ws,         // slide, slidevert, fade, slidefade, slidefadevert
    MotionOnly, // no style at all
}

pub(crate) fn spring_params(d: f64, s: f64, e: f64) -> Result<Motion, String> {
    Ok(Motion::Spring {
        damping: f64_in(d, "damping-ratio", 0.1, 10.0)?,
        stiffness: f64_in(s, "stiffness", 1.0, 100_000.0)?,
        epsilon: f64_in(e, "epsilon", 0.00001, 0.1)?,
    })
}

pub(crate) fn ease_params(ms: i64, curve: Option<&str>) -> Result<Motion, String> {
    let ms = int_in(ms, "duration-ms", 0, 10_000)? as u32;
    let curve = match curve {
        None => CurveRef::Cubic,
        Some("linear") => CurveRef::Linear,
        Some("ease-out-quad") => CurveRef::Quad,
        Some("ease-out-cubic") => CurveRef::Cubic,
        Some("ease-out-expo") => CurveRef::Expo,
        Some(name) => CurveRef::Named(name.to_string()),
    };
    Ok(Motion::Ease { ms, curve })
}

/// perc arrives as written (0..100); dir as the config word
pub(crate) fn style_from(
    family: StyleFamily,
    name: &str,
    perc: Option<f64>,
    dir: Option<&str>,
) -> Result<Style, String> {
    let perc = perc.map(|p| (p / 100.0).clamp(0.0, 1.0)).unwrap_or(0.8);
    let dir = match dir {
        None => None,
        Some("top") => Some(Dir::Up),
        Some("bottom") => Some(Dir::Down),
        Some("left") => Some(Dir::Left),
        Some("right") => Some(Dir::Right),
        Some(other) => return Err(format!("dir \"{other}\" is top, bottom, left or right")),
    };
    match (family, name) {
        (StyleFamily::Win, "popin") => Ok(Style::Popin { perc }),
        (StyleFamily::Win, "fade") => Ok(Style::Fade),
        (StyleFamily::Win, "slide") => Ok(Style::Slide { dir }),
        (StyleFamily::Ws, "slide") => Ok(Style::Slide { dir: None }),
        (StyleFamily::Ws, "slidevert") => Ok(Style::SlideVert),
        (StyleFamily::Ws, "fade") => Ok(Style::Fade),
        (StyleFamily::Ws, "slidefade") => Ok(Style::SlideFade { perc }),
        (StyleFamily::Ws, "slidefadevert") => Ok(Style::SlideFadeVert { perc }),
        _ => Err(format!("style \"{name}\" does not fit this animation")),
    }
}

/// motion + clock -> a running Anim
pub fn build_anim(
    clock: &crate::anim::AnimClock,
    motion: &Motion,
    anims: &AnimsCfg,
    from: f64,
    to: f64,
    v0: f64,
) -> crate::anim::Anim {
    match motion {
        Motion::Ease { ms, curve } => crate::anim::Anim::ease(clock, from, to, *ms, anims.curve(curve)),
        Motion::Spring { damping, stiffness, epsilon } => crate::anim::Anim::spring(
            clock,
            from,
            to,
            v0,
            crate::anim::SpringK::new(*damping, *stiffness, *epsilon),
        ),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub input: InputCfg,
    pub outputs: Vec<OutputCfg>,
    pub layout: LayoutCfg,
    pub cursor: CursorCfg,
    /// NAME "value" sets, NAME #null clears, for spawned children
    pub environment: Vec<(String, Option<String>)>,
    pub spawns: Vec<SpawnCfg>,
    pub prefer_no_csd: bool,
    pub screencast: ScreencastCfg,
    pub binds: Vec<Bind>,
    pub rules: Vec<WindowRule>,
    pub layer_rules: Vec<LayerRule>,
    pub remaps: Vec<RemapProfile>,
    pub debug: DebugCfg,
    pub animations: AnimsCfg,
    pub decoration: DecoCfg,
}

impl Default for Config {
    fn default() -> Config {
        default::embedded().clone()
    }
}

/// the truly empty config the parser accumulates into; the embedded
/// default is parsed on top of this, user files on top of a default clone
pub(crate) fn empty() -> Config {
    Config {
        input: InputCfg::default(),
        outputs: Vec::new(),
        layout: LayoutCfg::default(),
        cursor: CursorCfg::default(),
        environment: Vec::new(),
        spawns: Vec::new(),
        prefer_no_csd: false,
        screencast: ScreencastCfg::default(),
        binds: Vec::new(),
        rules: Vec::new(),
        layer_rules: Vec::new(),
        remaps: Vec::new(),
        debug: DebugCfg::default(),
        animations: AnimsCfg::default(),
        decoration: DecoCfg::default(),
    }
}

// mod bits match the seat's exact-set matcher
pub const M_SHIFT: u32 = 1 << 0;
pub const M_CTRL: u32 = 1 << 2;
pub const M_ALT: u32 = 1 << 3;
pub const M_SUPER: u32 = 1 << 6;
/// "Mod" in a chord; resolved against input.mod-key once the file is read
pub(crate) const M_MOD: u32 = 1 << 15;

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

pub enum Loaded {
    Ok(Config),
    /// no file existed; the embedded default was written out and used
    FirstRun(Config),
    /// the file failed to parse; the session runs on the embedded default
    Fallback { errors: Vec<String> },
}

pub fn load() -> Loaded {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(&path, DEFAULT_CONFIG) {
                Ok(()) => eprintln!("carrot: config: wrote default to {}", path.display()),
                Err(e) => eprintln!("carrot: config: cannot write {}: {e}", path.display()),
            }
            return Loaded::FirstRun(default::embedded().clone());
        }
        Err(e) => {
            return Loaded::Fallback { errors: vec![format!("{}: {e}", path.display())] };
        }
    };
    let parsed = if path.extension().is_some_and(|e| e == "lua") {
        lua::parse(&text)
    } else {
        parse(&text)
    };
    match parsed {
        Ok(cfg) => Loaded::Ok(cfg),
        Err(errors) => {
            let errors: Vec<String> = errors
                .into_iter()
                .map(|e| format!("{}: {e}", path.display()))
                .collect();
            for e in &errors {
                eprintln!("carrot: config: {e}");
            }
            Loaded::Fallback { errors }
        }
    }
}

/// `carrot check-config [path]`: parse and report without a session.
/// exit 0 clean, 1 broken
pub fn check(path: Option<&str>) -> i32 {
    let path = path.map(std::path::PathBuf::from).unwrap_or_else(config_path);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("carrot: {}: {e}", path.display());
            return 1;
        }
    };
    let parsed = if path.extension().is_some_and(|e| e == "lua") {
        lua::parse(&text)
    } else {
        parse(&text)
    };
    match parsed {
        Ok(_) => {
            println!("{}: ok", path.display());
            0
        }
        Err(errors) => {
            for e in errors {
                eprintln!("{}: {e}", path.display());
            }
            1
        }
    }
}

// -- shared leaf validators; both config languages come through here --

/// accumulated parse errors, each pre-rendered as "line:col: msg"
#[derive(Default)]
pub struct Errors {
    pub list: Vec<String>,
}

impl Errors {
    pub fn push(&mut self, rendered: String) {
        self.list.push(rendered);
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

pub(crate) fn color(s: &str) -> Result<[f32; 4], String> {
    let Some(hex) = s.strip_prefix('#') else {
        return Err(format!("color \"{s}\" needs a leading #"));
    };
    let expand = |c: u8| -> String {
        let ch = c as char;
        format!("{ch}{ch}")
    };
    let full: String = match hex.len() {
        3 | 4 => hex.bytes().map(expand).collect(),
        6 | 8 => hex.to_string(),
        _ => return Err(format!("color \"{s}\" is #rgb, #rgba, #rrggbb or #rrggbbaa")),
    };
    let byte = |i: usize| -> Result<f32, String> {
        u8::from_str_radix(&full[i..i + 2], 16)
            .map(|v| v as f32 / 255.0)
            .map_err(|_| format!("color \"{s}\" is #rgb, #rgba, #rrggbb or #rrggbbaa"))
    };
    let a = if full.len() == 8 { byte(6)? } else { 1.0 };
    Ok([byte(0)?, byte(2)?, byte(4)?, a])
}

pub(crate) fn int_in(v: i64, name: &str, lo: i64, hi: i64) -> Result<i64, String> {
    if v < lo || v > hi {
        return Err(format!("{name} is {lo}..{hi}, got {v}"));
    }
    Ok(v)
}

pub(crate) fn f64_in(v: f64, name: &str, lo: f64, hi: f64) -> Result<f64, String> {
    if !v.is_finite() || v < lo || v > hi {
        return Err(format!("{name} is {lo}..{hi}, got {v}"));
    }
    Ok(v)
}

pub(crate) fn accel_profile(p: &str) -> Result<String, String> {
    match p {
        "flat" | "adaptive" => Ok(p.to_string()),
        _ => Err("accel-profile is flat or adaptive".to_string()),
    }
}

pub(crate) fn regex(s: &str) -> Result<String, String> {
    regex_lite::Regex::new(s).map_err(|e| format!("bad regex \"{s}\": {e}"))?;
    Ok(s.to_string())
}

/// a regex compiled once at parse; per-frame matchers never rebuild
#[derive(Clone, Debug)]
pub struct Pattern {
    pub src: String,
    re: regex_lite::Regex,
}

impl Pattern {
    pub fn new(s: &str) -> Result<Pattern, String> {
        let re = regex_lite::Regex::new(s).map_err(|e| format!("bad regex \"{s}\": {e}"))?;
        Ok(Pattern { src: s.to_string(), re })
    }
    pub fn matches(&self, hay: &str) -> bool {
        self.re.is_match(hay)
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Pattern) -> bool {
        self.src == other.src
    }
}

pub(crate) fn parse_mode(s: &str) -> Option<(u32, u32, Option<u32>)> {
    let (res, hz) = match s.split_once('@') {
        Some((r, h)) => (r, Some(h.parse().ok()?)),
        None => (s, None),
    };
    let (w, h) = res.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, hz))
}

/// "Mod+Shift+Return" -> (mods with M_MOD placeholder, evdev code)
pub(crate) fn chord(spec: &str) -> Result<(u32, u32), String> {
    let mut mods = 0u32;
    let mut key = None;
    for part in spec.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "shift" => mods |= M_SHIFT,
            "ctrl" | "control" => mods |= M_CTRL,
            "alt" => mods |= M_ALT,
            "super" | "meta" | "logo" => mods |= M_SUPER,
            "mod" => mods |= M_MOD,
            "" => return Err(format!("chord \"{spec}\" has an empty part")),
            k => {
                if key.is_some() {
                    return Err(format!("chord \"{spec}\" has two keys"));
                }
                key = Some(
                    keycode(k).ok_or_else(|| format!("unknown key \"{part}\""))?,
                );
            }
        }
    }
    match key {
        Some(k) => Ok((mods, k)),
        None => Err(format!("chord \"{spec}\" has no key")),
    }
}

/// resolve M_MOD placeholders once input.mod-key is known
pub(crate) fn resolve_mod(binds: &mut [Bind], mod_key: ModKey) {
    let bit = match mod_key {
        ModKey::Super => M_SUPER,
        ModKey::Alt => M_ALT,
    };
    for b in binds {
        if b.mods & M_MOD != 0 {
            b.mods = (b.mods & !M_MOD) | bit;
        }
    }
}

// the full evdev keyboard map, KEY_* names lowercased, plus the common
// aliases; straight from input-event-codes.h
pub(crate) fn keycode(name: &str) -> Option<u32> {
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
        "mute" | "xf86audiomute" => 113,
        "volumedown" | "xf86audiolowervolume" => 114,
        "volumeup" | "xf86audioraisevolume" => 115,
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
        "nextsong" | "xf86audionext" => 163,
        "playpause" | "xf86audioplay" => 164,
        "previoussong" | "xf86audioprev" => 165,
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
        "brightnessdown" | "xf86monbrightnessdown" => 224,
        "brightnessup" | "xf86monbrightnessup" => 225, "media" => 226,
        "switchvideomode" => 227, "kbdillumtoggle" => 228,
        "kbdillumdown" => 229, "kbdillumup" => 230,
        "send" => 231, "reply" => 232, "forwardmail" => 233, "save" => 234,
        "documents" => 235, "battery" => 236, "bluetooth" => 237,
        "wlan" => 238, "uwb" => 239,
        "video_next" => 241, "video_prev" => 242,
        "brightness_cycle" => 243, "brightness_auto" | "brightness_zero" => 244,
        "display_off" => 245, "wwan" | "wimax" => 246, "rfkill" => 247,
        "micmute" => 248,
        // mouse buttons share the code space; chords say Mod+MouseLeft
        "btn_left" | "mouse_left" | "mouseleft" => 272,
        "btn_right" | "mouse_right" | "mouseright" => 273,
        "btn_middle" | "mouse_middle" | "mousemiddle" => 274,
        "btn_side" | "mouseside" => 275, "btn_extra" | "mouseextra" => 276,
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
    pub no_anim: bool,
    pub no_capture: bool,
    pub animation: Option<Style>,
    pub rounding: Option<i32>,
    pub shadow: Option<bool>,
    pub dim: Option<bool>,
    pub blur: Option<bool>,
}

fn matcher_hits(
    m: &RuleMatch,
    app_id: &str,
    title: &str,
    xwayland: bool,
    fullscreen: bool,
    floating: bool,
) -> bool {
    let re_hits = |pat: &Option<String>, hay: &str| -> bool {
        match pat {
            None => true,
            // validated at parse time; a stale failure just never matches
            Some(p) => regex_lite::Regex::new(p).is_ok_and(|re| re.is_match(hay)),
        }
    };
    re_hits(&m.app_id, app_id)
        && re_hits(&m.title, title)
        && m.is_xwayland.is_none_or(|w| w == xwayland)
        && m.is_fullscreen.is_none_or(|w| w == fullscreen)
        && m.is_floating.is_none_or(|w| w == floating)
}

pub fn rule_effects(
    cfg: &Config,
    app_id: &str,
    title: &str,
    xwayland: bool,
    fullscreen: bool,
) -> RuleFx {
    let mut fx = RuleFx::default();
    for r in cfg.rules.iter() {
        let hit = r
            .matches
            .iter()
            .any(|m| matcher_hits(m, app_id, title, xwayland, fullscreen, false));
        let vetoed = r
            .excludes
            .iter()
            .any(|m| matcher_hits(m, app_id, title, xwayland, fullscreen, false));
        if !hit || vetoed {
            continue;
        }
        if let Some(f) = r.open_floating {
            fx.floating = Some(f);
        }
        if let Some(ws) = r.open_on_workspace {
            fx.workspace = Some(ws);
        }
        fx.immediate |= r.allow_tearing;
        if let Some(o) = r.opacity {
            fx.opacity = Some(o);
        }
        if let Some(sz) = r.default_size {
            fx.size = Some(sz);
        }
        fx.center |= r.open_centered;
        fx.no_anim |= r.no_anim;
        fx.no_capture |= r.no_capture;
        if let Some(a) = &r.animation {
            fx.animation = Some(a.clone());
        }
        if let Some(v) = r.rounding {
            fx.rounding = Some(v);
        }
        if let Some(v) = r.shadow {
            fx.shadow = Some(v);
        }
        if let Some(v) = r.dim {
            fx.dim = Some(v);
        }
        if let Some(v) = r.blur {
            fx.blur = Some(v);
        }
    }
    fx
}

/// the translation for one key under the focused window, if any profile
/// matches. criteria AND; first matching profile wins
pub fn resolve_remap(
    cfg: &Config,
    app_id: &str,
    title: &str,
    is_x11: bool,
    pid: i32,
    ws_1based: usize,
    key: u32,
) -> Option<u32> {
    for p in cfg.remaps.iter() {
        if let Some(c) = &p.app_id {
            if c != app_id {
                continue;
            }
        }
        if let Some(t) = &p.title {
            if !title.contains(t.as_str()) {
                continue;
            }
        }
        if let Some(want) = p.is_xwayland {
            if want != is_x11 {
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
    fn first_run_writes_and_fallback_survives() {
        // one test owns the env var; first-run and fallback in sequence
        let dir = std::env::temp_dir().join(format!("carrot-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        match load() {
            Loaded::FirstRun(c) => assert!(!c.binds.is_empty(), "the default has binds"),
            _ => panic!("expected first run"),
        }
        assert!(dir.join("carrot/carrot.kdl").exists(), "default written to disk");
        std::fs::write(dir.join("carrot/carrot.kdl"), "nonsense {").unwrap();
        match load() {
            Loaded::Fallback { errors } => assert!(!errors.is_empty(), "errors reported"),
            _ => panic!("expected fallback"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn rules_match_by_regex_and_merge_in_order() {
        let cfg = parse(
            r##"
            window-rule {
                match app-id=#"^steam_app_.*$"#
                allow-tearing #true
                opacity 0.9
            }
            window-rule {
                match app-id=#"^steam_app_250900$"#
                open-floating #true
                default-size 800 600
                open-on-workspace 3
                no-capture
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
        assert!(fx.no_capture, "flag accumulates like the other booleans");
        // non-matching app id gets nothing
        let fx = rule_effects(&cfg, "foot", "shell", false, false);
        assert_eq!(fx, RuleFx::default());
    }

    #[test]
    fn excludes_veto_matches() {
        let cfg = parse(
            r##"
            window-rule {
                match app-id=#"^steam_app_"#
                exclude title=#"Isaac"#
                open-floating #true
            }
            "##,
        )
        .unwrap();
        let fx = rule_effects(&cfg, "steam_app_1", "Dead Cells", true, false);
        assert_eq!(fx.floating, Some(true));
        let fx = rule_effects(&cfg, "steam_app_1", "Isaac", true, false);
        assert_eq!(fx, RuleFx::default());
    }

    #[test]
    fn a_bad_rule_regex_fails_the_parse() {
        assert!(
            parse(r##"window-rule { match app-id=#"[unclosed"# ; open-floating #true }"##)
                .is_err()
        );
    }

    #[test]
    fn remap_parses_and_resolves() {
        let cfg = parse(
            r##"
            remap "binding-of-isaac" {
                match app-id="steam_app_250900"
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
        assert_eq!(p.app_id.as_deref(), Some("steam_app_250900"));
        assert_eq!(p.maps.len(), 4);
        // alt_r(100) -> left(105) under the matching app id
        assert_eq!(
            resolve_remap(&cfg, "steam_app_250900", "Isaac", true, 1, 1, 100),
            Some(105)
        );
        // wrong app id: untouched
        assert_eq!(resolve_remap(&cfg, "foot", "shell", false, 1, 1, 100), None);
        // unmapped key under the right app id: untouched
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
                match app-id="foot" workspace=3 is-xwayland=#false
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
            "binds {\n    Mod+Shift+1 { move-to-workspace 3; }\n    Mod+Ctrl+1 { send-to-workspace 3; }\n}\n",
        )
        .unwrap();
        assert_eq!(cfg.binds.len(), 2);
        assert_eq!(cfg.binds[0].action, Action::MoveToWorkspace(2));
        assert_eq!(cfg.binds[1].action, Action::SendToWorkspace(2));
    }

    #[test]
    fn mod_resolves_against_mod_key() {
        let cfg = parse("input { mod-key \"alt\" }\nbinds { Mod+Q { close-window; } }").unwrap();
        assert_eq!(cfg.binds[0].mods, M_ALT, "mod-key wins even declared first");
        let cfg = parse("binds { Mod+Q { close-window; } }").unwrap();
        assert_eq!(cfg.binds[0].mods, M_SUPER, "super is the default mod");
    }

    #[test]
    fn actions_roundtrip_as_json() {
        let a = Action::FocusWorkspace(3);
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&j).unwrap(), a);
        let j = serde_json::to_string(&Action::ToggleFullscreen).unwrap();
        assert_eq!(j, "\"toggle-fullscreen\"");
        let j = serde_json::to_string(&Action::AdjustSplitRatio(-0.1)).unwrap();
        assert_eq!(j, "{\"adjust-split-ratio\":-0.1}");
        let j = serde_json::to_string(&Action::FocusWorkspaceRel(-1)).unwrap();
        assert_eq!(j, "{\"focus-workspace-rel\":-1}");
        let j = serde_json::to_string(&Action::FocusDir(Dir::Left)).unwrap();
        assert_eq!(j, "{\"focus-dir\":\"left\"}");
        let j = serde_json::to_string(&Action::SwapDir(Dir::Down)).unwrap();
        assert_eq!(j, "{\"swap-dir\":\"down\"}");
    }

    #[test]
    fn colors_accept_all_four_widths() {
        assert_eq!(color("#fff").unwrap(), [1.0, 1.0, 1.0, 1.0]);
        // the a nibble doubles: 0xa -> 0xaa
        assert_eq!((color("#f00a").unwrap()[3] * 255.0).round(), 170.0);
        assert!(color("89b4fa").is_err(), "leading # is required now");
        assert!(color("#89b4fa").is_ok());
        assert!(color("#89b4fa80").is_ok());
    }

    #[test]
    fn chords_parse_and_reject() {
        let (m, k) = chord("Mod+Shift+Return").unwrap();
        assert_eq!(m, M_MOD | M_SHIFT);
        assert_eq!(k, 28);
        let (_, k) = chord("Mod+MouseLeft").unwrap();
        assert_eq!(k, 272);
        assert!(chord("Mod+Shift").is_err(), "no key");
        assert!(chord("Q+W").is_err(), "two keys");
    }
}
