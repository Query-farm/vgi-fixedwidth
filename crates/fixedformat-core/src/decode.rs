//! Decode record bytes into the neutral [`Value`] tree, driven by a [`Layout`].
//!
//! Offsets are interpreted **relative to the parent**: top-level field offsets
//! are relative to the record start; group-child offsets are relative to the
//! group occurrence. OCCURS fields decode into a [`Value::List`]; group items
//! decode into a [`Value::Struct`]. [`crate::encode`] is the exact inverse.

use std::collections::HashMap;

use crate::ebcdic;
use crate::layout::{Endian, Field, FieldKind, Justify, Layout, NumRepr, SignKind};
use crate::value::Value;
use crate::{packed, zoned, Encoding, Error, Result};

/// Decoded integer values keyed by UPPERCASE field name, used to resolve
/// `OCCURS … DEPENDING ON` controlling fields (which decode earlier).
type Scope = HashMap<String, i64>;

/// Decode all top-level fields of a record into `(name, value)` pairs. Pad
/// fields are consumed but omitted from the output.
pub fn decode_record(layout: &Layout, bytes: &[u8], enc: Encoding) -> Result<Vec<(String, Value)>> {
    layout.check_record_len(bytes.len())?;
    let mut scope = Scope::new();
    let (pairs, _consumed) = decode_seq(&layout.fields, 0, bytes, enc, &mut scope)?;
    Ok(pairs)
}

/// Decode a sibling field list positioned relative to `base`, returning the
/// non-pad `(name, value)` pairs and the bytes consumed past `base`.
///
/// Addressing is `base + field.offset + shift`, where `shift` accumulates the
/// body size of any `OCCURS … DEPENDING ON` table (which reserves no static
/// footprint) so following siblings land after it. REDEFINES variants share an
/// offset and thus overlap naturally; the consumed width is the furthest field
/// end, which is the max for an overlapping union and the sum for a sequence.
fn decode_seq(
    fields: &[Field],
    base: usize,
    bytes: &[u8],
    enc: Encoding,
    scope: &mut Scope,
) -> Result<(Vec<(String, Value)>, usize)> {
    let mut out = Vec::with_capacity(fields.len());
    let mut shift = 0usize;
    let mut end = base;
    for field in fields {
        let at = base + field.offset + shift;
        let (value, consumed) = decode_field(field, at, bytes, enc, scope)?;
        shift += consumed.saturating_sub(field.reserved_width());
        end = end.max(at + consumed);
        if matches!(field.kind, FieldKind::Pad { .. }) {
            continue;
        }
        // Record scalar integers so a later OCCURS … DEPENDING ON can find its
        // controlling field by name (COBOL names are case-insensitive).
        if let Value::Int(n) = value {
            scope.insert(field.name.to_ascii_uppercase(), n);
        }
        out.push((field.name.clone(), value));
    }
    Ok((out, end - base))
}

/// Decode a single field (handling OCCURS / OCCURS DEPENDING ON) at absolute
/// `at`, returning its value and the bytes it consumed.
fn decode_field(
    field: &Field,
    at: usize,
    bytes: &[u8],
    enc: Encoding,
    scope: &mut Scope,
) -> Result<(Value, usize)> {
    // The element count: a runtime value for OCCURS DEPENDING ON, else the fixed
    // OCCURS count, else a single (non-list) occurrence.
    let count = match &field.depending_on {
        Some(ctrl) => {
            let n = *scope.get(&ctrl.to_ascii_uppercase()).ok_or_else(|| {
                Error(format!(
                    "OCCURS DEPENDING ON {ctrl}: controlling field not decoded before the table"
                ))
            })?;
            if n < 0 {
                return Err(Error(format!(
                    "OCCURS DEPENDING ON {ctrl}: negative count {n}"
                )));
            }
            if let Some(max) = field.occurs {
                if n as usize > max {
                    return Err(Error(format!(
                        "OCCURS DEPENDING ON {ctrl}: count {n} exceeds maximum {max}"
                    )));
                }
            }
            Some(n as usize)
        }
        None => field.occurs,
    };

    match count {
        None => decode_one(field, at, bytes, enc, scope),
        Some(n) => {
            let mut items = Vec::with_capacity(n);
            let mut cursor = at;
            for _ in 0..n {
                let (v, consumed) = decode_one(field, cursor, bytes, enc, scope)?;
                cursor += consumed;
                items.push(v);
            }
            Ok((Value::List(items), cursor - at))
        }
    }
}

