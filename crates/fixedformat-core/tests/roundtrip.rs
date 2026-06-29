//! Property-based round-trip fuzzing: for any in-range value, `decode(encode(v))
//! == v` across every `FieldKind`, under both ASCII and EBCDIC encodings.
//!
//! Each test builds a single-field layout, encodes a generated value to record
//! bytes, decodes it back, and asserts equality. This is the strongest evidence
//! that the encode/decode codecs are exact inverses.

use fixedformat_core::decode::decode_record;
use fixedformat_core::encode::encode_record;
use fixedformat_core::layout::{Endian, Field, FieldKind, Justify, Layout, NumRepr, SignKind};
use fixedformat_core::value::Value;
use fixedformat_core::{packed, Encoding};
use proptest::prelude::*;

/// Round-trip a single field/value pair and return the decoded value.
fn rt(kind: FieldKind, width: usize, occurs: Option<usize>, value: Value, enc: Encoding) -> Value {
    let field = Field {
        name: "f".into(),
        offset: 0,
        width,
        kind,
        occurs,
        depending_on: None,
        redefines: None,
    };
    let layout = Layout::from_fields(vec![field]).unwrap();
    let bytes = encode_record(&layout, &[("f".into(), value)], enc).unwrap();
    decode_record(&layout, &bytes, enc)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .1
}

/// 10^n as i128 (n <= 18 keeps us well inside i128).
fn pow10(n: u32) -> i128 {
    10i128.pow(n)
}

fn enc_strategy() -> impl Strategy<Value = Encoding> {
    prop_oneof![Just(Encoding::Ascii), Just(Encoding::Ebcdic)]
}

