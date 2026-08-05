// the gpu driver's libc family: build, verify, stage, heal. the installer
// and the first-run dev heal share this one path, so every staged set
// passed the same pairing gate no matter which door it came through. the
// heal keys a cache by the origin version carrot links, which is what
// makes `cargo clean` cost a copy instead of a rebuild.

use std::path::{Path, PathBuf};

/// the taproot tag the fetch path builds; pinned per carrot release so
/// a cargo install pairs with the origin this binary links (the pairing
/// check refuses drift regardless)
pub const TAPROOT_TAG: &str = "v0.22.7";

/// the eight names a driver closure may ask for, staged as one set
pub fn names() -> Vec<&'static str> {
    ["libc.so.6", "libm.so.6"]
        .into_iter()
        .chain(crate::render::loader::STUB_SONAMES)
        .collect()
}

/// the per-file env override the loader honors for this name
pub(crate) fn env_for(name: &str) -> &'static str {
    match name {
        "libc.so.6" => "CARROT_LIBC",
        "libm.so.6" => "CARROT_LIBM",
        // the stubs have no per-file override; only CARROT_TAPROOT_DIR
        // and the on-disk search cover them
        _ => "CARROT_STUB_UNSET",
    }
}

pub enum Source {
    /// a taproot checkout on disk (the dev sibling)
    Sibling(PathBuf),
    /// the pinned release tarball, fetched with curl
    Fetch,
}

/// build the cdylib and stub from the given source and lay the eight
/// family names out in a temp dir. the pairing check downstream still
/// guards every result; nothing from here lands unverified.
pub fn build(source: Source) -> Result<PathBuf, String> {
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

    let tmp = std::env::temp_dir().join(format!("carrot-taproot-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Some((uid, gid, _)) = &ids {
        std::os::unix::fs::chown(&tmp, Some(*uid), Some(*gid))
            .map_err(|e| format!("chown {}: {e}", tmp.display()))?;
    }

    let src = match source {
        Source::Sibling(dir) => dir,
        Source::Fetch => {
            let url = std::env::var("CARROT_TAPROOT_URL").unwrap_or_else(|_| {
                format!(
                    "https://github.com/carrot-wm/taproot/archive/refs/tags/{TAPROOT_TAG}.tar.gz"
                )
            });
            let tarball = tmp.join("taproot.tar.gz");
            println!("carrot: fetching taproot {TAPROOT_TAG}");
            ok("curl", mk("curl").args(["-fsSL", "-o"]).arg(&tarball).arg(&url))?;
            let gz =
                std::fs::read(&tarball).map_err(|e| format!("{}: {e}", tarball.display()))?;
            untar(&gunzip(&gz)?, &tmp)?;
            // extraction ran in-process (as root under sudo); the user's
            // cargo must be able to write target/ into the tree
            if let Some((uid, gid, _)) = &ids {
                chown_tree(&tmp, *uid, *gid)?;
            }
            std::fs::read_dir(&tmp)
                .map_err(|e| format!("{}: {e}", tmp.display()))?
                .flatten()
                .map(|e| e.path())
                .find(|p| p.is_dir())
                .ok_or("the taproot tarball unpacked to no directory")?
        }
    };

    println!("carrot: building the libc family with your cargo (takes a few minutes once)");
    // no RUSTFLAGS: shared libraries cannot take crt-static, and the
    // caller's flags from the carrot build would cascade. the target-dir
    // and encoded variants scrub with it so a wrapping build context
    // cannot redirect or reflavor the artifacts. stable pins the same
    // compiler as the carrot build when rustup is in play
    let (prog, lead) = find_cargo(&ids);
    let mut c = mk(&prog);
    c.args(&lead)
        .args(["build", "--release", "--locked", "-p", "taproot", "-p", "taproot-stub"])
        .current_dir(&src)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTUP_TOOLCHAIN", "stable");
    ok("cargo", &mut c)?;

    let rel = src.join("target/release");
    let fam = tmp.join("family");
    std::fs::create_dir_all(&fam).map_err(|e| format!("{}: {e}", fam.display()))?;
    for name in names() {
        let src_so = if name == "libc.so.6" || name == "libm.so.6" {
            rel.join("libtaproot.so")
        } else {
            rel.join("libtaproot_stub.so")
        };
        std::fs::copy(&src_so, fam.join(name))
            .map_err(|e| format!("{}: {e}", src_so.display()))?;
    }
    Ok(fam)
}

/// prove the set pairs with this very binary, then land it as one whole:
/// all eight staged, strangers swept, so generations cannot mix
pub fn verify_and_stage(dir: &Path, dest: &Path) -> Result<(), String> {
    let src_libc = dir.join("libc.so.6");
    let lib = dlopen_rs::ElfLibrary::dlopen(
        &src_libc,
        dlopen_rs::OpenFlags::RTLD_NOW | dlopen_rs::OpenFlags::RTLD_LOCAL,
    )
    .map_err(|e| format!("{}: {e}", src_libc.display()))?;
    crate::render::loader::pairing_check(&lib, &src_libc)?;
    // a libc copy must never unmap, even a probe
    std::mem::forget(lib);
    for name in names() {
        put_bin(&dir.join(name), &dest.join(name))?;
    }
    if let Ok(rd) = std::fs::read_dir(dest) {
        for e in rd.flatten() {
            let n = e.file_name();
            if !names().iter().any(|f| n.as_os_str() == *f) {
                let _ = std::fs::remove_file(e.path());
                println!("  removed stale {}", e.path().display());
            }
        }
    }
    Ok(())
}

// -- the first-run dev heal --

/// the verified family cache for the origin this binary links; keyed so
/// an origin bump can never serve yesterday's layout
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("carrot/family")
        .join(env!("CARROT_ORIGIN_VERSION"))
}

