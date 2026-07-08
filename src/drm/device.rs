// per-card object graph. property ids, plane formats, crtc bit indices - all
// resolved once at bring-up so steady-state commits never string-match.

use crate::drm::connector::Connector;
use crate::drm::sys;
use crate::drm::{ObjId, PropId};
use crate::engine::{Engine, SpawnedFuture};
use crate::uring::Ring;
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::rc::Rc;

#[derive(Debug)]
pub enum DrmError {
    Op(&'static str, Errno),
    /// like Op but names the kms object, when "plane: EINVAL" pins nothing
    ObjOp(&'static str, u32, Errno),
    NotAtomic(Errno),
    MissingProp(String, ObjId),
    NoCrtc(ObjId),
    NoPrimaryPlane(ObjId),
    LostMaster,
}

impl fmt::Display for DrmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DrmError::Op(what, e) => write!(f, "{what}: {e}"),
            DrmError::ObjOp(what, id, e) => write!(f, "{what} {id}: {e}"),
            DrmError::NotAtomic(e) => write!(f, "device does not support atomic modesetting: {e}"),
            DrmError::MissingProp(name, obj) => {
                write!(f, "object {obj} has no {name} property")
            }
            DrmError::NoCrtc(conn) => write!(f, "no free crtc for connector {conn}"),
            DrmError::NoPrimaryPlane(conn) => {
                write!(f, "no usable primary plane for connector {conn}")
            }
            DrmError::LostMaster => write!(f, "drm master was revoked"),
        }
    }
}

impl std::error::Error for DrmError {}

// -- properties --

pub struct PropSet {
    map: HashMap<String, (u32, u64, Vec<u64>, Vec<sys::PropertyEnum>)>,
}

impl PropSet {
    pub fn of(fd: BorrowedFd<'_>, obj: ObjId, ty: u32) -> Result<PropSet, DrmError> {
        let raw = sys::object_properties(fd, obj.0, ty)
            .map_err(|e| DrmError::ObjOp("object properties", obj.0, e))?;
        let mut map = HashMap::new();
        for (prop, value) in raw {
            let meta = sys::property_meta(fd, prop)
                .map_err(|e| DrmError::ObjOp("property meta", prop, e))?;
            map.insert(meta.name, (prop, value, meta.values, meta.enums));
        }
        Ok(PropSet { map })
    }

    pub fn id(&self, name: &str) -> Option<PropId> {
        self.map.get(name).map(|(id, ..)| PropId(*id))
    }

    pub fn value(&self, name: &str) -> Option<u64> {
        self.map.get(name).map(|(_, v, ..)| *v)
    }

    /// [min, max] of a RANGE property
    pub fn range(&self, name: &str) -> Option<(u64, u64)> {
        self.map.get(name).and_then(|(_, _, vals, _)| match vals.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        })
    }

    /// numeric value of an ENUM entry, by name
    pub fn enum_value(&self, prop: &str, entry: &str) -> Option<u64> {
        self.map.get(prop).and_then(|(.., enums)| {
            enums.iter().find(|e| e.name() == entry).map(|e| e.value())
        })
    }

    pub fn require(&self, name: &str, obj: ObjId) -> Result<PropId, DrmError> {
        self.id(name)
            .ok_or_else(|| DrmError::MissingProp(name.to_string(), obj))
    }
}

// -- crtc --

pub struct CrtcProps {
    pub active: PropId,
    pub mode_id: PropId,
    pub out_fence_ptr: PropId,
    /// not all drivers expose it; vrr silently stays off without it
    pub vrr_enabled: Option<PropId>,
}

pub struct Crtc {
    pub id: ObjId,
    /// bit index in possible_crtcs masks
    pub idx: usize,
    pub props: CrtcProps,
    /// connector id currently driving this crtc, 0 = free
    pub connector: Cell<ObjId>,
}

// -- plane --

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PlaneType {
    Primary,
    Cursor,
    Overlay,
}

pub struct PlaneProps {
    pub crtc_id: PropId,
    pub fb_id: PropId,
    pub src_x: PropId,
    pub src_y: PropId,
    pub src_w: PropId,
    pub src_h: PropId,
    pub crtc_x: PropId,
    pub crtc_y: PropId,
    pub crtc_w: PropId,
    pub crtc_h: PropId,
    pub in_fence_fd: Option<PropId>,
}

pub struct Plane {
    pub id: ObjId,
    pub ty: PlaneType,
    pub possible_crtcs: u32,
    /// fourcc -> modifiers. empty list = no IN_FORMATS, plain format array only
    pub formats: Vec<(u32, Vec<u64>)>,
    pub props: PlaneProps,
    /// crtc id this plane is bound to, 0 = free
    pub crtc: Cell<ObjId>,
}

