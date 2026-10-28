#!/usr/bin/env python3
"""The disk the CI boot attaches as virtio-blk (SDD 25, DR-T9).

    disk-image.py write PATH   writes 4 MiB of pattern A, which block-test reads
    disk-image.py check PATH   checks the image holds pattern B, which block-test
                               writes over A, flushes and reads back

Byte i of sector s is (31 s + 7 i + 0x11) mod 256 in pattern A and
(13 s + 5 i + 0x77) mod 256 in pattern B, as kernel/drivers/block-test computes them.
"""

import sys

SECTORS = 8192
SECTOR = 512


def pattern(sector_factor, byte_factor, constant):
    out = bytearray(SECTORS * SECTOR)
    for s in range(SECTORS):
        base = (sector_factor * s + constant) & 0xFF
        at = s * SECTOR
        out[at : at + SECTOR] = bytes((base + byte_factor * i) & 0xFF for i in range(SECTOR))
    return bytes(out)


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("write", "check"):
        sys.exit("usage: disk-image.py write|check PATH")
    command, path = sys.argv[1], sys.argv[2]
    if command == "write":
        with open(path, "wb") as f:
            f.write(pattern(31, 7, 0x11))
        return
    with open(path, "rb") as f:
        data = f.read()
    want = pattern(13, 5, 0x77)
    if data == want:
        print(f"pass: the disk image holds pattern B in all {SECTORS} sectors")
        return
    if len(data) != len(want):
        print(f"FAIL: the disk image is {len(data)} bytes, not {len(want)}")
        sys.exit(1)
    first = next(i for i in range(len(data)) if data[i] != want[i])
    print(
        f"FAIL: the disk image differs from pattern B first at sector {first // SECTOR} "
        f"byte {first % SECTOR}: {data[first]:#04x}, not {want[first]:#04x}"
    )
    sys.exit(1)


if __name__ == "__main__":
    main()
