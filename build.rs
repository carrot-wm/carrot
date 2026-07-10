fn main() {
    // eyra provides program startup; drop the host startfiles.
    println!("cargo:rustc-link-arg=-nostartfiles");
}
