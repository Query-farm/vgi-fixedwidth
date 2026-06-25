//! `pack(struct, spec [, format =>, encoding =>])` — format a STRUCT back into a
//! fixed-width BLOB. The clean inverse of `unpack`: `pack(unpack(rec,s),s) == rec`.

use std::sync::Arc;

use arrow_array::builder::BinaryBuilder;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::DataType;
use fixedformat_core::encode::encode_record;
use fixedformat_core::Value;
use vgi::{
    ArgSpec, BindParams, BindResponse, FunctionExample, FunctionMetadata, ProcessParams,
    ScalarFunction,
};
use vgi_rpc::{Result, RpcError};

use crate::options;
use crate::value_in::value_at;

/// `pack_fixed`. Like `unpack_fixed`, the optional `encoding` is a positional
/// const argument (registered as an arity overload), not a named parameter.
pub struct Pack {
    /// Whether this overload accepts the 3rd positional `encoding` argument.
    pub with_encoding: bool,
}

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Representative `vgi.example_queries` (VGI306) for the 2-argument `pack_fixed`
/// overload. Each entry is a self-contained, catalog-qualified query.
fn example_queries_json() -> String {
    r#"[
  {
    "description": "Pack a (name, code) struct into a fixed-width ASCII record blob.",
    "sql": "SELECT fixed.main.pack_fixed({'name': 'Jo', 'code': 'X1'}, 'A2 A2')"
  },
  {
    "description": "Round-trip: pack the struct that unpack_fixed produced.",
    "sql": "SELECT fixed.main.pack_fixed(fixed.main.unpack_fixed('JohnDoe 12345', 'A8 A5'), 'A8 A5')"
  }
]"#
    .to_string()
}

/// Representative `vgi.example_queries` (VGI306) for the 3-argument `pack_fixed`
/// overload — these exercise the distinguishing positional `encoding` argument.
fn example_queries_json_with_encoding() -> String {
    r#"[
  {
    "description": "Pack a struct into an EBCDIC (CP037) record blob; the encoding also governs zoned/COMP-3 sign nibbles.",
    "sql": "SELECT fixed.main.pack_fixed({'name': 'Jo', 'id': 7}, 'A2 N', 'ebcdic')"
  },
  {
    "description": "EBCDIC round-trip: pack with the same encoding that unpack used.",
    "sql": "SELECT fixed.main.pack_fixed(fixed.main.unpack_fixed(rec, 'A2 N', 'ebcdic'), 'A2 N', 'ebcdic') FROM (SELECT 'Jo\x00\x00\x00\x07'::BLOB AS rec)"
  }
]"#
    .to_string()
}

