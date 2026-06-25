//! The `fixedformat` VGI worker.
//!
//! A standalone binary DuckDB launches and talks to over Apache Arrow IPC. It
//! brings Perl-`unpack` / Python-`struct` / COBOL-copybook fixed-width parsing
//! and formatting to SQL under the catalog `fixed`, schema `main`:
//!
//! - `fixed.main.unpack_fixed(rec, spec)` — parse a string/blob into a STRUCT
//! - `fixed.main.pack_fixed(struct, spec)` — format a STRUCT back into a BLOB
//! - `fixed.main.read_fixed(path, spec, ...)` — scan a fixed-width file
//! - `fixed.main.write_fixed((FROM rel), path, spec, ...)` — write one out

mod arrow_map;
mod buffering;
mod meta;
mod options;
mod scalar;
mod table;
mod value_in;

use vgi::catalog::{CatSchema, CatalogModel};
use vgi::Worker;

/// Catalog + schema metadata (description, provenance) surfaced to DuckDB and
/// the `vgi-lint` metadata-quality linter. The function objects themselves are
/// served from the registered scalars / table / buffering functions; this only
/// adds catalog/schema-level comments and tags.
fn catalog_metadata(name: &str) -> CatalogModel {
    CatalogModel {
        name: name.to_string(),
        comment: Some(
            "Fixed-width / Perl-unpack / Python-struct / COBOL-copybook record parsing and \
             formatting for SQL."
                .to_string(),
        ),
        tags: vec![
            (
                "vgi.title".to_string(),
                "Fixed-Width & COBOL Copybook Codec".to_string(),
            ),
            (
                "vgi.keywords".to_string(),
                crate::meta::keywords_json(
                    "fixed-width, fixed format, unpack, pack, struct, perl unpack, python struct, \
                     COBOL, copybook, mainframe, EBCDIC, COMP-3, packed decimal, zoned decimal, \
                     RDW, flat file, record layout, parse, encode",
                ),
            ),
            (
                "vgi.doc_llm".to_string(),
                "Parse and format fixed-width / flat-file records directly in SQL. Decode a \
                 record string or blob into a typed STRUCT with `unpack_fixed`, re-encode a STRUCT \
                 back to record bytes with `pack_fixed`, scan a fixed-width file into rows with \
                 `read_fixed`, and write a relation out to a fixed-width file with `write_fixed`. \
                 Layouts are given as Perl/Python `unpack` template strings, JSON field specs, or \
                 COBOL copybooks, and support ASCII or EBCDIC (CP037) encoding, packed/zoned \
                 decimals (COMP-3), OCCURS lists, nested groups, REDEFINES, and newline / fixed / \
                 RDW record framing. Use it to ingest or emit mainframe and legacy flat-file data."
                    .to_string(),
            ),
            (
                "vgi.doc_md".to_string(),
                "# fixed\n\nFixed-width / flat-file record parsing and formatting over Apache \
                 Arrow. Brings Perl-`unpack`, Python-`struct`, and COBOL-copybook style layouts to \
                 SQL so you can ingest and emit mainframe and legacy flat-file data without an \
                 external ETL step.\n\nA layout spec is given in one of three auto-detected \
                 formats — a Perl/Python `unpack` **template** string (e.g. `A10 N s>`), a **JSON** \
                 field list, or a COBOL **copybook** — and maps each field to a typed column \
                 (BIGINT / REAL / DOUBLE / VARCHAR / BOOLEAN, `DECIMAL(p,s)` for COMP-3 / zoned / \
                 implied-point numbers, LIST for `OCCURS`, STRUCT for groups and REDEFINES). \
                 Encodings are `ascii` (default) or `ebcdic` (CP037); record framing is `newline` \
                 (default), `fixed`, `rdw`, or `rdw_blocked`.\n\n**Scalars:** `unpack_fixed` \
                 (record → STRUCT), `pack_fixed` (STRUCT → record bytes, the inverse), and \
                 `fixedformat_version`.\n\n**Table functions:** `read_fixed` (scan a fixed-width \
                 file, path may glob) and `write_fixed` (write a relation out to a fixed-width \
                 file)."
                    .to_string(),
            ),
            ("vgi.author".to_string(), "Query.Farm".to_string()),
            (
                "vgi.copyright".to_string(),
                "Copyright 2026 Query Farm LLC - https://query.farm".to_string(),
            ),
            ("vgi.license".to_string(), "MIT".to_string()),
            (
                "vgi.support_contact".to_string(),
                "https://github.com/Query-farm/vgi-fixedwidth/issues".to_string(),
            ),
            (
                "vgi.support_policy_url".to_string(),
                "https://github.com/Query-farm/vgi-fixedwidth/blob/main/README.md".to_string(),
            ),
        ],
        source_url: Some("https://github.com/Query-farm/vgi-fixedwidth".to_string()),
        schemas: vec![CatSchema {
            name: "main".to_string(),
            comment: Some(
                "Fixed-width / copybook parsing, formatting, reading, and writing functions."
                    .to_string(),
            ),
            tags: vec![
                ("vgi.title".to_string(), "Fixed Format — main".to_string()),
                (
                    "vgi.keywords".to_string(),
                    crate::meta::keywords_json(
                        "fixed-width, unpack_fixed, pack_fixed, read_fixed, write_fixed, copybook, \
                         template, struct, EBCDIC, COMP-3, mainframe, flat file",
                    ),
                ),
                // VGI123 classifying tags (bare keys: domain/category/topic) for faceting.
                ("domain".to_string(), "data-engineering".to_string()),
                ("category".to_string(), "parsing-and-serialization".to_string()),
                ("topic".to_string(), "fixed-width-records".to_string()),
                (
                    "vgi.doc_llm".to_string(),
                    "Functions for fixed-width / flat-file records: parse a record into a STRUCT \
                     (`unpack_fixed`), format a STRUCT back into record bytes (`pack_fixed`), scan \
                     a fixed-width file into rows (`read_fixed`), and write a relation to a \
                     fixed-width file (`write_fixed`). Layouts are template strings, JSON specs, \
                     or COBOL copybooks; encodings ASCII or EBCDIC."
                        .to_string(),
                ),
                (
                    "vgi.doc_md".to_string(),
                    "The single schema for the `fixed` worker. It holds the fixed-width record \
                     functions — the `unpack_fixed` / `pack_fixed` scalar inverse pair and the \
                     `fixedformat_version` scalar — plus the `read_fixed` and `write_fixed` table \
                     functions for scanning and emitting fixed-width files. Layouts are given as \
                     Perl/Python `unpack` templates, JSON field specs, or COBOL copybooks."
                        .to_string(),
                ),
                // VGI506 representative example queries for the schema.
                (
                    "vgi.example_queries".to_string(),
                    "SELECT fixed.main.unpack_fixed('JohnDoe  00042', 'A8 N');\n\
                     SELECT fixed.main.pack_fixed({'name': 'Jo', 'id': 7}, 'A2 N');\n\
                     SELECT fixed.main.fixedformat_version();\n\
                     SELECT * FROM fixed.main.read_fixed('data/*.dat', 'A10 N');\n\
                     SELECT * FROM fixed.main.write_fixed((FROM tbl), '/tmp/out.dat', 'A10 N');"
                        .to_string(),
                ),
            ],
            views: Vec::new(),
            macros: Vec::new(),
            tables: Vec::new(),
        }],
        ..Default::default()
    }
}

fn main() {
    // Logs MUST go to stderr — stdout is the Arrow-IPC channel.
    let _ = env_logger::Builder::from_env(env_logger::Env::default().filter_or("VGI_LOG", "info"))
        .format_timestamp_millis()
        .try_init();

    // The catalog name DuckDB sees in `ATTACH 'fixed' (TYPE vgi, …)`. Default to
    // `fixed`, but honor an explicit override so a test harness can rename it.
    if std::env::var_os("VGI_WORKER_CATALOG_NAME").is_none() {
        std::env::set_var("VGI_WORKER_CATALOG_NAME", "fixed");
    }
    let catalog_name =
        std::env::var("VGI_WORKER_CATALOG_NAME").unwrap_or_else(|_| "fixed".to_string());

    let mut worker = Worker::new();
    scalar::register(&mut worker);
    table::register(&mut worker);
    buffering::register(&mut worker);
    worker.set_catalog(catalog_metadata(&catalog_name));
    worker.run();
}
