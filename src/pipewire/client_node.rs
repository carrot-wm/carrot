// the pipewire client-node: a Video/Source the graph can consume. server
// allocates memfd buffers out of its AddMem pool; we fill them, flip the
// io area to HAVE_DATA and, as the driver, kick every peer's activation
// eventfd. P1 scope: one output port, one fixed memfd format.

use super::pod::{PodBuilder, PodParser};
use super::{Frame, PwConn, PwError};
use rustix::fd::OwnedFd;
use std::collections::HashMap;
use std::rc::Rc;

// client-node methods and events
const CN_UPDATE: u8 = 2;
const CN_PORT_UPDATE: u8 = 3;
const CN_SET_ACTIVE: u8 = 4;
const EV_CN_TRANSPORT: u8 = 0;
const EV_CN_SET_PARAM: u8 = 1;
const EV_CN_PORT_SET_PARAM: u8 = 7;
const EV_CN_PORT_USE_BUFFERS: u8 = 8;
const EV_CN_PORT_SET_IO: u8 = 9;
const EV_CN_SET_ACTIVATION: u8 = 10;
// core events beyond the P0 set
const EV_CORE_ADD_MEM: u8 = 6;
const EV_CORE_REMOVE_MEM: u8 = 7;
const EV_CORE_BOUND_ID: u8 = 5;

pub const CLIENT_NODE_VERSION: i32 = 4;

/// the three port params: EnumFormat, Buffers, Meta. framerate is left
/// unconstrained on purpose - casts are damage-driven and consumers pin
/// their own rate; a fixed fraction here empties the intersection with
/// their EnumFormat and the link dies with "no more input formats"
fn port_params(b: &mut PodBuilder, w: u32, h: u32) {
    b.object(OBJ_FORMAT, PARAM_ENUM_FORMAT, |b| {
        b.prop(FMT_MEDIA_TYPE, |b| b.id(MEDIA_TYPE_VIDEO));
        b.prop(FMT_MEDIA_SUBTYPE, |b| b.id(MEDIA_SUBTYPE_RAW));
        b.prop(FMT_VIDEO_FORMAT, |b| b.choice_enum_id(VIDEO_FORMAT_BGRX, &[VIDEO_FORMAT_BGRX]));
        b.prop(FMT_VIDEO_SIZE, |b| b.rectangle(w, h));
    });
    b.object(OBJ_PARAM_BUFFERS, PARAM_BUFFERS, |b| {
        b.prop(BUFFERS_BUFFERS, |b| b.choice_range_int(3, 2, 8));
        b.prop(BUFFERS_BLOCKS, |b| b.int(1));
        b.prop(BUFFERS_SIZE, |b| b.int((w * h * 4) as i32));
        b.prop(BUFFERS_STRIDE, |b| b.int((w * 4) as i32));
        b.prop(BUFFERS_DATATYPE, |b| {
            b.choice(super::pod::CHOICE_FLAGS, super::pod::T_INT, 4, &[&(1u32 << DATA_MEM_FD).to_le_bytes()]);
        });
    });
    b.object(OBJ_PARAM_META, PARAM_META, |b| {
        b.prop(META_TYPE, |b| b.id(META_HEADER));
        b.prop(META_SIZE, |b| b.int(META_HEADER_BYTES as i32));
    });
}

// update masks
const UPDATE_PARAMS: u32 = 1 << 0;
const UPDATE_INFO: u32 = 1 << 1;
const NODE_CHANGE_FLAGS: u64 = 1 << 0;
const NODE_CHANGE_PARAMS: u64 = 1 << 2;
const PORT_CHANGE_FLAGS: u64 = 1 << 0;
const PORT_CHANGE_RATE: u64 = 1 << 1;
const PORT_CHANGE_PARAMS: u64 = 1 << 3;
const PARAM_INFO_SERIAL: u32 = 1 << 0;
const PARAM_INFO_READ: u32 = 1 << 1;
const DIRECTION_OUTPUT: u32 = 1;

