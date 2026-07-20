// the vulkan entry without the system loader: preload taproot's pure-rust
// libc.so.6/libm.so.6 GLOBAL, dlopen the card's mesa icd with dlopen-rs
// (a pure-rust dynamic linker), negotiate the loader<->icd interface, and
// build the ash entry from vk_icdGetInstanceProcAddr. the driver and its
// closure are loaded, never linked - the binary itself stays free of C.

use super::vulkan::RenderError;
use ash::vk;
use dlopen_rs::{ElfLibrary, OpenFlags};
use rustix::fs::fstat;
use std::ffi::c_char;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// the icd's global entry point: same ABI as vkGetInstanceProcAddr, valid for
/// NULL-instance global calls
type IcdGipa = unsafe extern "system" fn(vk::Instance, *const c_char) -> vk::PFN_vkVoidFunction;

// -- taproot preload --

/// locate one of taproot's libs: explicit env override, a staging dir
/// override, next to the binary (the flake stages them there), or
/// ../lib/carrot relative to it (where `carrot install` stages them)
pub(crate) fn taproot_lib(name: &str, env: &str) -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os(env) {
        return Ok(p.into());
    }
    if let Some(dir) = std::env::var_os("CARROT_TAPROOT_DIR") {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for p in [dir.join(name), dir.join("../lib/carrot").join(name)] {
                if p.exists() {
                    return Ok(p);
                }
            }
        }
    }
    Err(format!(
        "{name} not found next to the binary or in ../lib/carrot (set {env} \
         to a copy of libtaproot.so named {name}; dlopen matches by filename)"
    ))
}

// -- static tls surplus --

/// drivers built with DF_STATIC_TLS (nvidia's tls shim) need their block
/// at a fixed tp-relative offset in every thread. this surplus lives in
/// carrot's own tls segment, so it already exists everywhere, zeroed by
/// the runtime - which is why only zero-image (tbss) blocks are served.
const SURPLUS_SIZE: usize = 1024;
const SURPLUS_ALIGN: usize = 64;

// served from a const-init std::thread_local!: for a non-Drop type this
// lowers to a native .tbss "awT" entry at a link-time-constant tp offset
// (verified on stable 1.96.1), which is exactly what #[thread_local] gave
// us without the nightly gate. fallback if std ever changes the lowering:
// a global_asm .tbss symbol + lea sym@TPOFF accessor, see the design spec.
// UnsafeCell because drivers write through the donated block.
#[repr(align(64))]
struct TlsSurplus(std::cell::UnsafeCell<[u8; SURPLUS_SIZE]>);

std::thread_local! {
    static TLS_SURPLUS: TlsSurplus =
        const { TlsSurplus(std::cell::UnsafeCell::new([0; SURPLUS_SIZE])) };
}

/// this thread's copy locates the block; the tp-relative offset is the
/// same in every thread, static tls being exactly that
fn surplus_tp_offset() -> isize {
    let tp: usize;
    unsafe { std::arch::asm!("mov {}, fs:0", out(reg) tp) };
    let base = TLS_SURPLUS.with(|t| t.0.get() as usize);
    base as isize - tp as isize
}

static SURPLUS_NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn alloc_static_tls(size: usize, align: usize) -> Option<isize> {
    use std::sync::atomic::Ordering;
    if align > SURPLUS_ALIGN {
        return None;
    }
    let start = loop {
        let cur = SURPLUS_NEXT.load(Ordering::SeqCst);
        let start = (cur + align - 1) & !(align - 1);
        if start + size > SURPLUS_SIZE {
            return None;
        }
        if SURPLUS_NEXT
            .compare_exchange(cur, start + size, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            break start;
        }
    };
    Some(surplus_tp_offset() + start as isize)
}

/// the legacy sonames an icd closure may name; each staged file is an
/// empty taproot stub whose symbols all live in the preloaded libc.so.6.
/// without these, RUNPATH hands the closure real glibc pieces, and mixed
/// lock implementations deadlock the driver's initializers
pub(crate) const STUB_SONAMES: [&str; 6] = [
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
    "libutil.so.1",
    "libresolv.so.2",
    "ld-linux-x86-64.so.2",
];

