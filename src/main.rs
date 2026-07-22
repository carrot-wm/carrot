// module layout carved up front; empty mods are intentional.

// the render loader donates a const-init thread_local! surplus block to drivers

// links origin (startup) + c-gull (libc) so std runs with zero linked C
extern crate eyra;

// core runtime
mod anim;
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
mod install;
mod ipc;
mod pipewire;
mod portal;
mod doctor;
mod sighand;
mod tree;
mod xwayland;

/// the recent stderr tail, kept in memory for the crash report
const CRASH_TAIL: usize = 256 * 1024;
static CRASH_BUF: std::sync::Mutex<std::collections::VecDeque<u8>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// mirror stderr into the in-memory tail so a crash can still tell its
/// story; the tty keeps getting everything
fn tee_stderr() {
    use std::io::{Read, Write};
    use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
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
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let _ = tty.write_all(&buf[..n]);
                    let mut q = CRASH_BUF.lock().unwrap_or_else(|e| e.into_inner());
                    q.extend(&buf[..n]);
                    let over = q.len().saturating_sub(CRASH_TAIL);
                    if over > 0 {
                        q.drain(..over);
                    }
                }
            }
        }
    });
}

/// $XDG_CACHE_HOME/carrot, else ~/.cache/carrot
fn crash_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
        })?;
    Some(base.join("carrot"))
}

/// one past the highest <prefix><n>.log already there
fn next_report_number(dir: &std::path::Path, prefix: &str) -> u64 {
    let mut top = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(n) = name
                .strip_prefix(prefix)
                .and_then(|r| r.strip_suffix(".log"))
                .and_then(|r| r.parse::<u64>().ok())
            {
                top = top.max(n);
            }
        }
    }
    top + 1
}

/// every crash gets its own numbered file; nothing ever overwrites
fn write_crash_log(info: &std::panic::PanicHookInfo) -> Option<std::path::PathBuf> {
    write_crash_report(&crash_dir()?, info)
}

fn write_crash_report(
    dir: &std::path::Path,
    info: &dyn std::fmt::Display,
) -> Option<std::path::PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir).ok()?;
    let mut n = next_report_number(dir, "carrotCrashLog");
    loop {
        let path = dir.join(format!("carrotCrashLog{n}.log"));
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
                continue;
            }
            Err(_) => return None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "carrot {} crashed at unix {now}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(f, "{info}");
        // the message is on disk before the riskier captures run
        let _ = f.flush();
        let _ = writeln!(f, "{}", std::backtrace::Backtrace::force_capture());
        let _ = writeln!(f, "-- stderr tail --");
        // try_lock: a panic on the tee thread itself must not deadlock here
        if let Ok(q) = CRASH_BUF.try_lock() {
            let (a, b) = q.as_slices();
            let _ = f.write_all(a);
            let _ = f.write_all(b);
        } else {
            let _ = writeln!(f, "(tail unavailable)");
        }
        return Some(path);
    }
}

/// panic=abort still runs the hook: the report lands before the process dies
fn install_crash_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("carrot: {info}");
        match write_crash_log(info) {
            Some(p) => eprintln!("carrot: crash report: {}", p.display()),
            None => eprintln!("carrot: crash report could not be written"),
        }
    }));
}

fn main() {
    install_crash_hook();
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("carrot {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args().any(|a| a == "spike-scanout") {
        std::process::exit(spike::run());
    }
    if std::env::args().any(|a| a == "doctor") {
        std::process::exit(doctor::run());
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
    if std::env::args().any(|a| a == "portal-probe") {
        std::process::exit(portal::probe());
    }
    if let Some(i) = std::env::args().position(|a| a == "check-config") {
        let path = std::env::args().nth(i + 1);
        std::process::exit(config::check(path.as_deref()));
    }
    if let Some(i) = std::env::args().position(|a| a == "install") {
        let rest: Vec<String> = std::env::args().skip(i + 1).collect();
        std::process::exit(install::run(&rest));
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
    // same window: DISPLAY must be exported here, before any thread exists
    // and before run_spawn hands the environment to startup children
    let xclaim = xwayland::claim();
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
    state.globals.add(std::rc::Rc::new(protocol::presentation::PresentationGlobal));
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
        .add(std::rc::Rc::new(protocol::xdg_activation::ActivationGlobal));
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
    state
        .globals
        .add(std::rc::Rc::new(protocol::ext_workspace::ExtWorkspaceGlobal));
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
    // a broken file never strands the session: the embedded default takes
    // over and every error is printed + kept for ipc subscribers
    match config::load() {
        config::Loaded::Ok(c) | config::Loaded::FirstRun(c) => {
            *state.config.borrow_mut() = std::rc::Rc::new(c);
        }
        config::Loaded::Fallback { errors } => {
            *state.config.borrow_mut() = std::rc::Rc::new(config::Config::default());
            let ev = serde_json::json!({ "event": "config-loaded", "failed": true,
                                         "errors": errors, "cold-keys-pending": [] });
            *state.last_config_event.borrow_mut() = Some(ev.to_string());
        }
    }
    {
        let a = &state.config.borrow().animations;
        state.anim_clock.set_global(a.off, a.slowdown);
    }
    for sp in state.config.borrow().spawns.clone().iter() {
        ipc::run_spawn(&state, sp);
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
    let xwayland_task = xclaim.map(|claim| {
        let st = state.clone();
        engine.spawn("xwayland", async move {
            xwayland::run(st, claim).await;
        })
    });
    let st = state.clone();
    let _portal_env = engine.spawn(
        "portal env",
        dbus::export_session_env(engine.clone(), ring.clone(), st),
    );
    let st = state.clone();
    let _portal = engine.spawn("portal", portal::run(engine.clone(), ring.clone(), st));
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

#[cfg(test)]
mod tests {
    #[test]
    fn crash_logs_number_past_the_highest() {
        let dir = std::env::temp_dir().join(format!("carrot-crashnum-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(super::next_report_number(&dir, "carrotCrashLog"), 1, "empty dir starts at 1");
        std::fs::write(dir.join("carrotCrashLog1.log"), b"x").unwrap();
        std::fs::write(dir.join("carrotCrashLog7.log"), b"x").unwrap();
        std::fs::write(dir.join("carrotCrashLognope.log"), b"x").unwrap();
        std::fs::write(dir.join("unrelated.log"), b"x").unwrap();
        assert_eq!(super::next_report_number(&dir, "carrotCrashLog"), 8, "counts past the highest");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn crash_reports_land_numbered_and_complete() {
        let dir = std::env::temp_dir().join(format!("carrot-crashrep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p1 = super::write_crash_report(&dir, &"boom one").unwrap();
        let p2 = super::write_crash_report(&dir, &"boom two").unwrap();
        assert!(p1.ends_with("carrotCrashLog1.log"));
        assert!(p2.ends_with("carrotCrashLog2.log"), "the first report survives");
        let body = std::fs::read_to_string(&p1).unwrap();
        assert!(body.contains("boom one"));
        assert!(body.contains("crashed at unix"));
        assert!(body.contains("-- stderr tail --"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