// spa objects, params, format keys
const OBJ_FORMAT: u32 = 0x40003;
const OBJ_PARAM_BUFFERS: u32 = 0x40004;
const OBJ_PARAM_META: u32 = 0x40005;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_FORMAT: u32 = 4;
const PARAM_BUFFERS: u32 = 5;
const PARAM_META: u32 = 6;
const FMT_MEDIA_TYPE: u32 = 1;
const FMT_MEDIA_SUBTYPE: u32 = 2;
const FMT_VIDEO_FORMAT: u32 = 0x20001;
const FMT_VIDEO_SIZE: u32 = 0x20003;
const FMT_VIDEO_FRAMERATE: u32 = 0x20004;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_SUBTYPE_RAW: u32 = 1;
pub const VIDEO_FORMAT_BGRX: u32 = 8;
const BUFFERS_BUFFERS: u32 = 1;
const BUFFERS_BLOCKS: u32 = 2;
const BUFFERS_SIZE: u32 = 3;
const BUFFERS_STRIDE: u32 = 4;
const BUFFERS_DATATYPE: u32 = 6;
const META_TYPE: u32 = 1;
const META_SIZE: u32 = 2;
const META_HEADER: u32 = 1;
const META_HEADER_BYTES: u32 = 32;
const DATA_MEM_PTR: u32 = 1;
const DATA_MEM_FD: u32 = 2;
/// what the daemon actually sends in UseBuffers: it imports our requested
/// MemFd into the pool (AddMem) and rewrites the data as a pool id
const DATA_MEM_ID: u32 = 4;
const IO_BUFFERS: u32 = 1;
pub const STATUS_NEED_DATA: u32 = 1 << 0;
pub const STATUS_HAVE_DATA: u32 = 1 << 1;

/// demand signal from the io area: anything but a standing HAVE_DATA
/// means the graph took our last frame (or never had one) and rendering
/// a new one is paid for. the consumer's dequeue rate paces us, no
/// configured cap anywhere
pub fn graph_wants(status: u32) -> bool {
    status != STATUS_HAVE_DATA
}
const ACTIVATION_TRIGGERED: u32 = 1;
const ACTIVATION_NOT_TRIGGERED: u32 = 0;
const CHUNK_BYTES: usize = 16;

// -- the servers memfd pool --

struct MemBlock {
    fd: OwnedFd,
    write: bool,
}

#[derive(Default)]
pub struct MemPool {
    blocks: HashMap<u32, MemBlock>,
}

/// an mmapped window into a pool block; offset page-alignment handled here
pub struct MemMap {
    base: *mut u8,
    delta: usize,
    len: usize,
}

impl MemMap {
    pub fn ptr(&self, off: usize) -> *mut u8 {
        unsafe { self.base.add(self.delta + off) }
    }
}

impl Drop for MemMap {
    fn drop(&mut self) {
        unsafe {
            let _ = rustix::mm::munmap(self.base.cast(), self.len);
        }
    }
}

impl MemPool {
    fn add(&mut self, id: u32, fd: OwnedFd, write: bool) {
        self.blocks.insert(id, MemBlock { fd, write });
    }

    fn remove(&mut self, id: u32) {
        self.blocks.remove(&id);
    }

    fn map(&self, id: u32, offset: u32, size: u32) -> Result<Rc<MemMap>, PwError> {
        use rustix::mm::{MapFlags, ProtFlags, mmap};
        let b = self.blocks.get(&id).ok_or(PwError::Env("unknown mem id"))?;
        let page = 4096;
        let delta = offset as usize % page;
        let start = offset as usize - delta;
        let len = size as usize + delta;
        let prot = if b.write {
            ProtFlags::READ | ProtFlags::WRITE
        } else {
            ProtFlags::READ
        };
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                prot,
                MapFlags::SHARED,
                &b.fd,
                start as u64,
            )
        }?;
        Ok(Rc::new(MemMap { base: base.cast(), delta, len }))
    }
}

