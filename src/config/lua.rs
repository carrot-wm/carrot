// lua config via piccolo - pure rust, zero c, always compiled in. the
// script builds a global `carrot` table and the walk below turns it into
// the same Config kdl produces. parity rule: unknown keys are fatal,
// never silently ignored.
//
// carrot = {
//   general = { gaps_in = 5, border_size = 2, active_border = "89b4fa" },
//   input   = { accel_profile = "flat", repeat_rate = 35 },
//   devices = { ["turtle-beach"] = { accel_speed = -0.86 } },
//   outputs = { ["DP-3"] = { mode = "2560x1440@480", vrr = "off" } },
//   binds   = { { "Meta", "Return", "exec", "foot" },
//               { "Meta", "1", "workspace", 1 } },
// }

use super::{
    Action, AnimationCfg, Bind, BindKind, Config, DeviceRule, Dir, GpuCfg, LayerRule, OutputCfg,
    RemapProfile, WindowRule,
};
use piccolo::{Closure, Executor, Lua, Table, Value};

pub fn parse(src: &str) -> Result<Config, String> {
    let mut lua = Lua::core();
    let ex = lua
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("carrot.lua"), src.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .map_err(|e| format!("lua: {e}"))?;
    lua.execute::<()>(&ex).map_err(|e| format!("lua: {e}"))?;
    lua.try_enter(|ctx| {
        let root = match ctx.get_global("carrot") {
            Value::Table(t) => t,
            _ => return Ok(Err("script must define a global `carrot` table".to_string())),
        };
        Ok(build(root))
    })
    .map_err(|e| format!("lua: {e}"))?
    .map_err(|e| format!("lua: {e}"))
}

// -- value shims --

fn vstr(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
        _ => None,
    }
}

fn vint(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Number(n) => Some(*n as i64),
        _ => None,
    }
}

fn vnum(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Number(n) => Some(*n),
        _ => None,
    }
}

fn vbool(v: &Value) -> Option<bool> {
    match v {
        Value::Boolean(b) => Some(*b),
        _ => None,
    }
}

fn need_str(v: &Value, key: &str) -> Result<String, String> {
    vstr(v).ok_or_else(|| format!("`{key}` wants a string"))
}

fn need_int(v: &Value, key: &str) -> Result<i32, String> {
    vint(v)
        .map(|n| n as i32)
        .ok_or_else(|| format!("`{key}` wants an integer"))
}

fn need_num(v: &Value, key: &str) -> Result<f64, String> {
    vnum(v).ok_or_else(|| format!("`{key}` wants a number"))
}

fn need_bool(v: &Value, key: &str) -> Result<bool, String> {
    vbool(v).ok_or_else(|| format!("`{key}` wants true/false"))
}

// hex forms: rgb, rgba, rrggbb, rrggbbaa
fn color(s: &str, key: &str) -> Result<[f32; 4], String> {
    let s = s.trim_start_matches('#');
    let err = || format!("`{key}`: \"{s}\" is not a hex color");
    let b = s.as_bytes();
    let nib = |c: u8| (c as char).to_digit(16).ok_or_else(err);
    let (r, g, bl, a) = match b.len() {
        3 | 4 => {
            let mut v = [255u32; 4];
            for (i, c) in b.iter().enumerate() {
                let n = nib(*c)?;
                v[i] = n * 16 + n;
            }
            (v[0], v[1], v[2], v[3])
        }
        6 | 8 => {
            let mut v = [255u32; 4];
            for i in 0..b.len() / 2 {
                v[i] = nib(b[i * 2])? * 16 + nib(b[i * 2 + 1])?;
            }
            (v[0], v[1], v[2], v[3])
        }
        _ => return Err(err()),
    };
    Ok([
        r as f32 / 255.0,
        g as f32 / 255.0,
        bl as f32 / 255.0,
        a as f32 / 255.0,
    ])
}

// -- the walk --

fn build(root: Table) -> Result<Config, String> {
    let mut cfg = Config::default();
    for (k, v) in root.iter() {
        let Some(key) = vstr(&k) else {
            return Err("carrot table keys must be strings".to_string());
        };
        match key.as_str() {
            "general" => general(&v, &mut cfg)?,
            "input" => input(&v, &mut cfg)?,
            "devices" => devices(&v, &mut cfg)?,
            "outputs" => outputs(&v, &mut cfg)?,
            "binds" => binds(&v, &mut cfg)?,
            "decorations" => decorations(&v, &mut cfg)?,
            "animations" => animations(&v, &mut cfg)?,
            "window_rules" => window_rules(&v, &mut cfg)?,
            "layer_rules" => layer_rules(&v, &mut cfg)?,
            "gpus" => gpus(&v, &mut cfg)?,
            "submaps" => submaps(&v, &mut cfg)?,
            "specials" => specials(&v, &mut cfg)?,
            "remaps" => remaps(&v, &mut cfg)?,
            other => return Err(format!("unknown section `{other}`")),
        }
    }
    Ok(cfg)
}

