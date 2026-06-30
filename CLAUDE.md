# CLAUDE.md

Guidance for working in this repository.

## What this is

`vgi-fixedformat` is a **VGI worker** (a standalone binary DuckDB launches and
talks to over Apache Arrow IPC, `ATTACH 'fixed' (TYPE vgi, LOCATION '…')`) that
brings Perl-`unpack` / Python-`struct` / COBOL-copybook fixed-width parsing and
formatting to SQL. Functions live under catalog `fixed`, schema `main`.

Built on the published VGI Rust SDK (`vgi = "0.9.5"` from crates.io), arrow 59.
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
- `fixed.main.read_fixed(path, spec [, format =>, encoding =>, framing =>, record_length =>, compression =>])`
  — scan a fixed-width file (table function; `path` may glob). `compression =>`
  is `auto` (default; gzip/zstd detected by magic bytes) / `none` / `gzip` /
  `zstd` — decompressed before framing, local or `s3://` alike.
- `fixed.main.read_multi(path, spec [, encoding =>, framing =>, record_length =>, compression =>])`
  — scan a **heterogeneous** flat file whose records have different layouts selected
  by a discriminator field (header/detail/trailer), returning a single column
  `record` of Arrow **sparse Union** (DuckDB `UNION(H STRUCT(…), D STRUCT(…), …)`).
  Table function in `crates/fixedformat-worker/src/table/read_multi.rs`; core spec +
  variant selection in `crates/fixedformat-core/src/multirecord.rs`. The `spec` is a
  **multi-record JSON** object (see Spec formats below). Each framed record's
  discriminator bytes pick a variant `Layout` (`MultiLayout::select`); the record is
  decoded with that variant via the ordinary `decode_record`, wrapped in a
  `Value::Struct`, and the batch is built as a **sparse `UnionArray`** — each
  variant's child `StructArray` is **full batch length**, holding the decoded struct
  on its active rows and NULL on all others, with a per-row `type_ids` buffer (0..N,
  one per `records` entry in order) naming the active variant. Variant (union child)
  names are the discriminator tags. `union_tag(record)` gives the record type;
  `union_extract(record, 'D')` pulls the detail struct. Reuses the streaming
  source/framing/decompression machinery from `read_fixed` (`reader::{Source,
  open_stream, resolve_sources}`) and the `arrow_map` Layout→Arrow helpers
  (`layout_struct_type` builds each variant's STRUCT type). Framings: `newline`
  (default), `rdw`, `rdw_blocked` fully supported; `fixed` works too (one
  `record_length` for all types — records padded to a common length; defaults to the
  longest variant's static length) and is rejected for variable-length (OCCURS
  DEPENDING ON) variants. An unmatched discriminator value is a hard error unless the
  spec gives a `default` tag. `path` may glob / be `s3://`/`http(s)://` like
  `read_fixed`. No COPY-FROM or scalar (`unpack_multi`) counterpart yet, and
  `describe_fixed` does not (yet) accept multi-record specs.
- `fixed.main.write_fixed((FROM rel), path, spec [, format =>, encoding =>, framing =>])`
  — write a relation to a fixed-width file (table-buffering sink); returns
  `(rows_written, bytes_written)`.
- `fixed.main.describe_fixed(spec [, format =>])` — introspect a layout spec
  **without reading data** (table function in
  `crates/fixedformat-worker/src/table/describe_fixed.rs`; fixed output schema).
  One row per field (groups + children), columns: `path` (dotted, e.g.
  `item.sku`), `depth`, `kind` (codec label), `sql_type` (the DuckDB column type,
  e.g. `STRUCT`, `DECIMAL(9,2)`, `BIGINT[]`), `byte_offset` (named to dodge the
  `OFFSET` keyword), `width`, `occurs` (OCCURS max, else NULL), `depending_on`
  (ODO controller, else NULL). Flatten logic is in `fixedformat-core/src/describe.rs`.
- `fixed.main.fixedformat_version()`.
- `COPY <table> FROM '<path>' (FORMAT 'fixed.fixed', spec '<layout>' [, format, encoding,
  framing, record_length, compression, endpoint, region, url_style, use_ssl])` — load a fixed-width
  file straight into a DuckDB table (the COPY-FROM counterpart of `read_fixed`,
  in `crates/fixedformat-worker/src/copy_from.rs`). The format is **catalog-qualified**
  (`'<attach-name>.fixed'`, e.g. `'fixed.fixed'`). The output schema is the COPY
  **target table's** — each decoded column maps to a target column **by position**
  and is cast to its type (`arrow_cast::cast`), so the spec must produce the same
  number of columns in the same order. `path` may be local or `s3://`/`http(s)://`
  (credentials via `CREATE SECRET`, scoped per path — `secret_lookups` is forwarded
  from the SDK's `CopyFromTable`). COPY options are named (the source path comes from
  the COPY statement, not an option).
- `COPY (query|table) TO '<path>' (FORMAT 'fixed.fixed_out', spec '<layout>' [, format,
  encoding, framing, endpoint, region, url_style, use_ssl])` — write a relation out to a
  fixed-width file (the COPY-TO counterpart of `write_fixed`, in
  `crates/fixedformat-worker/src/copy_to.rs`). The writer uses a **distinct** format
  name `'<attach-name>.fixed_out'` (e.g. `'fixed.fixed_out'`) because the VGI worker SDK
  advertises FROM and TO as separate formats (and the extension registers one
  `CopyFunction` per format name). Each input column is matched to a layout field **by
  name** (same as `write_fixed`), encoded + framed, and written to `<path>`; DuckDB
  reports the row count. Mechanically it is a **buffered (Sink+Combine) function with no
  Source phase**: `write()` buffers each batch into `execution_id`-scoped
  cross-process storage; `close()` (DuckDB's once-only finalize) drains the shards and
  performs the terminal write. The Arrow→framed-bytes encode logic is shared with
  `write_fixed` via `crates/fixedformat-worker/src/record_writer.rs`. Local destinations
  are fully supported; an `s3://` destination works via the named overrides
  (`endpoint`/`region`/`url_style`/`use_ssl`) or ambient credentials, but `CREATE SECRET`
  credentials are **not** forwarded on the COPY-TO path (the SDK's `CopyToFunction` has
  no secret-bind hook) — use `write_fixed` for secret-backed cloud writes. COPY options
  are named (the destination path comes from the COPY statement, not an option).

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
two-phase-bind fix in the `vgi` SDK (`>= 0.9.5`, published on crates.io). Named overrides
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
  "justify","pad","sign"}, ...]` (or `{"fields":[...]}`). A field may instead carry
  a nested `"fields":[...]` array → it becomes a **group** (STRUCT; `type` optional),
  and with `"occurs"` → a LIST of STRUCT. So nested/repeating sub-records are
  expressible without a copybook (`jsonspec::layout_fields` recurses; children get
  group-relative offsets, same contract as copybook's `layout_nodes`).
- **multi-record** (`read_multi` only) — a JSON object selecting a per-record layout
  by a discriminator: `{"discriminator":{"offset","width"}, "records":{"H":[…],"D":
  […],"T":[…]}, "default":"D"?}`. Each `records` value is an ordinary **json** field
  list (reused verbatim via `jsonspec::parse`, so all field types / groups / OCCURS
  work per variant). `MultiLayout::parse` preserves the `records` order (stable union
  type-ids); `MultiLayout::select(record, enc)` reads + trims the discriminator bytes
  (EBCDIC-transcoded first), matches case-sensitively against the tags, else falls
  back to `default` (else errors). The discriminator bytes are part of each record's
  bytes, so a variant usually leads with a 1-byte `filler` covering the tag. Output is
  a `UNION` column (one STRUCT variant per record type), **not** the flat dynamic
  columns `read_fixed` produces.
- **copybook** — COBOL: nested groups (→ STRUCT), `PIC X/A/9/S/V`, `USAGE
  COMP-3`/`COMP`/`BINARY`, `OCCURS n` (→ LIST), `OCCURS [m TO] n DEPENDING ON ctrl`
  (variable-length table → LIST sized by the runtime value of `ctrl`), `REDEFINES`
  (→ folded STRUCT), `SIGN LEADING/TRAILING [SEPARATE]`.

`OCCURS … DEPENDING ON` makes the record **variable-length**: such a table reserves
**zero** static footprint, so fields after it shift by the actual body size.
`Layout.variable` flags it; `Field.depending_on` names the controller and
`Field.reserved_width()` returns 0. decode/encode (`decode.rs`/`encode.rs`) are
**shift-based**: each sibling list is walked with a running `shift` (= Σ consumed −
reserved), the controller's decoded Int is looked up from a name→value `scope`, and
encode grows its buffer (the static `record_len` is then only the **minimum**).
Variable layouts can't use `fixed` framing (the read path errors — use
`newline`/`rdw`); the controller must be decoded **before** the table.

Types: decimals (COMP-3/zoned/implied-point) → `DECIMAL(p,s)`, ints → `BIGINT`,
floats → `REAL`/`DOUBLE`, text/hex → `VARCHAR`, `?` → `BOOLEAN`. Encodings:
`ascii` (default) / `ebcdic` (CP037). Framing: `newline` (default) / `fixed` /
`rdw` / `rdw_blocked`.

Compression (read side only — `read_fixed` + `COPY … FROM`): `compression =>`
`auto` (default) / `none` / `gzip` / `zstd`. `auto` sniffs the leading magic
bytes (`1f 8b` gzip, `28 b5 2f fd` zstd) and decompresses before framing — so it
composes with every framing/encoding and works for local and `s3://` paths.
`Compression::{parse,detect}` live in `fixedformat-core/src/compression.rs`
(`decompress` is the buffered form); the **streaming** read path wraps the byte
source with `stream::decompress_reader` (a `Read` adapter over `flate2`/`zstd`,
peeking magic bytes via `BufRead::fill_buf` without consuming). `options::compression`
maps the named arg (`None` ⇒ auto). Writing compressed output is **not** supported
(`write_fixed` / `COPY … TO` emit raw bytes).

