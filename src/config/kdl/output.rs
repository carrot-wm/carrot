// output "DP-1" { mode, scale, position, variable-refresh-rate, off }
// the name matches a connector or a "make model serial" string

use super::{children, Cx};
use crate::config::*;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let Some(name) = cx.str_(node) else { return };
    let mut out = OutputCfg {
        name,
        vrr: Vrr::Off,
        scale: None,
        mode: None,
        position: None,
        off: false,
        allow_tearing: false,
    };
    for c in children(node) {
        match c.name().value() {
            "mode" => {
                if let Some(m) = cx.str_(c) {
                    match parse_mode(&m) {
                        Some(m) => out.mode = Some(m),
                        None => cx.at(c, "mode looks like \"2560x1440@240\""),
                    }
                }
            }
            "scale" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "scale", 0.25, 4.0) {
                        Ok(v) => out.scale = Some(v),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "position" => {
                let x = c.get("x").and_then(|v| v.as_integer());
                let y = c.get("y").and_then(|v| v.as_integer());
                match (x, y) {
                    (Some(x), Some(y)) => out.position = Some((x as i32, y as i32)),
                    _ => cx.at(c, "position wants x= and y="),
                }
            }
            "variable-refresh-rate" => {
                let on_demand = c.get("on-demand").and_then(|v| v.as_bool());
                out.vrr = match on_demand {
                    Some(true) => Vrr::OnDemand,
                    Some(false) | None => Vrr::Always,
                };
            }
            "off" => {
                if let Some(b) = cx.flag(c) {
                    out.off = b;
                }
            }
            "allow-tearing" => {
                if let Some(b) = cx.flag(c) {
                    out.allow_tearing = b;
                }
            }
            other => cx.at(c, &format!("unknown output key \"{other}\"")),
        }
    }
    cfg.outputs.push(out);
}
