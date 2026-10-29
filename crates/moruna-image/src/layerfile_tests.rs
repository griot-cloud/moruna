//! IF-T1 to IF-T4 (SDD 27, k): layer files written here and read back with
//! the microkernel's reader compiled for the host.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_imagefs::format::{Header, Kind, Node, OPAQUE, PAGE};

use super::{Data, Entry, Item, convert, read, tree, write};
use crate::testing::{Entry as Tar, LayoutBuilder, Scratch, media};
use crate::{Digest, OciLayout, unpack};

fn file(bytes: &[u8]) -> Entry {
    Entry::File {
        mode: 0o644,
        data: Data::Bytes(Arc::from(bytes)),
        link: None,
    }
}

fn dir(children: Vec<(&str, Entry)>) -> Entry {
    Entry::Directory {
        mode: 0o755,
        opaque: false,
        children: children
            .into_iter()
            .map(|(n, e)| (n.as_bytes().to_vec(), e))
            .collect(),
    }
}

/// The nodes of a layer file, read raw: what the reader is not shown through a merge.
fn raw_nodes(path: &Path) -> Vec<(Vec<u8>, Node)> {
    let bytes = fs::read(path).unwrap();
    let (_, g) = Header::parse(&bytes[..PAGE]).unwrap();
    (0..g.node_count)
        .map(|i| {
            let at = g.nodes_page as usize * PAGE + i as usize * 64;
            let node = Node::decode(&bytes[at..at + 64], i, &g).unwrap();
            let name_at = g.names_page as usize * PAGE + node.name_offset as usize;
            (
                bytes[name_at..name_at + node.name_len as usize].to_vec(),
                node,
            )
        })
        .collect()
}

/// IF-T1: a tree written and read back is the tree; the id is the file's SHA-256; a
/// whiteout and an opaque flag are in the file though one layer alone shows neither.
#[test]
fn if_t1_a_layer_round_trips() {
    let scratch = Scratch::new("if_t1");
    let mut three = vec![7u8; 3 * PAGE];
    three[PAGE..2 * PAGE].fill(0);
    let shared = Data::Bytes(Arc::from(&b"one file, two names"[..]));
    let tree = dir(vec![
        ("empty", file(b"")),
        ("page", file(&[1u8; PAGE])),
        ("three", file(&three)),
        ("odd", file(b"seventeen bytes!!")),
        (
            "link",
            Entry::Symlink {
                target: b"../elsewhere".to_vec(),
            },
        ),
        ("gone", Entry::Whiteout),
        (
            "d",
            Entry::Directory {
                mode: 0o700,
                opaque: true,
                children: BTreeMap::from([
                    (
                        b"a".to_vec(),
                        Entry::File {
                            mode: 0o600,
                            data: shared.clone(),
                            link: Some(1),
                        },
                    ),
                    (
                        b"b".to_vec(),
                        Entry::File {
                            mode: 0o600,
                            data: shared,
                            link: Some(1),
                        },
                    ),
                ]),
            },
        ),
        ("zeros", file(&vec![0u8; 5 * PAGE + 3])),
    ]);
    let out = scratch.path().join("x.layer");
    let written = write(&tree, &out).unwrap();
    assert_eq!(written.id, Digest::of(&fs::read(&out).unwrap()));
    assert_eq!(
        fs::metadata(&out).unwrap().len(),
        written.pages * PAGE as u64
    );
    let merged = read(std::slice::from_ref(&out)).unwrap();
    let want: BTreeMap<String, Item> = [
        ("/d", Item::Directory(0o700)),
        ("/d/a", Item::File(0o600, b"one file, two names".to_vec())),
        ("/d/b", Item::File(0o600, b"one file, two names".to_vec())),
        ("/empty", Item::File(0o644, vec![])),
        ("/link", Item::Symlink(b"../elsewhere".to_vec())),
        ("/odd", Item::File(0o644, b"seventeen bytes!!".to_vec())),
        ("/page", Item::File(0o644, vec![1u8; PAGE])),
        ("/three", Item::File(0o644, three.clone())),
        ("/zeros", Item::File(0o644, vec![0u8; 5 * PAGE + 3])),
    ]
    .into_iter()
    .map(|(p, i)| (p.to_string(), i))
    .collect();
    assert_eq!(merged, want);
    let nodes = raw_nodes(&out);
    let named = |n: &[u8]| nodes.iter().find(|(name, _)| name == n).unwrap().1;
    assert_eq!(named(b"gone").kind, Kind::Whiteout);
    assert_eq!(named(b"d").flags, OPAQUE);
    // Hard links share their extents; zero pages are holes and take no data page.
    assert_eq!(named(b"a").first, named(b"b").first);
    let (header, g) = Header::parse(&fs::read(&out).unwrap()[..PAGE]).unwrap();
    assert_eq!(
        header.data_pages,
        1 + 2 + 1 + 1,
        "page, three's two, odd, the link's file"
    );
    assert_eq!(g.pages, written.pages);
    // The same tree writes the same bytes.
    let again = write(&tree, &scratch.path().join("y.layer")).unwrap();
    assert_eq!(again.id, written.id);
}

