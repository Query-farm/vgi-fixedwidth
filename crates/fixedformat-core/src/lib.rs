//! `fixedformat-core` — pure, dependency-light fixed-width / Perl-`unpack` /
//! Python-`struct` / COBOL-copybook parsing and formatting.
//!
//! This crate has **no Arrow or VGI dependency**: it turns spec strings into a
//! [`Layout`] IR and decodes/encodes record bytes to/from a neutral [`Value`]
//! tree. The `fixedformat-worker` crate adapts those to DuckDB over Arrow. All
//! correctness lives here and is unit-tested directly.

pub mod compression;
pub mod copybook;
pub mod decode;
pub mod describe;
pub mod ebcdic;
pub mod encode;
pub mod framing;
pub mod jsonspec;
pub mod layout;
pub mod packed;
pub mod stream;
pub mod template;
pub mod value;
pub mod zoned;

pub use layout::{parse_spec, Endian, Field, FieldKind, Justify, Layout, NumRepr, SignKind};
pub use value::Value;

/// The crate / worker version string.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A spec or codec error. Surfaces in DuckDB as an "Invalid Input Error", so the
/// message body is what `statement error` tests match against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Convenience constructor for an [`Error`] from anything `Display`.
pub fn err(msg: impl std::fmt::Display) -> Error {
    Error(msg.to_string())
}

/// The crate result type.
pub type Result<T> = std::result::Result<T, Error>;

/// The byte encoding of a record stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// ASCII / Latin-1 — bytes are used as-is.
    #[default]
    Ascii,
    /// EBCDIC code page 037 — transcoded on the way in/out.
    Ebcdic,
}

impl Encoding {
    /// Parse an `encoding =>` option value.
    pub fn parse(s: &str) -> Result<Encoding> {
        match s.to_ascii_lowercase().as_str() {
            "ascii" | "latin1" | "latin-1" | "utf8" | "utf-8" => Ok(Encoding::Ascii),
            "ebcdic" | "cp037" | "ibm037" | "ibm-037" => Ok(Encoding::Ebcdic),
            other => Err(Error(format!("unknown encoding: {other}"))),
        }
    }
}
