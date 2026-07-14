// lua config via piccolo - pure rust, zero c, always compiled in. the
// script builds a global `carrot` table and the walk below turns it into
// the same Config kdl produces, through the same leaf validators. parity
// rule: unknown keys are fatal, never silently ignored. errors accumulate
// per section so one typo doesn't hide the rest.
//
// carrot = {
//   input  = { keyboard = { repeat_rate = 35 }, mouse = { accel_profile = "flat" },
//              devices = { ["razer viper"] = { dpi = 1600 } } },
//   layout = { gaps_in = 5, border = { width = 2, active_color = "#89b4fa" } },
//   outputs = { ["DP-3"] = { mode = "2560x1440@480", vrr = "always" } },
//   binds  = { { chord = "Mod+Return", action = "spawn", args = { "foot" } },
//              { chord = "Mod+1", action = "focus-workspace", args = { 1 } } },
// }

use super::{
    Action, Bind, BlurCfg, CenterFocus, ColWidthCfg, Config, CurveRef, DeviceRule, Dir, KindCfg, LayerRule,
    LayoutMode, ModKey, Motion, OutputCfg, PointerClassCfg, RemapProfile, RuleMatch,
    SetLayoutArg, ShadowCfg, SpawnCfg, Vrr, WindowRule, WsAxis,
};
use piccolo::{Callback, CallbackReturn, Closure, Executor, IntoValue, Lua, Table, Value};

/// the anchorless test entry; live paths use `parse_at` so include()
/// resolves against the real file
#[cfg(test)]
pub fn parse(src: &str) -> Result<Config, Vec<String>> {
    parse_inner(src, None)
}

/// same, anchored at the file's real path so `include("other.lua")`
/// resolves relative to it
pub fn parse_at(src: &str, path: &std::path::Path) -> Result<Config, Vec<String>> {
    parse_inner(src, path.parent().map(std::path::Path::to_path_buf))
}

fn parse_inner(src: &str, dir: Option<std::path::PathBuf>) -> Result<Config, Vec<String>> {
    let mut lua = Lua::core();
    let ex = lua
        .try_enter(|ctx| {
            // `include("file.lua")` runs the file in the same globals; the
            // core sandbox has no io, so the loader is ours to provide
            if let Some(dir) = dir {
                let budget = std::rc::Rc::new(std::cell::Cell::new(0usize));
                let include = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    let name: piccolo::String = stack.consume(ctx)?;
                    let rel = std::str::from_utf8(name.as_bytes())
                        .unwrap_or_default()
                        .to_string();
                    if budget.replace(budget.get() + 1) >= 64 {
                        return Err("include: too many included files"
                            .into_value(ctx)
                            .into());
                    }
                    let q = std::path::Path::new(&rel);
                    let p = if q.is_absolute() { q.to_path_buf() } else { dir.join(q) };
                    let text = match std::fs::read_to_string(&p) {
                        Ok(t) => t,
                        Err(e) => {
                            return Err(format!("include {}: {e}", p.display())
                                .into_value(ctx)
                                .into());
                        }
                    };
                    let closure = Closure::load(ctx, Some(rel.as_str()), text.as_bytes())?;
                    Ok(CallbackReturn::Call { function: closure.into(), then: None })
                });
                ctx.set_global("include", include)?;
            }
            let closure = Closure::load(ctx, Some("carrot.lua"), src.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .map_err(|e| vec![format!("lua: {e}")])?;
    lua.execute::<()>(&ex).map_err(|e| vec![format!("lua: {e}")])?;
    lua.try_enter(|ctx| {
        let root = match ctx.get_global("carrot") {
            Value::Table(t) => t,
            _ => {
                return Ok(Err(vec![
                    "lua: script must define a global `carrot` table".to_string(),
                ]))
            }
        };
        Ok(build(root))
    })
    .map_err(|e| vec![format!("lua: {e}")])?
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

fn need_int(v: &Value, key: &str) -> Result<i64, String> {
    vint(v).ok_or_else(|| format!("`{key}` wants an integer"))
}

fn need_num(v: &Value, key: &str) -> Result<f64, String> {
    vnum(v).ok_or_else(|| format!("`{key}` wants a number"))
}

fn need_bool(v: &Value, key: &str) -> Result<bool, String> {
    vbool(v).ok_or_else(|| format!("`{key}` wants true or false"))
}

fn table<'gc>(v: &Value<'gc>, key: &str) -> Result<Table<'gc>, String> {
    match v {
        Value::Table(t) => Ok(*t),
        _ => Err(format!("`{key}` wants a table")),
    }
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

/// name-keyed sub-tables walked in sorted order; lua iteration has no
/// defined order and reloads must be deterministic
fn named_entries<'gc>(v: &Value<'gc>, what: &str) -> Result<Vec<(String, Value<'gc>)>, String> {
    let mut out = Vec::new();
    for (k, v) in table(v, what)?.iter() {
        let name = vstr(&k).ok_or_else(|| format!("{what} keys are strings"))?;
        out.push((name, v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// index-keyed arrays walked in index order
fn indexed_entries<'gc>(v: &Value<'gc>, what: &str) -> Result<Vec<Value<'gc>>, String> {
    let mut out: Vec<(i64, Value)> = Vec::new();
    for (k, v) in table(v, what)?.iter() {
        let i = vint(&k).ok_or_else(|| format!("{what} is an array"))?;
        out.push((i, v));
    }
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, v)| v).collect())
}

// -- the walk --

