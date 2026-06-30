//! Table functions exposed by the fixedformat worker.

mod describe_fixed;
mod read_fixed;
pub(crate) mod read_multi;

use vgi::secrets::Secrets;
use vgi::Worker;
use vgi_rpc::{Result, RpcError};

use crate::cloud::{self, Location};

/// Register every table function on the worker.
pub fn register(worker: &mut Worker) {
    worker.register_table(read_fixed::ReadFixed);
    worker.register_table(read_multi::ReadMulti);
    worker.register_table(describe_fixed::DescribeFixed);
}

/// Resolve a local path spec to a concrete, sorted list of files (globs expand;
/// a literal path must exist). Mirrors the miint reader convention.
pub(crate) fn resolve_local(spec: &str) -> Result<Vec<String>> {
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

/// Resolve a path spec to concrete [`Location`]s. Local paths expand via
/// [`resolve_local`]; remote URLs (`s3://`, `http(s)://`) expand via object-store
/// listing when they contain a glob, else address a single object. `secrets` /
/// `overrides` are needed to build the store for remote globbing.
pub(crate) fn resolve_locations(
    spec: &str,
    secrets: &Secrets,
    overrides: &[(String, String)],
) -> Result<Vec<Location>> {
    match cloud::classify(spec)? {
        Location::Local(p) => Ok(resolve_local(&p)?
            .into_iter()
            .map(Location::Local)
            .collect()),
        Location::Remote(url) => {
            // Check the decoded key so a `?` wildcard (URL-encoded in the key) is
            // still recognized as a glob.
            if cloud::remote_key(&url).contains(['*', '?', '[']) {
                Ok(cloud::list_glob(&url, secrets, overrides)?
                    .into_iter()
                    .map(Location::Remote)
                    .collect())
            } else {
                Ok(vec![Location::Remote(url)])
            }
        }
    }
}
