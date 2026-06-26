//! Cloud object-store access for `s3://` and `http(s)://` paths.
//!
//! The worker runs as a subprocess outside DuckDB, so it has no `httpfs`. This
//! module is the single home for object-store I/O: it classifies a path as local
//! vs remote, maps a DuckDB `s3` secret (resolved via the VGI two-phase secret
//! bind) onto [`object_store`] S3 credentials, and reads/writes/lists objects.
//!
//! Scope (first cut): `s3://` (AWS S3, plus R2 / MinIO / GCS-HMAC via a `TYPE s3`
//! secret with `ENDPOINT`/`URL_STYLE`) and `http(s)://` reads. Native `gs://` /
//! `az://` are deliberately unsupported for now (a clear error, not a silent
//! local-file fallback).
//!
//! The worker is synchronous and, on the stdio transport, runs without an
//! ambient tokio runtime, so [`block_on`] owns one; under the HTTP transport it
//! reuses the ambient runtime via `block_in_place`.

use std::future::Future;
use std::sync::OnceLock;

use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutPayload};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use url::Url;
use vgi::secrets::{SecretLookup, Secrets};
use vgi_rpc::{Result, RpcError};

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Characters in an `s3://` key that must be percent-encoded before `Url::parse`
/// so they survive as part of the key: `?`/`#` are URL delimiters (query /
/// fragment) that would otherwise truncate the key, and `%` is encoded so any
/// `%xx` already in the key round-trips losslessly. Crucially this keeps a `?`
/// glob wildcard intact (the `url` crate would otherwise eat it as a query). All
/// of `*`, `[`, `]` pass through `Url` unharmed, so they are not encoded here.
/// object_store reverses this via `Path::from_url_path` (which percent-decodes).
const S3_KEY_ESCAPE: &AsciiSet = &CONTROLS.add(b'%').add(b'?').add(b'#');

/// A resolved path: either a local filesystem path or a remote object URL.
pub enum Location {
    Local(String),
    Remote(Url),
}

/// URL schemes routed to the object store. Anything else with a `scheme://`
/// shape is rejected (rather than silently treated as a local file).
const REMOTE_SCHEMES: &[&str] = &["s3", "s3a", "http", "https"];

/// Classify a `path` argument as a local file path or a remote object URL.
pub fn classify(path: &str) -> Result<Location> {
    if let Some((scheme, rest)) = path.split_once("://") {
        let lower = scheme.to_ascii_lowercase();
        match lower.as_str() {
            // s3: parse via an escaped key so glob/delimiter chars survive.
            "s3" | "s3a" => {
                let url = Url::parse(&encode_s3_url(&lower, rest))
                    .map_err(|e| ve(format!("bad URL '{path}': {e}")))?;
                return Ok(Location::Remote(url));
            }
            // http(s): a real URL — `?`/`#` are legitimately query/fragment.
            "http" | "https" => {
                let url = Url::parse(path).map_err(|e| ve(format!("bad URL '{path}': {e}")))?;
                return Ok(Location::Remote(url));
            }
            // Scheme-shaped but unknown (e.g. `gs://`, `az://`): refuse loudly so
            // it never gets misread as a local path.
            _ if !lower.is_empty()
                && lower
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
            {
                return Err(ve(format!(
                    "unsupported URL scheme '{lower}://' for '{path}' (supported: s3://, \
                     http://, https://; local paths have no scheme)"
                )));
            }
            _ => {}
        }
    }
    Ok(Location::Local(path.to_string()))
}

/// Build an `s3://bucket/key` URL string with the key's URL-delimiter chars
/// percent-encoded (see [`S3_KEY_ESCAPE`]) so `Url::parse` preserves the whole
/// key — including a `?` glob wildcard.
fn encode_s3_url(scheme: &str, rest: &str) -> String {
    let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
    let key_enc = utf8_percent_encode(key, S3_KEY_ESCAPE);
    format!("{scheme}://{bucket}/{key_enc}")
}

/// The decoded object key of a remote URL — the literal key with glob
/// metacharacters intact (reverses [`encode_s3_url`] and any encoding the `url`
/// crate applied).
pub fn remote_key(url: &Url) -> String {
    let p = url.path().strip_prefix('/').unwrap_or(url.path());
    percent_decode_str(p).decode_utf8_lossy().into_owned()
}

