// evdev devices: enumerate via logind, classify by capability bits,
// decode 24-byte events, batch on SYN_REPORT. keyboards and pointers only.

use super::InputEvent;
use crate::dbus::{DeviceEvent, LogindSession};
use crate::state::State;
use crate::util::AsyncQueue;
use rustix::io::Errno;
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, ioctl, opcode};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::c_void;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::rc::Rc;

// -- event codes (input-event-codes.h) --

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const KEY_ESC: u16 = 1;
const BTN_MOUSE: u16 = 0x110;

/// a drained edge releases as what it was: buttons stay buttons
fn synth_release(key: u32) -> InputEvent {
    if key >= BTN_MOUSE as u32 {
        InputEvent::Button { time_usec: 0, button: key, pressed: false }
    } else {
        InputEvent::Key { time_usec: 0, key, pressed: false }
    }
}
const BTN_LEFT: u16 = 0x110;
const BTN_JOYSTICK: u16 = 0x120;
const KEY_MAX: usize = 0x2ff;

// -- ioctls --

struct EvIoctl<'a, T: ?Sized> {
    opcode: Opcode,
    data: &'a mut T,
}

unsafe impl<T: ?Sized> Ioctl for EvIoctl<'_, T> {
    type Output = ();
    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        self.opcode
    }

    fn as_ptr(&mut self) -> *mut c_void {
        (self.data as *mut T).cast()
    }

    unsafe fn output_from_ptr(_: IoctlOutput, _: *mut c_void) -> rustix::io::Result<()> {
        Ok(())
    }
}

fn ev_read<T>(fd: BorrowedFd<'_>, nr: u8, data: &mut T) -> Result<(), Errno> {
    let op = opcode::read::<T>(b'E', nr);
    unsafe { ioctl(fd, EvIoctl { opcode: op, data }) }
}

/// EVIOCSCLOCKID: event timestamps on the monotonic clock, the base every
/// synthetic event and presentation time already uses. the default is
/// CLOCK_REALTIME, which steps under ntp
pub fn set_monotonic(fd: BorrowedFd<'_>) {
    let mut clock: i32 = 1; // CLOCK_MONOTONIC
    let op = opcode::write::<i32>(b'E', 0xa0);
    if let Err(e) = unsafe { ioctl(fd, EvIoctl { opcode: op, data: &mut clock }) } {
        crate::trace!("EVIOCSCLOCKID failed: {e}");
    }
}

