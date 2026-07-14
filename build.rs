fn main() {
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
             RUSTFLAGS=\"-C target-feature=+crt-static\" (the repo's \
             .cargo/config.toml sets this for clone builds)"
        );
    }
}