/// True if the path is (or parses to) a remote URL — a quick check that does not
/// allocate a `Url` for the common local case.
pub fn is_remote(path: &str) -> bool {
    path.split_once("://")
        .map(|(s, _)| REMOTE_SCHEMES.contains(&s.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// The DuckDB secret type to request for a remote URL, or `None` when the scheme
/// needs no credentials (`http(s)://`).
pub fn secret_type_for(url: &Url) -> Option<&'static str> {
    match url.scheme() {
        "s3" | "s3a" => Some("s3"),
        _ => None,
    }
}

/// The DuckDB secret to request for a `path` argument: an `s3`-type secret
/// **scoped to the URL** for `s3://` paths, or `None` for local / no-credential
/// (`http(s)://`) paths. Both `read_fixed` and `write_fixed` use this so a single
/// place decides what gets requested via the two-phase secret bind. Best-effort:
/// an unclassifiable path yields `None`; the real error surfaces at bind time.
pub fn secret_lookup(path: &str) -> Option<SecretLookup> {
    match classify(path) {
        Ok(Location::Remote(url)) => secret_type_for(&url).map(|t| SecretLookup {
            secret_type: t.to_string(),
            scope: Some(url.to_string()),
            name: None,
        }),
        _ => None,
    }
}

/// The secret lookups to request for a set of `paths` — one per distinct
/// (type, scope), so a multi-path call spanning several buckets resolves the
/// right secret for each. Deduped on (secret_type, scope).
pub fn secret_lookups(paths: &[String]) -> Vec<SecretLookup> {
    let mut out: Vec<SecretLookup> = Vec::new();
    for p in paths {
        if let Some(l) = secret_lookup(p) {
            if !out
                .iter()
                .any(|e| e.secret_type == l.secret_type && e.scope == l.scope)
            {
                out.push(l);
            }
        }
    }
    out
}

/// A shared multi-thread runtime owned by this process for cloud I/O. Built once.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for cloud I/O")
    })
}

/// Drive a future to completion from synchronous code. Reuses an ambient runtime
/// (HTTP transport) via `block_in_place`; otherwise uses the owned runtime
/// (stdio transport). Avoids the "runtime within a runtime" panic either way.
fn block_on<F: Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(move || handle.block_on(fut)),
        Err(_) => runtime().block_on(fut),
    }
}

pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// DuckDB stores an `s3` endpoint as a bare `host[:port]`; object_store wants a
/// URL. Prepend a scheme (honoring `use_ssl`) when one is absent.
pub(crate) fn normalize_endpoint(ep: &str, use_ssl: Option<bool>) -> String {
    if ep.contains("://") {
        ep.to_string()
    } else {
        let scheme = if use_ssl == Some(false) { "http" } else { "https" };
        format!("{scheme}://{ep}")
    }
}

/// Build an object store for `url`, mapping the resolved DuckDB `s3` secret fields
/// onto object_store S3 config keys. `overrides` are named-argument options
/// (`endpoint =>`, `region =>`, …) that win over secret-derived values. Returns
/// the store plus the object key (`Path`) addressed by the URL.
pub fn build_store(
    url: &Url,
    secrets: &Secrets,
    overrides: &[(String, String)],
) -> Result<(Box<dyn ObjectStore>, ObjPath)> {
    let mut opts: Vec<(String, String)> = if secret_type_for(url) == Some("s3") {
        s3_options(secrets, url)
    } else {
        Vec::new()
    };

    // Named-argument overrides take precedence (parse_url_opts uses the last
    // value for a repeated key).
    opts.extend(overrides.iter().cloned());

    let (store, path) =
        object_store::parse_url_opts(url, opts).map_err(|e| ve(format!("init store for '{url}': {e}")))?;
    Ok((store, path))
}

/// Map the DuckDB `s3` secret matching `url`'s scope onto object_store S3 config
/// keys. Selecting by scope+type means a call spanning several buckets uses the
/// right secret per URL. Returns empty when no `s3` secret matches.
fn s3_options(secrets: &Secrets, url: &Url) -> Vec<(String, String)> {
    let mut opts: Vec<(String, String)> = Vec::new();
    let Some(fields) = secrets.for_scope_of_type(url.as_str(), "s3") else {
        return opts;
    };
    let nonempty = |f: &str| fields.get(f).filter(|v| !v.is_empty()).cloned();
    let use_ssl = fields.get("use_ssl").and_then(|v| parse_bool(v));

    if let Some(v) = nonempty("key_id") {
        opts.push(("aws_access_key_id".into(), v));
    }
    if let Some(v) = nonempty("secret") {
        opts.push(("aws_secret_access_key".into(), v));
    }
    if let Some(v) = nonempty("session_token") {
        opts.push(("aws_session_token".into(), v));
    }
    if let Some(v) = nonempty("region") {
        opts.push(("aws_region".into(), v));
    }
    if let Some(v) = nonempty("endpoint") {
        opts.push(("aws_endpoint".into(), normalize_endpoint(&v, use_ssl)));
    }
    if let Some(v) = nonempty("url_style") {
        if v.eq_ignore_ascii_case("path") {
            opts.push(("aws_virtual_hosted_style_request".into(), "false".into()));
        }
    }
    if use_ssl == Some(false) {
        opts.push(("aws_allow_http".into(), "true".into()));
    }
    opts
}

