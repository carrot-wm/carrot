// burrow - talk to the compositor over its json socket.
//
//   burrow workspace 3          burrow windows
//   burrow spawn "foot"         burrow workspaces
//   burrow toggle-fullscreen    burrow reload
//   burrow close-window         burrow subscribe
//
// one request per line in, one json reply per line out. replies render as
// expanded key: value blocks; --json keeps the wire form, and subscribe
// always streams raw ndjson for scripts.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn socket_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let dir = std::path::PathBuf::from(dir);
    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        let p = dir.join(format!("carrot.{display}.sock"));
        if p.exists() {
            return Some(p);
        }
    }
    // no display in the env: take whatever carrot socket is around
    let mut found = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("carrot.") && name.ends_with(".sock") {
            found = Some(e.path());
        }
    }
    found
}

fn usage() -> ! {
    eprintln!(
        "usage: burrow [--json] <command>\n\
         actions:  workspace N|+-N | send-to-workspace N | toggle-fullscreen |\n\
                   toggle-floating | close-window | focus-next | focus-prev |\n\
                   focus-left|right|up|down | swap-left|right|up|down |\n\
                   split-ratio +-D | spawn CMD.. | quit\n\
         queries:  monitors | workspaces | windows | clients\n\
         control:  reload | subscribe"
    );
    std::process::exit(2)
}

// -- expanded output --

fn plain(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn block(cmd: &str, it: &serde_json::Value) {
    let mut skip: Vec<&str> = Vec::new();
    if cmd == "workspaces" {
        let active = if it["active"].as_bool().unwrap_or(false) { " (active)" } else { "" };
        println!("Workspace {}{active}:", plain(&it["index"]));
        skip.extend(["index", "active"]);
    } else {
        match it.get("id") {
            Some(id) => println!(
                "Window {} ({}): {}",
                plain(id),
                plain(&it["app-id"]),
                plain(&it["title"])
            ),
            None => println!("Window ({}): {}", plain(&it["app-id"]), plain(&it["title"])),
        }
        skip.extend(["id", "app-id", "title"]);
    }
    // geometry reads as a pair, not four scalars
    if let (Some(x), Some(y), Some(w), Some(h)) =
        (it.get("x"), it.get("y"), it.get("w"), it.get("h"))
    {
        println!("    at: {},{}", plain(x), plain(y));
        println!("    size: {},{}", plain(w), plain(h));
        skip.extend(["x", "y", "w", "h"]);
    }
    if let Some(map) = it.as_object() {
        for (k, v) in map {
            if !skip.contains(&k.as_str()) {
                println!("    {k}: {}", plain(v));
            }
        }
    }
}

fn render(cmd: &str, line: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        println!("{line}");
        return;
    };
    if let Some(e) = v.get("error") {
        eprintln!("burrow: {}", plain(e));
        std::process::exit(1);
    }
    let Some(ok) = v.get("ok") else {
        println!("{line}");
        return;
    };
    match ok {
        serde_json::Value::Array(items) => {
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                block(cmd, it);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                println!("{k}: {}", plain(v));
            }
        }
        serde_json::Value::Bool(true) => println!("ok"),
        other => println!("{}", plain(other)),
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let raw = args.first().is_some_and(|a| a == "--json" || a == "-j");
    if raw {
        args.remove(0);
    }
    if args.is_empty() {
        usage();
    }
    let n_arg = |i: usize| -> u64 {
        let n: u64 = args.get(i).and_then(|a| a.parse().ok()).unwrap_or_else(|| usage());
        // 1-based on the cli, 0-based inside
        n.saturating_sub(1)
    };
    let request = match args[0].as_str() {
        // a signed argument means a relative jump
        "workspace" if args.get(1).is_some_and(|a| a.starts_with(['+', '-'])) => {
            let d: i64 = args[1].parse().unwrap_or_else(|_| usage());
            format!("{{\"workspace-rel\":{d}}}")
        }
        "workspace" => format!("{{\"workspace\":{}}}", n_arg(1)),
        "workspace-rel" => {
            let d: i64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or_else(|| usage());
            format!("{{\"workspace-rel\":{d}}}")
        }
        "send-to-workspace" => format!("{{\"send-to-workspace\":{}}}", n_arg(1)),
        "move-to-workspace" => format!("{{\"move-to-workspace\":{}}}", n_arg(1)),
        "spawn" => {
            if args.len() < 2 {
                usage();
            }
            serde_json::json!({ "spawn": args[1..].join(" ") }).to_string()
        }
        "split-ratio" => {
            let d: f64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or_else(|| usage());
            serde_json::json!({ "split-ratio": d }).to_string()
        }
        cmd @ ("focus-left" | "focus-right" | "focus-up" | "focus-down") => {
            format!("{{\"focus-dir\":\"{}\"}}", &cmd["focus-".len()..])
        }
        cmd @ ("swap-left" | "swap-right" | "swap-up" | "swap-down") => {
            format!("{{\"swap-dir\":\"{}\"}}", &cmd["swap-".len()..])
        }
        cmd @ ("toggle-fullscreen" | "toggle-floating" | "close-window" | "focus-next"
        | "focus-prev" | "quit" | "monitors" | "workspaces" | "windows" | "clients" | "reload"
        | "subscribe" | "dump-shadow" | "dpms-on" | "dpms-off") => {
            serde_json::json!(cmd).to_string()
        }
        _ => usage(),
    };
    let Some(path) = socket_path() else {
        eprintln!("burrow: no carrot socket found (is carrot running?)");
        std::process::exit(1);
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("burrow: {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let reader = BufReader::new(stream);
    let streaming = args[0] == "subscribe";
    for line in reader.lines() {
        let Ok(l) = line else { break };
        if streaming || raw {
            println!("{l}");
        } else {
            render(&args[0], &l);
        }
        if !streaming {
            break;
        }
    }
}
