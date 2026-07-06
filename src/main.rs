// module layout carved up front; empty mods are intentional.

// core runtime
mod cpu_worker;
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
    if std::env::args().any(|a| a == "dbus-probe") {
        std::process::exit(dbus::probe());
    }
    if std::env::args().any(|a| a == "input-probe") {
        std::process::exit(input::probe());
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
    // bind before CpuWorker spawns its thread: the socket's set_var of
    // WAYLAND_DISPLAY is only sound single-threaded
    let sock = socket::WaylandSocket::new()?;
    println!("listening on {}", sock.name);
    let cpu = cpu_worker::CpuWorker::new(&engine, &ring)?;
    let state = state::State::new(&engine, &ring, wheel);
    state.globals.add(std::rc::Rc::new(surface::WlCompositorGlobal));
    state.globals.add(std::rc::Rc::new(surface::WlSubcompositorGlobal));
    state.globals.add(std::rc::Rc::new(protocol::shm::WlShmGlobal));
    state.globals.add(std::rc::Rc::new(shell::xdg::XdgWmBaseGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::data_device::WlDataDeviceManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::primary_selection::PrimarySelectionGlobal));
    state
        .globals
        .add(std::rc::Rc::new(shell::xdg::XdgDecorationManagerGlobal));
    let st = state.clone();
    let configure_pump = engine.spawn("configure pump", async move {
        shell::xdg::configure_loop(st).await;
    });
    match input::seat::SeatGlobal::new() {
        Ok(seat) => {
            state.globals.add(seat.clone());
            *state.seat.borrow_mut() = Some(seat);
        }
        // no seat means no keyboard, and no keyboard means no way to vt
        // switch away once logind darkens the console - bail while the
        // user can still read the error
        Err(e) => {
            eprintln!("carrot: cannot create the seat: {e}");
            state.clear();
            engine.clear();
            return Err(e.into());
        }
    }
    // headless is supported; the display comes up when logind hands over a
    // card (or, without a session, via direct open)
    let st = state.clone();
    let bring_up = engine.spawn("bring-up", async move {
        let session = match dbus::LogindSession::take_control(&st.eng, &st.ring).await {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("carrot: no logind session ({e}); trying direct device access");
                None
            }
        };
        let display = output::start(&st, session.as_ref()).await;
        if let Some(d) = &display {
            st.globals.add(std::rc::Rc::new(d.output_global()));
        }
        if let Some(s) = &session {
            *st.input.borrow_mut() = Some(input::start(&st, s).await);
        }
        *st.session.borrow_mut() = session;
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
    let st = state.clone();
    let police = engine.spawn("slow clients", async move {
        loop {
            let c = st.slow_clients.pop().await;
            c.check_queue_size().await;
        }
    });

    // blocks until something calls stop()
    let res = ring.run();

    drop(acceptor);
    drop(police);
    drop(bring_up);
    drop(configure_pump);
    state.clear();
    drop(cpu);
    engine.clear();
    res?;
    Ok(())
}
