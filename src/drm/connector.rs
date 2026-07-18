// per-output state and the two commit paths: full modeset and steady-state
// flip. EBUSY is a soft-fail (not-presented; next trigger rebuilds the commit).
// EACCES means the master went away (vt switch) - the caller's problem.

use crate::drm::atomic::{self, Change};
use crate::drm::device::{Crtc, DrmDevice, DrmError, Plane, PlaneType, PropSet};
use crate::drm::sys::{self, FlipComplete, ModeInfo};
use crate::drm::{ObjId, PropId};
use crate::format::{ARGB8888, XRGB8888};
use crate::util::AsyncEvent;
use rustix::io::Errno;
use std::cell::{Cell, RefCell};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::rc::Rc;

pub struct Connector {
    pub id: ObjId,
    prop_crtc_id: PropId,
    /// crtc the kernel had us on at probe; adopting it keeps the
    /// firmware-validated pipe pairing (480Hz modes may join pipes)
    live_crtc: Cell<ObjId>,
    /// clamp to the fb depth; deep color inflates link bandwidth for
    /// nothing when we render xrgb8888
    max_bpc: Option<(PropId, u64)>,
    /// stage GOOD every modeset; BAD after failed link training keeps the
    /// head dark until userspace re-commits
    link_status: Option<(PropId, u64)>,
    pub connected: Cell<bool>,
    /// kernel-style name, "DP-1"; what output config blocks match on
    pub name: String,
    pub vrr_capable: bool,
    /// desired vs programmed VRR_ENABLED; flips converge them
    pub vrr_want: Cell<bool>,
    vrr_cur: Cell<bool>,
    pub modes: RefCell<Vec<ModeInfo>>,
    pub pipe: RefCell<Option<Pipe>>,
    pub flip_pending: Cell<bool>,
    /// fires on every flip completion; the present loop hangs off this
    pub vblank: AsyncEvent,
    pub sequence: Cell<u32>,
    /// last flip's kernel timestamp, (tv_sec, tv_usec)
    pub flip_time: Cell<(u32, u32)>,
    /// flip sequence widened across u32 wraps
    pub seq64: Cell<u64>,
    out_fence: Cell<Option<OwnedFd>>,
    /// tearing is invisible in logs otherwise; announce the first async flip
    async_announced: Cell<bool>,
    /// reused commit buffer; vecs keep capacity across frames
    change: RefCell<Change>,
}

/// connector wired to hardware: crtc + planes + chosen mode
pub struct Pipe {
    pub crtc: Rc<Crtc>,
    pub primary: Rc<Plane>,
    pub cursor: Option<CursorPlane>,
    pub mode: ModeInfo,
    mode_blob: Cell<u32>,
    pub active: Cell<bool>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FlipResult {
    Queued,
    /// nothing reached the screen and nothing is pending; retry next trigger
    NotPresented,
}

impl Connector {
    pub fn probe(dev: &Rc<DrmDevice>, raw_id: u32) -> Result<Rc<Connector>, DrmError> {
        let id = ObjId(raw_id);
        let info = sys::connector(dev.fd.as_fd(), raw_id, true)
            .map_err(|e| DrmError::ObjOp("connector probe", raw_id, e))?;
        let props = PropSet::of(dev.fd.as_fd(), id, sys::OBJECT_CONNECTOR)?;
        let name = format!(
            "{}-{}",
            connector_type_name(info.connector_type),
            info.connector_type_id
        );
        let max_bpc = props.id("max bpc").and_then(|p| {
            let (min, max) = props.range("max bpc")?;
            Some((p, 8u64.clamp(min, max)))
        });
        let link_status = props.id("link-status").map(|p| {
            (p, props.enum_value("link-status", "Good").unwrap_or(0))
        });
        Ok(Rc::new(Connector {
            id,
            prop_crtc_id: props.require("CRTC_ID", id)?,
            live_crtc: Cell::new(ObjId(props.value("CRTC_ID").unwrap_or(0) as u32)),
            max_bpc,
            link_status,
            connected: Cell::new(info.connection == 1),
            name,
            vrr_capable: props.value("vrr_capable") == Some(1),
            vrr_want: Cell::new(false),
            vrr_cur: Cell::new(false),
            modes: RefCell::new(info.modes),
            pipe: RefCell::new(None),
            flip_pending: Cell::new(false),
            async_announced: Cell::new(false),
            vblank: AsyncEvent::default(),
            sequence: Cell::new(0),
            flip_time: Cell::new((0, 0)),
            seq64: Cell::new(0),
            out_fence: Cell::new(None),
            change: RefCell::new(Change::default()),
        }))
    }

