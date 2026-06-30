//! `COPY (query|table) TO '<path>' (FORMAT 'fixed.fixed_out', spec '<layout>', …)`
//! — write a relation out to a fixed-width / flat-file / COBOL-copybook file.
//!
//! This is the COPY-TO counterpart of [`write_fixed`](crate::buffering): it reuses
//! the same Arrow → framed-record encode path ([`crate::record_writer`]), but the
//! destination `path` comes from the `COPY ... TO 'path'` statement (not an
//! option) and the row count is reported by DuckDB. The format name is
//! **catalog-qualified** (`'<attach-name>.fixed_out'`, e.g. `'fixed.fixed_out'`).
//! The reader counterpart is `'fixed.fixed'` ([`crate::copy_from`]); the writer
//! uses a distinct `_out` name because the worker SDK advertises FROM and TO as
//! separate formats.
//!
//! ```sql
//! COPY (SELECT name, qty FROM accounts) TO 'data/out.dat'
//!   (FORMAT 'fixed.fixed_out', spec 'name:A10 qty:9(5)');
//! ```
//!
//! Mechanically a COPY-TO writer is a **buffered (Sink+Combine) function with no
//! Source phase**: [`write`](CopyToFunction::write) buffers each input batch into
//! cross-process, `execution_id`-scoped storage (so it survives pool rotation /
//! HTTP), and [`close`](CopyToFunction::close) — driven by DuckDB's once-only
//! finalize — drains the shards, encodes + frames every row, and writes the
//! destination.
//!
//! **Destinations.** Local paths are fully supported. An `s3://` destination
//! works via named overrides (`endpoint`/`region`/`url_style`/`use_ssl`) or
//! ambient credentials, but DuckDB `CREATE SECRET` credentials are **not**
//! forwarded on the COPY-TO path (the SDK's `CopyToFunction` has no secret-bind
//! hook). For secret-backed cloud writes use the `write_fixed` table function.

use arrow_array::RecordBatch;
use fixedformat_core::parse_spec;
use vgi::copy_to::{CopyToCloseContext, CopyToFunction, CopyToWriteContext};
use vgi::function::{ArgSpec, FunctionMetadata};
use vgi::ipc;
use vgi_rpc::{Result, RpcError};

use crate::cloud::{self, Location};
use crate::options;
use crate::record_writer::{assemble, encode_batch};

/// Append-only shard namespace (execution-scoped). Each `write()` appends one
/// IPC-serialized input batch; `close()` scans them back in append order.
const SHARD_NS: &[u8] = b"copy_to_shard";

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Register the `fixed_out` COPY-TO format on the worker.
pub fn register(worker: &mut vgi::Worker) {
    worker.register_copy_to(CopyToFixed);
}

/// `COPY … TO '<path>' (FORMAT 'fixed.fixed_out', …)` writer.
pub struct CopyToFixed;

impl CopyToFunction for CopyToFixed {
    fn format(&self) -> &str {
        "fixed_out"
    }

    fn handler_name(&self) -> &str {
        "copy_to_fixed"
    }

    fn comment(&self) -> Option<String> {
        Some("Write the COPY source out to a fixed-width / flat-file / COBOL-copybook file".into())
    }