fn complete(dir: &Path) -> bool {
    names().iter().all(|n| dir.join(n).exists())
}

/// the bin dir when this exe lives in a cargo target tree, else None. a
/// packaged carrot (or anything in /nix/store) never self-modifies
pub(crate) fn dev_bindir(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    exe.ancestors().any(|a| a.file_name().is_some_and(|n| n == "target")).then(|| dir.to_path_buf())
}

/// a dev build heals its own missing family, once: cache hit is a copy,
/// cache miss builds from the sibling checkout or the pinned tarball,
/// verified either way. packaged trees resolve on the first try and
/// never reach any of this.
pub fn ensure_dev_family() {
    let resolved = names()
        .iter()
        .all(|n| crate::render::loader::taproot_lib(n, env_for(n)).is_ok());
    if resolved {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(bindir) = dev_bindir(&exe) else { return };
    let cache = cache_dir();
    if !complete(&cache) {
        eprintln!(
            "carrot: gpu libc family missing; building it once (cached at {})",
            cache.display()
        );
        if let Err(e) = std::fs::create_dir_all(&cache) {
            eprintln!("carrot: family heal: {}: {e}", cache.display());
            return;
        }
        if let Err(e) = heal_into(&cache) {
            eprintln!("carrot: family heal: {e}");
            return;
        }
    }
    // seed the tree the loader searches; preload re-checks the pairing on
    // every launch regardless, so a poisoned cache cannot slip through
    for n in names() {
        if let Err(e) = put_bin(&cache.join(n), &bindir.join(n)) {
            eprintln!("carrot: family heal: {e}");
            return;
        }
    }
}

/// sibling checkout first (free and current), the pinned tarball second;
/// a sibling that builds but does not pair falls through to the tag
fn heal_into(cache: &Path) -> Result<(), String> {
    if let Some(sib) = sibling_checkout() {
        match build(Source::Sibling(sib)) {
            Ok(fam) => match verify_and_stage(&fam, cache) {
                Ok(()) => return Ok(()),
                Err(e) => eprintln!(
                    "carrot: family heal: the sibling build does not pair ({e}); \
                     fetching {TAPROOT_TAG} instead"
                ),
            },
            Err(e) => eprintln!(
                "carrot: family heal: sibling build failed ({e}); \
                 fetching {TAPROOT_TAG} instead"
            ),
        }
    }
    let fam = build(Source::Fetch)?;
    verify_and_stage(&fam, cache)
}

/// ../taproot next to the carrot checkout, located from the exe's target
/// dir; only a dir that carries the cdylib member counts
fn sibling_checkout() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let target = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target"))?;
    let repo = target.parent()?;
    let sib = repo.parent()?.join("taproot");
    sib.join("taproot/Cargo.toml").exists().then_some(sib)
}

