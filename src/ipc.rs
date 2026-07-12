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
            if line.trim() == "\"subscribe\"" {
                subscribe(&state, &fd).await;
                return;
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
        Ok("workspaces") => Ok(workspaces_json(state)),
        Ok("windows") => Ok(windows_json(state)),
        Ok("clients") => Ok(clients_json(state)),
        Ok("reload") => reload(state).map(|_| json!(true)),
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

pub fn dispatch_action(state: &Rc<State>, action: &Action) {
    match action {
        Action::FocusWorkspace(n) => crate::tree::switch_workspace(state, *n),
        Action::FocusWorkspaceRel(d) => crate::tree::switch_workspace_rel(state, *d),
        Action::SendToWorkspace(n) => crate::tree::send_to_workspace(state, *n, false),
        Action::MoveToWorkspace(n) => crate::tree::send_to_workspace(state, *n, true),
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
                if !win.floating.get()
                    && !win.fullscreen.get()
                    && crate::tree::dwindle::adjust_parent_ratio(&win, *d)
                {
                    let ws = crate::tree::workspace_of(state, &win)
                        .unwrap_or_else(|| crate::tree::active(state));
                    crate::tree::relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
        Action::Spawn(argv) => spawn_argv(state, argv),
        Action::SpawnSh(cmd) => spawn_sh(state, cmd),
        Action::Quit => state.ring.stop(),
    }
}

// reap first so dead children never pile up as zombies, then detach the
// new one into its own session
fn spawn_cmd(state: &Rc<State>, mut c: std::process::Command, what: &str) {
    use rustix::process::{WaitOptions, wait};
    while let Ok(Some(_)) = wait(WaitOptions::NOHANG) {}
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
    unsafe {
        c.pre_exec(|| {
            crate::sighand::unblock_all_in_child();
            let _ = rustix::process::setsid();
            Ok(())
        });
    }
    match c.spawn() {
        Ok(_) => {}
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

/// emit the config status and keep it for late subscribers
pub fn config_event(state: &Rc<State>, failed: bool, errors: &[String], cold: &[String]) {
    let ev = json!({ "event": "config-loaded", "failed": failed,
                     "errors": errors, "cold-keys-pending": cold });
    *state.last_config_event.borrow_mut() = Some(ev.to_string());
    emit(state, &ev);
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
        crate::config::lua::parse(&text)
    } else {
        crate::config::parse(&text)
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
    *state.config.borrow_mut() = Rc::new(cfg);
    if let Some(seat) = state.seat.borrow().clone() {
        seat.apply_input_config(state);
    }
    if let Some(d) = state.display.borrow().as_ref() {
        d.set_software_cursor(state, sw);
    }
    let ws = crate::tree::active(state);
    crate::tree::relayout(state, &ws);
    crate::shell::layer::arrange(state);
    state.damage.trigger();
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
    let list = state.workspaces.borrow();
    let ws: Vec<Value> = list
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let mut count = 0;
            w.for_each(|_| count += 1);
            let output = state.display.borrow().as_ref().and_then(|d| {
                d.outputs
                    .borrow()
                    .get(w.output.get())
                    .map(|o| o.conn.name.clone())
            });
            json!({
                "index": i + 1,
                "windows": count,
                "active": i == state.active_ws.get(),
                "output": output,
            })
        })
        .collect();
    json!(ws)
}

fn windows_json(state: &Rc<State>) -> Value {
    let seat = state.seat.borrow().clone();
    let focus = seat.and_then(|s| s.kb_focus.borrow().clone());
    let ws = crate::tree::active(state);
    let mut out = Vec::new();
    ws.for_each(|w| {
        let r = w.rect.get();
        out.push(json!({
            "title": w.title(),
            "app-id": w.app_id(),
            "x": r.x1, "y": r.y1, "w": r.width(), "h": r.height(),
            "floating": w.floating.get(),
            "fullscreen": w.fullscreen.get(),
            "focused": focus.as_ref().is_some_and(|f| Rc::ptr_eq(f, &w.surface())),
        }));
    });
    json!(out)
}

/// every window on every workspace, with owner and placement detail;
/// `windows` stays the light active-workspace view
fn clients_json(state: &Rc<State>) -> Value {
    let seat = state.seat.borrow().clone();
    let focus = seat.and_then(|s| s.kb_focus.borrow().clone());
    let list = state.workspaces.borrow().clone();
    let mut out = Vec::new();
    for (i, ws) in list.iter().enumerate() {
        let output = state.display.borrow().as_ref().and_then(|d| {
            d.outputs
                .borrow()
                .get(ws.output.get())
                .map(|o| o.conn.name.clone())
        });
        ws.for_each(|w| {
            let r = w.draw_rect(state);
            let s = w.surface();
            out.push(json!({
                "id": s.uid,
                "title": w.title(),
                "app-id": w.app_id(),
                "pid": s.client.pid,
                "workspace": i + 1,
                "output": output,
                "x": r.x1, "y": r.y1, "w": r.width(), "h": r.height(),
                "floating": w.floating.get(),
                "fullscreen": w.fullscreen.get(),
                "xwayland": w.x11_opt().is_some(),
                "mapped": s.mapped.get(),
                "focused": focus.as_ref().is_some_and(|f| Rc::ptr_eq(f, &s)),
            }));
        });
    }
    json!(out)
}

// -- the event stream --

pub struct Subscriber {
    fd: Rc<OwnedFd>,
    out: RefCell<Vec<u8>>,
    kick: AsyncEvent,
    dead: Cell<bool>,
}

async fn subscribe(state: &Rc<State>, fd: &Rc<OwnedFd>) {
    let sub = Rc::new(Subscriber {
        fd: fd.clone(),
        out: RefCell::new(Vec::new()),
        kick: AsyncEvent::default(),
        dead: Cell::new(false),
    });
    // snapshot first, deltas after
    sub.push(&json!({ "workspaces": workspaces_json(state) }));
    sub.push(&json!({ "windows": windows_json(state) }));
    // the config status is stateful: late subscribers get the last one
    if let Some(ev) = state.last_config_event.borrow().as_deref() {
        if let Ok(v) = serde_json::from_str::<Value>(ev) {
            sub.push(&v);
        }
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

// fan an event to every subscriber; senders never block on slow readers
pub fn emit<T: Serialize>(state: &Rc<State>, event: &T) {
    let subs = state.ipc_subs.borrow().clone();
    if subs.is_empty() {
        return;
    }
    let v = match serde_json::to_value(event) {
        Ok(v) => v,
        Err(_) => return,
    };
    for sub in subs {
        if !sub.dead.get() {
            sub.push(&v);
        }
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
