// client shm pool mappings, mapped eagerly at pool creation. a pool sealed
// against shrinking can never SIGBUS; unsealed pools are read through their fd
// instead of the mapping, so the process needs no signal handler.

use rustix::fs::{SealFlags, fcntl_get_seals, fstat, ftruncate};
use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::fmt;
use std::os::fd::OwnedFd;
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
