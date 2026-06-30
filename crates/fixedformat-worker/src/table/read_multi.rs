//! `read_multi(path, spec [, framing =>, encoding =>, compression =>, …])` — scan
//! a **heterogeneous** flat file whose records have different layouts selected by
//! a discriminator field, returning one column `record` of Arrow type **sparse
//! Union** (DuckDB `UNION(H STRUCT(…), D STRUCT(…), …)`).
//!
//! Each framed record's discriminator bytes pick a variant [`Layout`]
//! ([`MultiLayout::select`]); the record is decoded with that layout into a
//! `Value::Struct`, and the batch is assembled as a sparse `UnionArray`: every
//! variant's child `StructArray` is full record-batch length, holding the decoded
//! struct on the rows where that variant is active and NULL on all others, with a
//! per-row `type_ids` buffer naming the active variant.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, UnionArray};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{DataType, Field as ArrowField, Schema, SchemaRef, UnionFields, UnionMode};
use fixedformat_core::compression::Compression;
use fixedformat_core::decode::decode_record;
use fixedformat_core::framing::Framing;
use fixedformat_core::multirecord::MultiLayout;
use fixedformat_core::stream::Limits;
use fixedformat_core::{Encoding, Value};
use vgi::arguments::Arguments;
use vgi::secrets::SecretLookup;
use vgi::table_function::{TableFunction, TableProducer};
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams};
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::reader::{open_stream, read_ve as ve, RecordIter, Source, BATCH_ROWS};
use crate::{arrow_map, cloud, options};

pub struct ReadMulti;

/// DuckDB sparse-union type-ids are `i8`, so the number of record types is bounded.
const MAX_VARIANTS: usize = i8::MAX as usize;

/// Parse the multi-record spec from the const arg at position 1.
fn multi_layout(args: &Arguments) -> Result<MultiLayout> {
    let spec = args
        .const_str(1)
        .ok_or_else(|| ve("a multi-record layout spec string is required"))?;
    MultiLayout::parse(&spec).map_err(|e| ve(e.to_string()))
}

/// The Arrow [`UnionFields`] for a multi-record layout: one STRUCT child per
/// record type (named by its tag), with type-ids `0..N` in declaration order.
fn union_fields(ml: &MultiLayout) -> Result<UnionFields> {
    if ml.variants.len() > MAX_VARIANTS {
        return Err(ve(format!(
            "a multi-record spec may declare at most {MAX_VARIANTS} record types (got {})",
            ml.variants.len()
        )));
    }
    let mut type_ids: Vec<i8> = Vec::with_capacity(ml.variants.len());
    let mut fields: Vec<ArrowField> = Vec::with_capacity(ml.variants.len());
    for (i, (tag, layout)) in ml.variants.iter().enumerate() {
        type_ids.push(i as i8);
        let ty = arrow_map::layout_struct_type(layout)?;
        fields.push(ArrowField::new(tag, ty, true));
    }
    UnionFields::try_new(type_ids, fields).map_err(|e| RpcError::runtime_error(e.to_string()))
}

/// The single-column output schema: `record UNION(...)` (sparse).
fn output_schema(ml: &MultiLayout) -> Result<SchemaRef> {
    let uf = union_fields(ml)?;
    let field = ArrowField::new("record", DataType::Union(uf, UnionMode::Sparse), false);
    Ok(Arc::new(Schema::new(vec![field])))
}

/// Record length for `fixed` framing: an explicit `record_length =>` override, else
/// the longest variant's static record length (the usual "pad every record type to
/// a common length" convention).
fn record_length(args: &Arguments, ml: &MultiLayout) -> usize {
    if let Some(n) = args.named_i64("record_length") {
        return n.max(0) as usize;
    }
    ml.variants
        .iter()
        .map(|(_, l)| l.record_len)
        .max()
        .unwrap_or(0)
}

/// Reject `fixed` framing when any variant is variable-length (OCCURS DEPENDING
/// ON) — such records have no constant length to chunk on.
fn check_framing(ml: &MultiLayout, framing: Framing) -> Result<()> {
    if framing == Framing::Fixed {
        for (tag, layout) in &ml.variants {
            if layout.variable {
                return Err(ve(format!(
                    "record type {tag:?} is variable-length (OCCURS … DEPENDING ON); \
                     use framing => 'newline', 'rdw', or 'rdw_blocked' (not 'fixed')"
                )));
            }
        }
    }
    Ok(())
}

impl TableFunction for ReadMulti {
    fn name(&self) -> &str {
        "read_multi"
    }

