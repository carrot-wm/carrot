// kdl v2 config. parse errors are fatal at startup and rejected on reload -
// never silently fall back to defaults. reload parses fresh, diffs, applies;
// each key is hot (apply live) or cold (log that a restart is needed).

use kdl::{KdlDocument, KdlNode};
use serde::{Deserialize, Serialize};

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

/// a per-window key rebind profile: criteria AND, first match wins
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
}

/// secondary gpus can skip bringing up a renderer (import-only)
#[derive(Clone, Debug, PartialEq)]
pub struct GpuCfg {
    pub name: String,
    pub skip_renderer: bool,
}

/// per-device overrides, matched by normalized name substring
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRule {
    pub name: String,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<String>,
    pub natural_scroll: Option<bool>,
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
    pub input: InputCfg,
    pub devices: Vec<DeviceRule>,
    pub decoration: DecorationCfg,
    pub animations: AnimationsCfg,
    pub rules: Vec<WindowRule>,
    pub remaps: Vec<RemapProfile>,
    pub layer_rules: Vec<LayerRule>,
    pub outputs: Vec<OutputCfg>,
    pub gpus: Vec<GpuCfg>,
    pub binds: Vec<Bind>,
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
            remaps: Vec::new(),
            layer_rules: Vec::new(),
            outputs: Vec::new(),
            gpus: Vec::new(),
            binds: Vec::new(),
            submaps: Vec::new(),
            specials: Vec::new(),
        }
    }
}

pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("carrot").join("carrot.kdl")
}

// a missing file means defaults; an unreadable or unparsable one is an error
pub fn load() -> Result<Config, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let upto = &src[..offset.min(src.len())];
    let line = upto.matches('\n').count() + 1;
    let col = upto.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) + 1;
    (line, col)
}

