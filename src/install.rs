// `carrot install`: everything a display-manager session needs, written
// by the binary itself so the tree stays code-only. --prefix is where the
// session runs from; --root is a staging directory for packagers, written
// into without leaking into the recorded paths. the libc-family machinery
// (fetch, build, verify, stage) lives in crate::family, shared with the
// dev tree's first-run heal.

use crate::family;
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

const UDMABUF_RULE: &str = "KERNEL==\"udmabuf\", TAG+=\"uaccess\"\n";

pub fn run(args: &[String]) -> i32 {
    let mut prefix = PathBuf::from("/usr/local");
    let mut root = PathBuf::from("/");
    let mut build_taproot_flag = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let value = match a.as_str() {
            "--build-taproot" => {
                build_taproot_flag = true;
                continue;
            }
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
        family::put_bin(Path::new("/proc/self/exe"), &stage(&bin))?;
        let exe_dir = std::fs::read_link("/proc/self/exe")
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        // the ipc client builds alongside; a missing one is not fatal
        match exe_dir.as_ref().map(|d| d.join("burrow")) {
            Some(src) if src.exists() => {
                family::put_bin(&src, &stage(&prefix.join("bin/burrow")))?;
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
        let family_names = family::names();
        // --build-taproot supplies the family itself; otherwise it must
        // sit next to the binary (packages and the flake stage it there)
        let family_dir: Option<PathBuf> = if build_taproot_flag {
            Some(family::build(family::Source::Fetch)?)
        } else {
            exe_dir.clone()
        };
        let present: Vec<&str> = match family_dir.as_ref() {
            Some(d) => {
                family_names.iter().copied().filter(|n| d.join(n).exists()).collect()
            }
            None => Vec::new(),
        };
        if present.is_empty() {
            eprintln!(
                "carrot: install: no libc family next to the binary - the \
                 session will fail at gpu preload. rerun as \
                 `carrot install --build-taproot` to fetch and build it \
                 with your own cargo (needs curl and network), or \
                 stage all eight files next to the binary yourself \
                 (see README: Building)"
            );
        } else if present.len() < family_names.len() {
            let missing: Vec<&str> = family_names
                .iter()
                .copied()
                .filter(|n| !present.contains(n))
                .collect();
            return Err(format!(
                "the libc family next to the binary is partial (missing {}); \
                 a mixed staging fails at gpu preload - rebuild taproot and \
                 restage all eight files, then rerun install",
                missing.join(", ")
            ));
        } else {
            // the same check the session runs at preload, moved to the
            // moment that can still refuse: a drifted cdylib never lands
            let dir = family_dir.as_ref().unwrap();
            family::verify_and_stage(dir, &stage(&prefix.join("lib/carrot")))?;
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
        )?;
        // the zero-copy shm bridge opens /dev/udmabuf; uaccess hands it to the
        // active-seat user. 60- so it precedes systemd's 70-uaccess.rules
        put(
            &stage(&prefix.join("lib/udev/rules.d/60-carrot-udmabuf.rules")),
            UDMABUF_RULE,
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

fn usage() -> i32 {
    eprintln!("usage: carrot install [--prefix DIR] [--root DIR] [--build-taproot]");
    1
}