fn build(root: Table) -> Result<Config, Vec<String>> {
    let mut cfg = Config::default();
    let mut errs: Vec<String> = Vec::new();
    // a script that speaks a repeated section at all replaces the defaults
    for (k, _) in root.iter() {
        match vstr(&k).as_deref() {
            Some("binds") => cfg.binds.clear(),
            Some("outputs") => cfg.outputs.clear(),
            Some("window_rules") => cfg.rules.clear(),
            Some("layer_rules") => cfg.layer_rules.clear(),
            Some("remaps") => cfg.remaps.clear(),
            Some("spawn_at_startup") => cfg.spawns.clear(),
            Some("environment") => cfg.environment.clear(),
            _ => {}
        }
    }
    for (k, v) in root.iter() {
        let Some(key) = vstr(&k) else {
            errs.push("lua: carrot table keys must be strings".to_string());
            continue;
        };
        let r = match key.as_str() {
            "animations" => animations(&v, &mut cfg),
            "decoration" => decoration(&v, &mut cfg),
            "input" => input(&v, &mut cfg),
            "outputs" => outputs(&v, &mut cfg),
            "layout" => layout(&v, &mut cfg),
            "cursor" => cursor(&v, &mut cfg),
            "environment" => environment(&v, &mut cfg),
            "spawn_at_startup" => spawns(&v, &mut cfg),
            "prefer_no_csd" => need_bool(&v, "prefer_no_csd").map(|b| cfg.prefer_no_csd = b),
            "screencast" => screencast(&v, &mut cfg),
            "binds" => binds(&v, &mut cfg),
            "switch_events" => Err("switch_events: not implemented yet".to_string()),
            "window_rules" => window_rules(&v, &mut cfg),
            "layer_rules" => layer_rules(&v, &mut cfg),
            "remaps" => remaps(&v, &mut cfg),
            "debug" => debug(&v, &mut cfg),
            other => Err(format!("unknown section `{other}`")),
        };
        if let Err(e) = r {
            errs.push(format!("lua: {key}: {e}"));
        }
    }
    super::resolve_mod(&mut cfg.binds, cfg.input.mod_key);
    if errs.is_empty() { Ok(cfg) } else { Err(errs) }
}

fn input(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "input")?.iter() {
        let key = vstr(&k).ok_or("input keys must be strings")?;
        match key.as_str() {
            "keyboard" => keyboard(&v, cfg)?,
            "touchpad" => pointer_class(&v, "touchpad")
                .map(|p| cfg.input.touchpad = p)?,
            "mouse" => pointer_class(&v, "mouse").map(|p| cfg.input.mouse = p)?,
            "devices" => devices(&v, cfg)?,
            "mod_key" => {
                cfg.input.mod_key = match need_str(&v, &key)?.as_str() {
                    "super" => ModKey::Super,
                    "alt" => ModKey::Alt,
                    _ => return Err("mod_key is \"super\" or \"alt\"".to_string()),
                };
            }
            other => return Err(format!("unknown key `{other}`")),
        }
    }
    Ok(())
}

fn keyboard(v: &Value, cfg: &mut Config) -> Result<(), String> {
    let kb = &mut cfg.input.keyboard;
    for (k, v) in table(v, "keyboard")?.iter() {
        let key = vstr(&k).ok_or("keyboard keys must be strings")?;
        match key.as_str() {
            "xkb" => {
                for (k, v) in table(&v, "xkb")?.iter() {
                    let key = vstr(&k).ok_or("xkb keys must be strings")?;
                    match key.as_str() {
                        "layout" => kb.xkb.layout = Some(need_str(&v, &key)?),
                        "variant" => kb.xkb.variant = Some(need_str(&v, &key)?),
                        "options" => kb.xkb.options = Some(need_str(&v, &key)?),
                        other => return Err(format!("unknown xkb key `{other}`")),
                    }
                }
            }
            "repeat_rate" => {
                kb.repeat_rate = super::int_in(need_int(&v, &key)?, "repeat_rate", 1, 200)? as i32;
            }
            "repeat_delay" => {
                kb.repeat_delay =
                    super::int_in(need_int(&v, &key)?, "repeat_delay", 1, 5000)? as i32;
            }
            "numlock" => kb.numlock = need_bool(&v, &key)?,
            other => return Err(format!("unknown keyboard key `{other}`")),
        }
    }
    Ok(())
}

fn pointer_class(v: &Value, what: &str) -> Result<PointerClassCfg, String> {
    let mut out = PointerClassCfg::default();
    for (k, v) in table(v, what)?.iter() {
        let key = vstr(&k).ok_or("keys must be strings")?;
        match key.as_str() {
            "accel_profile" => {
                out.accel_profile = Some(super::accel_profile(&need_str(&v, &key)?)?);
            }
            "accel_speed" => {
                out.accel_speed =
                    Some(super::f64_in(need_num(&v, &key)?, "accel_speed", -1.0, 1.0)?);
            }
            "natural_scroll" => out.natural_scroll = need_bool(&v, &key)?,
            "tap" | "dwt" => return Err(format!("{key}: not implemented yet")),
            other => return Err(format!("unknown {what} key `{other}`")),
        }
    }
    Ok(out)
}

fn devices(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in named_entries(v, "devices")? {
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
                "accel_speed" => {
                    rule.accel_speed =
                        Some(super::f64_in(need_num(&v, &key)?, "accel_speed", -1.0, 1.0)?);
                }
                "accel_profile" => {
                    rule.accel_profile = Some(super::accel_profile(&need_str(&v, &key)?)?);
                }
                "natural_scroll" => rule.natural_scroll = Some(need_bool(&v, &key)?),
                "dpi" => {
                    rule.dpi = Some(super::f64_in(need_num(&v, &key)?, "dpi", 100.0, 40000.0)?);
                }
                other => return Err(format!("device \"{name}\": unknown key `{other}`")),
            }
        }
        cfg.input.devices.push(rule);
    }
    Ok(())
}

