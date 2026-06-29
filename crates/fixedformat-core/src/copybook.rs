//! COBOL copybook parser → [`Layout`].
//!
//! Supports the practical copybook subset: nested groups via level numbers,
//! elementary `PIC` items (`X`, `A`, `9`, `S`, `V`), `USAGE` (`DISPLAY`,
//! `COMP-3`/`PACKED-DECIMAL`, `COMP`/`COMP-4`/`COMP-5`/`BINARY`), `OCCURS n`
//! (→ LIST), `REDEFINES` (→ folded STRUCT), and `SIGN LEADING/TRAILING
//! [SEPARATE]`. `VALUE` clauses and level-88 condition names are ignored.
//!
//! The [`parse_pic`] helper is shared with the template front-end.

use std::collections::HashMap;

use crate::layout::{Endian, Field, FieldKind, Justify, Layout, NumRepr, SignKind};
use crate::{Error, Result};

/// USAGE of an elementary numeric item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    Display,
    Comp3,
    /// Binary (COMP / COMP-4 / COMP-5 / BINARY).
    Comp,
}

/// Parse a copybook into a [`Layout`].
pub fn parse(src: &str) -> Result<Layout> {
    let stmts = tokenize(src)?;
    let roots = build_tree(stmts)?;
    // If the copybook is a single 01 group, its children are the record fields.
    let record_nodes = if roots.len() == 1 && !roots[0].children.is_empty() {
        roots[0].children.clone()
    } else {
        roots
    };
    let (fields, _w) = layout_nodes(&record_nodes)?;
    Layout::from_fields(fields)
}

/// A raw parsed statement (one period-terminated clause).
#[derive(Debug, Clone)]
struct Raw {
    level: u8,
    name: String,
    pic: Option<String>,
    usage: Usage,
    occurs: Option<usize>,
    /// `OCCURS … DEPENDING ON name` — the controlling field's name.
    depending_on: Option<String>,
    redefines: Option<String>,
    sign: Option<SignKind>,
    children: Vec<Raw>,
}

/// Split a copybook into raw statements (period-terminated), ignoring blank and
/// comment lines (`*` in column 7, or `*>` inline).
fn tokenize(src: &str) -> Result<Vec<Raw>> {
    let mut cleaned = String::new();
    for line in src.lines() {
        // Drop fixed-format sequence/indicator columns when present: a '*' in
        // column 7 marks a comment line.
        let trimmed = line.trim_start();
        if trimmed.starts_with('*') {
            continue;
        }
        // Strip inline `*>` comments.
        let line = match line.split_once("*>") {
            Some((code, _)) => code,
            None => line,
        };
        cleaned.push_str(line);
        cleaned.push(' ');
    }

    let mut stmts = Vec::new();
    for raw in cleaned.split('.') {
        let toks: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
        if toks.is_empty() {
            continue;
        }
        if let Some(stmt) = parse_stmt(&toks)? {
            stmts.push(stmt);
        }
    }
    Ok(stmts)
}

