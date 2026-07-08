// xwayland lifecycle - spawn, handshake, then hand the wm socket to xwm.
//
// fixed child fd layout: 2 stderr, 3 displayfd, 4 x socket, 5 wm socket,
// 6 wayland socketpair (WAYLAND_SOCKET=6); launched -terminate -rootless
// -displayfd 3 -listenfd 4 -wm 5. no Xauthority file: the wm socket is
// trusted with empty auth. the server starts lazily on the first client
// knocking at the display socket and respawns if it dies.

mod xwm;

use crate::xparsnip::conn::Parsnip;
use crate::client::{Client, ClientError, Object};
use crate::protocol::data_device::SelectionSource;
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{xwayland_shell_v1, xwayland_surface_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use crate::surface::{SurfaceExt, SurfaceRole, WlSurface};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;

pub struct Xwayland {
    pub display: u32,
    pub parsnip: RefCell<Option<Rc<Parsnip>>>,
    // wl surfaces by their xwayland serial, ready for the wm to pair
    pub serials: RefCell<HashMap<u64, Rc<WlSurface>>>,
    // one queue serializes x events and compositor-side pokes into the wm
    pub queue: crate::util::AsyncQueue<XwmEvent>,
    pub client: RefCell<Option<Rc<Client>>>,
    // filled once by xwm bring-up, gone with the server
    pub atoms: RefCell<Option<Rc<XAtoms>>>,
}

pub enum XwmEvent {
    X(crate::xparsnip::wire::XEvent),
    // an xwayland surface committed; serial pairing and the map gate
    // both re-evaluate on this
    Commit(u64),
    // the wl-side selection changed; the bridge claims or drops x ownership
    WlSelection { primary: bool },
    // a wayland reader wants x selection data written to this fd
    XFetch { primary: bool, mime: String, fd: OwnedFd },
}

// -- the x-side selection provider --

// sits in the seat slot while an x client owns a selection; send()
// defers to the wm task, which runs the x conversion dance
pub struct X11SelectionSource {
    xw: Rc<Xwayland>,
    primary: bool,
    mimes: Vec<String>,
}

impl X11SelectionSource {
    pub fn new(xw: Rc<Xwayland>, primary: bool, mimes: Vec<String>) -> Self {
        Self { xw, primary, mimes }
    }
}

impl SelectionSource for X11SelectionSource {
    fn mimes(&self) -> Vec<String> {
        self.mimes.clone()
    }

    fn send(&self, mime: &str, fd: OwnedFd) {
        self.xw.queue.push(XwmEvent::XFetch {
            primary: self.primary,
            mime: mime.to_string(),
            fd,
        });
    }

    fn cancelled(&self) {}

    fn is_x11(&self) -> bool {
        true
    }
}

// interned once per server; windows keep an rc so focus and close can
// speak x without asking the wm
pub struct XAtoms {
    pub wm_protocols: u32,
    pub wm_delete_window: u32,
    pub wm_take_focus: u32,
    pub wm_hints: u32,
    pub wm_normal_hints: u32,
    pub wm_name: u32,
    pub net_wm_name: u32,
    pub utf8_string: u32,
    pub wm_class: u32,
    pub net_wm_state: u32,
    pub net_wm_state_fullscreen: u32,
    pub net_wm_state_modal: u32,
    pub net_wm_window_type: u32,
    pub type_dialog: u32,
    pub type_utility: u32,
    pub type_toolbar: u32,
    pub type_splash: u32,
    pub net_active_window: u32,
    pub net_client_list: u32,
    pub wm_transient_for: u32,
    pub wm_state: u32,
}

impl Xwayland {
    pub fn clear(&self) {
        if let Some(x) = self.parsnip.borrow_mut().take() {
            x.clear();
        }
        self.serials.borrow_mut().clear();
        self.queue.clear();
        self.client.borrow_mut().take();
        self.atoms.borrow_mut().take();
    }
}

// one x window as the wm sees it; the tree holds these behind
// WindowKind::X11 once the map gate passes
pub struct XWindow {
    pub xid: u32,
    pub parsnip: Rc<Parsnip>,
    pub override_redirect: Cell<bool>,
    pub x_mapped: Cell<bool>,
    pub serial: Cell<u64>,
    pub surface: RefCell<Option<Rc<WlSurface>>>,
    pub window: RefCell<Option<Rc<crate::tree::Window>>>,
    // last geometry the x side knows, self-positioned for overrides
    pub geo: Cell<crate::rect::Rect>,
    pub title: RefCell<String>,
    pub class: RefCell<String>,
    // icccm/ewmh, refreshed from properties
    pub input_hint: Cell<bool>,
    pub delete_window: Cell<bool>,
    pub min_size: Cell<(i32, i32)>,
    pub max_size: Cell<(i32, i32)>,
    pub modal: Cell<bool>,
    // window type in the dialog/utility/toolbar/splash set
    pub float_type: Cell<bool>,
    pub fullscreen_requested: Cell<bool>,
    pub transient_for: Cell<u32>,
    pub atoms: RefCell<Option<Rc<XAtoms>>>,
}

// SubstructureRedirect, the mask WM_TAKE_FOCUS rides on
const SUBSTRUCTURE_REDIRECT: u32 = 0x0010_0000;
const REVERT_TO_POINTER_ROOT: u8 = 1;
const XA_WINDOW: u32 = 33;

impl XWindow {
    pub fn surface(&self) -> Option<Rc<WlSurface>> {
        self.surface.borrow().clone()
    }

    // the tree hands us our slot; x coordinates are compositor-global
    // because the rootless root is the output
    pub fn configure_to(&self, r: crate::rect::Rect) {
        use crate::xparsnip::wire;
        self.parsnip.send(|b| {
            wire::configure_window(
                b,
                self.xid,
                &[
                    (0, r.x1 as u32),
                    (1, r.y1 as u32),
                    (2, r.width() as u32),
                    (3, r.height() as u32),
                ],
            )
        });
    }

    // WM_DELETE_WINDOW when the client offered it, KillClient otherwise
    pub fn close(&self) {
        use crate::xparsnip::wire;
        let Some(a) = self.atoms.borrow().clone() else { return };
        if self.delete_window.get() {
            let ev = wire::encode_client_message(
                self.xid,
                a.wm_protocols,
                32,
                &[a.wm_delete_window, 0, 0, 0, 0],
            );
            self.parsnip.send(|b| wire::send_event(b, false, self.xid, 0, &ev));
        } else {
            self.parsnip.send(|b| wire::kill_client(b, self.xid));
        }
    }

    // icccm focus: WM_TAKE_FOCUS always, real input focus only when the
    // hints ask for it; the root learns the active window either way
    pub fn take_focus(&self) {
        use crate::xparsnip::wire;
        let Some(a) = self.atoms.borrow().clone() else { return };
        let mask = if self.input_hint.get() { SUBSTRUCTURE_REDIRECT } else { 0 };
        let ev = wire::encode_client_message(
            self.xid,
            a.wm_protocols,
            32,
            &[a.wm_take_focus, 0, 0, 0, 0],
        );
        self.parsnip.send(|b| wire::send_event(b, false, self.xid, mask, &ev));
        if self.input_hint.get() {
            self.parsnip
                .send(|b| wire::set_input_focus(b, REVERT_TO_POINTER_ROOT, self.xid, 0));
        }
        self.parsnip.send(|b| {
            wire::change_property(
                b,
                0,
                self.parsnip.root,
                a.net_active_window,
                XA_WINDOW,
                32,
                &self.xid.to_ne_bytes(),
            )
        });
    }

    // modal, a floating window type, or a fixed min==max size; being
    // transient alone doesn't float
    pub fn wants_floating(&self) -> bool {
        if self.modal.get() || self.float_type.get() {
            return true;
        }
        let (miw, mih) = self.min_size.get();
        let (maw, mah) = self.max_size.get();
        miw > 0 && mih > 0 && miw == maw && mih == mah
    }
}

// -- the display socket --

struct DisplayClaim {
    n: u32,
    sock: Rc<OwnedFd>,
    lock_path: std::path::PathBuf,
    sock_path: std::path::PathBuf,
}

impl Drop for DisplayClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

fn claim_display() -> Option<DisplayClaim> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, bind, listen, socket_with};
    let dir = std::path::Path::new("/tmp/.X11-unix");
    if !dir.exists() {
        let _ = std::fs::create_dir(dir);
    }
    for n in 100..200u32 {
        let lock_path = std::path::PathBuf::from(format!("/tmp/.X{n}-lock"));
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path);
        let Ok(mut lock) = lock else { continue };
        use std::io::Write as _;
        let _ = writeln!(lock, "{:>10}", std::process::id());
        let sock_path = dir.join(format!("X{n}"));
        let _ = std::fs::remove_file(&sock_path);
        let fd = match socket_with(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC, None) {
            Ok(f) => f,
            Err(_) => {
                let _ = std::fs::remove_file(&lock_path);
                continue;
            }
        };
        let ok = SocketAddrUnix::new(&sock_path)
            .ok()
            .and_then(|a| bind(&fd, &a).ok())
            .and_then(|_| listen(&fd, 64).ok())
            .is_some();
        if !ok {
            let _ = std::fs::remove_file(&lock_path);
            continue;
        }
        return Some(DisplayClaim { n, sock: Rc::new(fd), lock_path, sock_path });
    }
    None
}