/// IF-T1: what cannot be a layer is refused before anything is left behind.
#[test]
fn if_t1_bad_trees_are_refused() {
    let scratch = Scratch::new("if_t1_bad");
    let out = scratch.path().join("bad.layer");
    for (tree, why) in [
        (file(b"x"), "a layer's root is a directory"),
        (dir(vec![("a/b", file(b"x"))]), "cannot be a name"),
        (
            dir(vec![("l", Entry::Symlink { target: vec![] })]),
            "cannot be written",
        ),
    ] {
        let e = write(&tree, &out).unwrap_err().to_string();
        assert!(e.contains(why), "{e}");
        assert!(!out.exists());
    }
    let missing = dir(vec![(
        "f",
        Entry::File {
            mode: 0o644,
            data: Data::Path(scratch.path().join("nope"), 3),
            link: None,
        },
    )]);
    assert!(write(&missing, &out).is_err());
    let short = scratch.path().join("short");
    fs::write(&short, b"ab").unwrap();
    let lying = dir(vec![(
        "f",
        Entry::File {
            mode: 0o644,
            data: Data::Path(short, 3),
            link: None,
        },
    )]);
    assert!(
        write(&lying, &out)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
}

/// The root an image unpacks to, as [`read`] lists a merged tree: the mount points
/// SDD 19 f.3 makes are the server's to add.
fn walk(root: &Path) -> BTreeMap<String, Item> {
    let mut out = BTreeMap::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        for child in fs::read_dir(root.join(&rel)).unwrap() {
            let child = child.unwrap();
            let rel = rel.join(child.file_name());
            let path = format!("/{}", rel.display());
            if ["/tmp", "/proc", "/dev"].contains(&path.as_str()) {
                continue;
            }
            let meta = fs::symlink_metadata(child.path()).unwrap();
            let mode = meta.mode() & 0o777;
            let item = if meta.file_type().is_symlink() {
                use std::os::unix::ffi::OsStrExt;
                Item::Symlink(
                    fs::read_link(child.path())
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                )
            } else if meta.is_dir() {
                todo.push(rel.clone());
                Item::Directory(mode)
            } else {
                let mut bytes = fs::read(child.path());
                if bytes.is_err() {
                    let mut p = meta.permissions();
                    std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o600);
                    fs::set_permissions(child.path(), p).unwrap();
                    bytes = fs::read(child.path());
                }
                Item::File(mode, bytes.unwrap())
            };
            out.insert(path, item);
        }
    }
    out
}

/// The image of `layers` unpacked by SDD 19 and merged from its layer files.
fn both(name: &str, layers: Vec<Vec<Tar>>) -> (BTreeMap<String, Item>, BTreeMap<String, Item>) {
    let scratch = Scratch::new(name);
    let mut b = LayoutBuilder::new(&scratch.path().join("layout"));
    for (i, entries) in layers.iter().enumerate() {
        let m = [media::TAR, media::GZIP, media::ZSTD][i % 3];
        b = b.layer(m, entries);
    }
    b.finish(Some("img:1"));
    let layout = OciLayout::open(scratch.path().join("layout")).unwrap();
    let converted = convert(&layout, "img:1", &scratch.path().join("layers")).unwrap();
    assert_eq!(converted.layers.len(), layers.len());
    let paths: Vec<PathBuf> = converted.layers.iter().map(|l| l.path.clone()).collect();
    let merged = read(&paths).unwrap();
    let image = unpack(&layout, "img:1", &scratch.path().join("cache")).unwrap();
    let unpacked = walk(&image.rootfs);
    // Make what the walk may have opened readable again unpackable by the scratch's drop.
    (merged, unpacked)
}

