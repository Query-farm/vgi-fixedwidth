//! Streaming record framing — split a *byte source* into record slices
//! incrementally, decoding one record at a time instead of buffering the whole
//! file.
//!
//! The slice-based [`crate::framing::records`] takes a `&[u8]` and returns every
//! record at once — fine for small inputs and required for the RDW family (whose
//! length-prefix walking needs the whole stream), but it forces the caller to
//! hold the entire file in memory. This module is its streaming counterpart for
//! the **self-delimiting** framings:
//!
//! - [`Framing::Newline`] reads one line per [`Iterator::next`] via
//!   [`BufRead::read_until`], stripping a trailing `\r\n` / `\n` and dropping the
//!   trailing empty record — byte-for-byte the same record set as
//!   [`crate::framing::records`]'s `newline`.
//! - [`Framing::Fixed`] reads exactly `record_len` bytes per record; a clean EOF
//!   on a record boundary ends iteration, while a partial trailing chunk is an
//!   error (mirroring the slice-based "not a multiple of record length" check).
//! - [`Framing::Rdw`] / [`Framing::RdwBlocked`] are **not** streamable
//!   (length-prefix walking needs the whole stream), so [`RecordStream`] reads
//!   the source to a buffer once and delegates to [`crate::framing::records`],
//!   yielding owned copies. Peak memory is then the whole (decompressed) object,
//!   same as before — the streaming win is for newline / fixed only.
//!
//! Compression is handled by [`decompress_reader`], which peeks the leading
//! magic bytes (without consuming them) to pick a decoder, then wraps the source
//! so the framer sees plaintext.

use std::io::{BufRead, Read};

use crate::compression::Compression;
use crate::framing::{records, Framing};
use crate::{Error, Result};

/// Resource limits applied while reading, to bound memory against a
/// decompression bomb (a tiny gzip/zstd that expands to gigabytes) or a
/// pathological record. Construct via [`Limits::default`] and override fields.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum total **decompressed** bytes that may flow through a gzip/zstd
    /// decoder for one source. Has no effect on uncompressed input (which is
    /// bounded by the real file/object size, not an expansion ratio). Exceeding
    /// it is a hard error. Default: 16 GiB.
    pub max_decompressed_bytes: u64,
    /// Maximum size of a single record buffered while framing — caps the
    /// streaming `newline` path so one delimiter-less "line" can't grow an
    /// unbounded `Vec`. Default: 512 MiB.
    pub max_record_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_decompressed_bytes: 16 * 1024 * 1024 * 1024,
            max_record_bytes: 512 * 1024 * 1024,
        }
    }
}

/// Cap on the zstd window size a frame may request, bounding the decoder's
/// internal allocation independent of payload size (2^27 = 128 MiB).
const ZSTD_WINDOW_LOG_MAX: u32 = 27;

/// A [`Read`] adapter that errors once more than `limit` bytes have been read
/// from `inner` — the decompression-bomb backstop. Overrun is detected within
/// one buffer fill of the limit, so peak buffering stays bounded.
struct LimitReader<R> {
    inner: R,
    read_so_far: u64,
    limit: u64,
}

impl<R: Read> Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read_so_far = self.read_so_far.saturating_add(n as u64);
        if self.read_so_far > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "decompressed input exceeds the maximum of {} bytes (possible decompression \
                     bomb); raise max_decompressed_bytes to allow it",
                    self.limit
                ),
            ));
        }
        Ok(n)
    }
}

/// Like [`BufRead::read_until`] but errors if the record grows past `max` bytes
/// before the delimiter is found, so a delimiter-less stream (e.g. a gzip bomb
/// that decompresses to one enormous line) can't grow an unbounded buffer.
fn read_until_capped<R: BufRead>(
    r: &mut R,
    delim: u8,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let (found, used) = {
            let avail = match r.fill_buf() {
                Ok(b) => b,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if avail.is_empty() {
                return Ok(total); // EOF
            }
            match avail.iter().position(|&b| b == delim) {
                Some(i) => {
                    buf.extend_from_slice(&avail[..=i]);
                    (true, i + 1)
                }
                None => {
                    buf.extend_from_slice(avail);
                    (false, avail.len())
                }
            }
        };
        r.consume(used);
        total += used;
        if found {
            return Ok(total);
        }
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "record exceeds the maximum size of {max} bytes (no record delimiter found)"
                ),
            ));
        }
    }
}

