//! Multi-record-type layouts: one file, many record shapes.
//!
//! Real flat files are heterogeneous — a header, many detail rows, a trailer —
//! each a DIFFERENT layout, selected by a "record type" discriminator field
//! (e.g. byte 0 = `H`/`D`/`T`). A [`MultiLayout`] holds one [`Layout`] per record
//! type plus the discriminator position; [`MultiLayout::select`] reads the
//! discriminator from a record's bytes and returns the matching variant's layout,
//! which the caller then decodes with the ordinary [`crate::decode::decode_record`].
//!
//! The spec is JSON and **additive** — it reuses the existing per-variant JSON
//! field syntax verbatim (so every field type / group / OCCURS works per variant):
//!
//! ```json
//! {
//!   "discriminator": {"offset": 0, "width": 1},
//!   "records": {
//!     "H": [ {"name":"co","type":"str","width":20} ],
//!     "D": [ {"name":"sku","type":"str","width":10}, {"name":"qty","type":"int","digits":5} ],
//!     "T": [ {"name":"count","type":"int","digits":6} ]
//!   },
//!   "default": "D"
//! }
//! ```
//!
//! `discriminator` is `{offset, width}` (bytes read from each record, transcoded
//! out of EBCDIC if needed, trimmed, matched case-sensitively against the
//! `records` keys). An unmatched value is a hard error unless `default` names a
//! fallback key. The `records` order is preserved so each variant gets a stable
//! index (used by the worker as the Arrow union type-id).

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::layout::Layout;
use crate::{ebcdic, Encoding, Error, Result};

/// A multi-record-type layout: a discriminator position plus one [`Layout`] per
/// record type, in declaration order (so variant `i` is stable).
#[derive(Debug, Clone, PartialEq)]
pub struct MultiLayout {
    /// `(offset, width)` of the discriminator field within each record's bytes.
    pub discriminator: (usize, usize),
    /// One `(tag, layout)` per record type, preserving the spec's `records` order.
    pub variants: Vec<(String, Layout)>,
    /// Index into `variants` to fall back to when a record's discriminator value
    /// matches no tag; `None` makes an unknown value a hard error.
    pub default: Option<usize>,
}

#[derive(Deserialize)]
struct Disc {
    offset: usize,
    width: usize,
}

/// The raw top-level spec. `records` is captured order-preserving via
/// [`OrderedRecords`] (serde_json's default map is sorted, which would scramble
/// the variant type-ids).
#[derive(Deserialize)]
struct RawSpec {
    discriminator: Disc,
    records: OrderedRecords,
    #[serde(default)]
    default: Option<String>,
}

/// A JSON object deserialized as an **ordered** list of `(key, value)` entries,
/// so the declared `records` order survives regardless of serde_json's
/// `preserve_order` feature.
struct OrderedRecords(Vec<(String, serde_json::Value)>);

