use std::path::{Path, PathBuf};

/// the taproot-origin pin out of Cargo.lock: the thread-layout authority
/// this binary links, and the key the verified family cache lives under
fn origin_version() -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let lock = std::fs::read_to_string(Path::new(&manifest).join("Cargo.lock"))
        .unwrap_or_default();
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "name = \"taproot-origin\""
            && let Some(v) = lines.next().and_then(|l| l.trim().strip_prefix("version = \""))
        {
            return v.trim_end_matches('"').to_string();
        }
    }
    "unknown".to_string()
}

/// seed the eight family names next to the future binary from the
/// version-keyed cache the runtime heal maintains. pure file copies:
/// no network, no nested cargo, safe in every sandbox. a missing cache
/// is one warning, never an error - the first `./carrot` run fills it.
fn seed_family(ver: &str) {
    let family = [
        "libc.so.6",
        "libm.so.6",
        "libpthread.so.0",
        "libdl.so.2",
        "librt.so.1",
        "libutil.so.1",
        "libresolv.so.2",
        "ld-linux-x86-64.so.2",
    ];
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    // OUT_DIR = target/<triple>/<profile>/build/carrot-<hash>/out
    let Some(bindir) = Path::new(&out_dir).ancestors().nth(3).map(Path::to_path_buf) else {
        return;
    };
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("carrot/family")
        .join(ver);
    // reruns when the cache appears or its generation changes
    println!("cargo:rerun-if-changed={}", cache.display());
    if !family.iter().all(|n| cache.join(n).exists()) {
        println!(
            "cargo:warning=gpu libc family not cached; run ./carrot once (or \
             `carrot install --build-taproot`) to build and cache it - until \
             then the binary runs headless"
        );
        return;
    }
    for n in family {
        // write-then-rename, the same discipline as family::put_bin: a
        // running session has these exact files mapped, and a plain copy
        // truncates the live inode. the session then pages the NEW
        // build's bytes into cold text it never executed and dies hours
        // later on a wild jump inside the driver closure. rename retires
        // the directory entry and leaves the mapped inode whole
        let tmp = bindir.join(format!(".{n}.seed"));
        if std::fs::copy(cache.join(n), &tmp).is_ok() {
            let _ = std::fs::rename(&tmp, bindir.join(n));
        }
    }
}

fn main() {
    let ver = origin_version();
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rustc-env=CARROT_ORIGIN_VERSION={ver}");
    seed_family(&ver);
    // eyra provides program startup; drop the host startfiles.
    println!("cargo:rustc-link-arg=-nostartfiles");
    // link flags ride the build script so registry installs (which never
    // see .cargo/config.toml) link the same way repo builds do.
    // export c-gull into .dynsym so a dlopened lib's libc resolves against us
    println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
    // first definition wins, and rustc orders dependents before dependencies:
    // c-scape's full getauxval shadows origin's few-types shim, and dlopen-rs's
    // real dl* shadow c-scape's stubs. adds no C - link behaviour only.
    println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
    // crt-static can't be set from here, only checked: without it the
    // result is a glibc-hosted binary that was never the tested artifact
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    if !features.split(',').any(|f| f == "crt-static") {
        panic!(
            "carrot links as a static-PIE; build with \
             RUSTFLAGS=\"-C target-feature=+crt-static\" AND \
             --target x86_64-unknown-linux-gnu. the explicit --target keeps \
             the flag off host proc-macros, which cannot be crt-static \
             (without it the build dies at ctor-proc-macro before this \
             message can print). the repo's .cargo/config.toml sets both \
             for clone builds"
        );
    }
}