// -- the node --

struct Buffer {
    /// the buffer's block in the pool, kept mapped
    _mem: Rc<MemMap>,
    /// a MemFd data block lives in its own mapping
    _data_mem: Option<Rc<MemMap>>,
    /// spa_meta_header, if negotiated
    header: Option<*mut u8>,
    /// the first data's spa_chunk
    chunk: *mut u8,
    /// the pixel bytes
    data: *mut u8,
    data_len: usize,
}

struct Peer {
    fd: OwnedFd,
    mem: Rc<MemMap>,
}

pub struct SourceNode {
    pub con: Rc<PwConn>,
    pub proxy_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    buffers: Vec<Buffer>,
    /// spa_io_buffers of the output port
    io: Option<(Rc<MemMap>, *mut u8)>,
    pool: MemPool,
    peers: HashMap<u32, Peer>,
    /// our own activation from Transport; kept mapped
    _activation: Option<(Rc<MemMap>, *mut u8)>,
    transport_read: Option<OwnedFd>,
    pub bound_global: Option<u32>,
    pub format_set: bool,
    seq: u64,
}

impl SourceNode {
    pub async fn create(con: Rc<PwConn>, proxy_id: u32, width: u32, height: u32, fps: u32) -> Result<SourceNode, PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.string("client-node");
            b.string("PipeWire:Interface:ClientNode");
            b.int(CLIENT_NODE_VERSION);
            b.dict(&[
                ("node.name", "carrot"),
                ("node.driver", "true"),
                ("media.class", "Video/Source"),
                ("media.type", "Video"),
                ("media.category", "Capture"),
                ("media.role", "Screen"),
            ]);
            b.uint(proxy_id);
        });
        con.send(0, super::CORE_CREATE_OBJECT, &b.buf).await?;
        let node = SourceNode {
            con,
            proxy_id,
            width,
            height,
            fps,
            buffers: Vec::new(),
            io: None,
            pool: MemPool::default(),
            peers: HashMap::new(),
            _activation: None,
            transport_read: None,
            bound_global: None,
            format_set: false,
            seq: 0,
        };
        node.send_update().await?;
        node.send_port_update().await?;
        node.set_active(true).await?;
        Ok(node)
    }

    async fn send_update(&self) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.uint(UPDATE_INFO);
            b.uint(0);
            b.struct_(|b| {
                b.uint(0); // max input ports
                b.uint(1); // max output ports
                b.long((NODE_CHANGE_PARAMS | NODE_CHANGE_FLAGS) as i64);
                b.long(0); // flags
                b.uint(0); // props
                b.uint(0); // params
            });
        });
        self.con.send(self.proxy_id, CN_UPDATE, &b.buf).await
    }

    async fn send_port_update(&self) -> Result<(), PwError> {
        let body = self.port_update_body();
        self.con.send(self.proxy_id, CN_PORT_UPDATE, &body).await
    }

    /// one output port: a single fixed memfd format, header meta, buffers
    fn port_update_body(&self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.uint(DIRECTION_OUTPUT);
            b.uint(0); // port id
            b.uint(UPDATE_PARAMS | UPDATE_INFO);
            b.uint(3); // params
            port_params(b, w, h);
            b.struct_(|b| {
                b.long((PORT_CHANGE_FLAGS | PORT_CHANGE_RATE | PORT_CHANGE_PARAMS) as i64);
                b.long(0); // flags
                b.int(0); // rate num
                b.int(1); // rate denom
                b.int(0); // props
                b.uint(3);
                b.id(PARAM_ENUM_FORMAT);
                b.uint(PARAM_INFO_READ | PARAM_INFO_SERIAL);
                b.id(PARAM_BUFFERS);
                b.uint(PARAM_INFO_READ | PARAM_INFO_SERIAL);
                b.id(PARAM_META);
                b.uint(PARAM_INFO_READ);
            });
        });
        b.buf
    }

    async fn set_active(&self, active: bool) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| b.bool_(active));
        self.con.send(self.proxy_id, CN_SET_ACTIVE, &b.buf).await
    }

    /// core + node event dispatch; unhandled frames return false
    pub fn handle(&mut self, f: &mut Frame) -> Result<bool, PwError> {
        match (f.id, f.opcode) {
            (0, EV_CORE_ADD_MEM) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let id = s.uint()?;
                let _ty = s.id()?;
                // fd pods carry indices; the fd itself rides SCM_RIGHTS
                let (_, d) = s.value()?;
                let fd_idx = i64::from_le_bytes(d[..8].try_into().unwrap());
                let flags = s.int()?;
                let fd = f
                    .fds
                    .get_mut(fd_idx as usize)
                    .map(std::mem::take)
                    .flatten()
                    .ok_or(PwError::Env("add_mem without fd"))?;
                self.pool.add(id, fd, flags & 2 != 0);
                Ok(true)
            }
            (0, EV_CORE_REMOVE_MEM) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                self.pool.remove(s.uint()?);
                Ok(true)
            }
            (0, EV_CORE_BOUND_ID) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let local = s.uint()?;
                let global = s.uint()?;
                if local == self.proxy_id {
                    self.bound_global = Some(global);
                }
                Ok(true)
            }
            (id, EV_CN_PORT_SET_PARAM) if id == self.proxy_id => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let _dir = s.uint()?;
                let _port = s.uint()?;
                let param = s.id()?;
                let _flags = s.uint()?;
                if param == PARAM_FORMAT && !s.done() {
                    self.format_set = true;
                }
                Ok(true)
            }
            (id, EV_CN_PORT_USE_BUFFERS) if id == self.proxy_id => {
                self.use_buffers(f)?;
                Ok(true)
            }
            (id, EV_CN_PORT_SET_IO) if id == self.proxy_id => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let _dir = s.uint()?;
                let _port = s.uint()?;
                let _mix = s.uint()?;
                let io_id = s.id()?;
                let mem_id = s.uint()?;
                let offset = s.uint()?;
                let size = s.uint()?;
                if io_id == IO_BUFFERS {
                    if mem_id == u32::MAX {
                        self.io = None;
                    } else {
                        let m = self.pool.map(mem_id, offset, size)?;
                        let p = m.ptr(0);
                        // io starts empty: no buffer, no status
                        unsafe {
                            (*(p as *const std::sync::atomic::AtomicU32))
                                .store(0, std::sync::atomic::Ordering::Relaxed);
                            (*(p.add(4) as *const std::sync::atomic::AtomicU32))
                                .store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
                        }
                        self.io = Some((m, p));
                    }
                }
                Ok(true)
            }
            (id, EV_CN_TRANSPORT) if id == self.proxy_id => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let (_, r) = s.value()?;
                let read_idx = i64::from_le_bytes(r[..8].try_into().unwrap());
                let (_, w) = s.value()?;
                let _write_idx = i64::from_le_bytes(w[..8].try_into().unwrap());
                let mem_id = s.uint()?;
                let offset = s.uint()?;
                let size = s.uint()?;
                if let Ok(m) = self.pool.map(mem_id, offset, size) {
                    let p = m.ptr(0);
                    self._activation = Some((m, p));
                }
                if let Some(fd) = f.fds.get_mut(read_idx as usize).map(std::mem::take).flatten() {
                    self.transport_read = Some(fd);
                }
                Ok(true)
            }
            (id, EV_CN_SET_ACTIVATION) if id == self.proxy_id => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let node = s.uint()?;
                let (_, d) = s.value()?;
                let fd_idx = i64::from_le_bytes(d[..8].try_into().unwrap());
                if fd_idx < 0 {
                    self.peers.remove(&node);
                    return Ok(true);
                }
                let mem_id = s.uint()?;
                let offset = s.uint()?;
                let size = s.uint()?;
                let fd = f
                    .fds
                    .get_mut(fd_idx as usize)
                    .map(std::mem::take)
                    .flatten()
                    .ok_or(PwError::Env("activation without fd"))?;
                let mem = self.pool.map(mem_id, offset, size)?;
                self.peers.insert(node, Peer { fd, mem });
                Ok(true)
            }
            (id, EV_CN_SET_PARAM) if id == self.proxy_id => Ok(true),
            _ => Ok(false),
        }
    }

    fn use_buffers(&mut self, f: &Frame) -> Result<(), PwError> {
        let mut p = PodParser::new(&f.body);
        let mut s = p.struct_()?;
        let _dir = s.uint()?;
        let _port = s.uint()?;
        let _mix = s.int()?;
        let _flags = s.uint()?;
        let n_buffers = s.uint()?;
        let mut out = Vec::new();
        for _ in 0..n_buffers {
            let mem_id = s.uint()?;
            let offset = s.uint()?;
            let size = s.uint()?;
            let mem = self.pool.map(mem_id, offset, size)?;
            let mut off = 0usize;
            let n_metas = s.uint()?;
            let mut header = None;
            for _ in 0..n_metas {
                let ty = s.id()?;
                let msize = s.uint()? as usize;
                if ty == META_HEADER {
                    header = Some(mem.ptr(off));
                }
                off += (msize + 7) & !7;
            }
            let n_datas = s.uint()?;
            let mut chunk = std::ptr::null_mut();
            let mut data = std::ptr::null_mut();
            let mut data_len = 0usize;
            let mut data_mem = None;
            for i in 0..n_datas {
                let ty = s.id()?;
                let data_id = s.uint()?;
                let _dflags = s.uint()?;
                let mapoffset = s.uint()?;
                let maxsize = s.uint()?;
                let c = mem.ptr(off);
                off += CHUNK_BYTES;
                // single-plane formats only; further planes just advance
                if i > 0 {
                    continue;
                }
                chunk = c;
                if ty == DATA_MEM_PTR {
                    data = mem.ptr(data_id as usize);
                    data_len = maxsize as usize;
                } else if ty == DATA_MEM_FD || ty == DATA_MEM_ID {
                    let dm = self.pool.map(data_id, mapoffset, maxsize)?;
                    data = dm.ptr(0);
                    data_len = maxsize as usize;
                    data_mem = Some(dm);
                }
            }
            if data.is_null() {
                continue;
            }
            out.push(Buffer { _mem: mem, _data_mem: data_mem, header, chunk, data, data_len });
        }
        self.buffers = out;
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.io.is_some() && !self.buffers.is_empty() && self.format_set
    }

    /// true when the graph can take a new frame; false while the last
    /// one sits unconsumed. pairs with produce()'s Release store
    pub fn wants_frame(&self) -> bool {
        use std::sync::atomic::{AtomicU32, Ordering};
        let Some((_, io)) = &self.io else { return false };
        graph_wants(unsafe { (*((*io) as *const AtomicU32)).load(Ordering::Acquire) })
    }

    /// re-advertise the port at a new size: stale buffers drop and the
    /// daemon renegotiates format and buffers. the node borrow releases
    /// before the await, so the pump stays free to handle events
    pub async fn resize(node: &Rc<std::cell::RefCell<SourceNode>>, w: u32, h: u32) -> Result<(), PwError> {
        let (con, proxy_id, body) = {
            let mut n = node.borrow_mut();
            n.width = w;
            n.height = h;
            n.format_set = false;
            n.buffers.clear();
            (n.con.clone(), n.proxy_id, n.port_update_body())
        };
        con.send(proxy_id, CN_PORT_UPDATE, &body).await
    }

    /// hand the next buffer to the graph: `fill` paints the pixels, then
    /// chunk, header, io status, and every peer's activation eventfd
    pub fn produce(&mut self, fill: impl FnOnce(&mut [u8], usize)) {
        use std::sync::atomic::{AtomicU32, Ordering};
        let Some((_, io)) = &self.io else { return };
        let io = *io;
        let cur = unsafe { (*(io.add(4) as *const AtomicU32)).load(Ordering::Relaxed) };
        let idx = ((cur.wrapping_add(1)) as usize) % self.buffers.len();
        let b = &self.buffers[idx];
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 4;
        let len = (stride * h).min(b.data_len);
        let px = unsafe { std::slice::from_raw_parts_mut(b.data, len) };
        fill(px, stride);
        unsafe {
            // spa_chunk: offset, size, stride, flags
            let c = b.chunk as *mut u32;
            c.write_volatile(0);
            c.add(1).write_volatile(len as u32);
            (c.add(2) as *mut i32).write_volatile(stride as i32);
            (c.add(3) as *mut i32).write_volatile(0);
            if let Some(hd) = b.header {
                let h32 = hd as *mut u32;
                h32.write_volatile(0); // flags
                h32.add(1).write_volatile(0); // offset
                (hd.add(8) as *mut i64).write_volatile(-1); // pts
                (hd.add(16) as *mut i64).write_volatile(0); // dts
                (hd.add(24) as *mut u64).write_volatile(self.seq);
            }
            (*(io.add(4) as *const AtomicU32)).store(idx as u32, Ordering::Relaxed);
            (*(io as *const AtomicU32)).store(STATUS_HAVE_DATA, Ordering::Release);
        }
        self.seq += 1;
        // the driver kicks every follower whose only missing signal is us
        for peer in self.peers.values() {
            unsafe {
                use std::sync::atomic::AtomicI32;
                let a = peer.mem.ptr(0);
                let required = &*(a.add(12) as *const AtomicI32);
                let pending = &*(a.add(16) as *const AtomicI32);
                let req = required.load(Ordering::Relaxed);
                pending.store(req - 1, Ordering::Relaxed);
                let status = &*(a as *const AtomicU32);
                if req == 1 {
                    status.store(ACTIVATION_TRIGGERED, Ordering::Release);
                    let _ = rustix::io::write(&peer.fd, &1u64.to_ne_bytes());
                } else {
                    status.store(ACTIVATION_NOT_TRIGGERED, Ordering::Release);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_leaves_framerate_unconstrained() {
        let mut b = PodBuilder::default();
        port_params(&mut b, 640, 360);
        let mut p = PodParser::new(&b.buf);
        let (ty, id, mut obj) = p.object().unwrap();
        assert_eq!((ty, id), (OBJ_FORMAT, PARAM_ENUM_FORMAT));
        let mut keys = Vec::new();
        while !obj.done() {
            keys.push(obj.prop_key().unwrap());
            obj.value().unwrap();
        }
        for want in [FMT_MEDIA_TYPE, FMT_MEDIA_SUBTYPE, FMT_VIDEO_FORMAT, FMT_VIDEO_SIZE] {
            assert!(keys.contains(&want), "EnumFormat lost key {want:#x}");
        }
        // a fixed framerate empties the intersection with real consumers
        // (obs pins its own rate); omitted = wildcard, like jay
        assert!(
            !keys.contains(&FMT_VIDEO_FRAMERATE),
            "EnumFormat must not constrain the framerate"
        );
    }

    #[test]
    fn production_paces_on_consumption() {
        // spa_io_buffers: { status, buffer_id }; a fresh mapping is zeroed
        assert!(graph_wants(0), "fresh io produces the priming frame");
        // the follower consumed and flipped the status back
        assert!(graph_wants(STATUS_NEED_DATA), "need-data produces");
        // our own last frame still sits unconsumed: a render now is pure
        // waste, and at present cadence it is the compositor-wide stall
        assert!(!graph_wants(STATUS_HAVE_DATA), "have-data skips the render");
    }
}
