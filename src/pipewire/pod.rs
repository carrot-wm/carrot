// spa pod codec: everything pipewire speaks is a pod - a size|type header,
// little-endian payload, everything padded to 8. only the shapes the client
// needs; unknown pods parse generically and skip clean.

pub const T_NONE: u32 = 0x01;
pub const T_BOOL: u32 = 0x02;
pub const T_ID: u32 = 0x03;
pub const T_INT: u32 = 0x04;
pub const T_LONG: u32 = 0x05;
pub const T_STRING: u32 = 0x08;
pub const T_STRUCT: u32 = 0x0e;
pub const T_OBJECT: u32 = 0x0f;
pub const T_FD: u32 = 0x12;

#[derive(Debug)]
pub enum PodError {
    Truncated,
    Type { want: u32, got: u32 },
    BadString,
}

impl std::fmt::Display for PodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PodError::Truncated => write!(f, "pod truncated"),
            PodError::Type { want, got } => write!(f, "pod type {got:#x}, wanted {want:#x}"),
            PodError::BadString => write!(f, "pod string is not utf-8/nul-terminated"),
        }
    }
}

// -- writer --

#[derive(Default)]
pub struct PodBuilder {
    pub buf: Vec<u8>,
}

impl PodBuilder {
    fn pod(&mut self, ty: u32, f: impl FnOnce(&mut PodBuilder)) {
        let at = self.buf.len();
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.buf.extend_from_slice(&ty.to_le_bytes());
        f(self);
        let size = (self.buf.len() - at - 8) as u32;
        self.buf[at..at + 4].copy_from_slice(&size.to_le_bytes());
        while self.buf.len() % 8 != 0 {
            self.buf.push(0);
        }
    }

    pub fn int(&mut self, v: i32) {
        self.pod(T_INT, |b| b.buf.extend_from_slice(&v.to_le_bytes()));
    }

    pub fn uint(&mut self, v: u32) {
        self.int(v as i32);
    }

    pub fn long(&mut self, v: i64) {
        self.pod(T_LONG, |b| b.buf.extend_from_slice(&v.to_le_bytes()));
    }

    pub fn id(&mut self, v: u32) {
        self.pod(T_ID, |b| b.buf.extend_from_slice(&v.to_le_bytes()));
    }

    pub fn bool_(&mut self, v: bool) {
        self.pod(T_BOOL, |b| b.buf.extend_from_slice(&(v as u32).to_le_bytes()));
    }

    /// fd pods carry an index into the message's fd array, not the fd
    pub fn fd(&mut self, index: i64) {
        self.pod(T_FD, |b| b.buf.extend_from_slice(&index.to_le_bytes()));
    }

    pub fn string(&mut self, v: &str) {
        self.pod(T_STRING, |b| {
            b.buf.extend_from_slice(v.as_bytes());
            b.buf.push(0);
        });
    }

    pub fn struct_(&mut self, f: impl FnOnce(&mut PodBuilder)) {
        self.pod(T_STRUCT, f);
    }

    /// pipewire's props dict: struct { n_items, then key/value strings }
    pub fn dict(&mut self, items: &[(&str, &str)]) {
        self.struct_(|b| {
            b.int(items.len() as i32);
            for (k, v) in items {
                b.string(k);
                b.string(v);
            }
        });
    }
}

// -- parser --

