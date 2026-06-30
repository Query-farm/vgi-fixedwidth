//! Convert Arrow input array elements into the core [`Value`] tree (used by
//! `pack` / `write_fixed` to read the STRUCT/relation columns being formatted).

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Decimal128Type, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type,
    Int8Type, Time64MicrosecondType, TimestampMicrosecondType, UInt16Type, UInt32Type, UInt64Type,
    UInt8Type,
};
use arrow_array::{Array, ArrayRef, UnionArray};
use arrow_schema::{DataType, TimeUnit};
use fixedformat_core::Value;
use vgi_rpc::{Result, RpcError};

fn rt(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

/// Read element `row` of a **sparse `UNION`** column as `(variant tag, struct
/// fields)`. The per-row `type_id` selects the active variant; it is mapped to a
/// child via the schema's [`arrow_schema::UnionFields`] (the child field NAME is
/// the variant tag), and that child's `StructArray` row is read as the variant's
/// `(name, value)` fields. Used by `write_multi` to recover each row's record
/// type and field values from the heterogeneous UNION input relation.
pub fn union_at(array: &ArrayRef, row: usize) -> Result<(String, Vec<(String, Value)>)> {
    let DataType::Union(union_fields, _mode) = array.data_type() else {
        return Err(rt(format!(
            "expected a UNION column, got {:?}",
            array.data_type()
        )));
    };
    let ua = array
        .as_any()
        .downcast_ref::<UnionArray>()
        .ok_or_else(|| rt("UNION column is not backed by a UnionArray"))?;
    let type_id = ua.type_id(row);
    // Map the active row's type-id to its variant tag (the union child's field
    // name). UnionFields is keyed by type-id, not positional index.
    let tag = union_fields
        .iter()
        .find(|(tid, _)| *tid == type_id)
        .map(|(_, f)| f.name().to_string())
        .ok_or_else(|| rt(format!("UNION type-id {type_id} has no matching variant")))?;
    // The active variant's child (sparse: full-length, the struct on this row).
    let child = ua.child(type_id);
    match value_at(child, row)? {
        Value::Struct(pairs) => Ok((tag, pairs)),
        Value::Null => Ok((tag, Vec::new())),
        other => Err(rt(format!(
            "UNION variant {tag:?} is not a struct: {other:?}"
        ))),
    }
}

/// Read element `row` of `array` as a core [`Value`].
pub fn value_at(array: &ArrayRef, row: usize) -> Result<Value> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    Ok(match array.data_type() {
        DataType::Utf8 => Value::Text(array.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Value::Text(array.as_string::<i64>().value(row).to_string()),
        DataType::Boolean => Value::Bool(array.as_boolean().value(row)),
        DataType::Int8 => Value::Int(array.as_primitive::<Int8Type>().value(row) as i64),
        DataType::Int16 => Value::Int(array.as_primitive::<Int16Type>().value(row) as i64),
        DataType::Int32 => Value::Int(array.as_primitive::<Int32Type>().value(row) as i64),
        DataType::Int64 => Value::Int(array.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Value::Int(array.as_primitive::<UInt8Type>().value(row) as i64),
        DataType::UInt16 => Value::Int(array.as_primitive::<UInt16Type>().value(row) as i64),
        DataType::UInt32 => Value::Int(array.as_primitive::<UInt32Type>().value(row) as i64),
        DataType::UInt64 => Value::Int(array.as_primitive::<UInt64Type>().value(row) as i64),
        DataType::Float32 => Value::Float(array.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float64 => Value::Float(array.as_primitive::<Float64Type>().value(row)),
        DataType::Decimal128(_, scale) => Value::Decimal {
            unscaled: array.as_primitive::<Decimal128Type>().value(row),
            scale: (*scale).max(0) as u8,
        },
        DataType::Date32 => Value::Date(array.as_primitive::<Date32Type>().value(row)),
        DataType::Time64(TimeUnit::Microsecond) => {
            Value::Time(array.as_primitive::<Time64MicrosecondType>().value(row))
        }
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            Value::Timestamp(array.as_primitive::<TimestampMicrosecondType>().value(row))
        }
        DataType::List(_) => {
            let list = array.as_list::<i32>();
            let items = list.value(row);
            let mut out = Vec::with_capacity(items.len());
            for i in 0..items.len() {
                out.push(value_at(&items, i)?);
            }
            Value::List(out)
        }
        DataType::Struct(fields) => {
            let sa = array.as_struct();
            let mut pairs = Vec::with_capacity(fields.len());
            for (i, f) in fields.iter().enumerate() {
                pairs.push((f.name().to_string(), value_at(sa.column(i), row)?));
            }
            Value::Struct(pairs)
        }
        other => return Err(rt(format!("unsupported input type {other:?}"))),
    })
}
