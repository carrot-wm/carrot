// the x window manager. bring-up claims the wm selections, redirects the
// root and advertises a single fake desktop; xwayland holds regular x
// clients until a wm exists, so this must happen before anything runs.
//
// lifecycle: CreateNotify tracks, MapRequest is always granted, and a
// window enters the tree only once it is both x-mapped and carrying a
// committed buffer on its paired wl surface. override-redirect windows
// go straight to the float stack and place themselves.

use crate::carrotconx::conn::{Xcon, XconError};
use crate::carrotconx::wire;
use crate::engine::SpawnedFuture;
use crate::protocol::data_device::{SelectionSource, same_source};
use crate::rect::Rect;
use crate::state::State;
use crate::tree::{Window, WindowKind};
use crate::util::Time;
use crate::xwayland::{X11SelectionSource, XAtoms, XWindow, Xwayland, XwmEvent};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::rc::Rc;

// root event mask: SubstructureRedirect | SubstructureNotify | PropertyChange
const ROOT_EVENTS: u32 = 0x0010_0000 | 0x0008_0000 | 0x0040_0000;
// per managed window, PropertyChange keeps title/class/hints fresh
const WINDOW_EVENTS: u32 = 0x0040_0000;
const EVENT_MASK_BIT: u8 = 11;
const INPUT_ONLY: u16 = 2;
const REDIRECT_MANUAL: u8 = 1;
const CURRENT_TIME: u32 = 0;
const PROP_REPLACE: u8 = 0;
const PROP_APPEND: u8 = 2;
const ATOM: u32 = 4;
const STRING: u32 = 31;
const WINDOW: u32 = 33;
// WM_SIZE_HINTS flags
const P_MIN_SIZE: u32 = 1 << 4;
const P_MAX_SIZE: u32 = 1 << 5;
// _NET_WM_STATE client message actions
const STATE_REMOVE: u32 = 0;
const STATE_ADD: u32 = 1;
const STATE_TOGGLE: u32 = 2;
// PRIMARY is predefined atom 1
const PRIMARY: u32 = 1;
// xfixes: SetSelectionOwner | SelectionWindowDestroy | SelectionClientClose
const SEL_EVENTS: u32 = 7;
// a paste bigger than this gets refused rather than incr-streamed
const MAX_TRANSFER: usize = 4 << 20;
// a single x request tops out at 256k; append properties in 64k slices
const PROP_SLICE: usize = 64 << 10;
const FETCH_TIMEOUT_NS: u64 = 5_000_000_000;

struct Xwm {
    state: Rc<State>,
    xw: Rc<Xwayland>,
    c: Rc<Xcon>,
    atoms: Rc<XAtoms>,
    // xwayland's own pairing atoms
    wl_surface_serial: u32,
    wl_surface_id: u32,
    windows: RefCell<HashMap<u32, Rc<XWindow>>>,
    by_serial: RefCell<HashMap<u64, u32>>,
    // managed xids in map order, mirrored to _NET_CLIENT_LIST on the root
    client_list: RefCell<Vec<u32>>,
    // -- selection bridging --
    sel_atoms: SelAtoms,
    // input-only window that owns our x-side selection claims
    selwin: u32,
    clipboard: SelState,
    primary_sel: SelState,
    // detached transfer tasks, self-pruning on completion
    transfers: Rc<RefCell<HashMap<u64, SpawnedFuture<()>>>>,
    transfer_next: Cell<u64>,
}

struct SelAtoms {
    clipboard: u32,
    targets: u32,
    text: u32,
    incr: u32,
    utf8: u32,
}

// an x->wayland conversion in flight; the fd eofs on failure or timeout
struct Fetch {
    fd: OwnedFd,
    deadline: Time,
}

// one side of the bridge: CLIPBOARD or PRIMARY
struct SelState {
    atom: u32,
    primary: bool,
    // own landing pads per selection, targets and data kept apart, so
    // concurrent conversions can't overwrite each other's replies
    prop_targets: u32,
    prop_data: u32,
    // the provider we installed on the seat, for is-this-ours checks
    source: RefCell<Option<Rc<X11SelectionSource>>>,
    // outstanding TARGETS conversions; only the newest reply counts, and
    // an owner that never answers expires instead of wedging the count
    targets_pending: Cell<u32>,
    targets_deadline: Cell<Option<Time>>,
    fetch: RefCell<Option<Fetch>>,
    // fetches wait their turn; the landing pad property can't interleave
    waiting: RefCell<VecDeque<(String, OwnedFd)>>,
}

impl SelState {
    fn new(atom: u32, primary: bool, prop_targets: u32, prop_data: u32) -> SelState {
        SelState {
            atom,
            primary,
            prop_targets,
            prop_data,
            source: RefCell::new(None),
            targets_pending: Cell::new(0),
            targets_deadline: Cell::new(None),
            fetch: RefCell::new(None),
            waiting: RefCell::new(VecDeque::new()),
        }
    }
}

// the fixed text targets; '/' atoms pass through by name at the call sites
fn target_to_mime(a: &SelAtoms, target: u32) -> Option<&'static str> {
    if target == a.utf8 {
        Some("text/plain;charset=utf-8")
    } else if target == STRING || target == a.text {
        Some("text/plain")
    } else {
        None
    }
}

fn mime_to_target(a: &SelAtoms, mime: &str) -> Option<u32> {
    match mime {
        "text/plain;charset=utf-8" => Some(a.utf8),
        "text/plain" => Some(STRING),
        _ => None,
    }
}

// StructureNotify, for synthetic ConfigureNotify deliveries
const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
const NORMAL_STATE: u32 = 1;
const WITHDRAWN_STATE: u32 = 0;