    fn metadata(&self) -> FunctionMetadata {
        let mut tags = crate::meta::object_tags(
            "Write Fixed-Width File (COPY TO)",
            "Write a query or table out to a fixed-width / flat-file / COBOL-copybook file — the \
             COPY-TO counterpart of write_fixed. Invoked via `COPY (<query>|<table>) TO '<path>' \
             (FORMAT 'fixed.fixed_out', spec '<layout>', …)`, not called directly. The writer uses \
             a DISTINCT format name 'fixed.fixed_out' (catalog-qualified by the ATTACH name) \
             because the VGI SDK advertises FROM and TO as separate formats. Each input column is \
             matched to a layout field BY NAME, encoded to record bytes per the `spec` option (a \
             Perl/Python `unpack` template, a JSON field list, or a COBOL copybook; auto-detected \
             unless `format` is given), framed per `framing`, and written to `<path>` (overwritten \
             if it exists). Options are named: `spec` (required), `format`, `encoding` \
             ('ascii'/'ebcdic'), `framing` ('newline'/'fixed'/'rdw'/'rdw_blocked'), plus \
             `endpoint`/`region`/`url_style`/`use_ssl` for `s3://` destinations. NOTE: CREATE \
             SECRET credentials are NOT forwarded on the COPY-TO path — use named S3 overrides, \
             ambient credentials, or write_fixed for secret-backed cloud writes.",
            "Write a relation to a fixed-width file: `COPY (<query>|<table>) TO '<path>' (FORMAT \
             'fixed.fixed_out', spec '<layout>')`. Input columns map to layout fields by name. \
             Named options: `spec` (required), `format`, `encoding`, `framing`, and S3 overrides. \
             The COPY-TO counterpart of `write_fixed`.",
            "copy to, write, export, fixed-width file, flat file, emit, copybook, mainframe, \
             EBCDIC, RDW, COMP-3, S3, unload, bulk export",
        );
        tags.push((
            "vgi.result_columns_md".into(),
            "Returns **no result set** — this is a `COPY … TO` writer. The COPY source rows are \
             encoded to fixed-width records and written to `<path>` (overwritten if it exists); \
             DuckDB reports the number of rows written as the statement's `Count`. Each input \
             column is matched to a layout field **by name** before encoding."
                .into(),
        ));
        FunctionMetadata {
            description:
                "Write the COPY source out to a fixed-width / flat-file / COBOL-copybook file \
                 (the COPY-TO counterpart of write_fixed)"
                    .into(),
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        // COPY options arrive as named arguments (position -1); the destination
        // path is supplied by the COPY statement, not as an option.
        vec![
            ArgSpec::column(
                "spec",
                -1,
                "varchar",
                "The record layout to encode each row with: a Perl/Python `unpack` template, a \
                 JSON field list, or a COBOL copybook. Each input column is matched to a layout \
                 field by name.",
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
                "Byte encoding to write: 'ascii' (the default) or 'ebcdic' (CP037).",
            ),
            ArgSpec::column(
                "framing",
                -1,
                "varchar",
                "How to delimit records in the output: 'newline' (the default), 'fixed', 'rdw', \
                 or 'rdw_blocked'.",
            ),
            ArgSpec::column(
                "compression",
                -1,
                "varchar",
                "Compress the output: 'auto' (the default — gzip if the path ends '.gz', zstd if \
                 '.zst', else raw), 'none', 'gzip', or 'zstd'.",
            ),
            ArgSpec::column(
                "endpoint",
                -1,
                "varchar",
                "Custom S3 endpoint for an `s3://` destination (e.g. MinIO/R2 'host:9000').",
            ),
            ArgSpec::column(
                "region",
                -1,
                "varchar",
                "AWS region for an `s3://` destination.",
            ),
            ArgSpec::column(
                "url_style",
                -1,
                "varchar",
                "S3 addressing for an `s3://` destination: 'path' (path-style, e.g. MinIO) or \
                 'vhost'.",
            ),
            ArgSpec::column(
                "use_ssl",
                -1,
                "boolean",
                "Whether to use TLS for an `s3://` destination's custom endpoint (default true).",
            ),
        ]
    }

    fn write(&self, ctx: &CopyToWriteContext, batch: &RecordBatch) -> Result<()> {
        // Validate the spec parses eagerly so a bad layout fails on the first
        // batch rather than only at the terminal write.
        let _ = self.layout(ctx.options)?;
        // Buffer one input batch as an IPC blob in execution-scoped storage.
        // `append` is atomic + race-safe across parallel sink threads/workers.
        let blob = ipc::write_batch(batch)?;
        ctx.storage.append(ctx.execution_id, SHARD_NS, b"", blob);
        Ok(())
    }

    fn close(&self, ctx: &CopyToCloseContext) -> Result<i64> {
        let layout = self.layout(ctx.options)?;
        let enc = options::encoding(ctx.options)?;
        let framing = options::framing(ctx.options)?;

        // Drain all buffered shards in append order (after_id=-1 → all), encoding
        // each row to a framed record. usize::MAX drains in one scan.
        let shards = ctx
            .storage
            .scan(ctx.execution_id, SHARD_NS, b"", -1, usize::MAX);
        let mut records: Vec<Vec<u8>> = Vec::new();
        for (_id, blob) in &shards {
            let batch = ipc::read_batch(blob)?;
            encode_batch(&batch, &layout, enc, &mut records)?;
        }

        // An empty COPY still writes an (empty) destination file. Compress the
        // assembled body whole when the path/option requests gzip/zstd.
        let codec = options::write_compression(ctx.options, ctx.path)?;
        let body = fixedformat_core::compression::compress(&assemble(&records, framing), codec)
            .map_err(ve)?;
        let rows_written = records.len() as i64;
        match cloud::classify(ctx.path)? {
            Location::Local(p) => {
                std::fs::write(&p, &body).map_err(|e| ve(format!("write {p}: {e}")))?
            }
            Location::Remote(url) => {
                let overrides = options::cloud_overrides(ctx.options);
                cloud::write_object(&url, &ctx.params.secrets, &overrides, &body)?
            }
        }
        Ok(rows_written)
    }
}

impl CopyToFixed {
    /// Parse the layout from the named `spec` option, honoring an optional named
    /// `format` override. (COPY options are named, so this can't reuse the
    /// positional `options::layout`.)
    fn layout(&self, options: &vgi::arguments::Arguments) -> Result<fixedformat_core::Layout> {
        let spec = options
            .named_str("spec")
            .ok_or_else(|| ve("COPY fixed_out: required option 'spec' is missing"))?;
        parse_spec(&spec, options.named_str("format").as_deref()).map_err(ve)
    }
}