    /// configured mode if it matches something advertised, else the panel's
    /// preferred (first) mode
    fn pick_mode(&self, prefer: Option<(u32, u32, Option<u32>)>) -> Option<ModeInfo> {
        let modes = self.modes.borrow();
        if let Some((w, h, hz)) = prefer {
            let mut best: Option<&ModeInfo> = None;
            for m in modes.iter() {
                if m.hdisplay as u32 != w || m.vdisplay as u32 != h {
                    continue;
                }
                best = match (best, hz) {
                    (None, _) => Some(m),
                    (Some(b), Some(hz)) => {
                        let d_new = (m.vrefresh as i64 - hz as i64).abs();
                        let d_old = (b.vrefresh as i64 - hz as i64).abs();
                        Some(if d_new < d_old { m } else { b })
                    }
                    // no refresh asked: the first (highest) match wins
                    (Some(b), None) => Some(b),
                };
            }
            if let Some(m) = best {
                return Some(*m);
            }
            eprintln!(
                "carrot: {}: no advertised mode matches {w}x{h}{}; using preferred",
                self.name,
                hz.map(|z| format!("@{z}")).unwrap_or_default()
            );
        }
        modes.first().copied()
    }

    /// next lower refresh at the same resolution, for the bandwidth ladder
    pub fn step_down_mode(&self) -> bool {
        let cur = match self.pipe.borrow().as_ref() {
            Some(p) => p.mode,
            None => return false,
        };
        let modes = self.modes.borrow();
        let next = modes
            .iter()
            .filter(|m| {
                m.hdisplay == cur.hdisplay
                    && m.vdisplay == cur.vdisplay
                    && m.vrefresh < cur.vrefresh
            })
            .max_by_key(|m| m.vrefresh)
            .copied();
        drop(modes);
        let Some(next) = next else { return false };
        if let Some(p) = self.pipe.borrow_mut().as_mut() {
            p.mode = next;
        }
        eprintln!(
            "carrot: {}: stepping down to {}x{}@{}",
            self.name, next.hdisplay, next.vdisplay, next.vrefresh
        );
        true
    }

    /// stage this head's full bring-up into a shared commit
    pub fn stage_modeset(&self, dev: &DrmDevice, fb: u32, ch: &mut Change) -> Result<u32, DrmError> {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return Ok(0);
        };
        let blob = create_mode_blob(dev, &pipe.mode)?;
        ch.set(self.id, self.prop_crtc_id, pipe.crtc.id.0 as u64);
        if let Some((prop, bpc)) = self.max_bpc {
            ch.set(self.id, prop, bpc);
        }
        if let Some((prop, good)) = self.link_status {
            ch.set(self.id, prop, good);
        }
        ch.set(pipe.crtc.id, pipe.crtc.props.active, 1);
        ch.set(pipe.crtc.id, pipe.crtc.props.mode_id, blob as u64);
        if let Some(prop) = pipe.crtc.props.vrr_enabled {
            ch.set(pipe.crtc.id, prop, self.vrr_want.get() as u64);
        }
        set_plane(ch, &pipe.primary, pipe.crtc.id, fb, &pipe.mode);
        if let Some(cur) = &pipe.cursor {
            cur.apply(ch, pipe.crtc.id);
        }
        Ok(blob)
    }

    /// bookkeeping after a shared commit landed for this head
    pub fn modeset_done(&self, dev: &DrmDevice, blob: u32) {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else { return };
        let old = pipe.mode_blob.replace(blob);
        if old != 0 {
            let _ = sys::destroy_blob(dev.fd.as_fd(), old);
        }
        if let Some(cur) = &pipe.cursor {
            cur.commit_done();
        }
        self.vrr_cur.set(self.vrr_want.get());
        pipe.active.set(true);
    }

