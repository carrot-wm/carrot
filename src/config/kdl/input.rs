// input { keyboard { xkb {} } touchpad {} mouse {} device "" {} mod-key }

use super::{children, Cx};
use crate::config::*;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    for c in children(node) {
        match c.name().value() {
            "keyboard" => keyboard(c, cfg, cx),
            "touchpad" => pointer_class(c, &mut cfg.input.touchpad, cx, true),
            "mouse" => pointer_class(c, &mut cfg.input.mouse, cx, false),
            "device" => device(c, cfg, cx),
            "mod-key" => {
                if let Some(s) = cx.str_(c) {
                    match s.as_str() {
                        "super" => cfg.input.mod_key = ModKey::Super,
                        "alt" => cfg.input.mod_key = ModKey::Alt,
                        _ => cx.at(c, "mod-key is \"super\" or \"alt\""),
                    }
                }
            }
            other => cx.at(c, &format!("unknown input key \"{other}\"")),
        }
    }
}

fn keyboard(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let kb = &mut cfg.input.keyboard;
    for c in children(node) {
        match c.name().value() {
            "xkb" => {
                for x in children(c) {
                    match x.name().value() {
                        "layout" => kb.xkb.layout = cx.str_(x),
                        "variant" => kb.xkb.variant = cx.str_(x),
                        "options" => kb.xkb.options = cx.str_(x),
                        other => cx.at(x, &format!("unknown xkb key \"{other}\"")),
                    }
                }
            }
            "repeat-rate" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "repeat-rate", 1, 200) {
                        Ok(v) => kb.repeat_rate = v as i32,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "repeat-delay" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "repeat-delay", 1, 5000) {
                        Ok(v) => kb.repeat_delay = v as i32,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "numlock" => {
                if let Some(b) = cx.flag(c) {
                    kb.numlock = b;
                }
            }
            other => cx.at(c, &format!("unknown keyboard key \"{other}\"")),
        }
    }
}

fn pointer_class(node: &KdlNode, out: &mut PointerClassCfg, cx: &mut Cx, touchpad: bool) {
    for c in children(node) {
        match c.name().value() {
            "accel-profile" => {
                if let Some(s) = cx.str_(c) {
                    match accel_profile(&s) {
                        Ok(p) => out.accel_profile = Some(p),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "accel-speed" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "accel-speed", -1.0, 1.0) {
                        Ok(v) => out.accel_speed = Some(v),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "natural-scroll" => {
                if let Some(b) = cx.flag(c) {
                    out.natural_scroll = b;
                }
            }
            // reserved until input stage S2 exists
            "tap" | "dwt" if touchpad => {
                cx.at(c, &format!("{}: not implemented yet", c.name().value()));
            }
            other => cx.at(c, &format!("unknown {} key \"{other}\"", node.name().value())),
        }
    }
}

fn device(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let Some(name) = cx.str_(node) else { return };
    let mut rule = DeviceRule {
        name,
        accel_speed: None,
        accel_profile: None,
        natural_scroll: None,
        dpi: None,
    };
    for c in children(node) {
        match c.name().value() {
            "accel-speed" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "accel-speed", -1.0, 1.0) {
                        Ok(v) => rule.accel_speed = Some(v),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "accel-profile" => {
                if let Some(s) = cx.str_(c) {
                    match accel_profile(&s) {
                        Ok(p) => rule.accel_profile = Some(p),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "natural-scroll" => {
                if let Some(b) = cx.flag(c) {
                    rule.natural_scroll = Some(b);
                }
            }
            "dpi" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "dpi", 100.0, 40000.0) {
                        Ok(v) => rule.dpi = Some(v),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            other => cx.at(c, &format!("unknown device key \"{other}\"")),
        }
    }
    cfg.input.devices.push(rule);
}
