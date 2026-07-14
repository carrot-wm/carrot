use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::c_char;
use core::ptr::null;
use spin::Mutex;

/// glibc contract: `dlerror` hands out the most recent failure message once,
/// then null until the next failure. `kept` pins the last returned buffer so
/// the pointer stays valid after the take.
struct LastError {
    pending: Option<CString>,
    kept: Option<CString>,
}

static LAST_ERROR: Mutex<LastError> = Mutex::new(LastError {
    pending: None,
    kept: None,
});

pub(crate) fn set_last_error(msg: String) {
    LAST_ERROR.lock().pending = CString::new(msg).ok();
}

/// # Safety
/// It is the same as `dlerror`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlerror() -> *const c_char {
    let mut guard = LAST_ERROR.lock();
    match guard.pending.take() {
        Some(msg) => {
            let ptr = msg.as_ptr();
            guard.kept = Some(msg);
            ptr
        }
        None => null(),
    }
}
