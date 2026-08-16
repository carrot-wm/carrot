// burrow socket - serde json over a unix socket, one dispatch path. every
// keybind action has an ipc twin. `subscribe` turns the connection into an
// ndjson event stream so shells never poll. unknown command -> error, not "ok".

use crate::config::Action;
use crate::engine::SpawnedFuture;
use crate::state::State;
use crate::util::AsyncEvent;
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::rc::Rc;

pub fn socket_path(display: &str) -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    dir.join(format!("carrot.{display}.sock"))
}

pub struct Ipc {
    pub path: std::path::PathBuf,
    _accept: SpawnedFuture<()>,
    _conns: Rc<RefCell<HashMap<u64, SpawnedFuture<()>>>>,
}

impl Drop for Ipc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn start(state: &Rc<State>, display: &str) -> Result<Ipc, String> {
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, bind, listen, socket_with,
    };
    let path = socket_path(display);
    let _ = std::fs::remove_file(&path);
    let fd = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("ipc socket: {e}"))?;
    let addr = SocketAddrUnix::new(&path).map_err(|e| format!("ipc addr: {e}"))?;
    bind(&fd, &addr).map_err(|e| format!("ipc bind {}: {e}", path.display()))?;
    listen(&fd, 8).map_err(|e| format!("ipc listen: {e}"))?;
    let listener = Rc::new(fd);
    let conns: Rc<RefCell<HashMap<u64, SpawnedFuture<()>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let st = state.clone();
    let cs = conns.clone();
    let accept = state.eng.spawn("ipc accept", async move {
        let mut next_id = 0u64;
        loop {
            match st.ring.accept(&listener).await {
                Ok(fd) => {
                    let id = next_id;
                    next_id += 1;
                    let st2 = st.clone();
                    let cs2 = cs.clone();
                    let task = st.eng.spawn("ipc conn", async move {
                        conn(st2.clone(), Rc::new(fd)).await;
                        // drop our own entry from a fresh task
                        let cs3 = cs2.clone();
                        st2.run_toplevel.schedule(move || {
                            cs3.borrow_mut().remove(&id);
                        });
                    });
                    cs.borrow_mut().insert(id, task);
                }
                Err(e) => {
                    eprintln!("carrot: ipc accept failed: {e}");
                    return;
                }
            }
        }
    });
    Ok(Ipc { path, _accept: accept, _conns: conns })
}

// -- the connection --

async fn conn(state: Rc<State>, fd: Rc<OwnedFd>) {
    let mut pending = Vec::new();
    loop {
        let buf = vec![0u8; 4096];
        let Ok((buf, n)) = state.ring.read(&fd, buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            if line.trim().is_empty() {
                continue;
            }
            // "subscribe" takes everything; {"subscribe":"windows,columns"}
            // filters server-side so an unwanted event is never serialized
            let sub_cats = match serde_json::from_str::<Value>(line.trim()) {
                Ok(Value::String(c)) if c == "subscribe" => Some(Ok(CATS_ALL)),
                Ok(Value::Object(m)) if m.len() == 1 => match m.get("subscribe") {
                    Some(Value::String(spec)) => Some(parse_cats(spec)),
                    Some(Value::Null) => Some(Ok(CATS_ALL)),
                    _ => None,
                },
                _ => None,
            };
            if let Some(cats) = sub_cats {
                match cats {
                    Ok(cats) => {
                        subscribe(&state, &fd, cats).await;
                        return;
                    }
                    Err(e) => {
                        let mut out = json!({ "error": e }).to_string().into_bytes();
                        out.push(b'\n');
                        let _ = write_all(&state, &fd, out).await;
                        return;
                    }
                }
            }
            let reply = match handle(&state, line.trim()) {
                Ok(v) => json!({ "ok": v }),
                Err(e) => json!({ "error": e }),
            };
            let mut out = reply.to_string().into_bytes();
            out.push(b'\n');
            if write_all(&state, &fd, out).await.is_err() {
                return;
            }
        }
    }
}