impl Plane {
    pub fn supports(&self, fourcc: u32) -> bool {
        self.formats.iter().any(|(f, _)| *f == fourcc)
    }

    pub fn modifiers(&self, fourcc: u32) -> &[u64] {
        self.formats
            .iter()
            .find(|(f, _)| *f == fourcc)
            .map(|(_, m)| m.as_slice())
            .unwrap_or(&[])
    }
}

// -- device --

pub struct DrmDevice {
    pub fd: Rc<OwnedFd>,
    /// kernel driver name; gates driver-private ioctls
    pub driver: String,
    pub cursor_size: (u32, u32),
    /// kernel takes PAGE_FLIP_ASYNC on atomic commits (6.8+); tearing needs it
    pub supports_async_flip: bool,
    pub crtcs: Vec<Rc<Crtc>>,
    pub planes: Vec<Rc<Plane>>,
    pub connectors: RefCell<Vec<Rc<Connector>>>,
}

impl DrmDevice {
    pub fn open(path: &Path) -> Result<Rc<DrmDevice>, DrmError> {
        let fd: OwnedFd = open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| DrmError::Op("open card", e))?;
        Self::with_fd(Rc::new(fd))
    }

    /// production path: the fd came from logind TakeDevice
    pub fn with_fd(fd: Rc<OwnedFd>) -> Result<Rc<DrmDevice>, DrmError> {
        // 2 = atomic + aspect-ratio bits; older kernels take only 1. either is
        // fine, no cap is not
        if sys::set_client_cap(fd.as_fd(), sys::CLIENT_CAP_ATOMIC, 2).is_err() {
            sys::set_client_cap(fd.as_fd(), sys::CLIENT_CAP_ATOMIC, 1)
                .map_err(DrmError::NotAtomic)?;
        }

        let driver = sys::driver_name(fd.as_fd()).unwrap_or_default();
        let cursor_size = (
            sys::get_cap(fd.as_fd(), sys::CAP_CURSOR_WIDTH).unwrap_or(64) as u32,
            sys::get_cap(fd.as_fd(), sys::CAP_CURSOR_HEIGHT).unwrap_or(64) as u32,
        );
        let supports_async_flip =
            sys::get_cap(fd.as_fd(), sys::CAP_ATOMIC_ASYNC_PAGE_FLIP).unwrap_or(0) == 1;

        let res = sys::resources(fd.as_fd()).map_err(|e| DrmError::Op("resources", e))?;
        let mut crtcs = Vec::new();
        for (idx, &raw) in res.crtcs.iter().enumerate() {
            let id = ObjId(raw);
            let props = PropSet::of(fd.as_fd(), id, sys::OBJECT_CRTC)?;
            crtcs.push(Rc::new(Crtc {
                id,
                idx,
                props: CrtcProps {
                    active: props.require("ACTIVE", id)?,
                    mode_id: props.require("MODE_ID", id)?,
                    out_fence_ptr: props.require("OUT_FENCE_PTR", id)?,
                    vrr_enabled: props.id("VRR_ENABLED"),
                },
                connector: Cell::new(ObjId(0)),
            }));
        }

        let plane_ids =
            sys::plane_resources(fd.as_fd()).map_err(|e| DrmError::Op("plane resources", e))?;
        let mut planes = Vec::new();
        for raw in plane_ids {
            let id = ObjId(raw);
            let info =
                sys::plane(fd.as_fd(), raw).map_err(|e| DrmError::ObjOp("plane", raw, e))?;
            let props = PropSet::of(fd.as_fd(), id, sys::OBJECT_PLANE)?;
            let ty = plane_type(fd.as_fd(), &props);
            let formats = match props.value("IN_FORMATS") {
                Some(blob_id) if blob_id != 0 => {
                    let blob = sys::get_blob(fd.as_fd(), blob_id as u32)
                        .map_err(|e| DrmError::ObjOp("IN_FORMATS blob", raw, e))?;
                    sys::parse_in_formats(&blob)
                }
                _ => info.formats.iter().map(|&f| (f, Vec::new())).collect(),
            };
            planes.push(Rc::new(Plane {
                id,
                ty,
                possible_crtcs: info.possible_crtcs,
                formats,
                props: PlaneProps {
                    crtc_id: props.require("CRTC_ID", id)?,
                    fb_id: props.require("FB_ID", id)?,
                    src_x: props.require("SRC_X", id)?,
                    src_y: props.require("SRC_Y", id)?,
                    src_w: props.require("SRC_W", id)?,
                    src_h: props.require("SRC_H", id)?,
                    crtc_x: props.require("CRTC_X", id)?,
                    crtc_y: props.require("CRTC_Y", id)?,
                    crtc_w: props.require("CRTC_W", id)?,
                    crtc_h: props.require("CRTC_H", id)?,
                    in_fence_fd: props.id("IN_FENCE_FD"),
                },
                crtc: Cell::new(ObjId(0)),
            }));
        }

        let dev = Rc::new(DrmDevice {
            fd,
            driver,
            cursor_size,
            supports_async_flip,
            crtcs,
            planes,
            connectors: RefCell::new(Vec::new()),
        });

        let mut connectors = Vec::new();
        for &id in &res.connectors {
            connectors.push(Connector::probe(&dev, id)?);
        }
        *dev.connectors.borrow_mut() = connectors;
        Ok(dev)
    }

    pub fn crtc(&self, id: ObjId) -> Option<&Rc<Crtc>> {
        self.crtcs.iter().find(|c| c.id == id)
    }

    /// greedy first-fit over every connected, unassigned connector; errors on
    /// the first that can't be satisfied - no partial silent skips
    pub fn assign_pipes(
        &self,
        prefer: &dyn Fn(&str) -> Option<(u32, u32, Option<u32>)>,
    ) -> Result<(), DrmError> {
        for conn in self.connectors.borrow().iter() {
            conn.assign_pipe(self, prefer(&conn.name))?;
        }
        Ok(())
    }

    /// bring every staged head up in ONE commit so the driver validates the
    /// combined bandwidth; on rejection, walk refresh rates down on the
    /// newest heads until it fits (the dual-head FIFO-underrun fix)
    pub fn modeset_heads(&self, heads: &[(Rc<Connector>, u32)]) -> Result<(), DrmError> {
        if heads.is_empty() {
            return Ok(());
        }
        loop {
            let mut ch = crate::drm::atomic::Change::default();
            // disables ride the same commit, never a follow-up: connectors we
            // don't drive detach, foreign crtcs go inactive, and every plane we
            // don't own clears
            let mut used_crtcs = Vec::new();
            let mut used_planes = Vec::new();
            for (conn, _) in heads {
                if let Some(p) = conn.pipe.borrow().as_ref() {
                    used_crtcs.push(p.crtc.id);
                    used_planes.push(p.primary.id);
                    if let Some(cur) = &p.cursor {
                        used_planes.push(cur.plane.id);
                    }
                }
            }
            for conn in self.connectors.borrow().iter() {
                if !heads.iter().any(|(c, _)| Rc::ptr_eq(c, conn)) {
                    conn.clear_routing(&mut ch);
                }
            }
            // reserved joiner slaves belong to the kernel: staging ANY prop
            // on the slave crtc OR its planes fights the join (the cursor
            // smudge). collect their pipe bits and steer around them.
            let mut slave_mask = 0u32;
            for crtc in &self.crtcs {
                let reserved =
                    crtc.connector.get() != ObjId(0) && !used_crtcs.contains(&crtc.id);
                if reserved {
                    slave_mask |= 1 << crtc.idx;
                    continue;
                }
                if !used_crtcs.contains(&crtc.id) && crtc.connector.get() == ObjId(0) {
                    ch.set(crtc.id, crtc.props.active, 0);
                    ch.set(crtc.id, crtc.props.mode_id, 0);
                }
            }
            for plane in &self.planes {
                if used_planes.contains(&plane.id) {
                    continue;
                }
                if plane.possible_crtcs & slave_mask != 0 {
                    continue;
                }
                ch.set(plane.id, plane.props.fb_id, 0);
                ch.set(plane.id, plane.props.crtc_id, 0);
            }
            let mut blobs = Vec::with_capacity(heads.len());
            let mut stage_err = None;
            for (conn, fb) in heads {
                match conn.stage_modeset(self, *fb, &mut ch) {
                    Ok(b) => blobs.push(b),
                    Err(e) => {
                        stage_err = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = stage_err {
                for b in blobs {
                    if b != 0 {
                        let _ = sys::destroy_blob(self.fd.as_fd(), b);
                    }
                }
                return Err(e);
            }
            match ch.commit(self.fd.as_fd(), crate::drm::atomic::ALLOW_MODESET, 0) {
                Ok(()) => {
                    for ((conn, _), blob) in heads.iter().zip(blobs) {
                        conn.modeset_done(self, blob);
                        if let Some(p) = conn.pipe.borrow().as_ref() {
                            eprintln!(
                                "carrot: modeset ok: {} {}x{}@{} on crtc idx {} (pipe {})",
                                conn.name,
                                p.mode.hdisplay,
                                p.mode.vdisplay,
                                p.mode.vrefresh,
                                p.crtc.idx,
                                (b'A' + p.crtc.idx as u8) as char
                            );
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    for b in blobs {
                        if b != 0 {
                            let _ = sys::destroy_blob(self.fd.as_fd(), b);
                        }
                    }
                    // only a validation rejection means "doesn't fit";
                    // anything else (lost master, dead device) is fatal
                    let bandwidth = matches!(e, Errno::INVAL | Errno::NOSPC | Errno::RANGE);
                    // newest head steps down first; primaries degrade last
                    let stepped = bandwidth && heads.iter().rev().any(|(c, _)| c.step_down_mode());
                    if !stepped {
                        return Err(DrmError::Op("modeset", e));
                    }
                }
            }
        }
    }

    /// one task per card: read flip completions off the drm fd and route them
    /// to the connector whose crtc finished scanning
    pub fn spawn_flip_pump(
        self: &Rc<Self>,
        engine: &Rc<Engine>,
        ring: &Rc<Ring>,
    ) -> SpawnedFuture<()> {
        let dev = self.clone();
        let ring = ring.clone();
        engine.spawn("drm events", async move {
            let mut buf = vec![0u8; 1024];
            loop {
                let (b, n) = match ring.read(&dev.fd, buf).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("carrot: drm event read failed: {e}");
                        return;
                    }
                };
                buf = b;
                for ev in sys::parse_flip_events(&buf[..n]) {
                    let Some(crtc) = dev.crtc(ObjId(ev.crtc_id)) else {
                        continue;
                    };
                    let conn_id = crtc.connector.get();
                    if conn_id != ObjId(0) {
                        let conn = dev
                            .connectors
                            .borrow()
                            .iter()
                            .find(|c| c.id == conn_id)
                            .cloned();
                        if let Some(conn) = conn {
                            conn.flip_done(&ev);
                        }
                    }
                }
            }
        })
    }
}

/// dev diagnostic (`carrot drm-probe`): bring up the object graph on every
/// card and dump pipe assignments. needs no drm master
pub fn probe_dump() -> i32 {
    let mut cards: Vec<_> = match std::fs::read_dir("/dev/dri") {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            eprintln!("cannot read /dev/dri: {e}");
            return 1;
        }
    };
    cards.sort();
    let mut failed = false;
    for path in cards {
        println!("=== {} ===", path.display());
        let dev = match DrmDevice::open(&path) {
            Ok(dev) => dev,
            Err(e) => {
                println!("FAIL: {e}");
                failed = true;
                continue;
            }
        };
        println!(
            "driver {:?}, {} crtcs, {} planes, cursor {}x{}",
            dev.driver,
            dev.crtcs.len(),
            dev.planes.len(),
            dev.cursor_size.0,
            dev.cursor_size.1
        );
        if let Err(e) = dev.assign_pipes(&|_| None) {
            println!("FAIL: pipe assignment: {e}");
            failed = true;
            continue;
        }
        for conn in dev.connectors.borrow().iter() {
            let pipe = conn.pipe.borrow();
            match (conn.connected.get(), pipe.as_ref()) {
                (true, Some(p)) => println!(
                    "connector {}: crtc {}, primary plane {}, cursor {}, mode {} ({} modifiers)",
                    conn.id,
                    p.crtc.id,
                    p.primary.id,
                    p.cursor
                        .as_ref()
                        .map(|c| c.plane.id.to_string())
                        .unwrap_or_else(|| "none".into()),
                    p.mode.name(),
                    p.primary.modifiers(crate::format::XRGB8888.drm).len(),
                ),
                (true, None) => println!("connector {}: connected, no pipe", conn.id),
                (false, _) => println!("connector {}: disconnected", conn.id),
            }
        }
    }
    failed as i32
}

fn plane_type(fd: BorrowedFd<'_>, props: &PropSet) -> PlaneType {
    let (Some(id), Some(value)) = (props.id("type"), props.value("type")) else {
        return PlaneType::Overlay;
    };
    let Ok(meta) = sys::property_meta(fd, id.0) else {
        return PlaneType::Overlay;
    };
    for e in &meta.enums {
        if e.value == value {
            return match e.name().as_str() {
                "Primary" => PlaneType::Primary,
                "Cursor" => PlaneType::Cursor,
                _ => PlaneType::Overlay,
            };
        }
    }
    PlaneType::Overlay
}