fn outputs(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in named_entries(v, "outputs")? {
        let mut out = OutputCfg {
            name: name.clone(),
            vrr: Vrr::Off,
            scale: None,
            mode: None,
            position: None,
            off: false,
            allow_tearing: false,
        };
        for (k, v) in table(&v, &name)?.iter() {
            let key = vstr(&k).ok_or("output keys must be strings")?;
            match key.as_str() {
                "mode" => {
                    let m = need_str(&v, &key)?;
                    out.mode = Some(
                        super::parse_mode(&m)
                            .ok_or_else(|| "mode looks like \"2560x1440@240\"".to_string())?,
                    );
                }
                "scale" => {
                    out.scale = Some(super::f64_in(need_num(&v, &key)?, "scale", 0.25, 4.0)?);
                }
                "position" => {
                    let (mut x, mut y) = (None, None);
                    for (k, v) in table(&v, &key)?.iter() {
                        match vstr(&k).as_deref() {
                            Some("x") => x = vint(&v),
                            Some("y") => y = vint(&v),
                            _ => return Err("position wants x and y".to_string()),
                        }
                    }
                    match (x, y) {
                        (Some(x), Some(y)) => out.position = Some((x as i32, y as i32)),
                        _ => return Err("position wants x and y".to_string()),
                    }
                }
                "vrr" => {
                    out.vrr = match need_str(&v, &key)?.as_str() {
                        "off" => Vrr::Off,
                        "on-demand" => Vrr::OnDemand,
                        "always" => Vrr::Always,
                        _ => return Err("vrr is off, on-demand or always".to_string()),
                    };
                }
                "off" => out.off = need_bool(&v, &key)?,
                "allow_tearing" => out.allow_tearing = need_bool(&v, &key)?,
                other => return Err(format!("output \"{name}\": unknown key `{other}`")),
            }
        }
        cfg.outputs.push(out);
    }
    Ok(())
}

fn layout(v: &Value, cfg: &mut Config) -> Result<(), String> {
    let l = &mut cfg.layout;
    for (k, v) in table(v, "layout")?.iter() {
        let key = vstr(&k).ok_or("layout keys must be strings")?;
        match key.as_str() {
            "mode" => {
                l.mode = match need_str(&v, &key)?.as_str() {
                    "dwindle" => LayoutMode::Dwindle,
                    "scrolling" => LayoutMode::Scrolling,
                    _ => return Err("mode is \"dwindle\" or \"scrolling\"".to_string()),
                };
            }
            "scrolling" => {
                for (k, v) in table(&v, "scrolling")?.iter() {
                    let key = vstr(&k).ok_or("scrolling keys must be strings")?;
                    match key.as_str() {
                        "preset_widths" => {
                            let ws: Vec<f64> = indexed_entries(&v, &key)?
                                .iter()
                                .filter_map(vnum)
                                .collect();
                            if ws.is_empty() || ws.iter().any(|w| !(0.05..=1.0).contains(w)) {
                                return Err(
                                    "preset_widths is one or more proportions in 0.05..1".into()
                                );
                            }
                            l.scrolling.preset_widths = ws;
                        }
                        "default_width" => {
                            l.scrolling.default_width = ColWidthCfg::Prop(super::f64_in(
                                need_num(&v, &key)?,
                                "default_width",
                                0.05,
                                1.0,
                            )?);
                        }
                        "default_width_px" => {
                            l.scrolling.default_width = ColWidthCfg::FixedPx(super::int_in(
                                need_int(&v, &key)?,
                                "default_width_px",
                                50,
                                100_000,
                            )?
                                as i32);
                        }
                        "center_focus" => {
                            l.scrolling.center_focus = match need_str(&v, &key)?.as_str() {
                                "never" => CenterFocus::Never,
                                "always" => CenterFocus::Always,
                                "on-overflow" => CenterFocus::OnOverflow,
                                _ => {
                                    return Err(
                                        "center_focus is never, always or on-overflow".into()
                                    );
                                }
                            };
                        }
                        other => return Err(format!("unknown scrolling key `{other}`")),
                    }
                }
            }
            "gaps_in" => l.gaps_in = super::int_in(need_int(&v, &key)?, "gaps_in", 0, 500)? as i32,
            "gaps_out" => {
                l.gaps_out = super::int_in(need_int(&v, &key)?, "gaps_out", 0, 500)? as i32;
            }
            "border" => {
                for (k, v) in table(&v, "border")?.iter() {
                    let key = vstr(&k).ok_or("border keys must be strings")?;
                    match key.as_str() {
                        "width" => {
                            l.border.width =
                                super::int_in(need_int(&v, &key)?, "width", 0, 100)? as i32;
                        }
                        "active_color" => l.border.active = super::color(&need_str(&v, &key)?)?,
                        "inactive_color" => {
                            l.border.inactive = super::color(&need_str(&v, &key)?)?;
                        }
                        other => return Err(format!("unknown border key `{other}`")),
                    }
                }
            }
            "float_above_fullscreen" => l.float_above_fullscreen = need_bool(&v, &key)?,
            "workspace_axis" => {
                l.ws_axis = match need_str(&v, &key)?.as_str() {
                    "horizontal" => WsAxis::Horizontal,
                    "vertical" => WsAxis::Vertical,
                    _ => return Err("workspace_axis is horizontal or vertical".into()),
                };
            }
            "focus_ring" | "shadow" | "struts" => {
                return Err(format!("{key}: not implemented yet"));
            }
            other => return Err(format!("unknown layout key `{other}`")),
        }
    }
    Ok(())
}

