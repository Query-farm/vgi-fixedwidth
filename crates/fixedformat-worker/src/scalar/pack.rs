//! `pack(struct, spec [, format =>, encoding =>])` — format a STRUCT back into a
//! fixed-width BLOB. The clean inverse of `unpack`: `pack(unpack(rec,s),s) == rec`.

use std::sync::Arc;

use arrow_array::builder::BinaryBuilder;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::DataType;
use fixedformat_core::encode::encode_record;
use fixedformat_core::Value;
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams, ScalarFunction};
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

impl ScalarFunction for Pack {
    fn name(&self) -> &str {
        // Named `pack_fixed` for symmetry with `unpack_fixed` (`unpack` being a
        // reserved DuckDB keyword).
        "pack_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Format a STRUCT into a fixed-width record BLOB (inverse of unpack_fixed)"
                .into(),
            return_type: Some(DataType::Binary),
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        let mut specs = vec![
            ArgSpec::any_column("data", 0, "STRUCT of field values to format"),
            ArgSpec::const_arg("spec", 1, "varchar", "Layout spec (template/JSON/copybook)"),
        ];
        if self.with_encoding {
            specs.push(ArgSpec::const_arg("encoding", 2, "varchar", "ascii (default) or ebcdic"));
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
                other => return Err(ve(format!("pack: argument must be a STRUCT, got {other:?}"))),
            };
            let bytes = encode_record(&layout, &pairs, enc).map_err(ve)?;
            b.append_value(bytes);
        }

        let out: ArrayRef = Arc::new(b.finish());
        RecordBatch::try_new(params.output_schema.clone(), vec![out])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}
