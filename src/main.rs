// module layout is carved up front - the empty mods are intentional, they
// keep any one file from growing into a god module later.

// core runtime
mod engine;
mod rect;
mod state;
mod trace;
mod uring;
mod util;

// wayland side
mod client;
mod protocol;
mod shell;
mod socket;
mod surface;

// display side
mod allocator;
mod drm;
mod format;
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

    if let Err(e) = run() {
        eprintln!("carrot: fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let engine = engine::Engine::new();
    let ring = uring::Ring::new(&engine, 32)?;
    let wheel = engine::Wheel::new(&engine, &ring)?;
    let sock = socket::WaylandSocket::new()?;
    println!("listening on {}", sock.name);
    let state = state::State::new(&engine, &ring, wheel);

    // the ring owns the loop; this blocks until something calls stop().
    // clients get accepted here once the client module lands.
    let res = ring.run();

    state.clear();
    engine.clear();
    res?;
    Ok(())
}
