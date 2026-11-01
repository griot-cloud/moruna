//! Host tests of the reader, the merge, the paths and the cache over layer files a
//! small builder here writes (the real writer is `moruna-image`'s, SDD 27 f.1, whose
//! own tests read its files back with this crate). The builder also writes files the
//! writer never would, to prove the reader refuses them.

use alloc::{collections::VecDeque, string::String, vec, vec::Vec};

use crate::cache::{Cache, Disk, Fetch};
use crate::error::{Error, Result};
use crate::format::{
    EXTENT_BYTES, Extent, FANOUT, Geometry, Header, Kind, NODE_BYTES, Node, OPAQUE, PAGE, PAGE_U64,
};
use crate::fs::{Place, Tree};
use crate::map;
use crate::overlay::{Found, Overlay};
use crate::scratch::Scratch;
use crate::sha256;
use crate::volume::Pages;

pub enum T {
    Dir(u32, bool, Vec<(&'static str, T)>),
    File(u32, Vec<u8>),
    Link(&'static str),
    Wh,
}

fn dir(children: Vec<(&'static str, T)>) -> T {
    T::Dir(0o755, false, children)
}

fn file(data: &[u8]) -> T {
    T::File(0o644, data.to_vec())
}

/// A layer file of `root`, laid out as SDD 27 e.1 says; `tamper` may change the
/// node table before the hashes are taken, so the file is well hashed and malformed.
fn build_with(root: &T, tamper: impl Fn(&mut Vec<Node>)) -> Vec<u8> {
    let mut nodes: Vec<Node> = vec![blank(Kind::Directory)];
    let mut names: Vec<u8> = Vec::new();
    let mut extents: Vec<Extent> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let mut queue: VecDeque<(usize, &T)> = VecDeque::from([(0, root)]);
    while let Some((at, t)) = queue.pop_front() {
        match t {
            T::Dir(mode, opaque, children) => {
                let mut sorted: Vec<&(&str, T)> = children.iter().collect();
                sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                nodes[at].mode = *mode;
                nodes[at].flags = if *opaque { OPAQUE } else { 0 };
                nodes[at].count = sorted.len() as u32;
                nodes[at].first = if sorted.is_empty() {
                    0
                } else {
                    nodes.len() as u32
                };
                for (name, child) in sorted {
                    let mut n = blank(Kind::Directory);
                    n.name_offset = names.len() as u64;
                    n.name_len = name.len() as u16;
                    names.extend_from_slice(name.as_bytes());
                    nodes.push(n);
                    queue.push_back((nodes.len() - 1, child));
                }
            }
            T::File(mode, bytes) => {
                let n = &mut nodes[at];
                n.kind = Kind::Regular;
                n.mode = *mode;
                n.size = bytes.len() as u64;
                n.first = if bytes.is_empty() {
                    0
                } else {
                    extents.len() as u32
                };
                let mut count = 0;
                for chunk in bytes.chunks(PAGE) {
                    let mut page = [0u8; PAGE];
                    page[..chunk.len()].copy_from_slice(chunk);
                    let zero = page.iter().all(|b| *b == 0);
                    let start = if zero {
                        0
                    } else {
                        1 + (data.len() / PAGE) as u64
                    };
                    match extents.last_mut() {
                        Some(e)
                            if count > 0
                                && (e.start == 0) == zero
                                && (zero || e.start + e.pages == start) =>
                        {
                            e.pages += 1
                        }
                        _ => {
                            extents.push(Extent { start, pages: 1 });
                            count += 1;
                        }
                    }
                    if !zero {
                        data.extend_from_slice(&page);
                    }
                }
                n.count = count;
            }
            T::Link(target) => {
                let n = &mut nodes[at];
                n.kind = Kind::Symlink;
                n.mode = 0o777;
                n.target_offset = names.len() as u64;
                n.target_len = target.len() as u32;
                names.extend_from_slice(target.as_bytes());
            }
            T::Wh => {
                nodes[at].kind = Kind::Whiteout;
                nodes[at].mode = 0;
            }
        }
    }
    tamper(&mut nodes);
    let g = Geometry::new(
        nodes.len() as u32,
        extents.len() as u32,
        names.len() as u64,
        (data.len() / PAGE) as u64,
    )
    .unwrap();
    let mut out = vec![0u8; g.pages as usize * PAGE];
    for (i, n) in nodes.iter().enumerate() {
        let at = g.nodes_page as usize * PAGE + i * NODE_BYTES;
        out[at..at + NODE_BYTES].copy_from_slice(&n.encode());
    }
    let at = g.names_page as usize * PAGE;
    out[at..at + names.len()].copy_from_slice(&names);
    for (i, e) in extents.iter().enumerate() {
        let fixed = Extent {
            start: if e.start == 0 {
                0
            } else {
                e.start - 1 + g.data_page
            },
            pages: e.pages,
        };
        let at = g.extents_page as usize * PAGE + i * EXTENT_BYTES;
        out[at..at + EXTENT_BYTES].copy_from_slice(&fixed.encode());
    }
    let at = g.data_page as usize * PAGE;
    out[at..at + data.len()].copy_from_slice(&data);
    let mut covered = (1..g.tree_page).collect::<Vec<u64>>();
    for l in 0..g.levels {
        let (first, _) = g.level[l];
        for (i, page) in covered.iter().enumerate() {
            let h = sha256::digest(&out[*page as usize * PAGE..(*page as usize + 1) * PAGE]);
            let at = (first as usize + i / FANOUT as usize) * PAGE + (i % FANOUT as usize) * 32;
            out[at..at + 32].copy_from_slice(&h);
        }
        covered = (first..first + g.level[l].1).collect();
    }
    let top = g.top() as usize;
    let root_hash = sha256::digest(&out[top * PAGE..(top + 1) * PAGE]);
    let header = Header {
        node_count: g.node_count,
        extent_count: g.extent_count,
        names_len: g.names_len,
        data_pages: g.data_pages,
        root: root_hash,
    };
    out[..PAGE].copy_from_slice(&header.encode().unwrap());
    out
}

fn build(root: &T) -> Vec<u8> {
    build_with(root, |_| {})
}

fn blank(kind: Kind) -> Node {
    Node {
        name_offset: 0,
        name_len: 0,
        kind,
        flags: 0,
        mode: 0,
        first: 0,
        count: 0,
        size: 0,
        target_offset: 0,
        target_len: 0,
    }
}

struct TestDisk {
    layers: Vec<Vec<u8>>,
    slots: Vec<[u8; PAGE]>,
    batches: Vec<Vec<Fetch>>,
    fail: bool,
}

impl TestDisk {
    fn new(layers: Vec<Vec<u8>>, slots: usize) -> TestDisk {
        TestDisk {
            layers,
            slots: vec![[0; PAGE]; slots],
            batches: Vec::new(),
            fail: false,
        }
    }

    fn pages_read(&self) -> usize {
        self.batches.iter().map(Vec::len).sum()
    }
}

impl Disk for TestDisk {
    fn fetch(&mut self, reads: &[Fetch]) -> Result<()> {
        if self.fail {
            return Err(Error::Io(String::from("the test disk fails")));
        }
        for r in reads {
            let at = r.page as usize * PAGE;
            let layer = &self.layers[r.layer];
            if at + PAGE > layer.len() {
                return Err(Error::Io(String::from("past the end")));
            }
            self.slots[r.slot as usize].copy_from_slice(&layer[at..at + PAGE]);
        }
        self.batches.push(reads.to_vec());
        Ok(())
    }

    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> Result<()> {
        out.copy_from_slice(&self.slots[slot as usize][offset..offset + out.len()]);
        Ok(())
    }

    fn device_pages(&mut self, layer: usize) -> Result<u64> {
        Ok((self.layers[layer].len() / PAGE) as u64)
    }
}

fn mount(layers: Vec<Vec<u8>>, slots: u32) -> (Cache<TestDisk>, Overlay) {
    let n = layers.len();
    let mut cache = Cache::new(TestDisk::new(layers, slots as usize), slots).unwrap();
    for _ in 0..n {
        cache.mount().unwrap();
    }
    let overlay = Overlay::new(cache.layers().to_vec());
    (cache, overlay)
}

fn read_all(cache: &mut Cache<TestDisk>, overlay: &Overlay, found: &Found) -> Result<Vec<u8>> {
    let Found::File { layer, node, .. } = found else {
        return Err(Error::IsDirectory);
    };
    let map = overlay.file(cache, *layer, node)?;
    let mut out = Vec::new();
    for page in 0..map.pages() {
        let mut buf = [0u8; PAGE];
        if let Some(lp) = map.layer_page(page).unwrap() {
            cache.read(*layer, lp, 0, &mut buf)?;
        }
        out.extend_from_slice(&buf);
    }
    out.truncate(map.size as usize);
    Ok(out)
}

fn lower() -> Vec<u8> {
    build(&dir(vec![
        (
            "bin",
            dir(vec![("sh", file(b"#!shell")), ("ls", file(b"ls"))]),
        ),
        (
            "etc",
            dir(vec![("passwd", file(b"root:x:0:0")), ("old", file(b"old"))]),
        ),
        (
            "gone",
            dir(vec![("a", file(b"a")), ("b", dir(vec![("c", file(b"c"))]))]),
        ),
        ("opq", dir(vec![("lower", file(b"lower"))])),
        ("replaced", file(b"a file below")),
        ("lib", T::Link("usr/lib")),
        (
            "usr",
            dir(vec![("lib", dir(vec![("libc.so", file(&[9u8; 9000]))]))]),
        ),
        (
            "tmp",
            dir(vec![("left", file(b"hidden by the scratch directory"))]),
        ),
    ]))
}

fn upper() -> Vec<u8> {
    let mut big = vec![0u8; 3 * PAGE];
    big.extend((0..2 * PAGE).map(|i| (i % 251) as u8 + 1));
    big.extend(vec![0u8; 100]);
    build(&dir(vec![
        (
            "etc",
            dir(vec![
                ("passwd", file(b"root:x:0:0:new")),
                ("old", T::Wh),
                ("new", file(b"n")),
            ]),
        ),
        ("gone", T::Wh),
        ("opq", T::Dir(0o700, true, vec![("upper", file(b"upper"))])),
        ("replaced", dir(vec![("inside", file(b"now a directory"))])),
        ("sparse", T::File(0o600, big)),
        ("empty", file(b"")),
        ("abs", T::Link("/etc/passwd")),
        ("up", T::Link("../etc/./passwd")),
        ("loop", T::Link("loop")),
        ("into-tmp", T::Link("/tmp")),
        ("dangling", T::Link("nowhere")),
        ("bin", dir(vec![("ls", T::Link("sh"))])),
    ]))
}

fn names(list: &[(Vec<u8>, Kind, u64)]) -> Vec<String> {
    list.iter()
        .map(|(n, _, _)| String::from_utf8(n.clone()).unwrap())
        .collect()
}

#[test]
fn if_t7_layers_merge_with_whiteouts_and_opaque_directories() {
    let (mut cache, overlay) = mount(vec![lower(), upper()], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let root = tree.resolve(&mut cache, b"/", true).unwrap();
    assert_eq!(
        names(&tree.list(&mut cache, &root).unwrap()),
        [
            "abs", "bin", "dangling", "dev", "empty", "etc", "into-tmp", "lib", "loop", "opq",
            "proc", "replaced", "sparse", "tmp", "up", "usr"
        ]
    );
    let etc = tree.resolve(&mut cache, b"/etc", true).unwrap();
    assert_eq!(
        names(&tree.list(&mut cache, &etc).unwrap()),
        ["new", "passwd"]
    );
    let passwd = tree.resolve(&mut cache, b"/etc/passwd", true).unwrap();
    let Place::Image(found) = &passwd else {
        panic!()
    };
    assert_eq!(
        read_all(&mut cache, &overlay, found).unwrap(),
        b"root:x:0:0:new"
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/etc/old", true),
        Err(Error::NotFound)
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/gone/b/c", true),
        Err(Error::NotFound)
    );
    let opq = tree.resolve(&mut cache, b"/opq", true).unwrap();
    assert_eq!(names(&tree.list(&mut cache, &opq).unwrap()), ["upper"]);
    assert_eq!(opq.stat(&scratch).unwrap().mode, 0o700);
    let replaced = tree.resolve(&mut cache, b"/replaced", true).unwrap();
    assert!(replaced.is_directory(&scratch));
    assert_eq!(
        names(&tree.list(&mut cache, &replaced).unwrap()),
        ["inside"]
    );
    let bin = tree.resolve(&mut cache, b"/bin", true).unwrap();
    assert_eq!(names(&tree.list(&mut cache, &bin).unwrap()), ["ls", "sh"]);
    let ls = tree.resolve(&mut cache, b"/bin/ls", false).unwrap();
    assert_eq!(ls.stat(&scratch).unwrap().kind, Kind::Symlink);
    assert_eq!(ls.stat(&scratch).unwrap().size, 2);
    let Place::Image(sh) = tree.resolve(&mut cache, b"/bin/ls", true).unwrap() else {
        panic!()
    };
    assert_eq!(read_all(&mut cache, &overlay, &sh).unwrap(), b"#!shell");
    // A lower file under a directory both layers have is merged in.
    let Place::Image(libc) = tree.resolve(&mut cache, b"/lib/libc.so", true).unwrap() else {
        panic!()
    };
    assert_eq!(
        read_all(&mut cache, &overlay, &libc).unwrap(),
        vec![9u8; 9000]
    );
    // The image's own /tmp is the scratch directory's.
    let tmp = tree.resolve(&mut cache, b"/tmp", true).unwrap();
    assert_eq!(tmp, Place::Scratch(0));
    assert!(tree.list(&mut cache, &tmp).unwrap().is_empty());
    assert_eq!(
        tree.resolve(&mut cache, b"/tmp/left", true),
        Err(Error::NotFound)
    );
    let dev = tree.resolve(&mut cache, b"/dev", true).unwrap();
    assert!(dev.is_directory(&scratch));
    assert!(tree.list(&mut cache, &dev).unwrap().is_empty());
    assert_eq!(dev.stat(&scratch).unwrap().kind, Kind::Directory);
    assert_eq!(
        tree.resolve(&mut cache, b"/dev/null", true),
        Err(Error::NotFound)
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/bin/dev", true),
        Err(Error::NotFound)
    );
}

#[test]
fn if_t7_an_opaque_root_hides_every_lower_name() {
    let top = build(&T::Dir(0o755, true, vec![("only", file(b"x"))]));
    let (mut cache, overlay) = mount(vec![lower(), top], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let root = tree.resolve(&mut cache, b"/", true).unwrap();
    assert_eq!(
        names(&tree.list(&mut cache, &root).unwrap()),
        ["dev", "only", "proc", "tmp"]
    );
}

#[test]
fn if_t7_paths_resolve_as_linux_resolves_them() {
    let (mut cache, overlay) = mount(vec![lower(), upper()], 64);
    let mut scratch = Scratch::new();
    let d = scratch.mkdir(0, b"d", 0o755).unwrap();
    let f = scratch.create(d, b"f", 0o644).unwrap();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let passwd = tree.resolve(&mut cache, b"/etc/passwd", true).unwrap();
    for path in [
        &b"/abs"[..],
        b"/up",
        b"//etc///passwd",
        b"/./etc/../etc/passwd",
        b"/../../etc/passwd",
        b"/bin/../up",
    ] {
        assert_eq!(
            tree.resolve(&mut cache, path, true).unwrap(),
            passwd,
            "{path:?}"
        );
    }
    assert_eq!(tree.resolve(&mut cache, b"/loop", true), Err(Error::Loop));
    assert_eq!(
        tree.resolve(&mut cache, b"/dangling", true),
        Err(Error::NotFound)
    );
    assert!(matches!(
        tree.resolve(&mut cache, b"/dangling", false),
        Ok(Place::Image(Found::Symlink { .. }))
    ));
    assert_eq!(
        tree.resolve(&mut cache, b"/etc/passwd/x", true),
        Err(Error::NotDirectory)
    );
    assert_eq!(tree.resolve(&mut cache, b"etc", true), Err(Error::BadPath));
    assert_eq!(tree.resolve(&mut cache, b"", true), Err(Error::BadPath));
    assert_eq!(
        tree.resolve(&mut cache, b"/a\0b", true),
        Err(Error::BadPath)
    );
    assert_eq!(
        tree.resolve(&mut cache, &[b'/'; 5000], true),
        Err(Error::BadPath)
    );
    let long = [&b"/"[..], &[b'x'; 300]].concat();
    assert_eq!(tree.resolve(&mut cache, &long, true), Err(Error::BadPath));
    // Through a link into the scratch directory, and back out of it.
    assert_eq!(
        tree.resolve(&mut cache, b"/into-tmp/d/f", true),
        Ok(Place::Scratch(f))
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/tmp/d", true),
        Ok(Place::Scratch(d))
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/tmp/d/../../etc/passwd", true)
            .unwrap(),
        passwd
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/tmp/d/f/g", true),
        Err(Error::NotDirectory)
    );
    assert_eq!(
        tree.resolve(&mut cache, b"/tmp/nope", true),
        Err(Error::NotFound)
    );
    let list = tree.list(&mut cache, &Place::Scratch(d)).unwrap();
    assert_eq!(names(&list), ["f"]);
    assert_eq!(tree.list(&mut cache, &passwd), Err(Error::NotDirectory));
    let (parent, name) = tree.parent(&mut cache, b"/tmp/d/new").unwrap();
    assert_eq!((parent, name.as_slice()), (Place::Scratch(d), &b"new"[..]));
    let (parent, _) = tree.parent(&mut cache, b"/etc/new").unwrap();
    assert!(matches!(parent, Place::Image(Found::Directory(_))));
    assert_eq!(
        tree.parent(&mut cache, b"/etc/passwd/x").map(|_| ()),
        Err(Error::NotDirectory)
    );
    assert_eq!(
        tree.parent(&mut cache, b"/").map(|_| ()),
        Err(Error::Exists)
    );
    assert_eq!(
        tree.parent(&mut cache, b"/etc/..").map(|_| ()),
        Err(Error::BadPath)
    );
    assert_eq!(tree.readlink(&mut cache, b"/abs").unwrap(), b"/etc/passwd");
    assert_eq!(
        tree.readlink(&mut cache, b"/etc/passwd"),
        Err(Error::BadPath)
    );
    assert_eq!(
        Place::Scratch(f).stat(&scratch).unwrap().kind,
        Kind::Regular
    );
    assert!(!Place::Scratch(f).is_directory(&scratch));
    assert!(crate::fs::image_file(&passwd).is_some());
    assert!(crate::fs::image_file(&Place::Scratch(f)).is_none());
}

#[test]
fn if_t5_holes_read_as_zeros_and_read_nothing() {
    let (mut cache, overlay) = mount(vec![lower(), upper()], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(found) = tree.resolve(&mut cache, b"/sparse", true).unwrap() else {
        panic!()
    };
    let Found::File { layer, node, .. } = &found else {
        panic!()
    };
    let map = overlay.file(&mut cache, *layer, node).unwrap();
    assert_eq!(map.pages(), 6);
    assert_eq!(map.layer_page(0), Some(None));
    assert_eq!(map.layer_page(5), Some(None));
    let before = cache.disk().pages_read();
    let data = read_all(&mut cache, &overlay, &found).unwrap();
    assert_eq!(data.len(), 5 * PAGE + 100);
    assert!(data[..3 * PAGE].iter().all(|b| *b == 0));
    assert_eq!(data[3 * PAGE], 1);
    // Two data pages, and the tree page above them only if it was not cached.
    assert!(cache.disk().pages_read() - before <= 3);
    let Place::Image(empty) = tree.resolve(&mut cache, b"/empty", true).unwrap() else {
        panic!()
    };
    assert_eq!(read_all(&mut cache, &overlay, &empty).unwrap(), b"");
    let stat = Place::Image(found).stat(&scratch).unwrap();
    assert_eq!(
        (stat.kind, stat.mode, stat.size),
        (Kind::Regular, 0o600, 5 * 4096 + 100)
    );
}

/// A layer whose one small file is followed by a large one.
fn cold_layer(big_pages: usize) -> Vec<u8> {
    let small: Vec<u8> = (0..PAGE).map(|i| (i * 13 % 256) as u8 | 1).collect();
    let big: Vec<u8> = (0..big_pages * PAGE).map(|i| (i % 253) as u8 | 1).collect();
    build(&dir(vec![
        ("cold", dir(vec![("a.txt", T::File(0o644, small))])),
        ("data", dir(vec![("big.bin", T::File(0o644, big))])),
    ]))
}

#[test]
fn if_t9_a_cold_read_of_one_page_reads_a_few() {
    // 8 MiB of data: two tree levels.
    let layer = cold_layer(2048);
    let (mut cache, overlay) = mount(vec![build(&dir(vec![])), layer], 256);
    let mounted = cache.disk().pages_read();
    assert_eq!(mounted, 2, "a header a layer");
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(found) = tree.resolve(&mut cache, b"/cold/a.txt", true).unwrap() else {
        panic!()
    };
    let data = read_all(&mut cache, &overlay, &found).unwrap();
    assert_eq!(data.len(), PAGE);
    let pages = cache.disk().pages_read() - mounted;
    // In the layer that holds it: the node page, the names, the extents and the data
    // page, and the two tree pages above them; in the layer below, its root's node
    // page and the tree page above it, which every later lookup shares.
    assert_eq!(pages, 8, "{pages} pages read");
    assert_eq!(cache.stats().pages_read, (mounted + pages) as u64);
}

#[test]
fn if_t8_a_second_read_is_served_from_the_cache() {
    let (mut cache, overlay) = mount(vec![lower(), upper()], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(found) = tree.resolve(&mut cache, b"/lib/libc.so", true).unwrap() else {
        panic!()
    };
    read_all(&mut cache, &overlay, &found).unwrap();
    let (reads, hits) = (cache.disk().pages_read(), cache.stats().hits);
    read_all(&mut cache, &overlay, &found).unwrap();
    assert_eq!(cache.disk().pages_read(), reads, "no page read again");
    assert!(cache.stats().hits > hits);
}

#[test]
fn if_t8_the_cache_is_bounded_and_evicts_the_least_recently_used() {
    let layer = cold_layer(64);
    let (mut cache, overlay) = mount(vec![layer], 16);
    assert_eq!(cache.capacity(), 16);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(found) = tree.resolve(&mut cache, b"/data/big.bin", true).unwrap() else {
        panic!()
    };
    let data = read_all(&mut cache, &overlay, &found).unwrap();
    assert_eq!(data.len(), 64 * PAGE);
    assert!(cache.resident() <= 16);
    assert!(cache.stats().evictions > 0);
    // The page touched last is cached; the first data page is not.
    let Found::File { layer, node, .. } = &found else {
        panic!()
    };
    let map = overlay.file(&mut cache, *layer, node).unwrap();
    let last = map.layer_page(63).unwrap().unwrap();
    let first = map.layer_page(0).unwrap().unwrap();
    assert!(cache.cached(0, last).is_some());
    assert!(cache.cached(0, first).is_none());
    let hits = cache.stats().hits;
    let mut b = [0u8; 1];
    cache.read(0, last, 0, &mut b).unwrap();
    assert_eq!(cache.stats().hits, hits + 1);
    assert!(Cache::new(TestDisk::new(vec![], 1), 15).is_err());
}

#[test]
fn if_t8_a_prefetch_is_one_batch() {
    let layer = cold_layer(40);
    let (mut cache, overlay) = mount(vec![layer], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(Found::File { layer, node, .. }) =
        tree.resolve(&mut cache, b"/data/big.bin", true).unwrap()
    else {
        panic!()
    };
    let map = overlay.file(&mut cache, layer, &node).unwrap();
    let pages: Vec<u64> = (0..8)
        .map(|p| map.layer_page(p).unwrap().unwrap())
        .collect();
    let batches = cache.disk().batches.len();
    cache.prefetch(layer, &pages).unwrap();
    assert_eq!(cache.disk().batches.len(), batches + 1);
    assert_eq!(cache.disk().batches.last().unwrap().len(), 8);
    cache.prefetch(layer, &pages).unwrap();
    assert_eq!(
        cache.disk().batches.len(),
        batches + 1,
        "nothing to read twice"
    );
    // Larger than half the cache: read in parts.
    let all: Vec<u64> = (8..40)
        .map(|p| map.layer_page(p).unwrap().unwrap())
        .collect();
    cache.prefetch(layer, &all).unwrap();
    assert!(cache.disk().batches.len() >= batches + 2);
    cache.disk().fail = true;
    let more: Vec<u64> = (0..8)
        .map(|p| map.layer_page(p).unwrap().unwrap())
        .collect();
    let evicted: Vec<u64> = more
        .into_iter()
        .filter(|p| cache.cached(0, *p).is_none())
        .collect();
    if !evicted.is_empty() {
        assert!(matches!(cache.prefetch(layer, &evicted), Err(Error::Io(_))));
    }
}

#[test]
fn if_t10_a_map_reads_runs_and_copies_what_is_cached() {
    let layer = cold_layer(10);
    let (mut cache, overlay) = mount(vec![layer], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(Found::File { layer, node, .. }) =
        tree.resolve(&mut cache, b"/data/big.bin", true).unwrap()
    else {
        panic!()
    };
    let file = overlay.file(&mut cache, layer, &node).unwrap();
    let p = map::plan(&cache, &file, 0, 10, 4).unwrap();
    assert_eq!(p.copies, vec![]);
    let first = file.layer_page(0).unwrap().unwrap();
    assert_eq!(
        p.runs,
        vec![
            map::Run {
                at: 0,
                layer_page: first,
                pages: 4
            },
            map::Run {
                at: 4,
                layer_page: first + 4,
                pages: 4
            },
            map::Run {
                at: 8,
                layer_page: first + 8,
                pages: 2
            },
        ]
    );
    let mut b = [0u8; 1];
    cache.read(layer, first + 5, 0, &mut b).unwrap();
    let p = map::plan(&cache, &file, 2, 6, 64).unwrap();
    assert_eq!(p.copies, vec![(3, first + 5)]);
    assert_eq!(p.runs.len(), 2);
    assert_eq!(map::plan(&cache, &file, 0, 11, 4), Err(Error::BadRange));
    assert_eq!(map::plan(&cache, &file, 0, 0, 4), Err(Error::BadRange));
    assert_eq!(
        map::plan(&cache, &file, u64::MAX, 2, 4),
        Err(Error::BadRange)
    );
    assert_eq!(map::plan(&cache, &file, 0, 1, 0), Err(Error::BadRange));
    let Place::Image(Found::File { layer, node, .. }) =
        tree.resolve(&mut cache, b"/cold/a.txt", true).unwrap()
    else {
        panic!()
    };
    let small = overlay.file(&mut cache, layer, &node).unwrap();
    assert_eq!(map::plan(&cache, &small, 0, 1, 4).unwrap().runs.len(), 1);
    let (mut cache, overlay) = mount(vec![upper()], 64);
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let Place::Image(Found::File { layer, node, .. }) =
        tree.resolve(&mut cache, b"/sparse", true).unwrap()
    else {
        panic!()
    };
    let sparse = overlay.file(&mut cache, layer, &node).unwrap();
    let p = map::plan(&cache, &sparse, 0, 6, 8).unwrap();
    assert_eq!((p.holes, p.runs.len(), p.runs[0].pages), (4, 1, 2));
}

/// A seeded generator for corruption, so a failure names the case that made it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Mounts and reads everything; any refusal is an error, never a panic.
fn walk_everything(layers: Vec<Vec<u8>>) -> Result<usize> {
    let n = layers.len();
    let mut cache = Cache::new(TestDisk::new(layers, 64), 64)?;
    for _ in 0..n {
        cache.mount()?;
    }
    let overlay = Overlay::new(cache.layers().to_vec());
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let mut seen = 0;
    let mut todo = vec![b"/".to_vec()];
    while let Some(path) = todo.pop() {
        let place = tree.resolve(&mut cache, &path, false)?;
        match &place {
            Place::Image(Found::Directory(_)) => {
                for (name, _, _) in tree.list(&mut cache, &place)? {
                    let mut child = path.clone();
                    if child.len() > 1 {
                        child.push(b'/');
                    }
                    child.extend_from_slice(&name);
                    todo.push(child);
                }
            }
            Place::Image(found @ Found::File { .. }) => {
                read_all(&mut cache, &overlay, found)?;
            }
            Place::Image(Found::Symlink { .. }) => {
                tree.readlink(&mut cache, &path)?;
            }
            _ => {}
        }
        seen += 1;
    }
    Ok(seen)
}

/// Mounts one layer alone and reads every node, name, target and file page it has,
/// whiteouts and what the scratch directory would hide included.
fn walk_layer(bytes: Vec<u8>) -> Result<usize> {
    let mut cache = Cache::new(TestDisk::new(vec![bytes], 64), 64)?;
    cache.mount()?;
    let layer = cache.layers()[0].clone();
    let mut seen = 0;
    let mut todo = vec![layer.root(&mut cache)?];
    while let Some(dir) = todo.pop() {
        for (_, _, node) in layer.children(&mut cache, &dir)? {
            seen += 1;
            match node.kind {
                Kind::Directory => todo.push(node),
                Kind::Regular => {
                    let map = layer.file(&mut cache, &node)?;
                    for page in 0..map.pages() {
                        if let Some(Some(lp)) = map.layer_page(page) {
                            let mut b = [0u8; PAGE];
                            cache.read(0, lp, 0, &mut b)?;
                        }
                    }
                }
                Kind::Symlink => {
                    layer.target(&mut cache, &node)?;
                }
                Kind::Whiteout => {}
            }
        }
    }
    Ok(seen)
}

#[test]
fn if_t4_every_flipped_byte_is_refused() {
    let good = vec![lower(), upper()];
    assert!(walk_everything(good.clone()).unwrap() > 20);
    for layer in &good {
        assert!(walk_layer(layer.clone()).unwrap() > 10);
    }
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    for case in 0..400 {
        let which = (rng.next() % 2) as usize;
        let mut layer = good[which].clone();
        let at = (rng.next() % layer.len() as u64) as usize;
        let flip = (rng.next() % 255 + 1) as u8;
        layer[at] ^= flip;
        let result = walk_layer(layer);
        assert!(
            matches!(result, Err(Error::Corrupt(_))),
            "case {case}: layer {which} byte {at} xor {flip:#x} gave {result:?}"
        );
    }
    // A file cut short, so its device is shorter than its header says, and nothing.
    let mut short = good[1].clone();
    short.truncate(short.len() - PAGE);
    assert!(matches!(walk_layer(short), Err(Error::Corrupt(_))));
    assert!(walk_layer(Vec::new()).is_err());
}

#[test]
fn if_t4_a_well_hashed_malformed_layer_is_refused() {
    let tree = || {
        dir(vec![
            ("a", file(b"a")),
            ("b", file(b"b")),
            ("c", dir(vec![])),
        ])
    };
    type Tamper = fn(&mut Vec<Node>);
    let cases: Vec<(&str, Tamper)> = vec![
        ("children out of order", |n| n.swap(1, 2)),
        ("a cycle through the root", |n| {
            n[3].first = 1;
            n[3].count = 1;
        }),
        ("a file whose extents do not cover it", |n| n[1].size = 9000),
        ("a mode with setuid", |n| n[2].mode = 0o4755),
    ];
    for (what, tamper) in cases {
        let layer = build_with(&tree(), tamper);
        let result = walk_everything(vec![layer]);
        assert!(result.is_err(), "{what}: {result:?}");
    }
    // A name holding `/`, and a target holding NUL.
    let layer = build_with(&dir(vec![("x/y", file(b"z"))]), |_| {});
    assert!(matches!(
        walk_everything(vec![layer]),
        Err(Error::Corrupt(_))
    ));
    let layer = build_with(&dir(vec![("l", T::Link("a\0b"))]), |_| {});
    assert!(matches!(
        walk_everything(vec![layer]),
        Err(Error::Corrupt(_))
    ));
    assert!(walk_everything(vec![build(&tree())]).is_ok());
}

#[test]
fn if_t8_a_failing_device_is_named() {
    let mut cache = Cache::new(TestDisk::new(vec![lower()], 16), 16).unwrap();
    cache.disk().fail = true;
    assert!(matches!(cache.mount(), Err(Error::Io(_))));
    cache.disk().fail = false;
    cache.mount().unwrap();
    cache.disk().fail = true;
    let mut b = [0u8; 4];
    assert!(matches!(cache.read(0, 1, 0, &mut b), Err(Error::Io(_))));
    assert_eq!(cache.read(0, 1, 4094, &mut b), Err(Error::BadRange));
    assert!(matches!(
        cache.read(3, 1, 0, &mut b),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(cache.expected(0, 0), Err(Error::Corrupt(_))));
    assert!(matches!(cache.expected(0, 1 << 40), Err(Error::Corrupt(_))));
    cache.disk().fail = false;
    let layer = &cache.layers()[0].clone();
    let root = layer.root(&mut cache).unwrap();
    assert!(layer.node(&mut cache, 1 << 20).is_err());
    assert_eq!(layer.target(&mut cache, &root), Err(Error::BadPath));
    assert_eq!(
        layer.file(&mut cache, &root).map(|_| ()),
        Err(Error::IsDirectory)
    );
    let mut file = root;
    file.kind = Kind::Regular;
    assert_eq!(
        layer.child(&mut cache, &file, b"x"),
        Err(Error::NotDirectory)
    );
    assert_eq!(
        layer.children(&mut cache, &file).map(|_| ()),
        Err(Error::NotDirectory)
    );
    let _ = PAGE_U64;
}

#[test]
fn kd2_t12_a_mounted_layers_seal_is_its_first_pages_hash() {
    let (lower, upper) = (lower(), upper());
    let (cache, _) = mount(vec![lower.clone(), upper.clone()], 64);
    let seals: Vec<[u8; 32]> = cache
        .layers()
        .iter()
        .map(|l| l.header.seal().unwrap())
        .collect();
    assert_eq!(
        seals,
        [
            sha256::digest(&lower[..PAGE]),
            sha256::digest(&upper[..PAGE])
        ],
        "the seal the server reports is the hash the launcher computes of the file's first page"
    );
    assert_ne!(seals[0], seals[1], "two layers, two seals");
}

#[test]
fn directory_capability_confines_links_and_parent_traversal() {
    let layer = build_with(
        &dir(vec![
            ("outside", file(b"private")),
            (
                "assets",
                dir(vec![
                    ("inside", file(b"public")),
                    ("absolute", T::Link("/inside")),
                    ("escape", T::Link("../outside")),
                ]),
            ),
        ]),
        |_| {},
    );
    let (mut cache, overlay) = mount(vec![layer], 64);
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let anchor = tree.resolve(&mut cache, b"/assets", true).unwrap();
    let inside = tree
        .resolve_at(&mut cache, Some(&anchor), b"/inside", true)
        .unwrap();
    assert_eq!(
        tree.resolve_at(&mut cache, Some(&anchor), b"/absolute", true)
            .unwrap(),
        inside
    );
    assert_eq!(
        tree.resolve_at(&mut cache, Some(&anchor), b"/escape", true),
        Err(Error::BadPath)
    );
    assert_eq!(
        tree.resolve_at(&mut cache, Some(&anchor), b"/../outside", true),
        Err(Error::BadPath)
    );
    assert_eq!(
        tree.resolve_at(&mut cache, Some(&anchor), b"/outside", true),
        Err(Error::NotFound)
    );
}

#[test]
fn pinned_scratch_directory_lives_until_its_last_view_closes() {
    let mut scratch = Scratch::new();
    let id = scratch.mkdir(0, b"assets", 0o700).unwrap();
    scratch.open(id).unwrap();
    scratch.remove(0, b"assets").unwrap();
    assert!(scratch.info(id).unwrap().directory);
    scratch.close(id);
    assert!(scratch.info(id).is_err());
}
