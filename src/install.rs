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

const UDMABUF_RULE: &str = "KERNEL==\"udmabuf\", TAG+=\"uaccess\"\n";

/// the taproot tag --build-taproot fetches; pinned per carrot release so
/// a cargo install pairs with the origin this binary links (the pairing
/// check refuses drift regardless)
const TAPROOT_TAG: &str = "v0.22.7";

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
        // --build-taproot supplies the family itself; otherwise it must
        // sit next to the binary (packages and the flake stage it there)
        let family_dir: Option<PathBuf> = if build_taproot_flag {
            Some(build_taproot(&family)?)
        } else {
            exe_dir.clone()
        };
        let present: Vec<&str> = match family_dir.as_ref() {
            Some(d) => family.iter().copied().filter(|n| d.join(n).exists()).collect(),
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
            let dir = family_dir.as_ref().unwrap();
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

/// under sudo, the build must run as the invoking user: root has no
/// cargo, and the toolchain lives under the user's home. /etc/passwd
/// gives the home dir; SUDO_UID/SUDO_GID give the ids for the children
fn sudo_user() -> Option<(u32, u32, PathBuf)> {
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    let user = std::env::var("SUDO_USER").ok()?;
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    let home = passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == user).then(|| f.nth(4).map(PathBuf::from))?
    })?;
    Some((uid, gid, home))
}

