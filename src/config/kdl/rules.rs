// window-rule { match/exclude ... effects }, layer-rule, remap.
// matchers are regexes in raw strings; AND within a node, OR across
// nodes, excludes veto

use super::{children, Cx};
use crate::config::*;

fn matcher(node: &KdlNode, cx: &mut Cx) -> Option<RuleMatch> {
    let mut m = RuleMatch::default();
    let mut any = false;
    for e in node.entries() {
        let Some(name) = e.name() else {
            cx.at(node, "match takes properties, like app-id=");
            return None;
        };
        any = true;
        match name.value() {
            "app-id" => match e.value().as_string().map(regex) {
                Some(Ok(p)) => m.app_id = Some(p),
                Some(Err(err)) => cx.leaf(node, err),
                None => cx.at(node, "app-id is a regex string"),
            },
            "title" => match e.value().as_string().map(regex) {
                Some(Ok(p)) => m.title = Some(p),
                Some(Err(err)) => cx.leaf(node, err),
                None => cx.at(node, "title is a regex string"),
            },
            "is-xwayland" => match e.value().as_bool() {
                Some(b) => m.is_xwayland = Some(b),
                None => cx.at(node, "is-xwayland is #true or #false"),
            },
            "is-floating" => match e.value().as_bool() {
                Some(b) => m.is_floating = Some(b),
                None => cx.at(node, "is-floating is #true or #false"),
            },
            "is-fullscreen" => match e.value().as_bool() {
                Some(b) => m.is_fullscreen = Some(b),
                None => cx.at(node, "is-fullscreen is #true or #false"),
            },
            other => cx.at(node, &format!("unknown matcher \"{other}\"")),
        }
    }
    if !any {
        cx.at(node, "an empty match matches nothing it should");
        return None;
    }
    Some(m)
}

pub(super) fn window_rule(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let mut rule = WindowRule::default();
    for c in children(node) {
        match c.name().value() {
            "match" => {
                if let Some(m) = matcher(c, cx) {
                    rule.matches.push(m);
                }
            }
            "exclude" => {
                if let Some(m) = matcher(c, cx) {
                    rule.excludes.push(m);
                }
            }
            "open-floating" => {
                if let Some(b) = cx.flag(c) {
                    rule.open_floating = Some(b);
                }
            }
            "open-on-workspace" => {
                match cx.int(c) {
                    Some(n) if n >= 1 => rule.open_on_workspace = Some(n as usize - 1),
                    Some(_) => cx.at(c, "open-on-workspace counts from 1"),
                    None => {}
                }
            }
            "default-size" => {
                let mut it = c.entries().iter().filter_map(|e| e.value().as_integer());
                match (it.next(), it.next()) {
                    (Some(w), Some(h)) if w > 0 && h > 0 => {
                        rule.default_size = Some((w as i32, h as i32));
                    }
                    _ => cx.at(c, "default-size needs a width and a height"),
                }
            }
            "open-centered" => {
                if let Some(b) = cx.flag(c) {
                    rule.open_centered = b;
                }
            }
            "opacity" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "opacity", 0.0, 1.0) {
                        Ok(v) => rule.opacity = Some(v),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "allow-tearing" => {
                if let Some(b) = cx.flag(c) {
                    rule.allow_tearing = b;
                }
            }
            other => cx.at(c, &format!("unknown window-rule key \"{other}\"")),
        }
    }
    if rule.matches.is_empty() {
        cx.at(node, "window-rule needs a match child");
        return;
    }
    cfg.rules.push(rule);
}

pub(super) fn layer_rule(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let mut rule = LayerRule::default();
    for c in children(node) {
        match c.name().value() {
            "match" => match c.get("namespace").and_then(|v| v.as_string()).map(regex) {
                Some(Ok(p)) => rule.matches.push(p),
                Some(Err(err)) => cx.leaf(c, err),
                None => cx.at(c, "match wants namespace=, a regex string"),
            },
            other => cx.at(c, &format!("unknown layer-rule key \"{other}\"")),
        }
    }
    if rule.matches.is_empty() {
        cx.at(node, "layer-rule needs a match child");
        return;
    }
    cfg.layer_rules.push(rule);
}

// remap "name" { match app-id="x" title="y" is-xwayland=#true pid=123 workspace=2
//                map "From" "To" }
pub(super) fn remap(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let mut p = RemapProfile {
        name: node
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_string())
            .unwrap_or_default()
            .to_string(),
        ..Default::default()
    };
    for c in children(node) {
        match c.name().value() {
            "match" => {
                for e in c.entries() {
                    let Some(name) = e.name() else {
                        cx.at(c, "match takes properties, like app-id=");
                        continue;
                    };
                    match name.value() {
                        "app-id" => p.app_id = e.value().as_string().map(str::to_string),
                        "title" => p.title = e.value().as_string().map(str::to_string),
                        "is-xwayland" => p.is_xwayland = e.value().as_bool(),
                        "pid" => p.pid = e.value().as_integer().map(|v| v as i32),
                        "workspace" => {
                            p.workspace = e.value().as_integer().map(|v| v.max(1) as usize)
                        }
                        other => cx.at(c, &format!("unknown matcher \"{other}\"")),
                    }
                }
            }
            "map" => {
                let args: Vec<String> = c
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .filter_map(|e| e.value().as_string().map(str::to_string))
                    .collect();
                let [from, to] = args.as_slice() else {
                    cx.at(c, "map wants two key names");
                    continue;
                };
                let Some(f) = keycode(&from.to_lowercase()) else {
                    cx.at(c, &format!("unknown key \"{from}\""));
                    continue;
                };
                let Some(t) = keycode(&to.to_lowercase()) else {
                    cx.at(c, &format!("unknown key \"{to}\""));
                    continue;
                };
                p.maps.push((f, t));
            }
            other => cx.at(c, &format!("remap: unknown key \"{other}\"")),
        }
    }
    if p.maps.is_empty() {
        cx.at(node, "remap has no map entries");
        return;
    }
    cfg.remaps.push(p);
}
