//! Spec introspection: flatten a [`Layout`] into a flat list of human-readable
//! field descriptors, without reading any data. Powers the `describe_fixed`
//! table function so users can see exactly how a spec resolves — dotted field
//! paths, the SQL/DuckDB column type, byte offset, width, and OCCURS info.
//!
//! Offsets/widths are the **static** layout positions. For a variable-length
//! layout (`OCCURS … DEPENDING ON`) the offsets of fields after the table shift
//! at decode time; the reported `offset` is then the static base and `occurs`
//! is the declared maximum (with `depending_on` naming the controlling field).

use crate::layout::{Endian, Field, FieldKind, Layout, NumRepr};

/// One row of `describe_fixed` output: a single field in the layout.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDesc {
    /// Dotted path to the field (e.g. `item.sku`); top-level fields have no dot.
    pub path: String,
    /// Nesting depth (0 for top-level fields).
    pub depth: usize,
    /// Short codec label for the field kind (e.g. `text`, `int32 LE`, `comp-3`).
    pub kind: String,
    /// The DuckDB/Arrow column type this field maps to (e.g. `VARCHAR`,
    /// `DECIMAL(9,2)`, `STRUCT`, `BIGINT[]`).
    pub sql_type: String,
    /// Static byte offset of the field within the record.
    pub offset: usize,
    /// Bytes consumed by one occurrence of the field.
    pub width: usize,
    /// Declared OCCURS count (maximum for `OCCURS … DEPENDING ON`); `None` for a
    /// non-repeating field.
    pub occurs: Option<usize>,
    /// Name of the controlling field for `OCCURS … DEPENDING ON`, else `None`.
    pub depending_on: Option<String>,
}

/// Flatten a layout into descriptor rows (depth-first, groups before children).
pub fn describe(layout: &Layout) -> Vec<FieldDesc> {
    let mut out = Vec::new();
    walk(&layout.fields, "", 0, 0, &mut out);
    out
}

fn walk(fields: &[Field], prefix: &str, base: usize, depth: usize, out: &mut Vec<FieldDesc>) {
    for f in fields {
        if matches!(f.kind, FieldKind::Pad { .. }) {
            continue;
        }
        let path = if prefix.is_empty() {
            f.name.clone()
        } else {
            format!("{prefix}.{}", f.name)
        };
        let offset = base + f.offset;
        out.push(FieldDesc {
            path: path.clone(),
            depth,
            kind: kind_label(f),
            sql_type: sql_type(f),
            offset,
            width: f.width,
            occurs: f.occurs,
            depending_on: f.depending_on.clone(),
        });
        if let FieldKind::Group(children) = &f.kind {
            walk(children, &path, offset, depth + 1, out);
        }
    }
}

fn endian_tag(e: Endian) -> &'static str {
    match e {
        Endian::Big => "BE",
        Endian::Little => "LE",
    }
}

/// A short, human-oriented label for the field's codec.
fn kind_label(f: &Field) -> String {
    match &f.kind {
        FieldKind::Text { .. } => "text".into(),
        FieldKind::Int { signed, .. } => {
            if *signed {
                "int (display, signed)".into()
            } else {
                "int (display)".into()
            }
        }
        FieldKind::Binary { endian, signed } => {
            let s = if *signed { "int" } else { "uint" };
            format!("{s}{} {}", f.width * 8, endian_tag(*endian))
        }
        FieldKind::Float { bits, endian } => format!("float{bits} {}", endian_tag(*endian)),
        FieldKind::Hex { .. } => "hex".into(),
        FieldKind::Bool => "bool".into(),
        FieldKind::Pad { .. } => "pad".into(),
        FieldKind::Decimal { repr, .. } => match repr {
            NumRepr::Comp3 => "comp-3".into(),
            NumRepr::Zoned => "zoned".into(),
            NumRepr::Display => "decimal (display)".into(),
        },
        FieldKind::Group(_) => "group".into(),
    }
}

/// The DuckDB column type, wrapping in `…[]` for OCCURS (matches `arrow_map`).
fn sql_type(f: &Field) -> String {
    let base = base_sql_type(f);
    if f.occurs.is_some() {
        format!("{base}[]")
    } else {
        base
    }
}

fn base_sql_type(f: &Field) -> String {
    match &f.kind {
        FieldKind::Text { .. } | FieldKind::Hex { .. } => "VARCHAR".into(),
        FieldKind::Int { .. } | FieldKind::Binary { .. } => "BIGINT".into(),
        FieldKind::Float { bits: 64, .. } => "DOUBLE".into(),
        FieldKind::Float { .. } => "REAL".into(),
        FieldKind::Bool => "BOOLEAN".into(),
        FieldKind::Pad { .. } => "NULL".into(),
        FieldKind::Decimal {
            precision, scale, ..
        } => {
            // Mirror arrow_map's DuckDB precision cap (38).
            let p = (*precision).clamp(1, 38);
            format!("DECIMAL({p},{scale})")
        }
        FieldKind::Group(_) => "STRUCT".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_spec;

    #[test]
    fn describes_flat_template() {
        let layout = parse_spec("name:A10 qty:9(5) amt:l<", None).unwrap();
        let rows = describe(&layout);
        assert_eq!(rows.len(), 3);

        assert_eq!(rows[0].path, "name");
        assert_eq!(rows[0].sql_type, "VARCHAR");
        assert_eq!(rows[0].offset, 0);
        assert_eq!(rows[0].width, 10);

        assert_eq!(rows[1].path, "qty");
        assert_eq!(rows[1].offset, 10);
        assert_eq!(rows[1].sql_type, "BIGINT");

        assert_eq!(rows[2].path, "amt");
        assert_eq!(rows[2].offset, 15);
        assert_eq!(rows[2].kind, "int32 LE");
    }

    #[test]
    fn pad_fields_omitted() {
        let layout = parse_spec("a:A2 x(3) b:A2", None).unwrap();
        let rows = describe(&layout);
        assert_eq!(rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // The pad still advances the offset of the field after it.
        assert_eq!(rows[1].offset, 5);
    }

    #[test]
    fn nested_group_and_occurs() {
        let spec = r#"[
            {"name":"hdr","type":"str","width":4},
            {"name":"item","occurs":2,"fields":[
                {"name":"sku","type":"str","width":3},
                {"name":"amt","type":"comp3","digits":5,"scale":2,"signed":true}
            ]}
        ]"#;
        let rows = describe(&parse_spec(spec, None).unwrap());
        // hdr, item (group), item.sku, item.amt
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].path, "item");
        assert_eq!(rows[1].sql_type, "STRUCT[]");
        assert_eq!(rows[1].occurs, Some(2));
        assert_eq!(rows[1].depth, 0);

        assert_eq!(rows[2].path, "item.sku");
        assert_eq!(rows[2].depth, 1);
        assert_eq!(rows[2].offset, 4); // group base
        assert_eq!(rows[3].path, "item.amt");
        assert_eq!(rows[3].sql_type, "DECIMAL(5,2)");
        assert_eq!(rows[3].offset, 7); // 4 + 3
    }
}
