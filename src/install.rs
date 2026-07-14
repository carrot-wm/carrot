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
        // keep a driver closure from reaching RUNPATH for real glibc
        let mut libs = 0;
        for name in [
            "libc.so.6",
            "libm.so.6",
            "libpthread.so.0",
            "libdl.so.2",
            "librt.so.1",
            "libutil.so.1",
            "libresolv.so.2",
            "ld-linux-x86-64.so.2",
        ] {
            match exe_dir.as_ref().map(|d| d.join(name)) {
                Some(src) if src.exists() => {
                    put_bin(&src, &stage(&prefix.join("lib/carrot").join(name)))?;
                    libs += 1;
                }
                _ => {}
            }
        }
        if libs < 2 {
            eprintln!(
                "carrot: install: libc.so.6/libm.so.6 not found next to the \
                 binary - the session will fail at gpu preload. build the \
                 taproot cdylib and place copies next to the carrot binary, \
                 then rerun install (see README: Building)"
            );
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
    std::fs::copy(src, dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", dst.display()))?;
    println!("  {}", dst.display());
    Ok(())
}

fn usage() -> i32 {
    eprintln!("usage: carrot install [--prefix DIR] [--root DIR]");
    1
}