fn parse_stmt(toks: &[String]) -> Result<Option<Raw>> {
    let level: u8 = toks[0]
        .parse()
        .map_err(|_| Error(format!("invalid level number {:?}", toks[0])))?;
    // Level-88 condition names carry no storage.
    if level == 88 {
        return Ok(None);
    }
    let name = toks.get(1).cloned().unwrap_or_else(|| "FILLER".into());

    let mut pic = None;
    let mut usage = Usage::Display;
    let mut occurs = None;
    let mut depending_on = None;
    let mut redefines = None;
    let mut sign: Option<SignKind> = None;

    let upper: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let mut i = 2;
    while i < toks.len() {
        match upper[i].as_str() {
            "PIC" | "PICTURE" => {
                // Optional "IS" then the picture string.
                let mut j = i + 1;
                if j < upper.len() && upper[j] == "IS" {
                    j += 1;
                }
                pic = toks.get(j).cloned();
                i = j + 1;
            }
            "REDEFINES" => {
                redefines = toks.get(i + 1).cloned();
                i += 2;
            }
            "OCCURS" => {
                // OCCURS integer-1 [TO integer-2] [TIMES] [DEPENDING ON name].
                // The element count used for the LIST/static reservation is the
                // maximum (integer-2 if a range is given, else integer-1).
                let mut j = i + 1;
                let first = toks
                    .get(j)
                    .and_then(|t| t.parse::<usize>().ok())
                    .ok_or_else(|| Error("OCCURS requires a count".into()))?;
                j += 1;
                let mut max = first;
                if upper.get(j).map(|s| s == "TO").unwrap_or(false) {
                    max = toks
                        .get(j + 1)
                        .and_then(|t| t.parse::<usize>().ok())
                        .ok_or_else(|| Error("OCCURS … TO requires an upper bound".into()))?;
                    j += 2;
                }
                if upper.get(j).map(|s| s == "TIMES").unwrap_or(false) {
                    j += 1;
                }
                if upper.get(j).map(|s| s == "DEPENDING").unwrap_or(false) {
                    j += 1;
                    if upper.get(j).map(|s| s == "ON").unwrap_or(false) {
                        j += 1;
                    }
                    let ctrl = toks
                        .get(j)
                        .cloned()
                        .ok_or_else(|| Error("DEPENDING ON requires a field name".into()))?;
                    depending_on = Some(ctrl);
                    j += 1;
                }
                occurs = Some(max);
                i = j;
            }
            "USAGE" => {
                i += 1; // "USAGE" optionally followed by "IS"
                if i < upper.len() && upper[i] == "IS" {
                    i += 1;
                }
                // The usage keyword itself is handled by the generic arm below.
            }
            "COMP-3" | "COMPUTATIONAL-3" | "PACKED-DECIMAL" => {
                usage = Usage::Comp3;
                i += 1;
            }
            "COMP" | "COMPUTATIONAL" | "COMP-4" | "COMPUTATIONAL-4" | "COMP-5"
            | "COMPUTATIONAL-5" | "BINARY" => {
                usage = Usage::Comp;
                i += 1;
            }
            "SIGN" => {
                // SIGN [IS] LEADING|TRAILING [SEPARATE]
                let mut j = i + 1;
                if j < upper.len() && upper[j] == "IS" {
                    j += 1;
                }
                let leading = upper.get(j).map(|s| s == "LEADING").unwrap_or(false);
                let separate = upper.iter().skip(j).any(|s| s == "SEPARATE");
                sign = Some(match (leading, separate) {
                    (true, true) => SignKind::LeadingSeparate,
                    (false, true) => SignKind::TrailingSeparate,
                    // Non-separate sign → overpunch (handled as zoned).
                    _ => SignKind::Embedded,
                });
                i = j + 1;
            }
            "VALUE" | "VALUES" => {
                // Skip the value literal(s) up to end of statement.
                break;
            }
            _ => {
                i += 1;
            }
        }
    }

    Ok(Some(Raw {
        level,
        name,
        pic,
        usage,
        occurs,
        depending_on,
        redefines,
        sign,
        children: Vec::new(),
    }))
}

/// Assemble the flat statement list into a tree using level numbers.
fn build_tree(stmts: Vec<Raw>) -> Result<Vec<Raw>> {
    let mut roots: Vec<Raw> = Vec::new();
    // Track the current ancestor path as index chains into `roots`, alongside
    // the COBOL level number at each depth.
    let mut path: Vec<usize> = Vec::new();
    let mut levels: Vec<u8> = Vec::new();

    for stmt in stmts {
        while let Some(&lvl) = levels.last() {
            if lvl >= stmt.level {
                levels.pop();
                path.pop();
            } else {
                break;
            }
        }
        if path.is_empty() {
            roots.push(stmt.clone());
            path.push(roots.len() - 1);
            levels.push(stmt.level);
        } else {
            // Navigate to the current parent and push as its child.
            let parent = navigate(&mut roots, &path);
            parent.children.push(stmt.clone());
            let child_idx = parent.children.len() - 1;
            path.push(child_idx);
            levels.push(stmt.level);
        }
    }
    Ok(roots)
}

