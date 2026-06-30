//! Streaming file/object reader shared by `read_fixed` and `COPY … FROM`.
//!
//! Records are read **incrementally** for the self-delimiting framings (newline
//! / fixed): each [`StreamingProducer::next_batch`] pulls up to [`BATCH_ROWS`]
//! records from a [`fixedformat_core::stream::RecordStream`] and decodes them,
//! so peak memory is ~one batch rather than the whole file plus every decoded
//! row. The RDW family still buffers the (decompressed) object — its
//! length-prefix walking needs the whole stream — but that buffering now lives
//! inside `RecordStream` so this module has a single code path.
//!
//! Byte sources are opened **lazily**: a local path becomes a
//! [`BufReader`]`<File>` when first read; a remote object's store is built up
//! front (no network call) but its bytes are fetched only when the producer
//! reaches that location, so a multi-file glob never holds more than one object
//! in memory at a time. (True per-range S3 streaming is a future step; a
//! lazy-per-object fetch already bounds peak memory to one object.)

use std::io::{BufRead, BufReader, Cursor, Read};

use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::SchemaRef;
use fixedformat_core::compression::Compression;
use fixedformat_core::decode::decode_record;
use fixedformat_core::framing::Framing;
use fixedformat_core::stream::{decompress_reader, Limits, RecordStream};
use fixedformat_core::{Encoding, Layout, Value};
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use url::Url;
use vgi::secrets::Secrets;
use vgi::table_function::TableProducer;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::cloud::{self, Location};

/// Rows emitted per `next_batch`. Kept identical to the previous buffered path.
pub(crate) const BATCH_ROWS: usize = 2048;

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// The concrete record iterator: a streaming framer over a (possibly
/// decompressed) boxed byte source.
pub(crate) type RecordIter = RecordStream<BufReader<Box<dyn Read + Send>>>;

/// Map a `Display` error into a value error. Shared with `read_multi`.
pub(crate) fn read_ve(e: impl std::fmt::Display) -> RpcError {
    ve(e)
}

/// A resolved byte source ready to open: a local file path, or a remote object
/// addressed by a pre-built object store + key. The store is constructed when
/// the source is resolved (cheap, no I/O); the bytes are fetched on [`open`].
pub(crate) enum Source {
    Local(String),
    Remote {
        store: Box<dyn ObjectStore>,
        path: ObjPath,
        /// Retained only for error messages.
        url: Url,
    },
}

impl Source {
    /// A short human label for this source, used to locate a failing record.
    pub(crate) fn label(&self) -> &str {
        match self {
            Source::Local(p) => p,
            Source::Remote { url, .. } => url.as_str(),
        }
    }

    /// Open this source as a buffered byte reader. Local → `BufReader<File>`;
    /// remote → the object's bytes fetched now and wrapped in a `Cursor`.
    fn open(&self) -> Result<Box<dyn BufRead + Send>> {
        match self {
            Source::Local(path) => {
                let f = std::fs::File::open(path).map_err(|e| ve(format!("read {path}: {e}")))?;
                Ok(Box::new(BufReader::new(f)))
            }
            Source::Remote { store, path, url } => {
                let bytes = cloud::fetch_object(store.as_ref(), path, url)?;
                Ok(Box::new(Cursor::new(bytes)))
            }
        }
    }
}

