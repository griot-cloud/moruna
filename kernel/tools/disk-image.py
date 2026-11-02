#!/usr/bin/env python3
"""The disks a boot writes and checks (SDD 25, DR-T9 and DR-T18; the boot
description's `disk` lines, which moruna-image writes to OUT/disks).

    disk-image.py write PATH SECTORS          writes SECTORS sectors of pattern A
    disk-image.py check PATH SECTORS a|b      checks the image holds that pattern:
                                              A untouched, or B, which a test writes
                                              over A, flushes and reads back

Byte i of sector s is (31 s + 7 i + 0x11) mod 256 in pattern A and
(13 s + 5 i + 0x77) mod 256 in pattern B, as kernel/drivers/block-test and
kernel/drivers/nvme-test compute them.
"""

import sys

SECTOR = 512
PATTERNS = {"a": (31, 7, 0x11), "b": (13, 5, 0x77)}


def pattern(name, sectors):
    sector_factor, byte_factor, constant = PATTERNS[name]
    # A sector's bytes depend only on its base, so there are 256 distinct sectors.
    rows = [bytes((base + byte_factor * i) & 0xFF for i in range(SECTOR)) for base in range(256)]
    return b"".join(rows[(sector_factor * s + constant) & 0xFF] for s in range(sectors))


def main():
    args = sys.argv[1:]
    if len(args) == 3 and args[0] == "write":
        _, path, sectors = args
        with open(path, "wb") as f:
            f.write(pattern("a", int(sectors)))
        return
    if len(args) != 4 or args[0] != "check" or args[3] not in PATTERNS:
        sys.exit("usage: disk-image.py write PATH SECTORS | check PATH SECTORS a|b")
    _, path, sectors, name = args
    sectors = int(sectors)
    with open(path, "rb") as f:
        data = f.read()
    want = pattern(name, sectors)
    label = name.upper()
    if data == want:
        print(f"pass: {path} holds pattern {label} in all {sectors} sectors")
        return
    if len(data) != len(want):
        print(f"FAIL: {path} is {len(data)} bytes, not {len(want)}")
        sys.exit(1)
    first = next(i for i in range(len(data)) if data[i] != want[i])
    print(
        f"FAIL: {path} differs from pattern {label} first at sector {first // SECTOR} "
        f"byte {first % SECTOR}: {data[first]:#04x}, not {want[first]:#04x}"
    )
    sys.exit(1)


if __name__ == "__main__":
    main()
