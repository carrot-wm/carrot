// zwp-linux-dmabuf-v1, version 3. gpu clients hand over dmabufs and the
// renderer samples them in place - no shm round trip. only xrgb/argb is
// advertised; modifiers and their plane counts come from the driver.

use crate::client::{Client, ClientError, Object};
use crate::format::{ARGB8888, Format, XRGB8888};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use crate::protocol::shm::{BufferStorage, DmabufImage, DmabufPlane, WlBuffer};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ERR_ALREADY_USED: u32 = 0;
const ERR_PLANE_IDX: u32 = 1;
const ERR_PLANE_SET: u32 = 2;
const ERR_INCOMPLETE: u32 = 3;
const ERR_INVALID_FORMAT: u32 = 4;
const ERR_INVALID_DIMENSIONS: u32 = 5;
const ERR_OUT_OF_BOUNDS: u32 = 6;
const ERR_INVALID_WL_BUFFER: u32 = 7;

const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const MAX_PLANES: usize = 4;

/// what display bring-up found on the render device; feeds the feedback
/// and modifier advertisement
pub struct DmabufInfo {
    /// primary node dev_t; clients resolve it to the matching render node
    pub main_device: u64,
    /// (fourcc, modifier, plane count) triples, table order = feedback
    /// tranche indices
    pub formats: Vec<(u32, u64, u32)>,
}

fn linear_fallback() -> Vec<(u32, u64, u32)> {
    vec![(XRGB8888.drm, MOD_LINEAR, 1), (ARGB8888.drm, MOD_LINEAR, 1)]
}

fn fourcc(format: u32) -> Option<&'static Format> {
    if format == XRGB8888.drm {
        Some(&XRGB8888)
    } else if format == ARGB8888.drm {
        Some(&ARGB8888)
    } else {
        None
    }
}

pub struct DmabufGlobal;

impl Global for DmabufGlobal {
    fn interface(&self) -> &'static str {
        zwp_linux_dmabuf_v1::NAME
    }

    fn version(&self) -> u32 {
        4
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(Dmabuf {
            id,
            client: client.clone(),
            version,
        }))?;
        // v4 clients get everything through feedback objects
        if version >= 4 {
            return Ok(());
        }
        let info = client.state.dmabuf_info.borrow();
        let formats = match info.as_ref() {
            Some(i) => i.formats.clone(),
            None => linear_fallback(),
        };
        drop(info);
        client.event(|o| {
            for &(fourcc, modifier, _) in &formats {
                if version >= 3 {
                    zwp_linux_dmabuf_v1::modifier::send(
                        o,
                        id,
                        fourcc,
                        (modifier >> 32) as u32,
                        modifier as u32,
                    );
                } else {
                    zwp_linux_dmabuf_v1::format::send(o, id, fourcc);
                }
            }
        });
        Ok(())
    }
}

