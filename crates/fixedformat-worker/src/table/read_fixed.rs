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
        let mut tags = crate::meta::object_tags(
            "Read Fixed-Width File",
            "Scan a fixed-width / flat-file data file into typed rows. `path` may be a glob (e.g. \
             '/data/*.dat'); matching files are read in sorted order and their rows concatenated. \
             Each record is framed per the named `framing =>` argument ('newline' the default, \
             'fixed', 'rdw', or 'rdw_blocked') and decoded into a row of typed columns according \
             to the layout `spec` — a Perl/Python `unpack` template, a JSON field list, or a COBOL \
             copybook (format auto-detected unless you pass `format =>` 'template' / 'json' / \
             'copybook'). The named `encoding =>` argument is 'ascii' (default) or 'ebcdic' \
             (CP037). The named `record_length =>` argument is the per-record length in BYTES; it \
             is only used for 'fixed' framing (back-to-back records of equal length) and defaults \
             to the length implied by the `spec`; it is ignored for the other framings. `format`, \
             `encoding`, `framing`, and `record_length` are all NAMED arguments. The returned \
             column set is dynamic: it is determined by the spec, with OCCURS becoming LIST \
             columns and groups / REDEFINES becoming STRUCT columns. Use it to ingest mainframe or \
             legacy flat-file data into SQL. This is the file-scanning counterpart of unpack_fixed \
             and the inverse of write_fixed.",
            "Scan a fixed-width file into rows, decoding each record per the layout `spec` \
             (template, JSON, or COBOL copybook). `path` may glob (e.g. \
             `read_fixed('/data/*.dat', 'A10 N')`), reading matching files in sorted order. \
             Optional NAMED args: `format =>`, `encoding =>` ('ascii'/'ebcdic'), `framing =>` \
             ('newline'/'fixed'/'rdw'/'rdw_blocked'), and `record_length =>` (per-record length in \
             bytes, used only for `fixed` framing). The returned columns are dynamic — they depend \
             on the spec, with OCCURS → LIST and group/REDEFINES → STRUCT.",
            "read fixed, scan, fixed-width file, flat file, ingest, copybook, mainframe, EBCDIC, \
             RDW, rdw_blocked, record_length, COMP-3, glob, file to rows, table function",
        );
        tags.push((
            "vgi.result_columns_md".into(),
            "The returned columns are **dynamic** — they are determined by the layout `spec` \
             argument, one column per top-level field. Column names come from the field names in \
             the spec, and types follow the field kinds:\n\n\
             | spec field kind | column type |\n\
             |---|---|\n\
             | text / hex | VARCHAR |\n\
             | integer | BIGINT |\n\
             | float / double | REAL / DOUBLE |\n\
             | COMP-3 / zoned / implied-point decimal | DECIMAL(p,s) |\n\
             | `?` boolean | BOOLEAN |\n\
             | OCCURS / repeat | LIST of the element type |\n\
             | group / REDEFINES | STRUCT of the child fields |\n\n\
             **Example usage** (illustrative — these scan real files, so they are not run in the \
             lint sandbox):\n\n\
             ```sql\n\
             -- Glob several files; columns name VARCHAR, id BIGINT:\n\
             SELECT * FROM fixed.main.read_fixed('/data/*.dat', 'A10 N');\n\n\
             -- Fixed framing, 16-byte records; an OCCURS spec yields a LIST column:\n\
             SELECT * FROM fixed.main.read_fixed('/data/cust.dat', 'A10 9(3) OCCURS 2',\n\
             \x20                               framing => 'fixed', record_length => 16);\n\n\
             -- A COBOL copybook with a nested group yields a STRUCT column:\n\
             SELECT * FROM fixed.main.read_fixed('/data/recs.bin', '<copybook text>',\n\
             \x20                               format => 'copybook', encoding => 'ebcdic');\n\
             ```"
            .into(),
        ));
        // NOTE: no `vgi.example_queries` here. `read_fixed` always scans an
        // external file, so any example returns zero rows in an environment
        // without the data file present (VGI902). The documented usage lives in
        // `vgi.doc_md` / the schema example queries / executable_examples; the
        // returned columns are documented via `vgi.result_columns_md` above.
        FunctionMetadata {
            description: "Read a fixed-width file (template / JSON / copybook spec) into rows"
                .into(),
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![
            ArgSpec::const_arg(
                "path",
                0,
                "varchar",
                "Path to the fixed-width file to read; may be a glob (e.g. 'data/*.dat') to scan \
                 several files in sorted order.",
            ),
            ArgSpec::const_arg(
                "spec",
                1,
                "varchar",
                "The record layout to decode each row with: a Perl/Python `unpack` template, a \
                 JSON field list, or a COBOL copybook. Determines the output column names and \
                 types; format is auto-detected unless `format` is given.",
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
                "Byte encoding of the file: 'ascii' (the default) or 'ebcdic' (CP037).",
            ),
            ArgSpec::const_arg(
                "framing",
                -1,
                "varchar",
                "How records are delimited in the file: 'newline' (the default), 'fixed' \
                 (back-to-back records of equal length), 'rdw', or 'rdw_blocked'.",
            ),
            ArgSpec::const_arg(
                "record_length",
                -1,
                "int64",
                "The length of each record in BYTES. Used only for 'fixed' framing (back-to-back \
                 equal-length records); ignored for the other framings. Defaults to the length \
                 implied by the layout `spec`.",
            ),
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
