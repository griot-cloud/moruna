# Moruna SDD 27: The image filesystem (`kernel/imagefs`, `moruna-image::layerfile`)

**Document type:** software design document, component 27
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KC2 (Brackly, 2026-10-28): "the image filesystem server: layers as block devices, one overlay, whiteouts, paging on demand"; `architecture/optimized-runtime.md` section 3 (the filesystem) and section 7 (images); SDD 25 section o (what KC2 needs of the block protocol); SDD 19 IM-O2 (layers as shared read-only devices)
**Decisions:** none new; MK1 (OSTD) stands
**Component location:** `kernel/imagefs/fs` (`moruna-imagefs`: the format, the reader, the merge, the paths, the page cache, the scratch directory; `no_std`, host tested), `kernel/imagefs/protocol` (`imagefs-protocol`: `moruna.imagefs` and its conformance suite), `kernel/imagefs/server` (`imagefs`, the server process), `kernel/imagefs/test` (`imagefs-test`, the boot's client); the block protocol's registered buffers in `kernel/drivers/block-protocol` and `kernel/drivers/virtio-blk` (SDD 25 d.3, f.9); `crates/moruna-image/src/layerfile.rs` (the writer, the conversion, the host reader) and `src/bin/moruna-layer.rs`; the boot's layer disks in `kernel/tools/{build,boot}.sh` and `kernel/initramfs/boot.image`; the image tool's `data:` handles (`kernel/tools/image`)
**Consumes:** `moruna-svc` (protocols, the dispatcher, the conformance harness), `block-protocol` 1.1, `devices-protocol` (the directory), SDD 19's unpacking (`moruna-image`), the `sha2` crate (host). **Consumed by:** KC1 (the Linux personality: `openat`, `read`, `pread`, `getdents`, `readlink`, `fstat`, `mmap` of a file), KD2 (the engine starts tasks on an image), KD4 (the launcher presents one block device per layer).

**Decisions worth your eye:** (1) the layer file is Moruna's own, read only and page aligned: a header, a node table, names, extents, every file's data from a page boundary, and a SHA-256 tree over every page, so a page read cold is checked against the root the header carries without reading anything else of the layer; (2) a layer file is what a layer did to the layers below it as SDD 19's unpacking did it, not the layer's tar, so the merge is three rules and reproduces the unpacking exactly, symbolic links of lower layers followed included; (3) the page cache is one DMA memory object registered once with every layer's driver (block protocol 1.1), so a cache miss pins nothing; (4) `map` fills a new read-only memory object, reading what the cache does not hold from the device straight into it, and keeps it, so the next task that maps the same pages gets the same memory; (5) no kernel call was added: the long-lived pin the page cache needs is a pin the driver keeps (m.1).

---

## a. Purpose and boundary

Every task process on the Moruna microkernel needs a root filesystem: its image, read only, and somewhere to write. The image arrives as one read-only virtio-blk device per layer (`moruna-vmm` presents them, KD4; the CI boot attaches two). This component owns: the layer file format and the host writer that converts an OCI image's layers to it; the reader, which checks every page it reads; the merge of the layers into one tree; path resolution over that tree and a task's scratch directory; the page cache shared by every task; the `moruna.imagefs` protocol, its server and its conformance suite; the block protocol's registered buffers, which the cache is built on.

It refuses to know: the Linux system call interface (KC1 translates it into this protocol), how an image is fetched or built (SDD 19, KA3), how layer devices are attached (KD4), and anything a task writes outside its scratch directory, which nothing may.

## b. Vocabulary

**Layer file.** One layer in the format of e.1. **Layer id.** The SHA-256 of a layer file. **Node.** One entry of a layer's node table: a directory, a regular file, a symbolic link or a whiteout. **Whiteout.** A node saying its name is gone from the layers below. **Opaque.** A directory flag: nothing of the layers below shows through it. **Content page.** A page of a layer file between the header and the tree. **Tree page.** A page of the SHA-256 tree. **Slot.** One page of the cache's memory object. **Registration.** A buffer a block client registered once with a driver (SDD 25 d.3). **Mapping.** The read-only memory object `map` answers with. **Scratch directory.** A task's `/tmp`, in the server's memory, under a quota.

## c. Invariants

- **IF-I1.** A page is believed only after it matches its hash: a content page against its slot of a tree page, a tree page against its parent, the top tree page against the root in the header, and the header against its own checksum. A page that does not match is refused (`Corrupt`) and never cached (IF-T4, IF-T12).
- **IF-I2.** Nothing the reader takes from a layer can make it loop, panic or allocate without bound: every count is bounded (e.2), every range is checked against its table, a directory's children are later nodes than it, a file's extents cover exactly its pages, and every allocation a count names is made fallibly (IF-T4).
- **IF-I3.** The layers merged are SDD 19's unpacking of the same image: every path, kind, mode, file's bytes and link's target, apart from the mount points SDD 19 f.3 makes, which the server adds (IF-T2, IF-T13).
- **IF-I4.** Every file's data starts on a page boundary of its layer file, and every page of it that is not all zero is a whole page there (IF-T3).
- **IF-I5.** Nothing is read from a device until something touches it: mounting reads one header page a layer, and a read of a file reads that file's pages and the metadata and tree pages on the way to them that the cache does not hold (IF-T9, IF-T12).
- **IF-I6.** The page cache holds at most its capacity, evicts the least recently used page, and is shared: a page one task's read brought in is a hit for every other task (IF-T8, IF-T15).
- **IF-I7.** A cache miss pins nothing: the cache's memory object is registered once with each layer's driver, and every miss is a transfer into the registration (SDD 25 f.9).
- **IF-I8.** The only writable path is a task's scratch directory: a write, a creation, a truncation or a removal anywhere else is `ReadOnly`, and the image's own `/tmp` is not reachable (IF-T7, IF-T16).
- **IF-I9.** A scratch directory holds at most its quota, counting every file's bytes and a fixed amount an entry; a write past it changes nothing and is `NoSpace`. It is the connection's own and ends with it (IF-T11, IF-T16).
- **IF-I10.** A mapping is read only (its handle carries no `WRITE`), holds the file's bytes in its range and zeros past the file's end, and its pages are never written again once handed out, so a task's view of a file never changes (IF-T14).

## d. Interfaces

### d.1 `moruna.imagefs` 1.0 (`imagefs-protocol`)

```rust
1  => fn open(path: Bytes, flags: u32, mode: u32) -> (u32, Stat);
2  => fn stat(path: Bytes, follow: bool) -> Stat;
3  => fn fstat(file: u32) -> Stat;
4  => fn readdir(file: u32, cookie: u32) -> (Vec<(Bytes, u8, u64)>, u32);
5  => fn readlink(path: Bytes) -> Bytes;
6  => fn read(file: u32, offset: u64, len: u32) -> Bytes;
7  => fn map(file: u32, offset: u64, len: u64) -> Memory;
8  => fn write(file: u32, offset: u64, data: Bytes) -> u32;
9  => fn truncate(file: u32, size: u64) -> ();
10 => fn close(file: u32) -> ();
11 => fn mkdir(path: Bytes, mode: u32) -> ();
12 => fn remove(path: Bytes) -> ();
13 => fn quota(bytes: u64) -> ();
14 => fn stats() -> Stats;
errors { 1 NotFound; 2 NotDirectory; 3 IsDirectory; 4 ReadOnly; 5 NoSpace; 6 Exists;
         7 NotEmpty; 8 BadPath; 9 Loop; 10 BadFile; 11 BadRange; 12 TooLarge; 13 Corrupt;
         14 Io; 15 TooManyFiles; 16 Unsupported; }
```

A connection is one task. Paths are absolute byte strings of at most 4096 bytes without NUL. `open` flags: `WRITE` 1, `CREATE` 2, `EXCLUSIVE` 4, `TRUNCATE` 8, `DIRECTORY` 16, `NOFOLLOW` 32; an unknown flag is `Unsupported`; `WRITE`, `TRUNCATE` and `CREATE` of a new file reach only the scratch directory; `NOFOLLOW` on a symbolic link is `Loop`, as `O_NOFOLLOW` is. A file number is the connection's (at most 1024 open). `Stat` is `kind` (1 directory, 2 regular, 3 symbolic link), `mode` (permission bits), `size` (a link's target length), `ino` (unique over the image: layer and node; the scratch directory's and the mount points' in ranges of their own). `readdir` lists a directory as it was at its first `readdir` after `open`, in name order, at most 48 KiB of entries an answer; the next cookie, and no entries at the end. `read` answers at most `READ_MAX` (32 KiB), fewer at the end, none past it. `map`'s offset is on a page and its length 1 byte to 256 MiB, inside the file's pages; the memory object is read only, holds the file's bytes and zeros after its end; a scratch file is `Unsupported`. `quota` is accepted only before the scratch directory is first used, and at most 1 GiB; until then it is 64 MiB (SDD 19's `TMP_BYTES`). `Stats` counts the cache's hits, misses, evictions and refused pages, the pages read from the devices and the requests that read them, the cache's resident and total slots, and the mappings served again, read into, and copied from the cache.

