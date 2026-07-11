// a portal screencast: one pipewire client-node per session, fed from the
// present tail. frames re-compose via screencopy/window_capture, so a cast
// keeps working even where the live path scans out on a hardware plane.
// size changes (window resizes, mode changes) renegotiate the stream.

use crate::engine::SpawnedFuture;
use crate::pipewire::client_node::SourceNode;
use crate::pipewire::{PwConn, PwError};
use crate::state::State;
use crate::tree::{Window, workspace::Workspace};
use crate::util::{AsyncQueue, Time};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

const PROXY_ID: u32 = 2;
/// the daemon binds the global asynchronously (permissions and all);
/// this is how long a cast waits for its BoundId
const BIND_WAIT_NS: u64 = 5_000_000_000;

/// what a session's token restores; windows match by ident in-session and
/// by app id + title across restarts (idents reset with the compositor).
/// the tag shape is the on-disk token format - change it and old tokens die
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RestoreData {
    Output { name: String },
    Window { ident: u64, app_id: String, title: String },
    Workspace { index: usize },
}

pub enum Pick {
    /// a picker choice; the window must still exist
    Ident(u64),
    Restored(RestoreData),
}

enum Source {
    Output(String),
    Window { win: Weak<Window>, ident: u64, app_id: String, title: String },
    /// follows the workspace across outputs, streaming only while it
    /// is on glass; reachable through the picker, not the portal types
    Workspace(usize),
}

pub struct Cast {
    /// the portal session handle path; Session.Close tears us down by it
    pub session: String,
    /// the daemon-side global; Start hands this to the app
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub pos: (i32, i32),
    source: Source,
    /// paint the pointer into the frames (portal cursor mode EMBEDDED)
    cursor: bool,
    node: Rc<RefCell<SourceNode>>,
    /// presents can outpace the negotiated rate; feed() gates on this
    frame_ns: u64,
    last: Cell<u64>,
    /// the size the resizer was last asked for; feed() pushes once per change
    resize_target: Cell<(u32, u32)>,
    resize_req: Rc<AsyncQueue<(u32, u32)>>,
    /// the pump lost the daemon; the present tail sweeps us out
    dead: Rc<Cell<bool>>,
    _pump: SpawnedFuture<()>,
    _resizer: SpawnedFuture<()>,
}

/// connect, create the node, wait for the daemon to bind it, register with
/// the state. the returned cast is already live at the present tail.
pub async fn start(
    state: &Rc<State>,
    session: String,
    cursor: bool,
    pick: Pick,
) -> Result<Rc<Cast>, PwError> {
    let (source, width, height, fps, pos) = resolve(state, pick)?;
    let con = Rc::new(PwConn::connect(&state.ring)?);
    con.hello().await?;
    let node = Rc::new(RefCell::new(
        SourceNode::create(con.clone(), PROXY_ID, width, height, fps).await?,
    ));
    let dead = Rc::new(Cell::new(false));
    let failed: Rc<RefCell<Option<PwError>>> = Rc::new(RefCell::new(None));
    let pump = state.eng.spawn("cast pump", {
        let con = con.clone();
        let node = node.clone();
        let dead = dead.clone();
        let failed = failed.clone();
        async move {
            if let Err(e) = crate::pipewire::pump_node(&con, &node).await {
                eprintln!("carrot: cast: {e}");
                *failed.borrow_mut() = Some(e);
            }
            dead.set(true);
        }
    });
    // BoundId lands whenever the export completes, not inside our sync
    // round-trip; the pump collects it while we poll
    let deadline = Time::now().nsec() + BIND_WAIT_NS;
    let node_id = loop {
        if let Some(id) = node.borrow().bound_global {
            break id;
        }
        if dead.get() {
            return Err(failed.borrow_mut().take().unwrap_or(PwError::Closed));
        }
        let now = Time::now().nsec();
        if now >= deadline {
            return Err(PwError::Env("the daemon never bound the node"));
        }
        let _ = state.ring.timeout(Time::from_nsec(now + 50_000_000)).await;
    };
    let resize_req: Rc<AsyncQueue<(u32, u32)>> = Rc::new(AsyncQueue::default());
    let resizer = state.eng.spawn("cast resize", {
        let node = node.clone();
        let q = resize_req.clone();
        async move {
            loop {
                let (w, h) = q.pop().await;
                let _ = SourceNode::resize(&node, w, h).await;
            }
        }
    });
    let cast = Rc::new(Cast {
        session,
        node_id,
        width,
        height,
        pos,
        source,
        cursor,
        node,
        frame_ns: 1_000_000_000 / fps as u64,
        last: Cell::new(0),
        resize_target: Cell::new((width, height)),
        resize_req,
        dead,
        _pump: pump,
        _resizer: resizer,
    });
    state.casts.borrow_mut().push(cast.clone());
    Ok(cast)
}