// -- the manager --

pub async fn run(state: Rc<State>) {
    let Some(claim) = claim_display() else {
        eprintln!("carrot: xwayland: no free display number, x11 disabled");
        return;
    };
    let xw = Rc::new(Xwayland {
        display: claim.n,
        parsnip: RefCell::new(None),
        serials: RefCell::new(HashMap::new()),
        queue: crate::util::AsyncQueue::default(),
        client: RefCell::new(None),
        atoms: RefCell::new(None),
    });
    *state.xwayland.borrow_mut() = Some(xw.clone());
    println!("carrot: xwayland on :{} (lazy)", claim.n);
    loop {
        // lazy: nothing runs until someone knocks
        if state.ring.readable(&claim.sock).await.is_err() {
            return;
        }
        match run_one(&state, &xw, &claim).await {
            Ok(()) => eprintln!("carrot: xwayland exited, will respawn on demand"),
            Err(e) => {
                eprintln!("carrot: xwayland: {e}");
                // don't spin on a broken setup
                let deadline = crate::util::Time::from_nsec(
                    crate::util::Time::now().nsec() + 1_000_000_000,
                );
                let _ = state.ring.timeout(deadline).await;
            }
        }
        xw.clear();
    }
}

async fn run_one(state: &Rc<State>, xw: &Rc<Xwayland>, claim: &DisplayClaim) -> Result<(), String> {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    use rustix::pipe::{PipeFlags, pipe_with};

    let (display_r, display_w) = pipe_with(PipeFlags::CLOEXEC).map_err(|e| format!("pipe: {e}"))?;
    let (wm_ours, wm_child) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("socketpair: {e}"))?;
    let (way_ours, way_child) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("socketpair: {e}"))?;

    let mut cmd = std::process::Command::new("Xwayland");
    cmd.args([
        "-terminate", "-rootless", "-verbose", "10",
        "-displayfd", "3", "-listenfd", "4", "-wm", "5",
    ]);
    cmd.env("WAYLAND_SOCKET", "6");
    cmd.env_remove("WAYLAND_DISPLAY");
    cmd.env_remove("DISPLAY");
    // sources move into the closure as raw fds; dup2 clears cloexec on
    // the child side and the parent copies stay owned out here
    let srcs = [
        display_w.as_raw_fd(),
        claim.sock.as_raw_fd(),
        wm_child.as_raw_fd(),
        way_child.as_raw_fd(),
    ];
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            crate::sighand::unblock_all_in_child();
            // two phases so a source sitting on 3..6 can't get clobbered
            let mut high = [0i32; 4];
            for (i, src) in srcs.iter().enumerate() {
                let fd = rustix::io::fcntl_dupfd_cloexec(
                    std::os::fd::BorrowedFd::borrow_raw(*src),
                    10,
                )
                .map_err(std::io::Error::from)?;
                high[i] = fd.into_raw_fd();
            }
            for (i, dst) in [3, 4, 5, 6].into_iter().enumerate() {
                let mut target = OwnedFd::from_raw_fd(dst);
                rustix::io::dup2(std::os::fd::BorrowedFd::borrow_raw(high[i]), &mut target)
                    .map_err(std::io::Error::from)?;
                std::mem::forget(target);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn Xwayland: {e}"))?;
    drop(display_w);
    drop(wm_child);
    drop(way_child);
    let pid = rustix::process::Pid::from_child(&child);
    let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
        .map_err(|e| format!("pidfd: {e}"))?;
    let pidfd = Rc::new(pidfd);

    // the wayland side must be live before readiness: the server's own
    // bring-up round-trips through it
    let wl_client = state.clients.spawn(state, way_ours);
    if let Some(c) = &wl_client {
        c.is_xwayland.set(true);
    }
    *xw.client.borrow_mut() = wl_client.clone();
    let wl_client_id = wl_client.map(|c| c.id);

    // readiness: the server writes its display number when it's up
    let display_r = Rc::new(display_r);
    let mut ready = Vec::new();
    loop {
        let buf = vec![0u8; 16];
        let Ok((buf, n)) = state.ring.read(&display_r, buf).await else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("displayfd closed before readiness".into());
        };
        if n == 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("displayfd eof before readiness".into());
        }
        ready.extend_from_slice(&buf[..n]);
        if ready.contains(&b'\n') {
            break;
        }
    }

    let parsnip = Parsnip::connect(&state.eng, &state.ring, wm_ours, b"", &[])
        .await
        .map_err(|e| format!("wm connect: {e}"))?;
    *xw.parsnip.borrow_mut() = Some(parsnip.clone());
    println!("carrot: xwayland up on :{}", xw.display);

    let xw2 = xw.clone();
    let xc2 = parsnip.clone();
    let pump = state.eng.spawn("xwm pump", async move {
        loop {
            let ev = xc2.events.pop().await;
            xw2.queue.push(XwmEvent::X(ev));
        }
    });
    let wm = state.eng.spawn("xwm", xwm::run(state.clone(), xw.clone(), parsnip.clone()));

    // the pidfd turning readable is the death notice
    let _ = state.ring.readable(&pidfd).await;
    drop(wm);
    drop(pump);
    let _ = child.wait();
    // x windows must not survive their server
    crate::tree::remove_x11_windows(state);
    // neither may x-backed selections: a paste against a dead bridge
    // would block on a pipe nobody will ever write
    if let Some(seat) = state.seat.borrow().clone() {
        if seat.data.current_source().is_some_and(|s| s.is_x11()) {
            seat.data.set_selection_source(state, None);
        }
        if seat.primary.current_source().is_some_and(|s| s.is_x11()) {
            seat.primary.set_selection_source(state, None);
        }
    }
    parsnip.clear();
    if let Some(id) = wl_client_id {
        state.clients.kill(id);
    }
    Ok(())
}