/// a cargo to build with: the rustup home, then PATH, then on nix
/// systems the carrot dev shell (a session on this class of machine may
/// carry no ambient toolchain at all - the dev shell pins the compiler)
fn find_cargo(ids: &Option<(u32, u32, PathBuf)>) -> (String, Vec<String>) {
    let home = ids
        .as_ref()
        .map(|(_, _, h)| h.clone())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_default();
    let rustup = home.join(".cargo/bin/cargo");
    if rustup.exists() {
        return (rustup.display().to_string(), Vec::new());
    }
    let on_path = std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| d.join("cargo").exists())
    });
    if on_path {
        return ("cargo".into(), Vec::new());
    }
    if let Some(repo) = carrot_repo_root() {
        if repo.join("flake.nix").exists() {
            return (
                "nix".into(),
                vec!["develop".into(), repo.display().to_string(), "-c".into(), "cargo".into()],
            );
        }
    }
    // let the spawn fail with the honest "is it installed?" message
    ("cargo".into(), Vec::new())
}

/// the checkout this dev binary was built from, via its target ancestor
fn carrot_repo_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let target = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target"))?;
    target.parent().map(Path::to_path_buf)
}

// -- shared helpers --

/// under sudo, a build must run as the invoking user: root has no cargo,
/// and the toolchain lives under the user's home. /etc/passwd gives the
/// home dir; SUDO_UID/SUDO_GID give the ids for the children
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

/// hand an extracted tree to the sudo user so their cargo can write
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

pub(crate) fn put_bin(src: &Path, dst: &Path) -> Result<(), String> {
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
                    if let Some((_, kv)) = rec.split_once(' ')
                        && let Some(p) = kv.strip_prefix("path=")
                    {
                        long_name = Some(p.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_tree_is_recognized_by_its_target_ancestor() {
        let dev = Path::new("/home/x/proj/target/x86_64-unknown-linux-gnu/release/carrot");
        assert_eq!(
            dev_bindir(dev),
            Some(PathBuf::from("/home/x/proj/target/x86_64-unknown-linux-gnu/release"))
        );
        let debug = Path::new("/x/target/debug/carrot");
        assert_eq!(dev_bindir(debug), Some(PathBuf::from("/x/target/debug")));
        // packaged and store paths never self-modify
        assert_eq!(dev_bindir(Path::new("/usr/local/bin/carrot")), None);
        assert_eq!(dev_bindir(Path::new("/nix/store/abc-carrot-0.1.1/bin/carrot")), None);
        assert_eq!(dev_bindir(Path::new("/home/x/.cargo/bin/carrot")), None);
    }

    #[test]
    fn the_family_is_eight_names_led_by_the_real_pair() {
        let n = names();
        assert_eq!(n.len(), 8);
        assert_eq!(&n[..2], &["libc.so.6", "libm.so.6"]);
        assert!(n.contains(&"ld-linux-x86-64.so.2"));
        assert_eq!(env_for("libc.so.6"), "CARROT_LIBC");
        assert_eq!(env_for("libm.so.6"), "CARROT_LIBM");
        assert_eq!(env_for("libpthread.so.0"), "CARROT_STUB_UNSET");
    }
}
