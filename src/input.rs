// pure evdev + kbvm. fds come from logind TakeDevice, we never EVIOCGRAB.
// one set_focus path; everything that changes focus goes through it.
// keyboard, pointer, wheel now; gestures, touch, tablets later.

pub mod cursor_theme;
pub mod default_keymap;
pub mod evdev;
pub mod focus;
pub mod keymap;
pub mod seat;

/// dev diagnostic (`carrot input-probe`): stream decoded events for a few
/// seconds. console is dark under TakeControl, so output also goes to the log.
pub fn probe() -> i32 {
    use crate::dbus::LogindSession;
    use crate::engine::{Engine, Wheel};
    use crate::state::State;
    use crate::util::Time;
    use crate::uring::Ring;
    use std::cell::Cell;
    use std::io::Write;
    use std::rc::Rc;

    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let wheel = match Wheel::new(&engine, &ring) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("wheel: {e}");
            return 1;
        }
    };
    let state = State::new(&engine, &ring, wheel);
    let status = Rc::new(Cell::new(1));
    let st = status.clone();
    let s = state.clone();
    let rng = ring.clone();
    let task = engine.spawn("input probe", async move {
        let Ok(mut log) = std::fs::File::create("/tmp/carrot-input-probe.log") else {
            eprintln!("cannot create /tmp/carrot-input-probe.log");
            rng.stop();
            return;
        };
        let session = match LogindSession::take_control(&s.eng, &s.ring).await {
            Ok(sess) => sess,
            Err(e) => {
                eprintln!("FAIL: session: {e}");
                rng.stop();
                return;
            }
        };
        let mgr = evdev::Manager::start(&s, &session).await;
        for d in mgr.devices.borrow().iter() {
            let line = format!("device {} {:?} active={} \"{}\"", d.devnum, d.kind, d.active.get(), d.name);
            println!("{line}");
            let _ = writeln!(log, "{line}");
        }
        println!("reading events for 6s - type and wiggle; log: /tmp/carrot-input-probe.log");
        let m = mgr.clone();
        let mut log2 = log.try_clone().expect("clone log fd");
        let drain = s.eng.spawn("probe drain", async move {
            loop {
                let (dev, ev) = m.sink.pop().await;
                let _ = writeln!(log2, "{dev} {ev:?}");
            }
        });
        let deadline = Time::from_nsec(Time::now().nsec() + 6_000_000_000);
        let _ = s.ring.timeout(deadline).await;
        drop(drain);
        let _ = writeln!(log, "PASS");
        println!("PASS");
        st.set(0);
        session.clear();
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    state.clear();
    engine.clear();
    status.get()
}

use crate::dbus::LogindSession;
use crate::engine::SpawnedFuture;
use crate::state::State;
use std::rc::Rc;

pub struct InputStack {
    pub mgr: Rc<evdev::Manager>,
    _consumer: SpawnedFuture<()>,
    _repeater: Option<SpawnedFuture<()>>,
}

pub async fn start(state: &Rc<State>, session: &Rc<LogindSession>) -> InputStack {
    let mgr = evdev::Manager::start(state, session).await;
    let m = mgr.clone();
    let s = session.clone();
    let st = state.clone();
    let consumer = state.eng.spawn("input consumer", async move {
        route_events(st, m, s).await;
    });
    let repeater = state.seat.borrow().clone().map(|seat| {
        let st = state.clone();
        state
            .eng
            .spawn("key repeat", async move { seat.repeat_loop(st).await })
    });
    InputStack {
        mgr,
        _consumer: consumer,
        _repeater: repeater,
    }
}

/// per-device knobs, re-resolved per event so config reloads apply live.
/// deltas normalize to a 1000dpi baseline so accel-speed means the same
/// on any device: dpi comes from udev's hwdb, the config key only
/// overrides it. accel scales the cursor, not the raw relative deltas.
fn device_factors(state: &Rc<State>, mgr: &evdev::Manager, devnum: u64) -> (f64, f64, bool, bool) {
    let cfg = state.config.borrow().clone();
    let mut speed = cfg.input.mouse.accel_speed.unwrap_or(0.0);
    let mut dpi = 1000.0f64;
    let mut natural = cfg.input.mouse.natural_scroll;
    let mut adaptive = cfg.input.mouse.accel_profile.as_deref() == Some("adaptive");
    if let Some(dev) = mgr.devices.borrow().iter().find(|d| d.devnum == devnum) {
        dpi = dev.dpi;
        let name: String = dev
            .name
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c == ' ' || c == '_' { '-' } else { c })
            .collect();
        for r in cfg.input.devices.iter() {
            if !r.name.is_empty() && name.contains(&r.name) {
                if let Some(s) = r.accel_speed {
                    speed = s;
                }
                if let Some(d) = r.dpi {
                    if d > 0.0 {
                        dpi = d;
                    }
                }
                if let Some(n) = r.natural_scroll {
                    natural = n;
                }
                if let Some(p) = &r.accel_profile {
                    adaptive = p == "adaptive";
                }
            }
        }
    }
    let scale = 1000.0 / dpi.max(1.0);
    (speed.clamp(-1.0, 1.0), scale, natural, adaptive)
}