Reads are **streaming** for the self-delimiting framings (`newline` / `fixed`):
the byte source (local `File`, or a remote object fetched lazily per glob match)
→ optional `stream::decompress_reader` → `stream::RecordStream` (the streaming
framer) → `reader::StreamingProducer`, which decodes **one `BATCH_ROWS` batch per
`next_batch`** so peak memory is ~one batch, not the whole file plus every decoded
row. `rdw` / `rdw_blocked` still buffer the whole (decompressed) object inside
`RecordStream` (length-prefix walking needs it). `COPY … FROM` drives the same
streaming framer via `reader::read_all`, which collects (it maps columns by
position). Streaming means a malformed record surfaces **mid-scan**: the exception
aborts the SQL statement (so nothing partial commits), but earlier batches may
already have been emitted — i.e. it is fail-fast per statement, not "validate the
whole file before emitting".

## Layout

- `crates/fixedformat-core` — pure codecs, **no Arrow/VGI deps** (`unsafe`
  forbidden). The Layout IR (`layout.rs`) + three parsers (`template`, `jsonspec`,
  `copybook`) + decode/encode + `packed` (COMP-3) / `zoned` / `ebcdic` (CP037
  tables) / `framing` (slice splitter) / `stream` (streaming framer +
  `decompress_reader`) / `compression` (gzip/zstd). All correctness lives here,
  unit-tested directly.