/// Follow `path` (root index, then child indices) to a mutable node.
fn navigate<'a>(roots: &'a mut [Raw], path: &[usize]) -> &'a mut Raw {
    let mut node = &mut roots[path[0]];
    for &idx in &path[1..] {
        node = &mut node.children[idx];
    }
    node
}

/// Compute offsets and build [`Field`]s for a sibling list (offsets relative to
/// the siblings' start). Returns the fields and the consumed width.
fn layout_nodes(nodes: &[Raw]) -> Result<(Vec<Field>, usize)> {
    let mut fields: Vec<Field> = Vec::new();
    let mut name_off: HashMap<String, usize> = HashMap::new();
    let mut cursor = 0usize;
    let mut max_end = 0usize;

    for node in nodes {
        let (kind, width) = if node.children.is_empty() {
            elementary(node)?
        } else {
            let (children, gw) = layout_nodes(&node.children)?;
            (FieldKind::Group(children), gw)
        };

        let offset = match &node.redefines {
            Some(target) => *name_off.get(&target.to_ascii_uppercase()).ok_or_else(|| {
                Error(format!(
                    "REDEFINES target {target:?} not found before {:?}",
                    node.name
                ))
            })?,
            None => cursor,
        };

        let total = width * node.occurs.unwrap_or(1);
        name_off.insert(node.name.to_ascii_uppercase(), offset);

        fields.push(Field {
            name: node.name.clone(),
            offset,
            width,
            kind,
            occurs: node.occurs,
            depending_on: node.depending_on.clone(),
            redefines: node.redefines.clone(),
        });

        // An `OCCURS … DEPENDING ON` table reserves no static footprint (the body
        // is positioned dynamically), so following siblings start right after the
        // table's offset; a fixed table advances by its full width.
        let reserved = if node.depending_on.is_some() {
            0
        } else {
            total
        };
        if node.redefines.is_none() {
            cursor = offset + reserved;
        }
        max_end = max_end.max(offset + reserved);
    }

    Ok((fold_redefines(fields), max_end))
}

/// Fold each base field together with the fields that REDEFINE it into a single
/// STRUCT-rendered group (named after the base), per the chosen mapping.
fn fold_redefines(fields: Vec<Field>) -> Vec<Field> {
    // Map base name → group index in the output.
    let mut out: Vec<Field> = Vec::new();
    let mut base_idx: HashMap<String, usize> = HashMap::new();

    for field in fields {
        match &field.redefines {
            None => {
                base_idx.insert(field.name.to_ascii_uppercase(), out.len());
                out.push(field);
            }
            Some(target) => {
                let key = target.to_ascii_uppercase();
                if let Some(&idx) = base_idx.get(&key) {
                    let base = out[idx].clone();
                    let group_offset = base.offset;
                    // Start (or extend) a group at `idx`.
                    let mut members: Vec<Field> = match &out[idx].kind {
                        FieldKind::Group(ch)
                            if out[idx].name == base.name && is_synthetic_union(&out[idx]) =>
                        {
                            ch.clone()
                        }
                        _ => vec![rebase(base.clone(), group_offset)],
                    };
                    members.push(rebase(field.clone(), group_offset));
                    let width = members.iter().map(|m| m.total_width()).max().unwrap_or(0);
                    out[idx] = Field {
                        name: out[idx].name.clone(),
                        offset: group_offset,
                        width,
                        kind: FieldKind::Group(members),
                        occurs: None,
                        depending_on: None,
                        redefines: None,
                    };
                } else {
                    // Unknown target — keep as-is (validation happened earlier).
                    out.push(field);
                }
            }
        }
    }
    out
}

