//! `write_multi((FROM rel), path, spec [, encoding =>, framing =>, compression =>])`
//! — write a relation whose single `UNION` column back out to a heterogeneous
//! multi-record-type fixed-width file (the inverse of `read_multi`).
//!
//! The input relation has exactly ONE column: a sparse `UNION` whose variant
//! names are the multi-record spec's discriminator tags (the exact shape
//! `read_multi` emits). Each row's active variant gives `(tag, struct fields)`;
//! the matching variant [`Layout`] encodes the fields to record bytes, the
//! discriminator field is stamped with the `tag` (a variant's discriminator
//! position is usually a filler the encoder zero-fills), records are framed per
//! `framing`, optionally compressed, and streamed to `path`. Returns one row:
//! `(rows_written BIGINT, bytes_written BIGINT)`.

use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use fixedformat_core::encode::encode_record;
use fixedformat_core::multirecord::MultiLayout;
use fixedformat_core::Encoding;
use vgi::buffering::{BufferingParams, TableBufferingFunction};
use vgi::function::{ArgSpec, BindParams, BindResponse, FunctionMetadata};
use vgi::ipc;
use vgi::secrets::SecretLookup;
use vgi::table_function::TableProducer;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::cloud::{self, Location};
use crate::options;
use crate::record_writer::assemble;
use crate::value_in::union_at;

const NS: &[u8] = b"write_multi";

pub struct WriteMulti;

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Parse the multi-record spec from the const arg at position 2.
fn multi_layout(args: &vgi::arguments::Arguments) -> Result<MultiLayout> {
    let spec = args
        .const_str(2)
        .ok_or_else(|| ve("write_multi: a multi-record layout spec string is required"))?;
    MultiLayout::parse(&spec).map_err(|e| ve(e.to_string()))
}

/// Encode every row of a single-column UNION batch into framed record bytes,
/// appending to `out`. For each row: recover `(tag, fields)`, pick the variant
/// layout for `tag`, encode the fields, then stamp the discriminator bytes over
/// the record at the spec's discriminator offset.
fn encode_multi_batch(
    batch: &RecordBatch,
    ml: &MultiLayout,
    enc: Encoding,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let col = batch.column(0);
    let (off, width) = ml.discriminator;
    for row in 0..batch.num_rows() {
        let (tag, fields) = union_at(col, row)?;
        let layout = ml.variant(&tag).ok_or_else(|| {
            ve(format!(
                "UNION variant {tag:?} has no matching record type in the multi-record spec"
            ))
        })?;
        let mut rec = encode_record(layout, &fields, enc).map_err(ve)?;
        // Stamp the discriminator: the variant layout's discriminator position is
        // usually a filler the encoder zero-filled, so overwrite it with the tag.
        let disc = ml.encode_discriminator(&tag, enc);
        if rec.len() < off + width {
            rec.resize(off + width, 0);
        }
        rec[off..off + width].copy_from_slice(&disc);
        out.push(rec);
    }
    Ok(())
}

impl TableBufferingFunction for WriteMulti {
    fn name(&self) -> &str {
        "write_multi"
    }