fn same(name: &str, layers: Vec<Vec<Tar>>) {
    let (merged, unpacked) = both(name, layers);
    for (path, item) in &unpacked {
        assert_eq!(merged.get(path), Some(item), "{name}: {path}");
    }
    for path in merged.keys() {
        assert!(
            unpacked.contains_key(path),
            "{name}: {path} is merged and was not unpacked"
        );
    }
}

/// IF-T2: whiteouts and opaque directories over three layers merge to what SDD 19's
/// unpacking makes of the same layers (IM-T2 to IM-T5's layers, one over another).
#[test]
fn if_t2_three_layers_merge_as_they_unpack() {
    same(
        "if_t2_whiteouts",
        vec![
            vec![
                Tar::dir("d"),
                Tar::file("d/gone", b"1"),
                Tar::file("d/stays", b"2"),
                Tar::file("d/again", b"old"),
                Tar::dir("tree"),
                Tar::dir("tree/sub"),
                Tar::file("tree/sub/f", b"1"),
                Tar::Dir("tree/sealed", 0o000),
                Tar::Symlink("tree/link", "/etc/passwd"),
                Tar::dir("keep"),
                Tar::dir("o"),
                Tar::file("o/lower", b"1"),
                Tar::dir("o/lowerdir"),
                Tar::file("o/lowerdir/f", b"2"),
            ],
            vec![
                Tar::file("d/.wh.gone", b""),
                Tar::file("d/.wh.never-was", b""),
                Tar::file("nowhere/.wh.x", b""),
                Tar::file("d/again", b"new"),
                Tar::file("d/.wh.again", b""),
                Tar::file("d/.wh..wh.plnk", b""),
                Tar::file(".wh.tree", b""),
            ],
            vec![
                Tar::file("o/before", b"b"),
                Tar::file("o/.wh..wh..opq", b""),
                Tar::file("o/after", b"a"),
                Tar::file("missing/.wh..wh..opq", b""),
                Tar::file("tree", b"a file where a tree was"),
                Tar::file("d/.wh.stays", b""),
            ],
        ],
    );
}

/// IF-T2: replacements of every kind, a directory emptied by whiteouts, links of a
/// lower layer followed by an upper one's paths, hard links, and modes.
#[test]
fn if_t2_replacements_links_and_modes_merge_as_they_unpack() {
    same(
        "if_t2_replacements",
        vec![
            vec![
                Tar::dir("etc"),
                Tar::file("etc/a", b"lower a"),
                Tar::file("etc/b", b"lower b"),
                Tar::dir("swap"),
                Tar::file("swap/inside", b"x"),
                Tar::file("flip", b"a file"),
                Tar::dir("usr"),
                Tar::dir("usr/lib"),
                Tar::file("usr/lib/libc.so", &[3u8; 9000]),
                Tar::Symlink("lib", "usr/lib"),
                Tar::dir("emptied"),
                Tar::file("emptied/x", b"x"),
                Tar::file("emptied/y", b"y"),
                Tar::File("secret", b"mode 0".to_vec(), 0o000),
                Tar::File("tool", b"setuid dropped".to_vec(), 0o4755),
            ],
            vec![
                Tar::file("etc/a", b"upper a"),
                Tar::file("etc/c", b"upper c"),
                Tar::file("swap", b"now a file"),
                Tar::dir("flip"),
                Tar::file("flip/inside", b"y"),
                Tar::file("lib/libextra.so", &[4u8; 5000]),
                Tar::file("emptied/.wh.x", b""),
                Tar::file("emptied/.wh.y", b""),
                Tar::Hardlink("etc/hard", "etc/b"),
                Tar::Dir("usr", 0o750),
            ],
            vec![
                Tar::Symlink("etc/a", "c"),
                Tar::file("emptied/z", b"z"),
                Tar::file("usr/lib/.wh.libc.so", b""),
                Tar::Hardlink("usr/lib/again", "lib/libextra.so"),
                Tar::dir("swap2"),
                Tar::file("swap2/deep", &vec![0u8; 3 * PAGE]),
            ],
        ],
    );
}

/// IF-T2: a layer that changes nothing, and a whiteout of the root's every entry.
#[test]
fn if_t2_nothing_and_everything_merge_as_they_unpack() {
    same(
        "if_t2_edges",
        vec![
            vec![Tar::dir("a"), Tar::file("a/f", b"1"), Tar::file("b", b"2")],
            vec![],
            vec![
                Tar::file(".wh.a", b""),
                Tar::file(".wh.b", b""),
                Tar::file("c", b"3"),
            ],
        ],
    );
}