/// Decode one occurrence of `field` whose bytes start at absolute `at`,
/// returning its value and the bytes it consumed.
fn decode_one(
    field: &Field,
    at: usize,
    bytes: &[u8],
    enc: Encoding,
    scope: &mut Scope,
) -> Result<(Value, usize)> {
    if let FieldKind::Group(children) = &field.kind {
        let (pairs, consumed) = decode_seq(children, at, bytes, enc, scope)?;
        return Ok((Value::Struct(pairs), consumed));
    }
    let slice = slice(bytes, at, field.width)?;
    let value = match &field.kind {
        FieldKind::Group(_) => unreachable!("handled above"),
        FieldKind::Text { justify, trim, pad } => {
            let ascii = to_ascii(slice, enc);
            let s = String::from_utf8_lossy(&ascii);
            let trimmed = if *trim {
                trim_pad(&s, *justify, *pad as char)
            } else {
                s.into_owned()
            };
            Value::Text(trimmed)
        }
        FieldKind::Int { signed, sign } => {
            let ascii = to_ascii(slice, enc);
            Value::Int(parse_display_int(&ascii, *signed, *sign)?)
        }
        FieldKind::Binary { endian, signed } => Value::Int(parse_binary_int(slice, *endian, *signed)?),
        FieldKind::Float { bits, endian } => Value::Float(parse_float(slice, *bits, *endian)?),
        FieldKind::Hex { order } => Value::Text(to_hex(slice, *order)),
        FieldKind::Bool => Value::Bool(slice.iter().any(|&b| b != 0 && b != b'0')),
        FieldKind::Pad { .. } => Value::Null,
        FieldKind::Decimal {
            precision: _,
            scale,
            repr,
            sign,
        } => {
            let unscaled = match repr {
                NumRepr::Comp3 => packed::decode(slice)?,
                NumRepr::Zoned => zoned::decode(&to_ascii(slice, enc))?,
                NumRepr::Display => parse_display_decimal(&to_ascii(slice, enc), *sign)?,
            };
            Value::Decimal {
                unscaled,
                scale: *scale,
            }
        }
    };
    Ok((value, field.width))
}

fn slice(bytes: &[u8], at: usize, width: usize) -> Result<&[u8]> {
    bytes.get(at..at + width).ok_or_else(|| {
        Error(format!(
            "field at offset {at} (+{width}) overruns the record"
        ))
    })
}

/// Normalize a field's bytes to ASCII (transcoding from EBCDIC when needed).
fn to_ascii(slice: &[u8], enc: Encoding) -> Vec<u8> {
    match enc {
        Encoding::Ascii => slice.to_vec(),
        Encoding::Ebcdic => ebcdic::decode_slice(slice),
    }
}

fn trim_pad(s: &str, justify: Justify, pad: char) -> String {
    match justify {
        Justify::Left => s.trim_end_matches(pad).to_string(),
        Justify::Right => s.trim_start_matches(pad).to_string(),
    }
    // Null-terminated strings additionally drop everything past the first NUL.
    .split('\0')
    .next()
    .unwrap_or("")
    .to_string()
}

fn parse_display_int(ascii: &[u8], _signed: bool, sign: SignKind) -> Result<i64> {
    let s = String::from_utf8_lossy(ascii);
    let (digits, negative) = strip_separate_sign(s.trim(), sign)?;
    let digits = digits.trim();
    let mag: i64 = if digits.is_empty() {
        0
    } else {
        digits
            .parse()
            .map_err(|_| Error(format!("invalid numeric field: {digits:?}")))?
    };
    Ok(if negative { -mag } else { mag })
}

fn parse_display_decimal(ascii: &[u8], sign: SignKind) -> Result<i128> {
    let s = String::from_utf8_lossy(ascii);
    let (digits, negative) = strip_separate_sign(s.trim(), sign)?;
    let digits: String = digits.chars().filter(|c| c.is_ascii_digit()).collect();
    let mag: i128 = if digits.is_empty() {
        0
    } else {
        digits
            .parse()
            .map_err(|_| Error(format!("invalid decimal field: {digits:?}")))?
    };
    Ok(if negative { -mag } else { mag })
}