    fn metadata(&self) -> FunctionMetadata {
        let mut tags = crate::meta::object_tags(
            "Read Multi-Record-Type File",
            "Scan a heterogeneous fixed-width / flat file whose records have DIFFERENT layouts — \
             e.g. a header, many detail rows, and a trailer — selected by a 'record type' \
             discriminator field. The `spec` is a JSON object: a `discriminator` ({offset, width} \
             of the bytes that identify each record's type) and a `records` map of tag → field \
             list (each field list uses the same JSON field syntax as `read_fixed`'s JSON spec, so \
             every field type / group / OCCURS works per record type). Each record is framed per \
             `framing =>` ('newline' the default, 'fixed', 'rdw', or 'rdw_blocked'), its \
             discriminator bytes pick the matching variant, and it is decoded with that variant's \
             layout. The result is a single column `record` of type UNION — one STRUCT variant per \
             record type, the variant names being the discriminator tags. Use `union_tag(record)` \
             to get a row's record type and `union_extract(record, 'D')` to pull out a given \
             variant's STRUCT. An unmatched discriminator value is an error unless the spec gives a \
             `default` tag. `path` may glob and may be a cloud URL. `framing`, `encoding`, \
             `compression`, and `record_length` are NAMED arguments.",
            "Scan a heterogeneous flat file whose records have different layouts selected by a \
             discriminator field. The JSON `spec` declares a `discriminator` ({offset,width}) and a \
             `records` map of tag → JSON field list; the result is one UNION column `record` with a \
             STRUCT variant per record type. Use `union_tag(record)` / `union_extract(record, \
             'TAG')`. Named args: `framing =>`, `encoding =>`, `compression =>`, `record_length =>`.",
            "read multi, multi-record, heterogeneous, header detail trailer, discriminator, record \
             type, union, copybook, flat file, mainframe, fixed-width",
        );
        tags.push((
            "vgi.result_columns_md".into(),
            "A single column `record` of type **UNION** — one STRUCT variant per record type, named \
             by the discriminator tag. Use `union_tag(record)` for the record type and \
             `union_extract(record, '<tag>')` for a variant's STRUCT.\n\n\
             **Example usage** (illustrative — scans a real file):\n\n\
             ```sql\n\
             SELECT union_tag(record) AS kind,\n\
             \x20      union_extract(record, 'D').sku AS sku\n\
             FROM fixed.main.read_multi('data/multi.dat',\n\
             \x20 '{\"discriminator\":{\"offset\":0,\"width\":1},\n\
             \x20   \"records\":{\"H\":[{\"name\":\"co\",\"type\":\"str\",\"width\":20}],\n\
             \x20              \"D\":[{\"name\":\"sku\",\"type\":\"str\",\"width\":10},\n\
             \x20                    {\"name\":\"qty\",\"type\":\"int\",\"digits\":5}],\n\
             \x20              \"T\":[{\"name\":\"count\",\"type\":\"int\",\"digits\":6}]}}');\n\
             ```"
            .into(),
        ));
        FunctionMetadata {
            description: "Read a heterogeneous (multi-record-type) flat file into a UNION column"
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
                "any",
                "Path(s) to the file(s) to read: a single VARCHAR or a LIST(VARCHAR). May glob \
                 (e.g. 'data/*.dat') or be a cloud URL ('s3://bucket/key' or 'https://host/file'). \
                 Credentials come from the matching DuckDB secret, scoped per path.",
            ),
            ArgSpec::const_arg(
                "spec",
                1,
                "varchar",
                "The multi-record JSON layout: a `discriminator` ({offset, width}) plus a \
                 `records` map of record-type tag → JSON field list (the same field syntax as \
                 read_fixed's JSON spec). An optional `default` tag handles unmatched values. \
                 Determines the UNION variants of the output `record` column.",
            ),
            ArgSpec::const_arg(
                "encoding",
                -1,
                "varchar",
                "Byte encoding of the file: 'ascii' (the default) or 'ebcdic' (CP037). The \
                 discriminator bytes are transcoded out of EBCDIC before matching.",
            ),
            ArgSpec::const_arg(
                "framing",
                -1,
                "varchar",
                "How records are delimited: 'newline' (the default), 'fixed' (back-to-back \
                 equal-length records — every record type padded to a common length), 'rdw', or \
                 'rdw_blocked'.",
            ),
            ArgSpec::const_arg(
                "record_length",
                -1,
                "int64",
                "Per-record length in BYTES for 'fixed' framing; ignored otherwise. Defaults to \
                 the longest record type's layout length.",
            ),
            ArgSpec::const_arg(
                "compression",
                -1,
                "varchar",
                "Input compression: 'auto' (default — detect gzip/zstd from magic bytes), 'none', \
                 'gzip', or 'zstd'. Decompression happens before framing/decoding.",
            ),
            ArgSpec::const_arg(
                "max_decompressed_bytes",
                -1,
                "int64",
                "Safety cap on total DECOMPRESSED bytes per file (default 16 GiB). Only applies to \
                 gzip/zstd input.",
            ),
        ]
        .into_iter()
        .chain(options::cloud_arg_specs())
        .collect()
    }

    fn secret_lookups(&self, params: &BindParams) -> Vec<SecretLookup> {
        match options::paths(&params.arguments, 0) {
            Ok(paths) => cloud::secret_lookups(&paths),
            Err(_) => Vec::new(),
        }
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let ml = multi_layout(&params.arguments)?;
        let paths = options::paths(&params.arguments, 0)?;
        for p in &paths {
            if !cloud::is_remote(p) {
                crate::table::resolve_local(p)?;
            }
        }
        Ok(BindResponse {
            output_schema: output_schema(&ml)?,
            opaque_data: Vec::new(),
        })
    }

    fn producer(&self, params: &ProcessParams) -> Result<Box<dyn TableProducer>> {
        let ml = multi_layout(&params.arguments)?;
        let enc = options::encoding(&params.arguments)?;
        let framing = options::framing(&params.arguments)?;
        let rec_len = record_length(&params.arguments, &ml);
        let compression = options::compression(&params.arguments)?;
        let limits = options::read_limits(&params.arguments)?;
        check_framing(&ml, framing)?;

        let paths = options::paths(&params.arguments, 0)?;
        let overrides = options::cloud_overrides(&params.arguments);
        let mut locations = Vec::new();
        for p in &paths {
            locations.extend(crate::table::resolve_locations(
                p,
                &params.secrets,
                &overrides,
            )?);
        }
        let sources = crate::reader::resolve_sources(&locations, &params.secrets, &overrides)?;
        let uf = union_fields(&ml)?;

        Ok(Box::new(MultiProducer {
            schema: params.output_schema.clone(),
            uf,
            ml,
            enc,
            framing,
            rec_len,
            compression,
            limits,
            sources,
            idx: 0,
            seen: 0,
            current: None,
        }))
    }
}