`Bytes` is a `Vec<u8>` on the wire, moved as one run.

### d.2 `moruna-imagefs`

`format` (`Geometry::new(nodes, extents, names, data_pages)`, `Geometry::parent(page)`, `Header::{encode, parse}`, `Node::{encode, decode}`, `Extent::{encode, decode}`, `valid_name`, `valid_target`); `volume` (`trait Pages { fn read(layer, page, offset, out) }`, `Layer::{node, name, target, child, children, file, root}`, `FileMap::{pages, layer_page}`); `overlay` (`Overlay::{root, lookup, list, file, target}`, `Dir`, `Found`); `fs` (`Tree { overlay, scratch }::{resolve, parent, list, readlink}`, `Place`, `Stat`); `cache` (`trait Disk { fetch(&[Fetch]), copy(slot, offset, out), device_pages(layer) }`, `Cache::{new, mount, ensure, prefetch, expected, cached, stats}`, `impl Pages for Cache`); `map` (`plan(cache, file, first, count, max_run) -> Plan { copies, runs, holes }`); `scratch` (`Scratch::{create, mkdir, remove, open, close, read, write, truncate, list, info, set_quota}`); `sha256`.

### d.3 The writer (`moruna-image::layerfile`, host)

```rust
pub enum Entry { Directory { mode, opaque, children }, File { mode, data: Data, link: Option<u64> },
                 Symlink { target }, Whiteout }
pub fn write(root: &Entry, out: &Path) -> Result<LayerFile>;            // LayerFile { id, path, pages }
pub fn convert(source: &dyn ImageSource, reference: &str, out: &Path) -> Result<Converted>;
pub fn read(layers: &[PathBuf]) -> Result<BTreeMap<String, Item>>;    // the merged tree, host reader
```

