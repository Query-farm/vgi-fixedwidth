# CLAUDE.md

Guidance for working in this repository.

## What this is

`vgi-fixedformat` is a **VGI worker** (a standalone binary DuckDB launches and
talks to over Apache Arrow IPC, `ATTACH 'fixed' (TYPE vgi, LOCATION '…')`) that
brings Perl-`unpack` / Python-`struct` / COBOL-copybook fixed-width parsing and
formatting to SQL. Functions live under catalog `fixed`, schema `main`.

Built on the published VGI Rust SDK (`vgi = "0.9.2"` from crates.io), arrow 59.
Modeled on `../vgi-miint`. The repo builds standalone — no local SDK checkout
needed.

## SQL surface

- `fixed.main.unpack_fixed(rec, spec [, encoding])` — parse a VARCHAR/BLOB record
  into a STRUCT (`unpack` is a reserved DuckDB keyword, hence the `_fixed`
  suffix). Scalar functions only take **positional** args in DuckDB, so `encoding`
  is a positional const (registered as a 2-arg + 3-arg arity overload), not named;
  spec format is auto-detected on the scalar path.
- `fixed.main.pack_fixed(struct, spec [, encoding])` — format a STRUCT back into a
  BLOB. `pack_fixed(unpack_fixed(rec, s), s) == rec`.
- `fixed.main.read_fixed(path, spec [, format =>, encoding =>, framing =>, record_length =>])`
  — scan a fixed-width file (table function; `path` may glob).
- `fixed.main.write_fixed((FROM rel), path, spec [, format =>, encoding =>, framing =>])`
  — write a relation to a fixed-width file (table-buffering sink); returns
  `(rows_written, bytes_written)`.
- `fixed.main.fixedformat_version()`.

### Cloud paths (S3-compatible + HTTP)

The worker runs outside DuckDB (no `httpfs`), so cloud access goes through
`object_store` in `crates/fixedformat-worker/src/cloud.rs`. A `path` may be:

- `s3://bucket/key` — AWS S3, and R2 / MinIO / GCS-HMAC via a `TYPE s3` secret
  with `ENDPOINT`/`URL_STYLE`. Globs (`s3://bucket/data/*.dat`) expand via object
  listing (list under the literal prefix, then glob-filter). Glob semantics match
  DuckDB's S3 globbing: `*`/`?`/`[...]` stay within one key segment and only `**`
  crosses `/` (`cloud::glob_matches` uses `require_literal_separator`); like
  DuckDB, brace `{a,b}` expansion is not supported. Both `read_fixed` and
  `write_fixed` support `s3://`.
- `https://host/file` / `http://…` — **read only** (no listing/glob; the URL is
  a single object). Writing to `http(s)://` errors.
- anything without a scheme → local filesystem (unchanged). An unknown scheme
  (`gs://`, `az://`) is a hard error, not a silent local fallback.

Credentials come from DuckDB's secret manager via the VGI two-phase secret bind:
both functions declare `secret_lookups()` requesting a `s3` secret **scoped to
the URL**, and the resolved fields (`key_id`, `secret`, `session_token`,
`region`, `endpoint`, `url_style`, `use_ssl`) are mapped onto `object_store`
`aws_*` options. `write_fixed` getting secrets relies on the buffering
two-phase-bind fix in the local `vgi` SDK (see the path dep in the root
`Cargo.toml`; **repin to a published vgi before release**). Named overrides
`endpoint =>` / `region =>` / `url_style =>` / `use_ssl =>` (declared via
`options::cloud_arg_specs`) win over secret-derived config — handy for MinIO
without a `CREATE SECRET`. Example:

```sql
CREATE SECRET (TYPE s3, KEY_ID '…', SECRET '…', REGION 'us-east-1');
SELECT * FROM fixed.main.read_fixed('s3://bucket/data/*.dat', 'A10 N');
-- MinIO without a secret:
SELECT * FROM fixed.main.read_fixed('s3://bucket/x.dat', 'A10 N',
    endpoint => 'localhost:9000', url_style => 'path', use_ssl => false);
```

### Spec formats (auto-detected; override with `format =>`)

- **template** — Perl/Python codes: `A`/`a`/`Z` strings, `c C s S l L i I q Q`
  ints, `n N v V` BE/LE aliases, `e f d` floats, `H h` hex, `?` bool, `x` pad,
  `< > ! = @` byte-order, plus display PIC tokens `9(5)` / `S9(7)V99` / `X(10)`.
  Count is a width for string/hex/pad codes, a repeat (→ LIST) for numerics.