pub fn parse(src: &str) -> Result<Config, String> {
    let doc: KdlDocument = src.parse().map_err(|e: kdl::KdlError| {
        let mut out = String::new();
        for d in &e.diagnostics {
            let (l, c) = line_col(src, d.span.offset());
            let msg = d.message.clone().unwrap_or_else(|| "parse error".into());
            out.push_str(&format!("{l}:{c}: {msg}; "));
        }
        out
    })?;
    let mut cfg = Config::default();
    let mut saw_bind = false;
    let mut unapplied: Vec<&str> = Vec::new();
    for node in doc.nodes() {
        match node.name().value() {
            "general" => parse_general(node, src, &mut cfg)?,
            "input" => parse_input(node, src, &mut cfg)?,
            "device" => {
                let name = first_str(node)
                    .ok_or_else(|| at(node, src, "device needs a name string"))?;
                let mut rule = DeviceRule {
                    name,
                    accel_speed: None,
                    accel_profile: None,
                    natural_scroll: None,
                };
                for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match c.name().value() {
                        "accel-speed" | "sensitivity" => rule.accel_speed = first_float(c),
                        "accel-profile" => rule.accel_profile = Some(need_str(c, src)?),
                        "natural-scroll" => rule.natural_scroll = Some(need_bool(c, src)?),
                        other => {
                            return Err(at(c, src, &format!("unknown device key \"{other}\"")))
                        }
                    }
                }
                cfg.devices.push(rule);
            }
            "bind" => {
                // the first bind in the file drops the defaults
                if !saw_bind {
                    cfg.binds.clear();
                    saw_bind = true;
                }
                if let Some(b) = parse_bind(node, src)? {
                    cfg.binds.push(b);
                }
            }
            "decoration" => {
                parse_decoration(node, src, &mut cfg)?;
                unapplied.push("decoration");
            }
            "animations" => {
                parse_animations(node, src, &mut cfg)?;
                unapplied.push("animations");
            }
            "window-rule" => {
                cfg.rules.push(parse_rule(node, src)?);
                unapplied.push("window-rule");
            }
            "layer-rule" => {
                cfg.layer_rules.push(parse_layer_rule(node, src)?);
                unapplied.push("layer-rule");
            }
            "output" => {
                let name = first_str(node)
                    .ok_or_else(|| at(node, src, "output needs a name string"))?;
                let mut out = OutputCfg { name, vrr: None, gpu: None, scale: None };
                for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match c.name().value() {
                        "vrr" => {
                            let mode = need_str(c, src)?;
                            match mode.as_str() {
                                "off" | "automatic" | "always" => out.vrr = Some(mode),
                                _ => {
                                    return Err(at(c, src, "vrr is off, automatic or always"))
                                }
                            }
                        }
                        "gpu" => out.gpu = Some(need_str(c, src)?),
                        "scale" => out.scale = first_float(c),
                        other => {
                            return Err(at(c, src, &format!("unknown output key \"{other}\"")))
                        }
                    }
                }
                cfg.outputs.push(out);
                unapplied.push("output");
            }
            "gpu" => {
                let name = first_str(node)
                    .ok_or_else(|| at(node, src, "gpu needs a name string"))?;
                let mut gpu = GpuCfg { name, skip_renderer: false };
                for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match c.name().value() {
                        "skip-renderer" => gpu.skip_renderer = need_bool(c, src)?,
                        other => {
                            return Err(at(c, src, &format!("unknown gpu key \"{other}\"")))
                        }
                    }
                }
                cfg.gpus.push(gpu);
                unapplied.push("gpu");
            }
            "special" => {
                let name = first_str(node)
                    .ok_or_else(|| at(node, src, "special needs a name string"))?;
                let mut spawn = None;
                for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match c.name().value() {
                        "spawn" => spawn = Some(need_str(c, src)?),
                        other => {
                            return Err(at(c, src, &format!("unknown special key \"{other}\"")))
                        }
                    }
                }
                cfg.specials.push((name, spawn));
                unapplied.push("special");
            }
            "submap" => {
                let name = first_str(node)
                    .ok_or_else(|| at(node, src, "submap needs a name string"))?;
                let mut binds = Vec::new();
                for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match c.name().value() {
                        "bind" => {
                            if let Some(b) = parse_bind(c, src)? {
                                binds.push(b);
                            }
                        }
                        other => {
                            return Err(at(c, src, &format!("unknown submap key \"{other}\"")))
                        }
                    }
                }
                cfg.submaps.push((name, binds));
                unapplied.push("submap");
            }
            "remap" => cfg.remaps.push(parse_remap(node, src)?),
            other => return Err(at(node, src, &format!("unknown key \"{other}\""))),
        }
    }
    unapplied.sort();
    unapplied.dedup();
    if !unapplied.is_empty() {
        eprintln!(
            "carrot: config: parsed but not applied yet: {}",
            unapplied.join(", ")
        );
    }
    Ok(cfg)
}

fn parse_decoration(node: &KdlNode, src: &str, cfg: &mut Config) -> Result<(), String> {
    for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "rounding" => cfg.decoration.rounding = need_int(c, src)?,
            "blur" => {
                for b in c.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match b.name().value() {
                        "enabled" => cfg.decoration.blur.enabled = need_bool(b, src)?,
                        "size" => cfg.decoration.blur.size = need_int(b, src)?,
                        "passes" => cfg.decoration.blur.passes = need_int(b, src)?,
                        other => {
                            return Err(at(b, src, &format!("unknown blur key \"{other}\"")))
                        }
                    }
                }
            }
            "shadow" => {
                for b in c.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match b.name().value() {
                        "enabled" => cfg.decoration.shadow.enabled = need_bool(b, src)?,
                        "range" => cfg.decoration.shadow.range = need_int(b, src)?,
                        other => {
                            return Err(at(b, src, &format!("unknown shadow key \"{other}\"")))
                        }
                    }
                }
            }
            other => return Err(at(c, src, &format!("unknown decoration key \"{other}\""))),
        }
    }
    Ok(())
}