/// preload the taproot family GLOBAL by absolute path, once. every NEEDED
/// entry in the icd's closure then reuses these by filename instead of
/// searching RUNPATH and finding glibc. the handles are leaked on
/// purpose: a libc must never unmap.
pub(crate) fn preload() -> Result<(), String> {
    static DONE: OnceLock<Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        elf_loader::tls::set_static_tls_allocator(alloc_static_tls);
        // a driver's closure (mesa->llvm->ncurses) references glibc
        // symbols taproot does not export - fortify __*_chk wrappers, C23
        // __isoc23_* aliases, locale - which it never calls on this path.
        // bind them to a trap instead of failing the whole dlopen, and
        // name each so a genuinely-called gap still surfaces
        elf_loader::relocation::set_unresolved_handler(|name| {
            eprintln!("carrot: vulkan: driver references {name}, unprovided; stubbed (traps only if called)");
        });
        let libc_path = taproot_lib("libc.so.6", "CARROT_LIBC")?;
        let libm_path = taproot_lib("libm.so.6", "CARROT_LIBM")?;
        for p in [&libc_path, &libm_path] {
            let lib = ElfLibrary::dlopen(p, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL)
                .map_err(|e| format!("preload {}: {e}", p.display()))?;
            std::mem::forget(lib);
        }
        for name in STUB_SONAMES {
            let Ok(p) = taproot_lib(name, "CARROT_STUB_UNSET") else {
                // an older staging: glibc may leak into heavy closures
                eprintln!("carrot: vulkan: {name} stub not staged; run carrot install");
                continue;
            };
            let lib = ElfLibrary::dlopen(&p, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL)
                .map_err(|e| format!("preload {}: {e}", p.display()))?;
            std::mem::forget(lib);
        }
        Ok(())
    })
    .clone()
}

// -- icd discovery --

/// the card's kernel driver name, from the device node's sysfs entry
pub(crate) fn kernel_driver(card: BorrowedFd<'_>) -> Result<String, String> {
    let rdev = fstat(card).map_err(|e| format!("fstat card: {e}"))?.st_rdev;
    let path = format!(
        "/sys/dev/char/{}:{}/device/driver",
        rustix::fs::major(rdev),
        rustix::fs::minor(rdev)
    );
    let link = std::fs::read_link(&path).map_err(|e| format!("{path}: {e}"))?;
    link.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{path}: no driver name"))
}

/// does this icd library serve the card's kernel driver? loading only the
/// matching icd keeps foreign drivers (and their quirks) out of the process
fn icd_matches(driver: &str, icd_file: &str) -> bool {
    match driver {
        "i915" | "xe" => icd_file.contains("intel") && !icd_file.contains("hasvk"),
        "amdgpu" | "radeon" => icd_file.contains("radeon"),
        "nouveau" => icd_file.contains("nouveau") || icd_file.contains("nvk"),
        // qemu's paravirtual gpu: the kernel names it virtio_gpu, mesa's
        // venus icd is libvulkan_virtio.so; the substring fallback below
        // never connects the two
        "virtio_gpu" => icd_file.contains("virtio"),
        _ => icd_file.contains(driver),
    }
}

/// every icd driver .so discoverable on this system, in discovery order.
/// VK_ICD_FILENAMES entries may be files or directories; then the standard
/// dirs (nixos puts the running system's drivers under /run/opengl-driver)
pub(crate) fn all_icd_libraries() -> Vec<PathBuf> {
    let mut manifests: Vec<PathBuf> = Vec::new();

    if let Ok(spec) = std::env::var("VK_ICD_FILENAMES") {
        for entry in spec.split(':').filter(|s| !s.is_empty()) {
            let p = Path::new(entry);
            if p.is_dir() {
                collect_json(p, &mut manifests);
            } else if p.is_file() {
                manifests.push(p.to_path_buf());
            }
        }
    }
    for dir in [
        "/run/opengl-driver/share/vulkan/icd.d",
        "/usr/share/vulkan/icd.d",
        "/etc/vulkan/icd.d",
    ] {
        collect_json(Path::new(dir), &mut manifests);
    }

    let mut out = Vec::new();
    for m in &manifests {
        if let Some(lib) = resolve_manifest(m) {
            if !out.contains(&lib) {
                out.push(lib);
            }
        }
    }
    out
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        let start = out.len();
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json") {
                out.push(p);
            }
        }
        // readdir order is filesystem-layout history; identical systems
        // must pick identical drivers
        out[start..].sort();
    }
}

