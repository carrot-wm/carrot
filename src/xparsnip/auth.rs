// xauthority parsing - find the MIT-MAGIC-COOKIE for our display, if any.
// only the probe path reads this; the wm socketpair authenticates by trust.

fn be16(b: &[u8], o: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]) as usize)
}

// records are family u16, then four length-prefixed fields: address,
// display number, auth name, auth data - all lengths big endian
pub fn cookie_for_display(display: u32) -> Option<Vec<u8>> {
    let path = std::env::var_os("XAUTHORITY")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".Xauthority"))
        })?;
    let data = std::fs::read(path).ok()?;
    let want = display.to_string();
    let mut o = 0;
    while o + 2 <= data.len() {
        o += 2;
        let mut field = |data: &[u8]| -> Option<(usize, usize)> {
            let len = be16(data, o)?;
            let start = o + 2;
            o = start + len;
            Some((start, len))
        };
        let _addr = field(&data)?;
        let (ns, nl) = field(&data)?;
        let (an, al) = field(&data)?;
        let (ds, dl) = field(&data)?;
        let number = &data[ns..ns + nl];
        let name = &data[an..an + al];
        if name == b"MIT-MAGIC-COOKIE-1" && (number.is_empty() || number == want.as_bytes()) {
            return Some(data[ds..ds + dl].to_vec());
        }
    }
    None
}
