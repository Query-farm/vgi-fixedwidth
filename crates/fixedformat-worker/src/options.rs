//! Shared option parsing: turn a worker call's arguments into a [`Layout`] plus
//! encoding/framing selections.

use fixedformat_core::framing::Framing;
use fixedformat_core::{parse_spec, Encoding, Layout};
use vgi::arguments::Arguments;
use vgi_rpc::{Result, RpcError};

fn ve(msg: impl Into<String>) -> RpcError {
    RpcError::value_error(msg.into())
}

/// Parse the layout from a const spec argument at `spec_pos`, honoring an
/// optional named `format` override (`template` | `json` | `copybook`).
pub fn layout(args: &Arguments, spec_pos: usize) -> Result<Layout> {
    let spec = args
        .const_str(spec_pos)
        .ok_or_else(|| ve("a layout spec string is required"))?;
    let format = args.named_str("format");
    parse_spec(&spec, format.as_deref()).map_err(|e| ve(e.to_string()))
}

/// Resolve the byte encoding from a named `encoding` / `codepage` argument
/// (table / buffering functions, which support named parameters).
pub fn encoding(args: &Arguments) -> Result<Encoding> {
    match args.named_str("encoding").or_else(|| args.named_str("codepage")) {
        Some(s) => Encoding::parse(&s).map_err(|e| ve(e.to_string())),
        None => Ok(Encoding::Ascii),
    }
}

/// Resolve the byte encoding from a positional const argument (scalar functions,
/// which only support positional parameters). Absent ⇒ ASCII.
pub fn encoding_at(args: &Arguments, pos: usize) -> Result<Encoding> {
    match args.const_str(pos) {
        Some(s) => Encoding::parse(&s).map_err(|e| ve(e.to_string())),
        None => Ok(Encoding::Ascii),
    }
}

/// Resolve the record framing from a named `framing` argument (default:
/// newline-delimited).
pub fn framing(args: &Arguments) -> Result<Framing> {
    match args.named_str("framing") {
        Some(s) => Framing::parse(&s).map_err(|e| ve(e.to_string())),
        None => Ok(Framing::Newline),
    }
}
