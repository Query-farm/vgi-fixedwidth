//! Encode a [`Value`] tree back into record bytes, the exact inverse of
//! [`crate::decode`]. Used by `pack` / `write_fixed`.
//!
//! The record buffer is sized to the layout and zero-initialized; each field
//! writes its full (padded) width at its offset. For a folded REDEFINES group
//! only the first (base) child is written — overlapping variants cannot all
//! occupy the same bytes.

use crate::ebcdic;
use crate::layout::{Endian, Field, FieldKind, Justify, Layout, NumRepr, SignKind};
use crate::value::Value;
use crate::{packed, zoned, Encoding, Error, Result};

/// Encode a record from `(name, value)` pairs. The buffer starts at the layout's
/// static length and grows as needed for `OCCURS … DEPENDING ON` bodies.
pub fn encode_record(
    layout: &Layout,
    values: &[(String, Value)],
    enc: Encoding,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; layout.record_len];
    encode_seq(&layout.fields, 0, &mut buf, values, enc)?;
    Ok(buf)
}

fn lookup<'a>(values: &'a [(String, Value)], name: &str) -> &'a Value {
    values
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
        .unwrap_or(&Value::Null)
}

/// Encode a sibling field list at `base`, returning bytes consumed past `base`.
/// Mirrors `decode::decode_seq`: addressing is `base + offset + shift`, where
/// `shift` accumulates each OCCURS DEPENDING ON body (which reserves no static
/// footprint). REDEFINES variants share an offset and overlap (last write wins).
fn encode_seq(
    fields: &[Field],
    base: usize,
    buf: &mut Vec<u8>,
    values: &[(String, Value)],
    enc: Encoding,
) -> Result<usize> {
    let mut shift = 0usize;
    let mut end = base;
    for field in fields {
        let at = base + field.offset + shift;
        let value = lookup(values, &field.name);
        let consumed = encode_field(field, at, buf, value, enc)?;
        shift += consumed.saturating_sub(field.reserved_width());
        end = end.max(at + consumed);
    }
    Ok(end - base)
}

/// Encode a single field (handling OCCURS / OCCURS DEPENDING ON) at `at`,
/// returning the bytes it consumed. For a DEPENDING ON table the element count
/// is the length of the supplied list (the controlling field is encoded from its
/// own value, which the producer is responsible for keeping consistent).
fn encode_field(
    field: &Field,
    at: usize,
    buf: &mut Vec<u8>,
    value: &Value,
    enc: Encoding,
) -> Result<usize> {
    let count = if field.depending_on.is_some() {
        match value {
            Value::List(items) => Some(items.len()),
            Value::Null => Some(0),
            other => {
                return Err(Error(format!(
                    "field {} expects a list, got {other:?}",
                    field.name
                )))
            }
        }
    } else {
        field.occurs
    };

    match count {
        None => encode_one(field, at, buf, value, enc),
        Some(n) => {
            let items: Vec<Value> = match value {
                Value::List(items) => items.clone(),
                Value::Null => vec![Value::Null; n],
                other => {
                    return Err(Error(format!(
                        "field {} expects a list, got {other:?}",
                        field.name
                    )))
                }
            };
            let mut cursor = at;
            for i in 0..n {
                let item = items.get(i).unwrap_or(&Value::Null);
                cursor += encode_one(field, cursor, buf, item, enc)?;
            }
            Ok(cursor - at)
        }
    }
}

