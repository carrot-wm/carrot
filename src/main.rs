// module layout carved up front; empty mods are intentional.

// links origin (startup) + c-gull (libc) so std runs with zero linked C
extern crate eyra;

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
mod xparsnip;
mod config;
mod dbus;
mod ei;
mod input;
mod ipc;
mod pipewire;
mod sighand;
mod tree;
mod xwayland;

/// mirror stderr into /tmp/carrot-last.log so a wedged session still
/// leaves its story behind; the tty keeps getting everything
fn tee_stderr() {
    use std::io::{Read, Write};
    use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
    let Ok(file) = std::fs::File::create("/tmp/carrot-last.log") else {
        return;
    };
    let tty = unsafe { BorrowedFd::borrow_raw(2) };
    let Ok(tty) = rustix::io::fcntl_dupfd_cloexec(tty, 3) else {
        return;
    };
    let Ok((r, w)) = rustix::pipe::pipe() else {
        return;
    };
    {
        let mut two = unsafe { OwnedFd::from_raw_fd(2) };
        if rustix::io::dup2(&w, &mut two).is_err() {
            let _ = two.into_raw_fd();
            return;
        }
        std::mem::forget(two);
    }
    drop(w);
    let mut tty = std::fs::File::from(tty);
    let mut src = std::fs::File::from(r);
    let mut log = file;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let _ = tty.write_all(&buf[..n]);
                    let _ = log.write_all(&buf[..n]);
                    let _ = log.flush();
                }
            }
        }
    });
}

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
    if std::env::args().any(|a| a == "xparsnip-probe") {
        std::process::exit(xparsnip::probe());
    }
    if std::env::args().any(|a| a == "pw-probe") {
        std::process::exit(pipewire::probe());
    }
    if std::env::args().any(|a| a == "pw-pattern") {
        std::process::exit(pipewire::pattern());
    }

    if let Err(e) = run() {
        eprintln!("carrot: fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // mask first: every thread spawned later inherits it, so int/term can
    // only ever land on the signalfd
    let sig_fd = sighand::install()?;
    tee_stderr();
    // which binary is actually running; kills every "is the fix live" doubt
    if let Ok(md) = std::fs::metadata("/proc/self/exe") {
        if let Ok(t) = md.modified() {
            eprintln!(
                "carrot: {} built {:?}",
                env!("CARGO_PKG_VERSION"),
                t
            );
        }
    }
    let engine = engine::Engine::new();
    let ring = uring::Ring::new(&engine, 32)?;
    let wheel = engine::Wheel::new(&engine, &ring)?;
    // bind before CpuWorker spawns its thread: the socket's set_var of
    // WAYLAND_DISPLAY is only sound single-threaded
    let sock = socket::WaylandSocket::new()?;
    println!("listening on {}", sock.name);
    // input injection is optional; the session runs fine without it
    let ei_sock = match ei::bind_socket() {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("carrot: ei: {e}");
            None
        }
    };
    let cpu = cpu_worker::CpuWorker::new(&engine, &ring)?;
    let state = state::State::new(&engine, &ring, wheel);
    let sig_task = sighand::run(&state, sig_fd);
    state.globals.add(std::rc::Rc::new(surface::WlCompositorGlobal));
    state.globals.add(std::rc::Rc::new(surface::WlSubcompositorGlobal));
    state.globals.add(std::rc::Rc::new(protocol::shm::WlShmGlobal));
    state.globals.add(std::rc::Rc::new(protocol::dmabuf::DmabufGlobal));
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
    state
        .globals
        .add(std::rc::Rc::new(shell::layer::LayerShellGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::output::XdgOutputManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(xwayland::XwaylandShellGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::tearing::TearingManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::relative_pointer::RelativePointerManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::screencopy::ScreencopyManagerGlobal));
    state.globals.add(std::rc::Rc::new(
        protocol::pointer_constraints::PointerConstraintsGlobal,
    ));
    state
        .globals
        .add(std::rc::Rc::new(protocol::data_control::DataControlManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::foreign_toplevel::ForeignToplevelGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::foreign_toplevel_list::ForeignToplevelListGlobal));
    // the source managers and the copy-capture manager ship together:
    // clients gate on the source manager and then use the consumer blind
    state
        .globals
        .add(std::rc::Rc::new(protocol::image_copy_capture::OutputSourceManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::image_copy_capture::ToplevelSourceManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::image_copy_capture::IccManagerGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::idle::IdleNotifierGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::idle::IdleInhibitGlobal));
    state
        .globals
        .add(std::rc::Rc::new(protocol::session_lock::SessionLockManagerGlobal));
    let st = state.clone();
    let _idle_pump = engine.spawn("idle pump", protocol::idle::pump(st));
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
    // config before anything can consume it; a broken file is fatal here
    // and only here - reloads reject instead
    match config::load() {
        Ok(c) => *state.config.borrow_mut() = std::rc::Rc::new(c),
        Err(e) => {
            eprintln!("carrot: config: {e}");
            state.clear();
            engine.clear();
            return Err(e.into());
        }
    }
    if let Some(seat) = state.seat.borrow().clone() {
        seat.apply_input_config(&state);
    }
    let ipc = match ipc::start(&state, &sock.name) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("carrot: {e}");
            state.clear();
            engine.clear();
            return Err(e.into());
        }
    };
    let ei = ei_sock.map(|s| ei::start(&state, s));
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
        if let Some(s) = &session {
            *st.input.borrow_mut() = Some(input::start(&st, s).await);
        }
        *st.session.borrow_mut() = session;
        *st.display.borrow_mut() = display;
        output::start_hotplug(&st);
    });

    let listen_fd = std::rc::Rc::new(sock.fd.try_clone()?);
    let st = state.clone();
    let acceptor = engine.spawn("acceptor", async move {
        let _sock = sock;
        loop {
            match st.ring.accept(&listen_fd).await {
                Ok(fd) => {
                    st.clients.spawn(&st, fd);
                }
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
    let xwayland_task = engine.spawn("xwayland", async move {
        xwayland::run(st).await;
    });
    let st = state.clone();
    let _portal_env = engine.spawn(
        "portal env",
        dbus::export_session_env(engine.clone(), ring.clone(), st),
    );
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
    drop(xwayland_task);
    drop(sig_task);
    drop(ipc);
    drop(ei);
    state.clear();
    drop(cpu);
    engine.clear();
    res?;
    Ok(())
}
