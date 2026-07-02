// wire codec: u32 words, fixed point, strings, wl_array. fds ride out of band
// via SCM_RIGHTS. reads are byte-cursor from_ne_bytes: no alignment demands, no
// unsafe. every read is bounds checked - a short message is a protocol error.

use std::collections::VecDeque;
use std::fmt;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use super::ObjectId;

/// header included; longer messages violate the protocol
pub const MAX_MESSAGE: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    TrailingData,
    BadString,
    BadUtf8,
    MissingFd,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated => write!(f, "message body shorter than its arguments"),
            WireError::TrailingData => write!(f, "message body longer than its arguments"),
            WireError::BadString => write!(f, "malformed string argument"),
            WireError::BadUtf8 => write!(f, "string argument is not utf-8"),
            WireError::MissingFd => write!(f, "message expects an fd that never arrived"),
        }
    }
}

impl std::error::Error for WireError {}

// -- reading --

/// cursor over one message body (header stripped). fds come from the
/// connection-level queue in arrival order.
pub struct MsgReader<'a> {
    body: &'a [u8],
    off: usize,
    fds: &'a mut VecDeque<OwnedFd>,
}

impl<'a> MsgReader<'a> {
    pub fn new(body: &'a [u8], fds: &'a mut VecDeque<OwnedFd>) -> MsgReader<'a> {
        MsgReader { body, off: 0, fds }
    }

    fn word(&mut self) -> Result<u32, WireError> {
        let end = self.off + 4;
        if end > self.body.len() {
            return Err(WireError::Truncated);
        }
        let v = u32::from_ne_bytes(self.body[self.off..end].try_into().unwrap());
        self.off = end;
        Ok(v)
    }

    pub fn uint(&mut self) -> Result<u32, WireError> {
        self.word()
    }

    pub fn int(&mut self) -> Result<i32, WireError> {
        Ok(self.word()? as i32)
    }

    pub fn fixed(&mut self) -> Result<super::Fixed, WireError> {
        Ok(super::Fixed(self.word()? as i32))
    }

    pub fn object(&mut self) -> Result<ObjectId, WireError> {
        Ok(ObjectId(self.word()?))
    }

    pub fn new_id(&mut self) -> Result<ObjectId, WireError> {
        Ok(ObjectId(self.word()?))
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let padded = (len + 3) & !3;
        let end = self.off.checked_add(padded).ok_or(WireError::Truncated)?;
        if end > self.body.len() {
            return Err(WireError::Truncated);
        }
        let out = &self.body[self.off..self.off + len];
        self.off = end;
        Ok(out)
    }

    pub fn array(&mut self) -> Result<&'a [u8], WireError> {
        let len = self.word()? as usize;
        self.bytes(len)
    }

    fn str_of(&mut self, len: usize) -> Result<&'a str, WireError> {
        // wire length includes the terminating NUL
        let raw = self.bytes(len)?;
        let Some((0, s)) = raw.split_last() else {
            return Err(WireError::BadString);
        };
        std::str::from_utf8(s).map_err(|_| WireError::BadUtf8)
    }

    pub fn string(&mut self) -> Result<&'a str, WireError> {
        let len = self.word()? as usize;
        if len == 0 {
            // zero length means absent, but this argument is required
            return Err(WireError::BadString);
        }
        self.str_of(len)
    }

    pub fn optstring(&mut self) -> Result<Option<&'a str>, WireError> {
        let len = self.word()? as usize;
        if len == 0 {
            return Ok(None);
        }
        self.str_of(len).map(Some)
    }

    pub fn fd(&mut self) -> Result<OwnedFd, WireError> {
        self.fds.pop_front().ok_or(WireError::MissingFd)
    }

    pub fn finish(&self) -> Result<(), WireError> {
        if self.off == self.body.len() {
            Ok(())
        } else {
            Err(WireError::TrailingData)
        }
    }
}

// -- writing --

/// one client's pending outbound bytes plus the fds pinned to them. the write
/// target the generated event senders append to; transport lives with the client.
#[derive(Default)]
pub struct EventOut {
    pub bytes: Vec<u8>,
    pub fds: Vec<Rc<OwnedFd>>,
}

/// appends one message; length is patched into the header at finish() since
/// strings and arrays make it variable
pub struct MsgWriter<'a> {
    out: &'a mut EventOut,
    start: usize,
}

impl<'a> MsgWriter<'a> {
    pub fn new(out: &'a mut EventOut, object: ObjectId, opcode: u32) -> MsgWriter<'a> {
        debug_assert!(opcode <= 0xffff);
        let start = out.bytes.len();
        out.bytes.extend_from_slice(&object.0.to_ne_bytes());
        out.bytes.extend_from_slice(&opcode.to_ne_bytes());
        MsgWriter { out, start }
    }

    pub fn uint(&mut self, v: u32) {
        self.out.bytes.extend_from_slice(&v.to_ne_bytes());
    }

    pub fn int(&mut self, v: i32) {
        self.uint(v as u32);
    }

    pub fn fixed(&mut self, v: super::Fixed) {
        self.uint(v.0 as u32);
    }

    pub fn object(&mut self, v: ObjectId) {
        self.uint(v.0);
    }

    pub fn string(&mut self, s: &str) {
        let len = s.len() + 1; // NUL included
        self.uint(len as u32);
        self.out.bytes.extend_from_slice(s.as_bytes());
        let padded = (len + 3) & !3;
        self.out.bytes.extend(std::iter::repeat_n(0, padded - s.len()));
    }

    pub fn optstring(&mut self, s: Option<&str>) {
        match s {
            Some(s) => self.string(s),
            None => self.uint(0),
        }
    }

    pub fn array(&mut self, a: &[u8]) {
        self.uint(a.len() as u32);
        self.out.bytes.extend_from_slice(a);
        let padded = (a.len() + 3) & !3;
        self.out.bytes.extend(std::iter::repeat_n(0, padded - a.len()));
    }

    pub fn fd(&mut self, fd: Rc<OwnedFd>) {
        self.out.fds.push(fd);
    }

    /// patch len<<16 into the second header word; returns message size
    pub fn finish(self) -> usize {
        let len = self.out.bytes.len() - self.start;
        debug_assert!(len >= 8 && len % 4 == 0);
        assert!(len <= MAX_MESSAGE, "outgoing message exceeds the wire limit");
        let at = self.start + 4;
        let word = u32::from_ne_bytes(self.out.bytes[at..at + 4].try_into().unwrap())
            | ((len as u32) << 16);
        self.out.bytes[at..at + 4].copy_from_slice(&word.to_ne_bytes());
        len
    }
}
