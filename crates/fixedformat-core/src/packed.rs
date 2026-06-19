//! COBOL packed decimal (COMP-3 / COMPUTATIONAL-3).
//!
//! Two decimal digits per byte, most-significant first; the final low nibble is
//! the sign (`0xC`/`0xF`/`0xA`/`0xE` = positive, `0xD`/`0xB` = negative). A field
//! of `d` digits occupies `d / 2 + 1` bytes, so for an even digit count the
//! leading high nibble is an unused zero.

use crate::{Error, Result};

/// Bytes needed to hold `digits` packed decimal digits (plus the sign nibble).
pub fn byte_width(digits: u8) -> usize {
    digits as usize / 2 + 1
}

/// Digits representable in `width` packed bytes (the complement of [`byte_width`]
/// for the canonical even/odd layout): `width * 2 - 1`.
pub fn max_digits(width: usize) -> usize {
    width * 2 - 1
}

/// Decode COMP-3 bytes into an unscaled signed integer.
pub fn decode(bytes: &[u8]) -> Result<i128> {
    if bytes.is_empty() {
        return Err(Error("comp-3 field is empty".into()));
    }
    let mut value: i128 = 0;
    let nibbles = bytes.len() * 2;
    for i in 0..nibbles - 1 {
        let byte = bytes[i / 2];
        let nib = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
        if nib > 9 {
            return Err(Error(format!("invalid comp-3 digit nibble {nib:#x}")));
        }
        value = value * 10 + nib as i128;
    }
    let sign_nibble = bytes[bytes.len() - 1] & 0x0F;
    let negative = match sign_nibble {
        0xD | 0xB => true,
        0xC | 0xF | 0xA | 0xE => false,
        other => return Err(Error(format!("invalid comp-3 sign nibble {other:#x}"))),
    };
    Ok(if negative { -value } else { value })
}

/// Encode an unscaled signed integer into `width` COMP-3 bytes. The sign nibble
/// is `0xC` (positive) or `0xD` (negative); `signed = false` always writes the
/// unsigned-positive nibble `0xF`.
pub fn encode(value: i128, width: usize, signed: bool) -> Result<Vec<u8>> {
    if width == 0 {
        return Err(Error("comp-3 width must be > 0".into()));
    }
    let negative = value < 0;
    let mut digits = value.unsigned_abs();
    let nibbles = width * 2;
    let digit_count = nibbles - 1;

    // Fill digit nibbles right-to-left.
    let mut nib = vec![0u8; nibbles];
    for slot in (0..digit_count).rev() {
        nib[slot] = (digits % 10) as u8;
        digits /= 10;
    }
    if digits != 0 {
        return Err(Error(format!(
            "value {value} does not fit in {digit_count} packed digits"
        )));
    }
    nib[nibbles - 1] = if !signed {
        0x0F
    } else if negative {
        0x0D
    } else {
        0x0C
    };

    let mut out = vec![0u8; width];
    for (i, n) in nib.iter().enumerate() {
        if i % 2 == 0 {
            out[i / 2] |= n << 4;
        } else {
            out[i / 2] |= n & 0x0F;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_widths() {
        // PIC S9(3) COMP-3 -> 3 digits -> 2 bytes; S9(7)V99 -> 9 digits -> 5 bytes.
        assert_eq!(byte_width(3), 2);
        assert_eq!(byte_width(9), 5);
        assert_eq!(byte_width(1), 1);
        assert_eq!(byte_width(2), 2);
    }

    #[test]
    fn decodes_positive() {
        // 123.45 stored as S9(3)V99 COMP-3 (5 digits, 3 bytes): 0x12 0x34 0x5C.
        assert_eq!(decode(&[0x12, 0x34, 0x5C]).unwrap(), 12345);
    }

    #[test]
    fn decodes_negative() {
        assert_eq!(decode(&[0x12, 0x34, 0x5D]).unwrap(), -12345);
    }

    #[test]
    fn decodes_unsigned_f_nibble() {
        assert_eq!(decode(&[0x00, 0x12, 0x3F]).unwrap(), 123);
    }

    #[test]
    fn encode_round_trips() {
        for &(v, w, signed) in &[
            (12345i128, 3usize, true),
            (-12345, 3, true),
            (123, 2, false),
            (0, 1, true),
            (-1, 1, true),
            (9999999999i128, 6, true),
        ] {
            let bytes = encode(v, w, signed).unwrap();
            assert_eq!(bytes.len(), w);
            let expect = if signed { v } else { v.abs() };
            assert_eq!(decode(&bytes).unwrap(), expect, "v={v} w={w}");
        }
    }

    #[test]
    fn encode_matches_known_bytes() {
        assert_eq!(encode(12345, 3, true).unwrap(), vec![0x12, 0x34, 0x5C]);
        assert_eq!(encode(-12345, 3, true).unwrap(), vec![0x12, 0x34, 0x5D]);
    }

    #[test]
    fn overflow_errors() {
        assert!(encode(123456, 2, true).is_err());
    }
}