/// flat is a constant gain from accel-speed. adaptive shapes it: a
/// precision zone below unity when slow, gain rising with speed to a cap;
/// v is 1000dpi-normalized counts per ms
fn accel_factor(speed: f64, adaptive: bool, v: f64) -> f64 {
    let base = 1.0 + speed;
    if !adaptive {
        return base;
    }
    let t = (v / 4.0).min(1.0);
    base * (0.6 + 1.4 * t)
}

/// device events into the seat; vt switch comes back out as an action
async fn route_events(state: Rc<State>, mgr: Rc<evdev::Manager>, session: Rc<LogindSession>) {
    // last motion time per device, for the adaptive velocity estimate
    let mut last_motion: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    loop {
        let (devnum, ev) = mgr.sink.pop().await;
        let Some(seat) = state.seat.borrow().clone() else {
            continue;
        };
        match ev {
            InputEvent::Key {
                time_usec,
                key,
                pressed,
            } => {
                seat.ensure_focus(&state);
                match seat.key(&state, time_usec, key, pressed) {
                    seat::KeyAction::SwitchVt(vt) => {
                        // switch-to-self tears the cursor down with no resume
                        if vt == session.vtnr {
                            continue;
                        }
                        // clear cursor plane while we still hold drm master
                        if let Some(d) = state.display.borrow().as_ref() {
                            d.prepare_vt_switch(&state, vt);
                        }
                        session.switch_vt(vt);
                    }
                    seat::KeyAction::Act(action) => {
                        crate::ipc::dispatch_action(&state, &action);
                    }
                    seat::KeyAction::Handled => {}
                }
            }
            InputEvent::Motion { time_usec, dx, dy } => {
                let (speed, scale, _, adaptive) = device_factors(&state, &mgr, devnum);
                let (udx, udy) = (dx * scale, dy * scale);
                let dt_ms = last_motion
                    .insert(devnum, time_usec)
                    .map(|prev| (time_usec.saturating_sub(prev) as f64 / 1000.0).max(0.1))
                    .unwrap_or(f64::INFINITY);
                let accel = accel_factor(speed, adaptive, udx.hypot(udy) / dt_ms);
                seat.pointer_motion(&state, time_usec, udx * accel, udy * accel, udx, udy);
                if let Some(d) = state.display.borrow().as_ref() {
                    d.move_cursor(&state, seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
                }
            }
            InputEvent::Button {
                time_usec,
                button,
                pressed,
            } => seat.pointer_button(&state, time_usec, button, pressed),
            InputEvent::Axis120 {
                time_usec,
                horizontal,
                dist,
            } => {
                let (_, _, natural, _) = device_factors(&state, &mgr, devnum);
                let dist = if natural { -dist } else { dist };
                seat.pointer_axis(time_usec, horizontal, dist)
            }
            InputEvent::Frame { .. } => seat.pointer_frame(),
        }
    }
}

/// seam between device layer and seat: decoded, batched, deduped. usec until the wire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Key {
        time_usec: u64,
        key: u32,
        /// kernel autorepeat (value 2) never reaches here
        pressed: bool,
    },
    Motion {
        time_usec: u64,
        dx: f64,
        dy: f64,
    },
    Button {
        time_usec: u64,
        button: u32,
        pressed: bool,
    },
    /// detents in 1/120 units, sign symmetric across axes
    Axis120 {
        time_usec: u64,
        horizontal: bool,
        dist: i32,
    },
    Frame {
        time_usec: u64,
    },
}
