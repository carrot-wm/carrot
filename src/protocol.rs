// wayland protocol layer. wl_protocol! is the single source of truth:
// interfaces, opcodes, stubs all generate from one declaration, requests and
// events numbered separately. wl_array and fd args are first class.

pub mod data_device;
pub mod display;
pub mod globals;
pub mod interfaces;
pub mod output;
pub mod primary_selection;
pub mod shm;
pub mod wire;

use std::fmt;

// -- identity --

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32);

impl ObjectId {
    pub const NONE: ObjectId = ObjectId(0);

    pub fn is_server(self) -> bool {
        self.0 >= MIN_SERVER_ID
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

pub const WL_DISPLAY_ID: ObjectId = ObjectId(1);
/// server-allocated ids start here
pub const MIN_SERVER_ID: u32 = 0xff00_0000;

/// signed 24.8 fixed point
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Fixed(pub i32);

impl Fixed {
    pub fn from_int(v: i32) -> Fixed {
        Fixed(v << 8)
    }

    pub fn to_f64(self) -> f64 {
        self.0 as f64 / 256.0
    }

    pub fn from_f64(v: f64) -> Fixed {
        Fixed((v * 256.0) as i32)
    }
}

// -- dispatch --

#[derive(Debug)]
pub enum DispatchError {
    /// also what a too-old bound version gets: indistinguishable from a bad opcode
    UnknownOpcode(u32),
    Wire(wire::WireError),
    Handler(Box<dyn std::error::Error>),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchError::UnknownOpcode(op) => write!(f, "unknown opcode {op}"),
            DispatchError::Wire(e) => write!(f, "malformed request: {e}"),
            DispatchError::Handler(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DispatchError {}

// -- the macro --

/// one declaration per interface in protocol XML order. every message becomes a
/// module: requests get OPCODE + Request + parse, events get OPCODE + send.
/// requests and events number independently from zero, in declaration order;
/// golden tests in interfaces.rs pin every opcode.
#[macro_export]
macro_rules! wl_protocol {
    (
        interface $iface:ident, version = $ver:literal;
        $(request $req:ident ( $($ra:ident : $rt:ident),* $(,)? ) $(since $rs:literal)? ;)*
        $(event $evt:ident ( $($ea:ident : $et:ident),* $(,)? ) $(since $es:literal)? ;)*
    ) => {
        pub mod $iface {
            #![allow(dead_code, unused_imports)]

            use $crate::protocol::DispatchError;
            use $crate::protocol::wire::MsgReader;

            pub const NAME: &str = stringify!($iface);
            pub const VERSION: u32 = $ver;

            $crate::wl_protocol!(@requests 0u32; $($req ( $($ra : $rt),* ) $(since $rs)? ;)*);

            pub trait Handler {
                $(fn $req(&self, req: $req::Request) -> Result<(), Box<dyn std::error::Error>>;)*
            }

            #[allow(unused_variables)]
            pub fn dispatch<H: Handler>(
                h: &H,
                version: u32,
                opcode: u32,
                r: &mut MsgReader<'_>,
            ) -> Result<(), DispatchError> {
                $crate::wl_protocol!(@dispatch h, version, opcode, r; $($req;)*);
                Err(DispatchError::UnknownOpcode(opcode))
            }

            $crate::wl_protocol!(@events 0u32; $($evt ( $($ea : $et),* ) $(since $es)? ;)*);
        }
    };

    // -- request modules, opcodes by position --
    (@requests $n:expr; ) => {};
    (@requests $n:expr;
        $name:ident ( $($arg:ident : $ty:ident),* ) $(since $s:literal)? ;
        $($rest:tt)*
    ) => {
        pub mod $name {
            #![allow(dead_code, unused_imports)]

            use $crate::protocol::wire::{MsgReader, WireError};

            pub const OPCODE: u32 = $n;
            pub const SINCE: u32 = $crate::wl_protocol!(@since $($s)?);

            #[derive(Debug)]
            pub struct Request {
                $(pub $arg: $crate::wl_protocol!(@own $ty),)*
            }

            impl Request {
                pub fn parse(r: &mut MsgReader<'_>) -> Result<Request, WireError> {
                    $(let $arg = $crate::wl_protocol!(@read $ty, r);)*
                    r.finish()?;
                    Ok(Request { $($arg,)* })
                }
            }
        }
        $crate::wl_protocol!(@requests ($n + 1); $($rest)*);
    };

    // -- dispatch arms, driven by the modules' own opcode consts --
    (@dispatch $h:ident, $v:ident, $op:ident, $r:ident; ) => {};
    (@dispatch $h:ident, $v:ident, $op:ident, $r:ident;
        $name:ident;
        $($rest:tt)*
    ) => {
        if $op == $name::OPCODE {
            if $v < $name::SINCE {
                return Err(DispatchError::UnknownOpcode($op));
            }
            let req = $name::Request::parse($r).map_err(DispatchError::Wire)?;
            return $h.$name(req).map_err(DispatchError::Handler);
        }
        $crate::wl_protocol!(@dispatch $h, $v, $op, $r; $($rest)*);
    };

    // -- event modules with send builders --
    (@events $n:expr; ) => {};
    (@events $n:expr;
        $name:ident ( $($arg:ident : $ty:ident),* ) $(since $s:literal)? ;
        $($rest:tt)*
    ) => {
        pub mod $name {
            #![allow(dead_code, unused_imports)]

            use $crate::protocol::ObjectId;
            use $crate::protocol::wire::{EventOut, MsgWriter};

            pub const OPCODE: u32 = $n;
            pub const SINCE: u32 = $crate::wl_protocol!(@since $($s)?);

            pub fn send(
                o: &mut EventOut,
                self_id: ObjectId
                $(, $arg: $crate::wl_protocol!(@param $ty))*
            ) {
                let mut w = MsgWriter::new(o, self_id, OPCODE);
                $($crate::wl_protocol!(@write $ty, w, $arg);)*
                w.finish();
            }
        }
        $crate::wl_protocol!(@events ($n + 1); $($rest)*);
    };

    (@since ) => { 1 };
    (@since $s:literal) => { $s };

    // owned types for parsed requests: strings/arrays are cold, so the
    // allocation buys a macro with no lifetime plumbing
    (@own int) => { i32 };
    (@own uint) => { u32 };
    (@own fixed) => { $crate::protocol::Fixed };
    (@own object) => { $crate::protocol::ObjectId };
    (@own new_id) => { $crate::protocol::ObjectId };
    (@own string) => { String };
    (@own optstring) => { Option<String> };
    (@own array) => { Vec<u8> };
    (@own fd) => { std::os::fd::OwnedFd };

    (@read int, $r:ident) => { $r.int()? };
    (@read uint, $r:ident) => { $r.uint()? };
    (@read fixed, $r:ident) => { $r.fixed()? };
    (@read object, $r:ident) => { $r.object()? };
    (@read new_id, $r:ident) => { $r.new_id()? };
    (@read string, $r:ident) => { $r.string()?.to_owned() };
    (@read optstring, $r:ident) => { $r.optstring()?.map(str::to_owned) };
    (@read array, $r:ident) => { $r.array()?.to_vec() };
    (@read fd, $r:ident) => { $r.fd()? };

    (@param int) => { i32 };
    (@param uint) => { u32 };
    (@param fixed) => { $crate::protocol::Fixed };
    (@param object) => { $crate::protocol::ObjectId };
    (@param new_id) => { $crate::protocol::ObjectId };
    (@param string) => { &str };
    (@param optstring) => { Option<&str> };
    (@param array) => { &[u8] };
    (@param fd) => { std::rc::Rc<std::os::fd::OwnedFd> };

    (@write int, $w:ident, $v:ident) => { $w.int($v); };
    (@write uint, $w:ident, $v:ident) => { $w.uint($v); };
    (@write fixed, $w:ident, $v:ident) => { $w.fixed($v); };
    (@write object, $w:ident, $v:ident) => { $w.object($v); };
    (@write new_id, $w:ident, $v:ident) => { $w.object($v); };
    (@write string, $w:ident, $v:ident) => { $w.string($v); };
    (@write optstring, $w:ident, $v:ident) => { $w.optstring($v); };
    (@write array, $w:ident, $v:ident) => { $w.array($v); };
    (@write fd, $w:ident, $v:ident) => { $w.fd($v); };
}
