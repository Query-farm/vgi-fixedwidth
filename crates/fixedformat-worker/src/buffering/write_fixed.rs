//! `write_fixed((FROM rel), path, spec [, format =>, encoding =>, framing =>])`
//! — write a relation out to a fixed-width file (the inverse of `read_fixed`).
//!
//! Each input row's columns are matched (by name) to the layout fields, encoded
//! to record bytes, framed per the `framing` option, and streamed to `path`.
//! Returns one row: `(rows_written BIGINT, bytes_written BIGINT)`.

use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use fixedformat_core::encode::encode_record;
use fixedformat_core::framing::Framing;
use fixedformat_core::{Encoding, Layout, Value};
use vgi::buffering::{BufferingParams, TableBufferingFunction};
use vgi::function::{ArgSpec, BindParams, BindResponse, FunctionMetadata};
use vgi::ipc;
use vgi::table_function::TableProducer;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::options;
use crate::value_in::value_at;

const NS: &[u8] = b"write_fixed";

pub struct WriteFixed;

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

impl TableBufferingFunction for WriteFixed {
    fn name(&self) -> &str {
        "write_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Write a relation to a fixed-width file (inverse of read_fixed)".into(),
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![
            ArgSpec::column("data", 0, "table", "Input relation to format"),
            ArgSpec::const_arg("path", 1, "varchar", "Output file path"),
            ArgSpec::const_arg("spec", 2, "varchar", "Layout spec (template/JSON/copybook)"),
            ArgSpec::const_arg("format", -1, "varchar", "Spec format override"),
            ArgSpec::const_arg("encoding", -1, "varchar", "ascii (default) or ebcdic"),
            ArgSpec::const_arg("framing", -1, "varchar", "newline (default) / fixed / rdw"),
        ]
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        params
            .input_schema
            .as_ref()
            .ok_or_else(|| ve("write_fixed: requires an input relation"))?;
        // Validate the spec parses now (fail fast).
        options::layout(&params.arguments, 2)?;
        let fields = vec![
            Field::new("rows_written", DataType::Int64, false),
            Field::new("bytes_written", DataType::Int64, false),
        ];
        Ok(BindResponse {
            output_schema: Arc::new(Schema::new(fields)),
            opaque_data: Vec::new(),
        })
    }

    fn process(&self, params: &BufferingParams, batch: &RecordBatch) -> Result<Vec<u8>> {
        params
            .storage
            .append(&params.execution_id, NS, b"", ipc::write_batch(batch)?);
        Ok(params.execution_id.clone())
    }

    fn combine(&self, params: &BufferingParams, _state_ids: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        Ok(vec![params.execution_id.clone()])
    }

    fn finalize_producer(
        &self,
        params: &BufferingParams,
        finalize_state_id: Vec<u8>,
    ) -> Result<Box<dyn TableProducer>> {
        let layout = options::layout(&params.arguments, 2)?;
        let enc = options::encoding(&params.arguments)?;
        let framing = options::framing(&params.arguments)?;
        let path = params
            .arguments
            .const_str(1)
            .ok_or_else(|| ve("write_fixed: path is required"))?;

        // Drain all buffered batches, encoding each row to a framed record.
        let mut records: Vec<Vec<u8>> = Vec::new();
        let mut after_id = 0i64;
        loop {
            let rows = params.storage.scan(&finalize_state_id, NS, b"", after_id, 256);
            if rows.is_empty() {
                break;
            }
            for (id, bytes) in rows {
                after_id = id;
                let batch = ipc::read_batch(&bytes)?;
                encode_batch(&batch, &layout, enc, &mut records)?;
            }
        }

        let body = assemble(&records, framing);
        let rows_written = records.len() as i64;
        let bytes_written = body.len() as i64;
        std::fs::write(&path, &body).map_err(|e| ve(format!("write {path}: {e}")))?;

        let batch = RecordBatch::try_new(
            params.output_schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![rows_written])),
                Arc::new(Int64Array::from(vec![bytes_written])),
            ],
        )
        .map_err(|e| RpcError::runtime_error(e.to_string()))?;
        Ok(Box::new(OneShot { batch: Some(batch) }))
    }
}

/// Encode every row of a relation batch into record bytes.
fn encode_batch(
    batch: &RecordBatch,
    layout: &Layout,
    enc: Encoding,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let schema = batch.schema();
    for row in 0..batch.num_rows() {
        let mut pairs: Vec<(String, Value)> = Vec::with_capacity(batch.num_columns());
        for (c, field) in schema.fields().iter().enumerate() {
            pairs.push((field.name().clone(), value_at(batch.column(c), row)?));
        }
        out.push(encode_record(layout, &pairs, enc).map_err(ve)?);
    }
    Ok(())
}

/// Frame the encoded records into the final file body.
fn assemble(records: &[Vec<u8>], framing: Framing) -> Vec<u8> {
    let mut body = Vec::new();
    match framing {
        Framing::Newline => {
            for rec in records {
                body.extend_from_slice(rec);
                body.push(b'\n');
            }
        }
        Framing::Fixed => {
            for rec in records {
                body.extend_from_slice(rec);
            }
        }
        Framing::Rdw => {
            for rec in records {
                push_descriptor(&mut body, rec.len() + 4);
                body.extend_from_slice(rec);
            }
        }
        Framing::RdwBlocked => {
            // One block wrapping all RDW-framed records.
            let block_len: usize = 4 + records.iter().map(|r| r.len() + 4).sum::<usize>();
            push_descriptor(&mut body, block_len);
            for rec in records {
                push_descriptor(&mut body, rec.len() + 4);
                body.extend_from_slice(rec);
            }
        }
    }
    body
}

/// Write a 4-byte descriptor word (big-endian length, then two zero bytes).
fn push_descriptor(body: &mut Vec<u8>, len: usize) {
    let len = len as u16;
    body.extend_from_slice(&len.to_be_bytes());
    body.extend_from_slice(&[0, 0]);
}

/// Emits a single precomputed batch, then EOF.
struct OneShot {
    batch: Option<RecordBatch>,
}

impl TableProducer for OneShot {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        Ok(self.batch.take())
    }
}
