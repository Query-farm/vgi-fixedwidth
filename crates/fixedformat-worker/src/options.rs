//! Shared option parsing: turn a worker call's arguments into a [`Layout`] plus
//! encoding/framing selections.

use fixedformat_core::compression::Compression;
use fixedformat_core::framing::Framing;
use fixedformat_core::stream::Limits;
use fixedformat_core::{parse_spec, Encoding, Layout};
use vgi::arguments::Arguments;
use vgi::ArgSpec;
use vgi_rpc::{Result, RpcError};

fn ve(msg: impl Into<String>) -> RpcError {
    RpcError::value_error(msg.into())
}

/// Read the path argument at `pos` as one or more paths: a single VARCHAR yields
/// one path; a `LIST(VARCHAR)` const yields each element (so `read_fixed` accepts
/// both `'s3://b/x.dat'` and `['s3://b/x.dat','s3://c/y.dat']`). Non-string and
/// null elements are skipped.
pub fn paths(args: &Arguments, pos: usize) -> Result<Vec<String>> {
    use arrow_array::cast::AsArray;
    use arrow_array::Array;
    // Single string (the common case).
    if let Some(s) = args.const_str(pos) {
        return Ok(vec![s]);
    }
    // LIST(VARCHAR): the 1-row positional arg is a list; read its element array.
    let Some(arr) = args.arg(pos) else {
        return Err(ve("a path (or list of paths) is required"));
    };
    let elems = if let Some(l) = arr.as_list_opt::<i32>() {
        l.value(0)
    } else if let Some(l) = arr.as_list_opt::<i64>() {
        l.value(0)
    } else {
        return Err(ve("path must be a VARCHAR or a LIST(VARCHAR)"));
    };
    let mut out = Vec::with_capacity(elems.len());
    if let Some(s) = elems.as_string_opt::<i32>() {
        for i in 0..s.len() {
            if s.is_valid(i) {
                out.push(s.value(i).to_string());
            }
        }
    } else if let Some(s) = elems.as_string_opt::<i64>() {
        for i in 0..s.len() {
            if s.is_valid(i) {
                out.push(s.value(i).to_string());
            }
        }
    } else {
        return Err(ve("path list elements must be VARCHAR"));
    }
    if out.is_empty() {
        return Err(ve("path list is empty"));
    }
    Ok(out)
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
    match args
        .named_str("encoding")
        .or_else(|| args.named_str("codepage"))
    {
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

/// Resolve the input compression from a named `compression` argument.
/// Returns `None` for "auto" (the default when the arg is absent) — meaning the
/// reader should detect the codec from the data's magic bytes — and `Some(codec)`
/// for an explicit `none` / `gzip` / `zstd`.
pub fn compression(args: &Arguments) -> Result<Option<Compression>> {
    match args.named_str("compression") {
        None => Ok(None),
        Some(s) if s.trim().eq_ignore_ascii_case("auto") => Ok(None),
        Some(s) => Compression::parse(&s)
            .map(Some)
            .map_err(|e| ve(e.to_string())),
    }
}

/// Resolve the read resource limits (decompression-bomb / pathological-record
/// backstops) from named arguments, falling back to [`Limits::default`].
/// `max_decompressed_bytes =>` raises (or lowers) the cap on total decompressed
/// bytes per source.
pub fn read_limits(args: &Arguments) -> Result<Limits> {
    let mut limits = Limits::default();
    if let Some(n) = args.named_i64("max_decompressed_bytes") {
        if n <= 0 {
            return Err(ve("max_decompressed_bytes must be a positive number of bytes"));
        }
        limits.max_decompressed_bytes = n as u64;
    }
    Ok(limits)
}

/// Named-argument object-store overrides (`endpoint =>`, `region =>`,
/// `url_style =>`, `use_ssl =>`) for `s3://` paths, mapped to `object_store`
/// `aws_*` config keys. These layer over (and win against) any secret-derived
/// config, letting a caller hit MinIO / a custom endpoint without a `CREATE
/// SECRET`. Returns an empty vec when none are given.
pub fn cloud_overrides(args: &Arguments) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // `use_ssl` is a BOOLEAN arg; fall back to a string form for robustness.
    let use_ssl = args.named_bool("use_ssl").or_else(|| {
        args.named_str("use_ssl")
            .and_then(|v| crate::cloud::parse_bool(&v))
    });
    if let Some(ep) = args.named_str("endpoint") {
        out.push((
            "aws_endpoint".into(),
            crate::cloud::normalize_endpoint(&ep, use_ssl),
        ));
    }
    if let Some(r) = args.named_str("region") {
        out.push(("aws_region".into(), r));
    }
    if let Some(s) = args.named_str("url_style") {
        if s.eq_ignore_ascii_case("path") {
            out.push(("aws_virtual_hosted_style_request".into(), "false".into()));
        }
    }
    if use_ssl == Some(false) {
        out.push(("aws_allow_http".into(), "true".into()));
    }
    out
}

/// Named argument specs for the object-store overrides, shared by `read_fixed`
/// and `write_fixed` so both accept `endpoint =>` / `region =>` / `url_style =>`
/// / `use_ssl =>` (DuckDB's binder drops named args that aren't declared).
pub fn cloud_arg_specs() -> Vec<ArgSpec> {
    vec![
        ArgSpec::const_arg(
            "endpoint",
            -1,
            "varchar",
            "Custom S3 endpoint for an `s3://` path (e.g. MinIO/R2 'host:9000'); a scheme is \
             inferred from `use_ssl` when omitted. Overrides any endpoint from a CREATE SECRET.",
        ),
        ArgSpec::const_arg(
            "region",
            -1,
            "varchar",
            "AWS region for an `s3://` path. Overrides the region from a CREATE SECRET.",
        ),
        ArgSpec::const_arg(
            "url_style",
            -1,
            "varchar",
            "S3 addressing for an `s3://` path: 'path' (path-style, e.g. MinIO) or 'vhost' \
             (the default). Overrides a CREATE SECRET's URL_STYLE.",
        ),
        ArgSpec::const_arg(
            "use_ssl",
            -1,
            "boolean",
            "Whether to use TLS for an `s3://` path's custom endpoint (default true). Set false \
             for a plain-HTTP endpoint such as a local MinIO.",
        ),
    ]
}
