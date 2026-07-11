// dbus marshalling, little-endian only - a big-endian peer gets a loud
// error, not a silent misparse. alignment is measured from message start.

use super::DbusError;
use std::os::fd::OwnedFd;
use std::rc::Rc;

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

pub const NO_REPLY_EXPECTED: u8 = 0x1;

/// header field codes
const F_PATH: u8 = 1;
const F_INTERFACE: u8 = 2;
const F_MEMBER: u8 = 3;
const F_ERROR_NAME: u8 = 4;
const F_REPLY_SERIAL: u8 = 5;
const F_DESTINATION: u8 = 6;
const F_SENDER: u8 = 7;
const F_SIGNATURE: u8 = 8;
const F_UNIX_FDS: u8 = 9;

// -- building --

pub struct MsgBuilder {
    buf: Vec<u8>,
    body_start: usize,
}

impl MsgBuilder {
    fn msg(mtype: u8, serial: u32, flags: u8) -> MsgBuilder {
        let mut buf = Vec::with_capacity(256);
        buf.push(b'l');
        buf.push(mtype);
        buf.push(flags);
        buf.push(1);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&serial.to_le_bytes());
        // header array length, backpatched in finish_header
        buf.extend_from_slice(&0u32.to_le_bytes());
        MsgBuilder { buf, body_start: 0 }
    }

    pub fn call(serial: u32, flags: u8) -> MsgBuilder {
        Self::msg(METHOD_CALL, serial, flags)
    }

    pub fn method_return(serial: u32, reply_to: u32, dest: &str) -> MsgBuilder {
        let mut b = Self::msg(METHOD_RETURN, serial, 1);
        b.field_u32(F_REPLY_SERIAL, reply_to);
        b.destination(dest);
        b
    }

    pub fn error_msg(serial: u32, reply_to: u32, dest: &str, name: &str) -> MsgBuilder {
        let mut b = Self::msg(ERROR, serial, 1);
        b.field_str(F_ERROR_NAME, b's', name);
        b.field_u32(F_REPLY_SERIAL, reply_to);
        b.destination(dest);
        b
    }

    /// fds ride out of band; the header carries only their count
    pub fn unix_fds(&mut self, n: u32) {
        self.field_u32(F_UNIX_FDS, n);
    }

    fn field_u32(&mut self, code: u8, value: u32) {
        self.pad(8);
        self.buf.push(code);
        self.buf.push(1);
        self.buf.push(b'u');
        self.buf.push(0);
        self.put_u32(value);
    }

    fn pad(&mut self, align: usize) {
        while self.buf.len() % align != 0 {
            self.buf.push(0);
        }
    }

    fn field_str(&mut self, code: u8, kind: u8, value: &str) {
        self.pad(8);
        self.buf.push(code);
        // variant: signature then payload
        self.buf.push(1);
        self.buf.push(kind);
        self.buf.push(0);
        self.put_str(value);
    }

    pub fn path(&mut self, v: &str) {
        self.field_str(F_PATH, b'o', v);
    }

    pub fn destination(&mut self, v: &str) {
        self.field_str(F_DESTINATION, b's', v);
    }

    pub fn interface(&mut self, v: &str) {
        self.field_str(F_INTERFACE, b's', v);
    }

    pub fn member(&mut self, v: &str) {
        self.field_str(F_MEMBER, b's', v);
    }

    pub fn signature(&mut self, v: &str) {
        self.pad(8);
        self.buf.push(F_SIGNATURE);
        self.buf.push(1);
        self.buf.push(b'g');
        self.buf.push(0);
        self.put_sig(v);
    }

    pub fn finish_header(&mut self) {
        let len = (self.buf.len() - 16) as u32;
        self.buf[12..16].copy_from_slice(&len.to_le_bytes());
        self.pad(8);
        self.body_start = self.buf.len();
    }

    // -- body writers; only what logind speaks --

    pub fn put_u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_i32(&mut self, v: i32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_bool(&mut self, v: bool) {
        self.put_u32(v as u32);
    }

    pub fn put_str(&mut self, v: &str) {
        self.pad(4);
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v.as_bytes());
        self.buf.push(0);
    }

    /// a{ss}: byte length, pad to the 8-aligned first entry, (str, str) pairs.
    /// the length counts from after that first pad, per the wire format
    pub fn put_str_dict(&mut self, entries: &[(&str, &str)]) {
        self.pad(4);
        let len_at = self.buf.len();
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.pad(8);
        let start = self.buf.len();
        for (k, v) in entries {
            self.pad(8);
            self.put_str(k);
            self.put_str(v);
        }
        let len = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    /// as: byte length, then packed strings
    pub fn put_str_array(&mut self, items: &[&str]) {
        self.pad(4);
        let len_at = self.buf.len();
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        let start = self.buf.len();
        for s in items {
            self.put_str(s);
        }
        let len = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn align(&mut self, n: usize) {
        self.pad(n);
    }

    /// variant: signature then the value, written by the closure
    pub fn put_variant(&mut self, sig: &str, f: impl FnOnce(&mut MsgBuilder)) {
        self.put_sig(sig);
        f(self);
    }

    /// array of elements with the given alignment; the length excludes the
    /// pad to the first element, per the wire format
    pub fn put_array(&mut self, elem_align: usize, f: impl FnOnce(&mut MsgBuilder)) {
        self.pad(4);
        let len_at = self.buf.len();
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.pad(elem_align);
        let start = self.buf.len();
        f(self);
        let len = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    fn put_sig(&mut self, v: &str) {
        self.buf.push(v.len() as u8);
        self.buf.extend_from_slice(v.as_bytes());
        self.buf.push(0);
    }

    pub fn finish(mut self) -> Vec<u8> {
        let body = (self.buf.len() - self.body_start) as u32;
        self.buf[4..8].copy_from_slice(&body.to_le_bytes());
        self.buf
    }
}

// -- parsing --

#[derive(Default, Debug)]
pub struct Header {
    pub mtype: u8,
    pub serial: u32,
    /// body byte range within the message
    pub body: (usize, usize),
    pub reply_serial: Option<u32>,
    pub error_name: Option<String>,
    pub signature: String,
    pub unix_fds: u32,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub path: Option<String>,
    pub sender: Option<String>,
}

/// total message length, once 16 prefix bytes are available
pub fn message_len(prefix: &[u8]) -> Option<usize> {
    if prefix.len() < 16 {
        return None;
    }
    let body = u32::from_le_bytes(prefix[4..8].try_into().unwrap()) as usize;
    let harr = u32::from_le_bytes(prefix[12..16].try_into().unwrap()) as usize;
    Some(16 + harr.next_multiple_of(8) + body)
}

pub fn parse(msg: &[u8]) -> Result<Header, DbusError> {
    if msg[0] != b'l' {
        return Err(DbusError::BigEndianPeer);
    }
    let mut h = Header {
        mtype: msg[1],
        serial: u32::from_le_bytes(msg[8..12].try_into().unwrap()),
        ..Default::default()
    };
    let harr = u32::from_le_bytes(msg[12..16].try_into().unwrap()) as usize;
    let body_start = 16 + harr.next_multiple_of(8);
    let body_len = u32::from_le_bytes(msg[4..8].try_into().unwrap()) as usize;
    h.body = (body_start, body_start + body_len);
    if h.body.1 > msg.len() {
        return Err(DbusError::Malformed("truncated body"));
    }

    let mut r = Rd {
        buf: &msg[..16 + harr],
        pos: 16,
        fds: &[],
    };
    while r.pos < r.buf.len() {
        r.align(8)?;
        if r.pos >= r.buf.len() {
            break;
        }
        let code = r.u8()?;
        let sig = r.sig()?;
        match (code, sig.as_str()) {
            (F_PATH, "o") => h.path = Some(r.str()?),
            (F_INTERFACE, "s") => h.interface = Some(r.str()?),
            (F_MEMBER, "s") => h.member = Some(r.str()?),
            (F_ERROR_NAME, "s") => h.error_name = Some(r.str()?),
            (F_REPLY_SERIAL, "u") => h.reply_serial = Some(r.u32()?),
            (F_SIGNATURE, "g") => h.signature = r.sig()?,
            (F_UNIX_FDS, "u") => h.unix_fds = r.u32()?,
            (F_SENDER, "s") => h.sender = Some(r.str()?),
            (F_DESTINATION, "s") => {
                let _ = r.str()?;
            }
            _ => r.skip_value(&sig)?,
        }
    }
    Ok(h)
}

#[derive(Debug, PartialEq)]
pub enum SvVal {
    U(u32),
    S(String),
}

pub struct Rd<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
    pub fds: &'a [Rc<OwnedFd>],
}

impl<'a> Rd<'a> {
    pub fn new(buf: &'a [u8], fds: &'a [Rc<OwnedFd>]) -> Rd<'a> {
        Rd { buf, pos: 0, fds }
    }

    pub fn align(&mut self, n: usize) -> Result<(), DbusError> {
        let new = self.pos.next_multiple_of(n);
        if new > self.buf.len() {
            return Err(DbusError::Malformed("padding past end"));
        }
        self.pos = new;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DbusError> {
        if self.pos + n > self.buf.len() {
            return Err(DbusError::Malformed("read past end"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, DbusError> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, DbusError> {
        self.align(4)?;
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn bool(&mut self) -> Result<bool, DbusError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DbusError::Malformed("boolean out of range")),
        }
    }

    pub fn str(&mut self) -> Result<String, DbusError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len + 1)?;
        std::str::from_utf8(&bytes[..len])
            .map(|s| s.to_string())
            .map_err(|_| DbusError::Malformed("string not utf-8"))
    }

    pub fn sig(&mut self) -> Result<String, DbusError> {
        let len = self.u8()? as usize;
        let bytes = self.take(len + 1)?;
        std::str::from_utf8(&bytes[..len])
            .map(|s| s.to_string())
            .map_err(|_| DbusError::Malformed("signature not utf-8"))
    }

    pub fn fd(&mut self) -> Result<Rc<OwnedFd>, DbusError> {
        let idx = self.u32()? as usize;
        self.fds
            .get(idx)
            .cloned()
            .ok_or(DbusError::Malformed("fd index out of range"))
    }

    /// the u32- and string-valued entries of an a{sv}; the rest is skipped
    pub fn sv_dict(&mut self) -> Result<Vec<(String, SvVal)>, DbusError> {
        let len = self.u32()? as usize;
        self.align(8)?;
        let end = self.pos + len;
        if end > self.buf.len() {
            return Err(DbusError::Malformed("dict past end"));
        }
        let mut out = Vec::new();
        while self.pos < end {
            self.align(8)?;
            let key = self.str()?;
            let sig = self.sig()?;
            match sig.as_str() {
                "u" => out.push((key, SvVal::U(self.u32()?))),
                "s" => out.push((key, SvVal::S(self.str()?))),
                _ => self.skip_value(&sig)?,
            }
        }
        Ok(out)
    }

    /// the u32-valued entries of an a{sv}; everything else is skipped
    pub fn u32_dict(&mut self) -> Result<Vec<(String, u32)>, DbusError> {
        Ok(self
            .sv_dict()?
            .into_iter()
            .filter_map(|(k, v)| match v {
                SvVal::U(u) => Some((k, u)),
                _ => None,
            })
            .collect())
    }

    /// generic skip for header fields we don't care about
    pub fn skip_value(&mut self, sig: &str) -> Result<(), DbusError> {
        match sig.as_bytes().first() {
            Some(b'y') => {
                self.u8()?;
            }
            Some(b'b' | b'u' | b'i') => {
                self.u32()?;
            }
            Some(b's' | b'o') => {
                self.str()?;
            }
            Some(b'g') => {
                self.sig()?;
            }
            Some(b't' | b'x' | b'd') => {
                self.align(8)?;
                self.take(8)?;
            }
            Some(b'a') => {
                let len = self.u32()? as usize;
                // element alignment for the aligned types that can appear here
                match sig.as_bytes().get(1) {
                    Some(b'(' | b't' | b'x' | b'd') => self.align(8)?,
                    _ => {}
                }
                self.take(len)?;
            }
            _ => return Err(DbusError::Malformed("unsupported field type")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_take_device(serial: u32, major: u32, minor: u32) -> Vec<u8> {
        let mut b = MsgBuilder::call(serial, 0);
        b.path("/org/freedesktop/login1/session/_32");
        b.destination("org.freedesktop.login1");
        b.interface("org.freedesktop.login1.Session");
        b.member("TakeDevice");
        b.signature("uu");
        b.finish_header();
        b.put_u32(major);
        b.put_u32(minor);
        b.finish()
    }

    #[test]
    fn build_then_parse_roundtrip() {
        let msg = build_take_device(7, 13, 68);
        assert_eq!(message_len(&msg), Some(msg.len()));
        let h = parse(&msg).unwrap();
        assert_eq!(h.mtype, METHOD_CALL);
        assert_eq!(h.serial, 7);
        assert_eq!(h.signature, "uu");
        assert_eq!(h.member.as_deref(), Some("TakeDevice"));
        assert_eq!(
            h.interface.as_deref(),
            Some("org.freedesktop.login1.Session")
        );
        assert_eq!(
            h.path.as_deref(),
            Some("/org/freedesktop/login1/session/_32")
        );
        assert_eq!(h.body.1 - h.body.0, 8);
        let mut r = Rd::new(&msg[h.body.0..h.body.1], &[]);
        assert_eq!(r.u32().unwrap(), 13);
        assert_eq!(r.u32().unwrap(), 68);
    }

    #[test]
    fn golden_prelude_bytes() {
        let msg = build_take_device(1, 0, 0);
        // endianness, type, flags, version, body len 8
        assert_eq!(&msg[..8], &[b'l', 1, 0, 1, 8, 0, 0, 0]);
        assert_eq!(&msg[8..12], &[1, 0, 0, 0]); // serial
        // body is 8-aligned and is the final 8 bytes
        assert_eq!(msg.len() % 8, 0);
    }

    #[test]
    fn fd_indices_resolve() {
        use rustix::fs::{MemfdFlags, memfd_create};
        let a = memfd_create("dbus-test-a", MemfdFlags::CLOEXEC).unwrap();
        let b = memfd_create("dbus-test-b", MemfdFlags::CLOEXEC).unwrap();
        let fds = [Rc::new(a), Rc::new(b)];
        // body "hb": fd index 1, bool true
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        let mut r = Rd::new(&body, &fds);
        let fd = r.fd().unwrap();
        assert!(Rc::ptr_eq(&fd, &fds[1]));
        assert!(r.bool().unwrap());
    }

    #[test]
    fn split_prefix_lengths() {
        let msg = build_take_device(3, 1, 2);
        assert_eq!(message_len(&msg[..8]), None);
        assert_eq!(message_len(&msg[..16]), Some(msg.len()));
    }

    #[test]
    fn u32_dict_reads_what_the_builder_wrote() {
        let mut b = MsgBuilder::call(1, 0);
        b.path("/x");
        b.destination("d");
        b.interface("i");
        b.member("m");
        b.signature("a{sv}");
        b.finish_header();
        b.put_array(8, |b| {
            b.align(8);
            b.put_str("cursor_mode");
            b.put_variant("u", |b| b.put_u32(2));
            b.align(8);
            b.put_str("restore_token");
            b.put_variant("s", |b| b.put_str("tok"));
            b.align(8);
            b.put_str("types");
            b.put_variant("u", |b| b.put_u32(1));
        });
        let msg = b.finish();
        let h = parse(&msg).unwrap();
        let mut r = Rd::new(&msg[h.body.0..h.body.1], &[]);
        let d = r.u32_dict().unwrap();
        assert_eq!(d, vec![("cursor_mode".into(), 2), ("types".into(), 1)]);
    }

    #[test]
    fn sv_dict_keeps_strings_and_u32s() {
        let mut b = MsgBuilder::call(1, 0);
        b.path("/x");
        b.destination("d");
        b.interface("i");
        b.member("m");
        b.signature("a{sv}");
        b.finish_header();
        b.put_array(8, |b| {
            b.align(8);
            b.put_str("persist_mode");
            b.put_variant("u", |b| b.put_u32(2));
            b.align(8);
            b.put_str("restore_token");
            b.put_variant("s", |b| b.put_str("carrot:1:2"));
            b.align(8);
            b.put_str("is_cool");
            b.put_variant("b", |b| b.put_bool(true));
        });
        let msg = b.finish();
        let h = parse(&msg).unwrap();
        let mut r = Rd::new(&msg[h.body.0..h.body.1], &[]);
        let d = r.sv_dict().unwrap();
        assert_eq!(
            d,
            vec![
                ("persist_mode".into(), SvVal::U(2)),
                ("restore_token".into(), SvVal::S("carrot:1:2".into())),
            ]
        );
    }

    #[test]
    fn streams_reply_shape_walks_back() {
        let mut b = MsgBuilder::method_return(2, 1, ":1.7");
        b.signature("ua{sv}");
        b.finish_header();
        b.put_u32(0);
        b.put_array(8, |b| {
            b.align(8);
            b.put_str("streams");
            b.put_variant("a(ua{sv})", |b| {
                b.put_array(8, |b| {
                    b.align(8);
                    b.put_u32(42);
                    b.put_array(8, |b| {
                        b.align(8);
                        b.put_str("size");
                        b.put_variant("(ii)", |b| {
                            b.align(8);
                            b.put_i32(1920);
                            b.put_i32(1080);
                        });
                    });
                });
            });
        });
        let msg = b.finish();
        let h = parse(&msg).unwrap();
        assert_eq!(h.signature, "ua{sv}");
        let mut r = Rd::new(&msg[h.body.0..h.body.1], &[]);
        assert_eq!(r.u32().unwrap(), 0); // response code
        let _results = r.u32().unwrap(); // results dict byte length
        r.align(8).unwrap();
        assert_eq!(r.str().unwrap(), "streams");
        assert_eq!(r.sig().unwrap(), "a(ua{sv})");
        let _streams = r.u32().unwrap(); // array byte length
        r.align(8).unwrap();
        assert_eq!(r.u32().unwrap(), 42); // node id
        let _props = r.u32().unwrap();
        r.align(8).unwrap();
        assert_eq!(r.str().unwrap(), "size");
        assert_eq!(r.sig().unwrap(), "(ii)");
        r.align(8).unwrap();
        assert_eq!(r.u32().unwrap(), 1920);
        assert_eq!(r.u32().unwrap(), 1080);
    }
}
