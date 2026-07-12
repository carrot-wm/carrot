// xcursor theme loading. honors XCURSOR_THEME/SIZE/PATH, follows
// index.theme Inherits chains (depth-capped; themes ship cycles).
// binary format parsed by hand, closest-size image wins.

use std::path::PathBuf;

pub struct CursorImage {
    /// ARGB8888, premultiplied
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot: (i32, i32),
}

pub fn load(name: &str, cfg: &crate::config::CursorCfg) -> Option<CursorImage> {
    // the config wins; the env keeps working for everything it already did
    let theme = cfg
        .xcursor_theme
        .clone()
        .or_else(|| std::env::var("XCURSOR_THEME").ok());
    let size: u32 = cfg
        .xcursor_size
        .or_else(|| std::env::var("XCURSOR_SIZE").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(24);
    let paths = search_paths();
    let file = theme
        .as_deref()
        .and_then(|t| try_theme(&paths, t, name, 0))
        .or_else(|| try_theme(&paths, "default", name, 0))?;
    crate::trace!("cursor theme file: {}", file.display());
    parse_xcursor(&std::fs::read(&file).ok()?, size)
}

fn search_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").ok();
    if let Ok(xcursor_path) = std::env::var("XCURSOR_PATH") {
        return xcursor_path
            .split(':')
            .map(|p| match (p.strip_prefix('~'), &home) {
                (Some(rest), Some(h)) => PathBuf::from(format!("{h}{rest}")),
                _ => PathBuf::from(p),
            })
            .collect();
    }
    let mut paths = Vec::new();
    if let Some(h) = &home {
        paths.push(PathBuf::from(h).join(".icons"));
        paths.push(PathBuf::from(h).join(".local/share/icons"));
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
        for dir in xdg.split(':') {
            paths.push(PathBuf::from(dir).join("icons"));
        }
    }
    if let Ok(user) = std::env::var("USER") {
        paths.push(PathBuf::from(format!(
            "/etc/profiles/per-user/{user}/share/icons"
        )));
    }
    paths.push(PathBuf::from("/run/current-system/sw/share/icons"));
    paths.push(PathBuf::from("/usr/share/icons"));
    paths
}

fn try_theme(paths: &[PathBuf], theme: &str, name: &str, depth: u8) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    for dir in paths {
        let cursor = dir.join(theme).join("cursors").join(name);
        if cursor.exists() {
            return Some(cursor);
        }
    }
    for dir in paths {
        if let Some(parents) = parse_inherits(&dir.join(theme).join("index.theme")) {
            for parent in parents {
                if parent != theme {
                    if let Some(p) = try_theme(paths, &parent, name, depth + 1) {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

fn parse_inherits(path: &PathBuf) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Inherits") {
            if let Some(rest) = rest.trim_start().strip_prefix('=') {
                let parents: Vec<String> = rest
                    .split([',', ';'])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !parents.is_empty() {
                    return Some(parents);
                }
            }
        }
    }
    None
}

fn parse_xcursor(data: &[u8], target_size: u32) -> Option<CursorImage> {
    if data.len() < 16 {
        return None;
    }
    let u32_at = |off: usize| u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
    // "Xcur"
    if u32_at(0) != 0x7275_6358 {
        return None;
    }
    let ntoc = u32_at(12) as usize;
    if data.len() < 16 + ntoc * 12 {
        return None;
    }
    let mut best: Option<(u32, usize)> = None;
    for i in 0..ntoc {
        let toc = 16 + i * 12;
        // image chunks only
        if u32_at(toc) != 0xFFFD_0002 {
            continue;
        }
        let diff = u32_at(toc + 4).abs_diff(target_size);
        let pos = u32_at(toc + 8) as usize;
        if best.map(|(d, _)| diff < d).unwrap_or(true) {
            best = Some((diff, pos));
        }
    }
    let pos = best?.1;
    if pos + 36 > data.len() {
        return None;
    }
    let width = u32_at(pos + 16);
    let height = u32_at(pos + 20);
    let hotspot = (u32_at(pos + 24) as i32, u32_at(pos + 28) as i32);
    let start = pos + 36;
    let bytes = (width as usize) * (height as usize) * 4;
    if start + bytes > data.len() {
        return None;
    }
    Some(CursorImage {
        pixels: data[start..start + bytes].to_vec(),
        width,
        height,
        hotspot,
    })
}
