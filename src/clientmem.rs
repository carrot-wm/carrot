// client shm pool mappings, mapped eagerly at pool creation. a pool sealed
// against shrinking can never SIGBUS; unsealed pools are read through their fd
// instead of the mapping, so the process needs no signal handler.

use rustix::fs::{Mode, OFlags, SealFlags, fcntl_get_seals, fstat, ftruncate, open};
use rustix::io::Errno;
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, ioctl, opcode};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::ffi::c_void;
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

#[derive(Debug)]
pub enum ClientMemError {
    Mmap(Errno),
}

impl fmt::Display for ClientMemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientMemError::Mmap(e) => write!(f, "mapping the pool failed: {e}"),
        }
    }
}

impl std::error::Error for ClientMemError {}

pub struct ClientMem {
    fd: Rc<OwnedFd>,
    ptr: *mut std::ffi::c_void,
    len: usize, // page rounded
    requested: usize,
    /// F_SEAL_SHRINK with enough backing - dereferencing can't fault
    sealed: bool,
}

impl ClientMem {
    pub fn new(fd: &Rc<OwnedFd>, requested: usize) -> Result<Rc<ClientMem>, ClientMemError> {
        let mut sealed = false;
        if let Ok(seals) = fcntl_get_seals(&**fd) {
            if seals.contains(SealFlags::SHRINK) {
                if let Ok(st) = fstat(&**fd) {
                    sealed = st.st_size >= requested as i64;
                }
            }
        }
        let page = rustix::param::page_size();
        let len = requested.div_ceil(page) * page;
        if sealed && len > requested {
            // sealed file may stop short of the page boundary; grow best-effort,
            // failure just leaves it unsealed-equivalent
            if ftruncate(&**fd, len as u64).is_err() {
                if let Ok(st) = fstat(&**fd) {
                    sealed = st.st_size >= len as i64;
                }
            }
        }
        let ptr = if len == 0 {
            std::ptr::null_mut()
        } else {
            unsafe {
                mmap(
                    std::ptr::null_mut(),
                    len,
                    // compositor only reads client pixels
                    ProtFlags::READ,
                    MapFlags::SHARED,
                    &**fd,
                    0,
                )
            }
            .map_err(ClientMemError::Mmap)?
        };
        Ok(Rc::new(ClientMem {
            fd: fd.clone(),
            ptr,
            len,
            requested,
            sealed,
        }))
    }

    pub fn requested(&self) -> usize {
        self.requested
    }

    /// sealed against shrinking: the mapping (and anything importing it)
    /// can never fault
    pub fn sealed(&self) -> bool {
        self.sealed
    }

    pub fn base_ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }

    /// page-rounded mapping length
    pub fn mapped_len(&self) -> usize {
        self.len
    }

    /// bridge a sealed pool to a dmabuf through the kernel's udmabuf
    /// device, for drivers whose host-pointer import rejects file-backed
    /// pages. the fd pins the pool's pages until every importer lets go
    pub fn udmabuf(&self) -> Option<OwnedFd> {
        if !self.sealed || self.len == 0 {
            return None;
        }
        let dev = match open("/dev/udmabuf", OFlags::RDWR | OFlags::CLOEXEC, Mode::empty()) {
            Ok(fd) => fd,
            Err(e) => {
                crate::trace!("udmabuf: open {e}");
                return None;
            }
        };
        let mut arg = udmabuf_create {
            memfd: self.fd.as_raw_fd() as u32,
            flags: UDMABUF_FLAGS_CLOEXEC,
            offset: 0,
            size: self.len as u64,
        };
        match unsafe { ioctl(&dev, UdmabufCreate { data: &mut arg }) } {
            Ok(fd) => Some(unsafe { OwnedFd::from_raw_fd(fd) }),
            Err(e) => {
                crate::trace!("udmabuf: create {e}");
                None
            }
        }
    }

    pub fn offset(self: &Rc<Self>, offset: usize) -> ClientMemOffset {
        ClientMemOffset {
            mem: self.clone(),
            offset,
        }
    }
}

impl Drop for ClientMem {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            let _ = unsafe { munmap(self.ptr, self.len) };
        }
    }
}

pub struct ClientMemOffset {
    mem: Rc<ClientMem>,
    offset: usize,
}

/// how the renderer reaches pixels: sealed pools by pointer, unsealed ones
/// through fd reads that cannot fault
#[allow(dead_code)]
pub enum ShmAccess<'a> {
    Ptr(*const u8),
    Fd { fd: &'a Rc<OwnedFd>, offset: usize },
}

impl ClientMemOffset {
    pub fn pool(&self) -> &Rc<ClientMem> {
        &self.mem
    }

    pub fn pool_offset(&self) -> usize {
        self.offset
    }

    /// writes go through the fd: the mapping is PROT_READ only
    pub fn write_target(&self) -> (&Rc<OwnedFd>, usize) {
        (&self.mem.fd, self.offset)
    }

    #[allow(dead_code)]
    pub fn safe_access(&self) -> ShmAccess<'_> {
        if self.mem.sealed {
            ShmAccess::Ptr(unsafe { self.mem.ptr.cast::<u8>().add(self.offset) })
        } else {
            ShmAccess::Fd {
                fd: &self.mem.fd,
                offset: self.offset,
            }
        }
    }
}

// -- udmabuf plumbing --

const UDMABUF_FLAGS_CLOEXEC: u32 = 0x01;

#[repr(C)]
struct udmabuf_create {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

struct UdmabufCreate<'a> {
    data: &'a mut udmabuf_create,
}

unsafe impl Ioctl for UdmabufCreate<'_> {
    // the ioctl's return value is the new dmabuf fd
    type Output = i32;
    const IS_MUTATING: bool = false;

    fn opcode(&self) -> Opcode {
        opcode::write::<udmabuf_create>(b'u', 0x42)
    }

    fn as_ptr(&mut self) -> *mut c_void {
        (self.data as *mut udmabuf_create).cast()
    }

    unsafe fn output_from_ptr(out: IoctlOutput, _: *mut c_void) -> rustix::io::Result<i32> {
        Ok(out)
    }
}