fn parse_animations(node: &KdlNode, src: &str, cfg: &mut Config) -> Result<(), String> {
    for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "bezier" => {
                let mut vals = c.entries().iter();
                let name = vals
                    .next()
                    .and_then(|e| e.value().as_string())
                    .ok_or_else(|| at(c, src, "bezier needs a name and four numbers"))?
                    .to_string();
                let mut pts = [0.0f64; 4];
                for p in &mut pts {
                    *p = vals
                        .next()
                        .and_then(|e| e.value().as_float().or_else(|| {
                            e.value().as_integer().map(|v| v as f64)
                        }))
                        .ok_or_else(|| at(c, src, "bezier needs a name and four numbers"))?;
                }
                cfg.animations.beziers.push((name, pts));
            }
            "spring" => {
                let mut vals = c.entries().iter();
                let name = vals
                    .next()
                    .and_then(|e| e.value().as_string())
                    .ok_or_else(|| at(c, src, "spring needs a name, mass, stiffness, damping"))?
                    .to_string();
                let mut pts = [0.0f64; 3];
                for p in &mut pts {
                    *p = vals
                        .next()
                        .and_then(|e| e.value().as_float().or_else(|| {
                            e.value().as_integer().map(|v| v as f64)
                        }))
                        .ok_or_else(|| at(c, src, "spring needs a name, mass, stiffness, damping"))?;
                }
                cfg.animations.springs.push((name, pts));
            }
            "animation" => {
                let name = first_str(c)
                    .ok_or_else(|| at(c, src, "animation needs a name string"))?;
                let mut a = AnimationCfg {
                    enabled: true,
                    speed: 1.0,
                    curve: String::new(),
                    style: None,
                };
                for b in c.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match b.name().value() {
                        "enabled" => a.enabled = need_bool(b, src)?,
                        "speed" => a.speed = need_float(b, src)?,
                        "curve" => a.curve = need_str(b, src)?,
                        "style" => a.style = Some(need_str(b, src)?),
                        other => {
                            return Err(at(b, src, &format!("unknown animation key \"{other}\"")))
                        }
                    }
                }
                cfg.animations.animations.push((name, a));
            }
            other => return Err(at(c, src, &format!("unknown animations key \"{other}\""))),
        }
    }
    Ok(())
}

// window-rule { match class="..." title="..."; immediate #true; ... }
fn parse_rule(node: &KdlNode, src: &str) -> Result<WindowRule, String> {
    let mut rule = WindowRule::default();
    let mut matched = false;
    for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "match" => {
                matched = true;
                rule.match_class =
                    c.get("class").and_then(|v| v.as_string()).map(str::to_string);
                rule.match_title =
                    c.get("title").and_then(|v| v.as_string()).map(str::to_string);
                rule.match_fullscreen = c.get("fullscreen").and_then(|v| v.as_bool());
                rule.match_xwayland = c.get("xwayland").and_then(|v| v.as_bool());
                rule.match_floating = c.get("floating").and_then(|v| v.as_bool());
            }
            "immediate" => rule.immediate = need_bool(c, src)?,
            "idle-inhibit" => rule.idle_inhibit = Some(need_str(c, src)?),
            "floating" => rule.floating = need_bool(c, src)?,
            "tile" => rule.tile = need_bool(c, src)?,
            "workspace" => {
                rule.workspace = c
                    .entries()
                    .first()
                    .and_then(|e| e.value().as_integer())
                    .map(|n| (n as usize).saturating_sub(1));
            }
            "opacity" => rule.opacity = Some(need_float(c, src)?),
            "size" => {
                let mut it = c.entries().iter().filter_map(|e| e.value().as_integer());
                rule.size = match (it.next(), it.next()) {
                    (Some(w), Some(h)) => Some((w as i32, h as i32)),
                    _ => return Err(at(c, src, "size needs a width and a height")),
                };
            }
            "center" => rule.center = need_bool(c, src)?,
            "rounding" => rule.rounding = Some(need_int(c, src)?),
            "blur" => rule.blur = Some(need_bool(c, src)?),
            "shadow" => rule.shadow = Some(need_bool(c, src)?),
            "dim" => rule.dim = Some(need_float(c, src)?),
            "pin" => rule.pin = need_bool(c, src)?,
            "keep-aspect-ratio" => rule.keep_aspect_ratio = need_bool(c, src)?,
            "focus-steal" => rule.focus_steal = need_bool(c, src)?,
            // input-redirect "passthrough" "f13" "f14"
            "input-redirect" => {
                let mut vals = c.entries().iter().filter(|e| e.name().is_none());
                let mode = vals
                    .next()
                    .and_then(|e| e.value().as_string())
                    .ok_or_else(|| at(c, src, "input-redirect is redirect or passthrough"))?;
                if mode != "redirect" && mode != "passthrough" {
                    return Err(at(c, src, "input-redirect is redirect or passthrough"));
                }
                rule.redirect_mode = Some(mode.to_string());
                rule.redirect_keys = vals
                    .filter_map(|e| e.value().as_string().map(str::to_string))
                    .collect();
            }
            other => return Err(at(c, src, &format!("unknown window-rule key \"{other}\""))),
        }
    }
    if !matched {
        return Err(at(node, src, "window-rule needs a match child"));
    }
    Ok(rule)
}