fn animations(v: &Value, cfg: &mut Config) -> Result<(), String> {
    let a = &mut cfg.animations;
    for (k, v) in table(v, "animations")?.iter() {
        let key = vstr(&k).ok_or("animations keys must be strings")?;
        match key.as_str() {
            "off" => a.off = need_bool(&v, &key)?,
            "slowdown" => {
                a.slowdown = super::f64_in(need_num(&v, &key)?, "slowdown", 0.1, 10.0)?;
            }
            "curves" => {
                for (name, v) in named_entries(&v, "curves")? {
                    let pts = indexed_entries(&v, &name)?;
                    let nums: Vec<f64> =
                        pts.iter().filter_map(|p| vnum(p)).collect();
                    let [x1, y1, x2, y2] = nums.as_slice() else {
                        return Err(format!("curve `{name}` wants {{x1, y1, x2, y2}}"));
                    };
                    if a.curves.iter().any(|(n, _)| *n == name) {
                        return Err(format!("duplicate curve `{name}`"));
                    }
                    a.curves
                        .push((name, crate::anim::CubicBezier::new(*x1, *y1, *x2, *y2)));
                }
            }
            "spring" => a.default_motion = lua_spring(&v)?,
            "ease" => a.default_motion = lua_ease(&v)?,
            "window_open" => anim_kind(&v, &key, super::StyleFamily::Win, &mut a.window_open)?,
            "window_close" => anim_kind(&v, &key, super::StyleFamily::Win, &mut a.window_close)?,
            "window_move" => {
                anim_kind(&v, &key, super::StyleFamily::MotionOnly, &mut a.window_move)?;
            }
            "window_resize" => {
                anim_kind(&v, &key, super::StyleFamily::MotionOnly, &mut a.window_resize)?;
            }
            "workspace_switch" => {
                anim_kind(&v, &key, super::StyleFamily::Ws, &mut a.workspace_switch)?;
            }
            "view_movement" => {
                anim_kind(&v, &key, super::StyleFamily::MotionOnly, &mut a.view_movement)?;
            }
            "layer_open" => anim_kind(&v, &key, super::StyleFamily::Win, &mut a.layer_open)?,
            "layer_close" => anim_kind(&v, &key, super::StyleFamily::Win, &mut a.layer_close)?,
            "border_color" => {
                anim_kind(&v, &key, super::StyleFamily::MotionOnly, &mut a.border_color)?;
            }
            other => return Err(format!("unknown animations key `{other}`")),
        }
    }
    // named curves must exist by the end of the section
    let known: Vec<&String> = a.curves.iter().map(|(n, _)| n).collect();
    let mut motions: Vec<&Option<Motion>> = vec![];
    for kc in [
        &a.window_open,
        &a.window_close,
        &a.window_move,
        &a.window_resize,
        &a.workspace_switch,
        &a.view_movement,
        &a.layer_open,
        &a.layer_close,
        &a.border_color,
    ] {
        motions.push(&kc.motion);
    }
    let default_slot = Some(a.default_motion.clone());
    for m in motions.into_iter().chain([&default_slot]) {
        if let Some(Motion::Ease { curve: CurveRef::Named(n), .. }) = m {
            if !known.contains(&n) {
                return Err(format!("unknown curve `{n}`"));
            }
        }
    }
    Ok(())
}

fn decoration(v: &Value, cfg: &mut Config) -> Result<(), String> {
    let d = &mut cfg.decoration;
    for (k, v) in table(v, "decoration")?.iter() {
        let key = vstr(&k).ok_or("decoration keys must be strings")?;
        match key.as_str() {
            "rounding" => {
                d.rounding =
                    super::int_in(need_int(&v, &key)?, "rounding", 0, 200)? as i32;
            }
            "rounding_power" => {
                d.rounding_power =
                    super::f64_in(need_num(&v, &key)?, "rounding_power", 1.0, 8.0)?;
            }
            "dim_inactive" => {
                d.dim_inactive =
                    super::f64_in(need_num(&v, &key)?, "dim_inactive", 0.0, 1.0)?;
            }
            "shadow" => {
                let mut s = ShadowCfg::default();
                for (k, v) in table(&v, "shadow")?.iter() {
                    let key = vstr(&k).ok_or("shadow keys must be strings")?;
                    match key.as_str() {
                        "size" => {
                            s.size = super::int_in(need_int(&v, &key)?, "size", 1, 200)? as i32;
                        }
                        "color" => s.color = super::color(&need_str(&v, &key)?)?,
                        "offset" => {
                            let xy: Vec<i64> =
                                indexed_entries(&v, &key)?.iter().filter_map(vint).collect();
                            let [x, y] = xy.as_slice() else {
                                return Err("offset is { x, y }".to_string());
                            };
                            if x.abs() > 500 || y.abs() > 500 {
                                return Err("offset is within -500..500".to_string());
                            }
                            s.offset = (*x as i32, *y as i32);
                        }
                        "power" => {
                            s.power = super::f64_in(need_num(&v, &key)?, "power", 0.5, 8.0)?;
                        }
                        other => return Err(format!("unknown shadow key `{other}`")),
                    }
                }
                d.shadow = Some(s);
            }
            "blur" => {
                let mut b = BlurCfg::default();
                for (k, v) in table(&v, "blur")?.iter() {
                    let key = vstr(&k).ok_or("blur keys must be strings")?;
                    match key.as_str() {
                        "passes" => {
                            b.passes =
                                super::int_in(need_int(&v, &key)?, "passes", 1, 4)? as i32;
                        }
                        "size" => b.size = super::f64_in(need_num(&v, &key)?, "size", 0.5, 6.0)?,
                        "noise" => b.noise = super::f64_in(need_num(&v, &key)?, "noise", 0.0, 1.0)?,
                        "contrast" => {
                            b.contrast = super::f64_in(need_num(&v, &key)?, "contrast", 0.0, 2.0)?;
                        }
                        "brightness" => {
                            b.brightness =
                                super::f64_in(need_num(&v, &key)?, "brightness", 0.0, 2.0)?;
                        }
                        "xray" => {
                            if !need_bool(&v, &key)? {
                                return Err("xray false: not implemented yet".to_string());
                            }
                        }
                        other => return Err(format!("unknown blur key `{other}`")),
                    }
                }
                d.blur = Some(b);
            }
            other => return Err(format!("unknown decoration key `{other}`")),
        }
    }
    Ok(())
}