fn table<'gc>(v: &Value<'gc>, key: &str) -> Result<Table<'gc>, String> {
    match v {
        Value::Table(t) => Ok(*t),
        _ => Err(format!("`{key}` wants a table")),
    }
}

// positional {a, b, ...} of exactly N numbers; index-addressed because
// lua table iteration has no defined order
fn num_array<const N: usize>(v: &Value, key: &str) -> Result<[f64; N], String> {
    let err = || format!("`{key}` wants {N} numbers");
    let t = table(v, key).map_err(|_| err())?;
    let mut out = [0.0f64; N];
    let mut seen = 0usize;
    for (k, v) in t.iter() {
        let i = vint(&k).filter(|i| (1..=N as i64).contains(i)).ok_or_else(err)?;
        out[(i - 1) as usize] = vnum(&v).ok_or_else(err)?;
        seen += 1;
    }
    if seen != N {
        return Err(err());
    }
    Ok(out)
}

fn str_array(v: &Value, key: &str) -> Result<Vec<String>, String> {
    let err = || format!("`{key}` wants an array of strings");
    let t = table(v, key).map_err(|_| err())?;
    let mut out: Vec<(i64, String)> = Vec::new();
    for (k, v) in t.iter() {
        let i = vint(&k).ok_or_else(err)?;
        out.push((i, vstr(&v).ok_or_else(err)?));
    }
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, s)| s).collect())
}

fn accel_profile(p: String, key: &str) -> Result<String, String> {
    match p.as_str() {
        "flat" | "adaptive" => Ok(p),
        _ => Err(format!("`{key}` is flat or adaptive")),
    }
}

fn general(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "general")?.iter() {
        let key = vstr(&k).ok_or("general keys must be strings")?;
        match key.as_str() {
            "gaps_in" => cfg.gaps_in = need_int(&v, &key)?,
            "gaps_out" => cfg.gaps_out = need_int(&v, &key)?,
            "border_size" => cfg.border = need_int(&v, &key)?,
            "active_border" => cfg.border_focused = color(&need_str(&v, &key)?, &key)?,
            "inactive_border" => cfg.border_unfocused = color(&need_str(&v, &key)?, &key)?,
            "allow_tearing" => cfg.allow_tearing = need_bool(&v, &key)?,
            "float_above_fullscreen" => cfg.float_above_fullscreen = need_bool(&v, &key)?,
            "layout" => {
                cfg.layout = need_str(&v, &key)?;
                if cfg.layout != "dwindle" {
                    eprintln!(
                        "carrot: config: layout \"{}\" not implemented yet, using dwindle",
                        cfg.layout
                    );
                }
            }
            other => return Err(format!("general: unknown key `{other}`")),
        }
    }
    Ok(())
}

fn input(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "input")?.iter() {
        let key = vstr(&k).ok_or("input keys must be strings")?;
        match key.as_str() {
            "accel_profile" => {
                cfg.input.accel_profile = Some(accel_profile(need_str(&v, &key)?, &key)?)
            }
            "natural_scroll" => cfg.input.natural_scroll = need_bool(&v, &key)?,
            "tap" => cfg.input.tap = need_bool(&v, &key)?,
            "dwt" => cfg.input.dwt = need_bool(&v, &key)?,
            "layout" => cfg.input.layout = Some(need_str(&v, &key)?),
            "numlock" => cfg.input.numlock = need_bool(&v, &key)?,
            "repeat_rate" => cfg.repeat_rate = need_int(&v, &key)?,
            "repeat_delay" => cfg.repeat_delay = need_int(&v, &key)?,
            other => return Err(format!("input: unknown key `{other}`")),
        }
    }
    Ok(())
}

