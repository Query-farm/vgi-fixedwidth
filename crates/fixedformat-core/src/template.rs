//! Perl-`unpack` / Python-`struct`-style template parser → [`Layout`].
//!
//! Whitespace-separated tokens, each optionally prefixed `name:`. A token is
//! either a byte-order control (`<` `>` `!` `=` `@`) that sets the default order
//! for following tokens, a format code with an optional count, or a COBOL-ish
//! display PIC token (`9(5)`, `S9(7)V99`, `X(10)`) handled by [`crate::copybook::parse_pic`].
//!
//! Format codes (count in `(n)` or as trailing digits):
//!
//! | code        | meaning                              | count is |
//! |-------------|--------------------------------------|----------|
//! | `A`/`a`/`Z` | string (space / null pad / null-term)| width    |
//! | `c`/`C`     | int8 / uint8                         | repeat   |
//! | `s`/`S`     | int16 / uint16                       | repeat   |
//! | `l`/`L` `i`/`I` | int32 / uint32                   | repeat   |
//! | `q`/`Q`     | int64 / uint64                       | repeat   |
//! | `n`/`N`     | uint16 / uint32, big-endian          | repeat   |
//! | `v`/`V`     | uint16 / uint32, little-endian       | repeat   |
//! | `e`/`f`/`d` | float16 / float32 / float64          | repeat   |
//! | `H`/`h`     | hex string (high / low nibble first) | width    |
//! | `?`         | boolean byte                         | repeat   |
//! | `x`         | pad byte(s)                          | width    |

use crate::copybook::{self, Usage};
use crate::layout::{Endian, Field, FieldKind, Justify, Layout};
use crate::{Error, Result};

const NATIVE: Endian = if cfg!(target_endian = "little") {
    Endian::Little
} else {
    Endian::Big
};

/// Parse a template string into a [`Layout`].
pub fn parse(src: &str) -> Result<Layout> {
    let mut fields = Vec::new();
    let mut offset = 0usize;
    let mut order = Endian::Big; // default network order; `<`/`=` change it
    let mut auto = 0usize;

    for token in src.split_whitespace() {
        // Standalone byte-order control.
        if let Some(o) = order_control(token) {
            order = o;
            continue;
        }

        let (name, body) = match token.split_once(':') {
            Some((n, b)) => (Some(n.to_string()), b),
            None => (None, token),
        };
        if body.is_empty() {
            return Err(Error(format!("empty token {token:?}")));
        }

        let name = name.unwrap_or_else(|| {
            auto += 1;
            format!("field_{auto}")
        });

        let (kind, width, occurs) = parse_body(body, order)?;
        let total = width * occurs.unwrap_or(1);
        // Pad tokens never produce a named output column but still advance.
        fields.push(Field {
            name,
            offset,
            width,
            kind,
            occurs,
            redefines: None,
        });
        offset += total;
    }

    Layout::from_fields(fields)
}

fn order_control(token: &str) -> Option<Endian> {
    match token {
        "<" => Some(Endian::Little),
        ">" | "!" => Some(Endian::Big),
        "=" | "@" => Some(NATIVE),
        _ => None,
    }
}

/// Parse a token body into `(kind, width, occurs)`.
fn parse_body(body: &str, order: Endian) -> Result<(FieldKind, usize, Option<usize>)> {
    let chars: Vec<char> = body.chars().collect();
    let code = chars[0];

    // PIC-looking display tokens delegate to the shared copybook PIC parser.
    if code == '9' || code == 'X' || (code == 'S' && chars.get(1) == Some(&'9')) {
        let (kind, width) = copybook::parse_pic(body, Usage::Display, None)?;
        return Ok((kind, width, None));
    }

    // Otherwise: a single-letter code, an optional count, an optional order
    // suffix (`<`/`>`).
    let mut rest = &chars[1..];
    let mut local_order = order;
    if let Some(&last) = rest.last() {
        if last == '<' {
            local_order = Endian::Little;
            rest = &rest[..rest.len() - 1];
        } else if last == '>' {
            local_order = Endian::Big;
            rest = &rest[..rest.len() - 1];
        }
    }
    let count = parse_count(rest)?;

    let width_kind = |w: usize, k: FieldKind| (k, w, None);
    let counted = |elem_width: usize, k: FieldKind| {
        // count = repeat → LIST when > 1.
        if let Some(n) = count {
            (k, elem_width, Some(n))
        } else {
            (k, elem_width, None)
        }
    };

    Ok(match code {
        'A' => width_kind(count.unwrap_or(1), FieldKind::Text { justify: Justify::Left, trim: true, pad: b' ' }),
        'a' => width_kind(count.unwrap_or(1), FieldKind::Text { justify: Justify::Left, trim: true, pad: 0 }),
        'Z' => width_kind(count.unwrap_or(1), FieldKind::Text { justify: Justify::Left, trim: true, pad: 0 }),
        'H' => width_kind(count.unwrap_or(1), FieldKind::Hex { order: Endian::Big }),
        'h' => width_kind(count.unwrap_or(1), FieldKind::Hex { order: Endian::Little }),
        'x' => width_kind(count.unwrap_or(1), FieldKind::Pad { pad: 0 }),
        'c' => counted(1, FieldKind::Binary { endian: local_order, signed: true }),
        'C' => counted(1, FieldKind::Binary { endian: local_order, signed: false }),
        's' => counted(2, FieldKind::Binary { endian: local_order, signed: true }),
        'S' => counted(2, FieldKind::Binary { endian: local_order, signed: false }),
        'l' | 'i' => counted(4, FieldKind::Binary { endian: local_order, signed: true }),
        'L' | 'I' => counted(4, FieldKind::Binary { endian: local_order, signed: false }),
        'q' => counted(8, FieldKind::Binary { endian: local_order, signed: true }),
        'Q' => counted(8, FieldKind::Binary { endian: local_order, signed: false }),
        'n' => counted(2, FieldKind::Binary { endian: Endian::Big, signed: false }),
        'N' => counted(4, FieldKind::Binary { endian: Endian::Big, signed: false }),
        'v' => counted(2, FieldKind::Binary { endian: Endian::Little, signed: false }),
        'V' => counted(4, FieldKind::Binary { endian: Endian::Little, signed: false }),
        'e' => counted(2, FieldKind::Float { bits: 16, endian: local_order }),
        'f' => counted(4, FieldKind::Float { bits: 32, endian: local_order }),
        'd' => counted(8, FieldKind::Float { bits: 64, endian: local_order }),
        '?' => counted(1, FieldKind::Bool),
        other => return Err(Error(format!("unknown template code {other:?}"))),
    })
}

