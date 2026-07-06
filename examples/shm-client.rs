// minimal shm client for bring-up: one gradient xdg_toplevel at the configured
// size, then it holds the connection open and acks further configures.
// run: WAYLAND_DISPLAY=wayland-2 cargo run --example shm-client

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::net::{
    AddressFamily, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix,
    SocketType, connect, send, sendmsg, socket,
};
use std::io::{IoSlice, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;

fn msg(out: &mut Vec<u8>, obj: u32, opcode: u32, args: &[u32]) {
    let len = 8 + args.len() * 4;
    out.extend_from_slice(&obj.to_ne_bytes());
    out.extend_from_slice(&(((len as u32) << 16) | opcode).to_ne_bytes());
    for a in args {
        out.extend_from_slice(&a.to_ne_bytes());
    }
}

fn string_args(s: &str) -> Vec<u32> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    let mut args = vec![(s.len() + 1) as u32];
    for chunk in bytes.chunks(4) {
        args.push(u32::from_ne_bytes(chunk.try_into().unwrap()));
    }
    args
}

fn main() {
    let xrd = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
    let disp = std::env::var("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY");
    let path = format!("{xrd}/{disp}");
    // the compositor may still be binding (or a stale socket may linger);
    // retry for a few seconds instead of dying on the race
    let mut stream = None;
    for _ in 0..50 {
        let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        if connect(&fd, &SocketAddrUnix::new(&*path).unwrap()).is_ok() {
            stream = Some(UnixStream::from(OwnedFd::from(fd)));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let Some(mut stream) = stream else {
        eprintln!("could not connect to {path} after 5s");
        std::process::exit(1);
    };

    // ids: 2 registry, 3 sync cb, 4 compositor, 5 shm, 6 surface,
    // 7 pool, 8 buffer
    let mut out = Vec::new();
    msg(&mut out, 1, 1, &[2]); // get_registry
    msg(&mut out, 1, 0, &[3]); // sync
    send(&stream, &out, SendFlags::empty()).unwrap();

    // read events until the sync callback fires, remembering globals
    let (mut comp_name, mut shm_name, mut seat_name, mut wm_name) = (0u32, 0u32, 0u32, 0u32);
    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();
    'wait: loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            panic!("server hung up during setup");
        }
        pending.extend_from_slice(&buf[..n]);
        while pending.len() >= 8 {
            let obj = u32::from_ne_bytes(pending[0..4].try_into().unwrap());
            let hdr = u32::from_ne_bytes(pending[4..8].try_into().unwrap());
            let len = (hdr >> 16) as usize;
            let opcode = hdr & 0xffff;
            if pending.len() < len {
                break;
            }
            let body = &pending[8..len];
            if obj == 2 && opcode == 0 {
                let name = u32::from_ne_bytes(body[0..4].try_into().unwrap());
                let slen = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
                let iface = std::str::from_utf8(&body[8..8 + slen - 1]).unwrap();
                match iface {
                    "wl_compositor" => comp_name = name,
                    "wl_shm" => shm_name = name,
                    "wl_seat" => seat_name = name,
                    "xdg_wm_base" => wm_name = name,
                    _ => {}
                }
            }
            if obj == 3 && opcode == 0 {
                pending.drain(..len);
                break 'wait;
            }
            pending.drain(..len);
        }
    }
    assert!(comp_name != 0 && shm_name != 0 && wm_name != 0, "missing globals");
    println!("globals: wl_compositor={comp_name} wl_shm={shm_name} xdg_wm_base={wm_name}");

    // bind everything, then the xdg dance: surface -> xdg_surface -> toplevel
    // -> bufferless commit, then wait for the initial configure
    let mut out = Vec::new();
    let mut bind = |name: u32, iface: &str, version: u32, id: u32, out: &mut Vec<u8>| {
        let mut args = vec![name];
        args.extend(string_args(iface));
        args.push(version);
        args.push(id);
        msg(out, 2, 0, &args);
    };
    bind(comp_name, "wl_compositor", 4, 4, &mut out);
    bind(shm_name, "wl_shm", 1, 5, &mut out);
    if seat_name != 0 {
        bind(seat_name, "wl_seat", 9, 9, &mut out);
        msg(&mut out, 9, 1, &[10]); // get_keyboard
        msg(&mut out, 9, 0, &[11]); // get_pointer
    }
    bind(wm_name, "xdg_wm_base", 6, 12, &mut out);
    msg(&mut out, 4, 0, &[6]); // create_surface
    msg(&mut out, 12, 2, &[13, 6]); // get_xdg_surface
    msg(&mut out, 13, 1, &[14]); // get_toplevel
    let title = string_args("carrot smoke client");
    msg(&mut out, 14, 2, &title); // set_title
    msg(&mut out, 6, 6, &[]); // commit, no buffer yet
    send(&stream, &out, SendFlags::empty()).unwrap();

    // toplevel configure carries the tile size, xdg_surface the serial to ack
    let (mut cw, mut ch) = (0i32, 0i32);
    let mut serial = 0u32;
    'configured: loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            panic!("server hung up before the initial configure");
        }
        pending.extend_from_slice(&buf[..n]);
        while pending.len() >= 8 {
            let obj = u32::from_ne_bytes(pending[0..4].try_into().unwrap());
            let hdr = u32::from_ne_bytes(pending[4..8].try_into().unwrap());
            let len = (hdr >> 16) as usize;
            if pending.len() < len {
                break;
            }
            let body = &pending[8..len];
            if obj == 14 && hdr & 0xffff == 0 {
                cw = i32::from_ne_bytes(body[0..4].try_into().unwrap());
                ch = i32::from_ne_bytes(body[4..8].try_into().unwrap());
            }
            if obj == 13 && hdr & 0xffff == 0 {
                serial = u32::from_ne_bytes(body[0..4].try_into().unwrap());
                pending.drain(..len);
                break 'configured;
            }
            pending.drain(..len);
        }
    }
    // 0x0 means the client picks
    let (w, h) = (
        if cw > 0 { cw as usize } else { 256 },
        if ch > 0 { ch as usize } else { 256 },
    );
    println!("initial configure: {cw}x{ch} serial={serial} -> buffer {w}x{h}");
    let mut out = Vec::new();
    msg(&mut out, 13, 4, &[serial]); // ack_configure

    // gradient in a sealed memfd at the configured size
    let size = w * h * 4;
    let mem = memfd_create("shm-client", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
    ftruncate(&mem, size as u64).unwrap();
    let mut px = vec![0u8; size];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            px[o] = (x * 255 / w) as u8; // b
            px[o + 1] = (y * 255 / h) as u8; // g
            px[o + 2] = 200; // r
            px[o + 3] = 255;
        }
    }
    rustix::io::pwrite(&mem, &px, 0).unwrap();

    // create_pool carries the fd as SCM_RIGHTS
    msg(&mut out, 5, 0, &[7, size as u32]);
    let fds = [mem.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut anc = SendAncillaryBuffer::new(&mut space);
    anc.push(SendAncillaryMessage::ScmRights(&fds));
    sendmsg(
        &stream,
        &[IoSlice::new(&out)],
        &mut anc,
        SendFlags::empty(),
    )
    .unwrap();

    let mut out = Vec::new();
    msg(
        &mut out,
        7,
        0,
        &[8, 0, w as u32, h as u32, (w * 4) as u32, 1], // format 1 = xrgb8888
    );
    msg(&mut out, 6, 1, &[8, 0, 0]); // attach
    msg(&mut out, 6, 6, &[]); // commit
    send(&stream, &out, SendFlags::empty()).unwrap();
    println!("toplevel committed - it should be tiled on screen now");

    // stay connected; decode input traffic into a log (the console is dark
    // while a compositor holds the vt)
    use std::io::Write;
    let mut log = std::fs::File::create("/tmp/carrot-client.log").ok();
    let mut note = move |line: String| {
        println!("{line}");
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    };
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        while pending.len() >= 8 {
            let obj = u32::from_ne_bytes(pending[0..4].try_into().unwrap());
            let hdr = u32::from_ne_bytes(pending[4..8].try_into().unwrap());
            let len = (hdr >> 16) as usize;
            if len < 8 || pending.len() < len {
                break;
            }
            let body = &pending[8..len];
            let u = |i: usize| u32::from_ne_bytes(body[i * 4..i * 4 + 4].try_into().unwrap());
            if obj == 14 && hdr & 0xffff == 0 {
                note(format!("toplevel configure: {}x{}", u(0) as i32, u(1) as i32));
            }
            if obj == 14 && hdr & 0xffff == 1 {
                note("toplevel close - exiting".into());
                return;
            }
            if obj == 13 && hdr & 0xffff == 0 {
                // ack but keep the old buffer; a real client would resize
                note(format!("configure serial={} - acking", u(0)));
                let mut out = Vec::new();
                msg(&mut out, 13, 4, &[u(0)]);
                let _ = send(&stream, &out, SendFlags::empty());
            }
            if obj == 11 {
                let fx = |i: usize| u(i) as i32 as f64 / 256.0;
                match hdr & 0xffff {
                    0 => note(format!("ptr enter: serial={} surface={} at {},{}", u(0), u(1), fx(2), fx(3))),
                    1 => note(format!("ptr leave: serial={}", u(0))),
                    2 => note(format!("ptr motion: {},{}", fx(1), fx(2))),
                    3 => note(format!(
                        "ptr button: {} {}",
                        u(2),
                        if u(3) == 1 { "pressed" } else { "released" }
                    )),
                    4 => note(format!("ptr axis: axis={} px={}", u(1), fx(2))),
                    5 => note("ptr frame".into()),
                    6 => note(format!("ptr axis_source: {}", u(0))),
                    9 => note(format!("ptr value120: axis={} v={}", u(0), u(1) as i32)),
                    op => note(format!("wl_pointer event {op}")),
                }
            }
            if obj == 10 {
                match hdr & 0xffff {
                    0 => note(format!("keymap: format={} size={}", u(0), u(1))),
                    1 => note(format!("enter: serial={} surface={}", u(0), u(1))),
                    2 => note(format!("leave: serial={}", u(0))),
                    3 => note(format!(
                        "key: key={} state={}",
                        u(2),
                        if u(3) == 1 { "pressed" } else { "released" }
                    )),
                    4 => note(format!(
                        "modifiers: dep={:#x} lat={:#x} lock={:#x} group={}",
                        u(1),
                        u(2),
                        u(3),
                        u(4)
                    )),
                    5 => note(format!("repeat_info: rate={} delay={}", u(0), u(1))),
                    op => note(format!("wl_keyboard event {op}")),
                }
            }
            pending.drain(..len);
        }
    }
}
