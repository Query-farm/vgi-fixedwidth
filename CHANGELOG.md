# Changelog

All notable changes to `vgi-fixedformat` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