fn encode_one(
    field: &Field,
    at: usize,
    buf: &mut Vec<u8>,
    value: &Value,
    enc: Encoding,
) -> Result<usize> {
    let width = field.width;
    match &field.kind {
        FieldKind::Group(children) => {
            let fields = match value {
                Value::Struct(f) => f.clone(),
                Value::Null => Vec::new(),
                other => {
                    return Err(Error(format!(
                        "field {} expects a struct, got {other:?}",
                        field.name
                    )))
                }
            };
            // Children are written by `encode_seq`; for a REDEFINES union every
            // variant shares offset 0, so they overlap and the last write wins.
            encode_seq(children, at, buf, &fields, enc)
        }
        FieldKind::Text { justify, pad, .. } => {
            let s = match value {
                Value::Text(s) => s.clone(),
                Value::Null => String::new(),
                other => display_scalar(other),
            };
            let bytes = justify_pad(s.as_bytes(), width, *justify, *pad)?;
            put(buf, at, &maybe_ebcdic(&bytes, enc));
            Ok(width)
        }
        FieldKind::Int { signed, sign } => {
            let n = as_i128(value)?;
            let bytes = encode_display_int(n, width, *signed, *sign)?;
            put(buf, at, &maybe_ebcdic(&bytes, enc));
            Ok(width)
        }
        FieldKind::Binary { endian, signed } => {
            let n = as_i128(value)?;
            put(buf, at, &encode_binary_int(n, width, *endian, *signed)?);
            Ok(width)
        }
        FieldKind::Float { bits, endian } => {
            let f = as_f64(value)?;
            put(buf, at, &encode_float(f, *bits, *endian)?);
            Ok(width)
        }
        FieldKind::Hex { order } => {
            let s = match value {
                Value::Text(s) => s.clone(),
                Value::Null => String::new(),
                other => return Err(Error(format!("hex field expects text, got {other:?}"))),
            };
            put(buf, at, &encode_hex(&s, width, *order)?);
            Ok(width)
        }
        FieldKind::Bool => {
            let b = matches!(value, Value::Bool(true) | Value::Int(1));
            put(buf, at, &[if b { 1 } else { 0 }]);
            Ok(width)
        }
        FieldKind::Pad { pad } => {
            fill(buf, at, width, *pad, enc);
            Ok(width)
        }
        FieldKind::Decimal {
            precision,
            scale,
            repr,
            sign,
        } => {
            let unscaled = as_decimal(value, *scale)?;
            let signed = !matches!(sign, SignKind::Unsigned);
            let bytes = match repr {
                NumRepr::Comp3 => packed::encode(unscaled, width, signed)?,
                NumRepr::Zoned => maybe_ebcdic(&zoned::encode(unscaled, width, signed)?, enc),
                NumRepr::Display => maybe_ebcdic(
                    &encode_display_decimal(unscaled, *precision, width, *sign)?,
                    enc,
                ),
            };
            put(buf, at, &bytes);
            Ok(width)
        }
    }
}

fn maybe_ebcdic(bytes: &[u8], enc: Encoding) -> Vec<u8> {
    match enc {
        Encoding::Ascii => bytes.to_vec(),
        Encoding::Ebcdic => ebcdic::encode_slice(bytes),
    }
}

/// Grow `buf` with zero bytes so index `end` is writable.
fn ensure(buf: &mut Vec<u8>, end: usize) {
    if buf.len() < end {
        buf.resize(end, 0);
    }
}

fn put(buf: &mut Vec<u8>, at: usize, bytes: &[u8]) {
    ensure(buf, at + bytes.len());
    buf[at..at + bytes.len()].copy_from_slice(bytes);
}

fn fill(buf: &mut Vec<u8>, at: usize, width: usize, pad: u8, enc: Encoding) {
    let b = match enc {
        Encoding::Ascii => pad,
        Encoding::Ebcdic => ebcdic::to_ebcdic(pad),
    };
    ensure(buf, at + width);
    for slot in &mut buf[at..at + width] {
        *slot = b;
    }
}

fn justify_pad(bytes: &[u8], width: usize, justify: Justify, pad: u8) -> Result<Vec<u8>> {
    if bytes.len() > width {
        return Err(Error(format!(
            "text value of {} bytes does not fit in field width {width}",
            bytes.len()
        )));
    }
    let mut out = vec![pad; width];
    match justify {
        Justify::Left => out[..bytes.len()].copy_from_slice(bytes),
        Justify::Right => out[width - bytes.len()..].copy_from_slice(bytes),
    }
    Ok(out)
}

fn display_scalar(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Decimal { unscaled, scale } => Value::decimal_string(*unscaled, *scale),
        _ => String::new(),
    }
}

fn as_i128(v: &Value) -> Result<i128> {
    match v {
        Value::Int(i) => Ok(*i as i128),
        Value::Decimal { unscaled, scale: 0 } => Ok(*unscaled),
        Value::Null => Ok(0),
        Value::Text(s) => s
            .trim()
            .parse()
            .map_err(|_| Error(format!("not an integer: {s:?}"))),
        other => Err(Error(format!("expected an integer, got {other:?}"))),
    }
}

fn as_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::Decimal { unscaled, scale } => Ok(*unscaled as f64 / 10f64.powi(*scale as i32)),
        Value::Null => Ok(0.0),
        other => Err(Error(format!("expected a float, got {other:?}"))),
    }
}

/// Coerce a value to an unscaled integer at the field's `scale`.
fn as_decimal(v: &Value, scale: u8) -> Result<i128> {
    match v {
        Value::Decimal { unscaled, scale: s } => rescale(*unscaled, *s, scale),
        Value::Int(i) => rescale(*i as i128, 0, scale),
        Value::Null => Ok(0),
        Value::Float(f) => Ok((*f * 10f64.powi(scale as i32)).round() as i128),
        other => Err(Error(format!("expected a decimal, got {other:?}"))),
    }
}

