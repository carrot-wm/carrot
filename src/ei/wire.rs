// ei framing: 16-byte header (u64 object, u32 total length, u32 opcode),
// native endian, args padded to 4. strings carry their nul in the length.
// requests and events number separately per interface, in declaration order.

pub const HEADER: usize = 16;
pub const MAX_MSG: usize = u16::MAX as usize;

/// object id, total length, opcode. None until 16 bytes are buffered
pub fn header(buf: &[u8]) -> Option<(u64, usize, u32)> {
    if buf.len() < HEADER {
        return None;
    }
    let object = u64::from_ne_bytes(buf[0..8].try_into().unwrap());
    let len = u32::from_ne_bytes(buf[8..12].try_into().unwrap()) as usize;
    let opcode = u32::from_ne_bytes(buf[12..16].try_into().unwrap());
    Some((object, len, opcode))
}

// -- events out --

pub struct MsgBuilder {
    buf: Vec<u8>,
}

impl MsgBuilder {
    pub fn new(object: u64, opcode: u32) -> MsgBuilder {
        let mut buf = Vec::with_capacity(32);
        buf.extend_from_slice(&object.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&opcode.to_ne_bytes());
        MsgBuilder { buf }
    }

    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_ne_bytes());
        self
    }

    pub fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_ne_bytes());
        self
    }

    pub fn f32(self, v: f32) -> Self {
        self.u32(v.to_bits())
    }

    pub fn string(mut self, s: &str) -> Self {
        let len = s.len() as u32 + 1;
        self.buf.extend_from_slice(&len.to_ne_bytes());
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
        self
    }

    /// backpatch the length and hand the bytes over
    pub fn finish(mut self) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[8..12].copy_from_slice(&len.to_ne_bytes());
        self.buf
    }
}

// -- request args in --

pub struct Args<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Args<'a> {
    pub fn new(body: &'a [u8]) -> Args<'a> {
        Args { buf: body, pos: 0 }
    }

    pub fn u32(&mut self) -> Result<u32, &'static str> {
        let end = self.pos.checked_add(4).filter(|&e| e <= self.buf.len());
        let end = end.ok_or("truncated u32")?;
        let v = u32::from_ne_bytes(self.buf[self.pos..end].try_into().unwrap());
        self.pos = end;
        Ok(v)
    }

    pub fn i32(&mut self) -> Result<i32, &'static str> {
        Ok(self.u32()? as i32)
    }

    pub fn u64(&mut self) -> Result<u64, &'static str> {
        let end = self.pos.checked_add(8).filter(|&e| e <= self.buf.len());
        let end = end.ok_or("truncated u64")?;
        let v = u64::from_ne_bytes(self.buf[self.pos..end].try_into().unwrap());
        self.pos = end;
        Ok(v)
    }

    pub fn f32(&mut self) -> Result<f32, &'static str> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub fn string(&mut self) -> Result<&'a str, &'static str> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Err("null string");
        }
        let padded = len.div_ceil(4) * 4;
        let end = self.pos.checked_add(padded).filter(|&e| e <= self.buf.len());
        let end = end.ok_or("truncated string")?;
        let bytes = &self.buf[self.pos..self.pos + len];
        if bytes[len - 1] != 0 {
            return Err("string missing nul");
        }
        self.pos = end;
        std::str::from_utf8(&bytes[..len - 1]).map_err(|_| "string not utf-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let msg = MsgBuilder::new(0xff00_0000_0000_0007, 3)
            .u32(42)
            .u64(9)
            .finish();
        let (object, len, opcode) = header(&msg).unwrap();
        assert_eq!(object, 0xff00_0000_0000_0007);
        assert_eq!(len, msg.len());
        assert_eq!(len, HEADER + 4 + 8);
        assert_eq!(opcode, 3);
        let mut a = Args::new(&msg[HEADER..]);
        assert_eq!(a.u32().unwrap(), 42);
        assert_eq!(a.u64().unwrap(), 9);
        assert!(header(&msg[..15]).is_none());
    }

    #[test]
    fn string_padding() {
        // "abc" -> len 4, no pad; "abcd" -> len 5, padded to 8
        let msg = MsgBuilder::new(1, 0).string("abc").string("abcd").finish();
        assert_eq!(msg.len(), HEADER + (4 + 4) + (4 + 8));
        assert_eq!(msg.len() % 4, 0);
        let mut a = Args::new(&msg[HEADER..]);
        assert_eq!(a.string().unwrap(), "abc");
        assert_eq!(a.string().unwrap(), "abcd");
        assert!(a.u32().is_err());
    }

    #[test]
    fn f32_and_truncation() {
        let msg = MsgBuilder::new(2, 1).f32(-1.5).finish();
        let mut a = Args::new(&msg[HEADER..]);
        assert_eq!(a.f32().unwrap(), -1.5);
        let mut short = Args::new(&[0u8; 3]);
        assert!(short.u32().is_err());
        let mut bad = Args::new(&[4, 0, 0, 0, b'a', b'b', b'c', b'd']);
        assert!(bad.string().is_err());
    }
}
