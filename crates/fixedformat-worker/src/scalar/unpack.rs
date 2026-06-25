//! `unpack(rec, spec [, format =>, encoding =>])` — parse a fixed-width string
//! or blob into a STRUCT whose fields are the layout's fields.
//!
//! The spec is a bind-time constant, so the STRUCT output type is resolved in
//! `on_bind`. `rec` may be VARCHAR or BLOB.

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::DataType;
use fixedformat_core::decode::decode_record;
use fixedformat_core::Value;
use vgi::{
    ArgSpec, BindParams, BindResponse, FunctionExample, FunctionMetadata, ProcessParams,
    ScalarFunction,
};
use vgi_rpc::{Result, RpcError};

use crate::arrow_map::{build_array, layout_fields};
use crate::options;

/// `unpack_fixed`. DuckDB scalar functions only support positional arguments, so
/// the optional `encoding` is a 3rd positional const (registered as a separate
/// arity overload) rather than a named parameter.
pub struct Unpack {
    /// Whether this overload accepts the 3rd positional `encoding` argument.
    pub with_encoding: bool,
}

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Representative `vgi.example_queries` (VGI306) for the 2-argument `unpack_fixed`
/// overload. Each entry is a self-contained, catalog-qualified query.
fn example_queries_json() -> String {
    r#"[
  {
    "description": "Unpack an 8-char name and a 5-char code from an ASCII record.",
    "sql": "SELECT fixed.main.unpack_fixed('JohnDoe 12345', 'A8 A5')"
  },
  {
    "description": "Unpack three space-padded text fields from one record.",
    "sql": "SELECT fixed.main.unpack_fixed('ACME      NY 100', 'A10 A3 A3')"
  }
]"#
    .to_string()
}

/// Representative `vgi.example_queries` (VGI306) for the 3-argument `unpack_fixed`
/// overload — these exercise the distinguishing positional `encoding` argument.
fn example_queries_json_with_encoding() -> String {
    r#"[
  {
    "description": "Unpack an ASCII record by passing 'ascii' explicitly (the default).",
    "sql": "SELECT fixed.main.unpack_fixed('JohnDoe 12345', 'A8 A5', 'ascii')"
  },
  {
    "description": "Unpack an EBCDIC (CP037) record; the encoding also governs zoned/COMP-3 sign nibbles.",
    "sql": "SELECT fixed.main.unpack_fixed(rec, 'A8 N', 'ebcdic') FROM (SELECT 'JohnDoe \x00\x00\x00\x2A'::BLOB AS rec)"
  }
]"#
    .to_string()
}