proptest! {
    // Unsigned display integer: PIC 9(w).
    #[test]
    fn display_uint_round_trips(
        (width, value) in (1u32..=18).prop_flat_map(|w| (Just(w), 0i128..pow10(w))),
        enc in enc_strategy(),
    ) {
        let kind = FieldKind::Int { signed: false, sign: SignKind::Unsigned };
        let out = rt(kind, width as usize, None, Value::Int(value as i64), enc);
        prop_assert_eq!(out, Value::Int(value as i64));
    }

    // Signed display integer with a leading or trailing separate sign byte.
    #[test]
    fn display_signed_separate_round_trips(
        (digits, mag) in (1u32..=17).prop_flat_map(|d| (Just(d), 0i128..pow10(d))),
        negative in any::<bool>(),
        trailing in any::<bool>(),
        enc in enc_strategy(),
    ) {
        let value = if negative { -mag } else { mag } as i64;
        let sign = if trailing { SignKind::TrailingSeparate } else { SignKind::LeadingSeparate };
        let kind = FieldKind::Int { signed: true, sign };
        let width = digits as usize + 1; // +1 for the separate sign byte
        let out = rt(kind, width, None, Value::Int(value), enc);
        prop_assert_eq!(out, Value::Int(value));
    }

    // Two's-complement binary integer (COMP / template c..q), big or little endian.
    #[test]
    fn binary_int_round_trips(
        wsel in prop::sample::select(vec![1usize, 2, 4, 8]),
        signed in any::<bool>(),
        little in any::<bool>(),
        raw in any::<i64>(),
    ) {
        // Constrain the value to the field's representable range.
        let bits = (wsel * 8) as u32;
        let value: i64 = if wsel == 8 {
            raw
        } else if signed {
            let lo = -(1i64 << (bits - 1));
            let hi = (1i64 << (bits - 1)) - 1;
            lo + raw.rem_euclid(hi - lo + 1)
        } else {
            (raw.rem_euclid(1i64 << bits)).abs()
        };
        let endian = if little { Endian::Little } else { Endian::Big };
        let kind = FieldKind::Binary { endian, signed };
        let out = rt(kind, wsel, None, Value::Int(value), Encoding::Ascii);
        prop_assert_eq!(out, Value::Int(value));
    }

    // COMP-3 packed decimal, signed, any scale.
    #[test]
    fn comp3_round_trips(
        (digits, mag) in (1u32..=18).prop_flat_map(|d| (Just(d), 0i128..pow10(d))),
        scale_frac in 0u32..=18,
        negative in any::<bool>(),
        enc in enc_strategy(),
    ) {
        let digits_u8 = digits as u8;
        let scale = (scale_frac % (digits + 1)) as u8;
        let unscaled = if negative { -mag } else { mag };
        let kind = FieldKind::Decimal {
            precision: digits_u8, scale, repr: NumRepr::Comp3, sign: SignKind::Embedded,
        };
        let width = packed::byte_width(digits_u8);
        let out = rt(kind, width, None, Value::Decimal { unscaled, scale }, enc);
        prop_assert_eq!(out, Value::Decimal { unscaled, scale });
    }

    // Zoned decimal (overpunch sign), signed, any scale.
    #[test]
    fn zoned_round_trips(
        (digits, mag) in (1u32..=18).prop_flat_map(|d| (Just(d), 0i128..pow10(d))),
        scale_frac in 0u32..=18,
        negative in any::<bool>(),
        enc in enc_strategy(),
    ) {
        let scale = (scale_frac % (digits + 1)) as u8;
        let unscaled = if negative { -mag } else { mag };
        let kind = FieldKind::Decimal {
            precision: digits as u8, scale, repr: NumRepr::Zoned, sign: SignKind::Embedded,
        };
        let out = rt(kind, digits as usize, None, Value::Decimal { unscaled, scale }, enc);
        prop_assert_eq!(out, Value::Decimal { unscaled, scale });
    }

    // float64 round-trips its own bytes exactly (finite values).
    #[test]
    fn float64_round_trips(bits in any::<u64>(), little in any::<bool>()) {
        let f = f64::from_bits(bits);
        prop_assume!(f.is_finite());
        let endian = if little { Endian::Little } else { Endian::Big };
        let kind = FieldKind::Float { bits: 64, endian };
        let out = rt(kind, 8, None, Value::Float(f), Encoding::Ascii);
        prop_assert_eq!(out, Value::Float(f));
    }

    // float32 round-trips through an f64 carrier exactly for f32-representable values.
    #[test]
    fn float32_round_trips(bits in any::<u32>(), little in any::<bool>()) {
        let f = f32::from_bits(bits);
        prop_assume!(f.is_finite());
        let endian = if little { Endian::Little } else { Endian::Big };
        let kind = FieldKind::Float { bits: 32, endian };
        let out = rt(kind, 4, None, Value::Float(f as f64), Encoding::Ascii);
        prop_assert_eq!(out, Value::Float(f as f64));
    }

    // Text fields round-trip when the value has no trailing pad to trim away.
    #[test]
    fn text_round_trips(
        s in "[A-Z0-9]{0,12}",
        right in any::<bool>(),
        enc in enc_strategy(),
    ) {
        let width = 12usize; // always >= |s|, so no truncation
        let justify = if right { Justify::Right } else { Justify::Left };
        let kind = FieldKind::Text { justify, trim: true, pad: b' ' };
        let out = rt(kind, width, None, Value::Text(s.clone()), enc);
        prop_assert_eq!(out, Value::Text(s));
    }

    // OCCURS: a list of display ints round-trips element-for-element.
    #[test]
    fn occurs_list_round_trips(
        vals in prop::collection::vec(0i64..1000, 1..=6),
    ) {
        let kind = FieldKind::Int { signed: false, sign: SignKind::Unsigned };
        let items: Vec<Value> = vals.iter().map(|v| Value::Int(*v)).collect();
        let out = rt(kind, 4, Some(vals.len()), Value::List(items.clone()), Encoding::Ascii);
        prop_assert_eq!(out, Value::List(items));
    }
}