    /// detach from whatever crtc firmware or a previous session left us on
    pub fn clear_routing(&self, ch: &mut Change) {
        ch.set(self.id, self.prop_crtc_id, 0);
    }

    /// the pipe is gone: any in-flight flip was retired by the disable
    /// commit, but its event routes by crtc.connector and that mapping is
    /// being severed - it will never reach flip_done. reset here or the
    /// flag gates every present of the next output built on this connector
    pub fn pipe_torn_down(&self) {
        self.flip_pending.set(false);
        self.vblank.trigger();
    }

    pub fn assign_pipe(
        self: &Rc<Self>,
        dev: &DrmDevice,
        prefer: Option<(u32, u32, Option<u32>)>,
    ) -> Result<(), DrmError> {
        if !self.connected.get() || self.pipe.borrow().is_some() {
            return Ok(());
        }
        let Some(mode) = self.pick_mode(prefer) else {
            // connected but modeless; treat like disconnected
            return Ok(());
        };

        let info = sys::connector(dev.fd.as_fd(), self.id.0, false)
            .map_err(|e| DrmError::ObjOp("connector", self.id.0, e))?;
        let mut crtc_mask = 0u32;
        for &enc in &info.encoders {
            crtc_mask |= sys::encoder_possible_crtcs(dev.fd.as_fd(), enc)
                .map_err(|e| DrmError::ObjOp("encoder", enc, e))?;
        }
        // the kernel's current routing first: firmware already proved that
        // pairing works, and high-refresh modes may gang adjacent pipes
        let live = self.live_crtc.get();
        let crtc = dev
            .crtcs
            .iter()
            .find(|c| c.id == live && live != ObjId(0) && c.connector.get() == ObjId(0))
            .or_else(|| {
                dev.crtcs
                    .iter()
                    .find(|c| crtc_mask & (1 << c.idx) != 0 && c.connector.get() == ObjId(0))
            })
            .cloned()
            .ok_or(DrmError::NoCrtc(self.id))?;

        let take_plane = |ty: PlaneType, fourcc: u32| {
            dev.planes
                .iter()
                .find(|p| {
                    p.ty == ty
                        && p.possible_crtcs & (1 << crtc.idx) != 0
                        && p.crtc.get() == ObjId(0)
                        && p.supports(fourcc)
                })
                .cloned()
        };
        let primary = take_plane(PlaneType::Primary, XRGB8888.drm)
            .ok_or(DrmError::NoPrimaryPlane(self.id))?;
        primary.crtc.set(crtc.id);
        // joined modes on xe: the kernel gangs two pipes and any cursor prop
        // we stage arms phantom doubled cursor state (the 1px smudge).
        // amdgpu joins internally and its cursor plane stays sane, so only
        // xe composites the cursor.
        let cursor = if mode_needs_joiner(&mode) && dev.driver == "xe" {
            eprintln!(
                "carrot: {}: joined mode, hardware cursor off (composited instead)",
                self.name
            );
            None
        } else {
            take_plane(PlaneType::Cursor, ARGB8888.drm).and_then(|p| {
            p.crtc.set(crtc.id);
            match CursorPlane::new(dev, p.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("carrot: cursor plane setup failed, continuing without: {e}");
                    p.crtc.set(ObjId(0));
                    None
                }
            }
            })
        };

