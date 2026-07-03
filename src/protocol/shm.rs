// wl_shm: pools, buffers, and the release contract. nothing reads the pixels
// yet - this phase is validation and lifecycle, not the renderer.

use crate::client::{Client, ClientError, Object};
use crate::clientmem::{ClientMem, ClientMemOffset};
use crate::format::{Format, shm_format_by_wl, shm_formats};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wl_buffer, wl_shm, wl_shm_pool};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::rc::Rc;

pub const INVALID_FORMAT: u32 = 0;
pub const INVALID_STRIDE: u32 = 1;
pub const INVALID_FD: u32 = 2;

// -- wl_shm --

pub struct WlShmGlobal;

impl Global for WlShmGlobal {
    fn interface(&self) -> &'static str {
        wl_shm::NAME
    }

    fn version(&self) -> u32 {
        2
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        let shm = Rc::new(WlShm {
            id,
            client: client.clone(),
            version,
        });
        client.add_client_obj(shm)?;
        for f in shm_formats() {
            let wire = f.wl_id.unwrap_or(f.drm);
            client.event(|o| wl_shm::format::send(o, id, wire));
        }
        Ok(())
    }
}

pub struct WlShm {
    id: ObjectId,
    client: Rc<Client>,
    version: u32,
}

impl wl_shm::Handler for WlShm {
    fn create_pool(&self, req: wl_shm::create_pool::Request) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if req.size <= 0 {
            c.protocol_error(self.id, INVALID_STRIDE, "pool sizes must be positive");
            return Ok(());
        }
        let fd = Rc::new(req.fd);
        let mem = match ClientMem::new(&fd, req.size as usize) {
            Ok(mem) => mem,
            Err(e) => {
                c.protocol_error(self.id, INVALID_FD, &e.to_string());
                return Ok(());
            }
        };
        c.add_client_obj(Rc::new(WlShmPool {
            id: req.id,
            client: c.clone(),
            fd,
            requested: Cell::new(req.size as usize),
            mem: RefCell::new(mem),
        }))?;
        Ok(())
    }

    fn release(&self, _req: wl_shm::release::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlShm {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_shm::NAME
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_shm::dispatch(&*self, self.version, opcode, r)
    }
}

// -- wl_shm_pool --

pub struct WlShmPool {
    id: ObjectId,
    client: Rc<Client>,
    fd: Rc<OwnedFd>,
    requested: Cell<usize>,
    mem: RefCell<Rc<ClientMem>>,
}

impl wl_shm_pool::Handler for WlShmPool {
    fn create_buffer(
        &self,
        req: wl_shm_pool::create_buffer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(format) = shm_format_by_wl(req.format) else {
            c.protocol_error(
                self.id,
                INVALID_FORMAT,
                &format!("format {:#x} is not supported", req.format),
            );
            return Ok(());
        };
        if req.offset < 0 || req.width <= 0 || req.height <= 0 || req.stride < 0 {
            c.protocol_error(self.id, INVALID_STRIDE, "buffer parameters must be positive");
            return Ok(());
        }
        if (req.stride as u64) < req.width as u64 * format.bytes_per_pixel as u64 {
            c.protocol_error(
                self.id,
                INVALID_STRIDE,
                &format!("stride {} is too small for width {}", req.stride, req.width),
            );
            return Ok(());
        }
        // against the size the client declared, not the page rounding
        let end = req.offset as u64 + req.stride as u64 * req.height as u64;
        if end > self.requested.get() as u64 {
            c.protocol_error(self.id, INVALID_STRIDE, "buffer extends past the pool");
            return Ok(());
        }
        let mem = self.mem.borrow().offset(req.offset as usize);
        let buf = Rc::new(WlBuffer {
            id: req.id,
            client: c.clone(),
            rect: Rect::new_sized_saturating(0, 0, req.width, req.height),
            format,
            stride: req.stride,
            mem,
            destroyed: Cell::new(false),
        });
        c.add_client_obj(buf.clone())?;
        c.objects.track_buffer(buf);
        Ok(())
    }

    fn destroy(&self, _req: wl_shm_pool::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        // buffers keep the mapping alive through their own Rcs
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn resize(&self, req: wl_shm_pool::resize::Request) -> Result<(), Box<dyn std::error::Error>> {
        if req.size < 0 || (req.size as usize) < self.requested.get() {
            return Err("pools can only grow".into());
        }
        // fresh mapping; existing buffers keep the old one, both alias the same file
        let mem = ClientMem::new(&self.fd, req.size as usize)?;
        *self.mem.borrow_mut() = mem;
        self.requested.set(req.size as usize);
        Ok(())
    }
}

impl Object for WlShmPool {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_shm_pool::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_shm_pool::dispatch(&*self, 1, opcode, r)
    }
}

// -- wl_buffer --