// remap "isaac" { class "..."; map "d" "left"; map "f" "space" }
fn parse_remap(node: &KdlNode, src: &str) -> Result<RemapProfile, String> {
    let mut p = RemapProfile {
        name: first_str(node).unwrap_or_default(),
        ..Default::default()
    };
    let Some(children) = node.children() else {
        return Err(at(node, src, "remap needs a child block"));
    };
    for child in children.nodes() {
        match child.name().value() {
            "class" => p.class = Some(need_str(child, src)?),
            "title" => p.title = Some(need_str(child, src)?),
            "type" => {
                let t = need_str(child, src)?;
                if t != "x11" && t != "wayland" {
                    return Err(at(child, src, "type is \"x11\" or \"wayland\""));
                }
                p.win_type = Some(t);
            }
            "pid" => p.pid = Some(need_int(child, src)?),
            "workspace" => p.workspace = Some(need_int(child, src)?.max(1) as usize),
            "map" => {
                let args: Vec<String> = child
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .filter_map(|e| e.value().as_string().map(str::to_string))
                    .collect();
                let [from, to] = args.as_slice() else {
                    return Err(at(child, src, "map wants two key names"));
                };
                let f = keycode(&from.to_lowercase())
                    .ok_or_else(|| at(child, src, &format!("unknown key \"{from}\"")))?;
                let t = keycode(&to.to_lowercase())
                    .ok_or_else(|| at(child, src, &format!("unknown key \"{to}\"")))?;
                p.maps.push((f, t));
            }
            other => return Err(at(child, src, &format!("remap: unknown key \"{other}\""))),
        }
    }
    if p.maps.is_empty() {
        return Err(at(node, src, "remap has no map entries"));
    }
    Ok(p)
}

// layer-rule { match namespace="..."; blur #true; ignore-alpha 0.2 }
fn parse_layer_rule(node: &KdlNode, src: &str) -> Result<LayerRule, String> {
    let mut rule = LayerRule::default();
    let mut matched = false;
    for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "match" => {
                matched = true;
                rule.match_namespace = c
                    .get("namespace")
                    .and_then(|v| v.as_string())
                    .map(str::to_string);
            }
            "animation" => rule.animation = Some(need_str(c, src)?),
            "blur" => rule.blur = Some(need_bool(c, src)?),
            "ignore-alpha" => rule.ignore_alpha = Some(need_float(c, src)?),
            other => return Err(at(c, src, &format!("unknown layer-rule key \"{other}\""))),
        }
    }
    if !matched {
        return Err(at(node, src, "layer-rule needs a match child"));
    }
    Ok(rule)
}