pub async fn run(state: Rc<State>, xw: Rc<Xwayland>, xcon: Rc<Xcon>) {
    if let Err(e) = bring_up(&xcon).await {
        eprintln!("carrot: xwm bring-up failed: {e}");
        return;
    }
    let (atoms, wl_surface_serial, wl_surface_id) = {
        let a = async {
            Ok::<_, XconError>((
                intern_atoms(&xcon).await?,
                xcon.intern("WL_SURFACE_SERIAL").await?,
                xcon.intern("WL_SURFACE_ID").await?,
            ))
        };
        match a.await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("carrot: xwm atoms: {e}");
                return;
            }
        }
    };
    let atoms = Rc::new(atoms);
    *xw.atoms.borrow_mut() = Some(atoms.clone());
    let (sel_atoms, cb_props, prim_props) = {
        let a = async {
            Ok::<_, XconError>((
                SelAtoms {
                    clipboard: xcon.intern("CLIPBOARD").await?,
                    targets: xcon.intern("TARGETS").await?,
                    text: xcon.intern("TEXT").await?,
                    incr: xcon.intern("INCR").await?,
                    utf8: atoms.utf8_string,
                },
                (
                    xcon.intern("_CARROT_CLIPBOARD_TARGETS").await?,
                    xcon.intern("_CARROT_CLIPBOARD").await?,
                ),
                (
                    xcon.intern("_CARROT_PRIMARY_TARGETS").await?,
                    xcon.intern("_CARROT_PRIMARY").await?,
                ),
            ))
        };
        match a.await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("carrot: xwm selection atoms: {e}");
                return;
            }
        }
    };
    // the selection window fields conversions and watches both selections
    let selwin = xcon.alloc_xid();
    xcon.send(|b| {
        wire::create_window(b, 0, selwin, xcon.root, -1, -1, 1, 1, 0, INPUT_ONLY, 0, &[])
    });
    let xfixes = xcon.ext.borrow().as_ref().map(|e| e.xfixes).unwrap_or(0);
    for sel in [sel_atoms.clipboard, PRIMARY] {
        xcon.send(|b| wire::xfixes_select_selection_input(b, xfixes, selwin, sel, SEL_EVENTS));
    }
    println!("carrot: xwm managing :{}", xw.display);
    let wm = Xwm {
        state,
        xw: xw.clone(),
        c: xcon,
        atoms,
        wl_surface_serial,
        wl_surface_id,
        windows: RefCell::new(HashMap::new()),
        by_serial: RefCell::new(HashMap::new()),
        client_list: RefCell::new(Vec::new()),
        clipboard: SelState::new(sel_atoms.clipboard, false, cb_props.0, cb_props.1),
        primary_sel: SelState::new(PRIMARY, true, prim_props.0, prim_props.1),
        sel_atoms,
        selwin,
        transfers: Rc::new(RefCell::new(HashMap::new())),
        transfer_next: Cell::new(0),
    };
    loop {
        let ev = match wm.next_deadline() {
            Some(d) => pop_or_timeout(&xw, &wm.state.ring, d).await,
            None => Some(xw.queue.pop().await),
        };
        match ev {
            Some(XwmEvent::X(ev)) => wm.x_event(ev).await,
            Some(XwmEvent::Commit(serial)) => wm.commit(serial),
            Some(XwmEvent::WlSelection { primary }) => wm.wl_selection(primary),
            Some(XwmEvent::XFetch { primary, mime, fd }) => wm.x_fetch(primary, mime, fd).await,
            // a fetch deadline passed
            None => wm.expire_fetches().await,
        }
    }
}

