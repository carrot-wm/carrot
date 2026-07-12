// binds { Mod+Return { spawn "foot"; } }
// the chord is the node name; exactly one child names the action; bind
// kinds ride as properties on the chord node

use super::{children, Cx};
use crate::config::*;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let mut spans: Vec<((u32, u32), String)> = Vec::new();
    for c in children(node) {
        bind(c, cfg, cx, &mut spans);
    }
}

fn bind(node: &KdlNode, cfg: &mut Config, cx: &mut Cx, spans: &mut Vec<((u32, u32), String)>) {
    let spec = node.name().value();
    let (mods, key) = match chord(spec) {
        Ok(v) => v,
        Err(e) => {
            cx.leaf(node, e);
            return;
        }
    };
    let mut b = Bind {
        mods,
        key,
        action: Action::Quit, // replaced below; a bind without an action errors
        on_release: false,
        repeat: false,
        allow_when_locked: false,
        cooldown_ms: None,
        title: None,
    };
    for e in node.entries() {
        let Some(name) = e.name() else {
            cx.at(node, "chord arguments ride the action child, not the chord");
            return;
        };
        match name.value() {
            "on" => match e.value().as_string() {
                Some("press") => b.on_release = false,
                Some("release") => b.on_release = true,
                _ => cx.at(node, "on is \"press\" or \"release\""),
            },
            "repeat" => match e.value().as_bool() {
                Some(v) => b.repeat = v,
                None => cx.at(node, "repeat is #true or #false"),
            },
            "allow-when-locked" => match e.value().as_bool() {
                Some(v) => b.allow_when_locked = v,
                None => cx.at(node, "allow-when-locked is #true or #false"),
            },
            "cooldown-ms" => match e.value().as_integer() {
                Some(v) if (1..=60_000).contains(&v) => b.cooldown_ms = Some(v as u32),
                _ => cx.at(node, "cooldown-ms is 1..60000"),
            },
            "title" => match e.value().as_string() {
                Some(t) => b.title = Some(t.to_string()),
                None => cx.at(node, "title is a string"),
            },
            other => cx.at(node, &format!("unknown bind property \"{other}\"")),
        }
    }
    let kids = children(node);
    let [action_node] = kids else {
        cx.at(node, "a bind holds exactly one action");
        return;
    };
    let Some(action) = action(action_node, cx) else { return };
    b.action = action;
    // both spans, so the fix is one glance
    if let Some((_, first)) = spans.iter().find(|(k, _)| *k == (b.mods, b.key)) {
        let first = first.clone();
        cx.errs.push(format!("{first}: chord {spec} first defined here"));
        cx.at(node, &format!("chord {spec} duplicated here"));
        return;
    }
    spans.push(((b.mods, b.key), cx.pos(node)));
    cfg.binds.push(b);
}