fn lua_spring(v: &Value) -> Result<Motion, String> {
    let (mut d, mut s, mut e) = (None, None, None);
    for (k, v) in table(v, "spring")?.iter() {
        let key = vstr(&k).ok_or("spring keys must be strings")?;
        match key.as_str() {
            "damping_ratio" => d = Some(need_num(&v, &key)?),
            "stiffness" => s = Some(need_num(&v, &key)?),
            "epsilon" => e = Some(need_num(&v, &key)?),
            other => return Err(format!("unknown spring key `{other}`")),
        }
    }
    let (Some(d), Some(s), Some(e)) = (d, s, e) else {
        return Err("spring needs damping_ratio, stiffness, epsilon".to_string());
    };
    super::spring_params(d, s, e)
}

fn lua_ease(v: &Value) -> Result<Motion, String> {
    let (mut ms, mut curve) = (None, None);
    for (k, v) in table(v, "ease")?.iter() {
        let key = vstr(&k).ok_or("ease keys must be strings")?;
        match key.as_str() {
            "duration_ms" => ms = Some(need_int(&v, &key)?),
            "curve" => curve = Some(need_str(&v, &key)?),
            other => return Err(format!("unknown ease key `{other}`")),
        }
    }
    let Some(ms) = ms else {
        return Err("ease needs duration_ms".to_string());
    };
    super::ease_params(ms, curve.as_deref())
}

/// { off = bool, spring = {..} | ease = {..}, style = { "name", perc = n, dir = ".." } }
fn anim_kind(
    v: &Value,
    what: &str,
    family: super::StyleFamily,
    out: &mut KindCfg,
) -> Result<(), String> {
    let mut has_motion = false;
    for (k, v) in table(v, what)?.iter() {
        let key = vstr(&k).ok_or_else(|| format!("{what} keys must be strings"))?;
        match key.as_str() {
            "off" => out.off = need_bool(&v, &key)?,
            "spring" | "ease" => {
                if has_motion {
                    return Err("spring and ease are mutually exclusive".to_string());
                }
                out.motion = Some(if key == "spring" { lua_spring(&v)? } else { lua_ease(&v)? });
                has_motion = true;
            }
            "style" => {
                if family == super::StyleFamily::MotionOnly {
                    return Err(format!("{what} takes no style"));
                }
                out.style = lua_style(&v, family)?;
            }
            other => return Err(format!("unknown animation key `{other}`")),
        }
    }
    Ok(())
}

/// { "name", perc = n, dir = ".." } through the shared style validator
fn lua_style(v: &Value, family: super::StyleFamily) -> Result<super::Style, String> {
    let t = table(v, "style")?;
    let name = t
        .iter()
        .find_map(|(k, v)| if vint(&k) == Some(1) { vstr(&v) } else { None })
        .ok_or("style wants { \"name\", ... }")?;
    let mut perc = None;
    let mut dir = None;
    for (k, v) in t.iter() {
        match vstr(&k).as_deref() {
            Some("perc") => perc = Some(need_num(&v, "perc")?),
            Some("dir") => dir = Some(need_str(&v, "dir")?),
            _ => {}
        }
    }
    super::style_from(family, &name, perc, dir.as_deref())
}

fn cursor(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "cursor")?.iter() {
        let key = vstr(&k).ok_or("cursor keys must be strings")?;
        match key.as_str() {
            "xcursor_theme" => cfg.cursor.xcursor_theme = Some(need_str(&v, &key)?),
            "xcursor_size" => {
                cfg.cursor.xcursor_size =
                    Some(super::int_in(need_int(&v, &key)?, "xcursor_size", 1, 512)? as u32);
            }
            "software" => cfg.cursor.software = need_bool(&v, &key)?,
            other => return Err(format!("unknown cursor key `{other}`")),
        }
    }
    Ok(())
}

/// NAME = "value" sets, NAME = false clears for spawned children
fn environment(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (name, v) in named_entries(v, "environment")? {
        match (vstr(&v), vbool(&v)) {
            (Some(s), _) => cfg.environment.push((name, Some(s))),
            (None, Some(false)) => cfg.environment.push((name, None)),
            _ => return Err(format!("`{name}` wants a string or false")),
        }
    }
    Ok(())
}

/// array entries: a table is argv, a string runs through sh
fn spawns(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for item in indexed_entries(v, "spawn_at_startup")? {
        cfg.spawns.push(spawn_cfg(&item, "spawn_at_startup")?);
    }
    Ok(())
}

fn spawn_cfg(v: &Value, key: &str) -> Result<SpawnCfg, String> {
    match v {
        Value::Table(_) => {
            let argv = str_array(v, key)?;
            if argv.is_empty() {
                return Err(format!("`{key}` spawn needs a command"));
            }
            Ok(SpawnCfg::Argv(argv))
        }
        _ => Ok(SpawnCfg::Sh(need_str(v, key)?)),
    }
}

fn screencast(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "screencast")?.iter() {
        let key = vstr(&k).ok_or("screencast keys must be strings")?;
        match key.as_str() {
            "picker" => cfg.screencast.picker = Some(need_str(&v, &key)?),
            other => return Err(format!("unknown screencast key `{other}`")),
        }
    }
    Ok(())
}