/// Parse a count: either `(n)` or bare trailing digits. Empty ⇒ None.
fn parse_count(rest: &[char]) -> Result<Option<usize>> {
    if rest.is_empty() {
        return Ok(None);
    }
    let s: String = rest.iter().collect();
    let inner = if let Some(stripped) = s.strip_prefix('(') {
        stripped
            .strip_suffix(')')
            .ok_or_else(|| Error(format!("unbalanced count parentheses in {s:?}")))?
    } else {
        &s
    };
    let n: usize = inner
        .trim()
        .parse()
        .map_err(|_| Error(format!("invalid count {inner:?}")))?;
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_record;
    use crate::value::Value;
    use crate::Encoding;

    #[test]
    fn parses_string_and_display_int() {
        let layout = parse("name:A10 qty:9(5)").unwrap();
        assert_eq!(layout.record_len, 15);
        assert_eq!(layout.fields[0].name, "name");
        assert_eq!(layout.fields[1].name, "qty");
        let out = decode_record(&layout, b"JOHN      00042", Encoding::Ascii).unwrap();
        assert_eq!(out[1].1, Value::Int(42));
    }

    #[test]
    fn auto_names_fields() {
        let layout = parse("A10 9(5)").unwrap();
        assert_eq!(layout.fields[0].name, "field_1");
        assert_eq!(layout.fields[1].name, "field_2");
    }

    #[test]
    fn binary_codes_and_order() {
        let layout = parse("a:s< b:l> c:N").unwrap();
        assert_eq!(layout.record_len, 2 + 4 + 4);
        match layout.fields[0].kind {
            FieldKind::Binary { endian, signed } => {
                assert_eq!(endian, Endian::Little);
                assert!(signed);
            }
            _ => panic!(),
        }
        match layout.fields[2].kind {
            FieldKind::Binary { endian, signed } => {
                assert_eq!(endian, Endian::Big);
                assert!(!signed);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn order_control_token_sets_default() {
        let layout = parse("< s S").unwrap();
        for f in &layout.fields {
            match f.kind {
                FieldKind::Binary { endian, .. } => assert_eq!(endian, Endian::Little),
                _ => panic!(),
            }
        }
    }

    #[test]
    fn repeat_count_makes_list() {
        let layout = parse("vals:s(3)").unwrap();
        assert_eq!(layout.fields[0].occurs, Some(3));
        assert_eq!(layout.record_len, 6);
    }

    #[test]
    fn float_and_pad_and_hex() {
        let layout = parse("f:d g:H4 x(2) b:?").unwrap();
        assert_eq!(layout.record_len, 8 + 4 + 2 + 1);
        assert!(matches!(layout.fields[1].kind, FieldKind::Hex { .. }));
        assert!(matches!(layout.fields[2].kind, FieldKind::Pad { .. }));
        assert!(matches!(layout.fields[3].kind, FieldKind::Bool));
    }

    #[test]
    fn decodes_against_struct_pack_bytes() {
        // Python: struct.pack('>hIf', -2, 7, 1.5) == b'\xff\xfe\x00\x00\x00\x07?\xc0\x00\x00'
        let layout = parse("a:s b:I c:f").unwrap();
        let bytes = [0xff, 0xfe, 0x00, 0x00, 0x00, 0x07, 0x3f, 0xc0, 0x00, 0x00];
        let out = decode_record(&layout, &bytes, Encoding::Ascii).unwrap();
        assert_eq!(out[0].1, Value::Int(-2));
        assert_eq!(out[1].1, Value::Int(7));
        assert_eq!(out[2].1, Value::Float(1.5));
    }
}