/// Wrap a buffered byte source with the appropriate decompressor so the caller
/// reads plaintext. When `compression` is `None` the codec is **auto-detected**
/// from the leading magic bytes — peeked via [`BufRead::fill_buf`], which does
/// **not** consume them, so the chosen decoder still sees the full stream.
/// `Some(codec)` forces that codec (skipping detection); [`Compression::None`]
/// passes the source through unchanged.
pub fn decompress_reader<R: BufRead + Send + 'static>(
    mut reader: R,
    compression: Option<Compression>,
    max_decompressed_bytes: u64,
) -> Result<Box<dyn Read + Send>> {
    let codec = match compression {
        Some(c) => c,
        None => {
            // Peek (not consume) the head; detection needs at most 4 bytes.
            let head = reader.fill_buf().map_err(|e| Error(format!("read: {e}")))?;
            Compression::detect(head)
        }
    };
    // Cap total decompressed output (decompression-bomb backstop). Only the
    // compressed paths are wrapped: uncompressed input is bounded by the real
    // file/object size, so capping it would wrongly reject large plain files.
    let limited = |inner: Box<dyn Read + Send>| -> Box<dyn Read + Send> {
        Box::new(LimitReader {
            inner,
            read_so_far: 0,
            limit: max_decompressed_bytes,
        })
    };
    Ok(match codec {
        Compression::None => Box::new(reader),
        // MultiGzDecoder handles concatenated gzip members, matching the
        // buffered `compression::decompress` path. The decoder only fails on the
        // first *read* (e.g. a bad header / corrupt body when the codec is forced
        // on non-gzip data), so wrap it to tag those errors `gzip decode: …` —
        // matching the buffered path's message.
        Compression::Gzip => limited(Box::new(LabeledReader {
            inner: flate2::read::MultiGzDecoder::new(reader),
            label: "gzip decode",
        })),
        Compression::Zstd => {
            let mut dec = zstd::stream::read::Decoder::new(reader)
                .map_err(|e| Error(format!("zstd decode: {e}")))?;
            // Bound the decoder's internal window allocation against a frame that
            // declares an enormous window independent of payload size.
            dec.window_log_max(ZSTD_WINDOW_LOG_MAX)
                .map_err(|e| Error(format!("zstd decode: {e}")))?;
            limited(Box::new(LabeledReader {
                inner: dec,
                label: "zstd decode",
            }))
        }
    })
}

/// A [`Read`] adapter that prefixes the wrapped reader's I/O errors with a fixed
/// `label` (e.g. `"gzip decode"`). Decompressors surface a bad stream as an
/// error on `read`, deep inside the framer; tagging it keeps the user-facing
/// message the same as the buffered `compression::decompress` path.
struct LabeledReader<R> {
    inner: R,
    label: &'static str,
}

impl<R: Read> Read for LabeledReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner
            .read(buf)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{label}: {e}", label = self.label)))
    }
}

/// Read into `buf` until it is full or EOF, returning the number of bytes read.
/// Unlike [`Read::read_exact`] a short read at EOF is **not** an error — the
/// caller distinguishes a clean boundary (`n == 0` or `n == buf.len()`) from a
/// ragged tail (`0 < n < buf.len()`). Retries on `Interrupted`.
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// A streaming iterator over the records of a byte source, framed per
/// [`Framing`]. Each [`Iterator::next`] yields one owned record (`Vec<u8>`), or
/// an [`Error`] on a malformed stream / I/O failure. Construct with
/// [`RecordStream::new`].
pub struct RecordStream<R: BufRead> {
    state: State<R>,
}

enum State<R: BufRead> {
    /// Newline-delimited: one `read_until('\n')` per record, capped at
    /// `max_record_bytes` so a delimiter-less stream can't grow unboundedly.
    Newline {
        reader: R,
        done: bool,
        max_record_bytes: usize,
    },
    /// Fixed-length: `record_len` bytes per record.
    Fixed {
        reader: R,
        record_len: usize,
        done: bool,
    },
    /// RDW / RDW-blocked: pre-split owned records (whole source buffered once).
    Buffered(std::vec::IntoIter<Vec<u8>>),
}

impl<R: BufRead> RecordStream<R> {
    /// Build a streaming framer over `reader`. For newline / fixed framing
    /// records are produced lazily; for the RDW family the whole `reader` is
    /// read into memory up front and split via [`crate::framing::records`].
    /// `record_len` is required (non-zero) for [`Framing::Fixed`]. `max_record_bytes`
    /// caps a single buffered record (the `newline` path).
    pub fn new(
        mut reader: R,
        framing: Framing,
        record_len: usize,
        max_record_bytes: usize,
    ) -> Result<Self> {
        let state = match framing {
            Framing::Newline => State::Newline {
                reader,
                done: false,
                max_record_bytes,
            },
            Framing::Fixed => {
                if record_len == 0 {
                    return Err(Error(
                        "fixed framing requires a non-zero record length".into(),
                    ));
                }
                State::Fixed {
                    reader,
                    record_len,
                    done: false,
                }
            }
            Framing::Rdw | Framing::RdwBlocked => {
                // Length-prefix framings can't be walked incrementally — read the
                // whole (decompressed) stream once and reuse the slice splitter,
                // copying each record out so it outlives the buffer.
                let mut buf = Vec::new();
                reader
                    .read_to_end(&mut buf)
                    .map_err(|e| Error(format!("read: {e}")))?;
                let recs: Vec<Vec<u8>> = records(&buf, framing, record_len)?
                    .into_iter()
                    .map(<[u8]>::to_vec)
                    .collect();
                State::Buffered(recs.into_iter())
            }
        };
        Ok(Self { state })
    }
}