// { chord = "Mod+Return", action = "spawn", args = { "foot" },
//   on = "release", ["repeat"] = true, allow_when_locked = true,
//   cooldown_ms = 200, title = "Terminal" }
fn binds(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (n, item) in indexed_entries(v, "binds")?.into_iter().enumerate() {
        let whine = |e: String| format!("bind {}: {e}", n + 1);
        let mut chord_s = None;
        let mut action_s = None;
        let mut args: Vec<LuaArg> = Vec::new();
        let mut b = Bind {
            mods: 0,
            key: 0,
            action: Action::Quit,
            on_release: false,
            repeat: false,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        };
        for (k, v) in table(&item, "bind")?.iter() {
            let key = vstr(&k).ok_or("bind keys must be strings")?;
            match key.as_str() {
                "chord" => chord_s = Some(need_str(&v, &key).map_err(whine)?),
                "action" => action_s = Some(need_str(&v, &key).map_err(whine)?),
                "args" => args = lua_args(&v).map_err(whine)?,
                "on" => match need_str(&v, &key).map_err(whine)?.as_str() {
                    "press" => b.on_release = false,
                    "release" => b.on_release = true,
                    _ => return Err(whine("on is press or release".to_string())),
                },
                "repeat" => b.repeat = need_bool(&v, &key).map_err(whine)?,
                "allow_when_locked" => {
                    b.allow_when_locked = need_bool(&v, &key).map_err(whine)?;
                }
                "cooldown_ms" => {
                    let cd = need_int(&v, &key).map_err(whine)?;
                    if !(1..=60_000).contains(&cd) {
                        return Err(whine("cooldown_ms is 1..60000".to_string()));
                    }
                    b.cooldown_ms = Some(cd as u32);
                }
                "title" => b.title = Some(need_str(&v, &key).map_err(whine)?),
                other => return Err(whine(format!("unknown bind key `{other}`"))),
            }
        }
        let chord_s = chord_s.ok_or_else(|| whine("needs a chord".to_string()))?;
        let action_s = action_s.ok_or_else(|| whine("needs an action".to_string()))?;
        let (mods, key) = super::chord(&chord_s).map_err(whine)?;
        b.mods = mods;
        b.key = key;
        b.action =
            action_from(&action_s, &args).map_err(|e| whine(format!("({chord_s}): {e}")))?;
        if cfg.binds.iter().any(|x| x.mods == b.mods && x.key == b.key) {
            return Err(whine(format!("chord {chord_s} duplicated")));
        }
        cfg.binds.push(b);
    }
    Ok(())
}

/// action args: strings and numbers, in order
enum LuaArg {
    S(String),
    N(f64),
}

fn lua_args(v: &Value) -> Result<Vec<LuaArg>, String> {
    let mut out: Vec<(i64, LuaArg)> = Vec::new();
    for (k, v) in table(v, "args")?.iter() {
        let i = vint(&k).ok_or("args is an array")?;
        match (vstr(&v), vnum(&v)) {
            (Some(s), _) => out.push((i, LuaArg::S(s))),
            (None, Some(n)) => out.push((i, LuaArg::N(n))),
            _ => return Err("args are strings or numbers".to_string()),
        }
    }
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, a)| a).collect())
}

fn action_from(name: &str, args: &[LuaArg]) -> Result<Action, String> {
    let str_args = || -> Vec<String> {
        args.iter()
            .map(|a| match a {
                LuaArg::S(s) => s.clone(),
                LuaArg::N(n) => n.to_string(),
            })
            .collect()
    };
    let ws = || -> Result<usize, String> {
        match args.first() {
            Some(LuaArg::N(n)) if *n >= 1.0 => Ok(*n as usize - 1),
            _ => Err("workspace actions take a number from 1".to_string()),
        }
    };
    Ok(match name {
        "spawn" => {
            let a = str_args();
            if a.is_empty() {
                return Err("spawn needs a command".to_string());
            }
            Action::Spawn(a)
        }
        "spawn-sh" => match args {
            [LuaArg::S(cmd)] => Action::SpawnSh(cmd.clone()),
            _ => return Err("spawn-sh takes one shell string".to_string()),
        },
        "focus-workspace" => match args.first() {
            Some(LuaArg::S(rel)) if rel.starts_with(['+', '-']) => Action::FocusWorkspaceRel(
                rel.parse::<i32>()
                    .map_err(|_| "relative workspace wants \"+N\" or \"-N\"".to_string())?,
            ),
            _ => Action::FocusWorkspace(ws()?),
        },
        "move-to-workspace" => Action::MoveToWorkspace(ws()?),
        "send-to-workspace" => Action::SendToWorkspace(ws()?),
        "close-window" => Action::CloseWindow,
        "toggle-fullscreen" => Action::ToggleFullscreen,
        "toggle-floating" => Action::ToggleFloating,
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
        "adjust-split-ratio" => match args.first() {
            Some(LuaArg::N(d)) if d.is_finite() && d.abs() <= 1.0 => Action::AdjustSplitRatio(*d),
            Some(LuaArg::S(s)) => match s.parse::<f64>() {
                Ok(d) if d.is_finite() && d.abs() <= 1.0 => Action::AdjustSplitRatio(d),
                _ => return Err("adjust-split-ratio wants a signed delta".to_string()),
            },
            _ => return Err("adjust-split-ratio wants a signed delta".to_string()),
        },
        "consume-or-expel-left" => Action::ConsumeOrExpelLeft,
        "consume-or-expel-right" => Action::ConsumeOrExpelRight,
        "move-column-left" => Action::MoveColumnLeft,
        "move-column-right" => Action::MoveColumnRight,
        "cycle-column-width" => Action::CycleColumnWidth,
        "cycle-column-width-back" => Action::CycleColumnWidthBack,
        "toggle-full-width" => Action::ToggleFullWidth,
        "center-column" => Action::CenterColumn,
        "pointer-move" => Action::PointerMove,
        "pointer-resize" => Action::PointerResize,
        "set-layout" => match args.first() {
            Some(LuaArg::S(s)) => match s.as_str() {
                "dwindle" => Action::SetLayout(SetLayoutArg::Dwindle),
                "scrolling" => Action::SetLayout(SetLayoutArg::Scrolling),
                "toggle" => Action::SetLayout(SetLayoutArg::Toggle),
                _ => return Err("set-layout is dwindle, scrolling or toggle".to_string()),
            },
            _ => return Err("set-layout is dwindle, scrolling or toggle".to_string()),
        },
        "quit" => Action::Quit,
        other => return Err(format!("unknown action \"{other}\"")),
    })
}

