# vgi-fixedformat

Read and write **fixed-width / flat-file / mainframe** data in DuckDB with SQL —
the equivalent of Perl `unpack()` / Python `struct`, plus COBOL copybooks
(COMP-3, zoned decimal, EBCDIC, OCCURS, REDEFINES).

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

**1. Build the worker** (needs Rust 1.86+):

```sh
cargo build --release          # produces target/release/fixedformat-worker
```

**2. Attach it in DuckDB** (any DuckDB with the `vgi` community extension):

```sql
INSTALL vgi FROM community;    -- one time
ATTACH 'fixed' (TYPE vgi, LOCATION '/absolute/path/to/fixedformat-worker');
SET search_path = 'fixed.main';   -- so you can call functions unqualified
```

Use an **absolute** `LOCATION` (it's resolved relative to DuckDB's working
directory).

---

## The four functions

| Function | Shape | What it does |
|----------|-------|--------------|
| `unpack_fixed(rec, spec [, encoding])` | scalar → STRUCT | Parse one VARCHAR/BLOB record into a struct of fields |
| `pack_fixed(struct, spec [, encoding])` | scalar → BLOB | Format a struct back into a fixed-width record |
| `read_fixed(path, spec [, options…])` | table function | Read a whole fixed-width file into rows |
| `write_fixed((FROM rel), path, spec [, options…])` | table function | Write a relation out to a fixed-width file |

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
| OCCURS | `LIST` of the above |
| group / REDEFINES | `STRUCT` |

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
- After rebuilding the worker, `DETACH fixed; ATTACH …` to pick up the new binary.

---

## Development

```sh
cargo test -p fixedformat-core   # fast unit tests (codecs & parsers)
cargo clippy --all-targets
./run_tests.sh                   # end-to-end SQLLogic suite (see below)
python3 data/generate_fixtures.py  # regenerate test fixtures
```

The end-to-end suite (`test/sql/*.test`) runs against the built worker through
the haybarn DuckDB unittest runner. One-time setup:

```sh
uv tool install haybarn-unittest
uv tool install haybarn
echo "INSTALL vgi FROM community;" | uvx haybarn-cli
```

The codebase splits into a pure-logic crate and a thin adapter — see
[`CLAUDE.md`](CLAUDE.md) for architecture and conventions.