        if prefer.is_some() {
            eprintln!(
                "carrot: {}: mode {}x{}@{}",
                self.name, mode.hdisplay, mode.vdisplay, mode.vrefresh
            );
        }
        // pixel clocks past one pipe's reach make the kernel gang the NEXT
        // pipe as an invisible joiner slave; hand that pipe to another head
        // and both starve mid-scanline (the dual-head 480Hz failure)
        if mode_needs_joiner(&mode) {
            if let Some(slave) = dev.crtcs.iter().find(|c| c.idx == crtc.idx + 1) {
                if slave.connector.get() == ObjId(0) {
                    slave.connector.set(self.id);
                    eprintln!(
                        "carrot: {}: {}MHz mode joins pipes; reserving pipe {}",
                        self.name,
                        mode.clock / 1000,
                        (b'A' + slave.idx as u8) as char
                    );
                }
            }
        }
        crtc.connector.set(self.id);
        *self.pipe.borrow_mut() = Some(Pipe {
            crtc,
            primary,
            cursor,
            mode,
            mode_blob: Cell::new(0),
            active: Cell::new(false),
        });
        Ok(())
    }

    /// steady-state page flip, one attempt. EBUSY is not-presented; the next
    /// trigger rebuilds the whole commit, so nothing partial survives
    pub fn flip(
        &self,
        dev: &DrmDevice,
        fb: u32,
        in_fence: Option<i32>,
    ) -> Result<FlipResult, DrmError> {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return Ok(FlipResult::NotPresented);
        };
        if self.flip_pending.get() {
            return Ok(FlipResult::NotPresented);
        }
        let build = |out_fd: &mut i32, ch: &mut Change, with_vrr: Option<bool>| {
            ch.clear();
            ch.set(
                pipe.crtc.id,
                pipe.crtc.props.out_fence_ptr,
                (out_fd as *mut i32) as usize as u64,
            );
            // full plane state every flip: partial updates tickle driver
            // bugs whose symptom is exactly stripe-corrupted scanout
            set_plane(ch, &pipe.primary, pipe.crtc.id, fb, &pipe.mode);
            if let (Some(prop), Some(fence)) = (pipe.primary.props.in_fence_fd, in_fence) {
                ch.set(pipe.primary.id, prop, fence as u64);
            }
            if let Some(cur) = &pipe.cursor {
                cur.apply(ch, pipe.crtc.id);
            }
            if let (Some(prop), Some(on)) = (pipe.crtc.props.vrr_enabled, with_vrr) {
                ch.set(pipe.crtc.id, prop, on as u64);
            }
        };
        // vrr rides the flip where drivers allow it; if they call it a
        // modeset instead, retry once without so the frame still lands
        let vrr = self.vrr_dirty().then(|| self.vrr_want.get());
        let mut out_fd: i32 = -1;
        let mut ch = self.change.borrow_mut();
        build(&mut out_fd, &mut ch, vrr);
        let mut res = ch.commit(dev.fd.as_fd(), atomic::NONBLOCK | atomic::PAGE_FLIP_EVENT, 0);
        let mut vrr_applied = vrr;
        if res.is_err() && vrr.is_some() && !matches!(res, Err(Errno::BUSY)) {
            build(&mut out_fd, &mut ch, None);
            res = ch.commit(dev.fd.as_fd(), atomic::NONBLOCK | atomic::PAGE_FLIP_EVENT, 0);
            vrr_applied = None;
            if res.is_ok() {
                eprintln!("carrot: {}: vrr change needs a modeset on this driver", self.name);
            }
        }
        match res {
            Ok(()) => {
                self.take_out_fence(out_fd);
                if let Some(cur) = &pipe.cursor {
                    cur.commit_done();
                }
                if let Some(on) = vrr_applied {
                    self.vrr_cur.set(on);
                    eprintln!("carrot: {}: vrr {}", self.name, if on { "on" } else { "off" });
                }
                self.flip_pending.set(true);
                Ok(FlipResult::Queued)
            }
            Err(Errno::BUSY) => Ok(FlipResult::NotPresented),
            Err(e) => Err(commit_error(e)),
        }
    }

    /// the next flip must carry a VRR_ENABLED change (so it can't be async)
    pub fn vrr_dirty(&self) -> bool {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return false;
        };
        pipe.crtc.props.vrr_enabled.is_some() && self.vrr_want.get() != self.vrr_cur.get()
    }

    /// tearing flip. the kernel only takes an async commit when FB_ID is the
    /// sole change, so no fences and no cursor ride along - the caller has
    /// already waited out the render, and cursor changes fall back to flip()
    pub fn flip_async(&self, dev: &DrmDevice, fb: u32) -> Result<FlipResult, DrmError> {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return Ok(FlipResult::NotPresented);
        };
        if self.flip_pending.get() {
            return Ok(FlipResult::NotPresented);
        }
        let mut ch = self.change.borrow_mut();
        ch.clear();
        ch.set(pipe.primary.id, pipe.primary.props.fb_id, fb as u64);
        let flags = atomic::NONBLOCK | atomic::PAGE_FLIP_EVENT | atomic::PAGE_FLIP_ASYNC;
        match ch.commit(dev.fd.as_fd(), flags, 0) {
            Ok(()) => {
                crate::trace!("async flip queued");
                if !self.async_announced.replace(true) {
                    eprintln!("carrot: tearing: async flips active");
                }
                self.flip_pending.set(true);
                Ok(FlipResult::Queued)
            }
            Err(Errno::BUSY) => Ok(FlipResult::NotPresented),
            Err(e) => Err(commit_error(e)),
        }
    }

    /// pending cursor plane changes would disqualify an async commit
    pub fn cursor_changed(&self) -> bool {
        let pipe = self.pipe.borrow();
        pipe.as_ref()
            .and_then(|p| p.cursor.as_ref().map(|c| c.changed()))
            .unwrap_or(false)
    }

    /// rip the cursor off the plane on vt-away while master is still held, so
    /// the next compositor never sees our arrow. commit BLOCKS: a flip is nearly
    /// always in flight and a nonblock attempt would EBUSY, leaving the arrow.
    pub fn cursor_hide(&self, dev: &DrmDevice) {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return;
        };
        let Some(cur) = &pipe.cursor else {
            return;
        };
        // unconditional: soft state may disagree with the kernel's view, which
        // is what actually bleeds through
        let p = &cur.plane;
        let mut ch = self.change.borrow_mut();
        ch.clear();
        ch.set(p.id, p.props.fb_id, 0);
        ch.set(p.id, p.props.crtc_id, 0);
        if let Err(e) = ch.commit(dev.fd.as_fd(), 0, 0) {
            eprintln!("carrot: cursor teardown failed, expect a stray arrow: {e}");
        }
        cur.set_enabled(false);
        cur.commit_done();
    }

    /// cursor-only update between flips; a pending flip already carries the
    /// change. no out-fence: nothing new gets scanned out. false = the
    /// kernel rejected the plane outright - the caller should stop trusting
    /// it and composite the cursor instead
    pub fn cursor_commit(&self, dev: &DrmDevice) -> bool {
        if self.flip_pending.get() {
            return true;
        }
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return true;
        };
        if !pipe.active.get() {
            return true;
        }
        let Some(cur) = &pipe.cursor else {
            return true;
        };
        if !cur.changed() {
            return true;
        }
        let mut ch = self.change.borrow_mut();
        ch.clear();
        cur.apply(&mut ch, pipe.crtc.id);
        match ch.commit(dev.fd.as_fd(), atomic::NONBLOCK, 0) {
            Ok(()) => {
                cur.commit_done();
                true
            }
            // contention: next motion retries
            Err(Errno::BUSY) => true,
            Err(e) => {
                eprintln!(
                    "carrot: {}: cursor plane commit failed ({e}); compositing the cursor instead",
                    self.name
                );
                false
            }
        }
    }

    pub fn flip_done(&self, ev: &FlipComplete) {
        self.flip_pending.set(false);
        self.sequence.set(ev.sequence);
        self.flip_time.set((ev.tv_sec, ev.tv_usec));
        let prev = self.seq64.get();
        let hi = if ev.sequence < prev as u32 { (prev >> 32) + 1 } else { prev >> 32 };
        self.seq64.set(hi << 32 | ev.sequence as u64);
        self.vblank.trigger();
    }

    /// previous frame's scanout fence; the renderer waits on it before
    /// drawing into the buffer being replaced
    pub fn take_scanout_fence(&self) -> Option<OwnedFd> {
        self.out_fence.take()
    }

    fn take_out_fence(&self, out_fd: i32) {
        if out_fd != -1 {
            self.out_fence
                .set(Some(unsafe { OwnedFd::from_raw_fd(out_fd) }));
        }
    }
}