fn devices(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in table(v, "devices")?.iter() {
        let name = vstr(&name).ok_or("devices keys are device names")?;
        let mut rule = DeviceRule {
            name: name.clone(),
            accel_speed: None,
            accel_profile: None,
            natural_scroll: None,
            dpi: None,
        };
        for (k, v) in table(&v, &name)?.iter() {
            let key = vstr(&k).ok_or("device keys must be strings")?;
            match key.as_str() {
                "accel_speed" => rule.accel_speed = Some(need_num(&v, &key)?),
                "accel_profile" => {
                    rule.accel_profile = Some(accel_profile(need_str(&v, &key)?, &key)?)
                }
                "natural_scroll" => rule.natural_scroll = Some(need_bool(&v, &key)?),
                "dpi" => rule.dpi = Some(need_num(&v, &key)?),
                other => return Err(format!("device \"{name}\": unknown key `{other}`")),
            }
        }
        cfg.devices.push(rule);
    }
    Ok(())
}

fn outputs(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in table(v, "outputs")?.iter() {
        let name = vstr(&name).ok_or("outputs keys are connector names")?;
        let mut out = OutputCfg {
            name: name.clone(),
            vrr: None,
            gpu: None,
            scale: None,
            mode: None,
        };
        for (k, v) in table(&v, &name)?.iter() {
            let key = vstr(&k).ok_or("output keys must be strings")?;
            match key.as_str() {
                "vrr" => {
                    let m = need_str(&v, &key)?;
                    match m.as_str() {
                        "off" | "automatic" | "always" => out.vrr = Some(m),
                        _ => {
                            return Err(format!(
                                "output \"{name}\": vrr is off, automatic or always"
                            ))
                        }
                    }
                }
                "gpu" => out.gpu = Some(need_str(&v, &key)?),
                "scale" => out.scale = Some(need_num(&v, &key)?),
                "mode" => {
                    let s = need_str(&v, &key)?;
                    out.mode = Some(
                        super::parse_mode(&s)
                            .ok_or_else(|| format!("output \"{name}\": bad mode \"{s}\""))?,
                    );
                }
                other => return Err(format!("output \"{name}\": unknown key `{other}`")),
            }
        }
        cfg.outputs.push(out);
    }
    Ok(())
}

// positional { mods, key, action, arg? }; optional `type` picks when the bind fires
fn binds(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (idx, v) in table(v, "binds")?.iter() {
        let n = vint(&idx).unwrap_or(0);
        if let Some(b) = bind_entry(&v, n)? {
            cfg.binds.push(b);
        }
    }
    Ok(())
}

