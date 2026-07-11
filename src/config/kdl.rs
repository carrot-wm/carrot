// the kdl v2 parser. shared types and string helpers live in the parent
// module; everything KdlNode-shaped is here.

use super::*;

fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let upto = &src[..offset.min(src.len())];
    let line = upto.matches('\n').count() + 1;
    let col = upto.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) + 1;
    (line, col)
}

pub fn parse(src: &str) -> Result<Config, String> {
    let doc: KdlDocument = src.parse().map_err(|e: ::kdl::KdlError| {
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
                    dpi: None,
                };
                for c in node.children().map(|c| c.nodes()).unwrap_or(&[]) {
                    match c.name().value() {
                        "accel-speed" | "sensitivity" => rule.accel_speed = first_float(c),
                        "accel-profile" => {
                            rule.accel_profile = Some(accel_profile(need_str(c, src)?, c, src)?)
                        }
                        "natural-scroll" => rule.natural_scroll = Some(need_bool(c, src)?),
                        "dpi" => rule.dpi = first_float(c),
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
                // float/tile/size/center/workspace/immediate/opacity apply
                // at map; the effect keys still wait on shaders
            }
            "layer-rule" => {
                cfg.layer_rules.push(parse_layer_rule(node, src)?);
                unapplied.push("layer-rule");
            }
            "output" => {
                let name = first_str(node)
                    .ok_or_else(|| at(node, src, "output needs a name string"))?;
                let mut out = OutputCfg { name, vrr: None, gpu: None, scale: None, mode: None };
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
                        "mode" => {
                            let m = need_str(c, src)?;
                            out.mode = Some(parse_mode(&m).ok_or_else(|| {
                                at(c, src, "mode looks like \"2560x1440@240\"")
                            })?);
                        }
                        "gpu" => {
                            out.gpu = Some(need_str(c, src)?);
                            unapplied.push("output-gpu");
                        }
                        "scale" => {
                            out.scale = first_float(c);
                            unapplied.push("output-scale");
                        }
                        other => {
                            return Err(at(c, src, &format!("unknown output key \"{other}\"")))
                        }
                    }
                }
                cfg.outputs.push(out);
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
            "remap" => {
                cfg.remaps.push(parse_remap(node, src)?);
            }
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
    for pat in [&rule.match_class, &rule.match_title] {
        if let Some(p) = pat {
            regex_lite::Regex::new(p)
                .map_err(|e| at(node, src, &format!("bad regex \"{p}\": {e}")))?;
        }
    }
    Ok(rule)
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
            "allow-tearing" => cfg.allow_tearing = need_bool(c, src)?,
            "picker" => cfg.picker = Some(need_str(c, src)?),
            "software-cursor" => cfg.software_cursor = need_bool(c, src)?,
            other => return Err(at(c, src, &format!("unknown general key \"{other}\""))),
        }
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
                cfg.input.accel_profile = Some(accel_profile(need_str(c, src)?, c, src)?);
            }
            "natural-scroll" => cfg.input.natural_scroll = need_bool(c, src)?,
            "tap" => {
                cfg.input.tap = need_bool(c, src)?;
                unapplied.push("tap");
            }
            "dwt" => {
                cfg.input.dwt = need_bool(c, src)?;
                unapplied.push("dwt");
            }
            "layout" => cfg.input.layout = Some(need_str(c, src)?),
            "numlock" => cfg.input.numlock = need_bool(c, src)?,
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

fn accel_profile(p: String, node: &KdlNode, src: &str) -> Result<String, String> {
    match p.as_str() {
        "flat" | "adaptive" => Ok(p),
        _ => Err(at(node, src, "accel-profile is flat or adaptive")),
    }
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

// remap "name" { class "x"; title "y"; type "x11"; pid 123; workspace 2;
// map "From" "To"; ... } - criteria AND, keys translate before delivery
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

fn dir_arg(args: &[String], node: &KdlNode, src: &str) -> Result<Dir, String> {
    match args.get(3).map(String::as_str) {
        Some("left" | "l") => Ok(Dir::Left),
        Some("right" | "r") => Ok(Dir::Right),
        Some("up" | "u") => Ok(Dir::Up),
        Some("down" | "d") => Ok(Dir::Down),
        _ => Err(at(node, src, "direction is left, right, up or down")),
    }
}

// bind "Meta+Shift" "q" "close"  /  bind "Meta" "1" "workspace" "1"
// type="press|release|repeat|lock-safe|mouse" picks when the bind fires
// known-but-unimplemented actions warn and skip, they don't fail the file
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

