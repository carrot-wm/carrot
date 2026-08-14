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
    /// follows the workspace across outputs; reachable through the
    /// picker, not the portal types
    Workspace(usize),
}

enum Cap {
    Out(usize),
    Ws(usize),
    Win(Rc<Window>),
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
    /// the source's own rate (its output's vrefresh); pacing derives
    /// from this and the config ceiling at each use, so a reload lands
    /// on running casts
    source_fps: u32,
    last: Cell<u64>,
    /// the size the resizer was last asked for; feed() pushes once per change
    resize_target: Cell<(u32, u32)>,
    resize_req: Rc<AsyncQueue<(u32, u32)>>,
    /// the pump lost the daemon; the present tail sweeps us out
    dead: Rc<Cell<bool>>,
    /// a hidden-source commit landed; the tick owes a frame
    dirty: Cell<bool>,
    /// a present-tail serve is in flight; later presents coalesce
    serving: Cell<bool>,
    /// the detached serve task; held so it survives, dies with the cast
    serve_task: Cell<Option<SpawnedFuture<()>>>,
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
                // coalesce a burst to its last size: replaying every
                // intermediate step renegotiates the stream per step
                // and stalls it for the whole tail of the drag
                let (mut w, mut h) = q.pop().await;
                while let Some(next) = q.try_pop() {
                    (w, h) = next;
                }
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
        source_fps: fps,
        last: Cell::new(0),
        resize_target: Cell::new((width, height)),
        resize_req,
        dead,
        dirty: Cell::new(false),
        serving: Cell::new(false),
        serve_task: Cell::new(None),
        _pump: pump,
        _resizer: resizer,
    });
    state.casts.borrow_mut().push(cast.clone());
    // a source born off-glass owes the tick its first frames: no glass
    // change or visible commit will ever come to mark it dirty
    if !cast.on_glass(state) {
        crate::trace!("cast: born hidden, tick bootstrap");
        cast.dirty.set(true);
        state.cast_kick.trigger();
    }
    let tick = state.cast_tick.take();
    if tick.is_some() {
        state.cast_tick.set(tick);
    } else {
        // first cast ever: the tick task lives for the compositor's life
        state
            .cast_tick
            .set(Some(state.eng.spawn("cast tick", tick_loop(state.clone()))));
    }
    Ok(cast)
}

/// a commit landed: casts sourcing it while off glass go dirty and the
/// tick owes them a frame
pub fn surface_committed(state: &Rc<State>, surface: &Rc<crate::surface::WlSurface>) {
    if state.casts.borrow().is_empty() {
        return;
    }
    let root = surface.get_root();
    // any workspace, not just the active one: a hidden cast's source is
    // off-glass by definition, so the active-only lookup never finds it
    let win = crate::tree::window_for_surface_any(state, &root);
    crate::trace!("cast: commit root={} win={}", root.id, win.is_some());
    let mut kick = false;
    for c in state.casts.borrow().iter() {
        if c.dead.get() || c.dirty.get() {
            kick |= c.dirty.get();
            continue;
        }
        let hit = match (&c.source, &win) {
            (Source::Window { win: w, .. }, Some(cw)) => {
                w.upgrade().is_some_and(|a| Rc::ptr_eq(&a, cw))
            }
            (Source::Workspace(i), Some(cw)) => {
                match (crate::tree::workspace_of(state, cw), state.workspaces.borrow().get(*i)) {
                    (Some(a), Some(b)) => Rc::ptr_eq(&a, b),
                    _ => false,
                }
            }
            _ => false,
        };
        if hit {
            crate::trace!("cast: src commit glass={} dirty={}", c.on_glass(state), c.dirty.get());
        }
        if hit && !c.on_glass(state) {
            c.dirty.set(true);
            kick = true;
        }
    }
    if kick {
        state.cast_kick.trigger();
    }
}