fn bind_entry<'gc>(v: &Value<'gc>, n: i64) -> Result<Option<Bind>, String> {
    let t = table(v, "binds entry")?;
    let mut args: [Value<'gc>; 4] = [Value::Nil; 4];
    let mut kind = BindKind::Press;
    for (k, v) in t.iter() {
        if let Some(i) = vint(&k) {
            if !(1..=4).contains(&i) {
                return Err(format!("bind {n}: wants mods, key, action, arg"));
            }
            args[(i - 1) as usize] = v;
        } else if vstr(&k).as_deref() == Some("type") {
            kind = match vstr(&v).as_deref() {
                Some("press") => BindKind::Press,
                Some("release") => BindKind::Release,
                Some("repeat") => BindKind::Repeat,
                Some("lock-safe") => BindKind::LockSafe,
                Some("mouse") => BindKind::Mouse,
                _ => {
                    return Err(format!(
                        "bind {n}: type is press, release, repeat, lock-safe or mouse"
                    ))
                }
            };
        } else {
            return Err(format!("bind {n}: unknown bind key"));
        }
    }
    {
        let mods_s = vstr(&args[0]).ok_or_else(|| format!("bind {n}: mods wants a string"))?;
        let key_s = vstr(&args[1]).ok_or_else(|| format!("bind {n}: key wants a string"))?;
        let act_s = vstr(&args[2]).ok_or_else(|| format!("bind {n}: action wants a string"))?;
        let arg = args[3];
        let mods = super::parse_mods(&mods_s).map_err(|e| format!("bind {n}: {e}"))?;
        let key = super::keycode(&key_s.to_lowercase())
            .ok_or_else(|| format!("bind {n}: unknown key \"{key_s}\""))?;
        let ws_arg = || -> Result<usize, String> {
            vint(&arg)
                .map(|w| (w as usize).saturating_sub(1))
                .ok_or_else(|| format!("bind {n}: workspace binds need a number"))
        };
        let dir_arg = || -> Result<Dir, String> {
            match vstr(&arg).as_deref() {
                Some("left" | "l") => Ok(Dir::Left),
                Some("right" | "r") => Ok(Dir::Right),
                Some("up" | "u") => Ok(Dir::Up),
                Some("down" | "d") => Ok(Dir::Down),
                _ => Err(format!("bind {n}: direction is left, right, up or down")),
            }
        };
        let action = match act_s.as_str() {
            "exec" | "spawn" => Action::Spawn(
                vstr(&arg).ok_or_else(|| format!("bind {n}: exec wants a command string"))?,
            ),
            "workspace" => {
                // string "+1"/"-1" (or "r+1") jumps relative; numbers stay absolute
                let rel = vstr(&arg)
                    .map(|a| a.strip_prefix('r').unwrap_or(&a).to_string())
                    .filter(|a| a.starts_with(['+', '-']));
                match rel {
                    Some(r) => Action::WorkspaceRel(r.parse::<i32>().map_err(|_| {
                        format!("bind {n}: relative workspace wants \"+N\" or \"-N\"")
                    })?),
                    None => Action::Workspace(ws_arg()?),
                }
            }
            "movetoworkspace" | "move-to-workspace" => Action::MoveToWorkspace(ws_arg()?),
            "sendtoworkspace" | "send-to-workspace" => Action::SendToWorkspace(ws_arg()?),
            "close" | "close-window" => Action::CloseWindow,
            "fullscreen" | "fullscreen-bordered" | "fullscreen-borderless"
            | "toggle-fullscreen" => Action::ToggleFullscreen,
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
            "focus" => Action::FocusDir(dir_arg()?),
            "swap" => Action::SwapDir(dir_arg()?),
            "split-ratio" => Action::SplitRatio(
                vnum(&arg)
                    .or_else(|| vstr(&arg).and_then(|s| s.parse().ok()))
                    .ok_or_else(|| format!("bind {n}: split-ratio wants a signed number"))?,
            ),
            "quit" => Action::Quit,
            // the rest of the dispatcher set; recognized so configs keep parsing
            known @ ("screenshot" | "move" | "resize" | "center" | "pin" | "toggle-group"
            | "group-next" | "group-prev" | "special" | "workspace-group" | "submap") => {
                eprintln!("carrot: config: bind action \"{known}\" not implemented yet, ignored");
                return Ok(None);
            }
            other => return Err(format!("bind {n}: unknown action \"{other}\"")),
        };
        Ok(Some(Bind { mods, key, action, kind }))
    }
}

// -- decorations --

fn decorations(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "decorations")?.iter() {
        let key = vstr(&k).ok_or("decorations keys must be strings")?;
        match key.as_str() {
            "rounding" => cfg.decoration.rounding = need_int(&v, &key)?,
            "blur" => {
                for (k, v) in table(&v, &key)?.iter() {
                    let key = vstr(&k).ok_or("blur keys must be strings")?;
                    match key.as_str() {
                        "enabled" => cfg.decoration.blur.enabled = need_bool(&v, &key)?,
                        "size" => cfg.decoration.blur.size = need_int(&v, &key)?,
                        "passes" => cfg.decoration.blur.passes = need_int(&v, &key)?,
                        other => return Err(format!("blur: unknown key `{other}`")),
                    }
                }
            }
            "shadow" => {
                for (k, v) in table(&v, &key)?.iter() {
                    let key = vstr(&k).ok_or("shadow keys must be strings")?;
                    match key.as_str() {
                        "enabled" => cfg.decoration.shadow.enabled = need_bool(&v, &key)?,
                        "range" => cfg.decoration.shadow.range = need_int(&v, &key)?,
                        other => return Err(format!("shadow: unknown key `{other}`")),
                    }
                }
            }
            other => return Err(format!("decorations: unknown key `{other}`")),
        }
    }
    Ok(())
}

// -- animations --

// lua tables have no iteration order, so the name-keyed maps below sort by
// name to keep reloads deterministic
fn animations(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "animations")?.iter() {
        let key = vstr(&k).ok_or("animations keys must be strings")?;
        match key.as_str() {
            "beziers" => {
                for (name, v) in table(&v, &key)?.iter() {
                    let name = vstr(&name).ok_or("beziers keys are curve names")?;
                    let pts = num_array::<4>(&v, &name)?;
                    cfg.animations.beziers.push((name, pts));
                }
                cfg.animations.beziers.sort_by(|a, b| a.0.cmp(&b.0));
            }
            "springs" => {
                for (name, v) in table(&v, &key)?.iter() {
                    let name = vstr(&name).ok_or("springs keys are curve names")?;
                    let pts = num_array::<3>(&v, &name)?;
                    cfg.animations.springs.push((name, pts));
                }
                cfg.animations.springs.sort_by(|a, b| a.0.cmp(&b.0));
            }
            "animations" => {
                for (name, v) in table(&v, &key)?.iter() {
                    let name = vstr(&name).ok_or("animations keys are names")?;
                    let mut a = AnimationCfg {
                        enabled: true,
                        speed: 1.0,
                        curve: String::new(),
                        style: None,
                    };
                    for (k, v) in table(&v, &name)?.iter() {
                        let key = vstr(&k).ok_or("animation keys must be strings")?;
                        match key.as_str() {
                            "enabled" => a.enabled = need_bool(&v, &key)?,
                            "speed" => a.speed = need_num(&v, &key)?,
                            "curve" => a.curve = need_str(&v, &key)?,
                            "style" => a.style = Some(need_str(&v, &key)?),
                            other => {
                                return Err(format!(
                                    "animation \"{name}\": unknown key `{other}`"
                                ))
                            }
                        }
                    }
                    cfg.animations.animations.push((name, a));
                }
                cfg.animations.animations.sort_by(|a, b| a.0.cmp(&b.0));
            }
            other => return Err(format!("animations: unknown key `{other}`")),
        }
    }
    Ok(())
}