- `crates/fixedformat-worker` — thin Arrow/VGI adapter: `arrow_map.rs` (Layout →
  Arrow fields, Value → arrays incl. Decimal128/List/Struct), `value_in.rs` (Arrow
  → Value for pack/write), `reader.rs` (streaming byte sources +
  `StreamingProducer` + the `read_all` collector), `options.rs`, and `scalar/`,
  `table/`, `buffering/`.
  `main.rs` registers everything and calls `Worker::run()`.
- `test/sql/*.test` — sqllogictest e2e (run via haybarn unittest). `data/` holds
  fixtures.

## Build & test

```sh
cargo test -p fixedformat-core   # 73 unit tests + proptest round-trip fuzzing (tests/roundtrip.rs)
cargo clippy --all-targets       # keep clean
cargo build --release            # build the worker
./run_tests.sh                   # end-to-end SQLLogic suite (13 files, see below)
./run_tests.sh test/sql/types.test   # single file (Catch2 filter; trailing * only)
```

Test fixtures under `data/` are regenerated deterministically by
`python3 data/generate_fixtures.py` (includes malformed fixtures for
`malformed.test`). `read_fixed` fails fast on any malformed record — the
exception aborts the statement (nothing partial commits). Newline/fixed reads
stream batch-by-batch, so the error can surface after earlier batches were
emitted; `malformed.test` asserts `statement error`, which still holds.

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
