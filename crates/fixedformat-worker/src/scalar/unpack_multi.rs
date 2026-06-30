//! `unpack_multi(rec, spec [, encoding])` — parse one heterogeneous record into a
//! DuckDB **UNION** value, the scalar counterpart of `read_multi`.
//!
//! The multi-record `spec` is a bind-time constant, so the UNION output type (one
//! STRUCT variant per record type) is resolved in `on_bind`. `rec` may be VARCHAR
//! or BLOB. The discriminator bytes of each record pick the variant to decode it
//! with; the result is a UNION whose tag is the record type
//! (`union_tag(...)` / `union_extract(..., 'TAG')`).

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType, UnionMode};
use fixedformat_core::decode::decode_record;
use fixedformat_core::Value;
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams, ScalarFunction};
use vgi_rpc::{Result, RpcError};

use crate::options;
use crate::table::read_multi::{build_union_array, multi_layout, union_fields};

/// `unpack_multi`. Like `unpack_fixed`, the optional `encoding` is a 3rd
/// positional const registered as a separate arity overload.
pub struct UnpackMulti {
    pub with_encoding: bool,
}

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

impl ScalarFunction for UnpackMulti {
    fn name(&self) -> &str {
        "unpack_multi"
    }

    fn metadata(&self) -> FunctionMetadata {
        let description = if self.with_encoding {
            "Parse one heterogeneous (multi-record-type) record into a DuckDB UNION using the \
             multi-record spec and byte encoding (ascii or ebcdic)"
        } else {
            "Parse one heterogeneous (multi-record-type) record into a DuckDB UNION using the \
             multi-record spec, assuming ASCII bytes"
        };
        let mut tags = crate::meta::object_tags(
            if self.with_encoding {
                "Unpack Multi-Record Record (with encoding)"
            } else {
                "Unpack Multi-Record Record"
            },
            "Decode a single record from a heterogeneous (multi-record-type) feed into a DuckDB \
             UNION value — the scalar counterpart of read_multi. The `spec` is the same \
             multi-record JSON object (a `discriminator` of {offset, width} plus a `records` map of \
             record-type tag → JSON field list); the record's discriminator bytes pick the variant \
             to decode it with. The result is a UNION with one STRUCT variant per record type, \
             named by the discriminator tag — use `union_tag(...)` for the record type and \
             `union_extract(..., 'TAG')` to pull a variant's STRUCT. An unmatched discriminator \
             value errors unless the spec gives a `default` tag. The 3-argument overload adds a \
             positional `encoding` ('ascii' default, or 'ebcdic'/CP037, which also governs the \
             discriminator bytes). The spec is a bind-time constant so the UNION type is known at \
             plan time.",
            "Parse one multi-record-type record into a UNION value, e.g. \
             `union_tag(unpack_multi(rec, '<spec>'))`. The JSON `spec` declares a `discriminator` \
             and a `records` map of tag → field list. Optional 3rd positional `encoding` \
             ('ascii'/'ebcdic'). The scalar counterpart of read_multi.",
            "unpack multi, multi-record, heterogeneous, discriminator, record type, union, decode, \
             copybook, fixed-width, mainframe, record to union",
        );
        tags.push((
            "vgi.example_queries".into(),
            r#"[
  {
    "description": "Decode one detail record into a UNION and read its tag + a field.",
    "sql": "SELECT union_tag(u) AS kind, union_extract(u, 'D').sku AS sku FROM (SELECT fixed.main.unpack_multi('DWIDGET    00042', '{\"discriminator\":{\"offset\":0,\"width\":1},\"records\":{\"H\":[{\"type\":\"pad\",\"width\":1},{\"name\":\"co\",\"type\":\"str\",\"width\":20}],\"D\":[{\"type\":\"pad\",\"width\":1},{\"name\":\"sku\",\"type\":\"str\",\"width\":10},{\"name\":\"qty\",\"type\":\"int\",\"digits\":5}]}}') AS u)"
  }
]"#
            .into(),
        ));
        FunctionMetadata {
            description: description.into(),
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        let mut specs = vec![
            ArgSpec::any_column(
                "rec",
                0,
                "A single multi-record-type record to decode — one row's bytes, whose discriminator \
                 field picks the layout it is decoded with.",
            ),
            ArgSpec::const_arg(
                "spec",
                1,
                "varchar",
                "The multi-record JSON layout: a `discriminator` ({offset, width}) plus a `records` \
                 map of record-type tag → JSON field list. Determines the UNION variants.",
            ),
        ];
        if self.with_encoding {
            specs.push(ArgSpec::const_arg(
                "encoding",
                2,
                "varchar",
                "Byte encoding of the record: 'ascii' (the default) or 'ebcdic' (CP037).",
            ));
        }
        specs
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let ml = multi_layout(&params.arguments)?;
        let uf = union_fields(&ml)?;
        Ok(BindResponse::result(DataType::Union(uf, UnionMode::Sparse)))
    }

    fn process(&self, params: &ProcessParams, batch: &RecordBatch) -> Result<RecordBatch> {
        let ml = multi_layout(&params.arguments)?;
        let enc = options::encoding_at(&params.arguments, 2)?;
        let uf = union_fields(&ml)?;

        let rec = batch.column(0);
        let nrows = batch.num_rows();
        let mut rows: Vec<(usize, Value)> = Vec::with_capacity(nrows);
        for i in 0..nrows {
            if rec.is_null(i) {
                // No discriminator to read — emit a NULL value under variant 0.
                rows.push((0, Value::Null));
                continue;
            }
            let bytes = record_bytes(rec, i)?;
            let (vidx, layout) = ml.select(bytes, enc).map_err(ve)?;
            let pairs = decode_record(layout, bytes, enc).map_err(ve)?;
            rows.push((vidx, Value::Struct(pairs)));
        }

        let out: ArrayRef = Arc::new(build_union_array(&uf, rows)?);
        RecordBatch::try_new(params.output_schema.clone(), vec![out])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}

/// Borrow the raw bytes of a record cell, accepting VARCHAR or BLOB columns.
fn record_bytes(rec: &ArrayRef, i: usize) -> Result<&[u8]> {
    match rec.data_type() {
        DataType::Utf8 => Ok(rec.as_string::<i32>().value(i).as_bytes()),
        DataType::LargeUtf8 => Ok(rec.as_string::<i64>().value(i).as_bytes()),
        DataType::Binary => Ok(rec.as_binary::<i32>().value(i)),
        DataType::LargeBinary => Ok(rec.as_binary::<i64>().value(i)),
        other => Err(ve(format!(
            "unpack_multi: rec must be VARCHAR or BLOB, got {other:?}"
        ))),
    }
}
