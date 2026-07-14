// ext-image-capture-source + ext-image-copy-capture v1: long-lived capture
// sessions over opaque source descriptors. shm only. a constraint burst ends
// in exactly one done, a dying session stops before its frame fails, and a
// size race re-states the constraints rather than failing.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    ext_foreign_toplevel_image_capture_source_manager_v1 as toplevel_source_mgr_v1,
    ext_image_capture_source_v1 as source_v1,
    ext_image_copy_capture_cursor_session_v1 as cursor_session_v1,
    ext_image_copy_capture_frame_v1 as frame_v1,
    ext_image_copy_capture_manager_v1 as manager_v1,
    ext_image_copy_capture_session_v1 as session_v1,
    ext_output_image_capture_source_manager_v1 as output_source_mgr_v1,
};
use crate::protocol::shm::WlBuffer;
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use crate::state::State;
use crate::surface::WlSurface;
use crate::tree::Window;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

const WL_SHM_XRGB8888: u32 = 1;
const WL_SHM_ARGB8888: u32 = 0;

const OPT_PAINT_CURSORS: u32 = 1;
const ERR_INVALID_OPTION: u32 = 1;
const ERR_DUPLICATE_FRAME: u32 = 1;
const ERR_NO_BUFFER: u32 = 1;
const ERR_INVALID_BUFFER_DAMAGE: u32 = 2;
const ERR_ALREADY_CAPTURED: u32 = 3;
const ERR_DUPLICATE_SESSION: u32 = 1;

const REASON_UNKNOWN: u32 = 0;
const REASON_STOPPED: u32 = 2;

// -- capture sources --

/// what a source names; resolved per use, never at creation
pub(crate) enum SourceKind {
    /// connector name: replug-safe where a slot index is not
    Output(String),
    Toplevel(RefCell<Weak<Window>>),
}

impl SourceKind {
    fn snapshot(&self) -> SourceKind {
        match self {
            SourceKind::Output(n) => SourceKind::Output(n.clone()),
            SourceKind::Toplevel(w) => SourceKind::Toplevel(RefCell::new(w.borrow().clone())),
        }
    }
}

pub struct CaptureSource {
    pub id: ObjectId,
    pub client: Rc<Client>,
    kind: SourceKind,
}

