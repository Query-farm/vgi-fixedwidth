# vgi-fixedformat

Read and write **fixed-width / flat-file / mainframe** data in DuckDB with SQL —
the equivalent of Perl `unpack()` / Python `struct`, plus COBOL copybooks
(COMP-3, zoned decimal, EBCDIC, `OCCURS` / `OCCURS … DEPENDING ON`, `REDEFINES`).
Nested records become `STRUCT`s, repeating groups become `LIST`s, and
`describe_fixed` shows you exactly how any spec resolves before you run it.

It runs as a [VGI worker](https://query.farm): a small standalone binary that
DuckDB launches and talks to over Apache Arrow. You `ATTACH` it and call its
functions like any other.

```sql
ATTACH 'fixed' (TYPE vgi, LOCATION './target/release/fixedformat-worker');
SET search_path = 'fixed.main';

SELECT unpack_fixed('JOHN      00042', 'name:A10 qty:9(5)');
-- {'name': JOHN, 'qty': 42}
```

---

## Quick start

**1. Get the worker binary.** Either download a prebuilt archive from the
[Releases page](https://github.com/Query-farm/vgi-fixedwidth/releases) for your
platform (`vgi-fixedformat-<version>-<platform>.tar.gz`, where `<platform>` is
one of `linux_amd64`, `linux_arm64`, `osx_amd64`, `osx_arm64`, `windows_amd64`)
and unpack the `fixedformat-worker` executable…

```sh
tar -xzf vgi-fixedformat-v0.1.0-osx_arm64.tar.gz   # → fixedformat-worker
```

…or build it from source (needs Rust 1.90+):

```sh
cargo build --release          # produces target/release/fixedformat-worker
```

Each release archive is accompanied by a SHA256 checksum, a keyless `cosign`
signature (`.cosign.bundle`), and a SLSA build-provenance attestation — see the
[release-tooling docs](https://github.com/Query-farm/vgi-actions) for the verify
recipe.

**2. Attach it in DuckDB** (any DuckDB with the `vgi` community extension):

```sql
INSTALL vgi FROM community;    -- one time
ATTACH 'fixed' (TYPE vgi, LOCATION '/absolute/path/to/fixedformat-worker');
SET search_path = 'fixed.main';   -- so you can call functions unqualified
```

Use an **absolute** `LOCATION` (it's resolved relative to DuckDB's working
directory).

---

## The functions

| Function | Shape | What it does |
|----------|-------|--------------|
| `unpack_fixed(rec, spec [, encoding])` | scalar → STRUCT | Parse one VARCHAR/BLOB record into a struct of fields |
| `pack_fixed(struct, spec [, encoding])` | scalar → BLOB | Format a struct back into a fixed-width record |
| `read_fixed(path, spec [, options…])` | table function | Read a whole fixed-width file into rows |
| `write_fixed((FROM rel), path, spec [, options…])` | table function | Write a relation out to a fixed-width file |
| `describe_fixed(spec [, format =>])` | table function | Introspect a spec (fields, types, offsets) without reading data |

`pack_fixed` is the exact inverse of `unpack_fixed`:
`pack_fixed(unpack_fixed(rec, s), s) = rec`.

### `unpack_fixed` — parse a record

```sql
SELECT unpack_fixed('JOHN      00042', 'name:A10 qty:9(5)').qty;   -- 42

-- Over a column:
SELECT (unpack_fixed(raw_line, 'name:A10 qty:9(5)')).*
FROM my_table;
```

`rec` can be `VARCHAR` or `BLOB` (use a BLOB for binary / COMP-3 / EBCDIC data).
The third argument is the byte encoding (`'ascii'` default, or `'ebcdic'`).

### `pack_fixed` — build a record

```sql
SELECT pack_fixed({'name': 'JOHN', 'qty': 42}, 'name:A10 qty:9(5)');
-- returns BLOB 'JOHN      00042'
```

### `read_fixed` — read a file

```sql
SELECT * FROM read_fixed('data/accounts.dat', 'name:A10 qty:9(5)');

-- COBOL copybook + EBCDIC + fixed-length records:
SELECT * FROM read_fixed('data/master.dat',
    '01 REC. 05 NM PIC X(20). 05 BAL PIC S9(7)V99 COMP-3.',
    encoding => 'ebcdic', framing => 'fixed');
```

`path` may be a glob (`data/*.dat`). Options are **named**: `format`, `encoding`,
`framing`, `record_length`.

### `write_fixed` — write a file

`write_fixed` is a **table function**, so call it in a `FROM` clause and pass the
input relation as a subquery `(FROM …)`:

```sql
SELECT * FROM write_fixed((FROM my_table), '/tmp/out.dat', 'name:A10 qty:9(5)');
-- returns one row: (rows_written, bytes_written)
```

### `describe_fixed` — introspect a spec

See exactly how a spec resolves — one row per field — **without reading any
data**. Great for debugging offsets or documenting a layout:

```sql
SELECT path, sql_type, byte_offset, width, occurs
FROM describe_fixed('name:A10 qty:9(5) vals:s(3)');
-- name  VARCHAR   0  10  NULL
-- qty   BIGINT   10   5  NULL
-- vals  BIGINT[] 15   2     3
```

Columns: `path` (dotted, e.g. `item.sku`), `depth`, `kind` (codec label),
`sql_type` (the DuckDB column type), `byte_offset`, `width`, `occurs`
(OCCURS maximum, else NULL), and `depending_on` (the `OCCURS … DEPENDING ON`
controller, else NULL).

---

## `COPY … FROM` / `COPY … TO`

The worker also plugs a fixed-width format into DuckDB's `COPY` statement, so you
can load and unload tables without the `read_fixed` / `write_fixed` calls. The
format name is **catalog-qualified** by the `ATTACH` name (`fixed` below). The
spec and other settings are passed as `COPY` options; the path comes from the
statement itself.

```sql
-- Load a fixed-width file straight into a table (reader: 'fixed.fixed').
CREATE TABLE accounts (name VARCHAR, qty INTEGER);
COPY accounts FROM 'accounts.dat' (FORMAT 'fixed.fixed', spec 'name:A10 qty:9(5)');

-- Write a query/table out to a fixed-width file (writer: 'fixed.fixed_out').
COPY (SELECT name, qty FROM accounts)
  TO 'out.dat' (FORMAT 'fixed.fixed_out', spec 'name:A10 qty:9(5)');
```

Options mirror the table functions: `spec` (required), `format`, `encoding`,
`framing`, plus `endpoint`/`region`/`url_style`/`use_ssl` for `s3://` paths. On
`COPY … FROM`, decoded columns map to the target table's columns **by position**;
on `COPY … TO`, input columns map to layout fields **by name**. The reader and
writer use **different** format names (`fixed.fixed` vs `fixed.fixed_out`) because
the VGI worker SDK advertises each direction as a separate format.

> Cloud note: `COPY … FROM` resolves `CREATE SECRET` credentials per path;
> `COPY … TO` does **not** forward DuckDB secrets — use named overrides / ambient
> credentials, or `write_fixed`, for secret-backed cloud writes.

---

## Spec formats

A *spec* describes the record layout. Three formats are accepted and
auto-detected (override with `format =>` on the table functions):

### 1. Template (Perl `unpack` / Python `struct` style)

Whitespace-separated tokens, each optionally `name:`-prefixed.

```
name:A10 qty:9(5) amt:l< flags:C
```

| Code | Meaning | The count is… |
|------|---------|---------------|
| `A` / `a` / `Z` | string — space-pad / null-pad / null-terminated | the **width** in bytes |
| `9(n)` / `S9(n)` / `X(n)` | display numeric / signed / text (COBOL PIC) | the digit/char count |
| `c` `C` | int8 / uint8 | a **repeat** → LIST |
| `s` `S` | int16 / uint16 | a repeat |
| `l` `L` (or `i` `I`) | int32 / uint32 | a repeat |
| `q` `Q` | int64 / uint64 | a repeat |
| `n` `N` | uint16 / uint32, **big-endian** | a repeat |
| `v` `V` | uint16 / uint32, **little-endian** | a repeat |
| `e` `f` `d` | float16 / float32 / float64 | a repeat |
| `H` `h` | hex string, high / low nibble first | the width |
| `?` | boolean byte | a repeat |
| `x` | pad byte(s) — consumed, no output column | the width |

**Byte order:** default is big-endian. A trailing `<` / `>` on a code sets it
per-field (`l<` = little-endian int32); a standalone `<` `>` `!` `=` `@` token
sets the default for everything after it. (`!`/`>` = network/big, `<` = little,
`=`/`@` = native.)

A count is `(n)` or trailing digits: `A10` and `A(10)` are the same; `s(3)` is
three int16s returned as a `LIST`.

### 2. JSON field list

Self-documenting; good for generated specs.

```json
[
  {"name": "id",   "type": "str",   "width": 10},
  {"name": "qty",  "type": "int",   "digits": 5},
  {"name": "amt",  "type": "comp3", "digits": 9, "scale": 2, "signed": true}
]
```

Types: `str`, `int`, `decimal`, `comp3`/`packed`, `zoned`, `binary`/`comp`,
`float`/`double`/`half`, `hex`, `bool`, `pad`. Options: `width`, `digits`,
`scale`, `signed`, `endian` (`big`/`little`), `occurs`, `justify` (`left`/`right`),
`pad`, `sign` (`leading`/`trailing`/`embedded`).

A field may instead carry a nested `fields` array, making it a **group**
(`STRUCT`; its `type` is then optional). Combined with `occurs` a group becomes a
`LIST` of `STRUCT`, so nested and repeating sub-records are expressible without a
COBOL copybook:

```json
[
  {"name": "hdr",   "type": "str", "width": 4},
  {"name": "lines", "occurs": 3, "fields": [
    {"name": "sku", "type": "str", "width": 3},
    {"name": "qty", "type": "int", "digits": 2}
  ]}
]
```

### 3. COBOL copybook

Real copybook text — paste it straight in.

```cobol
01  ACCOUNT-RECORD.
    05  ACCT-ID      PIC X(10).
    05  BALANCE      PIC S9(7)V99 COMP-3.
    05  HISTORY      OCCURS 12 PIC 9(6).
    05  RAW-DATE     PIC X(8).
    05  DATE-PARTS REDEFINES RAW-DATE.
        10  YYYY     PIC 9(4).
        10  MM       PIC 9(2).
        10  DD       PIC 9(2).
```

- Group items → `STRUCT` columns.
- `OCCURS n` → `LIST` (`UNNEST` it to get rows).
- `OCCURS [m TO] n DEPENDING ON ctrl` → a **variable-length** `LIST` whose length
  is the runtime value of the `ctrl` field (which must appear before the table).
  Records then vary in length, so the file must be `newline`- or `rdw`-framed
  (not `fixed`).
- `REDEFINES` → a `STRUCT` holding every overlapping interpretation of the same
  bytes (named after the base field).
- `USAGE COMP-3`/`PACKED-DECIMAL`, `COMP`/`COMP-4`/`COMP-5`/`BINARY`, and
  `SIGN LEADING/TRAILING [SEPARATE]` are honored.

---

## Type mapping

| Field | DuckDB type |
|-------|-------------|
| text / hex | `VARCHAR` |
| display / binary integer | `BIGINT` |
| COMP-3, zoned, implied-point decimal | `DECIMAL(p, s)` (exact) |
| float32 / float16 | `REAL` |
| float64 | `DOUBLE` |
| boolean | `BOOLEAN` |
| OCCURS / OCCURS DEPENDING ON | `LIST` of the above (variable-length for DEPENDING ON) |
| group / nested `fields` / REDEFINES | `STRUCT` |

## Encodings & framing

- **encoding**: `ascii` (default) or `ebcdic` (code page 037).
- **framing** (how records are delimited in a file):
  - `newline` (default) — one record per line.
  - `fixed` — no delimiters; records are exactly `record_length` bytes
    (defaults to the layout length; override with `record_length =>`).
  - `rdw` — IBM variable-length: each record prefixed with a 4-byte Record
    Descriptor Word.
  - `rdw_blocked` — RDW records inside Block Descriptor Word blocks.

---

## Recipes

**Split a fixed-width text column already in a table:**
```sql
SELECT (unpack_fixed(line, 'id:A8 name:A20 amt:9(9)')).*
FROM staging_lines;
```

**Convert a COBOL EBCDIC file to a Parquet file:**
```sql
COPY (
  SELECT * FROM read_fixed('mainframe.dat', '<copybook>',
                           encoding => 'ebcdic', framing => 'fixed')
) TO 'out.parquet';
```

**Flatten an OCCURS array into rows:**
```sql
SELECT r.acct_id, h.idx, h.val
FROM read_fixed('f.dat', '01 R. 05 ACCT-ID PIC X(10). 05 H OCCURS 3 PIC 9(4).') r,
     UNNEST(r.H) WITH ORDINALITY AS h(val, idx);
```

**Read variable-length records (`OCCURS … DEPENDING ON`):** the table length is
driven by an earlier count field, so the records vary in length — frame them with
`newline` (or `rdw`), not `fixed`:
```sql
SELECT N, ITEMS, TRAILER
FROM read_fixed('orders.dat',
  '01 R. 05 N PIC 9(1).
         05 ITEMS OCCURS 1 TO 9 TIMES DEPENDING ON N PIC X(2).
         05 TRAILER PIC X(3).',
  framing => 'newline');
```

**Debug a spec before running it** — see every field's type, byte offset, and
width without touching a file:
```sql
SELECT * FROM describe_fixed('01 R. 05 NM PIC X(20). 05 BAL PIC S9(7)V99 COMP-3.');
-- NM   …  VARCHAR        offset 0  width 20
-- BAL  …  DECIMAL(9,2)   offset 20 width 5
```

**Express nested / repeating records without COBOL** — give a JSON field a nested
`fields` array (a `STRUCT`), optionally with `occurs` (a `LIST` of `STRUCT`):
```sql
SELECT * FROM read_fixed('recs.dat',
  '[{"name":"hdr","type":"str","width":4},
    {"name":"lines","occurs":3,"fields":[
       {"name":"sku","type":"str","width":3},
       {"name":"qty","type":"int","digits":2}]}]');
```

**Round-trip / reformat a file** (read with one spec, write with another):
```sql
SELECT * FROM write_fixed(
  (FROM read_fixed('in.dat', 'a:A5 b:9(3)')),
  'out.dat', 'a:A8 b:9(5)');
```

---

## Gotchas

- The function is `unpack_fixed`, **not** `unpack` — `unpack` is a reserved
  keyword in DuckDB.
- `write_fixed` is a table function: use `SELECT * FROM write_fixed((FROM t), …)`,
  not `SELECT write_fixed(…)`.
- If a query **creates a table**, don't `SET search_path = 'fixed.main'` first —
  that points DDL at the read-only worker catalog. Either skip `search_path` and
  fully-qualify calls (`fixed.main.unpack_fixed(…)`), or create your tables before
  setting it.
- `pack_fixed`/`write_fixed` error if a value doesn't fit its field width — that's
  intentional (silent truncation corrupts fixed-width files).
- `read_fixed` decodes eagerly and **fails fast**: one malformed record (bad
  COMP-3 nibble, a too-short line, a ragged fixed-length stream) errors the whole
  query with a clear message rather than returning partial or corrupt rows.
- After rebuilding the worker, `DETACH fixed; ATTACH …` to pick up the new binary.

---

## Development

```sh
cargo test -p fixedformat-core   # unit tests + property-based round-trip fuzzing
cargo clippy --all-targets
./run_tests.sh                   # end-to-end SQLLogic suite (see below)
python3 data/generate_fixtures.py  # regenerate test fixtures
```

Coverage: 73 `fixedformat-core` unit tests, a `proptest` suite proving
`decode(encode(v)) == v` across every field kind (ASCII + EBCDIC), and 13 SQLLogic
files (160+ test directives) covering every function, spec format, framing mode, nested
and variable-length (`OCCURS … DEPENDING ON`) records, NULL handling, and
malformed-input behavior. Binary decoding is cross-checked against Python
`struct.pack` reference bytes.

The end-to-end suite (`test/sql/*.test`) runs against the built worker through
the haybarn DuckDB unittest runner. One-time setup:

```sh
uv tool install haybarn-unittest
uv tool install haybarn
echo "INSTALL vgi FROM community;" | uvx haybarn-cli
```

The codebase splits into a pure-logic crate and a thin adapter — see
[`CLAUDE.md`](CLAUDE.md) for architecture and conventions.
