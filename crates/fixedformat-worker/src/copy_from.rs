//! `COPY <table> FROM '<path>' (FORMAT fixed, spec '<layout>', …)` — load a
//! fixed-width / flat-file / COBOL-copybook file straight into a DuckDB table.
//!
//! This is the COPY-FROM counterpart of [`read_fixed`](crate::table): it reuses
//! the same decode path (local or `s3://` / `http(s)://` source, with DuckDB
//! secrets), but the output schema is the COPY **target table's** schema — each
//! decoded column is cast to the target column by position, since DuckDB inserts
//! the scanned batches with no cast.
//!
//! ```sql
//! CREATE TABLE accounts (name VARCHAR, qty INTEGER);
//! COPY accounts FROM 'data/accounts.dat' (FORMAT fixed, spec 'name:A10 qty:9(5)');
//! COPY accounts FROM 's3://bucket/accounts.dat' (FORMAT fixed, spec 'A10 N');
//! ```

use arrow_array::{ArrayRef, RecordBatch};
use fixedformat_core::{parse_spec, Value};
use vgi::copy_from::{CopyFromFunction, CopyFromReadContext};
use vgi::function::{ArgSpec, BindParams, FunctionMetadata};
use vgi::secrets::SecretLookup;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::arrow_map::{build_array, layout_fields};
use crate::cloud;
use crate::options;

const BATCH_ROWS: usize = 2048;

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Register the `fixed` COPY-FROM format on the worker.
pub fn register(worker: &mut vgi::Worker) {
    worker.register_copy_from(CopyFixed);
}

/// `COPY … FROM '<path>' (FORMAT fixed, …)` reader.
pub struct CopyFixed;

impl CopyFromFunction for CopyFixed {
    fn format(&self) -> &str {
        "fixed"
    }

    fn handler_name(&self) -> &str {
        "copy_fixed"
    }

    fn comment(&self) -> Option<String> {
        Some(
            "Load a fixed-width / flat-file / COBOL-copybook file into the COPY target table"
                .into(),
        )
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description:
                "Read a fixed-width / flat-file / COBOL-copybook file into the COPY target table \
                 (the COPY-FROM counterpart of read_fixed)"
                    .into(),
            tags: vec![
                ("domain".into(), "data-engineering".into()),
                ("category".into(), "copy_from".into()),
                ("topic".into(), "fixed-width-records".into()),
            ],
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        // COPY options arrive as named arguments (position -1); the source path
        // is supplied by the COPY statement, not as an option.
        vec![
            ArgSpec::column(
                "spec",
                -1,
                "varchar",
                "The record layout to decode each row with: a Perl/Python `unpack` template, a \
                 JSON field list, or a COBOL copybook. The decoded columns are assigned to the \
                 COPY target table's columns by position, so the spec must produce the same \
                 number of columns in the same order (each cast to the target column's type).",
            ),
            ArgSpec::column(
                "format",
                -1,
                "varchar",
                "Force how `spec` is interpreted: 'template', 'json', or 'copybook'. Omit to \
                 auto-detect.",
            ),
            ArgSpec::column(
                "encoding",
                -1,
                "varchar",
                "Byte encoding of the file: 'ascii' (the default) or 'ebcdic' (CP037).",
            ),
            ArgSpec::column(
                "framing",
                -1,
                "varchar",
                "How records are delimited: 'newline' (the default), 'fixed', 'rdw', or \
                 'rdw_blocked'.",
            ),
            ArgSpec::column(
                "record_length",
                -1,
                "int64",
                "The length of each record in BYTES; used only for 'fixed' framing. Defaults to \
                 the length implied by the layout `spec`.",
            ),
            ArgSpec::column(
                "endpoint",
                -1,
                "varchar",
                "Custom S3 endpoint for an `s3://` source (e.g. MinIO/R2 'host:9000'); overrides \
                 any endpoint from a CREATE SECRET.",
            ),
            ArgSpec::column(
                "region",
                -1,
                "varchar",
                "AWS region for an `s3://` source. Overrides the region from a CREATE SECRET.",
            ),
            ArgSpec::column(
                "url_style",
                -1,
                "varchar",
                "S3 addressing for an `s3://` source: 'path' (path-style, e.g. MinIO) or 'vhost'.",
            ),
            ArgSpec::column(
                "use_ssl",
                -1,
                "boolean",
                "Whether to use TLS for an `s3://` source's custom endpoint (default true).",
            ),
        ]
    }

    fn secret_lookups(&self, params: &BindParams) -> Vec<SecretLookup> {
        // Request the matching DuckDB secret (scoped to the source URL) when the
        // COPY source is a cloud path that needs credentials (s3://).
        params
            .copy_from
            .as_ref()
            .and_then(|cf| cloud::secret_lookup(&cf.file_path))
            .into_iter()
            .collect()
    }

    fn read(
        &self,
        ctx: &CopyFromReadContext,
        _out: &mut OutputCollector,
    ) -> Result<Vec<RecordBatch>> {
        let spec = ctx
            .options
            .named_str("spec")
            .ok_or_else(|| ve("COPY fixed: required option 'spec' is missing"))?;
        let layout = parse_spec(&spec, ctx.options.named_str("format").as_deref()).map_err(ve)?;
        let enc = options::encoding(ctx.options)?;
        let framing = options::framing(ctx.options)?;
        let rec_len = ctx
            .options
            .named_i64("record_length")
            .map(|n| n as usize)
            .unwrap_or(layout.record_len);
        let overrides = options::cloud_overrides(ctx.options);

        // The source path may be local or a cloud URL (with secrets/overrides).
        let locations = crate::table::resolve_locations(ctx.path, &ctx.params.secrets, &overrides)?;
        let rows = crate::table::read_all(
            &locations,
            &layout,
            enc,
            framing,
            rec_len,
            &ctx.params.secrets,
            &overrides,
        )?;

        // Decoded columns are positional; map them onto the COPY target schema.
        let natural = layout_fields(&layout)?;
        let expected = ctx.expected_schema;
        if natural.len() != expected.fields().len() {
            return Err(ve(format!(
                "COPY fixed: the spec produces {} column(s) but the target table has {} — they \
                 must match by position",
                natural.len(),
                expected.fields().len()
            )));
        }

        let mut batches = Vec::new();
        for chunk in rows.chunks(BATCH_ROWS) {
            let mut columns: Vec<ArrayRef> = Vec::with_capacity(expected.fields().len());
            for (j, exp_field) in expected.fields().iter().enumerate() {
                let col: Vec<Value> = chunk
                    .iter()
                    .map(|r| r.get(j).cloned().unwrap_or(Value::Null))
                    .collect();
                let natural_arr = build_array(natural[j].data_type(), &col)?;
                // DuckDB inserts the scan with no cast, so coerce each decoded
                // column to the exact target type when they differ.
                let arr = if natural[j].data_type() == exp_field.data_type() {
                    natural_arr
                } else {
                    arrow_cast::cast(&natural_arr, exp_field.data_type()).map_err(|e| {
                        ve(format!(
                            "COPY fixed: cannot cast spec column {} ({}) to target column {} ({}): {e}",
                            j,
                            natural[j].data_type(),
                            exp_field.name(),
                            exp_field.data_type()
                        ))
                    })?
                };
                columns.push(arr);
            }
            batches.push(
                RecordBatch::try_new(expected.clone(), columns)
                    .map_err(|e| RpcError::runtime_error(e.to_string()))?,
            );
        }
        Ok(batches)
    }
}
