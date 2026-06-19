//! Table functions exposed by the fixedformat worker.

mod read_fixed;

use vgi::Worker;
use vgi_rpc::{Result, RpcError};

/// Register every table function on the worker.
pub fn register(worker: &mut Worker) {
    worker.register_table(read_fixed::ReadFixed);
}

/// Resolve a path spec to a concrete, sorted list of files (globs expand; a
/// literal path must exist). Mirrors the miint reader convention.
pub(crate) fn resolve_paths(spec: &str) -> Result<Vec<String>> {
    if spec.contains('*') || spec.contains('?') || spec.contains('[') {
        let mut out = Vec::new();
        let entries = glob::glob(spec)
            .map_err(|e| RpcError::value_error(format!("bad glob '{spec}': {e}")))?;
        for entry in entries.flatten() {
            out.push(entry.to_string_lossy().into_owned());
        }
        out.sort();
        Ok(out)
    } else if std::path::Path::new(spec).exists() {
        Ok(vec![spec.to_string()])
    } else {
        Err(RpcError::value_error(format!("File not found: {spec}")))
    }
}
