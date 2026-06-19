//! Zoned decimal with an overpunch sign.
//!
//! Each digit is one display byte (`'0'..'9'`); the last byte's high nibble (the
//! "zone") encodes the sign of the whole number. Working on already-ASCII bytes,
//! the conventional overpunch character set is:
//!
//! | digit | positive | negative |
//! |-------|----------|----------|
//! | 0     | `{`      | `}`      |
//! | 1..9  | `A..I`   | `J..R`   |
//!
//! Plain `'0'..'9'` in the last byte is treated as unsigned/positive.

use crate::{Error, Result};

/// Decode `width` zoned-decimal bytes into an unscaled signed integer.
pub fn decode(bytes: &[u8]) -> Result<i128> {
    if bytes.is_empty() {
        return Err(Error("zoned field is empty".into()));
    }
    let mut value: i128 = 0;
    for &b in &bytes[..bytes.len() - 1] {
        let d = digit(b)?;
        value = value * 10 + d as i128;
    }
    let (last_digit, negative) = decode_overpunch(bytes[bytes.len() - 1])?;
    value = value * 10 + last_digit as i128;
    Ok(if negative { -value } else { value })
}

/// Encode an unscaled signed integer into `width` zoned-decimal bytes. When
/// `signed` is false the trailing byte is a plain ASCII digit.
pub fn encode(value: i128, width: usize, signed: bool) -> Result<Vec<u8>> {
    if width == 0 {
        return Err(Error("zoned width must be > 0".into()));
    }
    let negative = value < 0;
    let mut digits = value.unsigned_abs();
    let mut out = vec![0u8; width];
    for slot in (0..width).rev() {
        let d = (digits % 10) as u8;
        digits /= 10;
        out[slot] = if slot == width - 1 && signed {
            encode_overpunch(d, negative)
        } else {
            b'0' + d
        };
    }
    if digits != 0 {
        return Err(Error(format!(
            "value {value} does not fit in {width} zoned digits"
        )));
    }
    Ok(out)
}

fn digit(b: u8) -> Result<u8> {
    if b.is_ascii_digit() {
        Ok(b - b'0')
    } else {
        Err(Error(format!("invalid zoned digit byte {b:#x}")))
    }
}

/// Decode an overpunched last byte → (digit, negative).
fn decode_overpunch(b: u8) -> Result<(u8, bool)> {
    match b {
        b'0'..=b'9' => Ok((b - b'0', false)),
        b'{' => Ok((0, false)),
        b'}' => Ok((0, true)),
        b'A'..=b'I' => Ok((b - b'A' + 1, false)),
        b'J'..=b'R' => Ok((b - b'J' + 1, true)),
        other => Err(Error(format!("invalid zoned overpunch byte {other:#x}"))),
    }
}

/// Encode (digit, negative) → an overpunched byte.
fn encode_overpunch(digit: u8, negative: bool) -> u8 {
    match (digit, negative) {
        (0, false) => b'{',
        (0, true) => b'}',
        (d, false) => b'A' + (d - 1),
        (d, true) => b'J' + (d - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_positive_overpunch() {
        // 12345 positive: last digit 5 overpunched as 'E'.
        assert_eq!(decode(b"1234E").unwrap(), 12345);
    }

    #[test]
    fn decodes_negative_overpunch() {
        // -12345: last digit 5 negative overpunched as 'N'.
        assert_eq!(decode(b"1234N").unwrap(), -12345);
    }

    #[test]
    fn decodes_zero_overpunch() {
        assert_eq!(decode(b"123{").unwrap(), 1230);
        assert_eq!(decode(b"123}").unwrap(), -1230);
    }

    #[test]
    fn decodes_plain_digits_as_positive() {
        assert_eq!(decode(b"12345").unwrap(), 12345);
    }

    #[test]
    fn encode_round_trips() {
        for &(v, w) in &[(12345i128, 5usize), (-12345, 5), (0, 3), (-7, 1), (1230, 4)] {
            let bytes = encode(v, w, true).unwrap();
            assert_eq!(bytes.len(), w);
            assert_eq!(decode(&bytes).unwrap(), v, "v={v} w={w}");
        }
    }

    #[test]
    fn encode_known_bytes() {
        assert_eq!(encode(12345, 5, true).unwrap(), b"1234E".to_vec());
        assert_eq!(encode(-12345, 5, true).unwrap(), b"1234N".to_vec());
        assert_eq!(encode(12345, 5, false).unwrap(), b"12345".to_vec());
    }
}