/// A seeded generator, so a failing case names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_tree(rng: &mut Rng, depth: u32, files: &mut Vec<(Vec<u8>, Vec<u8>)>, at: &str) -> Entry {
    let mut children = BTreeMap::new();
    for i in 0..rng.below(6) {
        let name = format!("n{i}-{}", rng.below(1000));
        let path = format!("{at}/{name}");
        let entry = match rng.below(if depth > 2 { 3 } else { 4 }) {
            0 | 1 => {
                let len = match rng.below(4) {
                    0 => 0,
                    1 => rng.below(PAGE as u64) as usize,
                    2 => PAGE * rng.below(4) as usize,
                    _ => rng.below(5 * PAGE as u64) as usize,
                };
                let zero_pages = rng.below(2) == 0;
                let bytes: Vec<u8> = (0..len)
                    .map(|j| {
                        if zero_pages && (j / PAGE) % 2 == 1 {
                            0
                        } else {
                            (rng.next() & 0xff) as u8 | 1
                        }
                    })
                    .collect();
                files.push((path.into_bytes(), bytes.clone()));
                Entry::File {
                    mode: 0o644,
                    data: Data::Bytes(Arc::from(bytes)),
                    link: None,
                }
            }
            2 => Entry::Symlink {
                target: format!("t{}", rng.below(100)).into_bytes(),
            },
            _ => random_tree(rng, depth + 1, files, &path),
        };
        children.insert(name.into_bytes(), entry);
    }
    Entry::Directory {
        mode: 0o755,
        opaque: false,
        children,
    }
}

/// IF-T3: over random trees, every file's data starts on a page of the layer file and
/// reads back whole.
#[test]
fn if_t3_every_file_starts_on_a_page() {
    let scratch = Scratch::new("if_t3");
    for seed in 1..=60u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut files = Vec::new();
        let tree = random_tree(&mut rng, 0, &mut files, "");
        let out = scratch.path().join(format!("{seed}.layer"));
        write(&tree, &out).unwrap();
        let bytes = fs::read(&out).unwrap();
        let merged = read(std::slice::from_ref(&out)).unwrap();
        let (_, g) = Header::parse(&bytes[..PAGE]).unwrap();
        for (path, data) in &files {
            let path = String::from_utf8(path.clone()).unwrap();
            assert_eq!(
                merged.get(&path),
                Some(&Item::File(0o644, data.clone())),
                "seed {seed}: {path}"
            );
            // The first page that is not a hole is a whole page of the layer file, at a
            // page boundary inside the data region.
            if let Some(first) = data.chunks(PAGE).position(|c| c.iter().any(|b| *b != 0)) {
                let page = &data[first * PAGE..data.len().min((first + 1) * PAGE)];
                let found = (g.data_page..g.tree_page).any(|p| {
                    let at = p as usize * PAGE;
                    &bytes[at..at + page.len()] == page
                        && bytes[at + page.len()..at + PAGE].iter().all(|b| *b == 0)
                });
                assert!(found, "seed {seed}: {path}'s data is not on a page");
            }
        }
    }
}

/// IF-T4: a layer file with any byte changed is refused by the reader, never a panic.
#[test]
fn if_t4_a_corrupted_layer_is_refused() {
    let scratch = Scratch::new("if_t8");
    let mut rng = Rng(0x00c0_ffee_5eed);
    let mut files = Vec::new();
    let tree = random_tree(&mut rng, 0, &mut files, "");
    let good = scratch.path().join("good.layer");
    write(&tree, &good).unwrap();
    let bytes = fs::read(&good).unwrap();
    assert!(read(std::slice::from_ref(&good)).is_ok());
    let bad = scratch.path().join("bad.layer");
    for case in 0..300 {
        let mut b = bytes.clone();
        let at = rng.below(b.len() as u64) as usize;
        b[at] ^= (rng.below(255) + 1) as u8;
        fs::write(&bad, &b).unwrap();
        // The merged tree hides nothing of a single layer but its own whiteouts, which a
        // random tree has none of: every page is read.
        let result = read(std::slice::from_ref(&bad));
        assert!(
            result.is_err(),
            "case {case}: byte {at} changed and the layer was read"
        );
    }
    let mut cut = bytes.clone();
    cut.truncate(cut.len() - PAGE);
    fs::write(&bad, &cut).unwrap();
    assert!(
        read(std::slice::from_ref(&bad))
            .unwrap_err()
            .to_string()
            .contains("device holds")
    );
    assert!(read(&[scratch.path().join("absent")]).is_err());
}

