// the kdl v2 parser: entry, shared node readers, multi-error context.
// section walkers live in the child modules; every leaf value goes
// through the validators in the parent module so lua parses identically.

use super::*;

mod binds;
mod input;
mod layout;
mod misc;
mod output;
mod rules;

fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let upto = &src[..offset.min(src.len())];
    let line = upto.matches('\n').count() + 1;
    let col = upto.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) + 1;
    (line, col)
}

/// parse a user file: the embedded default underneath, the file's
/// sections over it. Ok only when nothing at all went wrong; Err carries
/// every rendered error
pub fn parse(src: &str) -> Result<Config, Vec<String>> {
    let mut cfg = default::embedded().clone();
    parse_into(&mut cfg, src, true)
}

/// the bootstrap path: the embedded text lands on the empty config (the
/// default cannot be built on top of itself)
pub(super) fn parse_bare(src: &str) -> Result<Config, Vec<String>> {
    let mut cfg = super::empty();
    parse_into(&mut cfg, src, false)
}

fn parse_into(cfg: &mut Config, src: &str, reset_lists: bool) -> Result<Config, Vec<String>> {
    let mut errs = Errors::default();
    let doc: KdlDocument = match src.parse::<KdlDocument>() {
        Ok(d) => d,
        Err(e) => {
            for d in &e.diagnostics {
                let (l, c) = line_col(src, d.span.offset());
                let msg = d.message.clone().unwrap_or_else(|| "parse error".into());
                errs.push(format!("{l}:{c}: {msg}"));
            }
            return Err(errs.list);
        }
    };
    // a user file that speaks a repeated section at all replaces the
    // default's entries wholesale; nothing embedded leaks underneath
    if reset_lists {
        for node in doc.nodes() {
            match node.name().value() {
                "binds" => cfg.binds.clear(),
                "output" => cfg.outputs.clear(),
                "window-rule" => cfg.rules.clear(),
                "layer-rule" => cfg.layer_rules.clear(),
                "remap" => cfg.remaps.clear(),
                "spawn-at-startup" | "spawn-sh-at-startup" => cfg.spawns.clear(),
                "environment" => cfg.environment.clear(),
                _ => {}
            }
        }
    }
    let mut cx = Cx { src, errs: &mut errs };
    let mut seen: Vec<&str> = Vec::new();
    for node in doc.nodes() {
        let name = node.name().value();
        // singleton sections appear once
        let singleton = matches!(
            name,
            "input" | "layout" | "cursor" | "screencast" | "binds" | "environment" | "debug"
        );
        if singleton {
            if seen.contains(&name) {
                cx.at(node, &format!("duplicate {name} section"));
                continue;
            }
            seen.push(name);
        }
        match name {
            "input" => input::parse(node, cfg, &mut cx),
            "output" => output::parse(node, cfg, &mut cx),
            "layout" => layout::parse(node, cfg, &mut cx),
            "binds" => binds::parse(node, cfg, &mut cx),
            "window-rule" => rules::window_rule(node, cfg, &mut cx),
            "layer-rule" => rules::layer_rule(node, cfg, &mut cx),
            "remap" => rules::remap(node, cfg, &mut cx),
            "cursor" | "environment" | "spawn-at-startup" | "spawn-sh-at-startup"
            | "prefer-no-csd" | "screencast" | "debug" => {
                misc::parse(node, cfg, &mut cx)
            }
            // reserved: the feature behind the name does not exist yet
            "workspace" | "animations" | "switch-events" => {
                cx.at(node, &format!("{name}: not implemented yet"));
            }
            other => cx.at(node, &format!("unknown key \"{other}\"")),
        }
    }
    resolve_mod(&mut cfg.binds, cfg.input.mod_key);
    if errs.is_empty() {
        Ok(cfg.clone())
    } else {
        Err(errs.list)
    }
}

/// per-parse context: the source for spans, the error accumulator
pub(crate) struct Cx<'a> {
    pub src: &'a str,
    pub errs: &'a mut Errors,
}

impl Cx<'_> {
    pub fn at(&mut self, node: &KdlNode, msg: &str) {
        let (l, c) = line_col(self.src, node.span().offset());
        self.errs.push(format!("{l}:{c}: {msg}"));
    }

    /// just the rendered position, for diagnostics that pair two spans
    pub fn pos(&self, node: &KdlNode) -> String {
        let (l, c) = line_col(self.src, node.span().offset());
        format!("{l}:{c}")
    }

    /// a leaf validator failed: same span rendering, upstream message
    pub fn leaf(&mut self, node: &KdlNode, e: String) {
        self.at(node, &e);
    }

    pub fn str_(&mut self, node: &KdlNode) -> Option<String> {
        let v = node
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_string())
            .map(str::to_string);
        if v.is_none() {
            self.at(node, &format!("{} needs a string", node.name().value()));
        }
        v
    }

    pub fn int(&mut self, node: &KdlNode) -> Option<i64> {
        let v = node
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_integer())
            .map(|v| v as i64);
        if v.is_none() {
            self.at(node, &format!("{} needs an integer", node.name().value()));
        }
        v
    }

    pub fn float(&mut self, node: &KdlNode) -> Option<f64> {
        let v = node.entries().iter().find(|e| e.name().is_none()).and_then(|e| {
            e.value()
                .as_float()
                .or_else(|| e.value().as_integer().map(|v| v as f64))
        });
        if v.is_none() {
            self.at(node, &format!("{} needs a number", node.name().value()));
        }
        v
    }

    /// bare node = true, `#false` argument = false; anything else is loud
    pub fn flag(&mut self, node: &KdlNode) -> Option<bool> {
        let args: Vec<_> = node.entries().iter().filter(|e| e.name().is_none()).collect();
        match args.as_slice() {
            [] => Some(true),
            [one] => match one.value().as_bool() {
                Some(b) => Some(b),
                None => {
                    self.at(node, &format!("{} is a bare flag or #true/#false", node.name().value()));
                    None
                }
            },
            _ => {
                self.at(node, &format!("{} is a bare flag or #true/#false", node.name().value()));
                None
            }
        }
    }

    pub fn no_children(&mut self, node: &KdlNode) {
        if node.children().is_some() {
            self.at(node, &format!("{} takes no block", node.name().value()));
        }
    }
}

pub(crate) fn children<'a>(node: &'a KdlNode) -> &'a [KdlNode] {
    node.children().map(|c| c.nodes()).unwrap_or(&[])
}