fn parse_general(node: &KdlNode, src: &str, cfg: &mut Config) -> Result<(), String> {
    let mut unapplied = Vec::new();
    for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "gaps-in" => cfg.gaps_in = need_int(c, src)?,
            "gaps-out" => cfg.gaps_out = need_int(c, src)?,
            "border-size" => cfg.border = need_int(c, src)?,
            "active-border" => cfg.border_focused = color(&need_str(c, src)?, c, src)?,
            "inactive-border" => cfg.border_unfocused = color(&need_str(c, src)?, c, src)?,
            "float-above-fullscreen" => cfg.float_above_fullscreen = need_bool(c, src)?,
            "layout" => {
                cfg.layout = need_str(c, src)?;
                if cfg.layout != "dwindle" {
                    eprintln!(
                        "carrot: config: layout \"{}\" not implemented yet, using dwindle",
                        cfg.layout
                    );
                }
            }
            "allow-tearing" => {
                cfg.allow_tearing = need_bool(c, src)?;
                unapplied.push("allow-tearing");
            }
            other => return Err(at(c, src, &format!("unknown general key \"{other}\""))),
        }
    }
    if !unapplied.is_empty() {
        eprintln!(
            "carrot: config: general settings parsed but not applied yet: {}",
            unapplied.join(", ")
        );
    }
    Ok(())
}

fn parse_input(node: &KdlNode, src: &str, cfg: &mut Config) -> Result<(), String> {
    let mut unapplied = Vec::new();
    for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "repeat-rate" => cfg.repeat_rate = need_int(c, src)?,
            "repeat-delay" => cfg.repeat_delay = need_int(c, src)?,
            "accel-profile" => {
                cfg.input.accel_profile = Some(need_str(c, src)?);
                unapplied.push("accel-profile");
            }
            "natural-scroll" => {
                cfg.input.natural_scroll = need_bool(c, src)?;
                unapplied.push("natural-scroll");
            }
            "tap" => {
                cfg.input.tap = need_bool(c, src)?;
                unapplied.push("tap");
            }
            "dwt" => {
                cfg.input.dwt = need_bool(c, src)?;
                unapplied.push("dwt");
            }
            "layout" => {
                cfg.input.layout = Some(need_str(c, src)?);
                unapplied.push("layout");
            }
            "numlock" => {
                cfg.input.numlock = need_bool(c, src)?;
                unapplied.push("numlock");
            }
            other => return Err(at(c, src, &format!("unknown input key \"{other}\""))),
        }
    }
    if !unapplied.is_empty() {
        eprintln!(
            "carrot: config: input settings parsed but not applied yet: {}",
            unapplied.join(", ")
        );
    }
    Ok(())
}

fn at(node: &KdlNode, src: &str, msg: &str) -> String {
    let (l, c) = line_col(src, node.span().offset());
    format!("{l}:{c}: {msg}")
}

fn first_str(node: &KdlNode) -> Option<String> {
    node.entries()
        .first()
        .and_then(|e| e.value().as_string())
        .map(str::to_string)
}

fn first_float(node: &KdlNode) -> Option<f64> {
    node.entries().first().and_then(|e| e.value().as_float().or_else(|| {
        e.value().as_integer().map(|v| v as f64)
    }))
}

fn need_int(node: &KdlNode, src: &str) -> Result<i32, String> {
    node.entries()
        .first()
        .and_then(|e| e.value().as_integer())
        .map(|v| v as i32)
        .ok_or_else(|| at(node, src, &format!("{} needs an integer", node.name().value())))
}

fn need_str(node: &KdlNode, src: &str) -> Result<String, String> {
    first_str(node)
        .ok_or_else(|| at(node, src, &format!("{} needs a string", node.name().value())))
}

fn need_float(node: &KdlNode, src: &str) -> Result<f64, String> {
    first_float(node)
        .ok_or_else(|| at(node, src, &format!("{} needs a number", node.name().value())))
}

