//! Structured JSON field-list parser → [`Layout`].
//!
//! Accepts either a bare array of field objects or `{ "fields": [ ... ] }`.
//! Each field object: `name`, `type`, and type-specific options (`width`,
//! `digits`, `scale`, `signed`, `endian`, `occurs`, `justify`, `pad`, `sign`).
//!
//! ```json
//! [
//!   {"name": "id",   "type": "str",   "width": 10},
//!   {"name": "qty",  "type": "int",   "digits": 5},
//!   {"name": "amt",  "type": "comp3", "digits": 9, "scale": 2, "signed": true}
//! ]
//! ```

use serde::Deserialize;

use crate::layout::{Endian, Field, FieldKind, Justify, Layout, NumRepr, SignKind};
use crate::{Error, Result};

#[derive(Deserialize)]
#[serde(untagged)]
enum Spec {
    Array(Vec<JsonField>),
    Wrapped { fields: Vec<JsonField> },
}

#[derive(Deserialize)]
struct JsonField {
    name: Option<String>,
    #[serde(rename = "type")]
    ty: String,
    width: Option<usize>,
    digits: Option<u8>,
    scale: Option<u8>,
    signed: Option<bool>,
    endian: Option<String>,
    occurs: Option<usize>,
    justify: Option<String>,
    pad: Option<String>,
    sign: Option<String>,
}

/// Parse a JSON spec into a [`Layout`].
pub fn parse(src: &str) -> Result<Layout> {
    let spec: Spec = serde_json::from_str(src)
        .map_err(|e| Error(format!("invalid JSON spec: {e}")))?;
    let raw = match spec {
        Spec::Array(v) => v,
        Spec::Wrapped { fields } => fields,
    };

    let mut fields = Vec::with_capacity(raw.len());
    let mut offset = 0usize;
    let mut auto = 0usize;
    for jf in raw {
        let name = jf.name.clone().unwrap_or_else(|| {
            auto += 1;
            format!("field_{auto}")
        });
        let (kind, width) = field_kind(&jf)?;
        let total = width * jf.occurs.unwrap_or(1);
        fields.push(Field {
            name,
            offset,
            width,
            kind,
            occurs: jf.occurs,
            redefines: None,
        });
        offset += total;
    }
    Layout::from_fields(fields)
}

fn field_kind(jf: &JsonField) -> Result<(FieldKind, usize)> {
    let endian = match jf.endian.as_deref() {
        None | Some("big") | Some("be") | Some("network") => Endian::Big,
        Some("little") | Some("le") => Endian::Little,
        Some(other) => return Err(Error(format!("unknown endian {other:?}"))),
    };
    let signed = jf.signed.unwrap_or(false);
    let sign = parse_sign(jf.sign.as_deref(), signed)?;

    match jf.ty.to_ascii_lowercase().as_str() {
        "str" | "string" | "text" | "char" | "x" => {
            let width = req_width(jf)?;
            Ok((
                FieldKind::Text {
                    justify: parse_justify(jf.justify.as_deref())?,
                    trim: true,
                    pad: parse_pad(jf.pad.as_deref(), b' ')?,
                },
                width,
            ))
        }
        "int" | "integer" | "display" | "num" | "numeric" => {
            let digits = req_digits(jf)?;
            let sep = matches!(sign, SignKind::LeadingSeparate | SignKind::TrailingSeparate) as usize;
            Ok((FieldKind::Int { signed, sign }, digits as usize + sep))
        }
        "binary" | "comp" | "comp4" | "comp5" => {
            let width = req_width(jf)?;
            Ok((FieldKind::Binary { endian, signed }, width))
        }
        "comp3" | "packed" | "packed-decimal" => {
            let digits = req_digits(jf)?;
            Ok((
                FieldKind::Decimal {
                    precision: digits,
                    scale: jf.scale.unwrap_or(0),
                    repr: NumRepr::Comp3,
                    sign: SignKind::Embedded,
                },
                crate::packed::byte_width(digits),
            ))
        }
        "zoned" => {
            let digits = req_digits(jf)?;
            Ok((
                FieldKind::Decimal {
                    precision: digits,
                    scale: jf.scale.unwrap_or(0),
                    repr: NumRepr::Zoned,
                    sign: SignKind::Embedded,
                },
                digits as usize,
            ))
        }
        "decimal" | "dec" => {
            let digits = req_digits(jf)?;
            let sep = matches!(sign, SignKind::LeadingSeparate | SignKind::TrailingSeparate) as usize;
            Ok((
                FieldKind::Decimal {
                    precision: digits,
                    scale: jf.scale.unwrap_or(0),
                    repr: NumRepr::Display,
                    sign,
                },
                digits as usize + sep,
            ))
        }
        "float" | "real" | "single" | "f32" => Ok((FieldKind::Float { bits: 32, endian }, 4)),
        "double" | "f64" => Ok((FieldKind::Float { bits: 64, endian }, 8)),
        "half" | "f16" => Ok((FieldKind::Float { bits: 16, endian }, 2)),
        "hex" => {
            let width = req_width(jf)?;
            Ok((FieldKind::Hex { order: endian }, width))
        }
        "bool" | "boolean" => Ok((FieldKind::Bool, 1)),
        "pad" | "filler" => Ok((FieldKind::Pad { pad: parse_pad(jf.pad.as_deref(), b' ')? }, req_width(jf)?)),
        other => Err(Error(format!("unknown field type {other:?}"))),
    }
}

