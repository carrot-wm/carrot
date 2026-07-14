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

/// locate one of taproot's libs: explicit env override, next to the
/// binary (the flake stages them there), or ../lib/carrot relative to it
/// (where `carrot install` stages them)
fn taproot_lib(name: &str, env: &str) -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os(env) {
        return Ok(p.into());
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

/// preload libc.so.6 + libm.so.6 GLOBAL by absolute path, once. every NEEDED
/// libc.so.6/libm.so.6 in the icd's closure then reuses these by filename
/// instead of searching RUNPATH and finding glibc. the handles are leaked on
/// purpose: a libc must never unmap.
fn preload() -> Result<(), String> {
    static DONE: OnceLock<Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        let libc_path = taproot_lib("libc.so.6", "CARROT_LIBC")?;
        let libm_path = taproot_lib("libm.so.6", "CARROT_LIBM")?;
        for p in [&libc_path, &libm_path] {
            let lib = ElfLibrary::dlopen(p, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL)
                .map_err(|e| format!("preload {}: {e}", p.display()))?;
            std::mem::forget(lib);
        }
        Ok(())
    })
    .clone()
}

// -- icd discovery --

/// the card's kernel driver name, from the device node's sysfs entry
fn kernel_driver(card: BorrowedFd<'_>) -> Result<String, String> {
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
        _ => icd_file.contains(driver),
    }
}

/// every icd driver .so discoverable on this system, in discovery order.
/// VK_ICD_FILENAMES entries may be files or directories; then the standard
/// dirs (nixos puts the running system's drivers under /run/opengl-driver)
fn all_icd_libraries() -> Vec<PathBuf> {
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
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json") {
                out.push(p);
            }
        }
    }
}

/// "library_path" out of an icd manifest, relative paths anchored at the
/// manifest
fn resolve_manifest(manifest: &Path) -> Option<PathBuf> {
    let txt = std::fs::read_to_string(manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let lp = PathBuf::from(v.get("ICD")?.get("library_path")?.as_str()?);
    Some(if lp.is_absolute() {
        lp
    } else {
        manifest.parent().unwrap_or(Path::new(".")).join(lp)
    })
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
    // can't satisfy fails here, loudly, naming itself
    let lib = ElfLibrary::dlopen(path, OpenFlags::RTLD_NOW | OpenFlags::RTLD_GLOBAL)
        .map_err(|e| format!("dlopen {}: {e}", path.display()))?;

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
    let icd = all
        .iter()
        .find(|p| icd_matches(&driver, &file(p).to_lowercase()))
        .ok_or_else(|| {
            let found: Vec<_> = all.iter().map(|p| file(p)).collect();
            RenderError::Load(format!(
                "no vulkan icd for drm driver {driver} (found: {})",
                found.join(", ")
            ))
        })?;
    eprintln!("carrot: vulkan: {} for {driver}", icd.display());
    entry_for_icd(icd).map_err(RenderError::Load)
}