impl ScalarFunction for Unpack {
    fn name(&self) -> &str {
        // `unpack` is a reserved keyword (type_function) in DuckDB, so the
        // SQL-callable name is `unpack_fixed`.
        "unpack_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        // The two arity overloads register under the same name; give each a
        // distinct description and example so they don't collide (VGI120).
        let description = if self.with_encoding {
            "Parse a fixed-width record into a STRUCT using the given layout spec and byte \
             encoding (ascii or ebcdic)"
        } else {
            "Parse a fixed-width record into a STRUCT using the given layout spec (template / \
             JSON / copybook), assuming ASCII bytes"
        };
        let example = if self.with_encoding {
            FunctionExample {
                sql: "SELECT fixed.main.unpack_fixed(rec, 'A8 N', 'ebcdic') FROM (SELECT \
                      'JohnDoe \\x00\\x00\\x00\\x2A'::BLOB AS rec);"
                    .into(),
                description: "Parse an EBCDIC-encoded record into a STRUCT.".into(),
                expected_output: None,
            }
        } else {
            FunctionExample {
                sql: "SELECT fixed.main.unpack_fixed('JohnDoe \\x00\\x00\\x00\\x2A'::BLOB, \
                      'A8 N');"
                    .into(),
                description: "Parse an 8-char name plus a big-endian 32-bit id into a STRUCT."
                    .into(),
                expected_output: None,
            }
        };
        let mut tags = if self.with_encoding {
            crate::meta::object_tags(
                "Unpack Fixed-Width Record (with encoding)",
                "Decode a single fixed-width / flat-file record (a VARCHAR or BLOB) into a typed \
                 STRUCT whose fields are named and typed by the layout spec, controlling the byte \
                 `encoding`. This 3-argument overload adds a positional `encoding` argument: \
                 'ascii' (the default) or 'ebcdic' (CP037). EBCDIC affects not only character \
                 fields but also the sign nibbles of zoned and COMP-3 (packed-decimal) numbers. \
                 The spec is a Perl/Python `unpack` template string, a JSON field list, or a COBOL \
                 copybook (auto-detected), and supports packed/zoned decimals, OCCURS lists (→ \
                 LIST), groups and REDEFINES (→ STRUCT). The spec is a bind-time constant so the \
                 STRUCT output type is known at plan time. This is the inverse of pack_fixed when \
                 the same encoding is supplied to both.",
                "Parse a fixed-width record into a STRUCT under an explicit byte encoding, e.g. \
                 `unpack_fixed(rec, 'A8 N', 'ebcdic')`. The third argument is `encoding` — 'ascii' \
                 (default) or 'ebcdic' (CP037), which also governs zoned/COMP-3 sign nibbles. It \
                 is positional, not named.",
                "unpack, parse, decode, fixed-width, flat file, perl unpack, python struct, \
                 copybook, COBOL, EBCDIC, CP037, encoding, COMP-3, zoned decimal, record to struct",
            )
        } else {
            crate::meta::object_tags(
                "Unpack Fixed-Width Record",
                "Decode a single fixed-width / flat-file record (a VARCHAR or BLOB) into a typed \
                 STRUCT whose fields are named and typed by the layout spec, assuming ASCII bytes \
                 (use the 3-argument overload to decode EBCDIC). The spec is a Perl/Python \
                 `unpack` template string, a JSON field list, or a COBOL copybook (auto-detected), \
                 and supports packed/zoned decimals, OCCURS lists (→ LIST), groups and REDEFINES \
                 (→ STRUCT). The spec is a bind-time constant so the STRUCT output type is known \
                 at plan time. This is the inverse of pack_fixed.",
                "Parse a fixed-width ASCII record into a STRUCT, e.g. \
                 `unpack_fixed('JohnDoe ...', 'A8 N')`. The layout spec is a template string, JSON \
                 spec, or COBOL copybook. To decode EBCDIC, use the 3-argument overload with an \
                 `encoding` argument.",
                "unpack, parse, decode, fixed-width, flat file, perl unpack, python struct, \
                 copybook, COBOL, EBCDIC, COMP-3, record to struct",
            )
        };
        let examples_json = if self.with_encoding {
            example_queries_json_with_encoding()
        } else {
            example_queries_json()
        };
        tags.push(("vgi.example_queries".into(), examples_json));
        FunctionMetadata {
            description: description.into(),
            examples: vec![example],
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        let mut specs = vec![
            ArgSpec::any_column(
                "rec",
                0,
                "A single fixed-width record to decode — one row's worth of bytes, laid out as \
                 described by `spec`.",
            ),
            ArgSpec::const_arg(
                "spec",
                1,
                "varchar",
                "The layout describing the record's fields: a Perl/Python `unpack` template \
                 string (e.g. 'A8 N'), a JSON field list, or a COBOL copybook. The format is \
                 auto-detected. Determines the STRUCT field names and types.",
            ),
        ];
        if self.with_encoding {
            specs.push(ArgSpec::const_arg(
                "encoding",
                2,
                "varchar",
                "Byte encoding of the record: 'ascii' (the default) or 'ebcdic' (CP037). \
                 Controls how text and zoned-number bytes are interpreted.",
            ));
        }
        specs
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let layout = options::layout(&params.arguments, 1)?;
        let fields = layout_fields(&layout)?;
        Ok(BindResponse::result(DataType::Struct(fields)))
    }

    fn process(&self, params: &ProcessParams, batch: &RecordBatch) -> Result<RecordBatch> {
        let layout = options::layout(&params.arguments, 1)?;
        let enc = options::encoding_at(&params.arguments, 2)?;
        let struct_ty = params.output_schema.field(0).data_type().clone();

        let rec = batch.column(0);
        let rows = batch.num_rows();
        let mut col: Vec<Value> = Vec::with_capacity(rows);
        for i in 0..rows {
            if rec.is_null(i) {
                col.push(Value::Null);
                continue;
            }
            let bytes = record_bytes(rec, i)?;
            let pairs = decode_record(&layout, bytes, enc).map_err(ve)?;
            col.push(Value::Struct(pairs));
        }

        let out: ArrayRef = build_array(&struct_ty, &col)?;
        RecordBatch::try_new(params.output_schema.clone(), vec![out])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}

/// Borrow the raw bytes of a record cell, accepting VARCHAR or BLOB columns.
fn record_bytes(rec: &ArrayRef, i: usize) -> Result<&[u8]> {
    match rec.data_type() {
        DataType::Utf8 => Ok(rec.as_string::<i32>().value(i).as_bytes()),
        DataType::LargeUtf8 => Ok(rec.as_string::<i64>().value(i).as_bytes()),
        DataType::Binary => Ok(rec.as_binary::<i32>().value(i)),
        DataType::LargeBinary => Ok(rec.as_binary::<i64>().value(i)),
        other => Err(ve(format!(
            "unpack: rec must be VARCHAR or BLOB, got {other:?}"
        ))),
    }
}