fn matcher(v: &Value, what: &str) -> Result<RuleMatch, String> {
    let mut m = RuleMatch::default();
    let mut any = false;
    for (k, v) in table(v, what)?.iter() {
        let key = vstr(&k).ok_or("matcher keys must be strings")?;
        any = true;
        match key.as_str() {
            "app_id" => m.app_id = Some(super::regex(&need_str(&v, &key)?)?),
            "title" => m.title = Some(super::regex(&need_str(&v, &key)?)?),
            "is_xwayland" => m.is_xwayland = Some(need_bool(&v, &key)?),
            "is_floating" => m.is_floating = Some(need_bool(&v, &key)?),
            "is_fullscreen" => m.is_fullscreen = Some(need_bool(&v, &key)?),
            other => return Err(format!("unknown matcher `{other}`")),
        }
    }
    if !any {
        return Err("an empty match matches nothing it should".to_string());
    }
    Ok(m)
}

fn window_rules(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (n, item) in indexed_entries(v, "window_rules")?.into_iter().enumerate() {
        let whine = |e: String| format!("rule {}: {e}", n + 1);
        let mut rule = WindowRule::default();
        for (k, v) in table(&item, "window_rule")?.iter() {
            let key = vstr(&k).ok_or("rule keys must be strings")?;
            match key.as_str() {
                "match" => {
                    for m in indexed_entries(&v, "match").map_err(whine)? {
                        rule.matches.push(matcher(&m, "match").map_err(whine)?);
                    }
                }
                "exclude" => {
                    for m in indexed_entries(&v, "exclude").map_err(whine)? {
                        rule.excludes.push(matcher(&m, "exclude").map_err(whine)?);
                    }
                }
                "open_floating" => {
                    rule.open_floating = Some(need_bool(&v, &key).map_err(whine)?);
                }
                "open_on_workspace" => {
                    let n = need_int(&v, &key).map_err(whine)?;
                    if n < 1 {
                        return Err(whine("open_on_workspace counts from 1".to_string()));
                    }
                    rule.open_on_workspace = Some(n as usize - 1);
                }
                "default_size" => {
                    let dims = indexed_entries(&v, "default_size").map_err(whine)?;
                    let dims: Vec<i64> = dims.iter().filter_map(vint).collect();
                    match dims.as_slice() {
                        [w, h] if *w > 0 && *h > 0 => {
                            rule.default_size = Some((*w as i32, *h as i32));
                        }
                        _ => return Err(whine("default_size is { w, h }".to_string())),
                    }
                }
                "open_centered" => rule.open_centered = need_bool(&v, &key).map_err(whine)?,
                "opacity" => {
                    rule.opacity = Some(
                        super::f64_in(need_num(&v, &key).map_err(whine)?, "opacity", 0.0, 1.0)
                            .map_err(whine)?,
                    );
                }
                "allow_tearing" => {
                    rule.allow_tearing = need_bool(&v, &key).map_err(whine)?;
                }
                "no_anim" => rule.no_anim = need_bool(&v, &key).map_err(whine)?,
                "no_capture" => rule.no_capture = need_bool(&v, &key).map_err(whine)?,
                "rounding" => {
                    rule.rounding = Some(
                        super::int_in(need_int(&v, &key).map_err(whine)?, "rounding", 0, 200)
                            .map_err(whine)? as i32,
                    );
                }
                "shadow" => rule.shadow = Some(need_bool(&v, &key).map_err(whine)?),
                "dim" => rule.dim = Some(need_bool(&v, &key).map_err(whine)?),
                "blur" => rule.blur = Some(need_bool(&v, &key).map_err(whine)?),
                "animation" => {
                    rule.animation =
                        Some(lua_style(&v, super::StyleFamily::Win).map_err(whine)?);
                }
                other => return Err(whine(format!("unknown rule key `{other}`"))),
            }
        }
        if rule.matches.is_empty() {
            return Err(whine("needs a match".to_string()));
        }
        cfg.rules.push(rule);
    }
    Ok(())
}

fn layer_rules(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (n, item) in indexed_entries(v, "layer_rules")?.into_iter().enumerate() {
        let whine = |e: String| format!("layer rule {}: {e}", n + 1);
        let mut rule = LayerRule::default();
        for (k, v) in table(&item, "layer_rule")?.iter() {
            let key = vstr(&k).ok_or("rule keys must be strings")?;
            match key.as_str() {
                "match" => {
                    for m in str_array(&v, "match").map_err(whine)? {
                        rule.matches.push(super::Pattern::new(&m).map_err(whine)?);
                    }
                }
                "blur" => rule.blur = need_bool(&v, &key).map_err(whine)?,
                "ignore_alpha" => {
                    rule.ignore_alpha = Some(
                        super::f64_in(need_num(&v, &key).map_err(whine)?, "ignore_alpha", 0.0, 1.0)
                            .map_err(whine)?,
                    )
                }
                "no_anim" => rule.no_anim = need_bool(&v, &key).map_err(whine)?,
                other => return Err(whine(format!("unknown key `{other}`"))),
            }
        }
        if rule.matches.is_empty() {
            return Err(whine("needs a match".to_string()));
        }
        cfg.layer_rules.push(rule);
    }
    Ok(())
}