// -- rules --

fn window_rules(v: &Value, cfg: &mut Config) -> Result<(), String> {
    let mut rules: Vec<(i64, WindowRule)> = Vec::new();
    for (idx, v) in table(v, "window_rules")?.iter() {
        let n = vint(&idx).ok_or("window_rules is an array of tables")?;
        let mut r = WindowRule::default();
        for (k, v) in table(&v, "window_rules entry")?.iter() {
            let key = vstr(&k).ok_or("window rule keys must be strings")?;
            match key.as_str() {
                "match_class" => r.match_class = Some(need_str(&v, &key)?),
                "match_title" => r.match_title = Some(need_str(&v, &key)?),
                "match_fullscreen" => r.match_fullscreen = Some(need_bool(&v, &key)?),
                "match_xwayland" => r.match_xwayland = Some(need_bool(&v, &key)?),
                "match_floating" => r.match_floating = Some(need_bool(&v, &key)?),
                "floating" => r.floating = need_bool(&v, &key)?,
                "tile" => r.tile = need_bool(&v, &key)?,
                "workspace" => {
                    r.workspace = Some((need_int(&v, &key)? as usize).saturating_sub(1))
                }
                "immediate" => r.immediate = need_bool(&v, &key)?,
                "idle_inhibit" => r.idle_inhibit = Some(need_str(&v, &key)?),
                "opacity" => r.opacity = Some(need_num(&v, &key)?),
                "size" => {
                    let wh = num_array::<2>(&v, &key)?;
                    r.size = Some((wh[0] as i32, wh[1] as i32));
                }
                "center" => r.center = need_bool(&v, &key)?,
                "rounding" => r.rounding = Some(need_int(&v, &key)?),
                "blur" => r.blur = Some(need_bool(&v, &key)?),
                "shadow" => r.shadow = Some(need_bool(&v, &key)?),
                "dim" => r.dim = Some(need_num(&v, &key)?),
                "pin" => r.pin = need_bool(&v, &key)?,
                "keep_aspect_ratio" => r.keep_aspect_ratio = need_bool(&v, &key)?,
                "focus_steal" => r.focus_steal = need_bool(&v, &key)?,
                "redirect_mode" => {
                    let m = need_str(&v, &key)?;
                    if m != "redirect" && m != "passthrough" {
                        return Err(format!(
                            "window rule {n}: redirect_mode is redirect or passthrough"
                        ));
                    }
                    r.redirect_mode = Some(m);
                }
                "redirect_keys" => r.redirect_keys = str_array(&v, &key)?,
                other => return Err(format!("window rule {n}: unknown key `{other}`")),
            }
        }
        for pat in [&r.match_class, &r.match_title] {
            if let Some(p) = pat {
                regex_lite::Regex::new(p)
                    .map_err(|e| format!("window rule {n}: bad regex \"{p}\": {e}"))?;
            }
        }
        rules.push((n, r));
    }
    rules.sort_by_key(|(n, _)| *n);
    cfg.rules.extend(rules.into_iter().map(|(_, r)| r));
    Ok(())
}

