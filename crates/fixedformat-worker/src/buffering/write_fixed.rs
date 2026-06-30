//! `write_fixed((FROM rel), path, spec [, format =>, encoding =>, framing =>])`
//! — write a relation out to a fixed-width file (the inverse of `read_fixed`).
//!
//! Each input row's columns are matched (by name) to the layout fields, encoded
//! to record bytes, framed per the `framing` option, and streamed to `path`.
//! Returns one row: `(rows_written BIGINT, bytes_written BIGINT)`.

use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use vgi::buffering::{BufferingParams, TableBufferingFunction};
use vgi::function::{ArgSpec, BindParams, BindResponse, FunctionMetadata};
use vgi::ipc;
use vgi::secrets::SecretLookup;
use vgi::table_function::TableProducer;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::cloud::{self, Location};
use crate::options;
use crate::record_writer::{assemble, encode_batch};

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
        let mut tags = crate::meta::object_tags(
            "Write Fixed-Width File",
            "Write an input relation out to a fixed-width / flat-file data file — the inverse of \
             read_fixed. The `data` relation is passed as a subquery (e.g. `(FROM tbl)` or \
             `(SELECT ...)`). Each input row's columns are matched by name to the layout fields, \
             encoded to record bytes per the `spec` (Perl/Python `unpack` template, JSON field \
             list, or COBOL copybook; format auto-detected unless you pass `format =>` 'template' \
             / 'json' / 'copybook'), framed per the named `framing =>` argument ('newline' the \
             default, 'fixed', 'rdw', or 'rdw_blocked'), encoded under named `encoding =>` ('ascii' \
             the default, or 'ebcdic'/CP037), and streamed to `path` (overwritten if it exists). \
             `format`, `encoding`, and `framing` are NAMED arguments. Use it to emit mainframe or \
             legacy flat-file output from SQL. Returns exactly one summary row with two columns: \
             `rows_written BIGINT` (number of records written) and `bytes_written BIGINT` (total \
             bytes written including framing).",
            "Write a relation to a fixed-width file, encoding each row per the layout `spec`. The \
             relation is a subquery, e.g. `write_fixed((FROM tbl), '/tmp/out.dat', 'A10 N')`. \
             Optional NAMED args: `format =>`, `encoding =>` ('ascii'/'ebcdic'), `framing =>` \
             ('newline'/'fixed'/'rdw'/'rdw_blocked'). The inverse of `read_fixed`; returns one row \
             `(rows_written BIGINT, bytes_written BIGINT)`.",
            "write fixed, export, fixed-width file, flat file, emit, copybook, mainframe, EBCDIC, \
             RDW, rdw_blocked, COMP-3, relation to file, table function, sink",
        );
        tags.push((
            "vgi.result_columns_md".into(),
            "| column | type | description |\n\
             |---|---|---|\n\
             | `rows_written` | BIGINT | Number of records written to the file. |\n\
             | `bytes_written` | BIGINT | Total number of bytes written, including framing. |"
                .into(),
        ));
        tags.push((
            "vgi.example_queries".into(),
            r#"[
  {
    "description": "Write a relation to a newline-framed fixed-width file (the default framing).",
    "sql": "SELECT * FROM fixed.main.write_fixed((SELECT 'Jo' AS name, 7 AS id), '/tmp/out.dat', 'A2 N')"
  },
  {
    "description": "Write back-to-back fixed-length records (no newline) by forcing the framing named argument.",
    "sql": "SELECT * FROM fixed.main.write_fixed((SELECT 'Jo' AS name, 7 AS id), '/tmp/out.dat', 'A2 N', framing => 'fixed')"
  }
]"#
            .into(),
        ));
        FunctionMetadata {
            description: "Write a relation to a fixed-width file (inverse of read_fixed)".into(),
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![
            ArgSpec::column(
                "data",
                0,
                "table",
                "The input relation to write out, supplied as a subquery (e.g. `(FROM tbl)`). Each \
                 row's columns are matched by name to the layout fields.",
            ),
            ArgSpec::const_arg(
                "path",
                1,
                "varchar",
                "Path of the fixed-width file to create; it is overwritten if it already exists. \
                 May also be an 's3://bucket/key' URL (AWS S3, or R2/MinIO/GCS-HMAC via a `CREATE \
                 SECRET (TYPE s3, …, ENDPOINT …)`) — credentials come from the matching DuckDB \
                 secret, scoped to the URL. Writing to 'http(s)://' is not supported.",
            ),
            ArgSpec::const_arg(
                "spec",
                2,
                "varchar",
                "The record layout to encode each row with: a Perl/Python `unpack` template, a \
                 JSON field list, or a COBOL copybook. Format is auto-detected unless `format` is \
                 given.",
            ),
            ArgSpec::const_arg(
                "format",
                -1,
                "varchar",
                "Force how `spec` is interpreted: 'template', 'json', or 'copybook'. Omit to \
                 auto-detect.",
            ),
            ArgSpec::const_arg(
                "encoding",
                -1,
                "varchar",
                "Byte encoding to write: 'ascii' (the default) or 'ebcdic' (CP037).",
            ),
            ArgSpec::const_arg(
                "framing",
                -1,
                "varchar",
                "How to delimit records in the output: 'newline' (the default), 'fixed', 'rdw', \
                 or 'rdw_blocked'.",
            ),
            ArgSpec::const_arg(
                "compression",
                -1,
                "varchar",
                "Compress the output: 'auto' (the default — gzip if the path ends '.gz', zstd if \
                 '.zst', else raw), 'none', 'gzip', or 'zstd'. The whole file is compressed.",
            ),
        ]
        .into_iter()
        .chain(options::cloud_arg_specs())
        .collect()
    }

    fn secret_lookups(&self, params: &BindParams) -> Vec<SecretLookup> {
        // Request the matching DuckDB secret (scoped to the URL) for remote
        // destinations that need credentials (s3://). Now honored by the SDK's
        // two-phase secret bind for buffering functions.
        params
            .arguments
            .const_str(1)
            .and_then(|p| cloud::secret_lookup(&p))
            .into_iter()
            .collect()
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
        let codec = options::write_compression(&params.arguments, &path)?;

        // Drain all buffered batches, encoding each row to a framed record.
        let mut records: Vec<Vec<u8>> = Vec::new();
        let mut after_id = 0i64;
        loop {
            let rows = params
                .storage
                .scan(&finalize_state_id, NS, b"", after_id, 256);
            if rows.is_empty() {
                break;
            }
            for (id, bytes) in rows {
                after_id = id;
                let batch = ipc::read_batch(&bytes)?;
                encode_batch(&batch, &layout, enc, &mut records)?;
            }
        }

        // Compress the assembled body whole (gzip/zstd) when the path/option asks
        // for it; `bytes_written` reflects the actual on-disk (compressed) size.
        let body = fixedformat_core::compression::compress(&assemble(&records, framing), codec)
            .map_err(ve)?;
        let rows_written = records.len() as i64;
        let bytes_written = body.len() as i64;
        match cloud::classify(&path)? {
            Location::Local(p) => {
                std::fs::write(&p, &body).map_err(|e| ve(format!("write {p}: {e}")))?
            }
            Location::Remote(url) => {
                let overrides = options::cloud_overrides(&params.arguments);
                cloud::write_object(&url, &params.secrets, &overrides, &body)?
            }
        }

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

/// Emits a single precomputed batch, then EOF.
struct OneShot {
    batch: Option<RecordBatch>,
}

impl TableProducer for OneShot {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        Ok(self.batch.take())
    }
}
