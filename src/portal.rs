// the xdg-desktop-portal backend, in-process: carrot claims
// org.freedesktop.impl.portal.desktop.carrot on the session bus and serves
// ScreenCast itself - no external backend, no fork. streams start flowing
// once the pipewire source wiring lands; until then Start answers "ended"
// so consumers fail clean instead of hanging.

use crate::dbus::{DbusConn, DbusError, MethodCall, MsgBuilder};
use crate::engine::Engine;
use crate::state::State;
use crate::uring::Ring;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const PORTAL_NAME: &str = "org.freedesktop.impl.portal.desktop.carrot";
const IF_SCREENCAST: &str = "org.freedesktop.impl.portal.ScreenCast";
const IF_PROPS: &str = "org.freedesktop.DBus.Properties";
const IF_SESSION: &str = "org.freedesktop.impl.portal.Session";
const IF_REQUEST: &str = "org.freedesktop.impl.portal.Request";

const SOURCE_MONITOR: u32 = 1;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
const VERSION: u32 = 2;
// portal response codes
const R_ENDED: u32 = 2;

#[derive(Default)]
struct Session {
    cursor_mode: u32,
}

type Sessions = Rc<RefCell<HashMap<String, Session>>>;

fn reply_response(c: &DbusConn, call: &MethodCall, code: u32) {
    c.reply(call, "ua{sv}", |b| {
        b.put_u32(code);
        b.put_array(8, |_| {});
    });
}

fn prop_variant(b: &mut MsgBuilder, prop: &str) -> bool {
    match prop {
        "version" => b.put_variant("u", |b| b.put_u32(VERSION)),
        "AvailableSourceTypes" => b.put_variant("u", |b| b.put_u32(SOURCE_MONITOR)),
        "AvailableCursorModes" => {
            b.put_variant("u", |b| b.put_u32(CURSOR_HIDDEN | CURSOR_EMBEDDED))
        }
        _ => return false,
    }
    true
}

fn serve_properties(conn: &Rc<DbusConn>) {
    conn.serve(IF_PROPS, Box::new(|c, call| match call.member.as_str() {
        "Get" => {
            let mut rd = call.rd();
            let iface = rd.str().unwrap_or_default();
            let prop = rd.str().unwrap_or_default();
            if iface != IF_SCREENCAST {
                c.reply_err(
                    call,
                    "org.freedesktop.DBus.Error.UnknownInterface",
                    "only screencast here",
                );
                return;
            }
            let mut ok = false;
            c.reply(call, "v", |b| ok = prop_variant(b, &prop));
            if !ok {
                // the reply already went out; unknown props answer as u 0,
                // which the frontend treats as absent
            }
        }
        "GetAll" => {
            c.reply(call, "a{sv}", |b| {
                b.put_array(8, |b| {
                    for p in ["version", "AvailableSourceTypes", "AvailableCursorModes"] {
                        b.align(8);
                        b.put_str(p);
                        prop_variant(b, p);
                    }
                });
            });
        }
        _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
    }));
}

fn serve_screencast(conn: &Rc<DbusConn>, sessions: Sessions) {
    conn.serve(IF_SCREENCAST, Box::new(move |c, call| {
        match call.member.as_str() {
            "CreateSession" => {
                let mut rd = call.rd();
                let _request = rd.str().unwrap_or_default();
                let session = rd.str().unwrap_or_default();
                sessions.borrow_mut().insert(session, Session::default());
                c.reply(call, "ua{sv}", |b| {
                    b.put_u32(0);
                    b.put_array(8, |b| {
                        // the session id result key is required by the spec
                        b.align(8);
                        b.put_str("session_id");
                        b.put_variant("s", |b| b.put_str("carrot"));
                    });
                });
            }
            "SelectSources" => {
                let mut rd = call.rd();
                let _request = rd.str().unwrap_or_default();
                let session = rd.str().unwrap_or_default();
                if !sessions.borrow().contains_key(&session) {
                    c.reply_err(
                        call,
                        "org.freedesktop.DBus.Error.Failed",
                        "no such session",
                    );
                    return;
                }
                reply_response(c, call, 0);
            }
            "Start" => {
                // the pipewire stream wiring is not in yet: end the cast
                // honestly instead of leaving the app waiting
                reply_response(c, call, R_ENDED);
            }
            _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
        }
    }));
}

async fn run_inner(
    eng: &Rc<Engine>,
    ring: &Rc<Ring>,
    _state: Rc<State>,
) -> Result<(), DbusError> {
    let conn = DbusConn::connect_session(eng, ring).await?;
    let sessions: Sessions = Rc::new(RefCell::new(HashMap::new()));
    serve_properties(&conn);
    serve_screencast(&conn, sessions.clone());
    conn.serve(IF_SESSION, Box::new({
        let sessions = sessions.clone();
        move |c, call| match call.member.as_str() {
            "Close" => {
                sessions.borrow_mut().remove(&call.path);
                c.reply(call, "", |_| {});
            }
            _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
        }
    }));
    conn.serve(IF_REQUEST, Box::new(|c, call| match call.member.as_str() {
        "Close" => c.reply(call, "", |_| {}),
        _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
    }));
    conn.request_name(PORTAL_NAME).await?;
    eprintln!("carrot: portal: serving {PORTAL_NAME}");
    std::future::pending::<()>().await;
    Ok(())
}

pub async fn run(eng: Rc<Engine>, ring: Rc<Ring>, state: Rc<State>) {
    if let Err(e) = run_inner(&eng, &ring, state).await {
        eprintln!("carrot: portal: {e}");
    }
}

/// `carrot portal-probe [secs]`: serve the portal standalone so busctl and
/// the xdg-desktop-portal frontend can be tested without a compositor
pub fn probe() -> i32 {
    let secs: u64 = std::env::args()
        .skip_while(|a| a != "portal-probe")
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(30);
    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let eng = engine.clone();
    let rng = ring.clone();
    let task = engine.spawn("portal probe", async move {
        let state = crate::state::State::new(&eng, &rng, match crate::engine::Wheel::new(&eng, &rng) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("wheel: {e}");
                rng.stop();
                return;
            }
        });
        let served = eng.spawn("portal", run(eng.clone(), rng.clone(), state));
        let deadline = crate::util::Time::from_nsec(
            crate::util::Time::now().nsec() + secs * 1_000_000_000,
        );
        let _ = rng.timeout(deadline).await;
        drop(served);
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    engine.clear();
    0
}