impl ScalarFunction for Pack {
    fn name(&self) -> &str {
        // Named `pack_fixed` for symmetry with `unpack_fixed` (`unpack` being a
        // reserved DuckDB keyword).
        "pack_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        // The two arity overloads register under the same name; give each a
        // distinct description and example so they don't collide (VGI120).
        let description = if self.with_encoding {
            "Format a STRUCT into a fixed-width record blob using the given layout spec and byte \
             encoding (ascii or ebcdic); the inverse of unpack_fixed"
        } else {
            "Format a STRUCT into a fixed-width record blob using the given layout spec (template \
             / JSON / copybook), emitting ASCII bytes; the inverse of unpack_fixed"
        };
        let example = if self.with_encoding {
            FunctionExample {
                sql: "SELECT fixed.main.pack_fixed({'name': 'Jo', 'id': 7}, 'A2 N', 'ebcdic');"
                    .into(),
                description: "Format a struct into an EBCDIC-encoded record blob.".into(),
                expected_output: None,
            }
        } else {
            FunctionExample {
                sql: "SELECT fixed.main.pack_fixed({'name': 'Jo', 'id': 7}, 'A2 N');".into(),
                description: "Format a (name, id) struct into a fixed-width record blob.".into(),
                expected_output: None,
            }
        };
        let mut tags = if self.with_encoding {
            crate::meta::object_tags(
                "Pack Fixed-Width Record (with encoding)",
                "Encode a STRUCT of field values back into a single fixed-width / flat-file record \
                 blob, controlling the byte `encoding`. This 3-argument overload adds a positional \
                 `encoding` argument: 'ascii' (the default) or 'ebcdic' (CP037). EBCDIC affects \
                 not only character fields but also the sign nibbles of zoned and COMP-3 \
                 (packed-decimal) numbers. The layout is the same kind of spec `unpack_fixed` uses \
                 (Perl/Python `unpack` template, JSON field list, or COBOL copybook); field values \
                 are matched to layout fields with padding, justification, packed/zoned decimals \
                 and sign handling applied per the spec. Returns a BLOB. This is the exact inverse \
                 of unpack_fixed when the same encoding is supplied to both: \
                 `pack_fixed(unpack_fixed(rec, s, e), s, e) = rec`.",
                "Format a STRUCT into a fixed-width record blob under an explicit byte encoding, \
                 e.g. `pack_fixed({'name': 'Jo', 'id': 7}, 'A2 N', 'ebcdic')`. The third argument \
                 is `encoding` — 'ascii' (default) or 'ebcdic' (CP037), which also governs \
                 zoned/COMP-3 sign nibbles. It is positional, not named. Returns a BLOB; the \
                 inverse of `unpack_fixed` under the same encoding.",
                "pack, encode, format, serialize, fixed-width, flat file, perl pack, python \
                 struct, copybook, COBOL, EBCDIC, CP037, encoding, COMP-3, zoned decimal, struct \
                 to record",
            )
        } else {
            crate::meta::object_tags(
                "Pack Fixed-Width Record",
                "Encode a STRUCT of field values back into a single fixed-width / flat-file record \
                 blob (ASCII bytes; use the 3-argument overload to emit EBCDIC), laid out by the \
                 same kind of spec `unpack_fixed` uses (Perl/Python `unpack` template, JSON field \
                 list, or COBOL copybook). Field values are matched to layout fields, with \
                 padding, justification, packed/zoned decimals and sign handling applied per the \
                 spec. Returns a BLOB. This is the exact inverse of unpack_fixed: \
                 `pack_fixed(unpack_fixed(rec, s), s) = rec`.",
                "Format a STRUCT into a fixed-width ASCII record blob, e.g. \
                 `pack_fixed({'name': 'Jo', 'id': 7}, 'A2 N')`. Returns a BLOB; the inverse of \
                 `unpack_fixed`. To emit EBCDIC, use the 3-argument overload with an `encoding` \
                 argument.",
                "pack, encode, format, serialize, fixed-width, flat file, perl pack, python \
                 struct, copybook, COBOL, EBCDIC, COMP-3, struct to record",
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
            return_type: Some(DataType::Binary),
            examples: vec![example],
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        let mut specs = vec![
            ArgSpec::any_column(
                "data",
                0,
                "The record to encode, as a STRUCT whose fields correspond to the layout's fields \
                 (the kind of value `unpack_fixed` returns).",
            ),
            ArgSpec::const_arg(
                "spec",
                1,
                "varchar",
                "The layout describing how to lay the fields out: a Perl/Python `unpack` template \
                 string (e.g. 'A2 N'), a JSON field list, or a COBOL copybook. The format is \
                 auto-detected.",
            ),
        ];
        if self.with_encoding {
            specs.push(ArgSpec::const_arg(
                "encoding",
                2,
                "varchar",
                "Byte encoding for the emitted record: 'ascii' (the default) or 'ebcdic' (CP037). \
                 Controls how text and zoned-number bytes are written.",
            ));
        }
        specs
    }

    fn on_bind(&self, _params: &BindParams) -> Result<BindResponse> {
        Ok(BindResponse::result(DataType::Binary))
    }

    fn process(&self, params: &ProcessParams, batch: &RecordBatch) -> Result<RecordBatch> {
        let layout = options::layout(&params.arguments, 1)?;
        let enc = options::encoding_at(&params.arguments, 2)?;

        let data = batch.column(0);
        let rows = batch.num_rows();
        let mut b = BinaryBuilder::new();
        for i in 0..rows {
            if data.is_null(i) {
                b.append_null();
                continue;
            }
            let pairs = match value_at(data, i)? {
                Value::Struct(pairs) => pairs,
                Value::Null => {
                    b.append_null();
                    continue;
                }
                other => {
                    return Err(ve(format!(
                        "pack: argument must be a STRUCT, got {other:?}"
                    )))
                }
            };
            let bytes = encode_record(&layout, &pairs, enc).map_err(ve)?;
            b.append_value(bytes);
        }

        let out: ArrayRef = Arc::new(b.finish());
        RecordBatch::try_new(params.output_schema.clone(), vec![out])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}