fn set_plane(ch: &mut Change, plane: &Plane, crtc_id: ObjId, fb: u32, mode: &ModeInfo) {
    let w = mode.hdisplay as u64;
    let h = mode.vdisplay as u64;
    ch.set(plane.id, plane.props.crtc_id, crtc_id.0 as u64);
    ch.set(plane.id, plane.props.fb_id, fb as u64);
    ch.set(plane.id, plane.props.src_x, 0);
    ch.set(plane.id, plane.props.src_y, 0);
    ch.set(plane.id, plane.props.src_w, w << 16);
    ch.set(plane.id, plane.props.src_h, h << 16);
    ch.set(plane.id, plane.props.crtc_x, 0);
    ch.set(plane.id, plane.props.crtc_y, 0);
    ch.set(plane.id, plane.props.crtc_w, w);
    ch.set(plane.id, plane.props.crtc_h, h);
}

fn create_mode_blob(dev: &DrmDevice, mode: &ModeInfo) -> Result<u32, DrmError> {
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (mode as *const ModeInfo) as *const u8,
            std::mem::size_of::<ModeInfo>(),
        )
    };
    sys::create_blob(dev.fd.as_fd(), bytes).map_err(|e| DrmError::Op("mode blob", e))
}

fn commit_error(e: Errno) -> DrmError {
    if e == Errno::ACCESS {
        DrmError::LostMaster
    } else {
        DrmError::Op("atomic commit", e)
    }
}