/// the glass changed: a source that just went hidden owes the tick a
/// bootstrap frame. its client may hold a frame callback nothing else
/// will ever fire - presents no longer walk that workspace, and the
/// commit-driven tick can't start without the commit that callback gates
pub fn glass_changed(state: &Rc<State>) {
    if state.casts.borrow().is_empty() {
        return;
    }
    let mut kick = false;
    for c in state.casts.borrow().iter() {
        crate::trace!(
            "cast: glass change dead={} dirty={} glass={}",
            c.dead.get(),
            c.dirty.get(),
            c.on_glass(state)
        );
        if !c.dead.get() && !c.dirty.get() && !c.on_glass(state) {
            c.dirty.set(true);
            kick = true;
        }
    }
    if kick {
        state.cast_kick.trigger();
    }
}

/// drain dirty hidden casts at their own rate; parks on the kick event
async fn tick_loop(state: Rc<State>) {
    loop {
        state.cast_kick.triggered().await;
        loop {
            let casts: Vec<Rc<Cast>> = state.casts.borrow().clone();
            let mut earliest: Option<u64> = None;
            let mut sweep = false;
            for c in casts.iter().filter(|c| c.dirty.get()) {
                if c.dead.get() {
                    sweep = true;
                    continue;
                }
                if c.on_glass(&state) {
                    c.dirty.set(false);
                    continue;
                }
                let ns = c.hidden_frame_ns(&state);
                let due = c.last.get() + ns - ns / 10;
                if Time::now().nsec() < due {
                    earliest = Some(earliest.map_or(due, |e: u64| e.min(due)));
                    continue;
                }
                // a present-tail serve can still be mid-flight from before
                // the source left the glass; feeding over it would race
                // the node's buffers under that serve's capture await
                let Some(_guard) = c.begin_serve() else {
                    let retry = Time::now().nsec() + 50_000_000;
                    earliest = Some(earliest.map_or(retry, |e: u64| e.min(retry)));
                    continue;
                };
                // clear before the serve: a commit landing during the
                // capture await re-marks and earns its own pass, where
                // clearing after would swallow it with the frame it missed
                c.dirty.set(false);
                if !c.feed_hidden(&state).await {
                    // stream blocked (negotiation, mid-resize, consumer
                    // holding every buffer): stay dirty and poll again
                    // shortly, or the wakeup is lost
                    c.dirty.set(true);
                    let retry = Time::now().nsec() + 50_000_000;
                    earliest = Some(earliest.map_or(retry, |e: u64| e.min(retry)));
                }
            }
            if sweep {
                state.casts.borrow_mut().retain(|c| !c.dead.get());
            }
            match earliest {
                Some(t) => {
                    let _ = state.ring.timeout(Time::from_nsec(t)).await;
                }
                None => break,
            }
        }
    }
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
            // a stale token must NOT fall back to whatever is focused:
            // that streams content nobody consented to. fail instead;
            // the portal falls back to a fresh consent flow
            let win = find_window(state, ident, &app_id, &title)
                .ok_or(PwError::Env("the restored window is gone"))?;
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
    // picker stdout and restore data are external input
    if index >= crate::tree::MAX_WORKSPACES {
        return Err(PwError::Env("no such workspace"));
    }
    // sharing is a demand site like switching: a workspace the session
    // never visited exists from the moment someone streams it
    crate::tree::ensure_workspace(state, index);
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
    let now = Time::now().nsec();
    for c in &casts {
        if c.dead.get() {
            sweep = true;
            continue;
        }
        // the cheap gates run before any task spawns: at 480Hz presents
        // the spawn itself (plus its string and vec clones) was real
        // churn, and most presents have nothing for a cast to do
        let ns = c.frame_ns(state);
        if now.saturating_sub(c.last.get()) < ns - ns / 10 {
            continue;
        }
        {
            let n = c.node.borrow();
            if !n.ready() || !n.wants_frame() {
                continue;
            }
        }
        // the serve runs as its own task: the fence await inside must
        // never hold up the present tail or the input reader. one serve
        // per cast at a time; presents during it coalesce into the next
        let Some(guard) = c.begin_serve() else {
            continue;
        };
        let st = state.clone();
        let cc = c.clone();
        let n = name.to_string();
        c.serve_task.set(Some(state.eng.spawn("cast serve", async move {
            let _guard = guard;
            cc.feed(&st, &n).await;
        })));
    }
    if sweep {
        state.casts.borrow_mut().retain(|c| !c.dead.get());
    }
}