fn resolve(state: &Rc<State>, pick: Pick) -> Result<(Source, u32, u32, u32, (i32, i32)), PwError> {
    match pick {
        Pick::Restored(RestoreData::Output { name }) => output_source(state, &name),
        Pick::Restored(RestoreData::Workspace { index }) => workspace_source(state, index),
        Pick::Ident(ident) => {
            let win = window_by_ident(state, ident)
                .ok_or(PwError::Env("the chosen window is gone"))?;
            window_source(state, win)
        }
        Pick::Restored(RestoreData::Window { ident, app_id, title }) => {
            // a stale token falls back to the focused window rather than
            // failing the whole cast; the picker will own this choice
            let win = find_window(state, ident, &app_id, &title)
                .or_else(|| crate::tree::focused_window(state))
                .ok_or(PwError::Env("no window to cast"))?;
            window_source(state, win)
        }
    }
}

/// a vanished output falls back to the focused one rather than failing
/// the restore outright
fn output_source(
    state: &Rc<State>,
    name: &str,
) -> Result<(Source, u32, u32, u32, (i32, i32)), PwError> {
    let d = state.display.borrow();
    let outs = d
        .as_ref()
        .map(|d| d.outputs.borrow().clone())
        .unwrap_or_default();
    let out = outs
        .iter()
        .find(|o| o.conn.name == name)
        .or_else(|| outs.get(state.focused_output.get()))
        .or_else(|| outs.first())
        .ok_or(PwError::Env("no output to cast"))?;
    Ok((
        Source::Output(out.conn.name.clone()),
        out.width,
        out.height,
        refresh(out),
        out.pos.get(),
    ))
}

/// a workspace sizes as its assigned output; the stream renegotiates if
/// it later shows somewhere else
fn workspace_source(
    state: &Rc<State>,
    index: usize,
) -> Result<(Source, u32, u32, u32, (i32, i32)), PwError> {
    let out_slot = state
        .workspaces
        .borrow()
        .get(index)
        .map(|ws| ws.output.get())
        .ok_or(PwError::Env("no such workspace"))?;
    let d = state.display.borrow();
    let outs = d
        .as_ref()
        .map(|d| d.outputs.borrow().clone())
        .unwrap_or_default();
    let out = outs
        .get(out_slot)
        .or_else(|| outs.first())
        .ok_or(PwError::Env("no output to cast"))?;
    Ok((
        Source::Workspace(index),
        out.width,
        out.height,
        refresh(out),
        out.pos.get(),
    ))
}

fn window_source(
    state: &Rc<State>,
    win: Rc<Window>,
) -> Result<(Source, u32, u32, u32, (i32, i32)), PwError> {
    let rect = win.draw_rect(state);
    if rect.is_empty() {
        return Err(PwError::Env("the window has no size yet"));
    }
    let fps = crate::tree::workspace_of(state, &win)
        .and_then(|ws| {
            let d = state.display.borrow();
            d.as_ref()?.outputs.borrow().get(ws.output.get()).map(refresh)
        })
        .unwrap_or(60);
    let source = Source::Window {
        win: Rc::downgrade(&win),
        ident: win.ident,
        app_id: win.app_id(),
        title: win.title(),
    };
    Ok((source, rect.width() as u32, rect.height() as u32, fps, (rect.x1, rect.y1)))
}

fn refresh(out: &Rc<crate::output::Output>) -> u32 {
    out.conn
        .pipe
        .borrow()
        .as_ref()
        .map(|p| p.mode.vrefresh)
        .unwrap_or(60)
        .max(1)
}

fn window_by_ident(state: &Rc<State>, ident: u64) -> Option<Rc<Window>> {
    let mut hit = None;
    for ws in state.workspaces.borrow().iter() {
        ws.for_each(|w| {
            if w.ident == ident {
                hit = Some(w.clone());
            }
        });
    }
    hit
}