`moruna-layer convert LAYOUT REFERENCE OUT` writes `<id>.layer` per layer, lowest first; `moruna-layer fixture OUT` (feature `testing`) writes the CI boot's disks and lists (k).

### d.4 The server's start (`imagefs`)

| slot | what |
|---|---|
| `console` | the debug console |
| `layers` | a memory object: `moruna-layers 1`, then `layer SERVICE SHA256` a line, lowest layer first |
| `directory` | the device manager's directory, for each `SERVICE` |
| `listen*` | listeners; every connection on any is a task |

The layer list's services are distinct, as are its ids; a device that is not read only, or whose sectors are not 512 bytes, is refused. The server drops its directory connection once every device is found.

## e. Data model, formats and state machines

### e.1 The layer file

Little endian; pages of 4096 bytes. In order:

| pages | region |
|---|---|
| 0 | the header (e.2) |
| `1 ..` | the node table: `nodes` entries of 64 bytes (e.3), padded to a page |
| then | the names: `names_len` bytes (every name and target, unterminated), padded |
| then | the extent table: `extents` entries of 16 bytes (e.3), padded |
| then | the data: `data_pages` pages, each file's runs from a page boundary |
| then | the tree: level 0 (the hash of every content page, 128 a page), level 1 (the hash of every level 0 page), and so on to a level of one page |

Every region's place follows from the four counts (`Geometry::new`), so nothing in the file says where a region is and no file can describe two overlapping. The layer id is the SHA-256 of the whole file.

### e.2 The header

| offset | field |
|---|---|
| 0 | `MRNLAYR1` |
| 8 | `u32` version, 1 |
| 12 | `u32` page size, 4096 |
| 16 | `u64` the file's pages |
| 24 | `u32` nodes (1 to 2^24) |
| 28 | `u32` extents (to 2^24) |
| 32 | `u64` bytes of names (to 2^30) |
| 40 | `u64` data pages (to 2^36) |
| 48 | `u32` tree levels |
| 52 | `u32` flags, 0 |
| 56 | the root: SHA-256 of the top tree page |
| 88 | SHA-256 of the page with these 32 bytes zero |
| 120 | zero to the end of the page |

