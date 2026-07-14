//! c interface

mod dl_find_object;
pub(crate) mod dl_iterate_phdr;
pub(crate) mod dladdr;
pub(crate) mod dlerror;
pub(crate) mod dlopen;
pub mod dlsym;

use alloc::boxed::Box;
use core::ffi::{c_int, c_void};

pub use self::dl_iterate_phdr::dl_iterate_phdr;
pub use self::dladdr::dladdr;
pub use self::dlerror::dlerror;
pub use self::dlopen::dlopen;
pub use self::dlsym::dlsym;

/// The dl* entry points every loaded object must bind to this loader's
/// implementations. Interposed ahead of the dependency scope during
/// relocation so a host libc's own definitions can never win the lookup.
pub(crate) fn dl_builtin(name: &str) -> Option<*const ()> {
    Some(match name {
        "dlopen" => self::dlopen::dlopen as *const (),
        "dlmopen" => self::dlopen::dlmopen as *const (),
        "dlsym" => self::dlsym::dlsym as *const (),
        "dlvsym" => self::dlsym::dlvsym as *const (),
        "dlclose" => dlclose as *const (),
        "dlerror" => self::dlerror::dlerror as *const (),
        _ => return None,
    })
}

/// # Safety
/// It is the same as `dlclose`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlclose(handle: *const c_void) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let lib = unsafe { Box::from_raw(handle as *mut crate::ElfLibrary) };
    let shortname = lib.shortname();
    log::info!("dlclose: Closing [{}]", shortname);
    0
}
