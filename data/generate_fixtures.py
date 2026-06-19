#!/usr/bin/env python3
"""Regenerate the binary test fixtures used by test/sql/*.test.

Run from the repo root:  python3 data/generate_fixtures.py

Each fixture is small and deterministic so the SQLLogic expectations are stable.
"""
import os

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
write(
    "accounts.dat",
    b"JOHN      00042\nJANE      00100\nBOB       00007\n",
)

# ASCII fixed-length, no delimiters: two 15-byte records.
write("accounts_fb.dat", b"JOHN      00042JANE      00100")

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

print("done")