/// A group we created purely to union REDEFINES variants is marked by having a
/// child whose name equals the group name (the base).
fn is_synthetic_union(group: &Field) -> bool {
    matches!(&group.kind, FieldKind::Group(ch) if ch.iter().any(|c| c.name == group.name))
}

/// Shift a field to be relative to `group_offset`, clearing its redefines link.
fn rebase(mut field: Field, group_offset: usize) -> Field {
    field.offset -= group_offset;
    field.redefines = None;
    field
}

/// Resolve an elementary item's `(FieldKind, width)` from its PIC + usage.
fn elementary(node: &Raw) -> Result<(FieldKind, usize)> {
    let pic = node
        .pic
        .as_deref()
        .ok_or_else(|| Error(format!("elementary item {:?} has no PIC clause", node.name)))?;
    parse_pic(pic, node.usage, node.sign)
}

/// Parse a PIC clause + USAGE into a [`FieldKind`] and byte width. Shared with
/// the template front-end (which always passes [`Usage::Display`]).
pub fn parse_pic(pic: &str, usage: Usage, sign: Option<SignKind>) -> Result<(FieldKind, usize)> {
    let p = PicShape::parse(pic)?;

    if p.text {
        return Ok((
            FieldKind::Text {
                justify: Justify::Left,
                trim: true,
                pad: b' ',
            },
            p.char_count,
        ));
    }

    let digits = p.int_digits + p.frac_digits;
    let scale = p.frac_digits as u8;
    let signed = p.signed;
    let sign_kind = resolve_sign(p.signed, sign);
    let sep_bytes = match sign_kind {
        SignKind::LeadingSeparate | SignKind::TrailingSeparate => 1,
        _ => 0,
    };

    match usage {
        Usage::Comp3 => Ok((
            FieldKind::Decimal {
                precision: digits as u8,
                scale,
                repr: NumRepr::Comp3,
                sign: SignKind::Embedded,
            },
            crate::packed::byte_width(digits as u8),
        )),
        Usage::Comp => {
            let width = comp_width(digits);
            Ok((
                FieldKind::Binary {
                    endian: Endian::Big,
                    signed,
                },
                width,
            ))
        }
        Usage::Display => {
            if scale == 0 {
                if signed && matches!(sign_kind, SignKind::Embedded) {
                    // Overpunch sign on the last digit → zoned.
                    Ok((
                        FieldKind::Decimal {
                            precision: digits as u8,
                            scale: 0,
                            repr: NumRepr::Zoned,
                            sign: SignKind::Embedded,
                        },
                        digits,
                    ))
                } else {
                    Ok((
                        FieldKind::Int {
                            signed,
                            sign: sign_kind,
                        },
                        digits + sep_bytes,
                    ))
                }
            } else {
                let repr = if signed && matches!(sign_kind, SignKind::Embedded) {
                    NumRepr::Zoned
                } else {
                    NumRepr::Display
                };
                Ok((
                    FieldKind::Decimal {
                        precision: digits as u8,
                        scale,
                        repr,
                        sign: sign_kind,
                    },
                    digits + sep_bytes,
                ))
            }
        }
    }
}

fn resolve_sign(signed: bool, explicit: Option<SignKind>) -> SignKind {
    match (signed, explicit) {
        (false, _) => SignKind::Unsigned,
        (true, Some(s)) => s,
        (true, None) => SignKind::Embedded,
    }
}

/// COMP binary width by total digit count (COBOL standard).
fn comp_width(digits: usize) -> usize {
    match digits {
        0..=4 => 2,
        5..=9 => 4,
        _ => 8,
    }
}

/// The decomposed shape of a PIC string.
struct PicShape {
    text: bool,
    char_count: usize,
    int_digits: usize,
    frac_digits: usize,
    signed: bool,
}