// wake for the earliest fetch deadline while the queue is idle
async fn pop_or_timeout(
    xw: &Xwayland,
    ring: &crate::uring::Ring,
    deadline: Time,
) -> Option<XwmEvent> {
    use std::future::Future as _;
    use std::task::Poll;
    let mut pop = xw.queue.pop();
    let mut timer = std::pin::pin!(ring.timeout(deadline));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(ev) = std::pin::Pin::new(&mut pop).poll(cx) {
            return Poll::Ready(Some(ev));
        }
        match timer.as_mut().poll(cx) {
            Poll::Ready(_) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

async fn intern_atoms(c: &Rc<Xcon>) -> Result<XAtoms, XconError> {
    Ok(XAtoms {
        wm_protocols: c.intern("WM_PROTOCOLS").await?,
        wm_delete_window: c.intern("WM_DELETE_WINDOW").await?,
        wm_take_focus: c.intern("WM_TAKE_FOCUS").await?,
        wm_hints: c.intern("WM_HINTS").await?,
        wm_normal_hints: c.intern("WM_NORMAL_HINTS").await?,
        wm_name: c.intern("WM_NAME").await?,
        net_wm_name: c.intern("_NET_WM_NAME").await?,
        utf8_string: c.intern("UTF8_STRING").await?,
        wm_class: c.intern("WM_CLASS").await?,
        net_wm_state: c.intern("_NET_WM_STATE").await?,
        net_wm_state_fullscreen: c.intern("_NET_WM_STATE_FULLSCREEN").await?,
        net_wm_state_modal: c.intern("_NET_WM_STATE_MODAL").await?,
        net_wm_window_type: c.intern("_NET_WM_WINDOW_TYPE").await?,
        type_dialog: c.intern("_NET_WM_WINDOW_TYPE_DIALOG").await?,
        type_utility: c.intern("_NET_WM_WINDOW_TYPE_UTILITY").await?,
        type_toolbar: c.intern("_NET_WM_WINDOW_TYPE_TOOLBAR").await?,
        type_splash: c.intern("_NET_WM_WINDOW_TYPE_SPLASH").await?,
        net_active_window: c.intern("_NET_ACTIVE_WINDOW").await?,
        net_client_list: c.intern("_NET_CLIENT_LIST").await?,
        wm_transient_for: c.intern("WM_TRANSIENT_FOR").await?,
        wm_state: c.intern("WM_STATE").await?,
    })
}

impl Xwm {
    async fn x_event(&self, ev: wire::XEvent) {
        use wire::XEvent as E;
        crate::trace!("xwm: {:?}", ev);
        match ev {
            E::CreateNotify { window, x, y, width, height, override_redirect, .. } => {
                let xwin = Rc::new(XWindow {
                    xid: window,
                    xcon: self.c.clone(),
                    override_redirect: Cell::new(override_redirect),
                    x_mapped: Cell::new(false),
                    serial: Cell::new(0),
                    surface: RefCell::new(None),
                    window: RefCell::new(None),
                    geo: Cell::new(Rect::new_sized_saturating(
                        x as i32,
                        y as i32,
                        width as i32,
                        height as i32,
                    )),
                    title: RefCell::new(String::new()),
                    class: RefCell::new(String::new()),
                    input_hint: Cell::new(true),
                    delete_window: Cell::new(false),
                    min_size: Cell::new((0, 0)),
                    max_size: Cell::new((0, 0)),
                    modal: Cell::new(false),
                    float_type: Cell::new(false),
                    fullscreen_requested: Cell::new(false),
                    transient_for: Cell::new(0),
                    atoms: RefCell::new(Some(self.atoms.clone())),
                });
                self.c.send(|b| {
                    wire::change_window_attributes(b, window, &[(EVENT_MASK_BIT, WINDOW_EVENTS)])
                });
                self.windows.borrow_mut().insert(window, xwin);
            }
            E::DestroyNotify { window, .. } => {
                let gone = self.windows.borrow_mut().remove(&window);
                if let Some(xwin) = gone {
                    let serial = xwin.serial.get();
                    if serial != 0 {
                        self.by_serial.borrow_mut().remove(&serial);
                        self.xw.serials.borrow_mut().remove(&serial);
                    }
                    xwin.x_mapped.set(false);
                    self.gate(&xwin);
                }
            }
            E::MapRequest { window, .. } => {
                // always granted; the tree decides placement after the
                // buffer shows up. everything is read once here, property
                // notifies keep it fresh afterwards
                if let Some(xwin) = self.win(window) {
                    self.refresh_all(&xwin).await;
                }
                self.set_wm_state(window, NORMAL_STATE);
                self.c.send(|b| wire::map_window(b, window));
            }
            E::MapNotify { window, override_redirect, .. } => {
                if let Some(xwin) = self.win(window) {
                    xwin.override_redirect.set(override_redirect);
                    // overrides skip MapRequest; this is their first read
                    if override_redirect {
                        self.refresh_all(&xwin).await;
                    }
                    xwin.x_mapped.set(true);
                    self.gate(&xwin);
                }
            }
            E::UnmapNotify { window, .. } => {
                if let Some(xwin) = self.win(window) {
                    xwin.x_mapped.set(false);
                    self.set_wm_state(window, WITHDRAWN_STATE);
                    self.gate(&xwin);
                }
            }
            E::ConfigureNotify { window, x, y, width, height, override_redirect, .. } => {
                if let Some(xwin) = self.win(window) {
                    xwin.override_redirect.set(override_redirect);
                    let r = Rect::new_sized_saturating(
                        x as i32,
                        y as i32,
                        width as i32,
                        height as i32,
                    );
                    xwin.geo.set(r);
                    // overrides own their position; follow them live
                    let win = xwin.window.borrow().clone();
                    if let Some(win) = win {
                        if win.floating.get() {
                            win.rect.set(r);
                            self.state.damage.trigger();
                        }
                    }
                }
            }
            E::ConfigureRequest { window, x, y, width, height, value_mask, .. } => {
                let xwin = self.win(window);
                let tiled = xwin
                    .as_ref()
                    .and_then(|xw| xw.window.borrow().clone())
                    .is_some_and(|w| !w.floating.get());
                if tiled {
                    // the tile is the answer; a synthetic notify keeps the
                    // client's view of the world honest
                    let r = xwin.unwrap().window.borrow().as_ref().unwrap().rect.get();
                    let ev = wire::encode_configure_notify(
                        window,
                        r.x1 as i16,
                        r.y1 as i16,
                        r.width() as u16,
                        r.height() as u16,
                        0,
                    );
                    self.c
                        .send(|b| wire::send_event(b, false, window, STRUCTURE_NOTIFY, &ev));
                } else {
                    let mut values: Vec<(u8, u32)> = Vec::new();
                    if value_mask & 1 != 0 {
                        values.push((0, x as u32));
                    }
                    if value_mask & 2 != 0 {
                        values.push((1, y as u32));
                    }
                    if value_mask & 4 != 0 {
                        values.push((2, width as u32));
                    }
                    if value_mask & 8 != 0 {
                        values.push((3, height as u32));
                    }
                    if !values.is_empty() {
                        self.c.send(|b| wire::configure_window(b, window, &values));
                    }
                }
            }
            E::ClientMessage { window, ty, data, .. } => {
                if ty == self.wl_surface_serial {
                    let serial = data[0] as u64 | (data[1] as u64) << 32;
                    if let Some(xwin) = self.win(window) {
                        xwin.serial.set(serial);
                        self.by_serial.borrow_mut().insert(serial, window);
                        self.try_pair(&xwin);
                        self.gate(&xwin);
                    }
                } else if ty == self.wl_surface_id {
                    // pre-shell fallback; modern xwayland sends serials
                    let id = crate::protocol::ObjectId(data[0]);
                    let surface = self
                        .xw
                        .client
                        .borrow()
                        .as_ref()
                        .and_then(|c| c.objects.surface(id));
                    if let (Some(xwin), Some(s)) = (self.win(window), surface) {
                        *xwin.surface.borrow_mut() = Some(s);
                        self.gate(&xwin);
                    }
                } else if ty == self.atoms.net_wm_state {
                    if let Some(xwin) = self.win(window) {
                        self.net_wm_state_message(&xwin, &data);
                    }
                }
            }
            E::PropertyNotify { window, atom, .. } => {
                if let Some(xwin) = self.win(window) {
                    self.refresh_prop(&xwin, atom).await;
                }
            }
            E::XfixesSelectionNotify { selection, owner, .. } => {
                self.xfixes_selection(selection, owner);
            }
            E::SelectionNotify { selection, target, property, .. } => {
                self.selection_notify(selection, target, property).await;
            }
            E::SelectionRequest { time, requestor, selection, target, property, .. } => {
                self.selection_request(time, requestor, selection, target, property)
                    .await;
            }
            // carrot is the sole focus authority; x-side focus reports are
            // trace-logged above and otherwise dropped
            E::FocusIn { .. } => {}
        }
    }

    // -- selection bridging --

    fn sel(&self, primary: bool) -> &SelState {
        if primary { &self.primary_sel } else { &self.clipboard }
    }

    fn sel_by_atom(&self, atom: u32) -> Option<&SelState> {
        if atom == self.sel_atoms.clipboard {
            Some(&self.clipboard)
        } else if atom == PRIMARY {
            Some(&self.primary_sel)
        } else {
            None
        }
    }

    fn seat_slot(&self, primary: bool) -> Option<Rc<dyn SelectionSource>> {
        let seat = self.state.seat.borrow().clone()?;
        if primary {
            seat.primary.current_source()
        } else {
            seat.data.current_source()
        }
    }

    fn set_seat_slot(&self, primary: bool, src: Option<Rc<dyn SelectionSource>>) {
        let Some(seat) = self.state.seat.borrow().clone() else { return };
        if primary {
            seat.primary.set_selection_source(&self.state, src);
        } else {
            seat.data.set_selection_source(&self.state, src);
        }
    }

    // does the seat still hold the provider we installed
    fn slot_is_ours(&self, sel: &SelState) -> bool {
        let Some(cur) = self.seat_slot(sel.primary) else { return false };
        sel.source
            .borrow()
            .as_ref()
            .is_some_and(|s| same_source(&cur, s))
    }

    // x ownership changed hands
    fn xfixes_selection(&self, selection: u32, owner: u32) {
        let Some(sel) = self.sel_by_atom(selection) else { return };
        if owner == self.selwin {
            // our own claim echoing back
            return;
        }
        if owner == 0 {
            if self.slot_is_ours(sel) {
                self.set_seat_slot(sel.primary, None);
            }
            sel.source.borrow_mut().take();
            return;
        }
        // ask the new owner what it has; selection_notify picks it up
        sel.targets_pending.set(sel.targets_pending.get() + 1);
        sel.targets_deadline
            .set(Some(Time::from_nsec(Time::now().nsec() + FETCH_TIMEOUT_NS)));
        self.c.send(|b| {
            wire::convert_selection(
                b,
                self.selwin,
                selection,
                self.sel_atoms.targets,
                sel.prop_targets,
                CURRENT_TIME,
            )
        });
    }

    // a conversion we asked for finished
    async fn selection_notify(&self, selection: u32, target: u32, property: u32) {
        let Some(sel) = self.sel_by_atom(selection) else { return };
        if target == self.sel_atoms.targets {
            let n = sel.targets_pending.get();
            if n == 0 {
                return;
            }
            sel.targets_pending.set(n - 1);
            if n == 1 {
                sel.targets_deadline.set(None);
            }
            // a newer conversion supersedes this reply
            if n > 1 || property == 0 {
                return;
            }
            let words = self.prop_words(self.selwin, sel.prop_targets).await;
            let mimes = self.targets_to_mimes(&words).await;
            if mimes.is_empty() {
                return;
            }
            let src = Rc::new(X11SelectionSource::new(self.xw.clone(), sel.primary, mimes));
            *sel.source.borrow_mut() = Some(src.clone());
            self.set_seat_slot(sel.primary, Some(src));
        } else {
            let Some(fetch) = sel.fetch.borrow_mut().take() else { return };
            if property != 0 {
                if let Ok(r) = self.c.get_property_full(self.selwin, sel.prop_data, 0).await {
                    // incr means a streamed transfer we don't do
                    if r.ty != self.sel_atoms.incr {
                        self.spawn_fd_write(fetch.fd, r.data);
                        self.start_next_fetch(sel).await;
                        return;
                    }
                }
            }
            // refused or unusable; the dropped fd is the eof
            drop(fetch);
            self.start_next_fetch(sel).await;
        }
    }

    // a wayland reader wants x data on this fd
    async fn x_fetch(&self, primary: bool, mime: String, fd: OwnedFd) {
        let sel = self.sel(primary);
        if sel.fetch.borrow().is_some() {
            sel.waiting.borrow_mut().push_back((mime, fd));
            return;
        }
        self.begin_fetch(sel, mime, fd).await;
    }

    async fn begin_fetch(&self, sel: &SelState, mime: String, fd: OwnedFd) {
        // unknown mime: the dropped fd answers
        let Some(target) = self.target_for_mime(&mime).await else { return };
        self.c.send(|b| {
            wire::convert_selection(b, self.selwin, sel.atom, target, sel.prop_data, CURRENT_TIME)
        });
        let deadline = Time::from_nsec(Time::now().nsec() + FETCH_TIMEOUT_NS);
        *sel.fetch.borrow_mut() = Some(Fetch { fd, deadline });
    }

    async fn start_next_fetch(&self, sel: &SelState) {
        while sel.fetch.borrow().is_none() {
            let next = sel.waiting.borrow_mut().pop_front();
            let Some((mime, fd)) = next else { return };
            self.begin_fetch(sel, mime, fd).await;
        }
    }

    fn next_deadline(&self) -> Option<Time> {
        let mut min: Option<Time> = None;
        for sel in [&self.clipboard, &self.primary_sel] {
            let fetch = sel.fetch.borrow().as_ref().map(|f| f.deadline);
            for d in [fetch, sel.targets_deadline.get()].into_iter().flatten() {
                min = Some(min.map_or(d, |m| m.min(d)));
            }
        }
        min
    }

    async fn expire_fetches(&self) {
        let now = Time::now().nsec();
        for sel in [&self.clipboard, &self.primary_sel] {
            let expired = sel
                .fetch
                .borrow()
                .as_ref()
                .is_some_and(|f| f.deadline.nsec() <= now);
            if expired {
                sel.fetch.borrow_mut().take();
                self.start_next_fetch(sel).await;
            }
            // targets conversions the owner never answered
            if sel.targets_deadline.get().is_some_and(|d| d.nsec() <= now) {
                sel.targets_pending.set(0);
                sel.targets_deadline.set(None);
            }
        }
    }

    // the wl-side selection changed; mirror it onto the x server
    fn wl_selection(&self, primary: bool) {
        let sel = self.sel(primary);
        match self.seat_slot(primary) {
            // an x client already owns it through our bridge
            Some(_) if self.slot_is_ours(sel) => {}
            Some(_) => {
                sel.source.borrow_mut().take();
                self.c
                    .send(|b| wire::set_selection_owner(b, self.selwin, sel.atom, CURRENT_TIME));
            }
            None => {
                sel.source.borrow_mut().take();
                self.c
                    .send(|b| wire::set_selection_owner(b, 0, sel.atom, CURRENT_TIME));
            }
        }
    }

    // an x client asked us, the owner, to convert; answer exactly once
    async fn selection_request(
        &self,
        time: u32,
        requestor: u32,
        selection: u32,
        target: u32,
        property: u32,
    ) {
        let a = &self.sel_atoms;
        // our own conversion caught us holding the selection; refusing it
        // keeps us from adopting a source that fronts ourselves, and the
        // refusal unwinds the pending count or fetch like any other
        if requestor == self.selwin {
            self.answer_request(time, requestor, selection, target, 0);
            return;
        }
        // obsolete requestors pass property None; the target atom stands in
        let property = if property == 0 { target } else { property };
        let provider = match self.sel_by_atom(selection) {
            Some(sel) if !self.slot_is_ours(sel) => self.seat_slot(sel.primary),
            _ => None,
        };
        // nothing bridged for this selection anymore
        let Some(provider) = provider else {
            self.answer_request(time, requestor, selection, target, 0);
            return;
        };
        let mimes = provider.mimes();
        if target == a.targets {
            let mut list = vec![a.targets];
            // any text mime advertises the whole text-target family
            if mimes.iter().any(|m| mime_to_target(a, m).is_some()) {
                list.extend([a.utf8, STRING, a.text]);
            }
            for m in &mimes {
                if let Ok(atom) = self.c.intern(m).await {
                    if !list.contains(&atom) {
                        list.push(atom);
                    }
                }
            }
            let bytes: Vec<u8> = list.iter().flat_map(|v| v.to_ne_bytes()).collect();
            self.c.send(|b| {
                wire::change_property(b, PROP_REPLACE, requestor, property, ATOM, 32, &bytes)
            });
            self.answer_request(time, requestor, selection, target, property);
            return;
        }
        let mime = self.mime_for_target(target).await;
        let Some(mime) = mime.filter(|m| mimes.contains(m)) else {
            self.answer_request(time, requestor, selection, target, 0);
            return;
        };
        // pipe to the provider; a detached task streams the read side back
        let Ok((r, w)) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) else {
            self.answer_request(time, requestor, selection, target, 0);
            return;
        };
        provider.send(&mime, w);
        self.spawn_transfer(time, requestor, selection, target, property, r);
    }

    fn answer_request(&self, time: u32, requestor: u32, selection: u32, target: u32, property: u32) {
        let ev = wire::encode_selection_notify(time, requestor, selection, target, property);
        self.c.send(|b| wire::send_event(b, false, requestor, 0, &ev));
    }

    // -- mime <-> target, the '/'-atom passthrough half --

    async fn target_for_mime(&self, mime: &str) -> Option<u32> {
        if let Some(t) = mime_to_target(&self.sel_atoms, mime) {
            return Some(t);
        }
        if mime.contains('/') {
            return self.c.intern(mime).await.ok();
        }
        None
    }

    async fn mime_for_target(&self, target: u32) -> Option<String> {
        if let Some(m) = target_to_mime(&self.sel_atoms, target) {
            return Some(m.to_string());
        }
        let reply = self.c.call(|b| wire::get_atom_name(b, target)).await.ok()?;
        let name = wire::parse_get_atom_name(&reply)?;
        name.contains('/').then_some(name)
    }

    async fn targets_to_mimes(&self, targets: &[u32]) -> Vec<String> {
        let mut mimes: Vec<String> = Vec::new();
        for &t in targets {
            if let Some(m) = target_to_mime(&self.sel_atoms, t) {
                if !mimes.iter().any(|x| x == m) {
                    mimes.push(m.to_string());
                }
            } else if t != self.sel_atoms.targets {
                if let Ok(reply) = self.c.call(|b| wire::get_atom_name(b, t)).await {
                    if let Some(name) = wire::parse_get_atom_name(&reply) {
                        if name.contains('/') && !mimes.contains(&name) {
                            mimes.push(name);
                        }
                    }
                }
            }
        }
        mimes
    }

    // -- detached transfers --

    fn transfer(&self, name: &'static str, f: impl std::future::Future<Output = ()> + 'static) {
        let id = self.transfer_next.get();
        self.transfer_next.set(id + 1);
        let transfers = self.transfers.clone();
        let state = self.state.clone();
        let task = self.state.eng.spawn(name, async move {
            f.await;
            // drop our own entry from a fresh task
            let t2 = transfers.clone();
            state.run_toplevel.schedule(move || {
                t2.borrow_mut().remove(&id);
            });
        });
        self.transfers.borrow_mut().insert(id, task);
    }

    // x -> wayland: hand the fetched bytes to the waiting reader
    fn spawn_fd_write(&self, fd: OwnedFd, data: Vec<u8>) {
        let ring = self.state.ring.clone();
        self.transfer("x selection write", async move {
            let fd = Rc::new(fd);
            let mut buf = data;
            while !buf.is_empty() {
                match ring.write(&fd, buf).await {
                    Ok((b, n)) if n > 0 => {
                        buf = b;
                        buf.drain(..n);
                    }
                    _ => return,
                }
            }
        });
    }

    // wayland -> x: drain the provider's pipe, land it on the requestor
    fn spawn_transfer(
        &self,
        time: u32,
        requestor: u32,
        selection: u32,
        target: u32,
        property: u32,
        pipe: OwnedFd,
    ) {
        let ring = self.state.ring.clone();
        let c = self.c.clone();
        self.transfer("x selection read", async move {
            let fd = Rc::new(pipe);
            let mut data = Vec::new();
            let mut ok = true;
            loop {
                let buf = vec![0u8; 65536];
                match ring.read(&fd, buf).await {
                    Ok((_, 0)) => break,
                    Ok((b, n)) => {
                        data.extend_from_slice(&b[..n]);
                        if data.len() > MAX_TRANSFER {
                            ok = false;
                            break;
                        }
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            let property = if ok { property } else { 0 };
            if ok {
                let mut mode = PROP_REPLACE;
                let mut rest: &[u8] = &data;
                loop {
                    let (chunk, tail) = rest.split_at(rest.len().min(PROP_SLICE));
                    c.send(|b| {
                        wire::change_property(b, mode, requestor, property, target, 8, chunk)
                    });
                    mode = PROP_APPEND;
                    rest = tail;
                    if rest.is_empty() {
                        break;
                    }
                }
            }
            let ev = wire::encode_selection_notify(time, requestor, selection, target, property);
            c.send(|b| wire::send_event(b, false, requestor, 0, &ev));
        });
    }

    // _NET_WM_STATE message: data[0] is the action, data[1] and data[2]
    // both carry candidate state atoms
    fn net_wm_state_message(&self, xwin: &Rc<XWindow>, data: &[u32; 5]) {
        let action = data[0];
        let apply = |cur: bool| match action {
            STATE_REMOVE => false,
            STATE_ADD => true,
            STATE_TOGGLE => !cur,
            _ => cur,
        };
        for atom in [data[1], data[2]] {
            if atom == self.atoms.net_wm_state_fullscreen {
                let win = xwin.window.borrow().clone();
                let cur = win
                    .as_ref()
                    .map_or(xwin.fullscreen_requested.get(), |w| w.fullscreen.get());
                let on = apply(cur);
                xwin.fullscreen_requested.set(on);
                if let Some(win) = win {
                    crate::tree::set_fullscreen(&self.state, &win, on);
                    win.set_fullscreen_state(on);
                }
            } else if atom == self.atoms.net_wm_state_modal {
                // takes effect at the next map gate
                xwin.modal.set(apply(xwin.modal.get()));
            }
        }
    }

    fn set_wm_state(&self, window: u32, wm_state: u32) {
        self.c.send(|b| {
            wire::change_property(
                b,
                PROP_REPLACE,
                window,
                self.atoms.wm_state,
                self.atoms.wm_state,
                32,
                &[wm_state.to_ne_bytes(), 0u32.to_ne_bytes()].concat(),
            )
        });
    }

    fn commit(&self, serial: u64) {
        let xid = self.by_serial.borrow().get(&serial).copied();
        let Some(xid) = xid else { return };
        let Some(xwin) = self.win(xid) else { return };
        self.try_pair(&xwin);
        self.gate(&xwin);
    }

    fn win(&self, xid: u32) -> Option<Rc<XWindow>> {
        self.windows.borrow().get(&xid).cloned()
    }

    fn try_pair(&self, xwin: &Rc<XWindow>) {
        if xwin.surface.borrow().is_some() {
            return;
        }
        let serial = xwin.serial.get();
        if serial == 0 {
            return;
        }
        if let Some(s) = self.xw.serials.borrow().get(&serial) {
            *xwin.surface.borrow_mut() = Some(s.clone());
        }
    }

    // the one place an x window enters or leaves the tree
    fn gate(&self, xwin: &Rc<XWindow>) {
        let mapped = xwin.x_mapped.get()
            && xwin
                .surface
                .borrow()
                .as_ref()
                .is_some_and(|s| s.mapped.get());
        let in_tree = xwin.window.borrow().is_some();
        if mapped && !in_tree {
            let win = Rc::new(Window::new(&self.state, WindowKind::X11(xwin.clone())));
            *xwin.window.borrow_mut() = Some(win.clone());
            if xwin.override_redirect.get() {
                // menus and tooltips place themselves and steal no focus
                win.floating.set(true);
                win.rect.set(xwin.geo.get());
                let ws = crate::tree::active(&self.state);
                ws.floats.borrow_mut().push(win);
                self.state.damage.trigger();
            } else {
                if xwin.wants_floating() {
                    // dialogs float at their own size, kept on screen
                    win.floating.set(true);
                    let r = self.clamp_to_output(xwin.geo.get());
                    win.rect.set(r);
                    if r != xwin.geo.get() {
                        xwin.configure_to(r);
                    }
                    let ws = crate::tree::active(&self.state);
                    ws.floats.borrow_mut().push(win.clone());
                    self.state.damage.trigger();
                } else {
                    crate::tree::map_window(&self.state, &win);
                }
                if xwin.fullscreen_requested.get() && !win.fullscreen.get() {
                    crate::tree::set_fullscreen(&self.state, &win, true);
                }
                self.client_list_add(xwin.xid);
            }
        } else if !mapped && in_tree {
            let win = xwin.window.borrow_mut().take().unwrap();
            if win.floating.get() {
                let ws = crate::tree::active(&self.state);
                ws.remove_float(&win);
                self.state.damage.trigger();
            } else {
                crate::tree::unmap_window(&self.state, &win);
            }
            self.client_list_remove(xwin.xid);
        }
    }

    fn clamp_to_output(&self, r: Rect) -> Rect {
        let (ow, oh) = crate::tree::output_extent(&self.state);
        let w = r.width().clamp(1, ow.max(1));
        let h = r.height().clamp(1, oh.max(1));
        let x = r.x1.clamp(0, (ow - w).max(0));
        let y = r.y1.clamp(0, (oh - h).max(0));
        Rect::new_sized_saturating(x, y, w, h)
    }

    // -- _NET_CLIENT_LIST --

    fn client_list_add(&self, xid: u32) {
        self.client_list.borrow_mut().push(xid);
        self.c.send(|b| {
            wire::change_property(
                b,
                PROP_APPEND,
                self.c.root,
                self.atoms.net_client_list,
                WINDOW,
                32,
                &xid.to_ne_bytes(),
            )
        });
    }

    fn client_list_remove(&self, xid: u32) {
        let mut list = self.client_list.borrow_mut();
        let before = list.len();
        list.retain(|w| *w != xid);
        if list.len() == before {
            return;
        }
        let bytes: Vec<u8> = list.iter().flat_map(|w| w.to_ne_bytes()).collect();
        self.c.send(|b| {
            wire::change_property(
                b,
                PROP_REPLACE,
                self.c.root,
                self.atoms.net_client_list,
                WINDOW,
                32,
                &bytes,
            )
        });
    }

    // -- property loaders --

    async fn refresh_all(&self, xwin: &Rc<XWindow>) {
        let a = self.atoms.clone();
        for atom in [
            a.wm_class,
            a.net_wm_name,
            a.wm_hints,
            a.wm_normal_hints,
            a.wm_protocols,
            a.net_wm_window_type,
            a.wm_transient_for,
            a.net_wm_state,
        ] {
            self.refresh_prop(xwin, atom).await;
        }
    }

    async fn refresh_prop(&self, xwin: &Rc<XWindow>, atom: u32) {
        let a = self.atoms.clone();
        if atom == a.wm_name || atom == a.net_wm_name {
            self.load_title(xwin).await;
        } else if atom == a.wm_class {
            // instance NUL class NUL; the class half is the app id
            let (_, data) = self.prop_bytes(xwin.xid, a.wm_class).await;
            let class = data.split(|c| *c == 0).nth(1).unwrap_or(b"");
            *xwin.class.borrow_mut() = String::from_utf8_lossy(class).into_owned();
        } else if atom == a.wm_hints {
            // flags bit 0 gates the input field; absent means focusable,
            // short reads zero-extend
            let words = self.prop_words(xwin.xid, a.wm_hints).await;
            let flags = words.first().copied().unwrap_or(0);
            let input = flags & 1 == 0 || words.get(1).copied().unwrap_or(0) != 0;
            xwin.input_hint.set(input);
        } else if atom == a.wm_normal_hints {
            let words = self.prop_words(xwin.xid, a.wm_normal_hints).await;
            let word = |i: usize| words.get(i).copied().unwrap_or(0) as i32;
            let flags = words.first().copied().unwrap_or(0);
            let min = if flags & P_MIN_SIZE != 0 { (word(5), word(6)) } else { (0, 0) };
            let max = if flags & P_MAX_SIZE != 0 { (word(7), word(8)) } else { (0, 0) };
            xwin.min_size.set(min);
            xwin.max_size.set(max);
        } else if atom == a.wm_protocols {
            let words = self.prop_words(xwin.xid, a.wm_protocols).await;
            xwin.delete_window.set(words.contains(&a.wm_delete_window));
        } else if atom == a.net_wm_window_type {
            let words = self.prop_words(xwin.xid, a.net_wm_window_type).await;
            let float = words.iter().any(|t| {
                [a.type_dialog, a.type_utility, a.type_toolbar, a.type_splash].contains(t)
            });
            xwin.float_type.set(float);
        } else if atom == a.wm_transient_for {
            let words = self.prop_words(xwin.xid, a.wm_transient_for).await;
            xwin.transient_for.set(words.first().copied().unwrap_or(0));
        } else if atom == a.net_wm_state {
            // the property is client-owned; state changes reach the tree
            // only through client messages
            let words = self.prop_words(xwin.xid, a.net_wm_state).await;
            xwin.fullscreen_requested
                .set(words.contains(&a.net_wm_state_fullscreen));
            xwin.modal.set(words.contains(&a.net_wm_state_modal));
        }
    }

    async fn load_title(&self, xwin: &Rc<XWindow>) {
        let a = &self.atoms;
        // utf8 wins whenever the client sets both names
        let (ty, mut data) = self.prop_bytes(xwin.xid, a.net_wm_name).await;
        if data.is_empty() || ty != a.utf8_string {
            let (ty, d) = self.prop_bytes(xwin.xid, a.wm_name).await;
            if ty != STRING && ty != a.utf8_string {
                return;
            }
            data = d;
        }
        *xwin.title.borrow_mut() = String::from_utf8_lossy(&data).into_owned();
    }

    // AnyPropertyType reads: asking with a wrong type would return an
    // empty value with bytes_after set and stall the chunk loop
    async fn prop_bytes(&self, window: u32, prop: u32) -> (u32, Vec<u8>) {
        match self.c.get_property_full(window, prop, 0).await {
            Ok(r) => (r.ty, r.data),
            Err(_) => (0, Vec::new()),
        }
    }

    async fn prop_words(&self, window: u32, prop: u32) -> Vec<u32> {
        match self.c.get_property_full(window, prop, 0).await {
            Ok(r) if r.format == 32 => r
                .data
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                .collect(),
            _ => Vec::new(),
        }
    }
}

async fn bring_up(c: &Rc<Xcon>) -> Result<(), XconError> {
    let composite = c.ext.borrow().as_ref().map(|e| e.composite).unwrap_or(0);
    let xfixes = c.ext.borrow().as_ref().map(|e| e.xfixes).unwrap_or(0);

    // redirect the root and see every child come and go
    c.send(|b| {
        wire::change_window_attributes(b, c.root, &[(EVENT_MASK_BIT, ROOT_EVENTS)])
    });
    c.send(|b| wire::composite_redirect_subwindows(b, composite, c.root, REDIRECT_MANUAL));
    // version negotiation replies; sending it void would desync the queue
    c.call(|b| wire::xfixes_query_version(b, xfixes, 6, 0)).await?;

    // the wm check window owns the selections and carries our identity
    let wm_win = c.alloc_xid();
    c.send(|b| {
        wire::create_window(b, 0, wm_win, c.root, -1, -1, 1, 1, 0, INPUT_ONLY, 0, &[])
    });
    let wm_s0 = c.intern("WM_S0").await?;
    let cm_s0 = c.intern("_NET_WM_CM_S0").await?;
    let check = c.intern("_NET_SUPPORTING_WM_CHECK").await?;
    let wm_name = c.intern("_NET_WM_NAME").await?;
    let utf8 = c.intern("UTF8_STRING").await?;
    let supported = c.intern("_NET_SUPPORTED").await?;
    c.send(|b| wire::set_selection_owner(b, wm_win, wm_s0, CURRENT_TIME));
    c.send(|b| wire::set_selection_owner(b, wm_win, cm_s0, CURRENT_TIME));
    c.send(|b| {
        wire::change_property(b, PROP_REPLACE, wm_win, check, WINDOW, 32, &wm_win.to_ne_bytes())
    });
    c.send(|b| {
        wire::change_property(b, PROP_REPLACE, c.root, check, WINDOW, 32, &wm_win.to_ne_bytes())
    });
    c.send(|b| wire::change_property(b, PROP_REPLACE, wm_win, wm_name, utf8, 8, b"carrot"));

    // a single fake desktop; steam refuses to believe in a wm without one
    let mut atoms: Vec<u32> = vec![check, wm_name];
    for name in [
        "_NET_WM_STATE",
        "_NET_WM_STATE_FULLSCREEN",
        "_NET_WM_STATE_MODAL",
        "_NET_ACTIVE_WINDOW",
        "_NET_CLIENT_LIST",
        "_NET_NUMBER_OF_DESKTOPS",
        "_NET_CURRENT_DESKTOP",
        "_NET_DESKTOP_VIEWPORT",
        "_NET_WM_WINDOW_TYPE",
        "_NET_WM_PID",
    ] {
        atoms.push(c.intern(name).await?);
    }
    let bytes: Vec<u8> = atoms.iter().flat_map(|a| a.to_ne_bytes()).collect();
    c.send(|b| wire::change_property(b, PROP_REPLACE, c.root, supported, ATOM, 32, &bytes));
    let n_desktops = c.intern("_NET_NUMBER_OF_DESKTOPS").await?;
    let cur_desktop = c.intern("_NET_CURRENT_DESKTOP").await?;
    let viewport = c.intern("_NET_DESKTOP_VIEWPORT").await?;
    let cardinal = 6u32;
    c.send(|b| {
        wire::change_property(b, PROP_REPLACE, c.root, n_desktops, cardinal, 32, &1u32.to_ne_bytes())
    });
    c.send(|b| {
        wire::change_property(b, PROP_REPLACE, c.root, cur_desktop, cardinal, 32, &0u32.to_ne_bytes())
    });
    let vp: Vec<u8> = [0u32, 0u32].iter().flat_map(|v| v.to_ne_bytes()).collect();
    c.send(|b| wire::change_property(b, PROP_REPLACE, c.root, viewport, cardinal, 32, &vp));
    // nothing is active yet
    let active = c.intern("_NET_ACTIVE_WINDOW").await?;
    c.send(|b| {
        wire::change_property(b, PROP_REPLACE, c.root, active, WINDOW, 32, &0u32.to_ne_bytes())
    });

    // fence everything so a failure here is loud and attributable
    c.call(|b| wire::get_input_focus(b)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms() -> SelAtoms {
        SelAtoms {
            clipboard: 100,
            targets: 101,
            text: 102,
            incr: 103,
            utf8: 105,
        }
    }

    #[test]
    fn mime_target_roundtrip() {
        let a = atoms();
        for mime in ["text/plain;charset=utf-8", "text/plain"] {
            let t = mime_to_target(&a, mime).unwrap();
            assert_eq!(target_to_mime(&a, t), Some(mime));
        }
        assert_eq!(mime_to_target(&a, "text/plain"), Some(STRING));
        // TEXT maps in, and goes back out through STRING
        assert_eq!(target_to_mime(&a, a.text), Some("text/plain"));
        // everything else resolves by atom name, not here
        assert_eq!(target_to_mime(&a, 999), None);
        assert_eq!(mime_to_target(&a, "image/png"), None);
    }
}