/// Strip a leading/trailing separate sign byte, returning (digits, negative).
fn strip_separate_sign(s: &str, sign: SignKind) -> Result<(String, bool)> {
    match sign {
        SignKind::LeadingSeparate => {
            let s = s.trim_start();
            if let Some(rest) = s.strip_prefix('-') {
                Ok((rest.to_string(), true))
            } else if let Some(rest) = s.strip_prefix('+') {
                Ok((rest.to_string(), false))
            } else {
                Ok((s.to_string(), false))
            }
        }
        SignKind::TrailingSeparate => {
            let s = s.trim_end();
            if let Some(rest) = s.strip_suffix('-') {
                Ok((rest.to_string(), true))
            } else if let Some(rest) = s.strip_suffix('+') {
                Ok((rest.to_string(), false))
            } else {
                Ok((s.to_string(), false))
            }
        }
        SignKind::Unsigned | SignKind::Embedded => Ok((s.to_string(), false)),
    }
}

fn parse_binary_int(slice: &[u8], endian: Endian, signed: bool) -> Result<i64> {
    let mut buf = [0u8; 16];
    let n = slice.len();
    if n == 0 || n > 8 {
        return Err(Error(format!("binary int width {n} not in 1..=8")));
    }
    // Place bytes into a 16-byte buffer as big-endian, then read i128.
    let normalized: Vec<u8> = match endian {
        Endian::Big => slice.to_vec(),
        Endian::Little => slice.iter().rev().copied().collect(),
    };
    buf[16 - n..].copy_from_slice(&normalized);
    let raw = i128::from_be_bytes(buf); // always non-negative here (high bytes 0)
    let value = if signed && normalized[0] & 0x80 != 0 {
        // Sign-extend: subtract 2^(8n).
        raw - (1i128 << (8 * n))
    } else {
        raw
    };
    Ok(value as i64)
}

fn parse_float(slice: &[u8], bits: u8, endian: Endian) -> Result<f64> {
    let ordered: Vec<u8> = match endian {
        Endian::Big => slice.to_vec(),
        Endian::Little => slice.iter().rev().copied().collect(),
    };
    match bits {
        32 => {
            let arr: [u8; 4] = ordered
                .as_slice()
                .try_into()
                .map_err(|_| Error("float32 needs 4 bytes".into()))?;
            Ok(f32::from_be_bytes(arr) as f64)
        }
        64 => {
            let arr: [u8; 8] = ordered
                .as_slice()
                .try_into()
                .map_err(|_| Error("float64 needs 8 bytes".into()))?;
            Ok(f64::from_be_bytes(arr))
        }
        16 => {
            let arr: [u8; 2] = ordered
                .as_slice()
                .try_into()
                .map_err(|_| Error("float16 needs 2 bytes".into()))?;
            Ok(f16_to_f64(u16::from_be_bytes(arr)))
        }
        other => Err(Error(format!("unsupported float width {other} bits"))),
    }
}

