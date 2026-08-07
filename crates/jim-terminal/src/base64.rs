//! Minimal standard-alphabet base64 decoder (no deps).
//!
//! Shared by the two OSC watchers that carry base64 payloads: OSC 133
//! (`crate::command_watch`, the shell shim base64s the command line) and
//! OSC 52 (`crate::osc52`, clipboard writes).

/// Decode a standard-alphabet (`+`/`/`) base64 string. Padding is
/// optional, ASCII whitespace is ignored, and any other byte makes the
/// whole decode fail (`None`).
pub fn decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut quad = [0u8; 4];
    let mut n = 0;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        quad[n] = val(c)?;
        n += 1;
        if n == 4 {
            out.push((quad[0] << 2) | (quad[1] >> 4));
            out.push((quad[1] << 4) | (quad[2] >> 2));
            out.push((quad[2] << 6) | quad[3]);
            n = 0;
        }
    }
    match n {
        0 => {}
        2 => out.push((quad[0] << 2) | (quad[1] >> 4)),
        3 => {
            out.push((quad[0] << 2) | (quad[1] >> 4));
            out.push((quad[1] << 4) | (quad[2] >> 2));
        }
        _ => return None, // n == 1 is impossible for valid base64
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_numbers() {
        assert_eq!(decode("Y2FyZ28gdGVzdA==").unwrap(), b"cargo test");
        assert_eq!(decode("aGk=").unwrap(), b"hi");
        assert_eq!(decode("aGk").unwrap(), b"hi"); // padding optional
        assert_eq!(decode("").unwrap(), b"");
    }

    #[test]
    fn rejects_junk() {
        assert!(decode("not base64!").is_none());
    }

    #[test]
    fn ignores_whitespace() {
        assert_eq!(decode("Y2Fy\n Z28g\tdGVzdA==").unwrap(), b"cargo test");
    }
}
