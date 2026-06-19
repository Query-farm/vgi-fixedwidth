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
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams, ScalarFunction};
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

impl ScalarFunction for Unpack {
    fn name(&self) -> &str {
        // `unpack` is a reserved keyword (type_function) in DuckDB, so the
        // SQL-callable name is `unpack_fixed`.
        "unpack_fixed"
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Parse a fixed-width record (template / JSON / copybook spec) into a STRUCT"
                .into(),
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        let mut specs = vec![
            ArgSpec::any_column("rec", 0, "Fixed-width record (VARCHAR or BLOB)"),
            ArgSpec::const_arg("spec", 1, "varchar", "Layout spec (template/JSON/copybook)"),
        ];
        if self.with_encoding {
            specs.push(ArgSpec::const_arg("encoding", 2, "varchar", "ascii (default) or ebcdic"));
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
        other => Err(ve(format!("unpack: rec must be VARCHAR or BLOB, got {other:?}"))),
    }
}
