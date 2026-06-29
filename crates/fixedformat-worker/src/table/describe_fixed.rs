//! `describe_fixed(spec [, format =>])` — introspect a layout spec without
//! reading any data. Returns one row per field (groups and their children) with
//! the resolved DuckDB column type, byte offset, width, and OCCURS info. Handy
//! for debugging a template / JSON / copybook spec before pointing it at a file.

use std::sync::Arc;

use arrow_array::builder::{Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field as ArrowField, Schema, SchemaRef};
use fixedformat_core::describe::{describe, FieldDesc};
use vgi::table_function::{TableFunction, TableProducer};
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams};
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::options;

pub struct DescribeFixed;

/// The fixed output schema (same for every call).
fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        ArrowField::new("path", DataType::Utf8, false),
        ArrowField::new("depth", DataType::Int64, false),
        ArrowField::new("kind", DataType::Utf8, false),
        ArrowField::new("sql_type", DataType::Utf8, false),
        ArrowField::new("byte_offset", DataType::Int64, false),
        ArrowField::new("width", DataType::Int64, false),
        ArrowField::new("occurs", DataType::Int64, true),
        ArrowField::new("depending_on", DataType::Utf8, true),
    ]))
}

impl TableFunction for DescribeFixed {
    fn name(&self) -> &str {
        "describe_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        let tags = crate::meta::object_tags(
            "Describe Fixed-Width Spec",
            "Introspect a fixed-width layout `spec` without reading any data: returns one row per \
             field (group items and their children included) describing how the spec resolves. \
             Columns: `path` (dotted field path, e.g. `item.sku`), `depth` (nesting level), `kind` \
             (codec label, e.g. `text`, `int32 LE`, `comp-3`), `sql_type` (the DuckDB column type \
             the field maps to, e.g. `VARCHAR`, `DECIMAL(9,2)`, `STRUCT`, `BIGINT[]`), `byte_offset` and \
             `width` (static byte position and per-occurrence width), `occurs` (declared repeat / \
             OCCURS maximum, else NULL), and `depending_on` (the controlling field for `OCCURS … \
             DEPENDING ON`, else NULL). `spec` is a Perl/Python `unpack` template, a JSON field \
             list, or a COBOL copybook (auto-detected unless you pass `format =>` \
             'template'/'json'/'copybook'). Use it to debug a spec, document a layout, or check \
             field offsets before running `read_fixed`. For a variable-length layout (OCCURS \
             DEPENDING ON) the reported offsets are the static positions before the table.",
            "Describe how a fixed-width layout `spec` resolves — one row per field with its dotted \
             path, codec kind, DuckDB type, byte offset, width, OCCURS count, and DEPENDING ON \
             controller. Reads no data. `format =>` forces template/json/copybook.",
            "describe, introspect, layout, schema, fields, offsets, copybook, template, JSON spec, \
             debug spec, fixed-width, OCCURS, DEPENDING ON",
        );
        FunctionMetadata {
            description: "Describe a fixed-width layout spec (fields, types, offsets) without \
                          reading data"
                .into(),
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![
            ArgSpec::const_arg(
                "spec",
                0,
                "varchar",
                "The record layout to describe: a Perl/Python `unpack` template, a JSON field \
                 list, or a COBOL copybook. Format is auto-detected unless `format` is given.",
            ),
            ArgSpec::const_arg(
                "format",
                -1,
                "varchar",
                "Force how `spec` is interpreted: 'template', 'json', or 'copybook'. Omit to \
                 auto-detect.",
            ),
        ]
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        // Parse now to surface spec errors at bind time (and validate `format`).
        options::layout(&params.arguments, 0)?;
        Ok(BindResponse {
            output_schema: schema(),
            opaque_data: Vec::new(),
        })
    }

    fn producer(&self, params: &ProcessParams) -> Result<Box<dyn TableProducer>> {
        let layout = options::layout(&params.arguments, 0)?;
        Ok(Box::new(DescribeProducer {
            schema: params.output_schema.clone(),
            rows: Some(describe(&layout)),
        }))
    }
}

struct DescribeProducer {
    schema: SchemaRef,
    /// `Some` until the single batch has been emitted, then `None`.
    rows: Option<Vec<FieldDesc>>,
}

impl TableProducer for DescribeProducer {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        let Some(rows) = self.rows.take() else {
            return Ok(None);
        };

        let mut path = StringBuilder::new();
        let mut depth = Int64Builder::new();
        let mut kind = StringBuilder::new();
        let mut sql_type = StringBuilder::new();
        let mut offset = Int64Builder::new();
        let mut width = Int64Builder::new();
        let mut occurs = Int64Builder::new();
        let mut depending_on = StringBuilder::new();

        for r in &rows {
            path.append_value(&r.path);
            depth.append_value(r.depth as i64);
            kind.append_value(&r.kind);
            sql_type.append_value(&r.sql_type);
            offset.append_value(r.offset as i64);
            width.append_value(r.width as i64);
            match r.occurs {
                Some(n) => occurs.append_value(n as i64),
                None => occurs.append_null(),
            }
            match &r.depending_on {
                Some(c) => depending_on.append_value(c),
                None => depending_on.append_null(),
            }
        }

        let columns: Vec<ArrayRef> = vec![
            Arc::new(path.finish()),
            Arc::new(depth.finish()),
            Arc::new(kind.finish()),
            Arc::new(sql_type.finish()),
            Arc::new(offset.finish()),
            Arc::new(width.finish()),
            Arc::new(occurs.finish()),
            Arc::new(depending_on.finish()),
        ];
        Ok(Some(
            RecordBatch::try_new(self.schema.clone(), columns)
                .map_err(|e| RpcError::runtime_error(e.to_string()))?,
        ))
    }
}