pub struct Dmabuf {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zwp_linux_dmabuf_v1::Handler for Dmabuf {
    fn destroy(
        &self,
        _req: zwp_linux_dmabuf_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn create_params(
        &self,
        req: zwp_linux_dmabuf_v1::create_params::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.add_client_obj(Rc::new(BufferParams {
            id: req.params_id,
            client: self.client.clone(),
            version: self.version,
            planes: RefCell::new(Vec::new()),
            modifier: Cell::new(None),
            used: Cell::new(false),
        }))?;
        Ok(())
    }

    fn get_default_feedback(
        &self,
        req: zwp_linux_dmabuf_v1::get_default_feedback::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        feedback(&self.client, req.id)
    }

    fn get_surface_feedback(
        &self,
        req: zwp_linux_dmabuf_v1::get_surface_feedback::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // one device, one tranche: per-surface preferences match the default
        feedback(&self.client, req.id)
    }
}

/// feedback is static here, so send the whole state up front and be done
fn feedback(c: &Rc<Client>, id: ObjectId) -> Result<(), Box<dyn std::error::Error>> {
    c.add_client_obj(Rc::new(Feedback {
        id,
        client: c.clone(),
    }))?;
    let (device, formats) = match c.state.dmabuf_info.borrow().as_ref() {
        Some(i) => (Some(i.main_device), i.formats.clone()),
        None => (None, linear_fallback()),
    };
    let mut table = Vec::with_capacity(formats.len() * 16);
    for &(fourcc, modifier, _) in &formats {
        table.extend_from_slice(&fourcc.to_ne_bytes());
        table.extend_from_slice(&0u32.to_ne_bytes());
        table.extend_from_slice(&modifier.to_ne_bytes());
    }
    let fd = rustix::fs::memfd_create("carrot-dmabuf-table", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| format!("memfd: {e}"))?;
    {
        use std::io::Write as _;
        let mut f = std::fs::File::from(fd.try_clone().map_err(|e| format!("dup: {e}"))?);
        f.write_all(&table).map_err(|e| format!("table write: {e}"))?;
    }
    let indices: Vec<u8> = (0..formats.len() as u16)
        .flat_map(|i| i.to_ne_bytes())
        .collect();
    let fd = Rc::new(fd);
    c.event(|o| {
        zwp_linux_dmabuf_feedback_v1::format_table::send(o, id, fd.clone(), table.len() as u32);
        if let Some(dev) = device {
            let dev = dev.to_ne_bytes();
            zwp_linux_dmabuf_feedback_v1::main_device::send(o, id, &dev);
            zwp_linux_dmabuf_feedback_v1::tranche_target_device::send(o, id, &dev);
        }
        zwp_linux_dmabuf_feedback_v1::tranche_flags::send(o, id, 0);
        zwp_linux_dmabuf_feedback_v1::tranche_formats::send(o, id, &indices);
        zwp_linux_dmabuf_feedback_v1::tranche_done::send(o, id);
        zwp_linux_dmabuf_feedback_v1::done::send(o, id);
    });
    Ok(())
}

pub struct Feedback {
    pub id: ObjectId,
    pub client: Rc<Client>,
}

impl zwp_linux_dmabuf_feedback_v1::Handler for Feedback {
    fn destroy(
        &self,
        _req: zwp_linux_dmabuf_feedback_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for Feedback {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_linux_dmabuf_feedback_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_linux_dmabuf_feedback_v1::dispatch(&*self, 4, opcode, r)
    }
}

impl Object for Dmabuf {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_linux_dmabuf_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_linux_dmabuf_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct BufferParams {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    planes: RefCell<Vec<DmabufPlane>>,
    /// all planes must agree on it
    modifier: Cell<Option<u64>>,
    used: Cell<bool>,
}

impl BufferParams {
    /// shared tail of create/create_immed; None means an error was posted
    fn build(
        &self,
        width: i32,
        height: i32,
        format: u32,
    ) -> Option<(&'static Format, DmabufImage)> {
        let c = &self.client;
        if self.used.replace(true) {
            c.protocol_error(self.id, ERR_ALREADY_USED, "params were already used");
            return None;
        }
        let planes = std::mem::take(&mut *self.planes.borrow_mut());
        if planes.is_empty() {
            c.protocol_error(self.id, ERR_INCOMPLETE, "no planes were added");
            return None;
        }
        let modifier = self.modifier.get().unwrap_or(MOD_INVALID);
        let Some(format) = fourcc(format) else {
            c.protocol_error(
                self.id,
                ERR_INVALID_FORMAT,
                &format!("format {format:#x} is not advertised"),
            );
            return None;
        };
        if width <= 0 || height <= 0 || width > 16384 || height > 16384 {
            c.protocol_error(self.id, ERR_INVALID_DIMENSIONS, "bad buffer dimensions");
            return None;
        }
        // the pair must be one we advertised; implicit falls back to linear
        let modifier = if modifier == MOD_INVALID { MOD_LINEAR } else { modifier };
        let expected = match c.state.dmabuf_info.borrow().as_ref() {
            Some(i) => i
                .formats
                .iter()
                .find(|&&(f, m, _)| f == format.drm && m == modifier)
                .map(|&(_, _, n)| n as usize),
            None => (modifier == MOD_LINEAR).then_some(1),
        };
        let Some(expected) = expected else {
            c.protocol_error(
                self.id,
                ERR_INVALID_FORMAT,
                &format!("modifier {modifier:#x} is not advertised for this format"),
            );
            return None;
        };
        if planes.len() != expected {
            c.protocol_error(self.id, ERR_INCOMPLETE, "plane count does not match the modifier");
            return None;
        }
        // only linear layouts are transparent enough to bounds-check; the
        // rest are driver-opaque and get validated at import
        if modifier == MOD_LINEAR {
            let plane = &planes[0];
            let size = rustix::fs::seek(&plane.fd, rustix::fs::SeekFrom::End(0)).unwrap_or(0);
            let need = plane.offset as u64
                + plane.stride as u64 * (height as u64 - 1)
                + width as u64 * 4;
            if plane.stride < width as u32 * 4 || need > size {
                c.protocol_error(self.id, ERR_OUT_OF_BOUNDS, "planes exceed the dmabuf");
                return None;
            }
        }
        Some((format, DmabufImage { planes, modifier }))
    }

    /// one bo backs the whole image: the import reads memory only from
    /// plane 0, so disjoint buffers can never bind correctly
    fn single_bo(img: &DmabufImage) -> bool {
        let Some((first, rest)) = img.planes.split_first() else {
            return true;
        };
        let Ok(base) = rustix::fs::fstat(&first.fd) else {
            return false;
        };
        rest.iter().all(|p| {
            rustix::fs::fstat(&p.fd)
                .is_ok_and(|st| (st.st_dev, st.st_ino) == (base.st_dev, base.st_ino))
        })
    }