fn action(node: &KdlNode, cx: &mut Cx) -> Option<Action> {
    let name = node.name().value();
    let strs: Vec<String> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .map(|e| {
            e.value()
                .as_string()
                .map(str::to_string)
                .unwrap_or_else(|| e.value().to_string())
        })
        .collect();
    let ws = |cx: &mut Cx| -> Option<usize> {
        match node
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_integer())
        {
            Some(n) if n >= 1 => Some(n as usize - 1),
            _ => {
                cx.at(node, "workspace actions take a number from 1");
                None
            }
        }
    };
    let none = |cx: &mut Cx, name: &str| {
        if !strs.is_empty() {
            cx.at(node, &format!("{name} takes no arguments"));
        }
    };
    Some(match name {
        "spawn" => {
            if strs.is_empty() {
                cx.at(node, "spawn needs a command");
                return None;
            }
            Action::Spawn(strs)
        }
        "spawn-sh" => {
            let [cmd] = strs.as_slice() else {
                cx.at(node, "spawn-sh takes one shell string");
                return None;
            };
            Action::SpawnSh(cmd.clone())
        }
        "focus-workspace" => {
            // "+1"/"-1" strings jump relative
            if let Some(rel) = strs.first().filter(|s| s.starts_with(['+', '-'])) {
                match rel.parse::<i32>() {
                    Ok(n) => Action::FocusWorkspaceRel(n),
                    Err(_) => {
                        cx.at(node, "relative workspace wants \"+N\" or \"-N\"");
                        return None;
                    }
                }
            } else {
                Action::FocusWorkspace(ws(cx)?)
            }
        }
        "move-to-workspace" => {
            let idx = ws(cx)?;
            match node.get("focus").and_then(|v| v.as_bool()) {
                Some(false) => Action::SendToWorkspace(idx),
                _ => Action::MoveToWorkspace(idx),
            }
        }
        "send-to-workspace" => Action::SendToWorkspace(ws(cx)?),
        "close-window" => {
            none(cx, name);
            Action::CloseWindow
        }
        "toggle-fullscreen" => {
            none(cx, name);
            Action::ToggleFullscreen
        }
        "toggle-floating" => {
            none(cx, name);
            Action::ToggleFloating
        }
        "focus-next" => {
            none(cx, name);
            Action::FocusNext
        }
        "focus-prev" => {
            none(cx, name);
            Action::FocusPrev
        }
        "focus-left" => Action::FocusDir(Dir::Left),
        "focus-right" => Action::FocusDir(Dir::Right),
        "focus-up" => Action::FocusDir(Dir::Up),
        "focus-down" => Action::FocusDir(Dir::Down),
        "swap-left" => Action::SwapDir(Dir::Left),
        "swap-right" => Action::SwapDir(Dir::Right),
        "swap-up" => Action::SwapDir(Dir::Up),
        "swap-down" => Action::SwapDir(Dir::Down),
        "adjust-split-ratio" => {
            let delta = strs.first().and_then(|s| s.parse::<f64>().ok()).or_else(|| {
                node.entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_float())
            });
            match delta {
                Some(d) if d.is_finite() && d.abs() <= 1.0 => Action::AdjustSplitRatio(d),
                _ => {
                    cx.at(node, "adjust-split-ratio wants a signed delta like \"+0.1\"");
                    return None;
                }
            }
        }
        "consume-or-expel-left" => {
            none(cx, name);
            Action::ConsumeOrExpelLeft
        }
        "consume-or-expel-right" => {
            none(cx, name);
            Action::ConsumeOrExpelRight
        }
        "cycle-column-width" => {
            none(cx, name);
            Action::CycleColumnWidth
        }
        "cycle-column-width-back" => {
            none(cx, name);
            Action::CycleColumnWidthBack
        }
        "toggle-full-width" => {
            none(cx, name);
            Action::ToggleFullWidth
        }
        "center-column" => {
            none(cx, name);
            Action::CenterColumn
        }
        "set-layout" => match strs.first().map(String::as_str) {
            Some("dwindle") => Action::SetLayout(SetLayoutArg::Dwindle),
            Some("scrolling") => Action::SetLayout(SetLayoutArg::Scrolling),
            Some("toggle") => Action::SetLayout(SetLayoutArg::Toggle),
            _ => {
                cx.at(node, "set-layout is dwindle, scrolling or toggle");
                return None;
            }
        },
        "quit" => {
            none(cx, name);
            Action::Quit
        }
        other => {
            cx.at(node, &format!("unknown action \"{other}\""));
            return None;
        }
    })
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
    fn scrolling_actions_parse() {
        let c = parse_ok(
            "binds {\n\
             Mod+BracketLeft { consume-or-expel-left; }\n\
             Mod+R { cycle-column-width; }\n\
             Mod+Shift+R { cycle-column-width-back; }\n\
             Mod+W { toggle-full-width; }\n\
             Mod+C { center-column; }\n\
             Mod+T { set-layout \"toggle\"; }\n\
             }",
        );
        let acts: Vec<_> = c.binds.iter().map(|b| b.action.clone()).collect();
        assert!(acts.contains(&Action::ConsumeOrExpelLeft));
        assert!(acts.contains(&Action::CycleColumnWidth));
        assert!(acts.contains(&Action::CycleColumnWidthBack));
        assert!(acts.contains(&Action::ToggleFullWidth));
        assert!(acts.contains(&Action::CenterColumn));
        assert!(acts.contains(&Action::SetLayout(SetLayoutArg::Toggle)));
        let errs = parse_errs("binds { Mod+T { set-layout \"spiral\"; } }");
        assert!(errs.iter().any(|e| e.contains("set-layout")), "{errs:?}");
    }
}
