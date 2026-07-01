// module layout is carved up front - the empty mods are intentional, they
// keep any one file from growing into a god module later.

// core runtime
mod engine;
mod state;
mod trace;
mod uring;

// wayland side
mod client;
mod protocol;
mod shell;
mod socket;
mod surface;

// display side
mod allocator;
mod drm;
mod render;

// the rest
mod carrotconx;
mod config;
mod dbus;
mod input;
mod ipc;
mod tree;
mod xwayland;

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("carrot {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let sock = socket::WaylandSocket::new().unwrap();
    println!("listening on {}", sock.name);

    // TODO: bring up the ring + engine, then accept clients as tasks
}