pub fn name(fd: BorrowedFd<'_>) -> String {
    let mut buf = [0u8; 256];
    // EVIOCGNAME
    if ev_read(fd, 0x06, &mut buf).is_err() {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn bits<const N: usize>(fd: BorrowedFd<'_>, ev: u16) -> [u8; N] {
    let mut buf = [0u8; N];
    // EVIOCGBIT(ev, len)
    let _ = ev_read(fd, 0x20 + ev as u8, &mut buf);
    buf
}

fn bit(buf: &[u8], idx: u16) -> bool {
    let (byte, bit) = (idx as usize / 8, idx as usize % 8);
    byte < buf.len() && buf[byte] & (1 << bit) != 0
}

/// kernel's view of held keys; vt-resume reads this so mid-switch keys still release
pub fn held_keys(fd: BorrowedFd<'_>) -> Vec<u32> {
    let mut buf = [0u8; KEY_MAX / 8 + 1];
    // EVIOCGKEY
    if ev_read(fd, 0x18, &mut buf).is_err() {
        return Vec::new();
    }
    let mut held = Vec::new();
    for code in 0..=KEY_MAX as u16 {
        if bit(&buf, code) {
            held.push(code as u32);
        }
    }
    held
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    Keyboard,
    Pointer,
    /// both halves of a combo device
    Both,
}

/// keyboard = regular keys, no button; pointer = button + relative axes
pub fn classify(fd: BorrowedFd<'_>) -> Option<DeviceKind> {
    let ev = bits::<4>(fd, 0);
    if !bit(&ev, EV_KEY) {
        return None;
    }
    let keys = bits::<{ KEY_MAX / 8 + 1 }>(fd, EV_KEY);
    let has_regular = (KEY_ESC..BTN_MOUSE).any(|k| bit(&keys, k));
    let has_button = (BTN_LEFT..BTN_JOYSTICK).any(|k| bit(&keys, k));
    let rel = bits::<2>(fd, EV_REL);
    let has_rel = bit(&ev, EV_REL) && (bit(&rel, REL_X) || bit(&rel, REL_Y));
    let kb = has_regular && !has_button;
    let ptr = has_button && has_rel;
    match (kb, ptr) {
        (true, true) => Some(DeviceKind::Both),
        (true, false) => Some(DeviceKind::Keyboard),
        (false, true) => Some(DeviceKind::Pointer),
        (false, false) => None,
    }
}

// -- devices --

pub struct Device {
    pub devnum: u64,
    pub name: String,
    pub kind: DeviceKind,
    pub fd: RefCell<Rc<OwnedFd>>,
    pub active: Cell<bool>,
    /// edge dedup + release synthesis on pause/removal
    pub pressed: RefCell<HashSet<u32>>,
    /// queue overruns announce once per device, not per drop
    drop_warned: Cell<bool>,
    /// from udev's MOUSE_DPI (hwdb), like libinput; 1000 when unlisted
    pub dpi: f64,
    reader: Cell<Option<crate::engine::SpawnedFuture<()>>>,
}

/// hwdb MOUSE_DPI, resolved by udevd at device-add into /run/udev/data (the
/// store libudev reads). no entry -> libinput's 1000dpi default.
fn udev_mouse_dpi(devnum: u64) -> f64 {
    let path = format!(
        "/run/udev/data/c{}:{}",
        rustix::fs::major(devnum),
        rustix::fs::minor(devnum)
    );
    let Ok(data) = std::fs::read_to_string(&path) else {
        return 1000.0;
    };
    for line in data.lines() {
        let Some(value) = line.strip_prefix("E:MOUSE_DPI=") else {
            continue;
        };
        return parse_mouse_dpi(value);
    }
    1000.0
}

/// "800@125", "1000", or a list "400@125 *800@125 1600@125" where the star
/// marks the device default
fn parse_mouse_dpi(value: &str) -> f64 {
    let mut first = None;
    for entry in value.split_whitespace() {
        let (starred, entry) = match entry.strip_prefix('*') {
            Some(e) => (true, e),
            None => (false, entry),
        };
        let dpi: f64 = entry
            .split('@')
            .next()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0.0);
        if dpi <= 0.0 {
            continue;
        }
        if starred {
            return dpi;
        }
        first.get_or_insert(dpi);
    }
    first.unwrap_or(1000.0)
}

const EVENT_SIZE: usize = 24;

pub struct Manager {
    pub devices: RefCell<Vec<Rc<Device>>>,
    pub sink: Rc<AsyncQueue<(u64, InputEvent)>>,
}

impl Manager {
    pub async fn start(state: &Rc<State>, session: &Rc<LogindSession>) -> Rc<Manager> {
        let mgr = Rc::new(Manager {
            devices: RefCell::new(Vec::new()),
            sink: Rc::new(AsyncQueue::default()),
        });
        let mut nodes: Vec<_> = match std::fs::read_dir("/dev/input") {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("event"))
                        .unwrap_or(false)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        nodes.sort();
        for node in nodes {
            mgr.add_device(state, session, &node).await;
        }
        mgr
    }

    /// bring one node under management; devnums already present are skipped
    pub async fn add_device(
        self: &Rc<Self>,
        state: &Rc<State>,
        session: &Rc<LogindSession>,
        node: &std::path::Path,
    ) -> Option<Rc<Device>> {
        let devnum = rustix::fs::stat(node).ok()?.st_rdev;
        if self.devices.borrow().iter().any(|d| d.devnum == devnum) {
            return None;
        }
        let (fd, inactive) = match session.take_device(devnum).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("carrot: {}: TakeDevice: {e}", node.display());
                return None;
            }
        };
        // a duplicate uevent may have raced us across the await
        if self.devices.borrow().iter().any(|d| d.devnum == devnum) {
            return None;
        }
        let Some(kind) = classify(fd.as_fd()) else {
            session.release_device(devnum);
            return None;
        };
        set_monotonic(fd.as_fd());
        let dev = Rc::new(Device {
            devnum,
            name: name(fd.as_fd()),
            kind,
            fd: RefCell::new(fd),
            active: Cell::new(!inactive),
            pressed: RefCell::new(HashSet::new()),
            drop_warned: Cell::new(false),
            dpi: udev_mouse_dpi(devnum),
            reader: Cell::new(None),
        });
        if dev.active.get() {
            dev.spawn_reader(state, &self.sink);
        }
        let d = dev.clone();
        let st2 = state.clone();
        let sink = self.sink.clone();
        let mgr = Rc::downgrade(self);
        let sess = Rc::downgrade(session);
        session.on_device(
            devnum,
            Rc::new(move |ev| match ev {
                DeviceEvent::Pause { .. } => {
                    d.active.set(false);
                    d.reader.take();
                    // clients must not see keys stuck across the vt
                    let held: Vec<u32> = d.pressed.borrow_mut().drain().collect();
                    for key in held {
                        sink.push((d.devnum, synth_release(key)));
                    }
                }
                DeviceEvent::Gone { .. } => {
                    if let (Some(m), Some(s)) = (mgr.upgrade(), sess.upgrade()) {
                        m.remove_device(&s, d.devnum);
                    }
                }
                DeviceEvent::Resume { fd, .. } => {
                    set_monotonic(fd.as_fd());
                    *d.fd.borrow_mut() = fd;
                    d.active.set(true);
                    // keys pressed while away: kernel bitmask is truth, seed so releases route
                    let held = held_keys(d.fd.borrow().as_fd());
                    *d.pressed.borrow_mut() = held.into_iter().collect();
                    d.spawn_reader(&st2, &sink);
                }
            }),
        );
        crate::trace!("input device {}: {} ({:?})", dev.devnum, dev.name, dev.kind);
        self.devices.borrow_mut().push(dev.clone());
        Some(dev)
    }

    /// unlist the device, stop its reader, and release held keys
    fn detach_device(&self, devnum: u64) -> Option<Rc<Device>> {
        let dev = {
            let mut devs = self.devices.borrow_mut();
            let pos = devs.iter().position(|d| d.devnum == devnum)?;
            devs.remove(pos)
        };
        dev.active.set(false);
        dev.reader.take();
        // removal can beat the pause that would have synthesized these
        let held: Vec<u32> = dev.pressed.borrow_mut().drain().collect();
        for key in held {
            self.sink.push((devnum, synth_release(key)));
        }
        Some(dev)
    }

    /// full teardown: reader, stuck keys, logind handler and lease
    pub fn remove_device(&self, session: &LogindSession, devnum: u64) {
        let Some(dev) = self.detach_device(devnum) else {
            return;
        };
        session.forget_device(devnum);
        session.release_device(devnum);
        println!("carrot: input: {} removed", dev.name);
    }
}