// -- cursor --

/// double-buffered dumb buffers, cpu-written, ARGB. changes ride along in
/// whatever commit goes out next; a bare move without content damage still
/// needs a flip-shaped commit from the caller
pub struct CursorPlane {
    pub plane: Rc<Plane>,
    bufs: [CursorBuf; 2],
    back: Cell<usize>,
    pub width: u32,
    pub height: u32,
    x: Cell<i32>,
    y: Cell<i32>,
    enabled: Cell<bool>,
    changed: Cell<bool>,
    /// back buffer got new content, rotate on next commit
    swap: Cell<bool>,
    /// subtracted from the position so the click point sits under the tip
    pub hotspot: Cell<(i32, i32)>,
}

struct CursorBuf {
    fd: Rc<OwnedFd>,
    handle: u32,
    fb: u32,
    pitch: u32,
    size: u64,
    map: *mut u8,
}

impl CursorPlane {
    pub fn new(dev: &DrmDevice, plane: Rc<Plane>) -> Result<CursorPlane, DrmError> {
        let (w, h) = dev.cursor_size;
        let mk = || -> Result<CursorBuf, DrmError> {
            let db = sys::create_dumb(dev.fd.as_fd(), w, h, 32)
                .map_err(|e| DrmError::Op("create cursor dumb", e))?;
            let offset = sys::map_dumb(dev.fd.as_fd(), db.handle)
                .map_err(|e| DrmError::Op("map cursor dumb", e))?;
            let map = unsafe {
                rustix::mm::mmap(
                    std::ptr::null_mut(),
                    db.size as usize,
                    rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                    rustix::mm::MapFlags::SHARED,
                    dev.fd.as_fd(),
                    offset,
                )
                .map_err(|e| DrmError::Op("mmap cursor", e))?
            } as *mut u8;
            let fb = sys::addfb2(
                dev.fd.as_fd(),
                w,
                h,
                ARGB8888.drm,
                &[db.handle],
                &[db.pitch],
                &[0],
                None,
            )
            .map_err(|e| DrmError::Op("cursor addfb2", e))?;
            Ok(CursorBuf {
                fd: dev.fd.clone(),
                handle: db.handle,
                fb,
                pitch: db.pitch,
                size: db.size,
                map,
            })
        };
        Ok(CursorPlane {
            plane,
            bufs: [mk()?, mk()?],
            back: Cell::new(0),
            width: w,
            height: h,
            x: Cell::new(0),
            y: Cell::new(0),
            enabled: Cell::new(false),
            changed: Cell::new(false),
            swap: Cell::new(false),
            hotspot: Cell::new((0, 0)),
        })
    }