impl PicShape {
    fn parse(pic: &str) -> Result<PicShape> {
        let pic = pic.trim().to_ascii_uppercase();
        let mut text = false;
        let mut char_count = 0;
        let mut int_digits = 0;
        let mut frac_digits = 0;
        let mut signed = false;
        let mut after_v = false;

        let chars: Vec<char> = pic.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            // A symbol may be followed by a repeat count in parentheses.
            let count = if i + 1 < chars.len() && chars[i + 1] == '(' {
                let close = chars[i + 1..]
                    .iter()
                    .position(|&c| c == ')')
                    .ok_or_else(|| Error(format!("unbalanced parentheses in PIC {pic:?}")))?;
                let num: String = chars[i + 2..i + 1 + close].iter().collect();
                let n: usize = num
                    .trim()
                    .parse()
                    .map_err(|_| Error(format!("invalid PIC repeat count {num:?}")))?;
                i += 1 + close + 1;
                n
            } else {
                i += 1;
                1
            };

            match c {
                'X' | 'A' => {
                    text = true;
                    char_count += count;
                }
                '9' => {
                    if after_v {
                        frac_digits += count;
                    } else {
                        int_digits += count;
                    }
                }
                'S' => signed = true,
                'V' => after_v = true,
                'P' => {
                    /* implied scaling position; treat as fractional pad */
                    frac_digits += count;
                }
                'Z' | '*' | '0' | ',' | '/' | 'B' => {
                    // Edited numeric symbols: approximate as display digit positions.
                    if after_v {
                        frac_digits += count;
                    } else {
                        int_digits += count;
                    }
                }
                '+' | '-' | '$' | '.' | 'C' | 'R' | 'D' => {
                    // Sign/insertion symbols in edited pictures — ignore extent.
                }
                _ => return Err(Error(format!("unsupported PIC symbol {c:?} in {pic:?}"))),
            }
        }

        if text && (int_digits + frac_digits) > 0 {
            return Err(Error(format!(
                "mixed text/numeric PIC not supported: {pic:?}"
            )));
        }
        Ok(PicShape {
            text,
            char_count,
            int_digits,
            frac_digits,
            signed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pic_text() {
        let (k, w) = parse_pic("X(10)", Usage::Display, None).unwrap();
        assert_eq!(w, 10);
        assert!(matches!(k, FieldKind::Text { .. }));
    }

    #[test]
    fn pic_unsigned_int() {
        let (k, w) = parse_pic("9(5)", Usage::Display, None).unwrap();
        assert_eq!(w, 5);
        assert!(matches!(k, FieldKind::Int { signed: false, .. }));
    }

    #[test]
    fn pic_comp3_decimal() {
        let (k, w) = parse_pic("S9(7)V99", Usage::Comp3, None).unwrap();
        assert_eq!(w, 5); // 9 digits -> 5 bytes
        match k {
            FieldKind::Decimal {
                precision,
                scale,
                repr,
                ..
            } => {
                assert_eq!(precision, 9);
                assert_eq!(scale, 2);
                assert_eq!(repr, NumRepr::Comp3);
            }
            _ => panic!("expected decimal"),
        }
    }

    #[test]
    fn pic_comp_binary_width() {
        let (k, w) = parse_pic("S9(9)", Usage::Comp, None).unwrap();
        assert_eq!(w, 4);
        assert!(matches!(k, FieldKind::Binary { signed: true, .. }));
        let (_k, w) = parse_pic("9(18)", Usage::Comp, None).unwrap();
        assert_eq!(w, 8);
    }

    #[test]
    fn simple_record() {
        let src = "01 REC.\n  05 ID    PIC X(4).\n  05 QTY   PIC 9(3).\n";
        let layout = parse(src).unwrap();
        assert_eq!(layout.record_len, 7);
        assert_eq!(layout.fields.len(), 2);
        assert_eq!(layout.fields[0].name, "ID");
        assert_eq!(layout.fields[0].offset, 0);
        assert_eq!(layout.fields[1].offset, 4);
    }

    #[test]
    fn group_becomes_struct() {
        let src = "01 REC.
          05 ITEM.
            10 SKU PIC X(5).
            10 QTY PIC 9(3).";
        let layout = parse(src).unwrap();
        assert_eq!(layout.fields.len(), 1);
        match &layout.fields[0].kind {
            FieldKind::Group(children) => {
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].offset, 0);
                assert_eq!(children[1].offset, 5);
            }
            _ => panic!("expected group"),
        }
        assert_eq!(layout.record_len, 8);
    }

