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
use fixedformat_core::stream::{decompress_reader, RecordStream};
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
type RecordIter = RecordStream<BufReader<Box<dyn Read + Send>>>;

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
fn open_stream(
    source: &Source,
    framing: Framing,
    rec_len: usize,
    compression: Option<Compression>,
) -> Result<RecordIter> {
    let raw = source.open()?;
    let plain = decompress_reader(raw, compression).map_err(ve)?;
    RecordStream::new(BufReader::new(plain), framing, rec_len).map_err(ve)
}

/// Build a `RecordBatch` for `rows` (one inner `Vec<Value>` per record, in column
/// order) against `schema`. Shared by the streaming producer and the eager
/// [`read_all`] collector so the Arrow array building lives in one place.
pub(crate) fn build_batch(schema: &SchemaRef, rows: &[Vec<Value>]) -> Result<RecordBatch> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for (j, field) in schema.fields().iter().enumerate() {
        let col: Vec<Value> = rows
            .iter()
            .map(|row| row.get(j).cloned().unwrap_or(Value::Null))
            .collect();
        columns.push(crate::arrow_map::build_array(field.data_type(), &col)?);
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
    secrets: &Secrets,
    overrides: &[(String, String)],
) -> Result<Vec<Vec<Value>>> {
    check_variable_framing(layout, framing)?;
    let sources = resolve_sources(locations, secrets, overrides)?;
    let mut rows = Vec::new();
    for source in &sources {
        let stream = open_stream(source, framing, rec_len, compression)?;
        for rec in stream {
            let rec = rec.map_err(ve)?;
            let pairs = decode_record(layout, &rec, enc).map_err(ve)?;
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
    layout: Layout,
    enc: Encoding,
    framing: Framing,
    rec_len: usize,
    compression: Option<Compression>,
    sources: Vec<Source>,
    /// Index of the next source to open once `current` is exhausted.
    idx: usize,
    /// The record iterator for the source currently being drained.
    current: Option<RecordIter>,
}

impl StreamingProducer {
    /// Build a streaming producer over already-resolved `sources`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        schema: SchemaRef,
        layout: Layout,
        enc: Encoding,
        framing: Framing,
        rec_len: usize,
        compression: Option<Compression>,
        sources: Vec<Source>,
    ) -> Self {
        StreamingProducer {
            schema,
            layout,
            enc,
            framing,
            rec_len,
            compression,
            sources,
            idx: 0,
            current: None,
        }
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
                )?);
            }
            let stream = self
                .current
                .as_mut()
                .expect("current stream was just ensured");
            match stream.next() {
                Some(rec) => {
                    let rec = rec.map_err(ve)?;
                    let pairs = decode_record(&self.layout, &rec, self.enc).map_err(ve)?;
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
        Ok(Some(build_batch(&self.schema, &rows)?))
    }
}
