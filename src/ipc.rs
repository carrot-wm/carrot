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

// one dispatch path for the whole vocabulary; keybinds call the same
// dispatch_action the socket does
fn handle(state: &Rc<State>, line: &str) -> Result<Value, String> {
    if let Ok(action) = serde_json::from_str::<Action>(line) {
        dispatch_action(state, &action);
        return Ok(json!(true));
    }
    match serde_json::from_str::<String>(line).as_deref() {
        Ok("workspaces") => Ok(workspaces_json(state)),
        Ok("windows") => Ok(windows_json(state)),
        Ok("reload") => reload(state).map(|_| json!(true)),
        Ok(other) => Err(format!("unknown command \"{other}\"")),
        Err(_) => Err(format!("cannot parse \"{line}\"")),
    }
}

pub fn dispatch_action(state: &Rc<State>, action: &Action) {
    match action {
        Action::Workspace(n) => crate::tree::switch_workspace(state, *n),
        Action::WorkspaceRel(d) => crate::tree::switch_workspace_rel(state, *d),
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
        Action::SplitRatio(d) => {
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
        Action::Spawn(cmd) => spawn(state, cmd),
        Action::Quit => state.ring.stop(),
    }
}

// reap first so dead children never pile up as zombies, then detach the
// new one into its own session
fn spawn(state: &Rc<State>, cmd: &str) {
    use rustix::process::{WaitOptions, wait};
    while let Ok(Some(_)) = wait(WaitOptions::NOHANG) {}
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
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
        Err(e) => eprintln!("carrot: spawn \"{cmd}\": {e}"),
    }
}

pub fn reload(state: &Rc<State>) -> Result<(), String> {
    let cfg = crate::config::load()?;
    *state.config.borrow_mut() = Rc::new(cfg);
    let ws = crate::tree::active(state);
    crate::tree::relayout(state, &ws);
    crate::shell::layer::arrange(state);
    state.damage.trigger();
    emit(state, &json!({ "config-reloaded": true }));
    Ok(())
}

// -- queries --

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