/// Decode an IEEE-754 half-precision value to f64 (no external crate).
fn f16_to_f64(bits: u16) -> f64 {
    let sign = (bits >> 15) & 1;
    let exp = (bits >> 10) & 0x1F;
    let frac = bits & 0x3FF;
    let val = if exp == 0 {
        // Subnormal / zero.
        (frac as f64) * 2f64.powi(-24)
    } else if exp == 0x1F {
        if frac == 0 {
            f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        (1.0 + (frac as f64) / 1024.0) * 2f64.powi(exp as i32 - 15)
    };
    if sign == 1 {
        -val
    } else {
        val
    }
}

fn to_hex(slice: &[u8], order: Endian) -> String {
    let mut s = String::with_capacity(slice.len() * 2);
    for &b in slice {
        match order {
            Endian::Big => {
                s.push(nibble_hex(b >> 4));
                s.push(nibble_hex(b & 0x0F));
            }
            Endian::Little => {
                s.push(nibble_hex(b & 0x0F));
                s.push(nibble_hex(b >> 4));
            }
        }
    }
    s
}

fn nibble_hex(n: u8) -> char {
    char::from_digit(n as u32, 16).unwrap_or('0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Field, FieldKind, Justify, SignKind};

    fn text_field(name: &str, offset: usize, width: usize) -> Field {
        Field {
            name: name.into(),
            offset,
            width,
            kind: FieldKind::Text {
                justify: Justify::Left,
                trim: true,
                pad: b' ',
            },
            occurs: None,
            depending_on: None,
            redefines: None,
        }
    }

    #[test]
    fn decodes_text_and_display_int() {
        let layout = Layout::from_fields(vec![
            text_field("name", 0, 10),
            Field {
                name: "qty".into(),
                offset: 10,
                width: 5,
                kind: FieldKind::Int {
                    signed: false,
                    sign: SignKind::Unsigned,
                },
                occurs: None,
                depending_on: None,
                redefines: None,
            },
        ])
        .unwrap();
        let out = decode_record(&layout, b"JOHN      00042", Encoding::Ascii).unwrap();
        assert_eq!(out[0], ("name".into(), Value::Text("JOHN".into())));
        assert_eq!(out[1], ("qty".into(), Value::Int(42)));
    }

    #[test]
    fn binary_int_little_and_big() {
        // 0x0102 BE = 258; LE bytes 0x02 0x01 = 258.
        let be = parse_binary_int(&[0x01, 0x02], Endian::Big, false).unwrap();
        let le = parse_binary_int(&[0x02, 0x01], Endian::Little, false).unwrap();
        assert_eq!(be, 258);
        assert_eq!(le, 258);
    }

    #[test]
    fn binary_int_signed_negative() {
        // 0xFF (int8) = -1.
        assert_eq!(parse_binary_int(&[0xFF], Endian::Big, true).unwrap(), -1);
        assert_eq!(parse_binary_int(&[0xFF], Endian::Big, false).unwrap(), 255);
    }

    #[test]
    fn float64_round() {
        let v = 3.5f64.to_be_bytes();
        assert_eq!(parse_float(&v, 64, Endian::Big).unwrap(), 3.5);
        let v = 3.5f64.to_le_bytes();
        assert_eq!(parse_float(&v, 64, Endian::Little).unwrap(), 3.5);
    }

    #[test]
    fn float16_one() {
        // 1.0 in half precision = 0x3C00.
        assert_eq!(f16_to_f64(0x3C00), 1.0);
        assert_eq!(f16_to_f64(0xC000), -2.0);
    }

    #[test]
    fn hex_orders() {
        assert_eq!(to_hex(&[0xAB, 0xCD], Endian::Big), "abcd");
        assert_eq!(to_hex(&[0xAB, 0xCD], Endian::Little), "badc");
    }

    #[test]
    fn comp3_decimal_field() {
        let layout = Layout::from_fields(vec![Field {
            name: "amt".into(),
            offset: 0,
            width: 3,
            kind: FieldKind::Decimal {
                precision: 5,
                scale: 2,
                repr: NumRepr::Comp3,
                sign: SignKind::Embedded,
            },
            occurs: None,
            depending_on: None,
            redefines: None,
        }])
        .unwrap();
        let out = decode_record(&layout, &[0x12, 0x34, 0x5C], Encoding::Ascii).unwrap();
        assert_eq!(
            out[0].1,
            Value::Decimal {
                unscaled: 12345,
                scale: 2
            }
        );
    }

    #[test]
    fn occurs_makes_list() {
        let layout = Layout::from_fields(vec![Field {
            name: "lines".into(),
            offset: 0,
            width: 2,
            kind: FieldKind::Int {
                signed: false,
                sign: SignKind::Unsigned,
            },
            occurs: Some(3),
            depending_on: None,
            redefines: None,
        }])
        .unwrap();
        let out = decode_record(&layout, b"010203", Encoding::Ascii).unwrap();
        assert_eq!(
            out[0].1,
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
    }

    #[test]
    fn ebcdic_text() {
        let layout = Layout::from_fields(vec![text_field("name", 0, 5)]);
        let layout = layout.unwrap();
        let ebcdic = ebcdic::encode_slice(b"HELLO");
        let out = decode_record(&layout, &ebcdic, Encoding::Ebcdic).unwrap();
        assert_eq!(out[0].1, Value::Text("HELLO".into()));
    }
}