impl<'de> Deserialize<'de> for OrderedRecords {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OrderedRecords;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of record-type tag to JSON field list")
            }
            fn visit_map<M>(self, mut map: M) -> std::result::Result<OrderedRecords, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, serde_json::Value>()? {
                    out.push((k, v));
                }
                Ok(OrderedRecords(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl MultiLayout {
    /// Parse a multi-record JSON spec into a [`MultiLayout`]. Each variant's field
    /// list is lowered by the existing [`crate::jsonspec`] parser, so all field
    /// types / groups / OCCURS work per variant.
    pub fn parse(src: &str) -> Result<MultiLayout> {
        let raw: RawSpec = serde_json::from_str(src)
            .map_err(|e| Error(format!("invalid multi-record spec: {e}")))?;
        if raw.discriminator.width == 0 {
            return Err(Error(
                "discriminator width must be greater than zero".into(),
            ));
        }
        if raw.records.0.is_empty() {
            return Err(Error(
                "multi-record spec must declare at least one record type".into(),
            ));
        }
        let mut variants = Vec::with_capacity(raw.records.0.len());
        for (tag, fields_json) in raw.records.0 {
            // Reuse the existing JSON field-list parser (accepts a bare array or a
            // `{"fields":[...]}` wrapper) by handing it the variant's value.
            let layout = crate::jsonspec::parse(&fields_json.to_string())
                .map_err(|e| Error(format!("record type {tag:?}: {e}")))?;
            variants.push((tag, layout));
        }
        let default =
            match raw.default {
                None => None,
                Some(tag) => Some(variants.iter().position(|(t, _)| *t == tag).ok_or_else(
                    || Error(format!("default record type {tag:?} is not in records")),
                )?),
            };
        Ok(MultiLayout {
            discriminator: (raw.discriminator.offset, raw.discriminator.width),
            variants,
            default,
        })
    }

    /// Pick the variant for `record` by reading its discriminator bytes. Returns
    /// the variant index and its [`Layout`]. EBCDIC bytes are transcoded to ASCII
    /// before matching; the value is trimmed and compared case-sensitively against
    /// the record-type tags, falling back to `default` (or erroring) on no match.
    pub fn select(&self, record: &[u8], enc: Encoding) -> Result<(usize, &Layout)> {
        let (off, width) = self.discriminator;
        let end = off + width;
        if record.len() < end {
            return Err(Error(format!(
                "record too short for the discriminator at offset {off} width {width}: \
                 got {} bytes",
                record.len()
            )));
        }
        let raw = &record[off..end];
        // Transcode the discriminator out of EBCDIC so the tags can be plain ASCII.
        let ascii: Vec<u8> = match enc {
            Encoding::Ascii => raw.to_vec(),
            Encoding::Ebcdic => raw.iter().map(|b| ebcdic::to_ascii(*b)).collect(),
        };
        let value = String::from_utf8_lossy(&ascii);
        let tag = value.trim();
        if let Some(i) = self.variants.iter().position(|(t, _)| t == tag) {
            return Ok((i, &self.variants[i].1));
        }
        if let Some(d) = self.default {
            return Ok((d, &self.variants[d].1));
        }
        Err(Error(format!(
            "unknown record type {tag:?}: no matching variant and no default declared"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_record;
    use crate::value::Value;

    const SPEC: &str = r#"{
        "discriminator": {"offset": 0, "width": 1},
        "records": {
            "H": [ {"name":"co","type":"str","width":20} ],
            "D": [ {"name":"sku","type":"str","width":10}, {"name":"qty","type":"int","digits":5} ],
            "T": [ {"name":"count","type":"int","digits":6} ]
        }
    }"#;

    #[test]
    fn parses_three_variants_in_order() {
        let ml = MultiLayout::parse(SPEC).unwrap();
        assert_eq!(ml.discriminator, (0, 1));
        assert_eq!(ml.variants.len(), 3);
        assert_eq!(ml.variants[0].0, "H");
        assert_eq!(ml.variants[1].0, "D");
        assert_eq!(ml.variants[2].0, "T");
        // Variant layouts came through the JSON parser intact.
        assert_eq!(ml.variants[1].1.fields.len(), 2);
        assert!(ml.default.is_none());
    }

    #[test]
    fn select_picks_variant_by_discriminator() {
        let ml = MultiLayout::parse(SPEC).unwrap();
        let (hi, _) = ml.select(b"HACME CORP          ", Encoding::Ascii).unwrap();
        assert_eq!(hi, 0);
        let (di, layout) = ml.select(b"DWIDGET   00042", Encoding::Ascii).unwrap();
        assert_eq!(di, 1);
        // Decode the detail record with the selected layout. The discriminator
        // byte is part of the record bytes; this variant models a 10-byte sku
        // (bytes 0..10) then a 5-digit qty (bytes 10..15), so qty is unambiguous.
        let out = decode_record(layout, b"DWIDGET   00042", Encoding::Ascii).unwrap();
        assert_eq!(out[1].1, Value::Int(42));
        let (ti, _) = ml.select(b"T000003", Encoding::Ascii).unwrap();
        assert_eq!(ti, 2);
    }

    #[test]
    fn unknown_tag_errors_without_default() {
        let ml = MultiLayout::parse(SPEC).unwrap();
        let err = ml.select(b"X........", Encoding::Ascii).unwrap_err().0;
        assert!(err.contains("unknown record type"), "{err}");
        assert!(err.contains("\"X\""), "{err}");
    }

    #[test]
    fn unknown_tag_honors_default() {
        let spec = r#"{
            "discriminator": {"offset": 0, "width": 1},
            "records": {
                "D": [ {"name":"sku","type":"str","width":10} ],
                "T": [ {"name":"count","type":"int","digits":6} ]
            },
            "default": "D"
        }"#;
        let ml = MultiLayout::parse(spec).unwrap();
        assert_eq!(ml.default, Some(0));
        let (i, _) = ml.select(b"ZHELLO     ", Encoding::Ascii).unwrap();
        assert_eq!(i, 0);
    }

    #[test]
    fn default_tag_must_exist() {
        let spec = r#"{
            "discriminator": {"offset": 0, "width": 1},
            "records": { "D": [ {"name":"x","type":"str","width":1} ] },
            "default": "Q"
        }"#;
        let err = MultiLayout::parse(spec).unwrap_err().0;
        assert!(err.contains("default record type"), "{err}");
    }

    #[test]
    fn discriminator_can_be_offset_and_trimmed() {
        let spec = r#"{
            "discriminator": {"offset": 2, "width": 3},
            "records": {
                "AB": [ {"name":"x","type":"str","width":4} ],
                "CD": [ {"name":"y","type":"str","width":4} ]
            }
        }"#;
        let ml = MultiLayout::parse(spec).unwrap();
        // Bytes 2..5 = "AB " → trimmed "AB" matches the first variant.
        let (i, _) = ml.select(b"xxAB yyyy", Encoding::Ascii).unwrap();
        assert_eq!(i, 0);
    }

    #[test]
    fn record_too_short_for_discriminator_errors() {
        let ml = MultiLayout::parse(SPEC).unwrap();
        let err = ml.select(b"", Encoding::Ascii).unwrap_err().0;
        assert!(err.contains("record too short"), "{err}");
    }
}
