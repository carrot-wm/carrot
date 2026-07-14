// animations { off, slowdown, curve defs, per-kind spring/ease/style }

use super::{children, Cx};
use crate::config::*;

use crate::config::StyleFamily as Family;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let a = &mut cfg.animations;
    for c in children(node) {
        match c.name().value() {
            "off" => {
                if let Some(b) = cx.flag(c) {
                    a.off = b;
                }
            }
            "slowdown" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "slowdown", 0.1, 10.0) {
                        Ok(v) => a.slowdown = v,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "curve" => curve_def(c, a, cx),
            "spring" | "ease" => {
                if let Some(m) = motion(c, cx) {
                    a.default_motion = m;
                }
            }
            "window-open" => kind(c, cx, Family::Win, |a| &mut a.window_open, a),
            "window-close" => kind(c, cx, Family::Win, |a| &mut a.window_close, a),
            "window-move" => kind(c, cx, Family::MotionOnly, |a| &mut a.window_move, a),
            "window-resize" => kind(c, cx, Family::MotionOnly, |a| &mut a.window_resize, a),
            "workspace-switch" => kind(c, cx, Family::Ws, |a| &mut a.workspace_switch, a),
            "view-movement" => kind(c, cx, Family::MotionOnly, |a| &mut a.view_movement, a),
            "layer-open" => kind(c, cx, Family::Win, |a| &mut a.layer_open, a),
            "layer-close" => kind(c, cx, Family::Win, |a| &mut a.layer_close, a),
            "border-color" => kind(c, cx, Family::MotionOnly, |a| &mut a.border_color, a),
            other => cx.at(c, &format!("unknown animations key \"{other}\"")),
        }
    }
    // named curves must exist by the end of the section
    let known: Vec<String> = a.curves.iter().map(|(n, _)| n.clone()).collect();
    let check = |m: &Option<Motion>, cx: &mut Cx| {
        if let Some(Motion::Ease { curve: CurveRef::Named(n), .. }) = m {
            if !known.contains(n) {
                cx.at(node, &format!("unknown curve \"{n}\""));
            }
        }
    };
    if let Motion::Ease { curve: CurveRef::Named(n), .. } = &a.default_motion {
        if !known.contains(n) {
            cx.at(node, &format!("unknown curve \"{n}\""));
        }
    }
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
        check(&kc.motion, cx);
    }
}

pub(super) fn prop_f64(node: &KdlNode, name: &str) -> Option<f64> {
    node.get(name)
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
}

pub(super) fn prop_str<'a>(node: &'a KdlNode, name: &str) -> Option<&'a str> {
    node.get(name).and_then(|v| v.as_string())
}

/// `spring damping-ratio=.. stiffness=.. epsilon=..` or
/// `ease duration-ms=.. curve=".."` - properties, not arguments
fn motion(node: &KdlNode, cx: &mut Cx) -> Option<Motion> {
    let r = match node.name().value() {
        "spring" => {
            let (Some(d), Some(s), Some(e)) = (
                prop_f64(node, "damping-ratio"),
                prop_f64(node, "stiffness"),
                prop_f64(node, "epsilon"),
            ) else {
                cx.at(node, "spring needs damping-ratio= stiffness= epsilon=");
                return None;
            };
            spring_params(d, s, e)
        }
        "ease" => {
            let Some(ms) = prop_f64(node, "duration-ms") else {
                cx.at(node, "ease needs duration-ms=");
                return None;
            };
            ease_params(ms as i64, prop_str(node, "curve"))
        }
        _ => unreachable!(),
    };
    match r {
        Ok(m) => Some(m),
        Err(e) => {
            cx.leaf(node, e);
            None
        }
    }
}

/// `curve "name" x1 y1 x2 y2` - a named cubic bezier
fn curve_def(node: &KdlNode, a: &mut AnimsCfg, cx: &mut Cx) {
    let args: Vec<_> = node.entries().iter().filter(|e| e.name().is_none()).collect();
    let name = args.first().and_then(|e| e.value().as_string());
    let nums: Vec<f64> = args
        .iter()
        .skip(1)
        .filter_map(|e| e.value().as_float().or_else(|| e.value().as_integer().map(|i| i as f64)))
        .collect();
    let (Some(name), [x1, y1, x2, y2]) = (name, nums.as_slice()) else {
        cx.at(node, "curve is: curve \"name\" x1 y1 x2 y2");
        return;
    };
    if a.curves.iter().any(|(n, _)| n == name) {
        cx.at(node, &format!("duplicate curve \"{name}\""));
        return;
    }
    a.curves
        .push((name.to_string(), crate::anim::CubicBezier::new(*x1, *y1, *x2, *y2)));
}