/// Streaming producer for `read_multi`: decodes one batch of records per
/// `next_batch`, advancing across `sources`, and assembles each batch as a sparse
/// `UnionArray`.
struct MultiProducer {
    schema: SchemaRef,
    uf: UnionFields,
    ml: MultiLayout,
    enc: Encoding,
    framing: Framing,
    rec_len: usize,
    compression: Option<Compression>,
    limits: Limits,
    sources: Vec<Source>,
    idx: usize,
    seen: u64,
    current: Option<RecordIter>,
}

impl MultiProducer {
    /// Assemble decoded `(variant index, struct value)` rows into a sparse
    /// `UnionArray`: every variant's child is full length (`rows.len()`), holding
    /// the decoded struct on its active rows and NULL elsewhere.
    fn build_union_batch(&self, rows: Vec<(usize, Value)>) -> Result<RecordBatch> {
        let nrows = rows.len();
        let nvariants = self.uf.len();
        let mut type_ids: Vec<i8> = Vec::with_capacity(nrows);
        let mut per_variant: Vec<Vec<Value>> =
            (0..nvariants).map(|_| Vec::with_capacity(nrows)).collect();
        for (vidx, val) in rows {
            type_ids.push(vidx as i8);
            // Sparse union: append a slot to EVERY variant child, then drop the
            // decoded struct into the active one (others stay NULL).
            for col in per_variant.iter_mut() {
                col.push(Value::Null);
            }
            if let Some(slot) = per_variant[vidx].last_mut() {
                *slot = val;
            }
        }
        let mut children: Vec<ArrayRef> = Vec::with_capacity(nvariants);
        for ((_, field), col) in self.uf.iter().zip(&per_variant) {
            children.push(arrow_map::build_array(field.data_type(), col)?);
        }
        let arr = UnionArray::try_new(
            self.uf.clone(),
            ScalarBuffer::from(type_ids),
            None, // sparse: no value-offsets
            children,
        )
        .map_err(|e| RpcError::runtime_error(e.to_string()))?;
        RecordBatch::try_new(self.schema.clone(), vec![Arc::new(arr)])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}

impl TableProducer for MultiProducer {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        let mut rows: Vec<(usize, Value)> = Vec::with_capacity(BATCH_ROWS);
        while rows.len() < BATCH_ROWS {
            if self.current.is_none() {
                if self.idx >= self.sources.len() {
                    break;
                }
                self.current = Some(open_stream(
                    &self.sources[self.idx],
                    self.framing,
                    self.rec_len,
                    self.compression,
                    self.limits,
                )?);
                self.seen = 0;
            }
            let stream = self
                .current
                .as_mut()
                .expect("current stream was just ensured");
            match stream.next() {
                Some(rec) => {
                    self.seen += 1;
                    let n = self.seen;
                    let label = self.sources[self.idx].label();
                    let rec = rec.map_err(|e| ve(format!("{label}: {e}")))?;
                    let (vidx, layout) = self
                        .ml
                        .select(&rec, self.enc)
                        .map_err(|e| ve(format!("{label}: record {n}: {e}")))?;
                    let pairs = decode_record(layout, &rec, self.enc)
                        .map_err(|e| ve(format!("{label}: record {n}: {e}")))?;
                    rows.push((vidx, Value::Struct(pairs)));
                }
                None => {
                    self.current = None;
                    self.idx += 1;
                }
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.build_union_batch(rows)?))
    }
}
