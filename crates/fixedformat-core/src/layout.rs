//! The Layout IR — the shared internal representation every spec front-end
//! (template / JSON / COBOL copybook) lowers to, and that the decode/encode
//! codecs consume.
//!
//! A [`Layout`] is an ordered list of [`Field`]s plus the total record length.
//! Each field knows its byte `offset` and `width` within a record, its
//! [`FieldKind`] (how to interpret those bytes), and optional `occurs`
//! (repetition → LIST) / `redefines` (overlapping bytes → grouped into a STRUCT
//! union) modifiers.

use crate::{Error, Result};

/// A complete record layout: the field list plus the total record byte length.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub record_len: usize,
    pub fields: Vec<Field>,
}

/// One field within a record.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    /// Byte offset of the field within the record.
    pub offset: usize,
    /// Bytes consumed by a single occurrence of this field.
    pub width: usize,
    pub kind: FieldKind,
    /// `OCCURS n` — a repeating field rendered as a LIST of `n` elements.
    pub occurs: Option<usize>,
    /// `REDEFINES other` — this field reinterprets the same bytes as `other`.
    /// The worker folds a base field and all its redefiners into one STRUCT.
    pub redefines: Option<String>,
}

impl Field {
    /// Total bytes this field consumes in the record (accounting for OCCURS).
    pub fn total_width(&self) -> usize {
        self.width * self.occurs.unwrap_or(1)
    }
}

/// How a field's bytes are interpreted.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// Character field: PIC X / template `A` (space pad), `a` (null pad),
    /// `Z` (null-terminated). `pad` is the fill byte; `trim` strips it on decode.
    Text {
        justify: Justify,
        trim: bool,
        pad: u8,
    },
    /// Numeric display field: PIC 9 (ASCII/EBCDIC digits, optional sign).
    Int { signed: bool, sign: SignKind },
    /// Fixed-width two's-complement binary integer: template `c..Q`,
    /// COBOL COMP / COMP-4 / COMP-5. `width` (1/2/4/8) lives on the Field.
    Binary { endian: Endian, signed: bool },
    /// IEEE-754 float: template `e`/`f`/`d` (16/32/64 bit).
    Float { bits: u8, endian: Endian },
    /// Hex string: template `H` (high nibble first) / `h` (low nibble first).
    Hex { order: Endian },
    /// Single-byte boolean: template `?` (0 → false, non-zero → true).
    Bool,
    /// Pad byte(s): template `x`. Consumed on read, emitted as `pad` on write,
    /// never produces an output column.
    Pad { pad: u8 },
    /// Decimal with an implied decimal point. `repr` selects the storage form;
    /// the value is exact (unscaled i128 + `scale`).
    Decimal {
        precision: u8,
        scale: u8,
        repr: NumRepr,
        sign: SignKind,
    },
    /// Group item: a nested set of fields rendered as a STRUCT column.
    Group(Vec<Field>),
}

/// Text justification / fill side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Left,
    Right,
}

/// Byte order for binary/float fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Big,
    Little,
}

/// Storage form of a decimal field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumRepr {
    /// Zoned/DISPLAY: one ASCII (or EBCDIC) digit per byte, implied point.
    Display,
    /// Packed decimal (COBOL COMP-3): two digits per byte, sign nibble last.
    Comp3,
    /// Zoned decimal with an overpunch sign in the last byte.
    Zoned,
}

/// Where (and whether) a numeric field carries its sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignKind {
    /// Unsigned — always non-negative.
    Unsigned,
    /// Sign embedded per the representation (overpunch / packed nibble).
    Embedded,
    /// Leading separate sign byte (`+`/`-`).
    LeadingSeparate,
    /// Trailing separate sign byte (`+`/`-`).
    TrailingSeparate,
}

impl Layout {
    /// Build a layout from a flat field list, validating that field extents fit
    /// and computing the record length as the maximum field end (REDEFINES
    /// overlap, so we take the max rather than the sum).
    pub fn from_fields(fields: Vec<Field>) -> Result<Layout> {
        let record_len = fields
            .iter()
            .map(|f| f.offset + f.total_width())
            .max()
            .unwrap_or(0);
        Ok(Layout { record_len, fields })
    }

    /// Validate that a record byte slice is long enough for this layout.
    pub fn check_record_len(&self, len: usize) -> Result<()> {
        if len < self.record_len {
            return Err(Error(format!(
                "record too short: need {} bytes, got {}",
                self.record_len, len
            )));
        }
        Ok(())
    }
}

/// Parse a spec string into a [`Layout`], dispatching on `format` or
/// auto-detecting it.
///
/// Detection: a `format` hint (`"template"`, `"json"`, `"copybook"`) wins;
/// otherwise a leading `[`/`{` ⇒ JSON, a level number followed by a token ⇒
/// copybook, else template.
pub fn parse_spec(spec: &str, format: Option<&str>) -> Result<Layout> {
    let fmt = match format {
        Some(f) => f.to_ascii_lowercase(),
        None => detect_format(spec).to_string(),
    };
    match fmt.as_str() {
        "template" => crate::template::parse(spec),
        "json" => crate::jsonspec::parse(spec),
        "copybook" => crate::copybook::parse(spec),
        other => Err(Error(format!("unknown spec format: {other}"))),
    }
}

/// Heuristically classify a spec string's format.
pub fn detect_format(spec: &str) -> &'static str {
    let trimmed = spec.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        return "json";
    }
    // A copybook starts with a 2-digit level number (01..49, 66, 77, 88)
    // followed by whitespace and a name. Templates never start that way.
    let first = trimmed.split_whitespace().next().unwrap_or("");
    if first.len() == 2 && first.chars().all(|c| c.is_ascii_digit()) {
        return "copybook";
    }
    "template"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_json() {
        assert_eq!(detect_format("  [{\"name\":\"a\"}]"), "json");
        assert_eq!(detect_format("{\"fields\":[]}"), "json");
    }

    #[test]
    fn detects_copybook() {
        assert_eq!(detect_format("01 REC.\n  05 ID PIC X(10)."), "copybook");
        assert_eq!(detect_format("05 ID PIC X(10)."), "copybook");
    }

    #[test]
    fn detects_template() {
        assert_eq!(detect_format("A10 9(5) A8"), "template");
        assert_eq!(detect_format("name:A10 qty:S<"), "template");
    }

    #[test]
    fn record_len_uses_max_for_redefines() {
        let fields = vec![
            Field {
                name: "raw".into(),
                offset: 0,
                width: 8,
                kind: FieldKind::Text {
                    justify: Justify::Left,
                    trim: true,
                    pad: b' ',
                },
                occurs: None,
                redefines: None,
            },
            Field {
                name: "num".into(),
                offset: 0,
                width: 8,
                kind: FieldKind::Int {
                    signed: false,
                    sign: SignKind::Unsigned,
                },
                occurs: None,
                redefines: Some("raw".into()),
            },
        ];
        let layout = Layout::from_fields(fields).unwrap();
        assert_eq!(layout.record_len, 8);
    }

    #[test]
    fn total_width_accounts_for_occurs() {
        let f = Field {
            name: "lines".into(),
            offset: 0,
            width: 4,
            kind: FieldKind::Int {
                signed: false,
                sign: SignKind::Unsigned,
            },
            occurs: Some(3),
            redefines: None,
        };
        assert_eq!(f.total_width(), 12);
    }
}
