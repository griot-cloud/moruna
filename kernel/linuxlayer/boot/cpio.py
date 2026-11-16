# SPDX-License-Identifier: MPL-2.0
"""Writes a directory as an uncompressed newc cpio archive, the initramfs the Linux
personality unpacks (core/src/fs/initramfs.rs).

    python3 cpio.py DIR OUT
"""
import os
import stat
import sys


def header(ino, mode, nlink, size, name):
    fields = [ino, mode, 0, 0, nlink, 0, size, 0, 0, 0, 0, len(name) + 1, 0]
    return b"070701" + b"".join(b"%08x" % f for f in fields)


def pad(n):
    return b"\0" * (-n % 4)


def entry(out, ino, mode, nlink, name, data):
    head = header(ino, mode, nlink, len(data), name) + name + b"\0"
    out.write(head + pad(len(head)) + data + pad(len(data)))


def main(root, path):
    ino = 1
    with open(path, "wb") as out:
        for at, dirs, files in os.walk(root):
            dirs.sort()
            rel = os.path.relpath(at, root)
            if rel != ".":
                entry(out, ino, stat.S_IFDIR | 0o755, 2, rel.encode(), b"")
                ino += 1
            for f in sorted(files):
                full = os.path.join(at, f)
                name = os.path.normpath(os.path.join(rel, f)).encode()
                mode = os.stat(full).st_mode
                with open(full, "rb") as src:
                    data = src.read()
                entry(out, ino, stat.S_IFREG | stat.S_IMODE(mode), 1, name, data)
                ino += 1
        entry(out, 0, 0, 1, b"TRAILER!!!", b"")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
