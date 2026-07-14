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
                        "scrolling" => l.mode = LayoutMode::Scrolling,
                        _ => cx.at(c, "mode is \"dwindle\" or \"scrolling\""),
                    }
                }
            }
            "scrolling" => scrolling(c, &mut l.scrolling, cx),
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
            "workspace-axis" => {
                if let Some(s) = cx.str_(c) {
                    match s.as_str() {
                        "horizontal" => l.ws_axis = WsAxis::Horizontal,
                        "vertical" => l.ws_axis = WsAxis::Vertical,
                        _ => cx.at(c, "workspace-axis is horizontal or vertical"),
                    }
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

fn scrolling(node: &KdlNode, out: &mut ScrollCfg, cx: &mut Cx) {
    for c in children(node) {
        match c.name().value() {
            "preset-widths" => {
                let ws: Vec<f64> = c
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .filter_map(|e| {
                        e.value().as_float().or_else(|| e.value().as_integer().map(|i| i as f64))
                    })
                    .collect();
                if ws.is_empty() || ws.iter().any(|w| !(0.05..=1.0).contains(w)) {
                    cx.at(c, "preset-widths is one or more proportions in 0.05..1");
                } else {
                    out.preset_widths = ws;
                }
            }
            "default-width" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "default-width", 0.05, 1.0) {
                        Ok(v) => out.default_width = ColWidthCfg::Prop(v),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "default-width-px" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "default-width-px", 50, 100_000) {
                        Ok(v) => out.default_width = ColWidthCfg::FixedPx(v as i32),
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "center-focus" => {
                if let Some(s) = cx.str_(c) {
                    out.center_focus = match s.as_str() {
                        "never" => CenterFocus::Never,
                        "always" => CenterFocus::Always,
                        "on-overflow" => CenterFocus::OnOverflow,
                        _ => {
                            cx.at(c, "center-focus is never, always or on-overflow");
                            continue;
                        }
                    };
                }
            }
            other => cx.at(c, &format!("unknown scrolling key \"{other}\"")),
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

#[cfg(test)]
mod tests {
    use crate::config::*;

    fn parse_ok(src: &str) -> Config {
        match crate::config::kdl::parse_bare(src) {
            Ok(c) => c,
            Err(e) => panic!("expected clean parse: {e:?}"),
        }
    }

    fn parse_errs(src: &str) -> Vec<String> {
        crate::config::kdl::parse_bare(src).err().unwrap_or_default()
    }

    #[test]
    fn scrolling_mode_and_block_parse() {
        let c = parse_ok(
            "layout {\n mode \"scrolling\"\n scrolling {\n preset-widths 0.25 0.5 0.75\n default-width 0.4\n center-focus \"on-overflow\"\n }\n }",
        );
        assert_eq!(c.layout.mode, LayoutMode::Scrolling);
        assert_eq!(c.layout.scrolling.preset_widths, vec![0.25, 0.5, 0.75]);
        assert_eq!(c.layout.scrolling.default_width, ColWidthCfg::Prop(0.4));
        assert_eq!(c.layout.scrolling.center_focus, CenterFocus::OnOverflow);
        let c = parse_ok("layout { scrolling { default-width-px 600 } }");
        assert_eq!(c.layout.scrolling.default_width, ColWidthCfg::FixedPx(600));
    }

    #[test]
    fn workspace_axis_parses() {
        assert_eq!(parse_ok("layout { }").layout.ws_axis, WsAxis::Horizontal);
        let c = parse_ok("layout { workspace-axis \"vertical\" }");
        assert_eq!(c.layout.ws_axis, WsAxis::Vertical);
        let errs = parse_errs("layout { workspace-axis \"diagonal\" }");
        assert!(errs.iter().any(|e| e.contains("workspace-axis")), "{errs:?}");
    }

    #[test]
    fn scrolling_block_rejects_bad_input() {
        for (src, needle) in [
            ("layout { mode \"spiral\" }", "mode"),
            ("layout { scrolling { center-focus \"sometimes\" } }", "center-focus"),
            ("layout { scrolling { preset-widths } }", "preset-widths"),
            ("layout { scrolling { default-width 40 } }", "default-width"),
            ("layout { scrolling { bogus 1 } }", "unknown"),
        ] {
            let errs = parse_errs(src);
            assert!(errs.iter().any(|e| e.contains(needle)), "{src}: {errs:?}");
        }
    }
}