/// exclusive right to run one serve for a cast; dropping it on any path,
/// including task cancellation, reopens the slot
struct ServeGuard(Rc<Cast>);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        self.0.serving.set(false);
    }
}

impl Cast {
    /// stream pacing: the source rate under the config ceiling. panel
    /// refresh is not a stream rate - unset caps at 60, and a prompt
    /// consumer must never pull captures at present rate
    fn frame_ns(&self, state: &Rc<State>) -> u64 {
        let cap = state.config.borrow().screencast.max_fps.unwrap_or(60).max(1);
        1_000_000_000 / u64::from(self.source_fps.clamp(1, cap))
    }

    /// hidden feeds may pace slower still, per config
    fn hidden_frame_ns(&self, state: &Rc<State>) -> u64 {
        let ns = self.frame_ns(state);
        match state.config.borrow().screencast.hidden_max_fps {
            Some(f) => ns.max(1_000_000_000 / u64::from(f.max(1))),
            None => ns,
        }
    }

    /// claim the single serve slot, or None while another serve is mid-
    /// flight. every path that can reach push_frame must hold this: two
    /// concurrent serves race the node's buffers under the capture await
    fn begin_serve(self: &Rc<Self>) -> Option<ServeGuard> {
        if self.serving.get() {
            return None;
        }
        self.serving.set(true);
        Some(ServeGuard(self.clone()))
    }

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

