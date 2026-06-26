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
mod cloud;
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
                 `read_fixed`, write a relation out to a fixed-width file with `write_fixed`, and \
                 report the worker version with `fixedformat_version`. Layouts are given as \
                 Perl/Python `unpack` template strings, JSON field specs, or COBOL copybooks, and \
                 support ASCII or EBCDIC (CP037) encoding, packed/zoned decimals (COMP-3), OCCURS \
                 lists, nested groups, REDEFINES, and four record-framing modes: newline, fixed, \
                 rdw, and rdw_blocked. The scalar pair `unpack_fixed`/`pack_fixed` round-trips: \
                 `pack_fixed(unpack_fixed(rec, s), s) == rec`. Zero-config defaults are newline \
                 framing and ascii encoding, so the common case is just `(record, spec)`. The spec \
                 format (template / JSON / copybook) is auto-detected; on the table functions you \
                 can force it with `format =>` if a layout is ambiguous. Use it to ingest or emit \
                 mainframe and legacy flat-file data."
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
                 (default), `fixed`, `rdw`, or `rdw_blocked`. The spec format is auto-detected from \
                 the spec text; on the table functions you can force it with `format =>` \
                 ('template' / 'json' / 'copybook') when a layout would otherwise be ambiguous. \
                 With the defaults (newline framing, ascii encoding) the common call is just \
                 `(record, spec)`.\n\n**Scalars:** `unpack_fixed` \
                 (record → STRUCT), `pack_fixed` (STRUCT → record bytes, the inverse), and \
                 `fixedformat_version`.\n\n**Table functions:** `read_fixed` (scan a fixed-width \
                 file, path may glob) and `write_fixed` (write a relation out to a fixed-width \
                 file)."
                    .to_string(),
            ),
            // Fixed agent-suitability suite run by `vgi-lint simulate` (2 single-call
            // smoke tests + 2 multi-concept tasks: a multi-file glob aggregate and the
            // headline EBCDIC/COMP-3 mainframe decode). Each prompt is shown to the
            // simulated analyst; the hidden reference_sql is the canonical solution,
            // re-run live to grade by deterministic result comparison. Prompts name
            // their output columns (grading is strict on names/values/order). The
            // file-based tasks use repo-relative `data/...` paths, so run `vgi-lint
            // simulate` from the repo root where the test fixtures live.
            (
                "vgi.agent_test_tasks".to_string(),
                crate::meta::agent_test_tasks_json(&[
                    (
                        "unpack_single",
                        "I have one fixed-width record 'JOHN      00042' where the first 10 \
                         characters are an account name and the next 5 are a zero-padded quantity. \
                         Parse it and return the quantity as a single integer column named qty.",
                        "SELECT (fixed.main.unpack_fixed('JOHN      00042', 'name:A10 qty:9(5)'))\
                         .qty AS qty",
                    ),
                    (
                        "profile_large",
                        "The file data/large.dat is a newline-delimited fixed-width feed where each \
                         record is a single 7-digit zero-padded id. Profile it: return one row with \
                         a column named records (the record count), a column named min_id, and a \
                         column named max_id.",
                        "SELECT count(*) AS records, min(id) AS min_id, max(id) AS max_id \
                         FROM fixed.main.read_fixed('data/large.dat', 'id:9(7)')",
                    ),
                    (
                        "glob_total",
                        "A nightly job drops one or more fixed-width account files into data/, \
                         named acct1.dat, acct2.dat, and so on. Each record is a 10-character \
                         account name followed by a 5-digit zero-padded quantity. Read all of the \
                         acct*.dat files at once and return the total quantity across every record \
                         as a single column named total_qty.",
                        "SELECT sum(qty) AS total_qty \
                         FROM fixed.main.read_fixed('data/acct*.dat', 'name:A10 qty:9(5)')",
                    ),
                    (
                        "ebcdic_comp3_max",
                        "We received a mainframe extract at data/ebcdic_comp3.dat. It is \
                         EBCDIC-encoded and fixed-length (no line delimiters between records). \
                         Each record is a 5-byte name (PIC X(5)) followed by a signed \
                         packed-decimal amount PIC S9(3)V99 COMP-3. Decode the file and return the \
                         largest amount as a single column named max_amount.",
                        "SELECT max(AMT) AS max_amount FROM fixed.main.read_fixed(\
                         'data/ebcdic_comp3.dat', \
                         '01 R. 05 NM PIC X(5). 05 AMT PIC S9(3)V99 COMP-3.', \
                         encoding => 'ebcdic', framing => 'fixed')",
                    ),
                    (
                        "pack_ascii_record",
                        "We need to emit a single fixed-width record for an export. The layout is a \
                         10-character left-justified, space-padded account name followed by a \
                         5-digit zero-padded quantity. Build the record for account name 'ALICE' \
                         with quantity 5 and return the resulting record text as a single column \
                         named record.",
                        "SELECT fixed.main.pack_fixed({'name': 'ALICE', 'qty': 5}, \
                         'name:A10 qty:9(5)')::VARCHAR AS record",
                    ),
                    (
                        "pack_ebcdic_comp3_hex",
                        "A mainframe ingest job needs an account record encoded the way the host \
                         expects it: the name as a 5-byte EBCDIC field (PIC X(5)) followed by a \
                         signed packed-decimal amount PIC S9(3)V99 COMP-3. Encode name 'ACME' with \
                         amount 123.45 and return the resulting record bytes as an uppercase hex \
                         string in a single column named record_hex.",
                        "SELECT hex(fixed.main.pack_fixed({'NM': 'ACME', 'AMT': 123.45}, \
                         '01 R. 05 NM PIC X(5). 05 AMT PIC S9(3)V99 COMP-3.', 'ebcdic')) \
                         AS record_hex",
                    ),
                    (
                        "write_accounts_file",
                        "Export two accounts to a newline-delimited fixed-width file at \
                         data/_agent_write.dat, where each record is a 10-character left-justified \
                         name followed by a 5-digit zero-padded quantity: ALICE with quantity 5 and \
                         BOB with quantity 999. Return the write summary with a column named \
                         rows_written and a column named bytes_written.",
                        "SELECT rows_written, bytes_written FROM fixed.main.write_fixed(\
                         (FROM (VALUES ('ALICE', 5), ('BOB', 999)) AS v(name, qty)), \
                         'data/_agent_write.dat', 'name:A10 qty:9(5)')",
                    ),
                    (
                        "worker_version",
                        "Before relying on the fixed-format worker in a pipeline, an analyst wants \
                         to record which build is attached. Return the worker's version string as \
                         a single row with one column named version.",
                        "SELECT fixed.main.fixedformat_version() AS version",
                    ),
                ]),
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
                     a fixed-width file into rows (`read_fixed`), write a relation to a \
                     fixed-width file (`write_fixed`), and report the worker version \
                     (`fixedformat_version`). Returned shapes: `unpack_fixed` → STRUCT, \
                     `pack_fixed` → BLOB, `write_fixed` → (rows_written, bytes_written), \
                     `read_fixed` → a dynamic column set driven by the spec. Layouts are template \
                     strings, JSON specs, or COBOL copybooks (auto-detected; force with \
                     `format =>` on the table functions). Field kinds map to columns as text/hex → \
                     VARCHAR, integers → BIGINT, COMP-3/zoned/implied-point → DECIMAL(p,s), OCCURS \
                     → LIST, group/REDEFINES → STRUCT. Encodings are ascii (default) or ebcdic \
                     (CP037); framing is newline (default), fixed, rdw, or rdw_blocked. With the \
                     defaults the common call is just `(record, spec)`."
                        .to_string(),
                ),
                (
                    "vgi.doc_md".to_string(),
                    "The single (and only) schema for the `fixed` worker — the catalog name \
                     matches the `ATTACH` name, so qualify calls as `fixed.main.<fn>(...)`. It \
                     holds the fixed-width record functions: the `unpack_fixed` (→ STRUCT) / \
                     `pack_fixed` (→ BLOB) scalar inverse pair and the `fixedformat_version` \
                     scalar, plus the `read_fixed` (→ dynamic columns) and `write_fixed` (→ \
                     `(rows_written, bytes_written)`) table functions for scanning and emitting \
                     fixed-width files. Layouts are given as Perl/Python `unpack` templates, JSON \
                     field specs, or COBOL copybooks (auto-detected; override with `format =>` on \
                     the table functions). Encodings are ascii (default) or ebcdic (CP037); record \
                     framing is newline (default), fixed, rdw, or rdw_blocked. Field kinds map to \
                     columns as text/hex → VARCHAR, integers → BIGINT, COMP-3/zoned/implied-point \
                     → DECIMAL(p,s), OCCURS → LIST, group/REDEFINES → STRUCT."
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
