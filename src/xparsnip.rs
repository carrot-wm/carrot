// our x11 wire library. async all the way down - a property read that can
// stall the compositor is a bug. request/reply/event codecs generate from
// tools/gen-xwire: nothing on the wire is hand-numbered.

mod auth;
pub mod conn;
pub mod wire;

// dev diagnostic (`carrot xparsnip-probe`): connect to the session's running
// x server as a plain client and exercise the whole request pipeline.
pub fn probe() -> i32 {
    use crate::engine::{Engine, Wheel};
    use crate::uring::Ring;
    use std::cell::Cell;
    use std::rc::Rc;

    let display: u32 = match std::env::var("DISPLAY") {
        Ok(d) => match d.strip_prefix(':').and_then(|n| n.split('.').next()) {
            Some(n) => n.parse().unwrap_or(0),
            None => 0,
        },
        Err(_) => {
            eprintln!("FAIL: DISPLAY not set");
            return 1;
        }
    };

    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let _wheel = match Wheel::new(&engine, &ring) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("wheel: {e}");
            return 1;
        }
    };
    let status = Rc::new(Cell::new(1));
    let st = status.clone();
    let eng = engine.clone();
    let rng = ring.clone();
    let task = engine.spawn("parsnip probe", async move {
        let run = async {
            use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, socket};
            let path = format!("/tmp/.X11-unix/X{display}");
            let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                .map_err(|e| format!("socket: {e}"))?;
            let addr = SocketAddrUnix::new(&*path).map_err(|e| format!("addr: {e}"))?;
            rustix::net::connect(&fd, &addr).map_err(|e| format!("{path}: {e}"))?;
            println!("-> connected to {path}");
            let cookie = auth::cookie_for_display(display);
            let (name, data): (&[u8], Vec<u8>) = match &cookie {
                Some(c) => {
                    println!("-> using MIT-MAGIC-COOKIE-1 from xauthority");
                    (b"MIT-MAGIC-COOKIE-1", c.clone())
                }
                None => {
                    println!("-> no xauthority cookie, empty auth");
                    (b"", Vec::new())
                }
            };
            let c = conn::Parsnip::connect(&eng, &rng, fd, name, &data)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            println!("-> setup ok: root {:#x} depth {}", c.root, c.root_depth);
            {
                let e = c.ext.borrow();
                let e = e.as_ref().unwrap();
                println!(
                    "-> extensions: composite {} xfixes {} (events {}) render {} res {}",
                    e.composite, e.xfixes, e.xfixes_first_event, e.render, e.res
                );
            }
            let geo = c
                .call(|b| wire::get_geometry(b, c.root))
                .await
                .map_err(|e| format!("get_geometry: {e}"))?;
            let geo = wire::parse_get_geometry(&geo).ok_or("bad geometry reply")?;
            println!("-> root geometry {}x{}", geo.width, geo.height);
            let wm_s0 = c.intern("WM_S0").await.map_err(|e| format!("intern: {e}"))?;
            let supported = c.intern("_NET_SUPPORTED").await.map_err(|e| e.to_string())?;
            println!("-> atoms: WM_S0={wm_s0} _NET_SUPPORTED={supported}");
            let prop = c
                .get_property_full(c.root, supported, 4, 64 << 10)
                .await
                .map_err(|e| format!("get_property: {e}"))?;
            println!(
                "-> _NET_SUPPORTED on root: {} atoms",
                prop.data.len() / 4
            );
            // a void request plus a reply after it exercises the fence
            let wid = c.alloc_xid();
            c.send(|b| {
                wire::create_window(b, 0, wid, c.root, -1, -1, 1, 1, 0, 2, 0, &[(9, 1)])
            });
            let geo = c
                .call(|b| wire::get_geometry(b, wid))
                .await
                .map_err(|e| format!("own window geometry: {e}"))?;
            let geo = wire::parse_get_geometry(&geo).ok_or("bad geometry reply")?;
            println!("-> created InputOnly window {wid:#x} ({}x{})", geo.width, geo.height);
            // map mode: act like a real client so the wm has something to
            // manage; the background pixel gives xwayland pixels to commit
            if std::env::var_os("CARROT_X_MAP").is_some() {
                let win = c.alloc_xid();
                c.send(|b| {
                    wire::create_window(
                        b, 0, win, c.root, 10, 10, 200, 150, 0, 1, 0,
                        &[(1, 0x00ff8000)],
                    )
                });
                // WM_CLASS is predefined atom 67, WM_NAME 39; set both plus
                // a utf8 _NET_WM_NAME so the wm has real strings to track
                let net_wm_name = c.intern("_NET_WM_NAME").await.map_err(|e| e.to_string())?;
                let utf8 = c.intern("UTF8_STRING").await.map_err(|e| e.to_string())?;
                c.send(|b| wire::change_property(b, 0, win, 67, 31, 8, b"xprobe\0XProbe\0"));
                c.send(|b| wire::change_property(b, 0, win, net_wm_name, utf8, 8, b"carrot x probe"));
                c.send(|b| wire::map_window(b, win));
                c.call(|b| wire::get_input_focus(b))
                    .await
                    .map_err(|e| format!("map fence: {e}"))?;
                println!("-> mapped InputOutput window {win:#x}, holding 3s");
                let deadline = crate::util::Time::from_nsec(
                    crate::util::Time::now().nsec() + 3_000_000_000,
                );
                let _ = rng.timeout(deadline).await;
            }
            c.clear();
            Ok::<(), String>(())
        };
        match run.await {
            Ok(()) => {
                println!("PASS");
                st.set(0);
            }
            Err(e) => eprintln!("FAIL: {e}"),
        }
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    engine.clear();
    status.get()
}

