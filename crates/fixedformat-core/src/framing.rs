//! Record framing — splitting a raw byte stream into individual record slices.
//!
//! Three modes cover ASCII flat files through mainframe variable-length data:
//! - [`Framing::Newline`] — records terminated by `\n` (a trailing `\r` is
//!   stripped). Used for ASCII exports.
//! - [`Framing::Fixed`] — no delimiters; every record is exactly `record_len`
//!   bytes (RECFM=FB).
//! - [`Framing::Rdw`] / [`Framing::RdwBlocked`] — IBM variable-length: each
//!   record is prefixed by a 4-byte Record Descriptor Word (big-endian length
//!   *including* the 4-byte header, then two zero bytes). The blocked variant
//!   additionally strips a leading Block Descriptor Word from each block.

use crate::{Error, Result};

/// How records are delimited within a byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// `\n`-terminated (CRLF tolerant).
    #[default]
    Newline,
    /// Fixed-length, no delimiters.
    Fixed,
    /// Variable-length, RDW per record (unblocked / already deblocked).
    Rdw,
    /// Variable-length blocked: BDW per block, RDW per record.
    RdwBlocked,
}

impl Framing {
    /// Parse a `framing =>` option value.
    pub fn parse(s: &str) -> Result<Framing> {
        match s.to_ascii_lowercase().as_str() {
            "newline" | "lines" | "text" => Ok(Framing::Newline),
            "fixed" | "fb" | "none" => Ok(Framing::Fixed),
            "rdw" | "vb" | "variable" => Ok(Framing::Rdw),
            "rdw_blocked" | "rdwblocked" | "vb_blocked" | "blocked" => Ok(Framing::RdwBlocked),
            other => Err(Error(format!("unknown framing: {other}"))),
        }
    }
}

/// Split `data` into record slices according to `framing`. `record_len` is the
/// layout's record length, required for [`Framing::Fixed`].
pub fn records(data: &[u8], framing: Framing, record_len: usize) -> Result<Vec<&[u8]>> {
    match framing {
        Framing::Newline => Ok(newline(data)),
        Framing::Fixed => fixed(data, record_len),
        Framing::Rdw => rdw(data),
        Framing::RdwBlocked => rdw_blocked(data),
    }
}

fn newline(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            let mut end = i;
            if end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(&data[start..end]);
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

fn fixed(data: &[u8], record_len: usize) -> Result<Vec<&[u8]>> {
    if record_len == 0 {
        return Err(Error("fixed framing requires a non-zero record length".into()));
    }
    if data.len() % record_len != 0 {
        return Err(Error(format!(
            "fixed-length stream of {} bytes is not a multiple of record length {record_len}",
            data.len()
        )));
    }
    Ok(data.chunks(record_len).collect())
}

/// Read a 4-byte RDW/BDW: big-endian total length (incl. the 4-byte header).
fn descriptor_len(data: &[u8], at: usize) -> Result<usize> {
    if at + 4 > data.len() {
        return Err(Error("truncated RDW/BDW: fewer than 4 bytes remain".into()));
    }
    let len = u16::from_be_bytes([data[at], data[at + 1]]) as usize;
    if len < 4 {
        return Err(Error(format!("invalid descriptor word length {len} (< 4)")));
    }
    Ok(len)
}

fn rdw(data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let len = descriptor_len(data, pos)?;
        let end = pos + len;
        if end > data.len() {
            return Err(Error(format!(
                "RDW record length {len} at offset {pos} overruns the stream"
            )));
        }
        out.push(&data[pos + 4..end]);
        pos = end;
    }
    Ok(out)
}

fn rdw_blocked(data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let block_len = descriptor_len(data, pos)?;
        let block_end = pos + block_len;
        if block_end > data.len() {
            return Err(Error(format!(
                "BDW block length {block_len} at offset {pos} overruns the stream"
            )));
        }
        let mut inner = pos + 4;
        while inner < block_end {
            let rec_len = descriptor_len(data, inner)?;
            let rec_end = inner + rec_len;
            if rec_end > block_end {
                return Err(Error(format!(
                    "RDW record length {rec_len} at offset {inner} overruns its block"
                )));
            }
            out.push(&data[inner + 4..rec_end]);
            inner = rec_end;
        }
        pos = block_end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_splits_and_strips_cr() {
        let recs = records(b"abc\r\ndef\nghi", Framing::Newline, 0).unwrap();
        assert_eq!(recs, vec![&b"abc"[..], &b"def"[..], &b"ghi"[..]]);
    }

    #[test]
    fn newline_no_trailing_empty() {
        let recs = records(b"abc\ndef\n", Framing::Newline, 0).unwrap();
        assert_eq!(recs, vec![&b"abc"[..], &b"def"[..]]);
    }

    #[test]
    fn fixed_chunks() {
        let recs = records(b"aaabbbccc", Framing::Fixed, 3).unwrap();
        assert_eq!(recs, vec![&b"aaa"[..], &b"bbb"[..], &b"ccc"[..]]);
    }

    #[test]
    fn fixed_rejects_ragged() {
        assert!(records(b"aaabb", Framing::Fixed, 3).is_err());
    }

    #[test]
    fn rdw_reads_records() {
        // Two records "AB" (len 6) and "CDE" (len 7).
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x06, 0x00, 0x00]);
        data.extend_from_slice(b"AB");
        data.extend_from_slice(&[0x00, 0x07, 0x00, 0x00]);
        data.extend_from_slice(b"CDE");
        let recs = records(&data, Framing::Rdw, 0).unwrap();
        assert_eq!(recs, vec![&b"AB"[..], &b"CDE"[..]]);
    }

    #[test]
    fn rdw_blocked_reads_records() {
        // One block (BDW len = 4 + 6 + 7 = 17) holding two records.
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 17, 0x00, 0x00]); // BDW
        data.extend_from_slice(&[0x00, 0x06, 0x00, 0x00]); // RDW "AB"
        data.extend_from_slice(b"AB");
        data.extend_from_slice(&[0x00, 0x07, 0x00, 0x00]); // RDW "CDE"
        data.extend_from_slice(b"CDE");
        let recs = records(&data, Framing::RdwBlocked, 0).unwrap();
        assert_eq!(recs, vec![&b"AB"[..], &b"CDE"[..]]);
    }

    #[test]
    fn rdw_truncation_errors() {
        let data = [0x00, 0x09, 0x00, 0x00, b'A']; // claims 9, only 5 present
        assert!(records(&data, Framing::Rdw, 0).is_err());
    }
}
