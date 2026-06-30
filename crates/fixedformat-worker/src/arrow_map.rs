//! Bridge between `fixedformat-core` and Arrow: [`Layout`] → Arrow `Fields`, and
//! decoded [`Value`] columns → Arrow arrays (including `Decimal128`, `List`, and
//! `Struct` for OCCURS / group / folded-REDEFINES fields).

use std::sync::Arc;

use arrow_array::builder::{
    BooleanBuilder, Decimal128Builder, Float32Builder, Float64Builder, Int64Builder, StringBuilder,
};
use arrow_array::{ArrayRef, StructArray};
use arrow_buffer::{NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow_schema::{DataType, Field as ArrowField, Fields};
use fixedformat_core::layout::{Field, FieldKind, Layout};
use fixedformat_core::Value;
use vgi_rpc::{Result, RpcError};

/// DuckDB DECIMAL caps precision at 38.
const MAX_DECIMAL_PRECISION: u8 = 38;

fn rt(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

/// Arrow output fields for a layout's top-level (non-pad) fields.
pub fn layout_fields(layout: &Layout) -> Result<Fields> {
    let mut out = Vec::new();
    for f in &layout.fields {
        if matches!(f.kind, FieldKind::Pad { .. }) {
            continue;
        }
        out.push(Arc::new(arrow_field(f)?));
    }
    Ok(Fields::from(out))
}

/// The Arrow STRUCT type for a whole layout's top-level (non-pad) fields. Used as
/// a `read_multi` union variant — each record type becomes a STRUCT variant whose
/// children are the variant layout's columns. `build_array` already turns a column
/// of `Value::Struct` into the matching `StructArray`, so no extra builder is
/// needed on the value side.
pub fn layout_struct_type(layout: &Layout) -> Result<DataType> {
    Ok(DataType::Struct(layout_fields(layout)?))
}

/// The Arrow `Field` for one layout field (wrapping in `List` for OCCURS).
fn arrow_field(f: &Field) -> Result<ArrowField> {
    let base = base_type(f)?;
    let ty = match f.occurs {
        Some(_) => DataType::List(Arc::new(ArrowField::new("item", base, true))),
        None => base,
    };
    Ok(ArrowField::new(&f.name, ty, true))
}

/// The Arrow type of a single (non-repeated) occurrence of `f`.
fn base_type(f: &Field) -> Result<DataType> {
    Ok(match &f.kind {
        FieldKind::Text { .. } | FieldKind::Hex { .. } => DataType::Utf8,
        FieldKind::Int { .. } | FieldKind::Binary { .. } => DataType::Int64,
        FieldKind::Float { bits: 64, .. } => DataType::Float64,
        FieldKind::Float { .. } => DataType::Float32,
        FieldKind::Bool => DataType::Boolean,
        FieldKind::Pad { .. } => DataType::Null,
        FieldKind::Decimal {
            precision, scale, ..
        } => {
            if *precision > MAX_DECIMAL_PRECISION {
                return Err(rt(format!(
                    "DECIMAL precision {precision} exceeds the maximum of {MAX_DECIMAL_PRECISION} \
                     (DuckDB/Arrow Decimal128 limit) — reduce the field's digit count"
                )));
            }
            DataType::Decimal128((*precision).max(1), *scale as i8)
        }
        FieldKind::Group(children) => {
            let mut fields = Vec::new();
            for c in children {
                if matches!(c.kind, FieldKind::Pad { .. }) {
                    continue;
                }
                fields.push(Arc::new(arrow_field(c)?));
            }
            DataType::Struct(Fields::from(fields))
        }
    })
}

/// Build an Arrow array of `dt` from a column of decoded values (one per row).
pub fn build_array(dt: &DataType, col: &[Value]) -> Result<ArrayRef> {
    match dt {
        DataType::Utf8 => {
            let mut b = StringBuilder::new();
            for v in col {
                match v {
                    Value::Text(s) => b.append_value(s),
                    Value::Null => b.append_null(),
                    other => b.append_value(scalar_text(other)),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Int64 => {
            let mut b = Int64Builder::new();
            for v in col {
                match v {
                    Value::Int(i) => b.append_value(*i),
                    Value::Decimal { unscaled, scale: 0 } => b.append_value(*unscaled as i64),
                    Value::Null => b.append_null(),
                    other => return Err(rt(format!("expected int, got {other:?}"))),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Float64 => {
            let mut b = Float64Builder::new();
            for v in col {
                match v {
                    Value::Float(f) => b.append_value(*f),
                    Value::Int(i) => b.append_value(*i as f64),
                    Value::Null => b.append_null(),
                    other => return Err(rt(format!("expected float, got {other:?}"))),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Float32 => {
            let mut b = Float32Builder::new();
            for v in col {
                match v {
                    Value::Float(f) => b.append_value(*f as f32),
                    Value::Int(i) => b.append_value(*i as f32),
                    Value::Null => b.append_null(),
                    other => return Err(rt(format!("expected float, got {other:?}"))),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Boolean => {
            let mut b = BooleanBuilder::new();
            for v in col {
                match v {
                    Value::Bool(x) => b.append_value(*x),
                    Value::Null => b.append_null(),
                    other => return Err(rt(format!("expected bool, got {other:?}"))),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Decimal128(p, s) => {
            let mut b = Decimal128Builder::new();
            for v in col {
                match v {
                    Value::Decimal { unscaled, scale } => {
                        b.append_value(rescale_i128(*unscaled, *scale, *s as u8))
                    }
                    Value::Int(i) => b.append_value(rescale_i128(*i as i128, 0, *s as u8)),
                    Value::Null => b.append_null(),
                    other => return Err(rt(format!("expected decimal, got {other:?}"))),
                }
            }
            let arr = b.finish().with_precision_and_scale(*p, *s).map_err(rt)?;
            Ok(Arc::new(arr))
        }
        DataType::List(item) => build_list(item, col),
        DataType::Struct(fields) => Ok(Arc::new(build_struct(fields, col)?)),
        other => Err(rt(format!("unsupported output type {other:?}"))),
    }
}

fn build_list(item: &Arc<ArrowField>, col: &[Value]) -> Result<ArrayRef> {
    let mut flat: Vec<Value> = Vec::new();
    let mut offsets: Vec<i32> = Vec::with_capacity(col.len() + 1);
    let mut valid: Vec<bool> = Vec::with_capacity(col.len());
    offsets.push(0);
    let mut total: i32 = 0;
    for v in col {
        match v {
            Value::List(items) => {
                for it in items {
                    flat.push(it.clone());
                }
                total += items.len() as i32;
                valid.push(true);
            }
            Value::Null => valid.push(false),
            other => return Err(rt(format!("expected list, got {other:?}"))),
        }
        offsets.push(total);
    }
    let child = build_array(item.data_type(), &flat)?;
    let nulls = NullBuffer::from(valid);
    let arr = arrow_array::ListArray::new(
        item.clone(),
        OffsetBuffer::new(ScalarBuffer::from(offsets)),
        child,
        Some(nulls),
    );
    Ok(Arc::new(arr))
}

fn build_struct(fields: &Fields, col: &[Value]) -> Result<StructArray> {
    let mut child_cols: Vec<Vec<Value>> = vec![Vec::with_capacity(col.len()); fields.len()];
    let mut valid: Vec<bool> = Vec::with_capacity(col.len());
    for v in col {
        match v {
            Value::Struct(pairs) => {
                for (i, f) in fields.iter().enumerate() {
                    let sub = pairs
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(f.name()))
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Null);
                    child_cols[i].push(sub);
                }
                valid.push(true);
            }
            Value::Null => {
                for c in &mut child_cols {
                    c.push(Value::Null);
                }
                valid.push(false);
            }
            other => return Err(rt(format!("expected struct, got {other:?}"))),
        }
    }
    let arrays: Vec<ArrayRef> = fields
        .iter()
        .zip(&child_cols)
        .map(|(f, c)| build_array(f.data_type(), c))
        .collect::<Result<_>>()?;
    Ok(StructArray::new(
        fields.clone(),
        arrays,
        Some(NullBuffer::from(valid)),
    ))
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Decimal { unscaled, scale } => Value::decimal_string(*unscaled, *scale),
        _ => String::new(),
    }
}

fn rescale_i128(unscaled: i128, from: u8, to: u8) -> i128 {
    use std::cmp::Ordering;
    match from.cmp(&to) {
        Ordering::Equal => unscaled,
        Ordering::Less => unscaled * 10i128.pow((to - from) as u32),
        Ordering::Greater => unscaled / 10i128.pow((from - to) as u32),
    }
}
