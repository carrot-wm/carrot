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
mod clientmem;
mod protocol;
mod shell;
mod socket;
mod surface;

// display side
mod allocator;
mod drm;
mod format;
mod output;
mod render;
mod spike;

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
    if std::env::args().any(|a| a == "spike-scanout") {
        std::process::exit(spike::run());
    }
    if std::env::args().any(|a| a == "drm-probe") {
        std::process::exit(drm::device::probe_dump());
    }
    if std::env::args().any(|a| a == "render-probe") {
        std::process::exit(render::renderer::probe());
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
    state.globals.add(std::rc::Rc::new(surface::WlCompositorGlobal));
    state.globals.add(std::rc::Rc::new(surface::WlSubcompositorGlobal));
    state.globals.add(std::rc::Rc::new(protocol::shm::WlShmGlobal));
    state.globals.add(std::rc::Rc::new(shell::xdg::XdgWmBaseGlobal));
    let st = state.clone();
    let configure_pump = engine.spawn("configure pump", async move {
        shell::xdg::configure_loop(st).await;
    });

    // headless is a supported mode; the display comes up when a card is
    // available, otherwise carrot still serves the socket
    let st = state.clone();
    let bring_up = engine.spawn("bring-up", async move {
        let display = output::start(&st, None).await;
        *st.display.borrow_mut() = display;
    });

    let listen_fd = std::rc::Rc::new(sock.fd.try_clone()?);
    let st = state.clone();
    let acceptor = engine.spawn("acceptor", async move {
        let _sock = sock;
        loop {
            match st.ring.accept(&listen_fd).await {
                Ok(fd) => st.clients.spawn(&st, fd),
                Err(e) => {
                    // a broken listening socket takes the session with it
                    eprintln!("carrot: accept failed: {e}");
                    st.ring.stop();
                    return;
                }
            }
        }
    });

    // the ring owns the loop; this blocks until something calls stop()
    let res = ring.run();

    drop(acceptor);
    drop(bring_up);
    drop(configure_pump);
    state.clear();
    engine.clear();
    res?;
    Ok(())
}