/// fetch the pinned taproot source with curl, build the cdylib and stub
/// with the user's own cargo (the same recipe every package uses), and
/// lay the eight family names out in a temp dir for the normal staging
/// path. the pairing check downstream still guards the result.
fn build_taproot(family: &[&str]) -> Result<PathBuf, String> {
    use std::process::Command;
    let ids = sudo_user();
    let mk = |prog: &str| -> Command {
        let mut c = Command::new(prog);
        if let Some((uid, gid, home)) = &ids {
            use std::os::unix::process::CommandExt;
            c.uid(*uid).gid(*gid).env("HOME", home);
            let path = std::env::var("PATH").unwrap_or_default();
            c.env("PATH", format!("{}/.cargo/bin:{path}", home.display()));
        }
        c
    };
    let ok = |name: &str, c: &mut Command| -> Result<(), String> {
        let st = c
            .status()
            .map_err(|e| format!("{name}: {e} (is it installed?)"))?;
        if st.success() { Ok(()) } else { Err(format!("{name} failed ({st})")) }
    };

    let url = std::env::var("CARROT_TAPROOT_URL").unwrap_or_else(|_| {
        format!("https://github.com/carrot-wm/taproot/archive/refs/tags/{TAPROOT_TAG}.tar.gz")
    });
    let tmp = std::env::temp_dir().join(format!("carrot-taproot-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Some((uid, gid, _)) = &ids {
        std::os::unix::fs::chown(&tmp, Some(*uid), Some(*gid))
            .map_err(|e| format!("chown {}: {e}", tmp.display()))?;
    }
    let tarball = tmp.join("taproot.tar.gz");
    println!("carrot: install: fetching taproot {TAPROOT_TAG}");
    ok("curl", mk("curl").args(["-fsSL", "-o"]).arg(&tarball).arg(&url))?;
    let gz = std::fs::read(&tarball).map_err(|e| format!("{}: {e}", tarball.display()))?;
    untar(&gunzip(&gz)?, &tmp)?;
    // extraction ran in-process (as root under sudo); the user's cargo
    // must be able to write target/ into the tree
    if let Some((uid, gid, _)) = &ids {
        chown_tree(&tmp, *uid, *gid)?;
    }
    let src = std::fs::read_dir(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .ok_or("the taproot tarball unpacked to no directory")?;

    println!("carrot: install: building the libc family with your cargo (takes a few minutes)");
    // no RUSTFLAGS: shared libraries cannot take crt-static, and the
    // caller's flags from the carrot build would cascade. stable pins
    // the same compiler as the carrot build when rustup is in play
    ok(
        "cargo",
        mk("cargo")
            .args(["build", "--release", "--locked", "-p", "taproot", "-p", "taproot-stub"])
            .current_dir(&src)
            .env_remove("RUSTFLAGS")
            .env("RUSTUP_TOOLCHAIN", "stable"),
    )?;

    let rel = src.join("target/release");
    let fam = tmp.join("family");
    std::fs::create_dir_all(&fam).map_err(|e| format!("{}: {e}", fam.display()))?;
    for name in family {
        let src_so = if *name == "libc.so.6" || *name == "libm.so.6" {
            rel.join("libtaproot.so")
        } else {
            rel.join("libtaproot_stub.so")
        };
        std::fs::copy(&src_so, fam.join(name))
            .map_err(|e| format!("{}: {e}", src_so.display()))?;
    }
    Ok(fam)
}

// -- tarball extraction, in-process --
// gzip framing here, deflate from miniz_oxide, the ustar walk by hand:
// curl stays the only tool the fetch path needs on the host

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn gunzip(gz: &[u8]) -> Result<Vec<u8>, String> {
    let err = |m: &str| format!("gzip: {m}");
    if gz.len() < 18 || gz[0] != 0x1f || gz[1] != 0x8b || gz[2] != 8 {
        return Err(err("not a gzip deflate stream"));
    }
    let flg = gz[3];
    let mut off = 10;
    if flg & 0x04 != 0 {
        let x = *gz.get(off).ok_or_else(|| err("truncated"))? as usize
            | (*gz.get(off + 1).ok_or_else(|| err("truncated"))? as usize) << 8;
        off += 2 + x;
    }
    for bit in [0x08u8, 0x10] {
        if flg & bit != 0 {
            off += gz.get(off..)
                .and_then(|r| r.iter().position(|&b| b == 0))
                .ok_or_else(|| err("truncated"))?
                + 1;
        }
    }
    if flg & 0x02 != 0 {
        off += 2;
    }
    let body = gz.get(off..gz.len() - 8).ok_or_else(|| err("truncated"))?;
    let out = miniz_oxide::inflate::decompress_to_vec(body)
        .map_err(|e| err(&format!("inflate: {e}")))?;
    let tail = &gz[gz.len() - 8..];
    if crc32(&out) != u32::from_le_bytes(tail[0..4].try_into().unwrap())
        || out.len() as u32 != u32::from_le_bytes(tail[4..8].try_into().unwrap())
    {
        return Err(err("checksum mismatch; the download is corrupt"));
    }
    Ok(out)
}

/// ustar plus the two long-name mechanisms real producers use: pax 'x'
/// records (git archive, so every github tarball) and gnu 'L' entries
/// (gnu tar). members must stay inside dest; the installer may be root.
fn untar(tar: &[u8], dest: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let cstr = |b: &[u8]| -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    };
    let octal = |b: &[u8]| -> usize {
        cstr(b).trim().chars().fold(0, |a, c| a * 8 + c.to_digit(8).unwrap_or(0) as usize)
    };
    let mut off = 0;
    let mut long_name: Option<String> = None;
    while off + 512 <= tar.len() {
        let hdr = &tar[off..off + 512];
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let size = octal(&hdr[124..136]);
        let typeflag = hdr[156];
        let data = tar
            .get(off + 512..off + 512 + size)
            .ok_or("tar: truncated member")?;
        off += 512 + size.div_ceil(512) * 512;

        match typeflag {
            b'x' => {
                // pax records: "len key=value\n"; only path matters here
                for rec in cstr(data).split_terminator('\n') {
                    if let Some((_, kv)) = rec.split_once(' ') {
                        if let Some(p) = kv.strip_prefix("path=") {
                            long_name = Some(p.to_string());
                        }
                    }
                }
                continue;
            }
            b'g' => continue,
            b'L' => {
                long_name = Some(cstr(data));
                continue;
            }
            _ => {}
        }
        let name = long_name.take().unwrap_or_else(|| {
            let prefix = cstr(&hdr[345..500]);
            let base = cstr(&hdr[0..100]);
            if prefix.is_empty() { base } else { format!("{prefix}/{base}") }
        });
        let rel = PathBuf::from(&name);
        if rel.is_absolute()
            || rel.components().any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("tar: member escapes the extract dir: {name}"));
        }
        let out = dest.join(&rel);
        match typeflag {
            b'5' => {
                std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            }
            b'0' | 0 => {
                if let Some(d) = out.parent() {
                    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
                }
                std::fs::write(&out, data).map_err(|e| format!("{}: {e}", out.display()))?;
                // the mode matters: tools/link-shim.sh must stay executable
                let mode = octal(&hdr[100..108]) as u32;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(|e| format!("{}: {e}", out.display()))?;
            }
            b'2' => {
                if let Some(d) = out.parent() {
                    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
                }
                let target = cstr(&hdr[157..257]);
                let _ = std::fs::remove_file(&out);
                std::os::unix::fs::symlink(&target, &out)
                    .map_err(|e| format!("{}: {e}", out.display()))?;
            }
            _ => {} // hardlinks, devices: nothing a source tarball needs
        }
    }
    Ok(())
}

/// hand the extracted tree to the sudo user so their cargo can write
/// target/ into it; lchown so a hostile symlink cannot redirect us
fn chown_tree(dir: &Path, uid: u32, gid: u32) -> Result<(), String> {
    std::os::unix::fs::lchown(dir, Some(uid), Some(gid))
        .map_err(|e| format!("chown {}: {e}", dir.display()))?;
    if dir.is_dir() && !dir.is_symlink() {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
            chown_tree(&e.path(), uid, gid)?;
        }
    }
    Ok(())
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
    eprintln!("usage: carrot install [--prefix DIR] [--root DIR] [--build-taproot]");
    1
}
