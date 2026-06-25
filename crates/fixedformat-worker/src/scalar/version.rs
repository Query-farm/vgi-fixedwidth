//! `fixedformat_version()` — return the worker's version string.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::DataType;
use vgi::{
    ArgSpec, BindParams, BindResponse, FunctionExample, FunctionMetadata, ProcessParams,
    ScalarFunction,
};
use vgi_rpc::{Result, RpcError};

pub struct FixedFormatVersion;

impl ScalarFunction for FixedFormatVersion {
    fn name(&self) -> &str {
        "fixedformat_version"
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Returns the fixedformat worker version string".into(),
            return_type: Some(DataType::Utf8),
            examples: vec![FunctionExample {
                sql: "SELECT fixed.main.fixedformat_version();".into(),
                description: "Return the fixedformat worker version string.".into(),
                expected_output: None,
            }],
            tags: {
                let mut tags = crate::meta::object_tags(
                    "Fixed Format Worker Version",
                    "Return the version string of the running fixedformat worker binary (the \
                     worker's own build version, not the SDK/protocol version; it is the crate's \
                     Cargo version). The string is semver, MAJOR.MINOR.PATCH (e.g. '0.1.0'). The \
                     function takes no arguments and is deterministic — it always returns the same \
                     single VARCHAR value (never NULL) for a given build, so it need not be \
                     re-evaluated per row. Useful for diagnostics and confirming which build is \
                     attached.",
                    "Return the fixedformat worker version string, e.g. \
                     `fixedformat_version()` → '0.1.0'. Argument-free and deterministic; returns a \
                     single semver (MAJOR.MINOR.PATCH) VARCHAR.",
                    "version, build version, fixedformat_version, diagnostics, worker version, \
                     semver",
                );
                // VGI509: ship at least one guaranteed-runnable example. This one
                // needs no file or external backend, so it executes cleanly.
                tags.push((
                    "vgi.executable_examples".into(),
                    r#"[
  {
    "description": "Return the worker version string.",
    "sql": "SELECT fixed.main.fixedformat_version() AS version"
  },
  {
    "description": "Unpack a pure-ASCII fixed-width record (an 8-char name and a 5-char code) into a struct, then pack it back unchanged (a clean round-trip).",
    "sql": [
      "SELECT fixed.main.unpack_fixed('JohnDoe 12345', 'A8 A5') AS rec",
      "SELECT fixed.main.pack_fixed(fixed.main.unpack_fixed('JohnDoe 12345', 'A8 A5'), 'A8 A5')::VARCHAR AS roundtrip"
    ]
  }
]"#
                    .into(),
                ));
                tags
            },
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        Vec::new()
    }

    fn on_bind(&self, _params: &BindParams) -> Result<BindResponse> {
        Ok(BindResponse::result(DataType::Utf8))
    }

    fn process(&self, params: &ProcessParams, batch: &RecordBatch) -> Result<RecordBatch> {
        let rows = batch.num_rows();
        let out: ArrayRef = Arc::new(StringArray::from(vec![fixedformat_core::version(); rows]));
        RecordBatch::try_new(params.output_schema.clone(), vec![out])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}
