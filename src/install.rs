// `carrot install`: everything a display-manager session needs, written
// by the binary itself so the tree stays code-only. --prefix is where the
// session runs from; --root is a staging directory for packagers, written
// into without leaking into the recorded paths.

use std::path::{Path, PathBuf};

const DESKTOP: &str = "[Desktop Entry]
Name=Carrot
Comment=A pure Rust tiling Wayland compositor
Exec={bin}
Type=Application
DesktopNames=carrot
";

// the portal backend is the compositor itself - register the bus name it
// serves and prefer it for screencasts
const PORTAL: &str = "[portal]
DBusName=org.freedesktop.impl.portal.desktop.carrot
Interfaces=org.freedesktop.impl.portal.ScreenCast
UseIn=carrot
";

const PORTALS_CONF: &str = "[preferred]
default=*
org.freedesktop.impl.portal.ScreenCast=carrot
";

pub fn run(args: &[String]) -> i32 {
    let mut prefix = PathBuf::from("/usr/local");
    let mut root = PathBuf::from("/");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let value = match a.as_str() {
            "--prefix" | "--root" => match it.next() {
                Some(v) => PathBuf::from(v),
                None => return usage(),
            },
            _ => return usage(),
        };
        if a == "--prefix" {
            prefix = value;
        } else {
            root = value;
        }
    }
    let stage = |p: &Path| root.join(p.strip_prefix("/").unwrap_or(p));
    let bin = prefix.join("bin/carrot");
    let share = prefix.join("share");

    let res = (|| -> Result<(), String> {
        put_bin(Path::new("/proc/self/exe"), &stage(&bin))?;
        let exe_dir = std::fs::read_link("/proc/self/exe")
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        // the ipc client builds alongside; a missing one is not fatal
        match exe_dir.as_ref().map(|d| d.join("burrow")) {
            Some(src) if src.exists() => {
                put_bin(&src, &stage(&prefix.join("bin/burrow")))?;
            }
            _ => eprintln!("carrot: install: no burrow next to the binary, skipped"),
        }
        // the gpu driver's libc: without libc.so.6/libm.so.6 the session
        // dies at icd preload. staged where the loader looks (../lib/carrot
        // from the binary); copies of taproot's libc.so.6. the stub names
        // keep a driver closure from reaching RUNPATH for real glibc.
        // the family installs as one set: a partial source dir is a broken
        // build, refused instead of staged, while none at all keeps the
        // headless flow alive
        let family: Vec<&str> = ["libc.so.6", "libm.so.6"]
            .into_iter()
            .chain(crate::render::loader::STUB_SONAMES)
            .collect();
        let present: Vec<&str> = match exe_dir.as_ref() {
            Some(d) => family.iter().copied().filter(|n| d.join(n).exists()).collect(),
            None => Vec::new(),
        };
        if present.is_empty() {
            eprintln!(
                "carrot: install: no libc family next to the binary - the \
                 session will fail at gpu preload. build the taproot cdylib \
                 and stage all eight files next to the carrot binary, then \
                 rerun install (see README: Building)"
            );
        } else if present.len() < family.len() {
            let missing: Vec<&str> =
                family.iter().copied().filter(|n| !present.contains(n)).collect();
            return Err(format!(
                "the libc family next to the binary is partial (missing {}); \
                 a mixed staging fails at gpu preload - rebuild taproot and \
                 restage all eight files, then rerun install",
                missing.join(", ")
            ));
        } else {
            let dir = exe_dir.as_ref().unwrap();
            // the same check the session runs at preload, moved to the
            // moment that can still refuse: a drifted cdylib never lands
            let src_libc = dir.join("libc.so.6");
            let lib = dlopen_rs::ElfLibrary::dlopen(
                &src_libc,
                dlopen_rs::OpenFlags::RTLD_NOW | dlopen_rs::OpenFlags::RTLD_LOCAL,
            )
            .map_err(|e| format!("{}: {e}", src_libc.display()))?;
            crate::render::loader::pairing_check(&lib, &src_libc)?;
            // a libc copy must never unmap, even the installer's probe
            std::mem::forget(lib);
            let libdir = prefix.join("lib/carrot");
            for name in &family {
                put_bin(&dir.join(name), &stage(&libdir.join(name)))?;
            }
            // sweep strangers so generations cannot mix across upgrades
            if let Ok(rd) = std::fs::read_dir(stage(&libdir)) {
                for e in rd.flatten() {
                    let n = e.file_name();
                    if !family.iter().any(|f| n.as_os_str() == *f) {
                        let _ = std::fs::remove_file(e.path());
                        println!("  removed stale {}", e.path().display());
                    }
                }
            }
        }
        put(
            &stage(&share.join("wayland-sessions/carrot.desktop")),
            &DESKTOP.replace("{bin}", &bin.display().to_string()),
        )?;
        put(
            &stage(&share.join("xdg-desktop-portal/portals/carrot.portal")),
            PORTAL,
        )?;
        put(
            &stage(&share.join("xdg-desktop-portal/carrot-portals.conf")),
            PORTALS_CONF,
        )
    })();
    match res {
        Ok(()) => {
            println!("carrot: installed; pick \"Carrot\" at the display manager");
            0
        }
        Err(e) => {
            eprintln!("carrot: install: {e}");
            1
        }
    }
}

fn put(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("  {}", path.display());
    Ok(())
}

fn put_bin(src: &Path, dst: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // write-then-rename: overwriting in place truncates the old inode,
    // which a running session has mapped (and the running binary itself
    // answers ETXTBSY); a rename retires the inode without touching it
    let tmp = dst.with_extension("carrot-staging");
    std::fs::copy(src, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    println!("  {}", dst.display());
    Ok(())
}

fn usage() -> i32 {
    eprintln!("usage: carrot install [--prefix DIR] [--root DIR]");
    1
}