/// is this file an ELF for the machine carrot runs on? multilib systems
/// ship the 32-bit halves of mesa in the same manifest dirs, and a
/// foreign-class library must be skipped, never dlopened
fn native_elf(p: &Path) -> bool {
    use std::io::Read;
    let mut ident = [0u8; 20];
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    if f.read_exact(&mut ident).is_err() {
        return false;
    }
    ident[..4] == [0x7f, b'E', b'L', b'F']
        && ident[4] == 2 // ELFCLASS64
        && u16::from_le_bytes([ident[18], ident[19]]) == 0x3e // EM_X86_64
}

/// "library_path" out of an icd manifest. absolute paths stand, paths
/// with directories anchor at the manifest, and a bare soname means the
/// system search path (nvidia ships "libGLX_nvidia.so.0"); without a
/// glibc loader we walk the usual lib dirs ourselves
fn resolve_manifest(manifest: &Path) -> Option<PathBuf> {
    let txt = std::fs::read_to_string(manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let icd = v.get("ICD")?;
    // multilib mesa ships 32- and 64-bit manifests side by side (fedora:
    // radeon_icd.x86_64.json and radeon_icd.i686.json); the khronos
    // loader skips foreign arches via library_arch, and the elf ident
    // check below covers manifests too old to carry the field
    if icd
        .get("library_arch")
        .and_then(|a| a.as_str())
        .is_some_and(|a| a != "64")
    {
        return None;
    }
    let lp = PathBuf::from(icd.get("library_path")?.as_str()?);
    let lib = manifest_library(manifest, lp, &lib_search_dirs())?;
    if !native_elf(&lib) {
        eprintln!(
            "carrot: vulkan: {}: {} is not an elf for this machine; skipped",
            manifest.display(),
            lib.display()
        );
        return None;
    }
    Some(lib)
}

fn manifest_library(manifest: &Path, lp: PathBuf, search: &[PathBuf]) -> Option<PathBuf> {
    if lp.is_absolute() {
        return Some(lp);
    }
    if lp.components().count() > 1 {
        return Some(manifest.parent().unwrap_or(Path::new(".")).join(lp));
    }
    for dir in search {
        let p = dir.join(&lp);
        // a foreign-class hit (a bare soname resolving into a 32-bit
        // libdir) keeps searching instead of shadowing the real driver
        if p.exists() && native_elf(&p) {
            return Some(p);
        }
    }
    eprintln!(
        "carrot: vulkan: {}: {} not found in the library dirs",
        manifest.display(),
        lp.display()
    );
    None
}

fn lib_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(paths) = std::env::var_os("LD_LIBRARY_PATH") {
        dirs.extend(std::env::split_paths(&paths));
    }
    for d in [
        // lib64 and the multiarch dir before plain lib: on fedora
        // /usr/lib IS the 32-bit libdir, and a bare soname must not
        // resolve there first
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib",
        "/usr/local/lib",
        "/lib64",
        "/lib",
        "/run/opengl-driver/lib",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

// -- entry --

/// dlopen + negotiate one icd and wrap its proc-addr in an ash entry. cached
/// per path: a driver is loaded once and stays mapped for the process
fn entry_for_icd(path: &Path) -> Result<ash::Entry, String> {
    static LOADED: OnceLock<Mutex<Vec<(PathBuf, ash::Entry)>>> = OnceLock::new();
    let cache = LOADED.get_or_init(|| Mutex::new(Vec::new()));
    let mut cache = cache.lock().unwrap();
    if let Some((_, entry)) = cache.iter().find(|(p, _)| p == path) {
        return Ok(entry.clone());
    }

    // RTLD_NOW forces every relocation immediately: a libc symbol taproot
    // can't satisfy fails here, loudly, naming itself. the bracket logs
    // turn a wedged driver initializer into a pointed report
    eprintln!("carrot: vulkan: loading the driver closure");
    let lib = ElfLibrary::dlopen(path, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL)
        .map_err(|e| format!("dlopen {}: {e}", path.display()))?;
    eprintln!("carrot: vulkan: driver closure loaded");

    // the khronos loader negotiates the icd interface version before anything
    // else; we stand in for it, and without this the icd won't enumerate
    if let Ok(neg) = unsafe {
        lib.get::<unsafe extern "C" fn(*mut u32) -> vk::Result>(
            "vk_icdNegotiateLoaderICDInterfaceVersion",
        )
    } {
        let mut ver: u32 = 5; // the newest loader<->icd interface we speak
        let _ = unsafe { neg(&mut ver) };
    }

    let gipa = unsafe { lib.get::<IcdGipa>("vk_icdGetInstanceProcAddr") }
        .map_err(|e| format!("{}: vk_icdGetInstanceProcAddr: {e}", path.display()))?;
    let static_fn = ash::StaticFn {
        get_instance_proc_addr: unsafe {
            std::mem::transmute::<IcdGipa, vk::PFN_vkGetInstanceProcAddr>(*gipa)
        },
    };
    let entry = unsafe { ash::Entry::from_static_fn(static_fn) };
    std::mem::forget(lib);
    cache.push((path.to_path_buf(), entry.clone()));
    Ok(entry)
}

/// the vulkan entry for this drm card: taproot preloaded, the card's own icd
/// dlopened, everything else untouched
pub fn entry_for(card: BorrowedFd<'_>) -> Result<ash::Entry, RenderError> {
    preload().map_err(RenderError::Load)?;
    let driver = kernel_driver(card).map_err(RenderError::Load)?;
    let all = all_icd_libraries();
    let file = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    // every matching icd gets a chance: one broken candidate (a stale
    // manifest, a driver missing its closure) must not kill the gpu
    // path while a working sibling sits right behind it
    let mut errors = Vec::new();
    for icd in all.iter().filter(|p| icd_matches(&driver, &file(p).to_lowercase())) {
        eprintln!("carrot: vulkan: {} for {driver}", icd.display());
        match entry_for_icd(icd) {
            Ok(entry) => return Ok(entry),
            Err(e) => {
                eprintln!("carrot: vulkan: {e}; trying the next icd");
                errors.push(e);
            }
        }
    }
    Err(RenderError::Load(if errors.is_empty() {
        let found: Vec<_> = all.iter().map(|p| file(p)).collect();
        format!("no vulkan icd for drm driver {driver} (found: {})", found.join(", "))
    } else {
        format!("every {driver} icd failed: {}", errors.join("; "))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the minimal ident native_elf accepts (or, class-flipped, rejects)
    fn write_elf(path: &Path, class: u8) {
        let mut ident = [0u8; 20];
        ident[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        ident[4] = class;
        ident[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
        std::fs::write(path, ident).unwrap();
    }

    #[test]
    fn manifest_paths_resolve_by_shape() {
        let m = Path::new("/usr/share/vulkan/icd.d/x.json");
        let dir = std::env::temp_dir().join(format!("carrot-libdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_elf(&dir.join("libGLX_test.so.0"), 2);
        let search = [dir.clone()];
        assert_eq!(
            manifest_library(m, PathBuf::from("/abs/libvk.so"), &search),
            Some(PathBuf::from("/abs/libvk.so"))
        );
        assert_eq!(
            manifest_library(m, PathBuf::from("../lib/libvk.so"), &search),
            Some(PathBuf::from("/usr/share/vulkan/icd.d/../lib/libvk.so"))
        );
        // a bare soname walks the search dirs, never the manifest dir
        assert_eq!(
            manifest_library(m, PathBuf::from("libGLX_test.so.0"), &search),
            Some(dir.join("libGLX_test.so.0"))
        );
        assert_eq!(manifest_library(m, PathBuf::from("libGLX_absent.so.0"), &search), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bare_soname_skips_foreign_class_hits() {
        // fedora layout: /usr/lib is 32-bit, /usr/lib64 is native. the
        // first-dir hit is ELFCLASS32 and must lose to the later native one
        let base = std::env::temp_dir().join(format!("carrot-multilib-{}", std::process::id()));
        let (lib32, lib64) = (base.join("lib"), base.join("lib64"));
        std::fs::create_dir_all(&lib32).unwrap();
        std::fs::create_dir_all(&lib64).unwrap();
        write_elf(&lib32.join("libvk.so"), 1);
        write_elf(&lib64.join("libvk.so"), 2);
        let m = Path::new("/usr/share/vulkan/icd.d/x.json");
        assert_eq!(
            manifest_library(m, PathBuf::from("libvk.so"), &[lib32.clone(), lib64.clone()]),
            Some(lib64.join("libvk.so"))
        );
        // nothing native anywhere: the search reports a miss
        std::fs::remove_file(lib64.join("libvk.so")).unwrap();
        assert_eq!(manifest_library(m, PathBuf::from("libvk.so"), &[lib32, lib64]), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// drivers spawn workers with the preloaded libc's pthread_create; those
    /// threads must carry the executable's tls image, or every thread-local
    /// access from loader code called back on them goes wild
    #[test]
    #[ignore = "wants the taproot lib paths"]
    fn cdylib_threads_carry_exe_tls() {
        use std::ffi::c_void;

        preload().unwrap();
        let libc_path = taproot_lib("libc.so.6", "CARROT_LIBC").unwrap();
        let lib = ElfLibrary::dlopen(&libc_path, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL)
            .unwrap();

        std::thread_local! {
            static PROBE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }

        extern "C" fn worker(out: *mut c_void) -> *mut c_void {
            // what loader code does when a driver thread calls back in:
            // thread-local writes and heap traffic
            PROBE.with(|p| p.set(0x5eed));
            let mut v = Vec::new();
            for i in 0..4096usize {
                v.push(i.to_string());
            }
            drop(v);
            let got = PROBE.with(|p| p.get());
            unsafe { *(out as *mut usize) = got };
            std::ptr::null_mut()
        }

        type PthreadCreate = unsafe extern "C" fn(
            *mut u64,
            *const c_void,
            extern "C" fn(*mut c_void) -> *mut c_void,
            *mut c_void,
        ) -> i32;
        type PthreadJoin = unsafe extern "C" fn(u64, *mut *mut c_void) -> i32;

        let create = unsafe { lib.get::<PthreadCreate>("pthread_create") }.unwrap();
        let join = unsafe { lib.get::<PthreadJoin>("pthread_join") }.unwrap();

        let mut seen: usize = 0;
        let mut tid: u64 = 0;
        let rc = unsafe {
            create(&mut tid, std::ptr::null(), worker, &mut seen as *mut usize as *mut c_void)
        };
        assert_eq!(rc, 0, "pthread_create through the preloaded libc");
        let rc = unsafe { join(tid, std::ptr::null_mut()) };
        assert_eq!(rc, 0, "pthread_join through the preloaded libc");
        assert_eq!(seen, 0x5eed, "worker's thread-local round trip");
        eprintln!("cdylib thread ok");
        std::mem::forget(lib);
    }

    #[test]
    fn surplus_offset_is_thread_invariant() {
        // the tp-relative offset must be a link-time constant (x86_64 TLS
        // variant II): identical in every thread, 64-aligned, zero image
        let here = surplus_tp_offset();
        let there = std::thread::spawn(surplus_tp_offset).join().unwrap();
        assert_eq!(here, there);
        assert_eq!(here.rem_euclid(64), 0);
        let fresh_sum: u64 = std::thread::spawn(|| {
            TLS_SURPLUS.with(|t| {
                let bytes = unsafe { &*t.0.get() };
                bytes.iter().map(|&b| b as u64).sum()
            })
        })
        .join()
        .unwrap();
        assert_eq!(fresh_sum, 0);
    }

    /// gpu-free by construction: an icd only opens the device node at
    /// vkCreateInstance, so loading its closure is pure elf work. run by
    /// hand when a driver's dlopen wedges:
    ///   CARROT_LIBC=... CARROT_LIBM=... cargo test dlopen_the_cards_icd -- --ignored --nocapture
    #[test]
    #[ignore = "wants the system's icd and taproot lib paths"]
    fn dlopen_the_cards_icd() {
        preload().unwrap();
        let all = all_icd_libraries();
        let icd = all
            .iter()
            .find(|p| {
                let f = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                std::env::var("CARROT_ICD_MATCH").map(|m| f.contains(&m)).unwrap_or(true)
            })
            .expect("an icd manifest resolves");
        eprintln!("dlopen {}", icd.display());
        let lib = ElfLibrary::dlopen(icd, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL).unwrap();
        eprintln!("dlopen done");
        std::mem::forget(lib);
    }
}