    /// drop off the crtc entirely (head teardown)
    pub fn clear_routing(&self, ch: &mut Change) {
        ch.set(self.plane.id, self.plane.props.fb_id, 0);
        ch.set(self.plane.id, self.plane.props.crtc_id, 0);
    }

    /// cleared first so a smaller cursor never leaves stale edges behind
    pub fn write(&self, pixels: &[u8], w: u32, h: u32) {
        let w = w.min(self.width);
        let h = h.min(self.height);
        let buf = &self.bufs[self.back.get()];
        let src_stride = (w * 4) as usize;
        unsafe {
            std::ptr::write_bytes(buf.map, 0, buf.size as usize);
            for row in 0..h as usize {
                let src = &pixels[row * src_stride..][..src_stride];
                let dst = buf.map.add(row * buf.pitch as usize);
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src_stride);
            }
        }
        self.swap.set(true);
        self.changed.set(true);
    }

    pub fn set_position(&self, x: i32, y: i32) {
        if self.x.get() != x || self.y.get() != y {
            self.x.set(x);
            self.y.set(y);
            self.changed.set(true);
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.get() != enabled {
            self.enabled.set(enabled);
            self.changed.set(true);
        }
    }

    pub fn changed(&self) -> bool {
        self.changed.get()
    }

    fn apply(&self, ch: &mut Change, crtc_id: ObjId) {
        if !self.changed.get() {
            return;
        }
        let p = &self.plane;
        if self.enabled.get() {
            // fresh content in the back buffer shows that, else the front one
            let idx = if self.swap.get() {
                self.back.get()
            } else {
                1 - self.back.get()
            };
            let w = self.width as u64;
            let h = self.height as u64;
            ch.set(p.id, p.props.crtc_id, crtc_id.0 as u64);
            ch.set(p.id, p.props.fb_id, self.bufs[idx].fb as u64);
            ch.set(p.id, p.props.src_x, 0);
            ch.set(p.id, p.props.src_y, 0);
            ch.set(p.id, p.props.src_w, w << 16);
            ch.set(p.id, p.props.src_h, h << 16);
            ch.set(p.id, p.props.crtc_x, self.x.get() as i64 as u64);
            ch.set(p.id, p.props.crtc_y, self.y.get() as i64 as u64);
            ch.set(p.id, p.props.crtc_w, w);
            ch.set(p.id, p.props.crtc_h, h);
        } else {
            ch.set(p.id, p.props.fb_id, 0);
            ch.set(p.id, p.props.crtc_id, 0);
        }
    }

    fn commit_done(&self) {
        if self.swap.get() {
            self.back.set(1 - self.back.get());
            self.swap.set(false);
        }
        self.changed.set(false);
    }
}

impl Drop for CursorBuf {
    fn drop(&mut self) {
        unsafe {
            let _ = rustix::mm::munmap(self.map.cast(), self.size as usize);
        }
        let _ = sys::rmfb(self.fd.as_fd(), self.fb);
        let _ = sys::destroy_dumb(self.fd.as_fd(), self.handle);
    }
}

/// past ~1GHz one display pipe can't clock the mode alone; the exact
/// driver threshold varies, erring low only over-reserves a pipe
pub fn mode_needs_joiner(mode: &ModeInfo) -> bool {
    mode.clock > 1_000_000
}

/// kernel connector-type names, drm_connector_enum_list verbatim
fn connector_type_name(ty: u32) -> &'static str {
    match ty {
        1 => "VGA",
        2 => "DVI-I",
        3 => "DVI-D",
        4 => "DVI-A",
        5 => "Composite",
        6 => "SVIDEO",
        7 => "LVDS",
        8 => "Component",
        9 => "DIN",
        10 => "DP",
        11 => "HDMI-A",
        12 => "HDMI-B",
        13 => "TV",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        17 => "DPI",
        18 => "Writeback",
        19 => "SPI",
        20 => "USB",
        _ => "Unknown",
    }
}