    async fn feed(&self, state: &Rc<State>, presented: &str) {
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
                // mid-switch the glass blends two workspaces; stream the
                // rest compose until the animation settles
                if crate::output::switching(state, presented) {
                    (Cap::Ws(*index), w, h)
                } else {
                    (Cap::Out(idx), w, h)
                }
            }
        };
        self.push_frame(state, cap, w, h).await;
    }

    /// false = the stream couldn't take a frame right now (negotiation,
    /// mid-resize, failed capture); the tick retries those
    async fn push_frame(&self, state: &Rc<State>, cap: Cap, w: u32, h: u32) -> bool {
        {
            let n = self.node.borrow();
            if w != n.width || h != n.height {
                crate::trace!("cast: size want {}x{} node {}x{}", w, h, n.width, n.height);
                if self.resize_target.get() != (w, h) {
                    self.resize_target.set((w, h));
                    self.resize_req.push((w, h));
                }
                return false;
            }
            if !n.ready() {
                crate::trace!("cast: node not ready");
                return false;
            }
        }
        // the rate gate comes first: a consumer that dequeues promptly
        // must never bypass it, or the feed runs at present rate
        let now = Time::now().nsec();
        let ns = self.frame_ns(state);
        if now.saturating_sub(self.last.get()) < ns - ns / 10 {
            // fresh enough counts as fed
            return true;
        }
        // the consumer still holds the last frame: a render now is
        // work nobody dequeues, and at present cadence that work is
        // the whole-session stall. its dequeue rate paces us - but the
        // content was NOT delivered, so this reports blocked and the
        // tick keeps the cast dirty: counting it as fed used to eat the
        // final frame of every burst
        if !self.node.borrow().wants_frame() {
            return false;
        }
        // the sink runs over the capture's mapped readback: one copy,
        // straight into the pipewire buffer, no frame-sized Vec between
        let sink = |px: &[u8], cw: u32, ch: u32| self.deliver(cw, ch, px);
        let delivered = match cap {
            Cap::Out(idx) => {
                let Some(region) = crate::rect::Rect::new_sized(0, 0, w as i32, h as i32) else {
                    return false;
                };
                crate::output::screencopy(state, idx, region, self.cursor, crate::config::CaptureKind::Video, sink).await
            }
            Cap::Ws(index) => crate::output::workspace_copy(state, index, sink).await,
            Cap::Win(win) => crate::output::window_capture(state, &win, sink).await,
        };
        crate::trace!("cast: frame {:?} t={}", delivered, Time::now().nsec());
        if !matches!(delivered, Some(true)) {
            return false;
        }
        self.last.set(now);
        true
    }

    /// revalidate the node against the geometry the pixels were rendered
    /// for (the pump and the resizer were free to renegotiate during the
    /// capture await), then deliver. a changed node keeps the frame dirty
    /// and the next serve captures at the new size
    fn deliver(&self, w: u32, h: u32, px: &[u8]) -> bool {
        let mut n = self.node.borrow_mut();
        if w != n.width || h != n.height || !n.ready() {
            if crate::output::cap_diag() {
                eprintln!(
                    "carrot: cast: frame dropped, captured {}x{} vs node {}x{} ready={}",
                    w,
                    h,
                    n.width,
                    n.height,
                    n.ready()
                );
            }
            return false;
        }
        n.produce(|dst, _| {
            let m = px.len().min(dst.len());
            dst[..m].copy_from_slice(&px[..m]);
        })
    }

    /// the present tail composes this source right now; the tick stays out
    fn on_glass(&self, state: &Rc<State>) -> bool {
        match &self.source {
            Source::Output(_) => true,
            Source::Window { win, .. } => match win.upgrade() {
                Some(win) => match crate::tree::workspace_of(state, &win) {
                    Some(ws) => ws_shown(state, &ws),
                    None => true,
                },
                None => true,
            },
            Source::Workspace(index) => match state.workspaces.borrow().get(*index) {
                Some(ws) => ws_shown(state, ws),
                None => true,
            },
        }
    }

    /// tick-driven feed for a source that is off glass; false = blocked,
    /// the caller keeps it dirty and retries. the heartbeat fires either
    /// way so the hidden clients stay alive through stream negotiation
    async fn feed_hidden(&self, state: &Rc<State>) -> bool {
        let ms = (Time::now().nsec() / 1_000_000) as u32;
        match &self.source {
            Source::Output(_) => true,
            Source::Window { win, .. } => {
                let Some(win) = win.upgrade() else {
                    self.dead.set(true);
                    return true;
                };
                if !win.surface().mapped.get() {
                    return true;
                }
                let rect = win.draw_rect(state);
                if rect.is_empty() {
                    return true;
                }
                let fed = self
                    .push_frame(state, Cap::Win(win.clone()), rect.width() as u32, rect.height() as u32)
                    .await;
                fire_tree(&win.surface(), ms);
                fed
            }
            Source::Workspace(index) => {
                let Some(ws) = state.workspaces.borrow().get(*index).cloned() else {
                    self.dead.set(true);
                    return true;
                };
                let (w, h) = {
                    let d = state.display.borrow();
                    let Some(d) = d.as_ref() else { return true };
                    let outs = d.outputs.borrow();
                    let Some(out) = outs.get(ws.output.get()).or_else(|| outs.first()) else {
                        return true;
                    };
                    (out.width, out.height)
                };
                crate::trace!("cast: tick ws {} feed", index);
                let fed = self.push_frame(state, Cap::Ws(*index), w, h).await;
                ws.for_each(|win| fire_tree(&win.surface(), ms));
                fed
            }
        }
    }
}

/// is this workspace the one on glass on its assigned output
fn ws_shown(state: &Rc<State>, ws: &Rc<Workspace>) -> bool {
    let d = state.display.borrow();
    let Some(d) = d.as_ref() else { return false };
    let outs = d.outputs.borrow();
    let Some(out) = outs.get(ws.output.get()) else { return false };
    state
        .workspaces
        .borrow()
        .get(out.ws.get())
        .is_some_and(|shown| Rc::ptr_eq(shown, ws))
}

/// frame callbacks for a whole surface tree: the tick is the only
/// heartbeat a fully hidden client has
fn fire_tree(s: &Rc<crate::surface::WlSurface>, ms: u32) {
    s.fire_frame_callbacks(ms);
    let children = s.children.borrow();
    if let Some(ch) = &*children {
        let subs: Vec<_> = ch
            .below
            .iter()
            .chain(ch.above.iter())
            .filter(|e| !e.pending.get())
            .map(|e| e.sub.clone())
            .collect();
        drop(children);
        for sub in subs {
            fire_tree(&sub.surface, ms);
        }
    }
}