#[cfg(test)]
mod tests {
    use super::wire;

    // expected bytes below come from the x11 protocol encoding tables,
    // not from running our own encoder against itself

    #[test]
    fn intern_atom_encodes_per_spec() {
        let mut b = Vec::new();
        wire::intern_atom(&mut b, false, b"WM_S0");
        let mut want = vec![16u8, 0];
        want.extend(4u16.to_ne_bytes()); // 16 bytes = 4 units
        want.extend(5u16.to_ne_bytes()); // name length
        want.extend([0, 0]);
        want.extend(b"WM_S0");
        want.extend([0, 0, 0]); // pad to 4
        assert_eq!(b, want);
    }

    #[test]
    fn configure_window_orders_the_value_list_by_bit() {
        let mut b = Vec::new();
        // passed out of order on purpose: width (bit 2) then x (bit 0)
        wire::configure_window(&mut b, 7, &[(2, 800), (0, 10)]);
        let mut want = vec![12u8, 0];
        want.extend(5u16.to_ne_bytes()); // 20 bytes = 5 units
        want.extend(7u32.to_ne_bytes());
        want.extend(0b101u16.to_ne_bytes()); // mask: x | width
        want.extend([0, 0]);
        want.extend(10u32.to_ne_bytes()); // bit 0 first
        want.extend(800u32.to_ne_bytes());
        assert_eq!(b, want);
    }

    #[test]
    fn get_property_reply_parses() {
        let mut f = vec![1u8, 8]; // reply, format 8
        f.extend(9u16.to_ne_bytes()); // sequence
        f.extend(2u32.to_ne_bytes()); // extra length units
        f.extend(31u32.to_ne_bytes()); // type atom
        f.extend(0u32.to_ne_bytes()); // bytes_after
        f.extend(4u32.to_ne_bytes()); // value length in format units
        f.extend([0u8; 12]);
        f.extend(b"carr");
        f.extend([0u8; 4]);
        let r = wire::parse_get_property(&f).unwrap();
        assert_eq!(r.format, 8);
        assert_eq!(r.ty, 31);
        assert_eq!(r.data, b"carr");
    }

    #[test]
    fn client_message_event_roundtrips() {
        let f = wire::encode_client_message(0x40_0002, 55, 32, &[1, 2, 3, 4, 5]);
        match wire::parse_event(&f, 0) {
            Some(wire::XEvent::ClientMessage { format, window, ty, data }) => {
                assert_eq!(format, 32);
                assert_eq!(window, 0x40_0002);
                assert_eq!(ty, 55);
                assert_eq!(data, [1, 2, 3, 4, 5]);
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn xfixes_selection_notify_matches_at_runtime_offset() {
        let mut f = [0u8; 32];
        f[0] = 87; // first_event 87 + 0
        f[1] = 1; // subtype
        f[4..8].copy_from_slice(&11u32.to_ne_bytes()); // window
        f[8..12].copy_from_slice(&22u32.to_ne_bytes()); // owner
        f[12..16].copy_from_slice(&33u32.to_ne_bytes()); // selection
        match wire::parse_event(&f, 87) {
            Some(wire::XEvent::XfixesSelectionNotify { subtype, window, owner, selection, .. }) => {
                assert_eq!((subtype, window, owner, selection), (1, 11, 22, 33));
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn setup_request_leads_with_the_byte_order() {
        let mut b = Vec::new();
        wire::encode_setup_request(&mut b, b"", b"");
        assert_eq!(b[0], if cfg!(target_endian = "little") { 0x6c } else { 0x42 });
        assert_eq!(&b[2..4], &11u16.to_ne_bytes());
        assert_eq!(b.len(), 12);
    }

    #[test]
    fn error_frames_parse_with_names() {
        let mut f = [0u8; 32];
        f[1] = 3; // BadWindow
        f[2..4].copy_from_slice(&41u16.to_ne_bytes());
        f[4..8].copy_from_slice(&0xdeadu32.to_ne_bytes());
        f[10] = 12;
        let e = wire::parse_error(&f).unwrap();
        assert_eq!((e.code, e.sequence, e.bad_value, e.major), (3, 41, 0xdead, 12));
        assert_eq!(wire::error_name(e.code), "Window");
    }
}
