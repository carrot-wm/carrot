// the kdl v2 parser: entry, shared node readers, multi-error context.
// section walkers live in the child modules; every leaf value goes
// through the validators in the parent module so lua parses identically.

use super::*;

mod anims;
mod binds;
mod deco;
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
/// every rendered error. include nodes need `parse_at`, which is why
/// the live paths go there and this stays a test entry
#[cfg(test)]
pub fn parse(src: &str) -> Result<Config, Vec<String>> {
    let mut cfg = default::embedded().clone();
    parse_into(&mut cfg, src, true, None)
}

/// same, anchored at the file's real path so `include "other.kdl"` nodes
/// resolve relative to it
pub fn parse_at(src: &str, path: &std::path::Path) -> Result<Config, Vec<String>> {
    let mut cfg = default::embedded().clone();
    parse_into(&mut cfg, src, true, path.parent())
}

/// the bootstrap path: the embedded text lands on the empty config (the
/// default cannot be built on top of itself)
pub(super) fn parse_bare(src: &str) -> Result<Config, Vec<String>> {
    let mut cfg = super::empty();
    parse_into(&mut cfg, src, false, None)
}

/// nested includes stop here; a config nine files deep is a cycle bug,
/// not a use case
const MAX_INCLUDE_DEPTH: usize = 8;

/// resolve one include argument against the including file's directory
fn include_target(node: &KdlNode, dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let arg = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())?;
    let p = std::path::Path::new(arg);
    Some(if p.is_absolute() { p.to_path_buf() } else { dir.join(p) })
}

/// which sections any file in the include tree speaks; drives the
/// wholesale list reset before anything applies
fn scan_names(
    doc: &KdlDocument,
    dir: Option<&std::path::Path>,
    depth: usize,
    f: &mut impl FnMut(&str),
) {
    for node in doc.nodes() {
        let name = node.name().value();
        if name == "include" {
            if depth >= MAX_INCLUDE_DEPTH {
                continue;
            }
            let Some(dir) = dir else { continue };
            let Some(target) = include_target(node, dir) else { continue };
            // unreadable or broken files are the walk's problem to report
            if let Ok(text) = std::fs::read_to_string(&target) {
                if let Ok(sub) = text.parse::<KdlDocument>() {
                    scan_names(&sub, target.parent(), depth + 1, f);
                }
            }
            continue;
        }
        f(name);
    }
}

fn parse_into(
    cfg: &mut Config,
    src: &str,
    reset_lists: bool,
    dir: Option<&std::path::Path>,
) -> Result<Config, Vec<String>> {
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
    // default's entries wholesale, wherever in the include tree it lives;
    // nothing embedded leaks underneath
    if reset_lists {
        scan_names(&doc, dir, 0, &mut |name| match name {
            "binds" => cfg.binds.clear(),
            "output" => cfg.outputs.clear(),
            "window-rule" => cfg.rules.clear(),
            "layer-rule" => cfg.layer_rules.clear(),
            "remap" => cfg.remaps.clear(),
            "spawn-at-startup" | "spawn-sh-at-startup" => cfg.spawns.clear(),
            "environment" => cfg.environment.clear(),
            _ => {}
        });
    }
    let mut cx = Cx { src, errs: &mut errs, label: None };
    let mut seen: Vec<String> = Vec::new();
    let mut visited: Vec<std::path::PathBuf> = Vec::new();
    walk(&doc, cfg, &mut cx, &mut seen, dir, 0, &mut visited);
    resolve_mod(&mut cfg.binds, cfg.input.mod_key);
    if errs.is_empty() {
        Ok(cfg.clone())
    } else {
        Err(errs.list)
    }
}

fn walk(
    doc: &KdlDocument,
    cfg: &mut Config,
    mut cx: &mut Cx,
    seen: &mut Vec<String>,
    dir: Option<&std::path::Path>,
    depth: usize,
    visited: &mut Vec<std::path::PathBuf>,
) {
    for node in doc.nodes() {
        let name = node.name().value();
        if name == "include" {
            let Some(dir) = dir else {
                cx.at(node, "include needs a config file on disk");
                continue;
            };
            if depth >= MAX_INCLUDE_DEPTH {
                cx.at(node, "include nesting too deep");
                continue;
            }
            let Some(target) = include_target(node, dir) else {
                cx.at(node, "include needs a path string");
                continue;
            };
            let canon = target.canonicalize().unwrap_or_else(|_| target.clone());
            if visited.contains(&canon) {
                cx.at(node, &format!("include cycle through {}", target.display()));
                continue;
            }
            let text = match std::fs::read_to_string(&target) {
                Ok(t) => t,
                Err(e) => {
                    cx.at(node, &format!("include {}: {e}", target.display()));
                    continue;
                }
            };
            let label = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.display().to_string());
            let sub: KdlDocument = match text.parse() {
                Ok(d) => d,
                Err(e) => {
                    for d in &e.diagnostics {
                        let (l, c) = line_col(&text, d.span.offset());
                        let msg = d.message.clone().unwrap_or_else(|| "parse error".into());
                        cx.errs.push(format!("{label} {l}:{c}: {msg}"));
                    }
                    continue;
                }
            };
            visited.push(canon);
            let mut sub_cx = Cx { src: &text, errs: cx.errs, label: Some(label) };
            walk(&sub, cfg, &mut sub_cx, seen, target.parent(), depth + 1, visited);
            visited.pop();
            continue;
        }
        // singleton sections appear once, across the whole include tree
        let singleton = matches!(
            name,
            "input" | "layout" | "cursor" | "screencast" | "binds" | "environment" | "debug"
                | "animations" | "decoration"
        );
        if singleton {
            if seen.iter().any(|s| s == name) {
                cx.at(node, &format!("duplicate {name} section"));
                continue;
            }
            seen.push(name.to_string());
        }
        match name {
            "animations" => anims::parse(node, cfg, &mut cx),
            "decoration" => deco::parse(node, cfg, &mut cx),
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
            "workspace" | "switch-events" => {
                cx.at(node, &format!("{name}: not implemented yet"));
            }
            other => cx.at(node, &format!("unknown key \"{other}\"")),
        }
    }
}

/// per-parse context: the source for spans, the error accumulator, and
/// the file name when the source came in through an include
pub(crate) struct Cx<'a> {
    pub src: &'a str,
    pub errs: &'a mut Errors,
    pub label: Option<String>,
}

impl Cx<'_> {
    pub fn at(&mut self, node: &KdlNode, msg: &str) {
        let (l, c) = line_col(self.src, node.span().offset());
        match &self.label {
            Some(f) => self.errs.push(format!("{f} {l}:{c}: {msg}")),
            None => self.errs.push(format!("{l}:{c}: {msg}")),
        }
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