use std::os::fd::AsFd;

impl Device {
    fn spawn_reader(self: &Rc<Self>, state: &Rc<State>, sink: &Rc<AsyncQueue<(u64, InputEvent)>>) {
        let dev = self.clone();
        let sink = sink.clone();
        let ring = state.ring.clone();
        self.reader.set(Some(state.eng.spawn("evdev reader", async move {
            let mut buf = vec![0u8; EVENT_SIZE * 64];
            // pending relative state, flushed on SYN_REPORT
            let mut dx = 0f64;
            let mut dy = 0f64;
            let mut wheel_v = 0i32;
            let mut wheel_h = 0i32;
            let mut hires_v = false;
            let mut hires_h = false;
            loop {
                let fd = dev.fd.borrow().clone();
                let (b, n) = match ring.read(&fd, buf).await {
                    Ok(r) => r,
                    Err(_) => {
                        // revoked or gone; pause/resume handles state
                        return;
                    }
                };
                buf = b;
                if n == 0 {
                    return;
                }
                for ev in buf[..n].chunks_exact(EVENT_SIZE) {
                    let sec = i64::from_ne_bytes(ev[0..8].try_into().unwrap());
                    let usec = i64::from_ne_bytes(ev[8..16].try_into().unwrap());
                    let time_usec = (sec * 1_000_000 + usec) as u64;
                    let ty = u16::from_ne_bytes(ev[16..18].try_into().unwrap());
                    let code = u16::from_ne_bytes(ev[18..20].try_into().unwrap());
                    let value = i32::from_ne_bytes(ev[20..24].try_into().unwrap());
                    match ty {
                        EV_KEY => {
                            // 2 = kernel autorepeat; ours is server-side
                            if value == 2 {
                                continue;
                            }
                            let pressed = value == 1;
                            let key = code as u32;
                            // dedup edges
                            let mut held = dev.pressed.borrow_mut();
                            if pressed && !held.insert(key) {
                                continue;
                            }
                            if !pressed && !held.remove(&key) {
                                continue;
                            }
                            drop(held);
                            let event = if code >= BTN_MOUSE {
                                InputEvent::Button {
                                    time_usec,
                                    button: key,
                                    pressed,
                                }
                            } else {
                                InputEvent::Key {
                                    time_usec,
                                    key,
                                    pressed,
                                }
                            };
                            sink.push((dev.devnum, event));
                        }
                        EV_REL => match code {
                            REL_X => dx += value as f64,
                            REL_Y => dy += value as f64,
                            REL_WHEEL_HI_RES => {
                                wheel_v += value;
                                hires_v = true;
                            }
                            REL_HWHEEL_HI_RES => {
                                wheel_h += value;
                                hires_h = true;
                            }
                            // legacy detents count only without a hi-res twin this frame
                            REL_WHEEL => {
                                if !hires_v {
                                    wheel_v += value * 120;
                                }
                            }
                            REL_HWHEEL => {
                                if !hires_h {
                                    wheel_h += value * 120;
                                }
                            }
                            _ => {}
                        },
                        // the kernel queue overran and events were lost; the
                        // pending rel state is garbage, start the batch over
                        EV_SYN if code == SYN_DROPPED => {
                            if !dev.drop_warned.replace(true) {
                                eprintln!(
                                    "carrot: input: {} overran the kernel queue, events dropped",
                                    dev.name
                                );
                            }
                            dx = 0.0;
                            dy = 0.0;
                            wheel_v = 0;
                            wheel_h = 0;
                        }
                        EV_SYN if code == SYN_REPORT => {
                            let mut any = false;
                            if dx != 0.0 || dy != 0.0 {
                                sink.push((dev.devnum, InputEvent::Motion { time_usec, dx, dy }));
                                dx = 0.0;
                                dy = 0.0;
                                any = true;
                            }
                            if wheel_v != 0 {
                                sink.push((
                                    dev.devnum,
                                    InputEvent::Axis120 {
                                        time_usec,
                                        horizontal: false,
                                        dist: wheel_v,
                                    },
                                ));
                                wheel_v = 0;
                                any = true;
                            }
                            if wheel_h != 0 {
                                sink.push((
                                    dev.devnum,
                                    InputEvent::Axis120 {
                                        time_usec,
                                        horizontal: true,
                                        dist: wheel_h,
                                    },
                                ));
                                wheel_h = 0;
                                any = true;
                            }
                            hires_v = false;
                            hires_h = false;
                            if any {
                                sink.push((dev.devnum, InputEvent::Frame { time_usec }));
                            }
                        }
                        EV_ABS => {
                            // touch territory - stage 3
                        }
                        _ => {}
                    }
                }
            }
        })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn mouse_dpi_parses_like_libinput() {
        assert_eq!(parse_mouse_dpi("800@125"), 800.0);
        assert_eq!(parse_mouse_dpi("1200"), 1200.0);
        assert_eq!(parse_mouse_dpi("400@125 *800@1000 1600@125"), 800.0);
        assert_eq!(parse_mouse_dpi("400@125 1600@125"), 400.0);
        assert_eq!(parse_mouse_dpi("garbage"), 1000.0);
        assert_eq!(parse_mouse_dpi(""), 1000.0);
    }

    fn fake_device(devnum: u64) -> Rc<Device> {
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        Rc::new(Device {
            devnum,
            name: "test kbd".into(),
            kind: DeviceKind::Keyboard,
            fd: RefCell::new(Rc::new(fd)),
            active: Cell::new(true),
            pressed: RefCell::new([30u32].into_iter().collect()),
            drop_warned: Cell::new(false),
            dpi: 1000.0,
            reader: Cell::new(None),
        })
    }

    #[test]
    fn detach_unlists_releases_keys_and_is_idempotent() {
        let mgr = Manager {
            devices: RefCell::new(Vec::new()),
            sink: Rc::new(AsyncQueue::default()),
        };
        let fake = fake_device(42);
        // a held mouse button drains alongside the key
        fake.pressed.borrow_mut().insert(0x110);
        mgr.devices.borrow_mut().push(fake);

        let dev = mgr.detach_device(42).expect("device was listed");
        assert!(mgr.devices.borrow().is_empty());
        assert!(!dev.active.get());
        assert!(dev.pressed.borrow().is_empty());

        // both edges were synthesized as releases of their own kind
        let mut cx = Context::from_waker(Waker::noop());
        let (mut key_up, mut btn_up) = (false, false);
        for _ in 0..2 {
            let mut pop = mgr.sink.pop();
            let Poll::Ready((devnum, ev)) = std::pin::Pin::new(&mut pop).poll(&mut cx) else {
                panic!("missing a synthesized release in the sink");
            };
            assert_eq!(devnum, 42);
            match ev {
                InputEvent::Key { key: 30, pressed: false, .. } => key_up = true,
                InputEvent::Button { button: 0x110, pressed: false, .. } => btn_up = true,
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert!(key_up && btn_up, "key released as key, button as button");

        // second detach is a no-op
        assert!(mgr.detach_device(42).is_none());
    }
}