    #[test]
    fn occurs_carries_through() {
        let src = "01 REC.\n  05 LINES OCCURS 3 PIC 9(4).";
        let layout = parse(src).unwrap();
        assert_eq!(layout.fields[0].occurs, Some(3));
        assert_eq!(layout.record_len, 12);
    }

    #[test]
    fn redefines_folds_into_struct() {
        let src = "01 REC.
          05 RAW PIC X(8).
          05 NUM REDEFINES RAW PIC 9(8).";
        let layout = parse(src).unwrap();
        assert_eq!(layout.fields.len(), 1);
        assert_eq!(layout.record_len, 8);
        match &layout.fields[0].kind {
            FieldKind::Group(children) => {
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].name, "RAW");
                assert_eq!(children[1].name, "NUM");
                // Both overlay the same bytes (offset 0 within the group).
                assert_eq!(children[0].offset, 0);
                assert_eq!(children[1].offset, 0);
            }
            _ => panic!("expected folded group"),
        }
    }

    #[test]
    fn occurs_depending_on_parses() {
        let src = "01 REC.
          05 N PIC 9(2).
          05 ITEMS OCCURS 1 TO 5 TIMES DEPENDING ON N PIC X(3).";
        let layout = parse(src).unwrap();
        assert!(layout.variable);
        // The table reserves no static footprint — record_len is the minimum
        // (just the count field) before any ODO body.
        assert_eq!(layout.record_len, 2);
        let items = &layout.fields[1];
        assert_eq!(items.occurs, Some(5)); // declared maximum
        assert_eq!(items.depending_on.as_deref(), Some("N"));
    }

    #[test]
    fn occurs_depending_on_decodes_variable_length() {
        use crate::decode::decode_record;
        use crate::value::Value;
        use crate::Encoding;
        let src = "01 REC.
          05 N PIC 9(1).
          05 ITEMS OCCURS 1 TO 9 TIMES DEPENDING ON N PIC X(2).
          05 TRAILER PIC X(3).";
        let layout = parse(src).unwrap();

        // N=2 → two 2-byte items, then the trailer shifts to follow them.
        let out = decode_record(&layout, b"2AABBEND", Encoding::Ascii).unwrap();
        assert_eq!(out[0].1, Value::Int(2));
        assert_eq!(
            out[1].1,
            Value::List(vec![Value::Text("AA".into()), Value::Text("BB".into())])
        );
        assert_eq!(out[2].1, Value::Text("END".into()));

        // N=0 → empty table; the trailer immediately follows the count.
        let out = decode_record(&layout, b"0ZZZ", Encoding::Ascii).unwrap();
        assert_eq!(out[1].1, Value::List(vec![]));
        assert_eq!(out[2].1, Value::Text("ZZZ".into()));
    }

    #[test]
    fn occurs_depending_on_round_trips() {
        use crate::decode::decode_record;
        use crate::encode::encode_record;
        use crate::Encoding;
        let src = "01 REC.
          05 N PIC 9(1).
          05 ITEMS OCCURS 1 TO 9 TIMES DEPENDING ON N PIC X(2).
          05 TRAILER PIC X(3).";
        let layout = parse(src).unwrap();
        for rec in [&b"3ABCDEFEND"[..], &b"1XYEND"[..], &b"0END"[..]] {
            let decoded = decode_record(&layout, rec, Encoding::Ascii).unwrap();
            let reenc = encode_record(&layout, &decoded, Encoding::Ascii).unwrap();
            assert_eq!(reenc, rec, "round-trip failed for {rec:?}");
        }
    }
}