/// Read an entire remote object into memory (matches the eager local
/// `std::fs::read`; whole-file is required for RDW framing anyway).
pub fn read_object(url: &Url, secrets: &Secrets, overrides: &[(String, String)]) -> Result<Vec<u8>> {
    let (store, path) = build_store(url, secrets, overrides)?;
    let bytes = block_on(async move {
        let r = store.get(&path).await?;
        r.bytes().await
    })
    .map_err(|e| ve(format!("read {url}: {e}")))?;
    Ok(bytes.to_vec())
}

/// Write a whole object to a remote store. `http(s)://` is read-only.
pub fn write_object(
    url: &Url,
    secrets: &Secrets,
    overrides: &[(String, String)],
    body: &[u8],
) -> Result<()> {
    if matches!(url.scheme(), "http" | "https") {
        return Err(ve(format!(
            "writing to '{}://' is not supported; use s3://",
            url.scheme()
        )));
    }
    let (store, path) = build_store(url, secrets, overrides)?;
    let payload = PutPayload::from(body.to_vec());
    block_on(async move { store.put(&path, payload).await }).map_err(|e| ve(format!("write {url}: {e}")))?;
    Ok(())
}

/// Does `key` match the glob `pattern` under DuckDB's S3 semantics? `*`, `?`,
/// `[...]` stay within one key segment; only `**` crosses `/`.
fn glob_matches(pattern: &glob::Pattern, key: &str) -> bool {
    pattern.matches_with(
        key,
        glob::MatchOptions {
            require_literal_separator: true,
            ..Default::default()
        },
    )
}

/// The list prefix for a glob key: everything up to and including the last `/`
/// before the first wildcard. Empty when the wildcard is in the first segment.
fn glob_prefix(key: &str) -> &str {
    match key.find(['*', '?', '[']) {
        Some(i) => match key[..i].rfind('/') {
            Some(slash) => &key[..=slash],
            None => "",
        },
        None => key,
    }
}