    fn buffer(
        &self,
        id: ObjectId,
        w: i32,
        h: i32,
        format: &'static Format,
        img: DmabufImage,
    ) -> Rc<WlBuffer> {
        Rc::new(WlBuffer {
            id,
            uid: self.client.state.next_uid(),
            client: self.client.clone(),
            rect: Rect::new_sized_saturating(0, 0, w, h),
            format,
            stride: img.planes[0].stride as i32,
            storage: BufferStorage::Dmabuf(img),
            destroyed: Cell::new(false),
        })
    }
}

impl zwp_linux_buffer_params_v1::Handler for BufferParams {
    fn destroy(
        &self,
        _req: zwp_linux_buffer_params_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn add(
        &self,
        req: zwp_linux_buffer_params_v1::add::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if self.used.get() {
            c.protocol_error(self.id, ERR_ALREADY_USED, "params were already used");
            return Ok(());
        }
        let mut planes = self.planes.borrow_mut();
        if req.plane_idx as usize != planes.len() || planes.len() >= MAX_PLANES {
            c.protocol_error(
                self.id,
                if planes.len() >= MAX_PLANES { ERR_PLANE_IDX } else { ERR_PLANE_SET },
                &format!("plane {} out of order or over the limit", req.plane_idx),
            );
            return Ok(());
        }
        let modifier = ((req.modifier_hi as u64) << 32) | req.modifier_lo as u64;
        if let Some(prev) = self.modifier.get() {
            if prev != modifier {
                c.protocol_error(self.id, ERR_INVALID_WL_BUFFER, "planes disagree on the modifier");
                return Ok(());
            }
        } else {
            self.modifier.set(Some(modifier));
        }
        planes.push(DmabufPlane {
            fd: req.fd,
            offset: req.offset,
            stride: req.stride,
        });
        Ok(())
    }

    fn create(
        &self,
        req: zwp_linux_buffer_params_v1::create::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some((format, img)) = self.build(req.width, req.height, req.format) else {
            return Ok(());
        };
        if req.flags != 0 || !Self::single_bo(&img) {
            // import failures on the async path answer with failed, not
            // a protocol violation; the client falls back
            c.event(|o| zwp_linux_buffer_params_v1::failed::send(o, self.id));
            return Ok(());
        }
        let id = c.objects.alloc_server_id();
        let buf = self.buffer(id, req.width, req.height, format, img);
        c.add_server_obj(buf.clone());
        c.objects.track_buffer(buf);
        c.event(|o| zwp_linux_buffer_params_v1::created::send(o, self.id, id));
        Ok(())
    }

    fn create_immed(
        &self,
        req: zwp_linux_buffer_params_v1::create_immed::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some((format, img)) = self.build(req.width, req.height, req.format) else {
            return Ok(());
        };
        if req.flags != 0 {
            c.protocol_error(self.id, ERR_INVALID_WL_BUFFER, "buffer flags are unsupported");
            return Ok(());
        }
        if !Self::single_bo(&img) {
            c.protocol_error(self.id, ERR_INVALID_WL_BUFFER, "planes span multiple buffers");
            return Ok(());
        }
        let buf = self.buffer(req.buffer_id, req.width, req.height, format, img);
        c.add_client_obj(buf.clone())?;
        c.objects.track_buffer(buf);
        Ok(())
    }
}

impl Object for BufferParams {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_linux_buffer_params_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_linux_buffer_params_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// wl_buffer's own dispatch lives in shm.rs and is storage-agnostic; nothing
// dmabuf-specific to add here. keep the import lazy: the renderer wraps the
// fd the first time the buffer is actually drawn.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use rustix::fs::{MemfdFlags, memfd_create};
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use zwp_linux_buffer_params_v1::Handler as _;
    use zwp_linux_dmabuf_v1::Handler as _;

    fn fake_dmabuf(bytes: usize) -> OwnedFd {
        let fd = memfd_create("fake-dmabuf", MemfdFlags::CLOEXEC).unwrap();
        let mut f = std::fs::File::from(fd);
        f.write_all(&vec![0u8; bytes]).unwrap();
        f.into()
    }

    fn params(client: &Rc<Client>) -> Rc<BufferParams> {
        let mgr = Dmabuf {
            id: ObjectId(80),
            client: client.clone(),
            version: 3,
        };
        mgr.create_params(zwp_linux_dmabuf_v1::create_params::Request {
            params_id: ObjectId(81),
        })
        .unwrap();
        Rc::new(BufferParams {
            id: ObjectId(81),
            client: client.clone(),
            version: 4,
            planes: RefCell::new(Vec::new()),
            modifier: Cell::new(None),
            used: Cell::new(false),
        })
    }

