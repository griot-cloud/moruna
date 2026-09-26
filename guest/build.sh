#!/usr/bin/env bash
# Build the Moruna guest image as an OCI layout, reproducibly, for this host's architecture.
#
#   guest/build.sh <moruna-cp314t-wheel> <out-dir>
#
# Everything is fetched by the version and digest in guest/pins.env and built inside the
# pinned build container, with SOURCE_DATE_EPOCH taken from the last commit, fixed kernel build
# identity, a Debian archive frozen at a snapshot, sorted archives with normalised times, and
# deterministic JSON (guest/oci.py). Two runs from one commit give one manifest digest; the
# release job checks that by building twice.
#
# Output: <out-dir>/oci (the layout `moruna-vmm boot --image` reads), <out-dir>/digest, and
# the three layers beside it (vmlinux or Image, initramfs.cpio.gz, rootfs.erofs), and the
# root filesystem tree the SBOM is taken from (rootfs/).
#
# Needs: Linux, docker. Runs natively; the release builds arm64 on an arm64 runner.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
wheel="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
out="$(mkdir -p "$2" && cd "$2" && pwd)"
[ -f "$wheel" ] || { echo "guest/build.sh: no wheel at $1" >&2; exit 2; }

# shellcheck source=/dev/null
. "$here/pins.env"

case "$(uname -m)" in
    x86_64) arch=x86_64 oci_arch=amd64 karch=x86_64 kimage=vmlinux ;;
    aarch64 | arm64) arch=aarch64 oci_arch=arm64 karch=arm64 kimage=arch/arm64/boot/Image ;;
    *) echo "guest/build.sh: unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac
py_sha_var="PYTHON_SHA256_${arch}"
pa_sha_var="PYARROW_SHA256_${arch}"

SOURCE_DATE_EPOCH="$(git -C "$repo" log -1 --format=%ct)"
export SOURCE_DATE_EPOCH

docker run --rm \
    -e SOURCE_DATE_EPOCH \
    -e ARCH="$arch" -e KARCH="$karch" -e KIMAGE="$kimage" \
    -e KERNEL_VERSION -e KERNEL_SHA256 \
    -e BUSYBOX_VERSION -e BUSYBOX_SHA256 \
    -e PYTHON_RELEASE -e PYTHON_VERSION -e PYTHON_SHA256="${!py_sha_var}" \
    -e PYARROW_VERSION -e PYARROW_SHA256="${!pa_sha_var}" \
    -e DEBIAN_SUITE -e DEBIAN_SNAPSHOT \
    -e WHEEL="/in/$(basename "$wheel")" \
    -v "$here:/guest:ro" \
    -v "$(dirname "$wheel"):/in:ro" \
    -v "$out:/out" \
    "$BUILD_IMAGE" bash -euo pipefail -c '