async fn write_all(state: &Rc<State>, fd: &Rc<OwnedFd>, mut buf: Vec<u8>) -> Result<(), ()> {
    while !buf.is_empty() {
        match state.ring.write(fd, buf).await {
            Ok((b, n)) if n > 0 => {
                buf = b;
                buf.drain(..n);
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

// keybinds share this same dispatch_action; queries are plain string commands
fn handle(state: &Rc<State>, line: &str) -> Result<Value, String> {
    if let Ok(action) = serde_json::from_str::<Action>(line) {
        dispatch_action(state, &action);
        return Ok(json!(true));
    }
    match serde_json::from_str::<String>(line).as_deref() {
        Ok("monitors") => Ok(monitors_json(state)),
        Ok("outputs") => Ok(outputs_json(state)),
        Ok("workspaces") => Ok(workspaces_json(state)),
        Ok("windows") => Ok(windows_json(state)),
        Ok("clients") => Ok(clients_json(state)),
        Ok("reload") => reload(state).map(|_| json!(true)),
        Ok("errors") => Ok(errors_json(state)),
        Ok("binds") => Ok(binds_json(state)),
        Ok("dump-shadow") => dump_shadow(state),
        Ok("dpms-off") => {
            crate::output::dpms(state, false);
            Ok(json!(true))
        }
        Ok("dpms-on") => {
            crate::output::dpms(state, true);
            Ok(json!(true))
        }
        Ok(other) => Err(format!("unknown command \"{other}\"")),
        Err(_) => Err(format!("cannot parse \"{line}\"")),
    }
}

/// verbs that reshape the strip: the shell redraws column topology off
/// one event instead of re-querying every window
fn reshapes_columns(action: &Action) -> bool {
    matches!(
        action,
        Action::MoveColumnLeft
            | Action::MoveColumnRight
            | Action::MoveColumnToFirst
            | Action::MoveColumnToLast
            | Action::ConsumeOrExpelLeft
            | Action::ConsumeOrExpelRight
            | Action::ConsumeIntoColumn
            | Action::ExpelFromColumn
            | Action::SwapDir(_)
            | Action::SetLayout(_)
    )
}

pub fn dispatch_action(state: &Rc<State>, action: &Action) {
    dispatch_inner(state, action);
    if reshapes_columns(action) {
        crate::ipc::columns_event(state, &crate::tree::active(state));
    }
}

fn dispatch_inner(state: &Rc<State>, action: &Action) {
    match action {
        Action::FocusWorkspace(n) => crate::tree::switch_workspace(state, *n),
        Action::FocusWorkspaceRel(d) => crate::tree::switch_workspace_rel(state, *d),
        Action::SendToWorkspace(n) => crate::tree::send_to_workspace(state, *n, false),
        Action::MoveToWorkspace(n) => crate::tree::send_to_workspace(state, *n, true),
        Action::MoveWorkspaceToOutput(n) => {
            crate::tree::move_workspace_to_output(state, state.active_ws.get(), *n)
        }
        Action::ToggleFullscreen => {
            if let Some(win) = crate::tree::focused_window(state) {
                let on = !win.fullscreen.get();
                crate::tree::set_fullscreen(state, &win, on);
                win.set_fullscreen_state(on);
            }
        }
        Action::ToggleFloating => {
            if let Some(win) = crate::tree::focused_window(state) {
                crate::tree::float::toggle_floating(state, &win);
            }
        }
        Action::CloseWindow => {
            if let Some(win) = crate::tree::focused_window(state) {
                win.send_close();
            }
        }
        Action::FocusNext => crate::tree::focus_cycle(state, 1),
        Action::FocusPrev => crate::tree::focus_cycle(state, -1),
        Action::FocusDir(d) => crate::tree::focus_dir(state, *d),
        Action::SwapDir(d) => crate::tree::swap_dir(state, *d),
        Action::AdjustSplitRatio(d) => {
            if let Some(win) = crate::tree::focused_window(state) {
                if !win.floating.get() && !win.fullscreen.get() {
                    let ws = crate::tree::workspace_of(state, &win)
                        .unwrap_or_else(|| crate::tree::active(state));
                    let hit = match ws.tiling.mode() {
                        crate::config::LayoutMode::Dwindle => {
                            crate::tree::dwindle::adjust_parent_ratio(&win, *d)
                        }
                        crate::config::LayoutMode::Scrolling => {
                            ws.tiling.strip.adjust_width(&win, *d)
                        }
                    };
                    if hit {
                        crate::tree::relayout(state, &ws);
                        state.damage.trigger();
                    }
                }
            }
        }
        Action::ConsumeOrExpelLeft | Action::ConsumeOrExpelRight => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws
                        .tiling
                        .strip
                        .consume_or_expel(&win, matches!(action, Action::ConsumeOrExpelLeft))
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::MoveColumnLeft | Action::MoveColumnRight => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws
                        .tiling
                        .strip
                        .move_column(&win, matches!(action, Action::MoveColumnLeft))
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::CycleColumnWidth | Action::CycleColumnWidthBack => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                crate::trace!(
                    "cycle-width: #{} ws_mode={:?} owned={} floating={} fs={}",
                    win.ident,
                    ws.tiling.mode(),
                    crate::tree::workspace_of(state, &win).is_some(),
                    win.floating.get(),
                    win.fullscreen.get()
                );
                let scfg = state.config.borrow().layout.scrolling.clone();
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws.tiling.strip.cycle_width(
                        &win,
                        &scfg,
                        matches!(action, Action::CycleColumnWidthBack),
                    )
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::ToggleFullWidth => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws.tiling.strip.toggle_full_width(&win)
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::CenterColumn => {
            let ws = crate::tree::active(state);
            if ws.tiling.mode() == crate::config::LayoutMode::Scrolling {
                ws.tiling
                    .strip
                    .center_active(crate::tree::tiling_area_for(state, &ws));
                crate::tree::relayout(state, &ws);
                state.damage.trigger();
            }
        }
        Action::FocusColumnFirst | Action::FocusColumnLast => {
            let ws = crate::tree::active(state);
            if ws.tiling.mode() == crate::config::LayoutMode::Scrolling {
                if let Some(next) =
                    ws.tiling.strip.focus_edge(matches!(action, Action::FocusColumnLast))
                {
                    crate::tree::relayout(state, &ws);
                    crate::tree::focus_window(state, Some(&next));
                    state.damage.trigger();
                }
            }
        }
        Action::MoveColumnToFirst | Action::MoveColumnToLast => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws
                        .tiling
                        .strip
                        .move_column_to_edge(&win, matches!(action, Action::MoveColumnToLast))
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::ConsumeIntoColumn | Action::ExpelFromColumn => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                let hit = ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && if matches!(action, Action::ConsumeIntoColumn) {
                        ws.tiling.strip.consume_into(&win)
                    } else {
                        ws.tiling.strip.expel_from(&win)
                    };
                if hit {
                    crate::tree::relayout(state, &ws);
                    if let Some(next) = ws.tiling.strip.active_window() {
                        crate::tree::focus_window(state, Some(&next));
                    }
                    state.damage.trigger();
                }
            }
        }
        Action::ExpandColumn => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws.tiling.strip.expand_width(&win)
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::CycleWindowHeight | Action::CycleWindowHeightBack => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                let scfg = state.config.borrow().layout.scrolling.clone();
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws.tiling.strip.cycle_tile_height(
                        &win,
                        &scfg,
                        matches!(action, Action::CycleWindowHeightBack),
                    )
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::ResetWindowHeight => {
            if let Some(win) = crate::tree::focused_window(state) {
                let ws = crate::tree::workspace_of(state, &win)
                    .unwrap_or_else(|| crate::tree::active(state));
                if ws.tiling.mode() == crate::config::LayoutMode::Scrolling
                    && !win.floating.get()
                    && !win.fullscreen.get()
                    && ws.tiling.strip.reset_tile_heights(&win)
                {
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::SetLayout(arg) => {
            let ws = crate::tree::active(state);
            let mode = match arg {
                crate::config::SetLayoutArg::Dwindle => crate::config::LayoutMode::Dwindle,
                crate::config::SetLayoutArg::Scrolling => crate::config::LayoutMode::Scrolling,
                crate::config::SetLayoutArg::Toggle => match ws.tiling.mode() {
                    crate::config::LayoutMode::Dwindle => crate::config::LayoutMode::Scrolling,
                    crate::config::LayoutMode::Scrolling => crate::config::LayoutMode::Dwindle,
                },
            };
            crate::tree::set_layout(state, &ws, mode);
        }
        Action::PointerMove => {
            if let Some(seat) = state.seat.borrow().clone() {
                let (x, y) = (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
                if let Some((win, ..)) = crate::tree::window_at(state, x, y) {
                    seat.start_move_grab(state, win);
                }
            }
        }
        Action::PointerResize => {
            if let Some(seat) = state.seat.borrow().clone() {
                let (x, y) = (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
                if let Some((win, ..)) = crate::tree::window_at(state, x, y) {
                    use crate::tree::dwindle::{EDGE_BOTTOM, EDGE_LEFT, EDGE_RIGHT, EDGE_TOP};
                    let r = win.rect.get();
                    let edges = if x < (r.x1 + r.x2) / 2 { EDGE_LEFT } else { EDGE_RIGHT }
                        | if y < (r.y1 + r.y2) / 2 { EDGE_TOP } else { EDGE_BOTTOM };
                    seat.start_resize_grab(state, win, edges);
                }
            }
        }
        Action::Spawn(argv) => spawn_argv(state, argv),
        Action::SpawnSh(cmd) => spawn_sh(state, cmd),
        Action::Quit => state.ring.stop(),
    }
}

/// reap only the children the spawn sites own; a blanket wait(-1) here
/// once stole the xwayland child's exit status out from under its waiter,
/// leaving a raw-pid kill aimed at a number the kernel could reuse
pub(crate) fn reap_spawned(state: &Rc<State>) {
    use rustix::process::{WaitOptions, waitpid};
    state
        .reap_pids
        .borrow_mut()
        .retain(|pid| matches!(waitpid(Some(*pid), WaitOptions::NOHANG), Ok(None)));
}

// reap first so dead children never pile up as zombies, then detach the
// new one into its own session
fn spawn_cmd(state: &Rc<State>, mut c: std::process::Command, what: &str) {
    reap_spawned(state);
    use std::os::unix::process::CommandExt;
    for (name, val) in state.config.borrow().environment.iter() {
        match val {
            Some(v) => {
                c.env(name, v);
            }
            None => {
                c.env_remove(name);
            }
        }
    }
    if let Some(xw) = state.xwayland.borrow().as_ref() {
        c.env("DISPLAY", format!(":{}", xw.display));
    }
    // never hand clients the launch tty: setsid drops it as controlling
    // terminal but the inherited fd still reads - a surviving child then
    // steals the shell's keystrokes once the compositor is gone
    c.stdin(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            crate::sighand::unblock_all_in_child();
            let _ = rustix::process::setsid();
            Ok(())
        });
    }
    match c.spawn() {
        Ok(child) => {
            if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                state.reap_pids.borrow_mut().push(pid);
            }
        }
        Err(e) => eprintln!("carrot: spawn \"{what}\": {e}"),
    }
}

fn spawn_argv(state: &Rc<State>, argv: &[String]) {
    let Some((bin, rest)) = argv.split_first() else { return };
    let mut c = std::process::Command::new(bin);
    c.args(rest);
    spawn_cmd(state, c, bin);
}

fn spawn_sh(state: &Rc<State>, cmd: &str) {
    let mut c = std::process::Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    spawn_cmd(state, c, cmd);
}

pub fn run_spawn(state: &Rc<State>, s: &crate::config::SpawnCfg) {
    match s {
        crate::config::SpawnCfg::Argv(argv) => spawn_argv(state, argv),
        crate::config::SpawnCfg::Sh(cmd) => spawn_sh(state, cmd),
    }
}

/// the last config load's diagnostics, startup or reload
fn errors_json(state: &Rc<State>) -> Value {
    let stored = state.last_config_event.borrow();
    let Some(v) = stored.as_deref().and_then(|ev| serde_json::from_str::<Value>(ev).ok()) else {
        return json!({ "failed": false, "errors": [], "cold-keys-pending": [] });
    };
    json!({
        "failed": v.get("failed").cloned().unwrap_or(json!(false)),
        "errors": v.get("errors").cloned().unwrap_or(json!([])),
        "cold-keys-pending": v.get("cold-keys-pending").cloned().unwrap_or(json!([])),
    })
}

/// emit the config status and keep it for late subscribers
pub fn config_event(state: &Rc<State>, failed: bool, errors: &[String], cold: &[String]) {
    let ev = json!({ "event": "config-loaded", "failed": failed,
                     "errors": errors, "cold-keys-pending": cold });
    *state.last_config_event.borrow_mut() = Some(ev.to_string());
    emit_cat(state, Cat::Config, || ev.clone());
}

// a failed reload keeps the running config, never a default
pub fn reload(state: &Rc<State>) -> Result<(), String> {
    let path = crate::config::config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("{}: {e}", path.display());
            config_event(state, true, std::slice::from_ref(&msg), &[]);
            return Err(msg);
        }
    };
    let parsed = if path.extension().is_some_and(|e| e == "lua") {
        crate::config::lua::parse_at(&text, &path)
    } else {
        crate::config::kdl::parse_at(&text, &path)
    };
    let cfg = match parsed {
        Ok(c) => c,
        Err(errors) => {
            config_event(state, true, &errors, &[]);
            return Err(errors.join("; "));
        }
    };
    // cold keys: swapped in, but the running session won't reflect them
    let mut cold: Vec<String> = Vec::new();
    {
        let old = state.config.borrow().clone();
        if old.outputs != cfg.outputs {
            cold.push("outputs".to_string());
        }
        if old.debug != cfg.debug {
            cold.push("debug".to_string());
        }
        if old.spawns != cfg.spawns {
            cold.push("spawn-at-startup".to_string());
        }
    }
    for k in &cold {
        eprintln!("carrot: config: {k}: needs restart");
    }
    let sw = cfg.cursor.software;
    state.anim_clock.set_global(cfg.animations.off, cfg.animations.slowdown);
    *state.config.borrow_mut() = Rc::new(cfg);
    if let Some(seat) = state.seat.borrow().clone() {
        seat.apply_input_config(state);
    }
    if let Some(d) = state.display.borrow().as_ref() {
        d.set_software_cursor(state, sw);
    }
    if let Some(d) = state.display.borrow().as_ref() {
        for out in d.outputs.borrow().iter() {
            out.blur_dirty.set(true);
        }
    }
    crate::tree::reapply_rules(state);
    let ws = crate::tree::active(state);
    crate::tree::relayout(state, &ws);
    crate::shell::layer::arrange(state);
    state.damage.trigger();
    // hidden-refresh/fps policy changes bite on the next tick pass
    state.cast_kick.trigger();
    config_event(state, false, &[], &cold);
    Ok(())
}

// -- queries --

// forensics: the focused window's commit-time shm shadow as a ppm.
// blank regions here mean the client committed blank pixels; full
// content here with a blank screen means the upload/quad path lies
fn dump_shadow(state: &Rc<State>) -> Result<Value, String> {
    let win = crate::tree::focused_window(state).ok_or("no focused window")?;
    let s = win.surface();
    let shadow = s.shm_shadow.borrow();
    let px = shadow.as_ref().ok_or("no shm shadow (dmabuf client?)")?;
    let buf = s.buffer.borrow();
    let b = &buf.as_ref().ok_or("no buffer attached")?.buf;
    let (w, h) = (b.rect.width() as usize, b.rect.height() as usize);
    if px.len() < w * h * 4 {
        return Err(format!("shadow {} short of {}x{}", px.len(), w, h));
    }
    let path = format!("/tmp/carrot-shadow-{}.ppm", s.uid);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in px[..w * h * 4].chunks_exact(4) {
        // shm is little-endian [a]rgb; ppm wants rgb
        out.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    std::fs::write(&path, out).map_err(|e| format!("write {path}: {e}"))?;
    Ok(json!({ "path": path, "w": w, "h": h, "gen": s.content_gen.get() }))
}

fn monitors_json(state: &Rc<State>) -> Value {
    let focused = state.focused_output.get();
    let d = state.display.borrow();
    let outs: Vec<Value> = d
        .as_ref()
        .map(|d| {
            d.outputs
                .borrow()
                .iter()
                .map(|o| {
                    let (x, y) = o.pos.get();
                    json!({
                        "name": o.conn.name,
                        "x": x,
                        "y": y,
                        "width": o.width,
                        "height": o.height,
                        // no fractional scale yet; honest constant
                        "scale": 1.0,
                        "workspace": o.ws.get() + 1,
                        "focused": o.index.get() == focused,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!(outs)
}

fn workspaces_json(state: &Rc<State>) -> Value {
    let list = state.workspaces.borrow().clone();
    let ws: Vec<Value> = list
        .iter()
        .enumerate()
        .map(|(i, w)| workspace_json(state, i, w))
        .collect();
    json!(ws)
}

fn windows_json(state: &Rc<State>) -> Value {
    let focus = focused_surface(state);
    let ws = crate::tree::active(state);
    let idx = state.active_ws.get();
    let out = ws_output(state, &ws);
    let mut list = Vec::new();
    ws.for_each(|w| {
        list.push(window_json(state, &ws, idx, out.as_deref(), w, focus.as_ref(), true));
    });
    json!(list)
}

/// every window on every workspace, with owner and placement detail;
/// `windows` stays the light active-workspace view
fn clients_json(state: &Rc<State>) -> Value {
    let focus = focused_surface(state);
    let list = state.workspaces.borrow().clone();
    let mut out = Vec::new();
    for (i, ws) in list.iter().enumerate() {
        let name = ws_output(state, ws);
        ws.for_each(|w| {
            out.push(window_json(state, ws, i, name.as_deref(), w, focus.as_ref(), true));
        });
    }
    json!(out)
}

// -- v2 record builders --
//
// one shape per thing, built once and fanned to every subscriber. events
// carry whole records so a shell never re-queries; that is the entire
// point of v2. `legacy` adds the flat pre-v2 keys that -j clients still
// promises, so query consumers keep working while events stay lean.

pub type Ws = Rc<crate::tree::workspace::Workspace>;

fn ws_index(state: &Rc<State>, ws: &Ws) -> usize {
    state
        .workspaces
        .borrow()
        .iter()
        .position(|w| Rc::ptr_eq(w, ws))
        .unwrap_or(0)
}

fn ws_output(state: &Rc<State>, ws: &Ws) -> Option<String> {
    state.display.borrow().as_ref().and_then(|d| {
        d.outputs
            .borrow()
            .get(ws.output.get())
            .map(|o| o.conn.name.clone())
    })
}

fn focused_surface(state: &Rc<State>) -> Option<Rc<crate::surface::WlSurface>> {
    let seat = state.seat.borrow().clone();
    seat.and_then(|s| s.kb_focus.borrow().clone())
}

/// the canonical window record
fn window_json(
    state: &Rc<State>,
    ws: &Ws,
    ws_idx: usize,
    output: Option<&str>,
    win: &Rc<crate::tree::Window>,
    focus: Option<&Rc<crate::surface::WlSurface>>,
    legacy: bool,
) -> Value {
    // painted rect: a fullscreen window reports where it draws, not the
    // tile it will return to
    let r = win.draw_rect(state);
    let s = win.surface();
    let (col, col_idx) = match ws.tiling.column_of(win) {
        Some((id, i)) => (json!(id), json!(i)),
        // dwindle is a bsp tree with no columns; null, not zero
        None => (Value::Null, Value::Null),
    };
    let focused = focus.is_some_and(|f| Rc::ptr_eq(f, &s));
    let mut v = json!({
        "id": s.uid,
        "title": win.title(),
        "app-id": win.app_id(),
        "pid": s.client.pid,
        "workspace": ws_idx + 1,
        "output": output,
        "column": col,
        "column-index": col_idx,
        "geometry": { "x": r.x1, "y": r.y1, "w": r.width(), "h": r.height() },
        "state": {
            "focused": focused,
            "fullscreen": win.fullscreen.get(),
            "floating": win.floating.get(),
            "xwayland": win.x11_opt().is_some(),
            "mapped": s.mapped.get(),
        },
    });
    if legacy {
        // pre-v2 flat keys. deprecated: read geometry{} and state{} instead
        let m = v.as_object_mut().expect("object");
        m.insert("x".into(), json!(r.x1));
        m.insert("y".into(), json!(r.y1));
        m.insert("w".into(), json!(r.width()));
        m.insert("h".into(), json!(r.height()));
        m.insert("floating".into(), json!(win.floating.get()));
        m.insert("fullscreen".into(), json!(win.fullscreen.get()));
        m.insert("xwayland".into(), json!(win.x11_opt().is_some()));
        m.insert("mapped".into(), json!(s.mapped.get()));
        m.insert("focused".into(), json!(focused));
    }
    v
}

/// the canonical workspace record
fn workspace_json(state: &Rc<State>, i: usize, ws: &Ws) -> Value {
    let mut count = 0;
    ws.for_each(|_| count += 1);
    json!({
        "id": i + 1,
        "index": i + 1,
        "output": ws_output(state, ws),
        "active": i == state.active_ws.get(),
        "window-count": count,
        // pre-v2 name for the same number; deprecated
        "windows": count,
        "layout": match ws.tiling.mode() {
            crate::config::LayoutMode::Dwindle => "dwindle",
            crate::config::LayoutMode::Scrolling => "scrolling",
        },
    })
}

/// name, geometry and the edges layer-shell reserved. transform and scale
/// are reported for forward compatibility and are constant today: carrot
/// implements neither output rotation nor fractional scale, so claiming
/// anything else would be a lie a shell would act on
fn output_json(out: &Rc<crate::output::Output>, focused: bool) -> Value {
    let r = out.rect();
    let u = out.usable.get();
    let refresh = out
        .conn
        .pipe
        .borrow()
        .as_ref()
        .map(|p| p.mode.vrefresh)
        .unwrap_or(0);
    json!({
        "name": out.conn.name,
        "x": r.x1,
        "y": r.y1,
        "width": out.width,
        "height": out.height,
        "scale": 1.0,
        "transform": 0,
        "refresh": refresh,
        "workspace": out.ws.get() + 1,
        "focused": focused,
        // exclusive zones, as edge insets from the output rect
        "reserved": {
            "top": u.y1 - r.y1,
            "bottom": r.y2 - u.y2,
            "left": u.x1 - r.x1,
            "right": r.x2 - u.x2,
        },
    })
}

fn outputs_json(state: &Rc<State>) -> Value {
    let focused = state.focused_output.get();
    let d = state.display.borrow();
    let outs: Vec<Value> = d
        .as_ref()
        .map(|d| {
            d.outputs
                .borrow()
                .iter()
                .map(|o| output_json(o, o.index.get() == focused))
                .collect()
        })
        .unwrap_or_default();
    json!(outs)
}

// -- event categories --

#[derive(Copy, Clone, PartialEq)]
pub enum Cat {
    Windows,
    Workspaces,
    Outputs,
    Columns,
    Config,
}

impl Cat {
    fn bit(self) -> u8 {
        match self {
            Cat::Windows => 1,
            Cat::Workspaces => 2,
            Cat::Outputs => 4,
            Cat::Columns => 8,
            Cat::Config => 16,
        }
    }

    fn parse(s: &str) -> Option<Cat> {
        Some(match s {
            "windows" => Cat::Windows,
            "workspaces" => Cat::Workspaces,
            "outputs" => Cat::Outputs,
            "columns" => Cat::Columns,
            "config" => Cat::Config,
            _ => return None,
        })
    }
}

const CATS_ALL: u8 = 31;

/// "windows,columns" -> mask. an unknown name is an error, not a silent
/// subscribe-to-nothing
fn parse_cats(spec: &str) -> Result<u8, String> {
    let mut mask = 0;
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match Cat::parse(part) {
            Some(c) => mask |= c.bit(),
            None => return Err(format!("unknown event category \"{part}\"")),
        }
    }
    if mask == 0 { Ok(CATS_ALL) } else { Ok(mask) }
}

// -- v2 event emitters --

/// build once, fan to everyone who asked for that category. the closure
/// never runs when nobody is listening, so an unsubscribed event costs a
/// borrow and a bitmask test
fn emit_cat(state: &Rc<State>, cat: Cat, make: impl FnOnce() -> Value) {
    let subs: Vec<Rc<Subscriber>> = state
        .ipc_subs
        .borrow()
        .iter()
        .filter(|s| !s.dead.get() && s.cats.get() & cat.bit() != 0)
        .cloned()
        .collect();
    if subs.is_empty() {
        return;
    }
    let v = make();
    for sub in subs {
        sub.push(&v);
    }
}

/// a whole-window event: opened, focused, fullscreen, moved
pub fn window_event(state: &Rc<State>, event: &str, win: &Rc<crate::tree::Window>) {
    emit_cat(state, Cat::Windows, || {
        let ws = crate::tree::workspace_of(state, win);
        let (ws, idx) = match ws {
            Some(w) => {
                let i = ws_index(state, &w);
                (w, i)
            }
            None => (crate::tree::active(state), state.active_ws.get()),
        };
        let out = ws_output(state, &ws);
        let focus = focused_surface(state);
        json!({
            "event": event,
            "window": window_json(state, &ws, idx, out.as_deref(), win, focus.as_ref(), false),
        })
    });
}

/// close carries only what still exists: the tree already forgot the
/// window, so the caller hands us the column it was in
pub fn window_closed_event(
    state: &Rc<State>,
    win: &Rc<crate::tree::Window>,
    ws_idx: usize,
    col: Option<(u32, usize)>,
) {
    emit_cat(state, Cat::Windows, || {
        json!({
            "event": "window-closed",
            "window": {
                "id": win.surface().uid,
                "title": win.title(),
                "app-id": win.app_id(),
                "workspace": ws_idx + 1,
                "column": col.map(|c| c.0),
                "column-index": col.map(|c| c.1),
            },
        })
    });
}

pub fn workspace_event(state: &Rc<State>, event: &str, idx: usize) {
    emit_cat(state, Cat::Workspaces, || {
        let ws = state.workspaces.borrow().get(idx).cloned();
        let record = match ws {
            Some(w) => workspace_json(state, idx, &w),
            None => json!({ "index": idx + 1 }),
        };
        json!({ "event": event, "workspace": record })
    });
}

/// strip topology after anything that moves windows between columns or
/// reorders the strip
pub fn columns_event(state: &Rc<State>, ws: &Ws) {
    emit_cat(state, Cat::Columns, || {
        let idx = ws_index(state, ws);
        let cols: Vec<Value> = ws
            .tiling
            .columns()
            .into_iter()
            .enumerate()
            .map(|(i, (id, wins))| json!({ "column": id, "column-index": i, "windows": wins }))
            .collect();
        json!({
            "event": "column-layout-changed",
            "workspace": idx + 1,
            "columns": cols,
        })
    });
}

pub fn outputs_event(state: &Rc<State>) {
    emit_cat(state, Cat::Outputs, || {
        json!({ "event": "outputs-changed", "outputs": outputs_json(state) })
    });
}

// -- the event stream --

pub struct Subscriber {
    fd: Rc<OwnedFd>,
    out: RefCell<Vec<u8>>,
    kick: AsyncEvent,
    dead: Cell<bool>,
    /// which event categories this subscriber asked for
    cats: Cell<u8>,
}

async fn subscribe(state: &Rc<State>, fd: &Rc<OwnedFd>, cats: u8) {
    let sub = Rc::new(Subscriber {
        fd: fd.clone(),
        out: RefCell::new(Vec::new()),
        kick: AsyncEvent::default(),
        dead: Cell::new(false),
        cats: Cell::new(cats),
    });
    // one snapshot, then deltas. everything the shell needs to paint is in
    // this single event, so a fresh subscriber costs one serialize
    let mut snap = serde_json::Map::new();
    snap.insert("event".into(), json!("state"));
    if cats & Cat::Workspaces.bit() != 0 {
        snap.insert("workspaces".into(), workspaces_json(state));
    }
    if cats & Cat::Windows.bit() != 0 {
        snap.insert("windows".into(), clients_json(state));
    }
    if cats & Cat::Outputs.bit() != 0 {
        snap.insert("outputs".into(), outputs_json(state));
    }
    sub.push(&Value::Object(snap));
    // the config status is stateful: late subscribers get the last one
    if cats & Cat::Config.bit() != 0
        && let Some(ev) = state.last_config_event.borrow().as_deref()
        && let Ok(v) = serde_json::from_str::<Value>(ev)
    {
        sub.push(&v);
    }
    state.ipc_subs.borrow_mut().push(sub.clone());
    loop {
        sub.kick.triggered().await;
        loop {
            let buf: Vec<u8> = std::mem::take(&mut *sub.out.borrow_mut());
            if buf.is_empty() {
                break;
            }
            if write_all(state, &sub.fd, buf).await.is_err() {
                sub.dead.set(true);
                state.ipc_subs.borrow_mut().retain(|s| !Rc::ptr_eq(s, &sub));
                return;
            }
        }
    }
}

impl Subscriber {
    fn push(&self, v: &Value) {
        let mut out = self.out.borrow_mut();
        out.extend_from_slice(v.to_string().as_bytes());
        out.push(b'\n');
        self.kick.trigger();
    }
}


/// every bind with its wire-twin action; shells render hotkey overlays
/// from this instead of a compositor-drawn one
fn binds_json(state: &Rc<State>) -> Value {
    let cfg = state.config.borrow().clone();
    let list: Vec<Value> = cfg
        .binds
        .iter()
        .map(|b| {
            json!({
                "mods": b.mods,
                "key": b.key,
                "action": b.action,
                "title": b.title,
                "allow-when-locked": b.allow_when_locked,
            })
        })
        .collect();
    json!(list)
}

#[cfg(test)]
mod burrow_contract {
    use crate::config::{Action, Dir};

    /// exactly what src/bin/burrow.rs puts on the wire. burrow is a separate
    /// binary with no lib to share, so this is the pin: rename an Action
    /// variant and the verb that sends it fails here instead of silently
    /// becoming a no-op at runtime, which is how five of them died before
    #[test]
    fn every_burrow_verb_parses_as_an_action() {
        let cases: &[(&str, Action)] = &[
            (r#"{"focus-workspace":2}"#, Action::FocusWorkspace(2)),
            (r#"{"focus-workspace-rel":1}"#, Action::FocusWorkspaceRel(1)),
            (r#"{"focus-workspace-rel":-1}"#, Action::FocusWorkspaceRel(-1)),
            (r#"{"send-to-workspace":2}"#, Action::SendToWorkspace(2)),
            (r#"{"move-to-workspace":2}"#, Action::MoveToWorkspace(2)),
            (r#"{"adjust-split-ratio":0.05}"#, Action::AdjustSplitRatio(0.05)),
            (
                r#"{"spawn":["foot","-e","bash"]}"#,
                Action::Spawn(vec!["foot".into(), "-e".into(), "bash".into()]),
            ),
            (r#"{"focus-dir":"left"}"#, Action::FocusDir(Dir::Left)),
            (r#"{"swap-dir":"right"}"#, Action::SwapDir(Dir::Right)),
            (r#""toggle-fullscreen""#, Action::ToggleFullscreen),
            (r#""toggle-floating""#, Action::ToggleFloating),
            (r#""close-window""#, Action::CloseWindow),
            (r#""focus-next""#, Action::FocusNext),
            (r#""focus-prev""#, Action::FocusPrev),
            (r#""quit""#, Action::Quit),
        ];
        for (wire, want) in cases {
            let got: Action = serde_json::from_str(wire)
                .unwrap_or_else(|e| panic!("burrow sends {wire}, which no longer parses: {e}"));
            assert_eq!(&got, want, "{wire}");
        }
    }

    /// every query burrow can send must be answered. driving handle()
    /// directly means the list cannot drift away from the real dispatch
    #[test]
    fn every_burrow_query_is_answered() {
        let (state, _client) = crate::client::test_utils::test_client();
        for q in [
            "monitors", "outputs", "workspaces", "windows", "clients", "errors", "binds",
        ] {
            let line = format!("\"{q}\"");
            let r = super::handle(&state, &line);
            assert!(r.is_ok(), "burrow offers \"{q}\" but handle() rejects it: {r:?}");
        }
    }
}

#[cfg(test)]
mod v2_shapes {
    use super::*;

    #[test]
    fn event_categories_parse_and_default_to_everything() {
        assert_eq!(parse_cats("windows").unwrap(), Cat::Windows.bit());
        assert_eq!(
            parse_cats("windows,columns").unwrap(),
            Cat::Windows.bit() | Cat::Columns.bit()
        );
        // whitespace and empties are tolerated, not an error
        assert_eq!(parse_cats(" windows , outputs ").unwrap(),
                   Cat::Windows.bit() | Cat::Outputs.bit());
        // no categories at all means everything, matching a bare subscribe
        assert_eq!(parse_cats("").unwrap(), CATS_ALL);
        // a typo must be loud: silently subscribing to nothing would look
        // like a dead compositor to a shell
        assert!(parse_cats("windwos").is_err());
    }

    #[test]
    fn a_window_record_carries_everything_a_shell_needs() {
        let (state, client) = crate::client::test_utils::test_client();
        let mut c = crate::config::Config::default();
        c.layout.mode = crate::config::LayoutMode::Scrolling;
        *state.config.borrow_mut() = Rc::new(c);
        let base = crate::shell::xdg::tests::mk_base(&client, 400);
        let (_s, _x, tl) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 401, 402, 403);
        let win = Rc::new(crate::tree::Window::new(&state, crate::tree::WindowKind::Xdg(tl)));
        let ws = crate::tree::active(&state);
        ws.tiling.insert(&win, 0, 0, &state.config.borrow().layout.scrolling);

        let v = window_json(&state, &ws, 0, Some("DP-1"), &win, None, false);
        for key in [
            "id", "title", "app-id", "pid", "workspace", "output", "column",
            "column-index", "geometry", "state",
        ] {
            assert!(v.get(key).is_some(), "window record is missing {key}");
        }
        for key in ["x", "y", "w", "h"] {
            assert!(v.get(key).is_none(), "{key} is a legacy key, not for events");
        }
        for key in ["focused", "fullscreen", "floating", "xwayland", "mapped"] {
            assert!(v["state"].get(key).is_some(), "state is missing {key}");
        }
        for key in ["x", "y", "w", "h"] {
            assert!(v["geometry"].get(key).is_some(), "geometry is missing {key}");
        }
        // a scrolling workspace puts it in a real column
        assert!(v["column"].is_u64(), "scrolling window has a column id");
        assert_eq!(v["column-index"], serde_json::json!(0));
        assert_eq!(v["workspace"], serde_json::json!(1), "1-based on the wire");

        // the query form keeps the deprecated flat keys
        let legacy = window_json(&state, &ws, 0, Some("DP-1"), &win, None, true);
        for key in ["x", "y", "w", "h", "floating", "fullscreen", "focused"] {
            assert!(legacy.get(key).is_some(), "-j clients lost {key}");
        }
    }

    #[test]
    fn a_dwindle_window_reports_no_column() {
        let (state, client) = crate::client::test_utils::test_client();
        let base = crate::shell::xdg::tests::mk_base(&client, 500);
        let (_s, _x, tl) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 501, 502, 503);
        let win = Rc::new(crate::tree::Window::new(&state, crate::tree::WindowKind::Xdg(tl)));
        let ws = crate::tree::active(&state);
        ws.tiling.insert(&win, 0, 0, &state.config.borrow().layout.scrolling);
        let v = window_json(&state, &ws, 0, None, &win, None, false);
        // null, not 0: a shell must not render a phantom column
        assert!(v["column"].is_null(), "dwindle has no columns");
        assert!(v["column-index"].is_null());
    }

    #[test]
    fn a_workspace_record_counts_its_windows() {
        let (state, _client) = crate::client::test_utils::test_client();
        let ws = crate::tree::active(&state);
        let v = workspace_json(&state, 0, &ws);
        assert_eq!(v["window-count"], serde_json::json!(0));
        // the pre-v2 alias stays until consumers move off it
        assert_eq!(v["windows"], serde_json::json!(0));
        assert_eq!(v["index"], serde_json::json!(1));
        assert_eq!(v["active"], serde_json::json!(true));
    }
}