/// Resolve concrete [`Location`]s into [`Source`]s, building each remote object's
/// store (no network call) so the bytes can be fetched lazily later.
pub(crate) fn resolve_sources(
    locations: &[Location],
    secrets: &Secrets,
    overrides: &[(String, String)],
) -> Result<Vec<Source>> {
    let mut out = Vec::with_capacity(locations.len());
    for loc in locations {
        match loc {
            Location::Local(p) => out.push(Source::Local(p.clone())),
            Location::Remote(url) => {
                let (store, path) = cloud::build_store(url, secrets, overrides)?;
                out.push(Source::Remote {
                    store,
                    path,
                    url: url.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// A variable-length layout (OCCURS … DEPENDING ON) has no constant record size,
/// so `fixed` framing — which chunks the stream into equal-length records —
/// cannot delimit it. Require a self-describing framing instead. (Shared so
/// `read_fixed` and `COPY … FROM` reject it identically.)
pub(crate) fn check_variable_framing(layout: &Layout, framing: Framing) -> Result<()> {
    if layout.variable && framing == Framing::Fixed {
        return Err(ve(
            "OCCURS … DEPENDING ON makes records variable-length; use framing => 'newline', \
             'rdw', or 'rdw_blocked' (not 'fixed')",
        ));
    }
    Ok(())
}

/// Open a streaming record iterator over one source: decompress (auto-detected
/// when `compression` is `None`) then frame per `framing`.
pub(crate) fn open_stream(
    source: &Source,
    framing: Framing,
    rec_len: usize,
    compression: Option<Compression>,
    limits: Limits,
) -> Result<RecordIter> {
    let raw = source.open()?;
    let plain = decompress_reader(raw, compression, limits.max_decompressed_bytes).map_err(ve)?;
    RecordStream::new(
        BufReader::new(plain),
        framing,
        rec_len,
        limits.max_record_bytes,
    )
    .map_err(ve)
}

/// Build a `RecordBatch` from `rows` (one inner `Vec<Value>` per record, holding
/// the record's **full** decoded fields in layout order) against `schema`.
///
/// `projection` maps each output column of `schema` to the index of its decoded
/// field in the full layout order — so a projected / reordered scan (e.g.
/// `SELECT c, a`) lands each decoded value under the **right** output column (by
/// name; see [`StreamingProducer::new`]) and only the projected columns are
/// materialized into Arrow arrays. With the identity projection (`output_schema`
/// == the full layout schema) this is the plain row-major → columnar transpose.
///
/// Takes ownership and **moves** each projected `Value` exactly once (no per-cell
/// clone) via `mem::replace` with `Value::Null`.
pub(crate) fn build_batch(
    schema: &SchemaRef,
    projection: &[usize],
    mut rows: Vec<Vec<Value>>,
) -> Result<RecordBatch> {
    let ncols = schema.fields().len();
    let mut cols: Vec<Vec<Value>> = (0..ncols).map(|_| Vec::with_capacity(rows.len())).collect();
    for row in &mut rows {
        for (col, &src) in cols.iter_mut().zip(projection) {
            // A short row (fewer decoded values than the layout implies) is
            // padded with NULL, matching the prior behavior.
            let v = row
                .get_mut(src)
                .map(|slot| std::mem::replace(slot, Value::Null))
                .unwrap_or(Value::Null);
            col.push(v);
        }
    }
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(ncols);
    for (field, col) in schema.fields().iter().zip(&cols) {
        columns.push(crate::arrow_map::build_array(field.data_type(), col)?);
    }
    RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| RpcError::runtime_error(e.to_string()))
}

/// Read and decode **every** record across `locations` into rows of column
/// values, eagerly. Used by `COPY … FROM`, which needs all rows to map decoded
/// columns onto the target table by position. Internally it drives the same
/// streaming framer as [`StreamingProducer`], so there is a single read path —
/// it just collects instead of paginating.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_all(
    locations: &[Location],
    layout: &Layout,
    enc: Encoding,
    framing: Framing,
    rec_len: usize,
    compression: Option<Compression>,
    limits: Limits,
    secrets: &Secrets,
    overrides: &[(String, String)],
) -> Result<Vec<Vec<Value>>> {
    check_variable_framing(layout, framing)?;
    let sources = resolve_sources(locations, secrets, overrides)?;
    let mut rows = Vec::new();
    for source in &sources {
        let label = source.label();
        let stream = open_stream(source, framing, rec_len, compression, limits)?;
        for (i, rec) in stream.enumerate() {
            let rec = rec.map_err(|e| ve(format!("{label}: {e}")))?;
            let pairs = decode_record(layout, &rec, enc)
                .map_err(|e| ve(format!("{label}: record {}: {e}", i + 1)))?;
            rows.push(pairs.into_iter().map(|(_, v)| v).collect());
        }
    }
    Ok(rows)
}

/// A streaming [`TableProducer`] for `read_fixed`: decodes one batch of records
/// per `next_batch`, advancing across `sources` as each is exhausted, so peak
/// memory is ~one batch (newline / fixed framing) instead of all rows.
pub(crate) struct StreamingProducer {
    schema: SchemaRef,
    /// For each output column of `schema`, the index of its field in the full
    /// decoded record (layout order). Identity unless projection pushdown
    /// narrowed/reordered `schema`. See [`StreamingProducer::new`].
    projection: Vec<usize>,
    layout: Layout,
    enc: Encoding,
    framing: Framing,
    rec_len: usize,
    compression: Option<Compression>,
    limits: Limits,
    sources: Vec<Source>,
    /// Index of the next source to open once `current` is exhausted.
    idx: usize,
    /// Records yielded so far from the source at `idx` (1-based in errors).
    seen: u64,
    /// The record iterator for the source currently being drained.
    current: Option<RecordIter>,
}

impl StreamingProducer {
    /// Build a streaming producer over already-resolved `sources`.
    ///
    /// `schema` is the (possibly projection-narrowed / reordered) output schema
    /// the worker must emit; `full_names` is the decode-order name of every
    /// top-level layout field (what [`decode_record`] yields). Each output
    /// column is mapped to its decoded field **by name** so a projected scan
    /// (`SELECT c, a …`) places every value under the right column and skips
    /// building the unprojected ones. COBOL field names are case-insensitive, so
    /// matching is ASCII-case-insensitive (consistent with the decode `scope`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        schema: SchemaRef,
        full_names: &[String],
        layout: Layout,
        enc: Encoding,
        framing: Framing,
        rec_len: usize,
        compression: Option<Compression>,
        limits: Limits,
        sources: Vec<Source>,
    ) -> Result<Self> {
        let projection = schema
            .fields()
            .iter()
            .map(|f| {
                full_names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(f.name()))
                    .ok_or_else(|| {
                        ve(format!(
                            "projected column '{}' is not a field of the layout spec",
                            f.name()
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(StreamingProducer {
            schema,
            projection,
            layout,
            enc,
            framing,
            rec_len,
            compression,
            limits,
            sources,
            idx: 0,
            seen: 0,
            current: None,
        })
    }
}

impl TableProducer for StreamingProducer {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(BATCH_ROWS);
        while rows.len() < BATCH_ROWS {
            // Ensure a current source stream, advancing to the next location
            // when needed; stop when all locations are exhausted.
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
                    let pairs = decode_record(&self.layout, &rec, self.enc)
                        .map_err(|e| ve(format!("{label}: record {n}: {e}")))?;
                    rows.push(pairs.into_iter().map(|(_, v)| v).collect());
                }
                None => {
                    // This source is drained; move on to the next one.
                    self.current = None;
                    self.idx += 1;
                }
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(build_batch(&self.schema, &self.projection, rows)?))
    }
}