fn find_window(state: &Rc<State>, ident: u64, app_id: &str, title: &str) -> Option<Rc<Window>> {
    if let Some(w) = window_by_ident(state, ident) {
        return Some(w);
    }
    // idents reset with the compositor; match identity by app id, narrowed
    // by title when it still fits
    let mut by_both = None;
    let mut by_app = None;
    for ws in state.workspaces.borrow().iter() {
        ws.for_each(|w| {
            if w.app_id() == app_id && !app_id.is_empty() {
                if w.title() == title && by_both.is_none() {
                    by_both = Some(w.clone());
                }
                if by_app.is_none() {
                    by_app = Some(w.clone());
                }
            }
        });
    }
    by_both.or(by_app)
}

/// the workspace the presented output currently shows
fn shown_workspace(state: &Rc<State>, name: &str) -> Option<Rc<Workspace>> {
    let d = state.display.borrow();
    let outs = d.as_ref()?.outputs.borrow();
    let out = outs.iter().find(|o| o.conn.name == name)?;
    state.workspaces.borrow().get(out.ws.get()).cloned()
}

/// present tail: the frame just shown is what casts of this output stream
pub fn output_presented(state: &Rc<State>, name: &str) {
    if state.casts.borrow().is_empty() {
        return;
    }
    let casts: Vec<Rc<Cast>> = state.casts.borrow().clone();
    let mut sweep = false;
    for c in &casts {
        if c.dead.get() {
            sweep = true;
            continue;
        }
        c.feed(state, name);
    }
    if sweep {
        state.casts.borrow_mut().retain(|c| !c.dead.get());
    }
}

impl Cast {
    /// what a fresh token for this cast should restore
    pub fn restore_data(&self) -> RestoreData {
        match &self.source {
            Source::Output(n) => RestoreData::Output { name: n.clone() },
            Source::Window { ident, app_id, title, .. } => RestoreData::Window {
                ident: *ident,
                app_id: app_id.clone(),
                title: title.clone(),
            },
            Source::Workspace(index) => RestoreData::Workspace { index: *index },
        }
    }

    fn feed(&self, state: &Rc<State>, presented: &str) {
        enum Cap {
            Out(usize),
            Win(Rc<Window>),
        }
        let (cap, w, h) = match &self.source {
            Source::Output(name) => {
                if name != presented {
                    return;
                }
                let Some((idx, w, h)) = crate::output::output_geometry(state, name) else {
                    return;
                };
                (Cap::Out(idx), w, h)
            }
            Source::Window { win, .. } => {
                let Some(win) = win.upgrade() else {
                    self.dead.set(true);
                    return;
                };
                if !win.surface().mapped.get() {
                    return;
                }
                // foreground gate: stream only while the window's workspace
                // is the one on glass on the presented output
                let Some(ws) = crate::tree::workspace_of(state, &win) else {
                    return;
                };
                let Some(shown) = shown_workspace(state, presented) else {
                    return;
                };
                if !Rc::ptr_eq(&ws, &shown) {
                    return;
                }
                let rect = win.draw_rect(state);
                if rect.is_empty() {
                    return;
                }
                (Cap::Win(win), rect.width() as u32, rect.height() as u32)
            }
            Source::Workspace(index) => {
                let Some(ws) = state.workspaces.borrow().get(*index).cloned() else {
                    self.dead.set(true);
                    return;
                };
                let Some(shown) = shown_workspace(state, presented) else {
                    return;
                };
                if !Rc::ptr_eq(&ws, &shown) {
                    return;
                }
                let Some((idx, w, h)) = crate::output::output_geometry(state, presented) else {
                    return;
                };
                (Cap::Out(idx), w, h)
            }
        };
        {
            let n = self.node.borrow();
            if w != n.width || h != n.height {
                if self.resize_target.get() != (w, h) {
                    self.resize_target.set((w, h));
                    self.resize_req.push((w, h));
                }
                return;
            }
            if !n.ready() {
                return;
            }
        }
        let now = Time::now().nsec();
        if now.saturating_sub(self.last.get()) < self.frame_ns - self.frame_ns / 10 {
            return;
        }
        let px = match cap {
            Cap::Out(idx) => {
                let Some(region) = crate::rect::Rect::new_sized(0, 0, w as i32, h as i32) else {
                    return;
                };
                crate::output::screencopy(state, idx, region, self.cursor)
            }
            Cap::Win(win) => crate::output::window_capture(state, &win),
        };
        let Some(px) = px else { return };
        self.node.borrow_mut().produce(|dst, _| {
            let n = px.len().min(dst.len());
            dst[..n].copy_from_slice(&px[..n]);
        });
        self.last.set(now);
    }
}