- **json** — `[{"name","type","width"|"digits","scale","signed","endian","occurs",
  "justify","pad","sign"}, ...]` (or `{"fields":[...]}`).
- **copybook** — COBOL: nested groups (→ STRUCT), `PIC X/A/9/S/V`, `USAGE
  COMP-3`/`COMP`/`BINARY`, `OCCURS n` (→ LIST), `REDEFINES` (→ folded STRUCT),
  `SIGN LEADING/TRAILING [SEPARATE]`.

Types: decimals (COMP-3/zoned/implied-point) → `DECIMAL(p,s)`, ints → `BIGINT`,
floats → `REAL`/`DOUBLE`, text/hex → `VARCHAR`, `?` → `BOOLEAN`. Encodings:
`ascii` (default) / `ebcdic` (CP037). Framing: `newline` (default) / `fixed` /
`rdw` / `rdw_blocked`.

## Layout

- `crates/fixedformat-core` — pure codecs, **no Arrow/VGI deps** (`unsafe`
  forbidden). The Layout IR (`layout.rs`) + three parsers (`template`, `jsonspec`,
  `copybook`) + decode/encode + `packed` (COMP-3) / `zoned` / `ebcdic` (CP037
  tables) / `framing`. All correctness lives here, unit-tested directly.
- `crates/fixedformat-worker` — thin Arrow/VGI adapter: `arrow_map.rs` (Layout →
  Arrow fields, Value → arrays incl. Decimal128/List/Struct), `value_in.rs` (Arrow
  → Value for pack/write), `options.rs`, and `scalar/`, `table/`, `buffering/`.
  `main.rs` registers everything and calls `Worker::run()`.
- `test/sql/*.test` — sqllogictest e2e (run via haybarn unittest). `data/` holds
  fixtures.

## Build & test

```sh
cargo test -p fixedformat-core   # 63 unit tests + proptest round-trip fuzzing (tests/roundtrip.rs)
cargo clippy --all-targets       # keep clean
cargo build --release            # build the worker
./run_tests.sh                   # end-to-end SQLLogic suite (9 files, see below)
./run_tests.sh test/sql/types.test   # single file (Catch2 filter; trailing * only)
```

Test fixtures under `data/` are regenerated deterministically by
`python3 data/generate_fixtures.py` (includes malformed fixtures for
`malformed.test`). `read_fixed` decodes eagerly and fails fast on any malformed
record (no partial rows).

End-to-end tests need the haybarn tooling (one-time):
```sh
uv tool install haybarn-unittest
uv tool install haybarn
echo "INSTALL vgi FROM community;" | uvx haybarn-cli
```
`run_tests.sh` builds the worker and runs `haybarn-unittest --test-dir "$PWD"
"test/sql/*"` with `VGI_TEST_WORKER` pointed at the binary.

## Conventions / gotchas

- All algorithms go in `fixedformat-core` with unit tests; the worker is a thin
  adapter. Binary codecs are cross-checked against Python `struct.pack` bytes.
- Logs go to **stderr** — stdout is the Arrow-IPC channel.
- The catalog name must match the ATTACH name; `main.rs` defaults
  `VGI_WORKER_CATALOG_NAME` to `fixed`.
- The spec is a **bind-time constant** so `unpack_fixed`'s STRUCT output type can
  be resolved in `on_bind`.
- `unpack` is a reserved keyword in DuckDB — the function is `unpack_fixed`.
- Scalar functions only support **positional** args (named `-1` specs are
  silently dropped by the binder). Table functions DO support named args, so
  `read_fixed`/`write_fixed` take `format =>`/`encoding =>`/`framing =>` named,
  while `unpack_fixed`/`pack_fixed` take a positional `encoding` via arity
  overloads.
- Pure-scalar tests use `SET search_path='fixed.main'`. Tests that `CREATE TABLE`
  in the default catalog must NOT set search_path (it would target the read-only
  worker catalog) — fully-qualify worker calls as `fixed.main.<fn>(...)` instead.
- Table-valued input (`write_fixed`) is passed as a subquery: `write_fixed((FROM
  tbl), …)`.
- REDEFINES folds the base + redefiners into a STRUCT named after the base
  (`raw STRUCT(raw, num)`), each child reinterpreting the same bytes.