    fn metadata(&self) -> FunctionMetadata {
        let mut tags = crate::meta::object_tags(
            "Write Multi-Record-Type File",
            "Write a relation whose single UNION column holds heterogeneous records back out to a \
             multi-record-type fixed-width / flat file — the inverse of read_multi. The `data` \
             relation (passed as a subquery, e.g. `(FROM read_multi(...))` or a `(SELECT ...)`) \
             must have exactly ONE column: a sparse UNION whose variant names are the \
             discriminator tags of the `spec` (the exact shape read_multi emits). For each row the \
             active variant gives its record type and its STRUCT field values; the matching \
             variant layout encodes those fields, the discriminator field is stamped with the tag, \
             the records are framed per the named `framing =>` argument ('newline' the default, \
             'fixed', 'rdw', or 'rdw_blocked'), encoded under named `encoding =>` ('ascii' the \
             default, or 'ebcdic'/CP037), optionally compressed via `compression =>`, and streamed \
             to `path` (overwritten if it exists). The `spec` is the SAME multi-record JSON as \
             read_multi: a `discriminator` ({offset,width}) plus a `records` map of tag → JSON \
             field list. `encoding`, `framing`, and `compression` are NAMED arguments. Use it to \
             emit header/detail/trailer or other heterogeneous mainframe flat files from SQL. \
             Returns exactly one summary row: `rows_written BIGINT` and `bytes_written BIGINT`.",
            "Write a relation's single UNION column out to a heterogeneous multi-record-type \
             fixed-width file — the inverse of read_multi. The relation is a subquery with one \
             UNION column whose variant names are the spec's discriminator tags. The `spec` is the \
             same multi-record JSON (`discriminator` + `records`). Each row's active variant is \
             encoded with its layout and the discriminator stamped with the tag. Optional NAMED \
             args: `encoding =>` ('ascii'/'ebcdic'), `framing =>` \
             ('newline'/'fixed'/'rdw'/'rdw_blocked'), `compression =>`. Returns one row \
             `(rows_written BIGINT, bytes_written BIGINT)`.",
            "write multi, multi-record, heterogeneous, header detail trailer, discriminator, union, \
             export, fixed-width file, flat file, emit, copybook, mainframe, EBCDIC, RDW, sink",
        );
        tags.push((
            "vgi.result_columns_md".into(),
            "| column | type | description |\n\
             |---|---|---|\n\
             | `rows_written` | BIGINT | Number of records written to the file. |\n\
             | `bytes_written` | BIGINT | Total number of bytes written, including framing. |"
                .into(),
        ));
        FunctionMetadata {
            description: "Write a relation's UNION column to a multi-record-type file (inverse of \
                          read_multi)"
                .into(),
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
                "The input relation to write out, supplied as a subquery (e.g. `(FROM \
                 read_multi(...))`). It must have exactly ONE column: a UNION whose variant names \
                 are the discriminator tags of `spec`.",
            ),
            ArgSpec::const_arg(
                "path",
                1,
                "varchar",
                "Path of the multi-record file to create; it is overwritten if it already exists. \
                 May also be an 's3://bucket/key' URL (AWS S3, or R2/MinIO/GCS-HMAC via a `CREATE \
                 SECRET (TYPE s3, …, ENDPOINT …)`) — credentials come from the matching DuckDB \
                 secret, scoped to the URL. Writing to 'http(s)://' is not supported.",
            ),
            ArgSpec::const_arg(
                "spec",
                2,
                "varchar",
                "The multi-record JSON layout (the same spec read_multi uses): a `discriminator` \
                 ({offset, width}) plus a `records` map of record-type tag → JSON field list. The \
                 UNION column's variant names must match these tags.",
            ),
            ArgSpec::const_arg(
                "encoding",
                -1,
                "varchar",
                "Byte encoding to write: 'ascii' (the default) or 'ebcdic' (CP037). The \
                 discriminator tag is transcoded to this encoding.",
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
        // destinations (s3://), honored by the SDK's two-phase secret bind.
        params
            .arguments
            .const_str(1)
            .and_then(|p| cloud::secret_lookup(&p))
            .into_iter()
            .collect()
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let schema = params
            .input_schema
            .as_ref()
            .ok_or_else(|| ve("write_multi: requires an input relation"))?;
        // The input must be exactly one UNION column whose variants are spec tags.
        let ml = multi_layout(&params.arguments)?;
        if schema.fields().len() != 1 {
            return Err(ve(format!(
                "write_multi: the input relation must have exactly one UNION column, got {} \
                 columns",
                schema.fields().len()
            )));
        }
        match schema.field(0).data_type() {
            DataType::Union(union_fields, _) => {
                for (_, f) in union_fields.iter() {
                    if ml.variant(f.name()).is_none() {
                        return Err(ve(format!(
                            "write_multi: UNION variant {:?} has no matching record type in the \
                             multi-record spec",
                            f.name()
                        )));
                    }
                }
            }
            other => {
                return Err(ve(format!(
                    "write_multi: the input column must be a UNION (as produced by read_multi), \
                     got {other:?}"
                )))
            }
        }
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
        let ml = multi_layout(&params.arguments)?;
        let enc = options::encoding(&params.arguments)?;
        let framing = options::framing(&params.arguments)?;
        let path = params
            .arguments
            .const_str(1)
            .ok_or_else(|| ve("write_multi: path is required"))?;
        let codec = options::write_compression(&params.arguments, &path)?;

        // Drain all buffered batches, encoding each UNION row to a framed record.
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
                encode_multi_batch(&batch, &ml, enc, &mut records)?;
            }
        }

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