pub struct WlBuffer {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub rect: Rect,
    pub format: &'static Format,
    pub stride: i32,
    mem: ClientMemOffset,
    pub destroyed: Cell<bool>,
}

impl WlBuffer {
    pub fn access(&self) -> crate::clientmem::ShmAccess<'_> {
        self.mem.safe_access()
    }
}

impl wl_buffer::Handler for WlBuffer {
    fn destroy(&self, _req: wl_buffer::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        // surfaces may keep displaying via their Rc; the flag only suppresses
        // events to the dead id
        self.destroyed.set(true);
        self.client.objects.forget_buffer(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlBuffer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_buffer::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_buffer::dispatch(&*self, 1, opcode, r)
    }
}

/// one attach of one buffer to one surface. the release contract: attach leaves
/// send_release false, commit flips it, and whoever drops the attachment sends
/// the event - replacement, null attach, and teardown all behave identically.
pub struct AttachedBuffer {
    pub buf: Rc<WlBuffer>,
    pub send_release: Cell<bool>,
}

impl Drop for AttachedBuffer {
    fn drop(&mut self) {
        if self.send_release.get() && !self.buf.destroyed.get() {
            let buf = &self.buf;
            buf.client.event(|o| wl_buffer::release::send(o, buf.id));
        }
    }
}

#[cfg(test)]
pub(crate) fn test_buffer(client: &Rc<Client>, id: ObjectId, w: i32, h: i32) -> Rc<WlBuffer> {
    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
    let fd = Rc::new(
        memfd_create("carrot-test", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap(),
    );
    let size = (w * h * 4) as usize;
    ftruncate(&*fd, size as u64).unwrap();
    let mem = ClientMem::new(&fd, size).unwrap();
    let buf = Rc::new(WlBuffer {
        id,
        client: client.clone(),
        rect: Rect::new_sized_saturating(0, 0, w, h),
        format: &crate::format::ARGB8888,
        stride: w * 4,
        mem: mem.offset(0),
        destroyed: Cell::new(false),
    });
    client.add_client_obj(buf.clone()).unwrap();
    client.objects.track_buffer(buf.clone());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::WL_DISPLAY_ID;
    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

    fn test_pool(client: &Rc<Client>, size: usize) -> WlShmPool {
        let fd = Rc::new(memfd_create("carrot-test-pool", MemfdFlags::CLOEXEC).unwrap());
        ftruncate(&*fd, size as u64).unwrap();
        let mem = ClientMem::new(&fd, size).unwrap();
        WlShmPool {
            id: ObjectId(50),
            client: client.clone(),
            fd,
            requested: Cell::new(size),
            mem: RefCell::new(mem),
        }
    }

    #[test]
    fn create_buffer_validation() {
        use wl_shm_pool::Handler as _;
        let (_state, client) = test_client();
        let pool = test_pool(&client, 4096);
        // stride too small for width
        pool.create_buffer(wl_shm_pool::create_buffer::Request {
            id: ObjectId(60),
            offset: 0,
            width: 10,
            height: 10,
            stride: 8,
            format: 0,
        })
        .unwrap();
        // out of bounds
        pool.create_buffer(wl_shm_pool::create_buffer::Request {
            id: ObjectId(61),
            offset: 0,
            width: 40,
            height: 40,
            stride: 160,
            format: 0,
        })
        .unwrap();
        // unknown format
        pool.create_buffer(wl_shm_pool::create_buffer::Request {
            id: ObjectId(62),
            offset: 0,
            width: 2,
            height: 2,
            stride: 8,
            format: 0xdeadbeef,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        // three protocol errors on the display object
        assert_eq!(count_events(&bytes, WL_DISPLAY_ID, 0), 3);
        assert!(client.objects.buffer(ObjectId(60)).is_none());
    }

    #[test]
    fn valid_buffer_is_tracked() {
        use wl_shm_pool::Handler as _;
        let (_state, client) = test_client();
        let pool = test_pool(&client, 4096);
        pool.create_buffer(wl_shm_pool::create_buffer::Request {
            id: ObjectId(60),
            offset: 0,
            width: 16,
            height: 16,
            stride: 64,
            format: 1,
        })
        .unwrap();
        let buf = client.objects.buffer(ObjectId(60)).unwrap();
        assert_eq!(buf.rect.width(), 16);
        assert_eq!(count_events(&client.queued_out_bytes(), WL_DISPLAY_ID, 0), 0);
    }

    #[test]
    fn pools_only_grow() {
        use wl_shm_pool::Handler as _;
        let (_state, client) = test_client();
        let pool = test_pool(&client, 4096);
        assert!(pool.resize(wl_shm_pool::resize::Request { size: 2048 }).is_err());
        assert!(pool.resize(wl_shm_pool::resize::Request { size: 8192 }).is_ok());
        assert_eq!(pool.requested.get(), 8192);
    }
}
