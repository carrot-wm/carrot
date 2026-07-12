// layout { mode, gaps, border {} } - the visible opinions

use super::{children, Cx};
use crate::config::*;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let l = &mut cfg.layout;
    for c in children(node) {
        match c.name().value() {
            "mode" => {
                if let Some(s) = cx.str_(c) {
                    match s.as_str() {
                        "dwindle" => l.mode = LayoutMode::Dwindle,
                        _ => cx.at(c, "mode is \"dwindle\""),
                    }
                }
            }
            "gaps-in" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "gaps-in", 0, 500) {
                        Ok(v) => l.gaps_in = v as i32,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "gaps-out" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "gaps-out", 0, 500) {
                        Ok(v) => l.gaps_out = v as i32,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "border" => border(c, &mut l.border, cx),
            "float-above-fullscreen" => {
                if let Some(b) = cx.flag(c) {
                    l.float_above_fullscreen = b;
                }
            }
            // reserved until the renderer draws them
            "focus-ring" | "shadow" | "struts" => {
                cx.at(c, &format!("{}: not implemented yet", c.name().value()));
            }
            other => cx.at(c, &format!("unknown layout key \"{other}\"")),
        }
    }
}

fn border(node: &KdlNode, out: &mut BorderCfg, cx: &mut Cx) {
    for c in children(node) {
        match c.name().value() {
            "width" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "width", 0, 100) {
                        Ok(v) => out.width = v as i32,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "active-color" => {
                if let Some(s) = cx.str_(c) {
                    match color(&s) {
                        Ok(v) => out.active = v,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "inactive-color" => {
                if let Some(s) = cx.str_(c) {
                    match color(&s) {
                        Ok(v) => out.inactive = v,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            other => cx.at(c, &format!("unknown border key \"{other}\"")),
        }
    }
}