fn need_bool(node: &KdlNode, src: &str) -> Result<bool, String> {
    let v = node.entries().first().map(|e| e.value());
    if let Some(b) = v.and_then(|v| v.as_bool()) {
        return Ok(b);
    }
    // quoted "true"/"false" also pass; kdl v2 reserves the bare words
    match v.and_then(|v| v.as_string()) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(at(node, src, &format!("{} needs #true or #false", node.name().value()))),
    }
}

fn color(s: &str, node: &KdlNode, src: &str) -> Result<[f32; 4], String> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 && hex.len() != 8 {
        return Err(at(node, src, "colors are rrggbb or rrggbbaa"));
    }
    let byte = |i: usize| -> Result<f32, String> {
        u8::from_str_radix(&hex[i..i + 2], 16)
            .map(|v| v as f32 / 255.0)
            .map_err(|_| at(node, src, "colors are rrggbb or rrggbbaa"))
    };
    let a = if hex.len() == 8 { byte(6)? } else { 1.0 };
    Ok([byte(0)?, byte(2)?, byte(4)?, a])
}

// bind "Meta+Shift" "q" "close"  /  bind "Meta" "1" "workspace" "1"
// type="press|release|repeat|lock-safe|mouse" picks when the bind fires
// unimplemented-but-known actions warn and skip instead of failing the file
fn parse_bind(node: &KdlNode, src: &str) -> Result<Option<Bind>, String> {
    let mut kind = BindKind::Press;
    let mut args: Vec<String> = Vec::new();
    for e in node.entries() {
        if let Some(name) = e.name() {
            match name.value() {
                "type" => {
                    kind = match e.value().as_string() {
                        Some("press") => BindKind::Press,
                        Some("release") => BindKind::Release,
                        Some("repeat") => BindKind::Repeat,
                        Some("lock-safe") => BindKind::LockSafe,
                        Some("mouse") => BindKind::Mouse,
                        _ => {
                            return Err(at(
                                node,
                                src,
                                "bind type is press, release, repeat, lock-safe or mouse",
                            ))
                        }
                    };
                }
                other => {
                    return Err(at(node, src, &format!("unknown bind property \"{other}\"")))
                }
            }
            continue;
        }
        args.push(
            e.value()
                .as_string()
                .map(str::to_string)
                .unwrap_or_else(|| e.value().to_string()),
        );
    }
    if args.len() < 3 {
        return Err(at(node, src, "bind needs: mods key action [arg]"));
    }
    let mods = parse_mods(&args[0]).map_err(|e| at(node, src, &e))?;
    let key = keycode(&args[1].to_ascii_lowercase())
        .ok_or_else(|| at(node, src, &format!("unknown key \"{}\"", args[1])))?;
    let ws_arg = || -> Result<usize, String> {
        args.get(3)
            .and_then(|a| a.parse::<usize>().ok())
            .map(|n| n.saturating_sub(1))
            .ok_or_else(|| at(node, src, "workspace binds need a number"))
    };
    let action = match args[2].as_str() {
        "exec" | "spawn" => Action::Spawn(
            args.get(3..)
                .filter(|r| !r.is_empty())
                .map(|r| r.join(" "))
                .ok_or_else(|| at(node, src, "exec needs a command"))?,
        ),
        "workspace" => {
            // "+1"/"-1" (an optional leading "r" is accepted) jumps relative
            let rel = args
                .get(3)
                .map(|a| a.strip_prefix('r').unwrap_or(a))
                .filter(|a| a.starts_with(['+', '-']));
            match rel {
                Some(r) => Action::WorkspaceRel(r.parse::<i32>().map_err(|_| {
                    at(node, src, "relative workspace wants \"+N\" or \"-N\"")
                })?),
                None => Action::Workspace(ws_arg()?),
            }
        }
        "movetoworkspace" | "move-to-workspace" => Action::MoveToWorkspace(ws_arg()?),
        "sendtoworkspace" | "send-to-workspace" => Action::SendToWorkspace(ws_arg()?),
        "close" | "close-window" => Action::CloseWindow,
        "fullscreen" | "fullscreen-bordered" | "fullscreen-borderless" | "toggle-fullscreen" => {
            Action::ToggleFullscreen
        }
        "float" | "toggle-floating" => Action::ToggleFloating,
        "focus-next" => Action::FocusNext,
        "focus-prev" => Action::FocusPrev,
        "focus-left" => Action::FocusDir(Dir::Left),
        "focus-right" => Action::FocusDir(Dir::Right),
        "focus-up" => Action::FocusDir(Dir::Up),
        "focus-down" => Action::FocusDir(Dir::Down),
        "swap-left" => Action::SwapDir(Dir::Left),
        "swap-right" => Action::SwapDir(Dir::Right),
        "swap-up" => Action::SwapDir(Dir::Up),
        "swap-down" => Action::SwapDir(Dir::Down),
        // two-arg spelling: `"focus" "left"`
        "focus" => Action::FocusDir(dir_arg(&args, node, src)?),
        "swap" => Action::SwapDir(dir_arg(&args, node, src)?),
        "split-ratio" => Action::SplitRatio(
            args.get(3)
                .and_then(|a| a.parse::<f64>().ok())
                .ok_or_else(|| at(node, src, "split-ratio needs a signed delta like \"+0.1\""))?,
        ),
        "quit" => Action::Quit,
        // the rest of the dispatcher set; recognized so configs keep parsing
        known @ ("screenshot" | "move" | "resize"
        | "center" | "pin" | "toggle-group" | "group-next" | "group-prev"
        | "special" | "workspace-group" | "submap") => {
            eprintln!("carrot: config: bind action \"{known}\" not implemented yet, ignored");
            return Ok(None);
        }
        other => return Err(at(node, src, &format!("unknown action \"{other}\""))),
    };
    Ok(Some(Bind { mods, key, action, kind }))
}