/// Expand a remote glob URL into the matching object URLs (sorted). For
/// `http(s)://` there is no listing, so the URL is returned as-is.
pub fn list_glob(url: &Url, secrets: &Secrets, overrides: &[(String, String)]) -> Result<Vec<Url>> {
    if matches!(url.scheme(), "http" | "https") {
        return Ok(vec![url.clone()]);
    }
    // Work in literal-key space: the pattern (with glob chars) and the listed
    // object keys are both decoded, so matching and URL rebuilding are exact.
    let key = remote_key(url);
    let pattern = glob::Pattern::new(&key).map_err(|e| ve(format!("bad glob '{url}': {e}")))?;
    let prefix = glob_prefix(&key).to_string();
    let (store, _) = build_store(url, secrets, overrides)?;
    let scheme = url.scheme().to_string();
    let bucket = url.host_str().unwrap_or_default().to_string();

    use futures::StreamExt;
    let prefix_path = (!prefix.is_empty()).then(|| ObjPath::from(prefix));
    let metas = block_on(async move {
        let mut stream = store.list(prefix_path.as_ref());
        let mut out = Vec::new();
        while let Some(meta) = stream.next().await {
            out.push(meta?);
        }
        Ok::<_, object_store::Error>(out)
    })
    .map_err(|e| ve(format!("list {url}: {e}")))?;

    let mut urls: Vec<Url> = metas
        .into_iter()
        .filter_map(|m| {
            // object_store keys come back percent-encoded; decode to the literal
            // key for matching and re-encode through encode_s3_url for the result.
            let literal = percent_decode_str(m.location.as_ref())
                .decode_utf8_lossy()
                .into_owned();
            glob_matches(&pattern, &literal)
                .then(|| Url::parse(&encode_s3_url(&scheme, &format!("{bucket}/{literal}"))))
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| ve(format!("rebuild s3 url under '{url}': {e}")))?;
    urls.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_locals_and_remotes() {
        assert!(matches!(classify("data/x.dat").unwrap(), Location::Local(_)));
        assert!(matches!(classify("/abs/x.dat").unwrap(), Location::Local(_)));
        assert!(matches!(classify("./rel*.dat").unwrap(), Location::Local(_)));
        assert!(matches!(
            classify("s3://bucket/x.dat").unwrap(),
            Location::Remote(_)
        ));
        assert!(matches!(
            classify("HTTPS://host/x.dat").unwrap(),
            Location::Remote(_)
        ));
        // Unknown scheme is an error, not a local path.
        assert!(classify("gs://bucket/x.dat").is_err());
        assert!(classify("az://c/x.dat").is_err());
    }

    #[test]
    fn is_remote_quick_check() {
        assert!(is_remote("s3://b/k"));
        assert!(is_remote("http://h/k"));
        assert!(!is_remote("data/x.dat"));
        assert!(!is_remote("gs://b/k")); // not a *supported* remote scheme
    }

    #[test]
    fn secret_lookup_requests_s3_for_s3_paths() {
        // An s3:// path requests a `s3` secret scoped to the URL.
        let l = secret_lookup("s3://bucket/data/file.dat").expect("s3 path requests a secret");
        assert_eq!(l.secret_type, "s3");
        assert_eq!(l.scope.as_deref(), Some("s3://bucket/data/file.dat"));
        assert!(l.name.is_none());
        assert_eq!(secret_lookup("s3a://b/k").unwrap().secret_type, "s3");
        // A glob s3 path still requests it (DuckDB prefix-matches the scope).
        let g = secret_lookup("s3://bucket/data/*.dat").expect("glob s3 path requests a secret");
        assert_eq!(g.scope.as_deref(), Some("s3://bucket/data/*.dat"));
        // http(s):// and local paths need no secret.
        assert!(secret_lookup("https://host/f.dat").is_none());
        assert!(secret_lookup("http://host/f.dat").is_none());
        assert!(secret_lookup("data/f.dat").is_none());
        assert!(secret_lookup("/abs/f.dat").is_none());
    }

    #[test]
    fn secret_lookups_dedup_per_scope() {
        // A list of paths across buckets → one lookup per distinct scope; same
        // scope repeated dedups; http/local contribute none.
        let paths: Vec<String> = [
            "s3://bucket-a/x.dat",
            "s3://bucket-a/x.dat", // dup scope
            "s3://bucket-b/y.dat",
            "https://host/z.dat", // no secret
            "local.dat",          // no secret
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let looks = secret_lookups(&paths);
        assert_eq!(looks.len(), 2);
        assert!(looks.iter().all(|l| l.secret_type == "s3"));
        let scopes: Vec<&str> = looks.iter().filter_map(|l| l.scope.as_deref()).collect();
        assert!(scopes.contains(&"s3://bucket-a/x.dat"));
        assert!(scopes.contains(&"s3://bucket-b/y.dat"));
    }

    /// Build a `Secrets` from (name, fields) entries.
    fn make_secrets(entries: &[(&str, &[(&str, &str)])]) -> Secrets {
        let by_name = entries
            .iter()
            .map(|(name, fields)| {
                (
                    name.to_string(),
                    fields
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                )
            })
            .collect();
        Secrets { by_name }
    }

    #[test]
    fn s3_options_select_secret_per_bucket() {
        // Two s3 secrets, one per bucket — each URL must get its own credentials.
        let secrets = make_secrets(&[
            (
                "sec_a",
                &[("type", "s3"), ("key_id", "AAA"), ("secret", "sa"), ("scope", "s3://bucket-a")],
            ),
            (
                "sec_b",
                &[("type", "s3"), ("key_id", "BBB"), ("secret", "sb"), ("scope", "s3://bucket-b")],
            ),
        ]);
        let opts_a = s3_options(&secrets, &Url::parse("s3://bucket-a/data/x.dat").unwrap());
        let opts_b = s3_options(&secrets, &Url::parse("s3://bucket-b/data/y.dat").unwrap());
        let key = |o: &[(String, String)]| {
            o.iter()
                .find(|(k, _)| k == "aws_access_key_id")
                .map(|(_, v)| v.clone())
        };
        assert_eq!(key(&opts_a).as_deref(), Some("AAA"));
        assert_eq!(key(&opts_b).as_deref(), Some("BBB"));
    }

    #[test]
    fn secret_type_by_scheme() {
        let s3 = Url::parse("s3://b/k").unwrap();
        let http = Url::parse("https://h/k").unwrap();
        assert_eq!(secret_type_for(&s3), Some("s3"));
        assert_eq!(secret_type_for(&http), None);
    }

    /// Build a one-secret `Secrets` from (field, value) pairs.
    fn secrets(name: &str, fields: &[(&str, &str)]) -> Secrets {
        let mut by_name = std::collections::HashMap::new();
        by_name.insert(
            name.to_string(),
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        Secrets { by_name }
    }

    /// Map a secret through build_store and read back the options it produced by
    /// re-running the same mapping (build_store is the source of truth, so we
    /// assert on a store being constructed plus the option derivation helpers).
    #[test]
    fn s3_secret_maps_to_store() {
        let url = Url::parse("s3://bucket/out.dat").unwrap();
        let sec = secrets(
            "s3",
            &[
                ("key_id", "AKIA"),
                ("secret", "shh"),
                ("region", "us-east-1"),
                ("endpoint", "minio:9000"),
                ("url_style", "path"),
                ("use_ssl", "false"),
            ],
        );
        // parse_url_opts must accept the derived options and yield a store.
        let (_store, path) = build_store(&url, &sec, &[]).expect("store builds");
        assert_eq!(path.as_ref(), "out.dat");
    }

    #[test]
    fn endpoint_scheme_is_inferred_from_use_ssl() {
        assert_eq!(normalize_endpoint("minio:9000", Some(false)), "http://minio:9000");
        assert_eq!(normalize_endpoint("minio:9000", Some(true)), "https://minio:9000");
        assert_eq!(normalize_endpoint("minio:9000", None), "https://minio:9000");
        assert_eq!(
            normalize_endpoint("http://already:9000", Some(true)),
            "http://already:9000"
        );
    }

    #[test]
    fn s3_key_preserves_glob_metachars_through_url() {
        // The `?` wildcard must survive Url parsing (it's a URL query delimiter).
        let Location::Remote(u) = classify("s3://bucket/data/f?.dat").unwrap() else {
            panic!("expected remote");
        };
        assert!(u.query().is_none(), "the `?` must not become a query string");
        assert_eq!(remote_key(&u), "data/f?.dat");
        // `*` and `[...]` survive too, and the host is the bucket.
        let Location::Remote(u2) = classify("s3://bucket/d/f[0-9]*.dat").unwrap() else {
            panic!("expected remote");
        };
        assert_eq!(u2.host_str(), Some("bucket"));
        assert_eq!(remote_key(&u2), "d/f[0-9]*.dat");
        // A literal `%` in a key round-trips.
        let Location::Remote(u3) = classify("s3://b/a%2Fb.dat").unwrap() else {
            panic!("expected remote");
        };
        assert_eq!(remote_key(&u3), "a%2Fb.dat");
    }

    #[test]
    fn glob_prefix_splits_at_last_slash_before_wildcard() {
        assert_eq!(glob_prefix("data/acct*.dat"), "data/");
        assert_eq!(glob_prefix("a/b/c/*.dat"), "a/b/c/");
        assert_eq!(glob_prefix("*.dat"), "");
        assert_eq!(glob_prefix("plain/key.dat"), "plain/key.dat");
        assert_eq!(glob_prefix("p/q[0-9].dat"), "p/");
    }

    #[test]
    fn glob_matches_duckdb_s3_semantics() {
        let p = |s: &str| glob::Pattern::new(s).unwrap();
        // `*` stays within one segment (does NOT cross `/`), like DuckDB.
        assert!(glob_matches(&p("data/*.dat"), "data/a.dat"));
        assert!(!glob_matches(&p("data/*.dat"), "data/sub/a.dat"));
        // `**` crosses `/` (recursive), matching zero or more segments.
        assert!(glob_matches(&p("data/**/*.dat"), "data/a.dat"));
        assert!(glob_matches(&p("data/**/*.dat"), "data/sub/deep/a.dat"));
        // `?` is a single char within a segment; `[...]` char classes work.
        assert!(glob_matches(&p("acct?.dat"), "acct1.dat"));
        assert!(!glob_matches(&p("acct?.dat"), "acct12.dat"));
        assert!(glob_matches(&p("f[0-9].dat"), "f7.dat"));
        // A `*` in a middle segment matches exactly one segment.
        assert!(glob_matches(&p("y=*/m=*/f.dat"), "y=2024/m=06/f.dat"));
        assert!(!glob_matches(&p("y=*/f.dat"), "y=2024/m=06/f.dat"));
    }

    #[test]
    fn parse_bool_forms() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("FALSE"), Some(false));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }
}