impl source_v1::Handler for CaptureSource {
    fn destroy(&self, _req: source_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.objects.forget_capture_source(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for CaptureSource {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        source_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        // the source interface is frozen at 1 whatever the factory bound
        source_v1::dispatch(&*self, 1, opcode, r)
    }

    fn break_loops(&self) {
        if let SourceKind::Toplevel(w) = &self.kind {
            *w.borrow_mut() = Weak::new();
        }
    }
}

fn add_source(client: &Rc<Client>, id: ObjectId, kind: SourceKind) -> Result<(), ClientError> {
    let src = Rc::new(CaptureSource {
        id,
        client: client.clone(),
        kind,
    });
    client.add_client_obj(src.clone())?;
    client.objects.track_capture_source(src);
    Ok(())
}

// -- the source factories --

pub struct OutputSourceManagerGlobal;

impl Global for OutputSourceManagerGlobal {
    fn interface(&self) -> &'static str {
        output_source_mgr_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(OutputSourceManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct OutputSourceManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl output_source_mgr_v1::Handler for OutputSourceManager {
    fn create_source(
        &self,
        req: output_source_mgr_v1::create_source::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(out) = c.objects.output(req.output) else {
            c.invalid_object(req.output);
            return Ok(());
        };
        // liveness is the consumer's problem at session creation
        add_source(c, req.source, SourceKind::Output(out.name.clone()))?;
        Ok(())
    }

    fn destroy(
        &self,
        _req: output_source_mgr_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // sources made here stay valid
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for OutputSourceManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        output_source_mgr_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        output_source_mgr_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct ToplevelSourceManagerGlobal;

impl Global for ToplevelSourceManagerGlobal {
    fn interface(&self) -> &'static str {
        toplevel_source_mgr_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(ToplevelSourceManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct ToplevelSourceManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl toplevel_source_mgr_v1::Handler for ToplevelSourceManager {
    fn create_source(
        &self,
        req: toplevel_source_mgr_v1::create_source::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(handle) = crate::protocol::foreign_toplevel_list::find_handle(
            &c.state,
            c.id,
            req.toplevel_handle,
        ) else {
            c.invalid_object(req.toplevel_handle);
            return Ok(());
        };
        // a closed handle yields a dead source; sessions on it get stopped
        add_source(c, req.source, SourceKind::Toplevel(RefCell::new(handle.window())))?;
        Ok(())
    }

    fn destroy(
        &self,
        _req: toplevel_source_mgr_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ToplevelSourceManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        toplevel_source_mgr_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        toplevel_source_mgr_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the copy-capture manager --

pub struct IccManagerGlobal;

impl Global for IccManagerGlobal {
    fn interface(&self) -> &'static str {
        manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(IccManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct IccManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl manager_v1::Handler for IccManager {
    fn create_session(
        &self,
        req: manager_v1::create_session::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        // paint_cursors is the only defined option
        if req.options & !OPT_PAINT_CURSORS != 0 {
            c.protocol_error(self.id, ERR_INVALID_OPTION, "unknown option bits");
            return Ok(());
        }
        let Some(src) = c.objects.capture_source(req.source) else {
            c.invalid_object(req.source);
            return Ok(());
        };
        let sess = new_session(
            c,
            req.session,
            self.version,
            src.kind.snapshot(),
            req.options & OPT_PAINT_CURSORS != 0,
        );
        c.add_client_obj(sess.clone())?;
        let state = &c.state;
        match sess.current_size(state) {
            Some((w, h)) => {
                send_constraints(&sess, w, h);
                state.icc_sessions.borrow_mut().push(sess);
            }
            None => {
                // dead source: stopped instead of constraints
                sess.stopped.set(true);
                c.event(|o| session_v1::stopped::send(o, sess.id));
            }
        }
        Ok(())
    }

    fn create_pointer_cursor_session(
        &self,
        req: manager_v1::create_pointer_cursor_session::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(src) = c.objects.capture_source(req.source) else {
            c.invalid_object(req.source);
            return Ok(());
        };
        if c.objects.get(req.pointer).is_none() {
            c.invalid_object(req.pointer);
            return Ok(());
        }
        c.add_client_obj(Rc::new(IccCursorSession {
            id: req.session,
            client: c.clone(),
            version: self.version,
            source: src.kind.snapshot(),
            have_session: Cell::new(false),
        }))?;
        Ok(())
    }

    fn destroy(&self, _req: manager_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for IccManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the session --

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameStatus {
    Unused,
    Capturing,
    Ready,
    Failed,
}

pub struct IccSession {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    me: Weak<IccSession>,
    source: SourceKind,
    stopped: Cell<bool>,
    /// the frame slot's state; frame objects are thin shells over it
    status: Cell<FrameStatus>,
    frame: RefCell<Option<Rc<IccFrame>>>,
    buffer: RefCell<Option<Rc<WlBuffer>>>,
    /// the next capture completes synchronously: session birth, and again
    /// after any failure
    force: Cell<bool>,
    /// compose the pointer into captures (the paint_cursors option)
    paint_cursors: bool,
    /// one constraint re-burst per size race, cleared by the next attach
    size_debounce: Cell<bool>,
    /// last successful delivery; paces continuous captures below the
    /// present rate
    served: Cell<u64>,
}

/// continuous captures complete at most ~60 times a second; a skipped
/// present leaves the frame pending and the next change completes it
const SERVE_MIN_NS: u64 = 15_000_000;

fn new_session(
    client: &Rc<Client>,
    id: ObjectId,
    version: u32,
    source: SourceKind,
    paint_cursors: bool,
) -> Rc<IccSession> {
    Rc::new_cyclic(|me| IccSession {
        id,
        client: client.clone(),
        version,
        me: me.clone(),
        source,
        stopped: Cell::new(false),
        status: Cell::new(FrameStatus::Unused),
        frame: RefCell::new(None),
        buffer: RefCell::new(None),
        force: Cell::new(true),
        size_debounce: Cell::new(false),
        paint_cursors,
        served: Cell::new(0),
    })
}

impl IccSession {
    /// the source's current buffer size; None means the source is dead
    fn current_size(&self, state: &Rc<State>) -> Option<(u32, u32)> {
        match &self.source {
            SourceKind::Output(name) => {
                crate::output::output_geometry(state, name).map(|(_, w, h)| (w, h))
            }
            SourceKind::Toplevel(weak) => {
                let win = weak.borrow().upgrade()?;
                let r = win.draw_rect(state);
                Some((r.width() as u32, r.height() as u32))
            }
        }
        .filter(|&(w, h)| w > 0 && h > 0)
    }
}

/// the full constraint batch, terminated by exactly one done
fn send_constraints(sess: &IccSession, w: u32, h: u32) {
    let id = sess.id;
    sess.client.event(|o| {
        // XRGB8888 first: clients that take the first format get the safe one
        session_v1::shm_format::send(o, id, WL_SHM_XRGB8888);
        session_v1::shm_format::send(o, id, WL_SHM_ARGB8888);
        session_v1::buffer_size::send(o, id, w, h);
        session_v1::done::send(o, id);
    });
}

/// fail the current frame; its object lives until the client destroys it
fn fail_frame(sess: &IccSession, reason: u32) {
    let frame = sess.frame.borrow().clone();
    if let Some(f) = frame {
        f.client.event(|o| frame_v1::failed::send(o, f.id, reason));
    }
    sess.status.set(FrameStatus::Failed);
    sess.force.set(true);
}

/// the session is over: stopped first, only then the in-flight frame fails
pub(crate) fn stop_session(state: &Rc<State>, sess: &Rc<IccSession>) {
    if sess.stopped.replace(true) {
        return;
    }
    sess.client.event(|o| session_v1::stopped::send(o, sess.id));
    if sess.status.get() == FrameStatus::Capturing {
        fail_frame(sess, REASON_STOPPED);
    }
    state
        .icc_sessions
        .borrow_mut()
        .retain(|s| !Rc::ptr_eq(s, sess));
}

fn send_ready(sess: &IccSession, frame_id: ObjectId, bw: u32, bh: u32) {
    let nsec = crate::util::Time::now().nsec();
    let sec = nsec / 1_000_000_000;
    let rem = (nsec % 1_000_000_000) as u32;
    sess.client.event(|o| {
        frame_v1::transform::send(o, frame_id, 0);
        // full-buffer damage every frame is always correct
        frame_v1::damage::send(o, frame_id, 0, 0, bw as i32, bh as i32);
        frame_v1::presentation_time::send(o, frame_id, (sec >> 32) as u32, sec as u32, rem);
        frame_v1::ready::send(o, frame_id);
    });
}

/// complete one pending capture: resolve the source, copy, report
fn service(state: &Rc<State>, sess: &Rc<IccSession>) {
    if sess.status.get() != FrameStatus::Capturing {
        return;
    }
    let frame = sess.frame.borrow().clone();
    let Some(frame) = frame else { return };
    let buf = sess.buffer.borrow().clone();
    let Some(buf) = buf else { return };
    enum Src {
        Out(usize),
        Win(Rc<Window>),
    }
    let (src, cur_w, cur_h) = match &sess.source {
        SourceKind::Output(name) => {
            let Some((slot, w, h)) = crate::output::output_geometry(state, name) else {
                stop_session(state, sess);
                return;
            };
            (Src::Out(slot), w, h)
        }
        SourceKind::Toplevel(weak) => {
            let win = weak.borrow().upgrade();
            let Some(win) = win else {
                stop_session(state, sess);
                return;
            };
            let r = win.draw_rect(state);
            let (w, h) = (r.width() as u32, r.height() as u32);
            (Src::Win(win), w, h)
        }
    };
    let (bw, bh) = (buf.rect.width() as u32, buf.rect.height() as u32);
    if (bw, bh) != (cur_w, cur_h) && !sess.size_debounce.replace(true) {
        // never failed(buffer_constraints): re-state the constraints, then
        // still complete the copy cropped to the intersection
        send_constraints(sess, cur_w, cur_h);
    }
    let (cw, ch) = (bw.min(cur_w), bh.min(cur_h));
    if cw == 0 || ch == 0 || buf.destroyed.get() {
        fail_frame(sess, REASON_UNKNOWN);
        return;
    }
    // only shm constraints were ever advertised
    let Some((fd, base)) = buf.shm_write_target() else {
        fail_frame(sess, REASON_UNKNOWN);
        return;
    };
    let stride = buf.stride as usize;
    let row = cw as usize * 4;
    if stride < row {
        fail_frame(sess, REASON_UNKNOWN);
        return;
    }
    let (px, src_stride) = match src {
        Src::Out(slot) => {
            let region = Rect::new_sized_saturating(0, 0, cw as i32, ch as i32);
            match crate::output::screencopy(state, slot, region, sess.paint_cursors) {
                Some(px) => (px, row),
                None => {
                    fail_frame(sess, REASON_UNKNOWN);
                    return;
                }
            }
        }
        Src::Win(win) => match crate::output::window_capture(state, &win) {
            Some(px) => (px, cur_w as usize * 4),
            None => {
                fail_frame(sess, REASON_UNKNOWN);
                return;
            }
        },
    };
    for r in 0..ch as usize {
        let off = (base + r * stride) as u64;
        if let Err(e) = rustix::io::pwrite(fd, &px[r * src_stride..][..row], off) {
            eprintln!("carrot: image copy write failed: {e}");
            fail_frame(sess, REASON_UNKNOWN);
            return;
        }
    }
    sess.status.set(FrameStatus::Ready);
    sess.force.set(false);
    sess.served.set(crate::util::Time::now().nsec());
    send_ready(sess, frame.id, bw, bh);
}

impl session_v1::Handler for IccSession {
    fn create_frame(
        &self,
        req: session_v1::create_frame::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if self.frame.borrow().is_some() {
            c.protocol_error(self.id, ERR_DUPLICATE_FRAME, "the session already has a frame");
            return Ok(());
        }
        let sess = self.me.upgrade().expect("session dispatched without an rc");
        let frame = Rc::new(IccFrame {
            id: req.frame,
            client: c.clone(),
            version: self.version,
            session: sess,
        });
        c.add_client_obj(frame.clone())?;
        *self.frame.borrow_mut() = Some(frame);
        Ok(())
    }

    fn destroy(&self, _req: session_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        // the frame object survives a session destroy; its capture cannot
        if self.status.get() == FrameStatus::Capturing {
            fail_frame(self, REASON_STOPPED);
        }
        self.stopped.set(true);
        let state = &self.client.state;
        state
            .icc_sessions
            .borrow_mut()
            .retain(|s| !(s.id == self.id && s.client.id == self.client.id));
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for IccSession {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        session_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        session_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.frame.borrow_mut().take();
        self.buffer.borrow_mut().take();
        if let SourceKind::Toplevel(w) = &self.source {
            *w.borrow_mut() = Weak::new();
        }
    }
}

// -- the frame --

pub struct IccFrame {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    session: Rc<IccSession>,
}

impl frame_v1::Handler for IccFrame {
    fn destroy(&self, _req: frame_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        let sess = &self.session;
        if sess.frame.borrow().as_ref().is_some_and(|f| f.id == self.id) {
            sess.frame.borrow_mut().take();
            sess.buffer.borrow_mut().take();
            sess.status.set(FrameStatus::Unused);
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn attach_buffer(
        &self,
        req: frame_v1::attach_buffer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let sess = &self.session;
        if sess.status.get() != FrameStatus::Unused {
            c.protocol_error(self.id, ERR_ALREADY_CAPTURED, "the frame was already captured");
            return Ok(());
        }
        let Some(buf) = c.objects.buffer(req.buffer) else {
            c.invalid_object(req.buffer);
            return Ok(());
        };
        *sess.buffer.borrow_mut() = Some(buf);
        // the client reacted to the last burst; racing sizes may re-send
        sess.size_debounce.set(false);
        Ok(())
    }

    fn damage_buffer(
        &self,
        req: frame_v1::damage_buffer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if self.session.status.get() != FrameStatus::Unused {
            c.protocol_error(self.id, ERR_ALREADY_CAPTURED, "the frame was already captured");
            return Ok(());
        }
        if req.x < 0 || req.y < 0 || req.width <= 0 || req.height <= 0 {
            c.protocol_error(self.id, ERR_INVALID_BUFFER_DAMAGE, "invalid damage rectangle");
            return Ok(());
        }
        // the rects are a hint; every copy writes the whole buffer
        Ok(())
    }

    fn capture(&self, _req: frame_v1::capture::Request) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let sess = &self.session;
        if sess.status.get() != FrameStatus::Unused {
            c.protocol_error(self.id, ERR_ALREADY_CAPTURED, "the frame was already captured");
            return Ok(());
        }
        if sess.buffer.borrow().is_none() {
            c.protocol_error(self.id, ERR_NO_BUFFER, "capture without an attached buffer");
            return Ok(());
        }
        if sess.stopped.get() {
            // not a protocol error: the stop can race the client's capture
            fail_frame(sess, REASON_STOPPED);
            return Ok(());
        }
        sess.status.set(FrameStatus::Capturing);
        if sess.force.get() {
            // prompt first frame; later ones wait for the content to change
            service(&c.state, sess);
        }
        Ok(())
    }
}

impl Object for IccFrame {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        frame_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        frame_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.session.frame.borrow_mut().take();
        self.session.buffer.borrow_mut().take();
    }
}

// -- the cursor session --

pub struct IccCursorSession {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    source: SourceKind,
    have_session: Cell<bool>,
}

impl cursor_session_v1::Handler for IccCursorSession {
    fn destroy(
        &self,
        _req: cursor_session_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_capture_session(
        &self,
        req: cursor_session_v1::get_capture_session::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if self.have_session.replace(true) {
            c.protocol_error(
                self.id,
                ERR_DUPLICATE_SESSION,
                "the cursor session already has a capture session",
            );
            return Ok(());
        }
        // no cursor capture path yet: an honest stop, not a stalled client
        let sess = new_session(c, req.session, self.version, self.source.snapshot(), false);
        c.add_client_obj(sess.clone())?;
        sess.stopped.set(true);
        c.event(|o| session_v1::stopped::send(o, sess.id));
        Ok(())
    }
}

impl Object for IccCursorSession {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        cursor_session_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        cursor_session_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        if let SourceKind::Toplevel(w) = &self.source {
            *w.borrow_mut() = Weak::new();
        }
    }
}

// -- fan-out from the present/commit/hotplug paths --

/// end of a present: the output's content changed, pending captures complete
pub fn output_presented(state: &Rc<State>, name: &str) {
    if state.icc_sessions.borrow().is_empty() {
        return;
    }
    let now = crate::util::Time::now().nsec();
    let sessions = state.icc_sessions.borrow().clone();
    for sess in sessions {
        if sess.status.get() != FrameStatus::Capturing {
            continue;
        }
        if now.saturating_sub(sess.served.get()) < SERVE_MIN_NS {
            continue;
        }
        if matches!(&sess.source, SourceKind::Output(n) if n == name) {
            service(state, &sess);
        }
    }
}

/// a surface commit changed pixels; captures of the toplevel above it
/// complete, wherever its workspace is
pub fn content_changed(state: &Rc<State>, s: &WlSurface) {
    if state.icc_sessions.borrow().is_empty() {
        return;
    }
    let now = crate::util::Time::now().nsec();
    let sessions = state.icc_sessions.borrow().clone();
    for sess in sessions {
        if sess.status.get() != FrameStatus::Capturing {
            continue;
        }
        if now.saturating_sub(sess.served.get()) < SERVE_MIN_NS {
            continue;
        }
        let SourceKind::Toplevel(weak) = &sess.source else {
            continue;
        };
        let win = weak.borrow().upgrade();
        let Some(win) = win else { continue };
        if tree_contains(&win.surface(), s.uid) {
            service(state, &sess);
        }
    }
}

fn tree_contains(s: &Rc<WlSurface>, uid: u64) -> bool {
    if s.uid == uid {
        return true;
    }
    let children = s.children.borrow();
    let Some(ch) = &*children else { return false };
    let subs: Vec<Rc<WlSurface>> = ch
        .below
        .iter()
        .chain(ch.above.iter())
        .map(|e| e.sub.surface.clone())
        .collect();
    drop(children);
    subs.iter().any(|c| tree_contains(c, uid))
}

pub fn window_unmapped(state: &Rc<State>, win: &Rc<Window>) {
    let sessions = state.icc_sessions.borrow().clone();
    for sess in sessions {
        let is_win = match &sess.source {
            SourceKind::Toplevel(w) => w.borrow().upgrade().is_some_and(|x| Rc::ptr_eq(&x, win)),
            _ => false,
        };
        if is_win {
            stop_session(state, &sess);
        }
    }
}

pub fn output_removed(state: &Rc<State>, name: &str) {
    let sessions = state.icc_sessions.borrow().clone();
    for sess in sessions {
        if matches!(&sess.source, SourceKind::Output(n) if n == name) {
            stop_session(state, &sess);
        }
    }
}

pub fn drop_client(state: &Rc<State>, id: ClientId) {
    state.icc_sessions.borrow_mut().retain(|s| s.client.id != id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, event_seq, test_client};
    use crate::protocol::MIN_SERVER_ID;
    use crate::shell::xdg::test_support::{mapped_toplevel, unmap_toplevel};
    use cursor_session_v1::Handler as _;
    use frame_v1::Handler as _;
    use manager_v1::Handler as _;
    use session_v1::Handler as _;
    use toplevel_source_mgr_v1::Handler as _;

    const ERR: ObjectId = ObjectId(1);
    const HANDLE: ObjectId = ObjectId(MIN_SERVER_ID);
    const SOURCE: ObjectId = ObjectId(72);
    const SESSION: ObjectId = ObjectId(73);
    const FRAME: ObjectId = ObjectId(74);

    /// mapped toplevel + ext handle + toplevel capture source + icc manager
    fn setup() -> (Rc<State>, Rc<Client>, Rc<WlSurface>, Rc<IccManager>) {
        let (state, client) = test_client();
        let (s, _xdg, _tl) = mapped_toplevel(&state, &client, [30, 10, 40, 50, 20]);
        crate::protocol::foreign_toplevel_list::ForeignToplevelListGlobal
            .bind(&client, ObjectId(60), 1)
            .unwrap();
        let tsm = Rc::new(ToplevelSourceManager {
            id: ObjectId(70),
            client: client.clone(),
            version: 1,
        });
        client.add_client_obj(tsm.clone()).unwrap();
        tsm.create_source(toplevel_source_mgr_v1::create_source::Request {
            source: SOURCE,
            toplevel_handle: HANDLE,
        })
        .unwrap();
        let mgr = Rc::new(IccManager {
            id: ObjectId(71),
            client: client.clone(),
            version: 1,
        });
        client.add_client_obj(mgr.clone()).unwrap();
        (state, client, s, mgr)
    }

    fn make_session(state: &Rc<State>, mgr: &Rc<IccManager>) -> Rc<IccSession> {
        mgr.create_session(manager_v1::create_session::Request {
            session: SESSION,
            source: SOURCE,
            options: 0,
        })
        .unwrap();
        state
            .icc_sessions
            .borrow()
            .iter()
            .find(|s| s.id == SESSION)
            .cloned()
            .unwrap()
    }

    fn make_frame(client: &Rc<Client>, sess: &Rc<IccSession>) -> Rc<IccFrame> {
        sess.create_frame(session_v1::create_frame::Request { frame: FRAME })
            .unwrap();
        let f = sess.frame.borrow().clone().unwrap();
        // a buffer the size of the advertised constraints
        let r = {
            let SourceKind::Toplevel(w) = &sess.source else { unreachable!() };
            let win = w.borrow().upgrade().unwrap();
            win.draw_rect(&client.state)
        };
        let b = crate::protocol::shm::test_buffer(client, ObjectId(75), r.width(), r.height());
        f.attach_buffer(frame_v1::attach_buffer::Request { buffer: b.id })
            .unwrap();
        f
    }

    #[test]
    fn session_bursts_constraints_then_one_done() {
        let (state, client, _s, mgr) = setup();
        let before = client.queued_out_bytes().len();
        make_session(&state, &mgr);
        let bytes = client.queued_out_bytes();
        let tail = &bytes[before..];
        let seq = event_seq(tail);
        assert_eq!(
            seq,
            vec![
                (SESSION.0, session_v1::shm_format::OPCODE),
                (SESSION.0, session_v1::shm_format::OPCODE),
                (SESSION.0, session_v1::buffer_size::OPCODE),
                (SESSION.0, session_v1::done::OPCODE),
            ]
        );
        // XRGB8888 leads: clients that allocate formats[0] get the safe one
        let first_format = u32::from_ne_bytes(tail[8..12].try_into().unwrap());
        assert_eq!(first_format, WL_SHM_XRGB8888);
        assert_eq!(count_events(tail, SESSION, session_v1::stopped::OPCODE), 0);
    }

    #[test]
    fn dead_source_session_stops_instead() {
        let (state, client, s, mgr) = setup();
        unmap_toplevel(&s);
        let before = client.queued_out_bytes().len();
        mgr.create_session(manager_v1::create_session::Request {
            session: SESSION,
            source: SOURCE,
            options: 0,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        let tail = &bytes[before..];
        assert_eq!(count_events(tail, SESSION, session_v1::stopped::OPCODE), 1);
        assert_eq!(count_events(tail, SESSION, session_v1::done::OPCODE), 0);
        assert!(state.icc_sessions.borrow().is_empty());
    }

    #[test]
    fn unknown_option_bits_are_an_error() {
        let (_state, client, _s, mgr) = setup();
        mgr.create_session(manager_v1::create_session::Request {
            session: SESSION,
            source: SOURCE,
            options: 2,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn second_frame_is_duplicate_frame() {
        let (state, client, _s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        sess.create_frame(session_v1::create_frame::Request { frame: FRAME })
            .unwrap();
        sess.create_frame(session_v1::create_frame::Request { frame: ObjectId(76) })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn capture_without_a_buffer_is_no_buffer() {
        let (state, client, _s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        sess.create_frame(session_v1::create_frame::Request { frame: FRAME })
            .unwrap();
        let f = sess.frame.borrow().clone().unwrap();
        f.capture(frame_v1::capture::Request {}).unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn bad_damage_rects_are_an_error() {
        let (state, client, _s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        let f = make_frame(&client, &sess);
        f.damage_buffer(frame_v1::damage_buffer::Request {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0, "valid rect passes");
        f.damage_buffer(frame_v1::damage_buffer::Request {
            x: -1,
            y: 0,
            width: 16,
            height: 16,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn stopped_precedes_the_frames_failure() {
        let (state, client, s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        let f = make_frame(&client, &sess);
        // park the capture on the content-change path
        sess.force.set(false);
        f.capture(frame_v1::capture::Request {}).unwrap();
        assert!(sess.status.get() == FrameStatus::Capturing);
        let before = client.queued_out_bytes().len();
        unmap_toplevel(&s);
        let bytes = client.queued_out_bytes();
        let seq: Vec<_> = event_seq(&bytes[before..])
            .into_iter()
            .filter(|&(obj, _)| obj == SESSION.0 || obj == FRAME.0)
            .collect();
        assert_eq!(
            seq,
            vec![
                (SESSION.0, session_v1::stopped::OPCODE),
                (FRAME.0, frame_v1::failed::OPCODE),
            ]
        );
        assert!(state.icc_sessions.borrow().is_empty());
    }

    #[test]
    fn requests_after_capture_are_already_captured() {
        let (state, client, s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        let f = make_frame(&client, &sess);
        unmap_toplevel(&s);
        // capture on a stopped session fails, never a protocol error
        let before = client.queued_out_bytes().len();
        f.capture(frame_v1::capture::Request {}).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes[before..], ERR, 0), 0);
        assert_eq!(
            count_events(&bytes[before..], FRAME, frame_v1::failed::OPCODE),
            1
        );
        // the frame is spent now: everything else is already_captured
        f.attach_buffer(frame_v1::attach_buffer::Request { buffer: ObjectId(75) })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn frame_destroy_frees_the_session_slot() {
        let (state, client, _s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        let f = make_frame(&client, &sess);
        f.destroy(frame_v1::destroy::Request {}).unwrap();
        assert!(sess.frame.borrow().is_none());
        assert!(sess.buffer.borrow().is_none());
        assert!(sess.status.get() == FrameStatus::Unused);
        // the slot is free for the next frame
        sess.create_frame(session_v1::create_frame::Request { frame: ObjectId(76) })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
    }

    #[test]
    fn ready_metadata_is_ordered() {
        let (state, _client, _s, mgr) = setup();
        let sess = make_session(&state, &mgr);
        let before = sess.client.queued_out_bytes().len();
        send_ready(&sess, ObjectId(99), 64, 64);
        let bytes = sess.client.queued_out_bytes();
        assert_eq!(
            event_seq(&bytes[before..]),
            vec![
                (99, frame_v1::transform::OPCODE),
                (99, frame_v1::damage::OPCODE),
                (99, frame_v1::presentation_time::OPCODE),
                (99, frame_v1::ready::OPCODE),
            ]
        );
    }

    #[test]
    fn cursor_sessions_stop_immediately() {
        let (_state, client, _s, mgr) = setup();
        mgr.create_pointer_cursor_session(manager_v1::create_pointer_cursor_session::Request {
            session: ObjectId(80),
            source: SOURCE,
            pointer: ObjectId(71),
        })
        .unwrap();
        assert!(client.objects.get(ObjectId(80)).is_some());
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
        let src = client.objects.capture_source(SOURCE).unwrap();
        let cs = Rc::new(IccCursorSession {
            id: ObjectId(81),
            client: client.clone(),
            version: 1,
            source: src.kind.snapshot(),
            have_session: Cell::new(false),
        });
        client.add_client_obj(cs.clone()).unwrap();
        let before = client.queued_out_bytes().len();
        cs.get_capture_session(cursor_session_v1::get_capture_session::Request {
            session: ObjectId(82),
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        let tail = &bytes[before..];
        // stopped at birth, never constraints
        assert_eq!(
            count_events(tail, ObjectId(82), session_v1::stopped::OPCODE),
            1
        );
        assert_eq!(count_events(tail, ObjectId(82), session_v1::done::OPCODE), 0);
        // only one capture session per cursor session
        cs.get_capture_session(cursor_session_v1::get_capture_session::Request {
            session: ObjectId(83),
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }
}