    #[test]
    fn create_immed_builds_a_dmabuf_buffer() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        let buf = client.objects.buffer(ObjectId(82)).unwrap();
        assert!(buf.dmabuf().is_some());
        assert!(buf.shm_access().is_none());
        assert_eq!(buf.rect.width(), 64);
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);
    }

    #[test]
    fn an_undersized_dmabuf_is_rejected() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(1024),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn feedback_sends_the_whole_state() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(DmabufInfo {
            main_device: 0xe280,
            formats: vec![
                (XRGB8888.drm, 0, 1),
                (XRGB8888.drm, 42, 1),
                (ARGB8888.drm, 42, 1),
            ],
        });
        feedback(&client, ObjectId(90)).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(90), 1), 1, "format_table");
        assert_eq!(count_events(&bytes, ObjectId(90), 2), 1, "main_device");
        assert_eq!(count_events(&bytes, ObjectId(90), 4), 1, "tranche_target_device");
        assert_eq!(count_events(&bytes, ObjectId(90), 5), 1, "tranche_formats");
        assert_eq!(count_events(&bytes, ObjectId(90), 3), 1, "tranche_done");
        assert_eq!(count_events(&bytes, ObjectId(90), 0), 1, "done");
    }

    #[test]
    fn modifiers_gate_on_the_advertised_set() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(DmabufInfo {
            main_device: 0,
            formats: vec![(XRGB8888.drm, 42, 1)],
        });
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 0,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 42,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // tiled path skips the linear bounds check and lands
        let buf = client.objects.buffer(ObjectId(82)).unwrap();
        assert_eq!(buf.dmabuf().unwrap().modifier, 42);
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);

        // an unadvertised modifier is a loud error
        let p2 = Rc::new(BufferParams {
            id: ObjectId(84),
            client: client.clone(),
            version: 4,
            planes: RefCell::new(Vec::new()),
            modifier: Cell::new(None),
            used: Cell::new(false),
        });
        p2.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 0,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 7,
        })
        .unwrap();
        p2.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(83),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn create_with_unsupported_flags_sends_failed() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create(zwp_linux_buffer_params_v1::create::Request {
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 1,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        // the client survives and gets failed, not created
        assert_eq!(count_events(&bytes, ObjectId(1), 0), 0);
        assert_eq!(count_events(&bytes, ObjectId(81), 1), 1, "failed");
        assert_eq!(count_events(&bytes, ObjectId(81), 0), 0, "created");
    }

    #[test]
    fn create_immed_with_unsupported_flags_is_fatal() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 1,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn disjoint_plane_buffers_fail_without_killing_the_async_client() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(DmabufInfo {
            main_device: 0,
            formats: vec![(XRGB8888.drm, 42, 2)],
        });
        let p = params(&client);
        // two planes, two unrelated buffers: the import reads only one
        for idx in 0..2u32 {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: fake_dmabuf(4096),
                plane_idx: idx,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 42,
            })
            .unwrap();
        }
        p.create(zwp_linux_buffer_params_v1::create::Request {
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(1), 0), 0, "client survives");
        assert_eq!(count_events(&bytes, ObjectId(81), 1), 1, "failed");
        assert_eq!(count_events(&bytes, ObjectId(81), 0), 0, "created");
    }

    #[test]
    fn plane_count_must_match_the_modifier() {
        let (_state, client) = test_client();
        let p = params(&client);
        for idx in 0..2u32 {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: fake_dmabuf(64 * 64 * 4),
                plane_idx: idx,
                offset: 0,
                stride: 64 * 4,
                modifier_hi: 0,
                modifier_lo: 0,
            })
            .unwrap();
        }
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // linear is single-plane; two planes die before touching the driver
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn multi_plane_modifier_accepts_only_the_driver_count() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(DmabufInfo {
            main_device: 0,
            formats: vec![(XRGB8888.drm, 42, 2)],
        });
        let p = params(&client);
        // both planes ride the same bo, like a real tiled allocation
        let bo = fake_dmabuf(4096);
        for idx in 0..2u32 {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: bo.try_clone().unwrap(),
                plane_idx: idx,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 42,
            })
            .unwrap();
        }
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // the driver-reported count lands as-is
        let buf = client.objects.buffer(ObjectId(82)).unwrap();
        assert_eq!(buf.dmabuf().unwrap().planes.len(), 2);
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);

        // a single plane under the same modifier is short
        let p2 = Rc::new(BufferParams {
            id: ObjectId(84),
            client: client.clone(),
            version: 4,
            planes: RefCell::new(Vec::new()),
            modifier: Cell::new(None),
            used: Cell::new(false),
        });
        p2.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 0,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 42,
        })
        .unwrap();
        p2.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(83),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn params_are_single_use_and_single_plane() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 1,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        // plane_idx 1 is a protocol error straight away
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }
}
