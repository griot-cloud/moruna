"""Package factory for REQUIRED actual cross-adapter RO-assets acceptance.

Package this through the canonical PEX packager with args=true and identity table
schemas. Supply an arbitrary real source directory (not /assets) containing
weights.txt with exactly `kd3-real-assets-v1` and a nested directory. The same
package/args run on Linux sandbox and native Task; no fake service is used.
"""
import errno
import mmap
import os


def readonly_identity(args):
    if args != {"fixture": "kd3-assets-v1"}:
        raise ValueError("actual canonical package args differ")
    with open("/assets/weights.txt", "rb") as source:
        if source.read() != b"kd3-real-assets-v1":
            raise ValueError("actual delegated asset bytes differ")
        with mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as view:
            if view[:] != b"kd3-real-assets-v1":
                raise ValueError("actual readonly asset mapping differs")
    denied = {errno.EACCES, errno.EPERM, errno.EROFS}

    def refuses(operation):
        try:
            operation()
        except OSError as error:
            if error.errno not in denied:
                raise
        else:
            raise AssertionError("readonly assets mutation unexpectedly succeeded")

    def write_existing():
        with open("/assets/weights.txt", "r+b") as target:
            target.write(b"corrupt")

    def create_new():
        with open("/assets/new.txt", "xb") as target:
            target.write(b"corrupt")

    refuses(write_existing)
    refuses(create_new)
    refuses(lambda: os.rename("/assets/weights.txt", "/assets/moved.txt"))
    # Parent traversal cannot mutate the original view either.
    refuses(lambda: os.unlink("/assets/nested/../weights.txt"))
    with open("/assets/weights.txt", "rb") as source:
        if source.read() != b"kd3-real-assets-v1":
            raise ValueError("asset data changed after denied mutations")

    def identity(batch):
        return batch

    return identity