pub struct PodParser<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> PodParser<'a> {
    pub fn new(d: &'a [u8]) -> PodParser<'a> {
        PodParser { d, pos: 0 }
    }

    pub fn done(&self) -> bool {
        self.pos >= self.d.len()
    }

    fn header(&mut self) -> Result<(usize, u32), PodError> {
        if self.pos + 8 > self.d.len() {
            return Err(PodError::Truncated);
        }
        let size = u32::from_le_bytes(self.d[self.pos..self.pos + 4].try_into().unwrap()) as usize;
        let ty = u32::from_le_bytes(self.d[self.pos + 4..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        if self.pos + size > self.d.len() {
            return Err(PodError::Truncated);
        }
        Ok((size, ty))
    }

    /// consume the payload + its pad
    fn advance(&mut self, size: usize) {
        self.pos += size;
        self.pos += (8 - self.pos % 8) % 8;
        if self.pos > self.d.len() {
            self.pos = self.d.len();
        }
    }

    /// skip one pod of any type
    pub fn skip(&mut self) -> Result<(), PodError> {
        let (size, _) = self.header()?;
        self.advance(size);
        Ok(())
    }

    fn fixed(&mut self, want: u32, n: usize) -> Result<&'a [u8], PodError> {
        let (size, ty) = self.header()?;
        if ty != want {
            return Err(PodError::Type { want, got: ty });
        }
        if size < n {
            return Err(PodError::Truncated);
        }
        let d = &self.d[self.pos..self.pos + n];
        self.advance(size);
        Ok(d)
    }

    pub fn int(&mut self) -> Result<i32, PodError> {
        Ok(i32::from_le_bytes(self.fixed(T_INT, 4)?.try_into().unwrap()))
    }

    pub fn uint(&mut self) -> Result<u32, PodError> {
        Ok(self.int()? as u32)
    }

    pub fn long(&mut self) -> Result<i64, PodError> {
        Ok(i64::from_le_bytes(self.fixed(T_LONG, 8)?.try_into().unwrap()))
    }

    pub fn id(&mut self) -> Result<u32, PodError> {
        Ok(u32::from_le_bytes(self.fixed(T_ID, 4)?.try_into().unwrap()))
    }

    pub fn string(&mut self) -> Result<&'a str, PodError> {
        let (size, ty) = self.header()?;
        // a None where a string was expected is pipewire's null string
        if ty == T_NONE {
            self.advance(size);
            return Ok("");
        }
        if ty != T_STRING {
            return Err(PodError::Type { want: T_STRING, got: ty });
        }
        let d = &self.d[self.pos..self.pos + size];
        self.advance(size);
        let end = d.iter().position(|&b| b == 0).ok_or(PodError::BadString)?;
        std::str::from_utf8(&d[..end]).map_err(|_| PodError::BadString)
    }

    /// enter a struct: a sub-parser over its payload
    pub fn struct_(&mut self) -> Result<PodParser<'a>, PodError> {
        let (size, ty) = self.header()?;
        if ty != T_STRUCT {
            return Err(PodError::Type { want: T_STRUCT, got: ty });
        }
        let d = &self.d[self.pos..self.pos + size];
        self.advance(size);
        Ok(PodParser::new(d))
    }

    pub fn dict(&mut self) -> Result<Vec<(String, String)>, PodError> {
        let mut s = self.struct_()?;
        let n = s.int()?.max(0) as usize;
        let mut out = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let k = s.string()?.to_string();
            let v = s.string()?.to_string();
            out.push((k, v));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_pads() {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(-5);
            b.uint(7);
            b.string("seven*7"); // len 7 + nul = 8, no pad
            b.string("eight*&8"); // len 8 + nul = 9, pads to 16
            b.long(1 << 40);
            b.id(3);
            b.dict(&[("node.name", "carrot"), ("media.class", "Video/Source")]);
        });
        assert_eq!(b.buf.len() % 8, 0);
        let mut p = PodParser::new(&b.buf);
        let mut s = p.struct_().unwrap();
        assert_eq!(s.int().unwrap(), -5);
        assert_eq!(s.uint().unwrap(), 7);
        assert_eq!(s.string().unwrap(), "seven*7");
        assert_eq!(s.string().unwrap(), "eight*&8");
        assert_eq!(s.long().unwrap(), 1 << 40);
        assert_eq!(s.id().unwrap(), 3);
        let d = s.dict().unwrap();
        assert_eq!(d[1], ("media.class".to_string(), "Video/Source".to_string()));
        assert!(s.done());
        assert!(p.done());
    }

    #[test]
    fn skips_unknown_pods_cleanly() {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.pod(0x42, |b| b.buf.extend_from_slice(&[1, 2, 3]));
            b.int(9);
        });
        let mut p = PodParser::new(&b.buf);
        let mut s = p.struct_().unwrap();
        s.skip().unwrap();
        assert_eq!(s.int().unwrap(), 9);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut b = PodBuilder::default();
        b.int(1);
        let mut p = PodParser::new(&b.buf[..6]);
        assert!(matches!(p.int(), Err(PodError::Truncated)));
    }
}