// -- xwayland_shell_v1 --

pub struct XwaylandShellGlobal;

impl Global for XwaylandShellGlobal {
    fn interface(&self) -> &'static str {
        xwayland_shell_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        if !client.is_xwayland.get() {
            client.protocol_error(id, 0, "xwayland_shell_v1 is for xwayland only");
            return Ok(());
        }
        client.add_client_obj(Rc::new(XwaylandShell {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct XwaylandShell {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl xwayland_shell_v1::Handler for XwaylandShell {
    fn destroy(
        &self,
        _req: xwayland_shell_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_xwayland_surface(
        &self,
        req: xwayland_shell_v1::get_xwayland_surface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        if surface.has_live_role() {
            c.protocol_error(self.id, 1, "the surface already has a role object");
            return Ok(());
        }
        if let Err(old) = surface.set_role(SurfaceRole::Xwayland) {
            c.protocol_error(
                self.id,
                1,
                &format!("the surface already has the {} role", old.name()),
            );
            return Ok(());
        }
        let xs = Rc::new(XwaylandSurface {
            id: req.id,
            client: c.clone(),
            version: self.version,
            surface: surface.clone(),
            serial: Cell::new(None),
            registered: Cell::new(false),
        });
        c.add_client_obj(xs.clone())?;
        *surface.ext.borrow_mut() = Rc::new(XwaylandExt { xs });
        Ok(())
    }
}

impl Object for XwaylandShell {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xwayland_shell_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xwayland_shell_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct XwaylandSurface {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub surface: Rc<WlSurface>,
    serial: Cell<Option<u64>>,
    registered: Cell<bool>,
}

impl xwayland_surface_v1::Handler for XwaylandSurface {
    fn set_serial(
        &self,
        req: xwayland_surface_v1::set_serial::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let serial = (req.serial_hi as u64) << 32 | req.serial_lo as u64;
        if serial == 0 || self.serial.get().is_some() {
            self.client
                .protocol_error(self.id, 0, "serial must be set exactly once, nonzero");
            return Ok(());
        }
        self.serial.set(Some(serial));
        Ok(())
    }

    fn destroy(
        &self,
        _req: xwayland_surface_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        *self.surface.ext.borrow_mut() = Rc::new(crate::surface::NoneExt);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for XwaylandSurface {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xwayland_surface_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xwayland_surface_v1::dispatch(&*self, self.version, opcode, r)
    }
}

struct XwaylandExt {
    xs: Rc<XwaylandSurface>,
}

impl SurfaceExt for XwaylandExt {
    // serial pairing goes live at commit; every later commit re-pokes the
    // wm so the map gate tracks the buffer coming and going
    fn after_apply(&self) {
        let xs = &self.xs;
        let Some(serial) = xs.serial.get() else { return };
        let state = &xs.client.state;
        let Some(xw) = state.xwayland.borrow().clone() else { return };
        if !xs.registered.replace(true) {
            xw.serials.borrow_mut().insert(serial, xs.surface.clone());
            crate::trace!("xwayland surface serial {} registered", serial);
        }
        xw.queue.push(XwmEvent::Commit(serial));
    }

    // xwayland tears surfaces down in its own order; that's its business
    fn on_surface_destroy(&self) -> Result<(), ()> {
        Ok(())
    }
}