fn layer_rules(v: &Value, cfg: &mut Config) -> Result<(), String> {
    let mut rules: Vec<(i64, LayerRule)> = Vec::new();
    for (idx, v) in table(v, "layer_rules")?.iter() {
        let n = vint(&idx).ok_or("layer_rules is an array of tables")?;
        let mut r = LayerRule::default();
        for (k, v) in table(&v, "layer_rules entry")?.iter() {
            let key = vstr(&k).ok_or("layer rule keys must be strings")?;
            match key.as_str() {
                "match_namespace" => r.match_namespace = Some(need_str(&v, &key)?),
                "animation" => r.animation = Some(need_str(&v, &key)?),
                "blur" => r.blur = Some(need_bool(&v, &key)?),
                "ignore_alpha" => r.ignore_alpha = Some(need_num(&v, &key)?),
                other => return Err(format!("layer rule {n}: unknown key `{other}`")),
            }
        }
        rules.push((n, r));
    }
    rules.sort_by_key(|(n, _)| *n);
    cfg.layer_rules.extend(rules.into_iter().map(|(_, r)| r));
    Ok(())
}

// -- gpus, submaps, specials, remaps --

fn gpus(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in table(v, "gpus")?.iter() {
        let name = vstr(&name).ok_or("gpus keys are card names")?;
        let mut gpu = GpuCfg { name: name.clone(), skip_renderer: false };
        for (k, v) in table(&v, &name)?.iter() {
            let key = vstr(&k).ok_or("gpu keys must be strings")?;
            match key.as_str() {
                "skip_renderer" => gpu.skip_renderer = need_bool(&v, &key)?,
                other => return Err(format!("gpu \"{name}\": unknown key `{other}`")),
            }
        }
        cfg.gpus.push(gpu);
    }
    cfg.gpus.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

fn submaps(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in table(v, "submaps")?.iter() {
        let name = vstr(&name).ok_or("submaps keys are submap names")?;
        let mut entries: Vec<(i64, Bind)> = Vec::new();
        for (idx, v) in table(&v, &name)?.iter() {
            let n = vint(&idx).ok_or_else(|| format!("submap \"{name}\" is a bind array"))?;
            if let Some(b) = bind_entry(&v, n)? {
                entries.push((n, b));
            }
        }
        entries.sort_by_key(|(n, _)| *n);
        cfg.submaps
            .push((name, entries.into_iter().map(|(_, b)| b).collect()));
    }
    cfg.submaps.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(())
}

// name = "command" spawns on first toggle; name = true is a bare scratchpad
fn specials(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in table(v, "specials")?.iter() {
        let name = vstr(&name).ok_or("specials keys are scratchpad names")?;
        let spawn = match (vstr(&v), vbool(&v)) {
            (Some(s), _) => Some(s),
            (None, Some(true)) => None,
            _ => {
                return Err(format!(
                    "special \"{name}\" wants a spawn string or true"
                ))
            }
        };
        cfg.specials.push((name, spawn));
    }
    cfg.specials.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(())
}

fn remaps(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in table(v, "remaps")?.iter() {
        let name = vstr(&name).ok_or("remaps keys are profile names")?;
        let mut p = RemapProfile { name: name.clone(), ..Default::default() };
        for (k, v) in table(&v, &name)?.iter() {
            let key = vstr(&k).ok_or("remap keys must be strings")?;
            match key.as_str() {
                "class" => p.class = Some(need_str(&v, &key)?),
                "title" => p.title = Some(need_str(&v, &key)?),
                "type" => {
                    let t = need_str(&v, &key)?;
                    if t != "x11" && t != "wayland" {
                        return Err(format!(
                            "remap \"{name}\": type is \"x11\" or \"wayland\""
                        ));
                    }
                    p.win_type = Some(t);
                }
                "pid" => p.pid = Some(need_int(&v, &key)?),
                "workspace" => p.workspace = Some(need_int(&v, &key)?.max(1) as usize),
                "maps" => {
                    let mut entries: Vec<(i64, (u32, u32))> = Vec::new();
                    for (idx, v) in table(&v, &key)?.iter() {
                        let n = vint(&idx)
                            .ok_or_else(|| format!("remap \"{name}\": maps is an array"))?;
                        let pair = str_array(&v, &key)?;
                        let [from, to] = pair.as_slice() else {
                            return Err(format!("remap \"{name}\": map wants two key names"));
                        };
                        let f = super::keycode(&from.to_lowercase()).ok_or_else(|| {
                            format!("remap \"{name}\": unknown key \"{from}\"")
                        })?;
                        let t = super::keycode(&to.to_lowercase()).ok_or_else(|| {
                            format!("remap \"{name}\": unknown key \"{to}\"")
                        })?;
                        entries.push((n, (f, t)));
                    }
                    entries.sort_by_key(|(n, _)| *n);
                    p.maps = entries.into_iter().map(|(_, m)| m).collect();
                }
                other => return Err(format!("remap \"{name}\": unknown key `{other}`")),
            }
        }
        if p.maps.is_empty() {
            return Err(format!("remap \"{name}\" has no map entries"));
        }
        cfg.remaps.push(p);
    }
    cfg.remaps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lua_config_builds_the_same_shape() {
        let cfg = parse(
            r#"
            carrot = {
                general = {
                    gaps_in = 5, gaps_out = 10, border_size = 2,
                    active_border = "89b4fa", inactive_border = "585b70",
                    allow_tearing = true,
                },
                input = { accel_profile = "flat", natural_scroll = true, repeat_rate = 35 },
                devices = { ["turtle-beach"] = { accel_speed = -0.86 } },
                outputs = { ["DP-3"] = { mode = "2560x1440@480" } },
                binds = {
                    { "Meta", "Return", "exec", "foot" },
                    { "Meta", "1", "workspace", 1 },
                },
            }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.gaps_in, 5);
        assert_eq!(cfg.border, 2);
        assert!(cfg.allow_tearing);
        assert_eq!(cfg.input.accel_profile.as_deref(), Some("flat"));
        assert_eq!(cfg.repeat_rate, 35);
        assert_eq!(cfg.devices[0].accel_speed, Some(-0.86));
        assert_eq!(cfg.outputs[0].mode, Some((2560, 1440, Some(480))));
        // defaults come first, both parsers append; ours are the tail
        let base = Config::default().binds.len();
        assert_eq!(cfg.binds.len(), base + 2);
        assert_eq!(cfg.binds[base].action, Action::Spawn("foot".to_string()));
        assert_eq!(cfg.binds[base + 1].action, Action::Workspace(0));
    }

    // kdl is the reference: every section spelled in both languages must
    // land on the identical Config
    #[test]
    fn kdl_and_lua_reach_the_same_config() {
        let kdl = super::super::parse(
            r##"
            general {
                gaps-in 4
                gaps-out 8
                border-size 2
                active-border "89b4fa"
                inactive-border "585b70cc"
                float-above-fullscreen #true
                layout "dwindle"
                allow-tearing #true
            }
            input {
                repeat-rate 35
                repeat-delay 250
                accel-profile "adaptive"
                natural-scroll #true
                tap #true
                dwt #true
                layout "de"
                numlock #true
            }
            device "turtle-beach" {
                accel-speed -0.86
                accel-profile "flat"
                natural-scroll #false
                dpi 1600.0
            }
            bind "Meta" "Return" "exec" "foot"
            bind "Meta" "1" "workspace" "1"
            bind "Meta+Shift" "q" "close" type="release"
            decoration {
                rounding 8
                blur { enabled #true; size 6; passes 3 }
                shadow { enabled #true; range 24 }
            }
            animations {
                bezier "ease" 0.25 0.1 0.25 1.0
                spring "pop" 1.0 200.0 20.0
                animation "windows" { enabled #true; speed 2.0; curve "ease"; style "popin" }
            }
            window-rule {
                match class="^foot$" title="^scratch$" fullscreen=#false xwayland=#false floating=#true
                floating #true
                workspace 3
                immediate #true
                idle-inhibit "focus"
                opacity 0.9
                size 800 600
                center #true
                rounding 4
                blur #false
                shadow #false
                dim 0.2
                pin #true
                keep-aspect-ratio #true
                focus-steal #true
                input-redirect "passthrough" "f13" "f14"
            }
            window-rule {
                match class="^steam_app_.*$"
                tile #true
            }
            layer-rule {
                match namespace="^launcher$"
                animation "fade"
                blur #true
                ignore-alpha 0.2
            }
            output "DP-3" {
                mode "2560x1440@480"
                vrr "automatic"
                gpu "card1"
                scale 1.5
            }
            gpu "card1" { skip-renderer #true }
            special "term" { spawn "foot -a term" }
            special "zzz"
            submap "resize" {
                bind "" "h" "focus-left"
            }
            remap "isaac" {
                class "steam_app_250900"
                title "Isaac"
                type "x11"
                pid 4242
                workspace 2
                map "Alt_R" "Left"
                map "Slash" "Up"
            }
            "##,
        )
        .unwrap();
        let lua = parse(
            r#"
            carrot = {
                general = {
                    gaps_in = 4, gaps_out = 8, border_size = 2,
                    active_border = "89b4fa", inactive_border = "585b70cc",
                    float_above_fullscreen = true, layout = "dwindle",
                    allow_tearing = true,
                },
                input = {
                    repeat_rate = 35, repeat_delay = 250,
                    accel_profile = "adaptive", natural_scroll = true,
                    tap = true, dwt = true, layout = "de", numlock = true,
                },
                devices = {
                    ["turtle-beach"] = {
                        accel_speed = -0.86, accel_profile = "flat",
                        natural_scroll = false, dpi = 1600.0,
                    },
                },
                binds = {
                    { "Meta", "Return", "exec", "foot" },
                    { "Meta", "1", "workspace", 1 },
                    { "Meta+Shift", "q", "close", type = "release" },
                },
                decorations = {
                    rounding = 8,
                    blur = { enabled = true, size = 6, passes = 3 },
                    shadow = { enabled = true, range = 24 },
                },
                animations = {
                    beziers = { ease = { 0.25, 0.1, 0.25, 1.0 } },
                    springs = { pop = { 1.0, 200.0, 20.0 } },
                    animations = {
                        windows = { enabled = true, speed = 2.0, curve = "ease", style = "popin" },
                    },
                },
                window_rules = {
                    {
                        match_class = "^foot$", match_title = "^scratch$",
                        match_fullscreen = false, match_xwayland = false, match_floating = true,
                        floating = true, workspace = 3, immediate = true,
                        idle_inhibit = "focus", opacity = 0.9, size = { 800, 600 },
                        center = true, rounding = 4, blur = false, shadow = false,
                        dim = 0.2, pin = true, keep_aspect_ratio = true, focus_steal = true,
                        redirect_mode = "passthrough", redirect_keys = { "f13", "f14" },
                    },
                    { match_class = "^steam_app_.*$", tile = true },
                },
                layer_rules = {
                    {
                        match_namespace = "^launcher$", animation = "fade",
                        blur = true, ignore_alpha = 0.2,
                    },
                },
                outputs = {
                    ["DP-3"] = { mode = "2560x1440@480", vrr = "automatic", gpu = "card1", scale = 1.5 },
                },
                gpus = { card1 = { skip_renderer = true } },
                specials = { term = "foot -a term", zzz = true },
                submaps = {
                    resize = { { "", "h", "focus-left" } },
                },
                remaps = {
                    isaac = {
                        class = "steam_app_250900", title = "Isaac", type = "x11",
                        pid = 4242, workspace = 2,
                        maps = { { "Alt_R", "Left" }, { "Slash", "Up" } },
                    },
                },
            }
            "#,
        )
        .unwrap();
        assert_eq!(kdl, lua);
    }

    #[test]
    fn new_lua_sections_fail_loud() {
        assert!(parse("carrot = { decorations = { runding = 1 } }").is_err());
        assert!(parse("carrot = { animations = { bezier = {} } }").is_err());
        assert!(
            parse(r#"carrot = { animations = { beziers = { e = { 0.1, 0.2 } } } }"#).is_err(),
            "a bezier wants four numbers"
        );
        assert!(
            parse(r#"carrot = { window_rules = { { match_class = "[unclosed" } } }"#).is_err(),
            "bad regex must fail at parse"
        );
        assert!(
            parse(r#"carrot = { window_rules = { { redirect_mode = "sometimes" } } }"#).is_err()
        );
        assert!(parse(r#"carrot = { layer_rules = { { blurr = true } } }"#).is_err());
        assert!(parse(r#"carrot = { gpus = { card0 = { skip = true } } }"#).is_err());
        assert!(parse(r#"carrot = { specials = { s = false } }"#).is_err());
        assert!(
            parse(r#"carrot = { remaps = { r = { class = "x" } } }"#).is_err(),
            "a remap with no maps is dead weight"
        );
        assert!(
            parse(r#"carrot = { remaps = { r = { maps = { { "a", "nope" } } } } }"#).is_err(),
            "unknown key names must fail"
        );
        assert!(
            parse(r#"carrot = { input = { accel_profile = "warp" } }"#).is_err(),
            "accel profile is flat or adaptive"
        );
        assert!(
            parse(r#"carrot = { outputs = { ["DP-1"] = { vrr = "sometimes" } } }"#).is_err()
        );
        assert!(
            parse(r#"carrot = { binds = { { "Meta", "q", "close", type = "hold" } } }"#).is_err()
        );
    }

    #[test]
    fn lua_fails_loud() {
        // real logic runs before the table lands
        assert!(parse("this is not lua").is_err());
        assert!(parse("x = 1").is_err(), "no carrot table");
        assert!(
            parse("carrot = { generall = {} }").is_err(),
            "typo'd section must not be ignored"
        );
        assert!(
            parse("carrot = { general = { gaps_in = 'five' } }").is_err(),
            "wrong type must not be ignored"
        );
    }
}