fn dir_arg(args: &[String], node: &KdlNode, src: &str) -> Result<Dir, String> {
    match args.get(3).map(String::as_str) {
        Some("left" | "l") => Ok(Dir::Left),
        Some("right" | "r") => Ok(Dir::Right),
        Some("up" | "u") => Ok(Dir::Up),
        Some("down" | "d") => Ok(Dir::Down),
        _ => Err(at(node, src, "direction is left, right, up or down")),
    }
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
        "enter" | "return" => 28, "leftctrl" => 29,
        "a" => 30, "s" => 31, "d" => 32, "f" => 33, "g" => 34,
        "h" => 35, "j" => 36, "k" => 37, "l" => 38,
        "semicolon" => 39, "apostrophe" => 40, "grave" => 41,
        "leftshift" => 42, "backslash" => 43,
        "z" => 44, "x" => 45, "c" => 46, "v" => 47, "b" => 48,
        "n" => 49, "m" => 50,
        "comma" => 51, "dot" | "period" => 52, "slash" => 53,
        "rightshift" => 54, "kpasterisk" => 55, "leftalt" => 56,
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
        "kpenter" => 96, "rightctrl" => 97, "kpslash" => 98, "sysrq" => 99,
        "rightalt" => 100, "linefeed" => 101,
        "home" => 102, "up" => 103, "pageup" => 104, "left" => 105,
        "right" => 106, "end" => 107, "down" => 108, "pagedown" => 109,
        "insert" => 110, "delete" => 111, "macro" => 112,
        "mute" => 113, "volumedown" => 114, "volumeup" => 115,
        "power" => 116, "kpequal" => 117, "kpplusminus" => 118,
        "pause" => 119, "scale" => 120, "kpcomma" => 121,
        "hangeul" | "hanguel" => 122, "hanja" => 123, "yen" => 124,
        "leftmeta" => 125, "rightmeta" => 126, "compose" | "menu" => 127,
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

#[cfg(test)]
mod tests {
    use super::*;

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
