//! Transparent input decompression — gzip and zstd, with magic-byte
//! auto-detection.
//!
//! `read_fixed` / `COPY … FROM` read a whole file (or S3 object) into a byte
//! buffer before framing it into records (RDW framing needs the whole stream
//! anyway). This module sits in that gap: given the raw bytes, it decompresses
//! them so the framer sees plaintext. Pure bytes-in/bytes-out, so it unit-tests
//! directly alongside the other codecs.
//!
//! Detection is by **magic bytes** (authoritative, and works the same for local
//! and remote sources): gzip starts `1f 8b`, zstd starts `28 b5 2f fd`. A
//! caller can also force a codec (or force "no decompression") via the
//! `compression =>` option.

use std::io::Read;

use crate::{Error, Result};

/// A supported input compression codec (or [`Compression::None`] for raw bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// No compression — bytes are used as-is.
    #[default]
    None,
    /// gzip (RFC 1952). Multi-member streams are decoded in full.
    Gzip,
    /// Zstandard (RFC 8878).
    Zstd,
}

impl Compression {
    /// Parse an explicit `compression =>` value: `none`/`raw`, `gzip`/`gz`, or
    /// `zstd`/`zst`. The sentinel `auto` is **not** handled here — the worker
    /// maps it to [`Compression::detect`] — so passing it is an error.
    pub fn parse(s: &str) -> Result<Compression> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "raw" | "off" => Ok(Compression::None),
            "gzip" | "gz" => Ok(Compression::Gzip),
            "zstd" | "zst" => Ok(Compression::Zstd),
            other => Err(Error(format!(
                "unknown compression: {other} (expected none, gzip, zstd, or auto)"
            ))),
        }
    }

    /// Infer the codec from a destination path's extension (`.gz`/`.gzip` →
    /// gzip, `.zst`/`.zstd` → zstd, else none). Used on the **write** side, where
    /// there are no magic bytes yet, to auto-select a codec from the file name.
    pub fn from_path(path: &str) -> Compression {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".gz") || lower.ends_with(".gzip") {
            Compression::Gzip
        } else if lower.ends_with(".zst") || lower.ends_with(".zstd") {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Detect the codec from a buffer's leading magic bytes. Unknown / too-short
    /// input is treated as [`Compression::None`] (raw).
    pub fn detect(data: &[u8]) -> Compression {
        if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
            Compression::Gzip
        } else if data.len() >= 4 && data[0..4] == [0x28, 0xb5, 0x2f, 0xfd] {
            Compression::Zstd
        } else {
            Compression::None
        }
    }
}

/// Decompress `data` according to `compression`, returning the plaintext bytes.
/// [`Compression::None`] returns the input unchanged (no copy). A corrupt or
/// truncated stream is a hard error (surfaced to the caller, which fails the
/// query) rather than a partial/garbled result.
pub fn decompress(data: Vec<u8>, compression: Compression) -> Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data),
        Compression::Gzip => {
            // MultiGzDecoder handles concatenated gzip members (common when
            // files are appended), not just a single member.
            let mut out = Vec::new();
            flate2::read::MultiGzDecoder::new(&data[..])
                .read_to_end(&mut out)
                .map_err(|e| Error(format!("gzip decode: {e}")))?;
            Ok(out)
        }
        Compression::Zstd => {
            let mut out = Vec::new();
            let mut dec = zstd::stream::read::Decoder::new(&data[..])
                .map_err(|e| Error(format!("zstd decode: {e}")))?;
            dec.read_to_end(&mut out)
                .map_err(|e| Error(format!("zstd decode: {e}")))?;
            Ok(out)
        }
    }
}

/// Compress `data` according to `compression`, the inverse of [`decompress`].
/// [`Compression::None`] returns the input unchanged. Used by the write path
/// (`write_fixed` / `COPY … TO`) to emit a `.gz` / `.zst` file.
pub fn compress(data: &[u8], compression: Compression) -> Result<Vec<u8>> {
    use std::io::Write;
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data)
                .map_err(|e| Error(format!("gzip encode: {e}")))?;
            e.finish().map_err(|e| Error(format!("gzip encode: {e}")))
        }
        Compression::Zstd => {
            zstd::stream::encode_all(data, 0).map_err(|e| Error(format!("zstd encode: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn zstd_(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 0).unwrap()
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(Compression::parse("none").unwrap(), Compression::None);
        assert_eq!(Compression::parse("RAW").unwrap(), Compression::None);
        assert_eq!(Compression::parse("gz").unwrap(), Compression::Gzip);
        assert_eq!(Compression::parse("GZIP").unwrap(), Compression::Gzip);
        assert_eq!(Compression::parse(" zst ").unwrap(), Compression::Zstd);
        assert!(Compression::parse("auto").is_err());
        assert!(Compression::parse("lz4").is_err());
    }

    #[test]
    fn detect_by_magic() {
        assert_eq!(Compression::detect(&gzip(b"hello")), Compression::Gzip);
        assert_eq!(Compression::detect(&zstd_(b"hello")), Compression::Zstd);
        assert_eq!(Compression::detect(b"plain text"), Compression::None);
        assert_eq!(Compression::detect(b""), Compression::None);
        assert_eq!(Compression::detect(&[0x1f]), Compression::None); // too short
    }

    #[test]
    fn roundtrip_gzip() {
        let plain = b"NAME      00042\nJANE      00007\n".to_vec();
        let comp = gzip(&plain);
        assert_eq!(decompress(comp, Compression::Gzip).unwrap(), plain);
    }

    #[test]
    fn roundtrip_zstd() {
        let plain = b"NAME      00042\nJANE      00007\n".to_vec();
        let comp = zstd_(&plain);
        assert_eq!(decompress(comp, Compression::Zstd).unwrap(), plain);
    }

    #[test]
    fn none_is_identity() {
        let raw = b"untouched".to_vec();
        assert_eq!(decompress(raw.clone(), Compression::None).unwrap(), raw);
    }

    #[test]
    fn compress_roundtrips_through_decompress() {
        let plain = b"NAME      00042\nJANE      00007\n";
        for codec in [Compression::None, Compression::Gzip, Compression::Zstd] {
            let comp = compress(plain, codec).unwrap();
            assert_eq!(decompress(comp, codec).unwrap(), plain);
        }
        // The codec a write picks for a path round-trips via magic-byte detect.
        assert_eq!(Compression::from_path("out.dat.gz"), Compression::Gzip);
        assert_eq!(Compression::from_path("OUT.ZST"), Compression::Zstd);
        assert_eq!(Compression::from_path("plain.dat"), Compression::None);
        let gz = compress(plain, Compression::from_path("x.gz")).unwrap();
        assert_eq!(Compression::detect(&gz), Compression::Gzip);
    }

    #[test]
    fn multi_member_gzip_concatenates() {
        let mut concat = gzip(b"first\n");
        concat.extend_from_slice(&gzip(b"second\n"));
        assert_eq!(
            decompress(concat, Compression::Gzip).unwrap(),
            b"first\nsecond\n"
        );
    }

    #[test]
    fn corrupt_gzip_errors() {
        // Flip a byte in the 8-byte trailer (CRC32 + ISIZE), which the decoder
        // verifies on read_to_end — the header's MTIME field is not checked.
        let mut comp = gzip(b"hello world");
        let n = comp.len();
        comp[n - 1] ^= 0xff; // corrupt ISIZE
        assert!(decompress(comp, Compression::Gzip).is_err());
    }
}