A header is refused unless its checksum holds, its constants are these, its reserved bytes are zero, its counts are inside their bounds, and its length and depth are what the counts give.

### e.3 Nodes and extents

A node: `u64` name offset, `u16` name length, `u8` kind (1 directory, 2 regular, 3 symbolic link, 4 whiteout), `u8` flags (`OPAQUE` 1, a directory's only), `u32` mode (at most `0o777`), `u32` first and `u32` count (a directory's children; a file's extents), `u64` size (a file's), `u64` target offset and `u32` target length (a link's), zero to 64. Node 0 is the root, a directory with no name. The nodes are breadth first: each directory's children are consecutive, later than it, and in strictly increasing byte order of name. A name is 1 to 255 bytes with no `/` or NUL and is neither `.` nor `..`; a target is 1 to 4095 bytes with no NUL. Unused fields are zero.

An extent: `u64` first page (0 is a hole of zero pages, which reads nothing) and `u64` pages (at least one). A file's extents, in order, cover exactly `ceil(size / 4096)` pages; every extent that is not a hole lies inside the data region. Hard links share extents.

### e.4 The tree

Content page `p` (1 to the tree's first page, exclusive) has its hash at slot `(p - 1) mod 128` of level 0 page `(p - 1) / 128`; tree page `j` of level `l` has its hash at slot `j mod 128` of level `l + 1` page `j / 128`; the top level is one page, hashed to the root. A 100 MiB layer has three levels (201, 2 and 1 pages).

### e.5 The cache

A slot is free, or holds one verified page keyed by (layer, page), on a doubly linked recency list. A miss takes a free slot or the least recently used one (an eviction), reads the page into it, hashes it and files it at the front; a hit moves it to the front. The header is read into a slot at mount, copied out and the slot freed: headers are not cached.

### e.6 A mapping kept

The server keeps at most 256 mappings and 64 MiB of them, keyed by (layer, node, first page, pages), dropping the least recently used to make room; a mapping larger than 64 MiB is not kept. A task's handle to a mapping it was given stays valid whatever the server drops.

## f. Algorithms and policies

### f.1 Conversion

`convert` reads the manifest and config, then for each layer in order: applies it to a working root with SDD 19's own unpacking (`unpack::apply_layer`, f.2 there: the digest checked, parents resolved through the root's links, whiteouts and opaque markers applied, special bits dropped, devices skipped); takes a snapshot of the root (every path's kind, mode, inode and device); and writes the difference from the snapshot before:

- an entry for every path whose inode, device or kind changed, whose mode changed, or that lies under a directory marked opaque, with its parent directories (their current modes) as the entries that hold it;
- a directory marked opaque when it replaced something below (a new inode where the layers below had a path), or when every entry the layers below had in it is gone or replaced;
- a whiteout for every path the layers below had that is gone, whose parent is still there, unchanged and not opaque (a removed tree is one whiteout at its top).

Unpacking never rewrites a file in place (it removes and makes it), so a changed path is a new inode, and an unchanged one is the same inode. A file's bytes are read from the working root; a file of mode 0 is made readable for the read and given its mode back. After the last layer, SDD 19 f.3's mount point check runs, so an image SDD 19 refuses is refused here. The result is IF-I3 by construction: merging the differences top down is applying the layers.

### f.2 Writing

`write` numbers the tree breadth first, each directory's entries in name order; writes every file's pages, in node order and once for hard links, to a spill file, a run of zero pages becoming a hole; computes the geometry from the counts; writes a zero page, the node table, the names, the extent table (holes as 0, data runs at their page in the data region), the data, then each tree level from the hashes of the pages before it; writes the header last; and hashes the file for its id. The same tree writes the same bytes.

### f.3 The merge

A directory of the merged tree is the list of the layers' directories it is made of, top first. A name in it is looked for in each of them in turn; the first that has it decides: a whiteout hides it; a file or a link is it; a directory is it, merged with the same name's directory in the lower parts while each is a directory, stopping after one that is opaque or at one that is anything else. The root is every layer's root, stopping after an opaque one. A listing takes each name from the highest part that has it and leaves out whiteouts.

### f.4 Mapping

`map` checks the range, answers a kept mapping as a new read-only handle, or else: makes a DMA memory object of the range's pages; plans each page (`map::plan`): a hole is left zero, a page the cache holds is copied from its slot, the rest form runs of consecutive layer pages no longer than the device's largest transfer; takes the hash of every page of the runs from the tree (through the cache); registers the object with the layer's driver, reads the runs into it (up to 32 requests in flight), ends the registration; checks every page read against its hash; keeps the mapping and answers a handle without `WRITE`.

### f.5 Reading and the cost of a cold read

`read` maps the request to the file's pages, reads in one batch the pages the cache lacks (`prefetch`, which sorts them and merges consecutive pages that landed in consecutive slots into one request), and copies the bytes out. A miss costs the page and each tree page above it that is not cached. A cold read of one 4 KiB file from the top layer of the CI boot, a layer of 101 MiB (25,957 pages), cost 10 pages in 10 requests (IF-T12): in that layer, the node, names, extents and data pages and the three tree pages above them (7); in the layer below, its node page, its names page and the tree page above them (3), read because a directory's merge asks every lower layer whether it has the same directory (f.3), and shared by every later lookup. Mounting read the two headers. The same read over an 8 MiB layer on the host costs 8 (IF-T9).

### f.6 Paths

A path is split on `/`; empty parts and `.` are dropped. Each part is looked for from the directory on top of a stack: `..` pops it (the root stays), a directory is pushed, a symbolic link on the way (or at the end when following) has its target's parts put in front of the rest, from the root when absolute, at most 40 links. `tmp` in the root is the scratch directory; `proc` and `dev` in the root are empty directories when the image has none. A file followed by more parts is `NotDirectory`. `parent` resolves all but the last part, which must be a name.

### f.7 The scratch directory

Each connection has its own. An entry costs 256 bytes and its name; a file its bytes; a write that grows a file charges the growth first and is refused whole when it would pass the quota. A file removed while open keeps its bytes (and its charge) until its last close. There are no links and no renames.

### f.8 Registered buffers (block protocol 1.1)

SDD 25 d.3 and f.9: `register` pins a client's DMA memory object once and keeps the pin and each page's device address with the connection; `read_at` and `write_at` move sectors between the disk and pages of a registration and pin nothing; `unregister`, or the connection's end, drops the registration, whose pin goes to the quarantine once no transfer in flight holds it, and the quarantine is released when nothing is in flight. A connection holds at most 64 registrations.

## g. Concurrency within the component

The server is single threaded: one dispatcher over its listeners and connections, the cache and the kept mappings shared through `Rc<RefCell<_>>`. A request that misses waits for its device reads before the server turns again; the reads of one request are sent together (up to 32 in flight a device), so a request costs one round of device latency, not one a page. KC1's tasks are served one request at a time; a server per CPU or reads that do not block the dispatcher are m.3.

## h. Behaviour

The CI boot: the device manager starts `block1` and `block2` for the two read-only layer disks; `imagefs` looks them up, registers the cache with each, mounts both layers and prints `KC2 imagefs: up:`; `imagefs-test` runs IF-T12 to IF-T17 and exits; the server ends when its last client and listener have gone, printing its counts; the drivers end when it has.

## i. Configuration

None at run time. Constants: a cache of 4096 pages (16 MiB); kept mappings 256 and 64 MiB; 32 requests in flight a device; 60 s for a device's answer; `READ_MAX` 32 KiB; `MAP_MAX` 256 MiB; 1024 open files a connection; 48 KiB of directory entries an answer; scratch quota 64 MiB until set, at most 1 GiB, 256 bytes an entry; 40 links a path; 4096 bytes a path; 2^20 entries a directory; 64 registrations a block connection.

## j. Observability

The server prints its layers (service, the first twelve digits of each id, pages) and its cache's size when it is up; every page it refuses and every device failure, with the layer and page, when it happens; and, when it ends, its connections, hits, misses, pages read, requests, evictions and kept mappings. `stats` gives the same counts to any client.

## k. Tests

Host tests run with `cargo test -p moruna-imagefs` in `kernel/` (and in CI's kernel job), `cargo test -p moruna-image` at the root, and `cargo test -p moruna-image --features moruna-abi/manifest` in `kernel/` (the image tool). Boot tests run in the kernel job's boots on 1, 2 and 4 CPUs, judged by `check-boot.sh` against the description's lines. `kernel/tools/build.sh` writes the boot's disks with `moruna-layer fixture`: two layers (the lower: binaries, a hard link, `/etc`, a directory and an opaque directory's lower entries, a file that becomes a directory, a link `lib -> usr/lib`, a sparse file, a file of mode 0, and a file that becomes a directory; the upper: replacements, whiteouts of a file, a directory and a file below a link, an opaque directory, a file written through the lower layer's link, a link replacing a file, a 4 KiB `/cold/a.txt`, a 100 MiB `/data/big.bin` and eighty small files) and the list of SDD 19's unpacking of the same image, every file with its SHA-256.

| id | test | where | asserts |
|---|---|---|---|
| IF-T1 | `if_t1_*` | `moruna-image` | a tree written reads back as it was through the host reader, whiteout and opaque flag in the file, hard links sharing extents, zero pages holes; the id is the file's SHA-256; the same tree writes the same bytes; trees a layer cannot hold are refused and leave nothing |
| IF-T2 | `if_t2_*` | `moruna-image` | three layers of SDD 19's tests (IM-T2 to IM-T5's entries, links of a lower layer followed, hard links, modes of 0 and setuid, an empty layer, a whiteout of every root entry) converted and merged equal SDD 19's unpacking, path for path |
| IF-T3 | `if_t3_*` | `moruna-image` | sixty seeded random trees: every file reads back, and its first page that is not a hole is a whole page of the layer file's data region |
| IF-T4 | `if_t4_*` | `moruna-image`, `moruna-imagefs` | a byte of a layer file changed at a seeded place, 300 and 400 times, is refused (`Corrupt`), never a panic; a cut file, a missing one, and well hashed files that break the format (children out of order, a cycle, a mode with setuid, extents short of the file, a name with `/`, a target with NUL) are refused |
| IF-T5 | `if_t5_*` | `moruna-imagefs` | the layout from the counts, a 100 MiB layer's three levels and every page's parent; the header round trips, any flipped byte is refused, and a resealed header with a wrong constant is still refused; nodes and extents round trip and every refusal of e.3; file maps and holes |
| IF-T6 | `if_t6_*` | `moruna-imagefs` | SHA-256: the FIPS vectors, and pieces hash as the whole |
| IF-T7 | `if_t7_*` | `moruna-imagefs` | the merge: whiteouts of a file and a tree, an opaque directory, a file replaced by a directory, a lower file under a directory both layers have, an opaque root; paths: absolute and relative links, `..` taken physically, a loop, a dangling link, `/tmp` the scratch directory, `proc` and `dev`, bad paths |
| IF-T8 | `if_t8_*` | `moruna-imagefs` | a second read is all hits; the cache stays within its capacity, evicts the least recently used page and keeps the last; a prefetch is one batch and is not repeated; a device failure and a page off the tree are named |
| IF-T9 | `if_t9_*` | `moruna-imagefs` | a cold read of one 4 KiB file from an 8 MiB layer reads 8 pages after the two headers |
| IF-T10 | `if_t10_*` | `moruna-imagefs` | a mapping's plan: runs bounded by the largest transfer, a cached page copied, holes, bad ranges |
| IF-T11 | `if_t11_*` | `moruna-imagefs` | the scratch directory's files and directories, its quota (exact, fixed once used, a refused write changes nothing), a removed open file living until closed |
| IF-T12 | the cold start | boot: `imagefs-test` | `KC2 cold start: ok:`: `/cold/a.txt` read first from the 101 MiB layer, its bytes the host's, at most 10 pages read (10 measured, f.5) |
| IF-T13 | the tree | boot: `imagefs-test` | `KC2 tree: ok:`: every entry of the host's list stat'ed as listed, every directory listing the host's names (and `dev`, `proc`, `tmp` at the root), every file read whole with the host's SHA-256, every link's target the host's |
| IF-T14 | map | boot: `imagefs-test` | `KC2 map: ok:`: every file of at most 1 MiB, and 1 MiB from the middle of the 100 MiB file, mapped hold what `read` gives; the same pages mapped again are the kept mapping, with no page read |
| IF-T15 | the cache | boot: `imagefs-test` | `KC2 cache: ok:`: a second connection reads a file the first read with no block request and no page read, and at least a hit a page |
| IF-T16 | scratch and read only | boot: `imagefs-test` | `KC2 scratch: ok:`: `/tmp` refuses the write past a 64 KiB quota with `NoSpace` and keeps its length; another connection does not see the file; opening an image file to write or truncate, making a file or a directory in the image, removing an image file and removing `/tmp` are each `ReadOnly`, and the file reads as before |
| IF-T17 | the suite | boot: `imagefs-test` | `KC2 conformance moruna.imagefs: ok:`: SV-T7's shared checks and the protocol's own (the root's listing, bad paths and file numbers, the image read only, the scratch directory's life, the quota, `map` against `read` and its refusals, unknown flags, `stats`) |
| IF-T18 | `if_t18_*` | `tools/image` | `data:` handles carry a file of the build's data directory once at `data/PATH`; paths that could leave it, and a file that is not there, are refused |
| IF-T19 | registered buffers | boot: `block-test` | DR-T10's suite (block 1.1) against `block0`: `read_at` into a registered buffer's third page holds what `read` gives; every refusal (not DMA, no `WRITE`, no registration, past the registration, bad count, past the disk, read only, after `unregister`); a sector written from one registration reads back through another; at most 64 registrations |

Coverage on the host (`cargo llvm-cov`, lines): see m.4.

## l. Implementation notes for the agent

- A cache slot and a mapping are read and written eight bytes an access (`Mapping::read64`, `write64`), since `Mapping::read` moves one byte a volatile access.
- `moruna-image` reaches `moruna-imagefs` by path (`kernel/imagefs/fs`), so the writer and the kernel share one format module; the root workspace builds it on its own toolchain.
- `build.sh` runs the root workspace's `moruna-layer` before the image tool, which reads the lists through `data:` handles from the build's output directory.

## m. Open items

1. **The kernel call the brief named.** KC2's brief asked for the long-lived registered pin SDD 25 o listed as a kernel call. It needs none: `memory_pin` already makes a pin its holder may keep as long as it likes, and the quarantine already holds a closed pin until its device is done. What was missing was protocol: a client registering a buffer once, and the driver keeping the pin with the connection and moving sectors into any of its pages (block protocol 1.1, f.8). SDD 20's table is unchanged; this item records the choice for review.
2. **A mapping is filled when it is made.** `map` reads its whole range before it answers, because the kernel has no pager: a page fault in a task ends the task (SDD 20 KK-I10). A mapping that pages in as it is touched needs the kernel to send a fault on a memory object to a server and wait for the page; until then KC1 should map the ranges a task touches (a loader maps segments, not whole files).
3. **One request at a time.** The server waits for its devices inside a request (g).
4. **Coverage.** `moruna-imagefs` and the writer are measured with `cargo llvm-cov` in the build container; the figures are in KC2's report.

## n. Traceability

| requirement (BOARD EK KC2 brief) | here |
|---|---|
| the layer format: header, sorted directory index, extent table, page-aligned data; content addressed | e.1 to e.4; `kernel/imagefs/fs/src/format.rs`; IF-T1, IF-T3, IF-T5 |
| the writer, host side, in `moruna-image`, from the unpacked OCI layer | d.3, f.1, f.2; `crates/moruna-image/src/layerfile.rs`; IF-T1, IF-T2 |
| the reader and server in the kernel, `no_std`, safe, over a published protocol | d.1, d.2; `kernel/imagefs`; `forbid(unsafe_code)` in every crate |
| layers as read-only virtio-blk devices, an ordered list from the start handles | d.4; `boot.sh`, `boot.image` |
| the merge, whiteouts and opaque directories exactly as SDD 19 | f.1, f.3; IF-I3; IF-T2, IF-T7, IF-T13 |
| paging on demand; a shared, bounded, LRU page cache keyed by layer and offset | e.5, f.5; IF-I5, IF-I6; IF-T8, IF-T9, IF-T12, IF-T15 |
| the long-lived registered pin | f.8, m.1; IF-I7; IF-T19 |
| the scratch directory, per task, with a quota | f.7; IF-I8, IF-I9; IF-T11, IF-T16 |
| `map` over a file's pages | f.4, m.2; IF-I10; IF-T10, IF-T14 |
| a corrupted layer refused, never a panic | IF-I1, IF-I2; IF-T4 |
| the conformance suite through KB2's harness | IF-T17 |

## o. Deferred

**IF-O1. A pager** (m.2): mappings that page in on touch. **IF-O2. Concurrent requests** (m.3). **IF-O3. Layers shared across images**: the server serves one image's layers; several images over shared layers is one server with a list per image, keyed as the cache already is. **IF-O4. Links and renames in the scratch directory**, which KC1 may need for some programs. **IF-O5. The layer id checked**: the server takes the id with the device and checks every page against the header's root, not the header against the id; making the id the hash of the header page would let the server check a layer end to end without reading it.