fn rescale(unscaled: i128, from: u8, to: u8) -> Result<i128> {
    use std::cmp::Ordering;
    Ok(match from.cmp(&to) {
        Ordering::Equal => unscaled,
        Ordering::Less => unscaled * 10i128.pow((to - from) as u32),
        Ordering::Greater => unscaled / 10i128.pow((from - to) as u32),
    })
}

fn encode_display_int(n: i128, width: usize, signed: bool, sign: SignKind) -> Result<Vec<u8>> {
    let negative = n < 0;
    let mag = n.unsigned_abs().to_string();
    match sign {
        SignKind::LeadingSeparate => {
            let digits = width
                .checked_sub(1)
                .ok_or_else(|| Error("width too small for sign".into()))?;
            let body = zero_pad(&mag, digits)?;
            let mut out = vec![if negative { b'-' } else { b'+' }];
            out.extend_from_slice(body.as_bytes());
            Ok(out)
        }
        SignKind::TrailingSeparate => {
            let digits = width
                .checked_sub(1)
                .ok_or_else(|| Error("width too small for sign".into()))?;
            let body = zero_pad(&mag, digits)?;
            let mut out = body.into_bytes();
            out.push(if negative { b'-' } else { b'+' });
            Ok(out)
        }
        SignKind::Unsigned | SignKind::Embedded => {
            if negative && !signed {
                return Err(Error("negative value in an unsigned field".into()));
            }
            Ok(zero_pad(&mag, width)?.into_bytes())
        }
    }
}

fn encode_display_decimal(
    unscaled: i128,
    precision: u8,
    width: usize,
    sign: SignKind,
) -> Result<Vec<u8>> {
    let signed = !matches!(sign, SignKind::Unsigned);
    encode_display_int(unscaled, width.max(precision as usize), signed, sign).and_then(|b| {
        if b.len() > width {
            Err(Error(format!(
                "decimal value does not fit in field width {width}"
            )))
        } else {
            Ok(b)
        }
    })
}

fn zero_pad(mag: &str, width: usize) -> Result<String> {
    if mag.len() > width {
        return Err(Error(format!(
            "numeric value '{mag}' does not fit in {width} digits"
        )));
    }
    Ok(format!("{}{}", "0".repeat(width - mag.len()), mag))
}

fn encode_binary_int(n: i128, width: usize, endian: Endian, _signed: bool) -> Result<Vec<u8>> {
    if width == 0 || width > 8 {
        return Err(Error(format!("binary int width {width} not in 1..=8")));
    }
    let be = n.to_be_bytes(); // 16 bytes
    let bytes = &be[16 - width..];
    Ok(match endian {
        Endian::Big => bytes.to_vec(),
        Endian::Little => bytes.iter().rev().copied().collect(),
    })
}

fn encode_float(f: f64, bits: u8, endian: Endian) -> Result<Vec<u8>> {
    let be: Vec<u8> = match bits {
        32 => (f as f32).to_be_bytes().to_vec(),
        64 => f.to_be_bytes().to_vec(),
        16 => f64_to_f16(f).to_be_bytes().to_vec(),
        other => return Err(Error(format!("unsupported float width {other} bits"))),
    };
    Ok(match endian {
        Endian::Big => be,
        Endian::Little => be.into_iter().rev().collect(),
    })
}

/// Encode an f64 to IEEE-754 half precision (round to nearest, ties handled by
/// f32 rounding; adequate for the practical range).
fn f64_to_f16(value: f64) -> u16 {
    let f = value as f32;
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mant = bits & 0x7FFFFF;
    if exp <= 0 {
        sign
    } else if exp >= 0x1F {
        sign | 0x7C00
    } else {
        sign | ((exp as u16) << 10) | ((mant >> 13) as u16)
    }
}