mirror="http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}"
echo "deb [check-valid-until=no] $mirror $DEBIAN_SUITE main" > /etc/apt/sources.list
rm -f /etc/apt/sources.list.d/*
apt-get -o Acquire::Check-Valid-Until=false update -qq
apt-get install -qq -y --no-install-recommends \
    build-essential bc bison flex libelf-dev libssl-dev xz-utils bzip2 cpio \
    erofs-utils mmdebstrap ca-certificates curl python3 >/dev/null

work=/tmp/build && mkdir -p $work && cd $work
fetch() { curl -fsSL -o "$2" "$1" && echo "$3  $2" | sha256sum -c --quiet -; }

# The kernel: allnoconfig plus the Moruna fragments, with a fixed build identity.
fetch "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" \
    linux.tar.xz "$KERNEL_SHA256"
tar xf linux.tar.xz && cd "linux-${KERNEL_VERSION}"
export KBUILD_BUILD_TIMESTAMP="@${SOURCE_DATE_EPOCH}" KBUILD_BUILD_USER=moruna \
    KBUILD_BUILD_HOST=moruna KBUILD_BUILD_VERSION=1
make -s ARCH=$KARCH allnoconfig
scripts/kconfig/merge_config.sh -m -O . .config /guest/kernel/moruna.config \
    /guest/kernel/${KARCH}.config >/dev/null
make -s ARCH=$KARCH olddefconfig
# Every fragment line must survive olddefconfig; a dropped one is a silent misbuild.
grep -hE "^CONFIG_[A-Z0-9_]+=y" /guest/kernel/moruna.config /guest/kernel/${KARCH}.config \
    | while read -r want; do grep -qx "$want" .config || { echo "kernel: $want lost" >&2; exit 1; }; done
make -s ARCH=$KARCH -j"$(nproc)"
cp "$KIMAGE" /out/$(basename "$KIMAGE")
cd $work

# BusyBox, static: the first-stage init and poweroff.
fetch "https://busybox.net/downloads/busybox-${BUSYBOX_VERSION}.tar.bz2" bb.tar.bz2 \
    "$BUSYBOX_SHA256"
tar xf bb.tar.bz2 && cd "busybox-${BUSYBOX_VERSION}"
make -s defconfig
sed -i "s/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/; s/^CONFIG_TC=y/# CONFIG_TC is not set/" .config
make -s -j"$(nproc)"
cp busybox $work/busybox
cd $work

# The initramfs: busybox and /init, sorted, times normalised, gzip without a name or time.
mkdir -p initramfs/bin initramfs/dev initramfs/proc initramfs/sys initramfs/newroot
cp busybox initramfs/bin/busybox
cp /guest/initramfs/init initramfs/init
chmod 0755 initramfs/init initramfs/bin/busybox
(cd initramfs && find . -print0 | LC_ALL=C sort -z \
    | xargs -0 touch -h -d "@${SOURCE_DATE_EPOCH}" \
    && find . -print0 | LC_ALL=C sort -z \
    | cpio --null --create --format=newc --owner=0:0 --reproducible --quiet) \
    | gzip -9 -n > /out/initramfs.cpio.gz

# The root filesystem: Debian minbase with udev, free-threaded CPython 3.14, pyarrow and the
# moruna wheel, BusyBox, and the Moruna init.
mmdebstrap --quiet --variant=minbase --include=udev,ca-certificates \
    "$DEBIAN_SUITE" rootfs "$mirror"
fetch "https://github.com/astral-sh/python-build-standalone/releases/download/${PYTHON_RELEASE}/cpython-${PYTHON_VERSION}+${PYTHON_RELEASE}-${ARCH}-unknown-linux-gnu-freethreaded-install_only.tar.gz" \
    python.tar.gz "$PYTHON_SHA256"
mkdir -p rootfs/opt && tar xf python.tar.gz -C rootfs/opt
fetch "https://files.pythonhosted.org/packages/cp314/p/pyarrow/pyarrow-${PYARROW_VERSION}-cp314-cp314t-manylinux_2_28_${ARCH}.whl" \
    "pyarrow-${PYARROW_VERSION}-cp314-cp314t-manylinux_2_28_${ARCH}.whl" "$PYARROW_SHA256"
PY=rootfs/opt/python/bin/python3
$PY -m pip install --quiet --no-index --no-deps --no-compile \
    "pyarrow-${PYARROW_VERSION}-cp314-cp314t-manylinux_2_28_${ARCH}.whl" "$WHEEL"
# Bytecode is compiled once here, deterministically, so the read-only root never needs it.
SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH $PY -m compileall -q --invalidation-mode unchecked-hash \
    rootfs/opt/python/lib
cp busybox rootfs/usr/local/bin/busybox
cp -r /guest/rootfs/. rootfs/
chmod 0755 rootfs/sbin/moruna-init rootfs/usr/local/bin/moruna-report
mkdir -p rootfs/disk
rm -rf rootfs/var/cache/apt/* rootfs/var/lib/apt/lists/* rootfs/var/log/* rootfs/tmp/*
# The tree itself stays beside the image: the release takes the SBOM from it.
rm -rf /out/rootfs && cp -a rootfs /out/rootfs
mkfs.erofs --quiet -T "$SOURCE_DATE_EPOCH" --all-root -U 00000000-0000-0000-0000-000000000000 \
    -zlz4hc /out/rootfs.erofs rootfs

cd /out && python3 /guest/oci.py oci '"$oci_arch"' $(basename "$KIMAGE") \
    initramfs.cpio.gz rootfs.erofs > digest
'
echo "guest image: $(cat "$out/digest") in $out/oci"
