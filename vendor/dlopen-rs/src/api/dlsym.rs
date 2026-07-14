use crate::core_impl::loader::find_symbol;
use crate::core_impl::register::{global_find, next_find};
use crate::{Result, Symbol, error::find_symbol_error};
use core::{
    ffi::{CStr, c_char, c_void},
    ptr::null,
};

/// # Safety
/// It is the same as `dlsym`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlsym(handle: *const c_void, symbol_name: *const c_char) -> *const c_void {
    const RTLD_DEFAULT: usize = 0;
    const RTLD_NEXT: usize = usize::MAX;
    let value = handle as usize;
    let name = match unsafe { CStr::from_ptr(symbol_name).to_str() } {
        Ok(name) => name,
        Err(_) => return null(),
    };

    let sym = if value == RTLD_DEFAULT {
        log::info!("dlsym: Use RTLD_DEFAULT flag to find symbol [{}]", name);
        // the dl* family binds to this loader here too, ahead of any
        // libc's own definitions in the global scope
        crate::api::dl_builtin(name)
            .map(|s| s.cast::<()>() as *const ())
            .or_else(|| dlsym_default::<()>(name).ok().map(|s| s.into_raw()))
    } else if value == RTLD_NEXT {
        log::info!("dlsym: Use RTLD_NEXT flag to find symbol [{}]", name);
        unsafe { dlsym_next::<()>(name).ok().map(|s| s.into_raw()) }
    } else {
        let lib = unsafe { &*(handle as *const crate::ElfLibrary) };
        let libs = lib.deps.as_ref().unwrap();
        // ld.so consults the global scope after the handle's dep scope
        // (RTLD_GLOBAL objects); without it a lookup through a stub like
        // libpthread.so.0 can't reach libc's definitions
        find_symbol::<()>(&libs[..], name)
            .ok()
            .map(|sym| sym.into_raw())
            .or_else(|| unsafe { global_find::<()>(name) }.map(|sym| sym.into_raw()))
    };
    match sym {
        Some(sym) => sym.cast(),
        None => {
            crate::api::dlerror::set_last_error(alloc::format!("undefined symbol: {}", name));
            null()
        }
    }
}

/// # Safety
/// It is the same as `dlvsym`. Version-blind: symbol lookup here ignores
/// version tables, so the versioned probe degrades to the plain lookup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlvsym(
    handle: *const c_void,
    symbol_name: *const c_char,
    _version: *const c_char,
) -> *const c_void {
    unsafe { dlsym(handle, symbol_name) }
}

/// Find a symbol in the global search scope.
#[inline]
pub fn dlsym_default<T>(name: &str) -> Result<Symbol<'static, T>> {
    unsafe { global_find(name) }
        .ok_or_else(|| find_symbol_error(alloc::format!("can not find symbol:{}", name)))
}

/// Find the next occurrence of a symbol in the search order after the caller.
///
/// # Safety
/// This function uses inline assembly to determine the caller's address.
#[inline(always)]
pub unsafe fn dlsym_next<T>(name: &str) -> Result<Symbol<'static, T>> {
    let caller = unsafe {
        let ra: usize;
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            "mov {}, [rbp + 8]",
            out(reg) ra,
            options(nostack, readonly)
        );
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!(
            "mov {}, lr",
            out(reg) ra,
            options(nostack, readonly)
        );
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!(
            "mv {}, ra",
            out(reg) ra,
            options(nostack, readonly)
        );
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )))]
        let ra = 0;
        ra
    };
    unsafe { next_find(caller, name) }
        .ok_or_else(|| find_symbol_error(alloc::format!("can not find symbol:{}", name)))
}