fn req_width(jf: &JsonField) -> Result<usize> {
    jf.width
        .or(jf.digits.map(|d| d as usize))
        .ok_or_else(|| Error(format!("field {:?} requires a width", jf.name)))
}

fn req_digits(jf: &JsonField) -> Result<u8> {
    jf.digits
        .or(jf.width.map(|w| w as u8))
        .ok_or_else(|| Error(format!("field {:?} requires digits", jf.name)))
}

fn parse_justify(s: Option<&str>) -> Result<Justify> {
    match s {
        None | Some("left") | Some("l") => Ok(Justify::Left),
        Some("right") | Some("r") => Ok(Justify::Right),
        Some(other) => Err(Error(format!("unknown justify {other:?}"))),
    }
}

fn parse_pad(s: Option<&str>, default: u8) -> Result<u8> {
    match s {
        None => Ok(default),
        Some("space") => Ok(b' '),
        Some("zero") => Ok(b'0'),
        Some("null") | Some("nul") => Ok(0),
        Some(other) => other
            .bytes()
            .next()
            .ok_or_else(|| Error("empty pad".into())),
    }
}

fn parse_sign(s: Option<&str>, signed: bool) -> Result<SignKind> {
    match s {
        None => Ok(if signed { SignKind::Embedded } else { SignKind::Unsigned }),
        Some("leading") | Some("leading_separate") => Ok(SignKind::LeadingSeparate),
        Some("trailing") | Some("trailing_separate") => Ok(SignKind::TrailingSeparate),
        Some("embedded") | Some("overpunch") => Ok(SignKind::Embedded),
        Some("none") | Some("unsigned") => Ok(SignKind::Unsigned),
        Some(other) => Err(Error(format!("unknown sign {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_record;
    use crate::value::Value;
    use crate::Encoding;

    #[test]
    fn parses_array_form() {
        let spec = r#"[
            {"name":"id","type":"str","width":10},
            {"name":"code","type":"int","digits":5},
            {"name":"amt","type":"comp3","digits":9,"scale":2,"signed":true}
        ]"#;
        let layout = parse(spec).unwrap();
        assert_eq!(layout.fields.len(), 3);
        assert_eq!(layout.fields[0].offset, 0);
        assert_eq!(layout.fields[1].offset, 10);
        assert_eq!(layout.fields[2].offset, 15);
        // 9 packed digits -> 5 bytes.
        assert_eq!(layout.record_len, 20);
    }

    #[test]
    fn parses_wrapped_form() {
        let spec = r#"{"fields":[{"name":"a","type":"str","width":3}]}"#;
        let layout = parse(spec).unwrap();
        assert_eq!(layout.record_len, 3);
    }

    #[test]
    fn decode_with_json_spec() {
        let spec = r#"[{"name":"name","type":"str","width":10},{"name":"qty","type":"int","digits":5}]"#;
        let layout = parse(spec).unwrap();
        let out = decode_record(&layout, b"JOHN      00042", Encoding::Ascii).unwrap();
        assert_eq!(out[0].1, Value::Text("JOHN".into()));
        assert_eq!(out[1].1, Value::Int(42));
    }

    #[test]
    fn occurs_and_endian() {
        let spec = r#"[{"name":"v","type":"binary","width":2,"endian":"little","occurs":3}]"#;
        let layout = parse(spec).unwrap();
        assert_eq!(layout.fields[0].occurs, Some(3));
        assert_eq!(layout.record_len, 6);
    }
}
