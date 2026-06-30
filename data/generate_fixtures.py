#!/usr/bin/env python3
"""Regenerate the binary test fixtures used by test/sql/*.test.

Run from the repo root:  python3 data/generate_fixtures.py

Each fixture is small and deterministic so the SQLLogic expectations are stable.
"""
import gzip
import os
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))


def comp3(value: int, nbytes: int) -> bytes:
    """COBOL COMP-3 packed decimal: 2 digits/byte, sign nibble last."""
    neg = value < 0
    digits = f"{abs(value):0{nbytes * 2 - 1}d}"
    nibbles = [int(c) for c in digits] + [0xD if neg else 0xC]
    return bytes((nibbles[i] << 4) | nibbles[i + 1] for i in range(0, len(nibbles), 2))


def write(name: str, data: bytes) -> None:
    path = os.path.join(HERE, name)
    with open(path, "wb") as f:
        f.write(data)
    print(f"wrote {name} ({len(data)} bytes)")


# ASCII newline-delimited: name X(10) + qty 9(5).
ACCOUNTS = b"JOHN      00042\nJANE      00100\nBOB       00007\n"
write("accounts.dat", ACCOUNTS)

# Compressed copies of accounts.dat for transparent-decompression tests. They
# decode byte-for-byte to ACCOUNTS, so the SQL expectations match the plain file.
# gzip uses a fixed mtime=0 so the bytes are reproducible across runs.
write("accounts.dat.gz", gzip.compress(ACCOUNTS, mtime=0))
write(
    "accounts.dat.zst",
    subprocess.run(
        ["zstd", "-q", "-19", "-c"], input=ACCOUNTS, stdout=subprocess.PIPE, check=True
    ).stdout,
)

# ASCII fixed-length, no delimiters: two 15-byte records.
write("accounts_fb.dat", b"JOHN      00042JANE      00100")

# Three-column ASCII newline file for projection-pushdown tests: a X(3), b X(3),
# c 9(3). Distinct values per column so a reordered/subset projection
# (e.g. SELECT c, a) would surface any positional (vs. by-name) mis-mapping.
write("proj.dat", b"ABCDEF012\nGHIJKL034\nMNOPQR056\n")

# EBCDIC (CP037) name X(5) + COMP-3 S9(3)V99: ACME/+123.45, WIDGE/-67.89.
ebcdic = b""
for name, amt in [("ACME", 12345), ("WIDGET", -6789)]:
    ebcdic += name[:5].ljust(5).encode("cp037") + comp3(amt, 3)
write("ebcdic_comp3.dat", ebcdic)

# Large file: 5000 newline records id 9(7), values 0..4999.
write("large.dat", "".join(f"{i:07d}\n" for i in range(5000)).encode())

# RDW variable-length framing: three fixed 3-byte records, each with a 4-byte RDW.
rdw = b""
for rec in (b"AAA", b"BBB", b"CCC"):
    ln = len(rec) + 4
    rdw += bytes([ln >> 8, ln & 0xFF, 0, 0]) + rec
write("rdw.dat", rdw)

# Glob set: acct1.dat / acct2.dat, name X(10) + qty 9(5).
write("acct1.dat", b"AA        00001\n")
write("acct2.dat", b"BB        00002\nCC        00003\n")


def rdw(rec: bytes) -> bytes:
    ln = len(rec) + 4
    return bytes([ln >> 8, ln & 0xFF, 0, 0]) + rec


def block(recs) -> bytes:
    body = b"".join(rdw(r) for r in recs)
    ln = len(body) + 4
    return bytes([ln >> 8, ln & 0xFF, 0, 0]) + body  # BDW + RDW records


# RDW *blocked* framing: two BDW blocks, each holding RDW-framed records.
write("rdw_blocked.dat", block([b"AAA", b"BBB"]) + block([b"CCC"]))

# --- malformed fixtures (for test/sql/malformed.test) ---

# Fixed-length name X(3) + COMP-3 S9(3) (3 digits -> 2 bytes). Second record's
# packed field has an invalid digit nibble (0xA where a 0-9 digit is required).
write("bad_comp3.dat", b"ABC" + bytes([0x12, 0x3C]) + b"XYZ" + bytes([0xA0, 0x0C]))

# Newline file whose second line is too short for a name:A10 qty:9(5) layout.
write("short_line.dat", b"JOHN      00042\nSHORT\n")

# Fixed-length stream whose length is not a multiple of the record length (5).
write("ragged_fb.dat", b"AAA00BB")

# --- OCCURS DEPENDING ON (variable-length records, newline-framed) ---
# Layout: N PIC 9(1), ITEMS OCCURS 1 TO 9 DEPENDING ON N PIC X(2), TRAILER X(3).
# Each record's length varies with N, so it needs newline (or RDW) framing.
write("odo.dat", b"2AABBEND\n1XYEND\n0ZZZ\n")

# --- multi-record-type file (for test/sql/multirecord.test) ---
# Newline-delimited; a 1-byte discriminator at offset 0 selects the layout:
#   H = header : co X(20)
#   D = detail : sku X(10) + qty 9(5)
#   T = trailer: count 9(6)
# The whole record (including the discriminator byte) is the layout's bytes, so
# each variant's first field starts at byte 0 and overlaps the 1-byte tag.
multi = b""
multi += b"H" + b"ACME CORP".ljust(20)[:20] + b"\n"
multi += b"D" + b"WIDGET".ljust(10)[:10] + b"00042" + b"\n"
multi += b"D" + b"GADGET".ljust(10)[:10] + b"00007" + b"\n"
multi += b"T" + b"000002" + b"\n"
write("multi.dat", multi)

# Same shape plus an UNKNOWN record type 'Z' (detail-shaped: 1 + 10 + 5 bytes) to
# exercise both the hard error (no default) and the `default` fallback.
write("multi_bad.dat", multi + b"Z" + b"MYSTERY".ljust(10)[:10] + b"00099" + b"\n")

# Fixed-length multi-record file: EVERY record type is padded to a common 21-byte
# length (1-byte tag + 20-byte payload), so it can be read with `framing => 'fixed'`
# (no delimiters). Used by the fixed-framing multi round-trip test.
#   H = tag + co X(20)
#   D = tag + sku X(10) + qty 9(5) + 5 pad
#   T = tag + cnt 9(6) + 14 pad
mf = b""
mf += b"H" + b"ACME CORP".ljust(20)[:20]
mf += b"D" + b"WIDGET".ljust(10)[:10] + b"00042" + b" " * 5
mf += b"D" + b"GADGET".ljust(10)[:10] + b"00007" + b" " * 5
mf += b"T" + b"000002" + b" " * 14
write("multi_fixed.dat", mf)

# Newline file of YYYYMMDD date fields, for the read_fixed date-type test.
write("dates.dat", b"20240131\n20231225\n20250704\n")

print("done")