// ordered array; profile order is precedence, same as file order in kdl
fn remaps(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (n, item) in indexed_entries(v, "remaps")?.into_iter().enumerate() {
        let whine = |e: String| format!("remap {}: {e}", n + 1);
        let mut p = RemapProfile::default();
        for (k, v) in table(&item, "remap")?.iter() {
            let key = vstr(&k).ok_or("remap keys must be strings")?;
            match key.as_str() {
                "name" => p.name = need_str(&v, &key).map_err(whine)?,
                "match" => {
                    for (k, v) in table(&v, "match").map_err(whine)?.iter() {
                        let key = vstr(&k).ok_or("matcher keys must be strings")?;
                        match key.as_str() {
                            "app_id" => p.app_id = Some(need_str(&v, &key).map_err(whine)?),
                            "title" => p.title = Some(need_str(&v, &key).map_err(whine)?),
                            "is_xwayland" => {
                                p.is_xwayland = Some(need_bool(&v, &key).map_err(whine)?);
                            }
                            "pid" => p.pid = Some(need_int(&v, &key).map_err(whine)? as i32),
                            "workspace" => {
                                p.workspace =
                                    Some(need_int(&v, &key).map_err(whine)?.max(1) as usize);
                            }
                            other => return Err(whine(format!("unknown matcher `{other}`"))),
                        }
                    }
                }
                "maps" => {
                    for pair in indexed_entries(&v, "maps").map_err(whine)? {
                        let pair = str_array(&pair, "maps").map_err(whine)?;
                        let [from, to] = pair.as_slice() else {
                            return Err(whine("map entries are { \"From\", \"To\" }".to_string()));
                        };
                        let f = super::keycode(&from.to_lowercase())
                            .ok_or_else(|| whine(format!("unknown key \"{from}\"")))?;
                        let t = super::keycode(&to.to_lowercase())
                            .ok_or_else(|| whine(format!("unknown key \"{to}\"")))?;
                        p.maps.push((f, t));
                    }
                }
                other => return Err(whine(format!("unknown key `{other}`"))),
            }
        }
        if p.maps.is_empty() {
            return Err(whine("has no map entries".to_string()));
        }
        cfg.remaps.push(p);
    }
    Ok(())
}

fn debug(v: &Value, cfg: &mut Config) -> Result<(), String> {
    for (k, v) in table(v, "debug")?.iter() {
        let key = vstr(&k).ok_or("debug keys must be strings")?;
        match key.as_str() {
            "render_drm_device" => cfg.debug.render_drm_device = Some(need_str(&v, &key)?),
            "ignore_drm_devices" => {
                cfg.debug.ignore_drm_devices = str_array(&v, &key)?;
            }
            "latency_policy" => {
                cfg.debug.latency_policy = match need_str(&v, &key)?.as_str() {
                    "late-latch" => crate::config::LatencyPolicy::LateLatch,
                    "vblank" => crate::config::LatencyPolicy::Vblank,
                    other => return Err(format!("unknown latency_policy `{other}`")),
                }
            }
            "latch_margin_us" => {
                cfg.debug.latch_margin_us = Some(need_int(&v, &key)?.max(0) as u32);
            }
            other => return Err(format!("unknown debug key `{other}`")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_and_kdl_parse_to_the_same_config() {
        let kdl = crate::config::kdl::parse(
            r##"
            input {
                keyboard { repeat-rate 35; repeat-delay 250 }
                mouse { accel-profile "flat"; natural-scroll }
                device "turtle-beach" { accel-speed -0.86 }
            }
            layout {
                gaps-in 4
                gaps-out 8
                workspace-axis "vertical"
                border { width 2; active-color "#89b4fa"; inactive-color "#585b70" }
            }
            animations {
                slowdown 2.0
                curve "shoot" 0.05 0.9 0.1 1.05
                spring damping-ratio=1.0 stiffness=600 epsilon=0.001
                window-open { ease duration-ms=200 curve="shoot"; style "slide" dir="top" }
                workspace-switch { style "slidefadevert" perc=30 }
                window-move { off }
            }
            output "DP-3" { mode "2560x1440@480"; variable-refresh-rate }
            screencast { picker "fuzzel-pick" }
            binds {
                Mod+Return { spawn "foot"; }
                Mod+1 { focus-workspace 1; }
                XF86AudioMute allow-when-locked=#true { spawn-sh "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"; }
            }
            window-rule {
                match app-id=#"^steam_app_"# title=#"Isaac"#
                open-floating #true
                allow-tearing #true
            }
            remap "isaac" {
                match app-id="steam_app_250900"
                map "Alt_R" "Left"
            }
            "##,
        )
        .unwrap();
        let lua = parse(
            r##"
            carrot = {
                input = {
                    keyboard = { repeat_rate = 35, repeat_delay = 250 },
                    mouse = { accel_profile = "flat", natural_scroll = true },
                    devices = { ["turtle-beach"] = { accel_speed = -0.86 } },
                },
                layout = {
                    gaps_in = 4,
                    gaps_out = 8,
                    workspace_axis = "vertical",
                    border = { width = 2, active_color = "#89b4fa", inactive_color = "#585b70" },
                },
                animations = {
                    slowdown = 2.0,
                    curves = { shoot = { 0.05, 0.9, 0.1, 1.05 } },
                    spring = { damping_ratio = 1.0, stiffness = 600, epsilon = 0.001 },
                    window_open = {
                        ease = { duration_ms = 200, curve = "shoot" },
                        style = { "slide", dir = "top" },
                    },
                    workspace_switch = { style = { "slidefadevert", perc = 30 } },
                    window_move = { off = true },
                },
                outputs = { ["DP-3"] = { mode = "2560x1440@480", vrr = "always" } },
                screencast = { picker = "fuzzel-pick" },
                binds = {
                    { chord = "Mod+Return", action = "spawn", args = { "foot" } },
                    { chord = "Mod+1", action = "focus-workspace", args = { 1 } },
                    { chord = "XF86AudioMute", action = "spawn-sh",
                      args = { "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle" },
                      allow_when_locked = true },
                },
                window_rules = {
                    { match = { { app_id = "^steam_app_", title = "Isaac" } },
                      open_floating = true, allow_tearing = true },
                },
                remaps = {
                    { name = "isaac", match = { app_id = "steam_app_250900" },
                      maps = { { "Alt_R", "Left" } } },
                },
            }
            "##,
        )
        .unwrap();
        assert_eq!(kdl, lua);
    }

    #[test]
    fn lua_errors_are_loud_and_accumulate() {
        let errs = parse("carrot = { nonsense = 1 }").unwrap_err();
        assert!(errs.iter().any(|e| e.contains("unknown section")), "{errs:?}");
        let errs = parse(
            "carrot = { layout = { gaps_in = \"soup\" }, cursor = { sizes = 1 } }",
        )
        .unwrap_err();
        assert_eq!(errs.len(), 2, "both sections report: {errs:?}");
    }
}