fn kind(
    node: &KdlNode,
    cx: &mut Cx,
    family: Family,
    get: impl Fn(&mut AnimsCfg) -> &mut KindCfg,
    a: &mut AnimsCfg,
) {
    let mut out = get(a).clone();
    let mut has_motion = false;
    for c in children(node) {
        match c.name().value() {
            "off" => {
                if let Some(b) = cx.flag(c) {
                    out.off = b;
                }
            }
            "spring" | "ease" => {
                if has_motion {
                    cx.at(c, "spring and ease are mutually exclusive");
                    continue;
                }
                if let Some(m) = motion(c, cx) {
                    out.motion = Some(m);
                    has_motion = true;
                }
            }
            "style" => {
                if family == Family::MotionOnly {
                    cx.at(c, &format!("{} takes no style", node.name().value()));
                    continue;
                }
                if let Some(s) = style(c, cx, family) {
                    out.style = s;
                }
            }
            other => cx.at(c, &format!("unknown animation key \"{other}\"")),
        }
    }
    *get(a) = out;
}

fn style(node: &KdlNode, cx: &mut Cx, family: Family) -> Option<Style> {
    let name = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())?;
    match style_from(family, name, prop_f64(node, "perc"), prop_str(node, "dir")) {
        Ok(s) => Some(s),
        Err(e) => {
            cx.leaf(node, e);
            None
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
        match crate::config::kdl::parse_bare(src) {
            Ok(_) => vec![],
            Err(e) => e,
        }
    }

    #[test]
    fn anims_defaults_when_absent() {
        let c = parse_ok("layout { mode \"dwindle\" }");
        assert!(!c.animations.off);
        assert_eq!(c.animations.slowdown, 1.0);
        assert!(matches!(c.animations.window_open.style, Style::Popin { .. }));
    }

    #[test]
    fn anims_both_flavors_parse() {
        let c = parse_ok(
            "animations {\n\
             slowdown 2.0\n\
             curve \"shoot\" 0.05 0.9 0.1 1.05\n\
             spring damping-ratio=1.0 stiffness=600 epsilon=0.001\n\
             window-open { ease duration-ms=200 curve=\"shoot\"; style \"slide\" dir=\"top\" }\n\
             workspace-switch { style \"slidefadevert\" perc=30 }\n\
             window-move { off }\n\
             }",
        );
        assert_eq!(c.animations.slowdown, 2.0);
        assert_eq!(c.animations.curves.len(), 1);
        assert!(
            matches!(c.animations.default_motion, Motion::Spring { stiffness, .. } if stiffness == 600.0)
        );
        assert!(matches!(
            c.animations.window_open.motion,
            Some(Motion::Ease { ms: 200, curve: CurveRef::Named(ref n) }) if n == "shoot"
        ));
        assert!(matches!(c.animations.window_open.style, Style::Slide { dir: Some(_) }));
        assert!(matches!(
            c.animations.workspace_switch.style,
            Style::SlideFadeVert { perc } if (perc - 0.3).abs() < 1e-9
        ));
        assert!(c.animations.window_move.off);
        // inheritance: window-move has no own motion, and off wins
        assert!(c.animations.window_move.motion.is_none());
        assert!(c.animations.motion(AnimKind::WindowMove).is_none());
        assert!(c.animations.motion(AnimKind::ViewMovement).is_some());
    }

    #[test]
    fn window_rule_animation_overrides() {
        let c = parse_ok(
            "window-rule {\n match app-id=\"foo\"\n no-anim\n animation \"fade\"\n }",
        );
        assert!(c.rules[0].no_anim);
        assert_eq!(c.rules[0].animation, Some(Style::Fade));
        let fx = rule_effects(&c, "foo", "", false, false);
        assert!(fx.no_anim);
        assert_eq!(fx.animation, Some(Style::Fade));
        // workspace styles never fit a window rule
        let errs =
            parse_errs("window-rule {\n match app-id=\"x\"\n animation \"slidefadevert\"\n }");
        assert!(errs.iter().any(|e| e.contains("style")), "{errs:?}");
    }

    #[test]
    fn anims_rejects_bad_input() {
        for (src, needle) in [
            (
                "animations { window-open { spring damping-ratio=1.0 stiffness=800 epsilon=0.001; ease duration-ms=100 curve=\"linear\" } }",
                "exclusive",
            ),
            ("animations { window-open { ease duration-ms=100 curve=\"nope\" } }", "curve"),
            ("animations { window-open { style \"slidefadevert\" } }", "style"),
            ("animations { window-move { style \"popin\" } }", "style"),
            ("animations { spring damping-ratio=99 stiffness=800 epsilon=0.001 }", "damping"),
            ("animations { bogus 1 }", "unknown"),
        ] {
            let errs = parse_errs(src);
            assert!(
                errs.iter().any(|e| e.to_lowercase().contains(needle)),
                "{src} should error mentioning {needle}, got {errs:?}"
            );
        }
    }
}