/// The CI boot's fixture (with a small big file): the check list it writes is what the
/// layer files it writes merge to, line for line, and the layer list names both.
#[test]
fn if_t13_the_fixture_lists_what_its_layers_hold() {
    let scratch = Scratch::new("if_t13");
    let f = crate::testing::fixture_with(scratch.path(), 3 * PAGE + 5).unwrap();
    let paths: Vec<PathBuf> = f.layers.iter().map(|(p, _)| p.clone()).collect();
    let merged = read(&paths).unwrap();
    let text = fs::read_to_string(&f.checks).unwrap();
    let mut lines = text.lines();
    let header = lines.next().unwrap();
    assert_eq!(header, format!("moruna-checks 1 {}", text.len()));
    let mut listed = 0;
    for line in lines {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields.as_slice() {
            ["cold", p] | ["big", p] => assert!(merged.contains_key(*p), "{p}"),
            ["d", mode, p] => {
                assert_eq!(
                    merged[*p],
                    Item::Directory(u32::from_str_radix(mode, 8).unwrap()),
                    "{p}"
                );
                listed += 1;
            }
            ["f", mode, size, sha, p] => {
                let Item::File(m, bytes) = &merged[*p] else {
                    panic!("{p}")
                };
                assert_eq!(*m, u32::from_str_radix(mode, 8).unwrap(), "{p}");
                assert_eq!(bytes.len(), size.parse::<usize>().unwrap(), "{p}");
                assert_eq!(Digest::of(bytes).hex(), *sha, "{p}");
                listed += 1;
            }
            ["l", target, p] => {
                assert_eq!(merged[*p], Item::Symlink(target.as_bytes().to_vec()), "{p}");
                listed += 1;
            }
            other => panic!("a line of the list: {other:?}"),
        }
    }
    assert_eq!(listed, merged.len());
    let list = fs::read_to_string(&f.list).unwrap();
    assert_eq!(
        list,
        format!(
            "moruna-layers 1\nlayer block1 {}\nlayer block2 {}\n",
            f.layers[0].1.hex(),
            f.layers[1].1.hex()
        )
    );
    for (p, id) in &f.layers {
        assert_eq!(Digest::of(&fs::read(p).unwrap()), *id);
    }
}

/// LX-T17 (SDD 28, k): a host directory as one layer, as the boot's Linux image puts
/// its test programs above an image's layers: files with their modes, directories and
/// links read back as they were; a special file is refused.
#[test]
fn lx_t17_a_directory_as_a_layer() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("lx_t17");
    let root = scratch.path().join("programs");
    fs::create_dir_all(root.join("kc1/bin")).unwrap();
    fs::write(root.join("kc1/bin/hello"), b"\x7fELF a program").unwrap();
    fs::set_permissions(
        root.join("kc1/bin/hello"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(root.join("kc1/greeting.txt"), b"greetings\n").unwrap();
    std::os::unix::fs::symlink("bin/hello", root.join("kc1/run")).unwrap();
    let out = scratch.path().join("top.layer");
    let written = write(&tree(&root).unwrap(), &out).unwrap();
    assert_eq!(written.id, Digest::of(&fs::read(&out).unwrap()));
    let merged = read(&[out]).unwrap();
    assert_eq!(
        merged.get("/kc1/bin/hello"),
        Some(&Item::File(0o755, b"\x7fELF a program".to_vec()))
    );
    assert_eq!(
        merged.get("/kc1/greeting.txt"),
        Some(&Item::File(0o644, b"greetings\n".to_vec()))
    );
    assert_eq!(
        merged.get("/kc1/run"),
        Some(&Item::Symlink(b"bin/hello".to_vec()))
    );
    assert!(matches!(merged.get("/kc1/bin"), Some(Item::Directory(_))));
    // Not a directory, and a special file inside one, are refused.
    assert!(tree(&root.join("kc1/greeting.txt")).is_err());
    let odd = scratch.path().join("odd");
    fs::create_dir_all(&odd).unwrap();
    let fifo = odd.join("pipe");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    if made.is_ok_and(|s| s.success()) {
        assert!(tree(&odd).is_err());
    }
}