fn encode_hex(s: &str, width: usize, order: Endian) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(Error(
            "hex string must have an even number of digits".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(clean.len() / 2);
    let chars: Vec<char> = clean.chars().collect();
    for pair in chars.chunks(2) {
        let hi = pair[0]
            .to_digit(16)
            .ok_or_else(|| Error(format!("bad hex digit {:?}", pair[0])))? as u8;
        let lo = pair[1]
            .to_digit(16)
            .ok_or_else(|| Error(format!("bad hex digit {:?}", pair[1])))? as u8;
        bytes.push(match order {
            Endian::Big => (hi << 4) | lo,
            Endian::Little => (lo << 4) | hi,
        });
    }
    if bytes.len() > width {
        return Err(Error(format!(
            "hex value of {} bytes exceeds field width {width}",
            bytes.len()
        )));
    }
    bytes.resize(width, 0);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_record;

    fn rt(layout: &Layout, values: Vec<(String, Value)>, enc: Encoding) -> Vec<(String, Value)> {
        let bytes = encode_record(layout, &values, enc).unwrap();
        decode_record(layout, &bytes, enc).unwrap()
    }

    fn field(name: &str, offset: usize, width: usize, kind: FieldKind) -> Field {
        Field {
            name: name.into(),
            offset,
            width,
            kind,
            occurs: None,
            depending_on: None,
            redefines: None,
        }
    }

    #[test]
    fn text_and_int_round_trip() {
        let layout = Layout::from_fields(vec![
            field(
                "name",
                0,
                10,
                FieldKind::Text {
                    justify: Justify::Left,
                    trim: true,
                    pad: b' ',
                },
            ),
            field(
                "qty",
                10,
                5,
                FieldKind::Int {
                    signed: false,
                    sign: SignKind::Unsigned,
                },
            ),
        ])
        .unwrap();
        let vals = vec![
            ("name".into(), Value::Text("JOHN".into())),
            ("qty".into(), Value::Int(42)),
        ];
        let bytes = encode_record(&layout, &vals, Encoding::Ascii).unwrap();
        assert_eq!(&bytes, b"JOHN      00042");
        assert_eq!(rt(&layout, vals.clone(), Encoding::Ascii), vals);
    }

    #[test]
    fn comp3_round_trip() {
        let layout = Layout::from_fields(vec![field(
            "amt",
            0,
            3,
            FieldKind::Decimal {
                precision: 5,
                scale: 2,
                repr: NumRepr::Comp3,
                sign: SignKind::Embedded,
            },
        )])
        .unwrap();
        for n in [12345i128, -6789, 0] {
            let vals = vec![(
                "amt".into(),
                Value::Decimal {
                    unscaled: n,
                    scale: 2,
                },
            )];
            assert_eq!(rt(&layout, vals.clone(), Encoding::Ascii), vals);
        }
    }

    #[test]
    fn binary_and_float_round_trip() {
        let layout = Layout::from_fields(vec![
            field(
                "a",
                0,
                4,
                FieldKind::Binary {
                    endian: Endian::Little,
                    signed: true,
                },
            ),
            field(
                "b",
                4,
                8,
                FieldKind::Float {
                    bits: 64,
                    endian: Endian::Big,
                },
            ),
        ])
        .unwrap();
        let vals = vec![
            ("a".into(), Value::Int(-12345)),
            ("b".into(), Value::Float(2.5)),
        ];
        assert_eq!(rt(&layout, vals.clone(), Encoding::Ascii), vals);
    }

    #[test]
    fn separate_sign_round_trip() {
        let layout = Layout::from_fields(vec![field(
            "n",
            0,
            6,
            FieldKind::Int {
                signed: true,
                sign: SignKind::LeadingSeparate,
            },
        )])
        .unwrap();
        let vals = vec![("n".into(), Value::Int(-123))];
        let bytes = encode_record(&layout, &vals, Encoding::Ascii).unwrap();
        assert_eq!(&bytes, b"-00123");
        assert_eq!(rt(&layout, vals.clone(), Encoding::Ascii), vals);
    }

    #[test]
    fn ebcdic_round_trip() {
        let layout = Layout::from_fields(vec![
            field(
                "name",
                0,
                5,
                FieldKind::Text {
                    justify: Justify::Left,
                    trim: true,
                    pad: b' ',
                },
            ),
            field(
                "amt",
                5,
                3,
                FieldKind::Decimal {
                    precision: 5,
                    scale: 2,
                    repr: NumRepr::Comp3,
                    sign: SignKind::Embedded,
                },
            ),
        ])
        .unwrap();
        let vals = vec![
            ("name".into(), Value::Text("ABC".into())),
            (
                "amt".into(),
                Value::Decimal {
                    unscaled: 12345,
                    scale: 2,
                },
            ),
        ];
        assert_eq!(rt(&layout, vals.clone(), Encoding::Ebcdic), vals);
    }

    #[test]
    fn text_overflow_errors() {
        let layout = Layout::from_fields(vec![field(
            "name",
            0,
            3,
            FieldKind::Text {
                justify: Justify::Left,
                trim: true,
                pad: b' ',
            },
        )])
        .unwrap();
        let vals = vec![("name".into(), Value::Text("TOOLONG".into()))];
        assert!(encode_record(&layout, &vals, Encoding::Ascii).is_err());
    }
}
