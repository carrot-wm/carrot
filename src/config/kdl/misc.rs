// the small sections: cursor, environment, spawns, screencast,
// switch-events, debug, prefer-no-csd

use super::{children, Cx};
use crate::config::*;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    match node.name().value() {
        "cursor" => cursor(node, cfg, cx),
        "environment" => environment(node, cfg, cx),
        "spawn-at-startup" => {
            let argv: Vec<String> = node
                .entries()
                .iter()
                .filter(|e| e.name().is_none())
                .filter_map(|e| e.value().as_string().map(str::to_string))
                .collect();
            if argv.is_empty() {
                cx.at(node, "spawn-at-startup needs a command");
                return;
            }
            cx.no_children(node);
            cfg.spawns.push(SpawnCfg::Argv(argv));
        }
        "spawn-sh-at-startup" => {
            if let Some(cmd) = cx.str_(node) {
                cx.no_children(node);
                cfg.spawns.push(SpawnCfg::Sh(cmd));
            }
        }
        "prefer-no-csd" => {
            if let Some(b) = cx.flag(node) {
                cfg.prefer_no_csd = b;
            }
        }
        "screencast" => {
            for c in children(node) {
                match c.name().value() {
                    "picker" => cfg.screencast.picker = cx.str_(c),
                    other => cx.at(c, &format!("unknown screencast key \"{other}\"")),
                }
            }
        }
        "debug" => debug(node, cfg, cx),
        _ => unreachable!("dispatched by name"),
    }
}

fn cursor(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    for c in children(node) {
        match c.name().value() {
            "xcursor-theme" => cfg.cursor.xcursor_theme = cx.str_(c),
            "xcursor-size" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "xcursor-size", 1, 512) {
                        Ok(v) => cfg.cursor.xcursor_size = Some(v as u32),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "software" => {
                if let Some(b) = cx.flag(c) {
                    cfg.cursor.software = b;
                }
            }
            other => cx.at(c, &format!("unknown cursor key \"{other}\"")),
        }
    }
}

fn environment(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    for c in children(node) {
        let name = c.name().value().to_string();
        let entry = c.entries().iter().find(|e| e.name().is_none());
        match entry.map(|e| e.value()) {
            Some(v) if v.is_null() => cfg.environment.push((name, None)),
            Some(v) => match v.as_string() {
                Some(s) => cfg.environment.push((name, Some(s.to_string()))),
                None => cx.at(c, "environment values are strings or #null"),
            },
            None => cx.at(c, "environment values are strings or #null"),
        }
    }
}

fn debug(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    for c in children(node) {
        match c.name().value() {
            "render-drm-device" => cfg.debug.render_drm_device = cx.str_(c),
            "ignore-drm-device" => {
                if let Some(s) = cx.str_(c) {
                    cfg.debug.ignore_drm_devices.push(s);
                }
            }
            "latency-policy" => match cx.str_(c).as_deref() {
                Some("late-latch") => cfg.debug.latency_policy = LatencyPolicy::LateLatch,
                Some("vblank") => cfg.debug.latency_policy = LatencyPolicy::Vblank,
                Some(other) => cx.at(c, &format!("unknown latency-policy \"{other}\"")),
                None => {}
            },
            "latch-margin-us" => {
                if let Some(v) = cx.int(c) {
                    cfg.debug.latch_margin_us = Some(v.max(0) as u32);
                }
            }
            other => cx.at(c, &format!("unknown debug key \"{other}\"")),
        }
    }
}
