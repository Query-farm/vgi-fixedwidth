//! `read_fixed(path, spec [, format =>, encoding =>, framing =>, record_length =>])`
//! — scan a fixed-width file into typed rows.
//!
//! `path` may be a glob. Records are framed per the `framing` option (newline /
//! fixed / RDW), then each field is decoded into a typed column (LIST/STRUCT for
//! OCCURS / group / folded REDEFINES).

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{Schema, SchemaRef};
use fixedformat_core::decode::decode_record;
use fixedformat_core::framing::{records, Framing};
use fixedformat_core::{Encoding, Layout, Value};
use vgi::arguments::Arguments;
use vgi::table_function::{TableFunction, TableProducer};
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams};
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::arrow_map::layout_fields;
use crate::options;

const BATCH_ROWS: usize = 2048;

pub struct ReadFixed;

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

fn output_schema(layout: &Layout) -> Result<SchemaRef> {
    let fields = layout_fields(layout)?;
    Ok(Arc::new(Schema::new(fields)))
}

/// Record length to use for fixed framing (override or layout-derived).
fn record_length(args: &Arguments, layout: &Layout) -> usize {
    args.named_i64("record_length")
        .map(|n| n as usize)
        .unwrap_or(layout.record_len)
}

impl TableFunction for ReadFixed {
    fn name(&self) -> &str {
        "read_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Read a fixed-width file (template / JSON / copybook spec) into rows".into(),
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![
            ArgSpec::const_arg("path", 0, "varchar", "File path or glob"),
            ArgSpec::const_arg("spec", 1, "varchar", "Layout spec (template/JSON/copybook)"),
            ArgSpec::const_arg("format", -1, "varchar", "Spec format override"),
            ArgSpec::const_arg("encoding", -1, "varchar", "ascii (default) or ebcdic"),
            ArgSpec::const_arg("framing", -1, "varchar", "newline (default) / fixed / rdw"),
            ArgSpec::const_arg("record_length", -1, "int64", "Override record length (fixed framing)"),
        ]
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let layout = options::layout(&params.arguments, 1)?;
        // Validate the path early (mirrors the native "File not found").
        let path = params
            .arguments
            .const_str(0)
            .ok_or_else(|| ve("read_fixed: path is required"))?;
        crate::table::resolve_paths(&path)?;
        Ok(BindResponse {
            output_schema: output_schema(&layout)?,
            opaque_data: Vec::new(),
        })
    }

    fn producer(&self, params: &ProcessParams) -> Result<Box<dyn TableProducer>> {
        let layout = options::layout(&params.arguments, 1)?;
        let enc = options::encoding(&params.arguments)?;
        let framing = options::framing(&params.arguments)?;
        let rec_len = record_length(&params.arguments, &layout);

        let path = params
            .arguments
            .const_str(0)
            .ok_or_else(|| ve("read_fixed: path is required"))?;
        let files = crate::table::resolve_paths(&path)?;

        let rows = read_all(&files, &layout, enc, framing, rec_len)?;
        Ok(Box::new(FixedProducer {
            schema: params.output_schema.clone(),
            rows,
            pos: 0,
        }))
    }
}

/// Read and decode every record across `files` into rows of column values.
fn read_all(
    files: &[String],
    layout: &Layout,
    enc: Encoding,
    framing: Framing,
    rec_len: usize,
) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    for path in files {
        let data = std::fs::read(path).map_err(|e| ve(format!("read {path}: {e}")))?;
        let recs = records(&data, framing, rec_len).map_err(ve)?;
        for rec in recs {
            let pairs = decode_record(layout, rec, enc).map_err(ve)?;
            rows.push(pairs.into_iter().map(|(_, v)| v).collect());
        }
    }
    Ok(rows)
}

struct FixedProducer {
    schema: SchemaRef,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl TableProducer for FixedProducer {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let end = (self.pos + BATCH_ROWS).min(self.rows.len());
        let chunk = &self.rows[self.pos..end];
        self.pos = end;

        let ncols = self.schema.fields().len();
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(ncols);
        for (j, field) in self.schema.fields().iter().enumerate() {
            let col: Vec<Value> = chunk
                .iter()
                .map(|row| row.get(j).cloned().unwrap_or(Value::Null))
                .collect();
            columns.push(crate::arrow_map::build_array(field.data_type(), &col)?);
        }

        Ok(Some(
            RecordBatch::try_new(self.schema.clone(), columns)
                .map_err(|e| RpcError::runtime_error(e.to_string()))?,
        ))
    }
}