impl<R: BufRead> Iterator for RecordStream<R> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            State::Newline {
                reader,
                done,
                max_record_bytes,
            } => {
                if *done {
                    return None;
                }
                let mut buf = Vec::new();
                match read_until_capped(reader, b'\n', &mut buf, *max_record_bytes) {
                    // EOF with nothing buffered: no trailing empty record.
                    Ok(0) => {
                        *done = true;
                        None
                    }
                    Ok(_) => {
                        // Strip the line terminator: `\n`, plus a preceding `\r`.
                        // A final line without a trailing `\n` keeps its bytes
                        // verbatim and ends the stream.
                        if buf.last() == Some(&b'\n') {
                            buf.pop();
                            if buf.last() == Some(&b'\r') {
                                buf.pop();
                            }
                        } else {
                            *done = true;
                        }
                        Some(Ok(buf))
                    }
                    Err(e) => {
                        *done = true;
                        Some(Err(Error(format!("read: {e}"))))
                    }
                }
            }
            State::Fixed {
                reader,
                record_len,
                done,
            } => {
                if *done {
                    return None;
                }
                let mut buf = vec![0u8; *record_len];
                match read_full(reader, &mut buf) {
                    // Clean EOF on a record boundary: end of stream.
                    Ok(0) => {
                        *done = true;
                        None
                    }
                    Ok(n) if n == *record_len => Some(Ok(buf)),
                    // A partial trailing record — the stream is not a whole
                    // number of fixed-length records.
                    Ok(n) => {
                        *done = true;
                        Some(Err(Error(format!(
                            "fixed-length stream is not a multiple of record length {record_len} \
                             (trailing partial record of {n} bytes)"
                        ))))
                    }
                    Err(e) => {
                        *done = true;
                        Some(Err(Error(format!("read: {e}"))))
                    }
                }
            }
            State::Buffered(iter) => iter.next().map(Ok),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Collect every record from a streaming framer over `data` (generous cap).
    fn collect(data: &[u8], framing: Framing, record_len: usize) -> Result<Vec<Vec<u8>>> {
        RecordStream::new(
            Cursor::new(data.to_vec()),
            framing,
            record_len,
            Limits::default().max_record_bytes,
        )?
        .collect()
    }

    #[test]
    fn newline_final_record_without_trailing_newline() {
        // "ghi" has no trailing `\n` — it must still be yielded.
        let recs = collect(b"abc\ndef\nghi", Framing::Newline, 0).unwrap();
        assert_eq!(
            recs,
            vec![b"abc".to_vec(), b"def".to_vec(), b"ghi".to_vec()]
        );
    }

    #[test]
    fn newline_strips_crlf() {
        let recs = collect(b"abc\r\ndef\nghi", Framing::Newline, 0).unwrap();
        assert_eq!(
            recs,
            vec![b"abc".to_vec(), b"def".to_vec(), b"ghi".to_vec()]
        );
    }

    #[test]
    fn newline_drops_trailing_empty_record() {
        // Trailing `\n` must NOT produce a final empty record.
        let recs = collect(b"abc\ndef\n", Framing::Newline, 0).unwrap();
        assert_eq!(recs, vec![b"abc".to_vec(), b"def".to_vec()]);
    }

    #[test]
    fn newline_keeps_interior_empty_record() {
        // A blank line between records IS a (empty) record.
        let recs = collect(b"abc\n\ndef\n", Framing::Newline, 0).unwrap();
        assert_eq!(recs, vec![b"abc".to_vec(), b"".to_vec(), b"def".to_vec()]);
    }

    #[test]
    fn fixed_exact_multiple() {
        let recs = collect(b"aaabbbccc", Framing::Fixed, 3).unwrap();
        assert_eq!(
            recs,
            vec![b"aaa".to_vec(), b"bbb".to_vec(), b"ccc".to_vec()]
        );
    }

    #[test]
    fn fixed_ragged_is_error() {
        // Five bytes is not a multiple of three: the trailing "bb" errors.
        let err = collect(b"aaabb", Framing::Fixed, 3);
        assert!(err.is_err());
    }

    #[test]
    fn fixed_zero_record_len_is_error() {
        assert!(
            RecordStream::new(Cursor::new(vec![1u8, 2, 3]), Framing::Fixed, 0, 1 << 20).is_err()
        );
    }

    #[test]
    fn empty_input_yields_zero_records() {
        assert!(collect(b"", Framing::Newline, 0).unwrap().is_empty());
        assert!(collect(b"", Framing::Fixed, 4).unwrap().is_empty());
        assert!(collect(b"", Framing::Rdw, 0).unwrap().is_empty());
    }

    #[test]
    fn forcing_gzip_on_plain_data_errors_with_gzip_label() {
        // Forcing the gzip codec on non-gzip bytes must surface as a `gzip
        // decode` error (matching the buffered path) — the error only appears on
        // the first read, so it propagates out of the framer.
        let r = decompress_reader(
            std::io::BufReader::new(Cursor::new(b"plain\n".to_vec())),
            Some(Compression::Gzip),
            Limits::default().max_decompressed_bytes,
        )
        .unwrap();
        let err = RecordStream::new(std::io::BufReader::new(r), Framing::Newline, 0, 1 << 20)
            .unwrap()
            .next()
            .unwrap()
            .unwrap_err();
        assert!(err.0.contains("gzip decode"), "got: {}", err.0);
    }

    #[test]
    fn per_record_cap_rejects_a_delimiterless_line() {
        // A 10 KB stream with no newline, capped at 1 KB per record, must error
        // rather than buffer the whole thing into one record.
        let data = vec![b'x'; 10_000];
        let err = RecordStream::new(Cursor::new(data), Framing::Newline, 0, 1024)
            .unwrap()
            .next()
            .unwrap()
            .unwrap_err();
        assert!(err.0.contains("maximum size"), "got: {}", err.0);
    }

    #[test]
    fn decompressed_size_cap_trips_on_a_bomb() {
        use std::io::Write;
        // ~1 MB of zeros gzips tiny; with a 4 KB decompressed cap, reading it must
        // error (the decompression-bomb backstop) rather than expand in full.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&vec![0u8; 1_000_000]).unwrap();
        let gz = enc.finish().unwrap();
        assert!(gz.len() < 4096, "fixture should be a high-ratio payload");
        let mut r =
            decompress_reader(std::io::BufReader::new(Cursor::new(gz)), None, 4096).unwrap();
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).unwrap_err();
        assert!(
            err.to_string().contains("decompressed input exceeds"),
            "got: {err}"
        );
    }

    #[test]
    fn rdw_buffered_matches_slice_framer() {
        // Two records "AB" (len 6) and "CDE" (len 7).
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x06, 0x00, 0x00]);
        data.extend_from_slice(b"AB");
        data.extend_from_slice(&[0x00, 0x07, 0x00, 0x00]);
        data.extend_from_slice(b"CDE");
        let recs = collect(&data, Framing::Rdw, 0).unwrap();
        assert_eq!(recs, vec![b"AB".to_vec(), b"CDE".to_vec()]);
    }

    #[test]
    fn streaming_newline_matches_slice_framer() {
        // Cross-check the streaming output against the canonical slice framer for
        // a few shapes, so the two never drift.
        for input in [
            &b"abc\r\ndef\nghi"[..],
            &b"abc\ndef\n"[..],
            &b"abc\n\ndef\n"[..],
            &b""[..],
            &b"\n"[..],
            &b"only-line"[..],
        ] {
            let stream = collect(input, Framing::Newline, 0).unwrap();
            let slice: Vec<Vec<u8>> = records(input, Framing::Newline, 0)
                .unwrap()
                .into_iter()
                .map(<[u8]>::to_vec)
                .collect();
            assert_eq!(stream, slice, "mismatch for {input:?}");
        }
    }

    #[test]
    fn decompress_reader_autodetects_and_passes_through() {
        use std::io::Write;
        // gzip round-trip via the streaming wrapper (auto-detect).
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello stream\n").unwrap();
        let gz = enc.finish().unwrap();
        let mut out = Vec::new();
        decompress_reader(
            std::io::BufReader::new(Cursor::new(gz)),
            None,
            Limits::default().max_decompressed_bytes,
        )
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
        assert_eq!(out, b"hello stream\n");

        // Plain bytes pass straight through.
        let mut out2 = Vec::new();
        decompress_reader(
            std::io::BufReader::new(Cursor::new(b"plain\n".to_vec())),
            None,
            Limits::default().max_decompressed_bytes,
        )
        .unwrap()
        .read_to_end(&mut out2)
        .unwrap();
        assert_eq!(out2, b"plain\n");
    }
}
