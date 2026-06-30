# Changelog

All notable changes to `vgi-fixedformat` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

## [0.6.0] — write & cloud

### Added
- **Write-side compression**: `write_fixed` / `COPY … TO` now emit gzip/zstd —
  `compression =>` `auto` (default: gzip for `.gz`, zstd for `.zst`, else raw) /
  `none` / `gzip` / `zstd`. (Replaces the previous "reject a compressed
  destination" guard.)
- **`write_multi`**: the inverse of `read_multi` — write a relation whose single
  column is a `UNION` back out to a heterogeneous multi-record-type file
  (stamps the discriminator + encodes each row with its variant layout).
- **True S3/HTTP byte-streaming**: remote objects are now read in 8 MiB byte
  ranges on demand instead of being fetched whole, so a large object streams with
  bounded memory (≈ one chunk + one batch) for newline/fixed framing.

## [0.5.0] — functionality

### Added
- **Multi-record-type files** (`read_multi`): a JSON spec with a `discriminator`
  + per-type layouts decodes each record with the layout chosen by its type and
  returns a single `record` column of DuckDB **`UNION`** (one `STRUCT` variant per
  record type — `union_tag` / `union_extract` to access). Header/detail/trailer
  files now read in one pass.
- **Date / time field type**: JSON `"date"` / `"time"` / `"datetime"` with a
  strftime `format` parse fixed-width display bytes into DuckDB `DATE` / `TIME` /
  `TIMESTAMP` (and back on write).
- **Edited (PICTURE-editing) numerics**: report/print-image PICs like
  `ZZ,ZZ9.99`, `$$$,$$9.99`, `9(5)CR`, `**1,234.50` decode to `DECIMAL(p,s)`
  (stripping the editing); the non-floating masks round-trip on write.
- **Projection pushdown** for `read_fixed`: only the selected columns are
  materialized, mapped **by name** (which also fixed a latent reorder bug in the
  positional transpose).

### Fixed
- **COBOL `SYNCHRONIZED` (SYNC) alignment**: binary items now align to their
  natural halfword/fullword/doubleword boundary via implicit slack bytes — a SYNC
  copybook previously computed wrong offsets for the item and everything after it.

## [0.4.0] — hardening

### Security / hardening (untrusted input)
- **Decompression-bomb caps.** gzip/zstd input is bounded by
  `max_decompressed_bytes` (16 GiB default, configurable on `read_fixed` /
  `COPY … FROM`), a single record by 512 MiB, and the zstd window is bounded.
  Uncompressed input is unaffected.
- **`OCCURS` count clamp.** An attacker-controlled `OCCURS … DEPENDING ON` count
  can no longer pre-allocate gigabytes; it fails fast on the first out-of-bounds
  read instead.
- **SSRF guard.** An `http(s)://` read aimed at an internal host (cloud metadata
  `169.254.169.254`, loopback, RFC-1918/ULA) is refused; override with
  `FIXEDFORMAT_ALLOW_INTERNAL_HOSTS=1`.
- **Strict write-by-name.** `pack_fixed` / `write_fixed` / `COPY … TO` now error
  on a missing or mis-named (typo'd) column instead of silently writing a blank
  field. A present-but-`NULL` value is still allowed.
- **Checked decimal arithmetic.** COMP-3 / zoned decode error on overflow instead
  of silently wrapping in release builds; `DECIMAL` precision > 38 is rejected.
- **Caps on `record_length` and spec nesting depth** (max 64) to bound allocation
  and recursion.
- **`COPY … TO` / `write_fixed` reject a `.gz`/`.zst` destination** (write-side
  compression is unsupported — no more raw bytes under a compressed name).

### Added
- Streaming `newline`/`fixed` reads (flat memory) and a record source that fetches
  each globbed remote object lazily.
- Read errors now name the source file and 1-based record number.

### Changed
- Per-batch transpose moves `Value`s instead of cloning them.
- `block_on` handles a current-thread ambient runtime without panicking.

## [0.3.0]
- Streaming reads for `newline` / `fixed` framing (peak memory ≈ one batch).

## [0.2.0]
- Transparent gzip/zstd decompression on the read path (`read_fixed` /
  `COPY … FROM`), auto-detected by magic bytes or forced via `compression =>`.

## [0.1.1]
- Internal: removed a brittle hardcoded version assertion from the test suite.

## [0.1.0]
- First release: `unpack_fixed` / `pack_fixed` scalars; `read_fixed` /
  `write_fixed` / `describe_fixed` table functions; `COPY … FROM` / `COPY … TO`;
  template / JSON / COBOL-copybook specs; ASCII + EBCDIC; COMP-3 / zoned decimals;
  `OCCURS` / `OCCURS … DEPENDING ON` / `REDEFINES`; local + `s3://` + `http(s)://`
  paths. Signed, multi-platform release binaries.
