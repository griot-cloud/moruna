//! SDD 19 IM-T1 to IM-T15: the layout, the unpacking and the cache, over layouts built here.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::Ordering;

use crate::testing::{Counting, Entry, LayoutBuilder, Scratch, gzip, media, tar_of};
use crate::{COMPLETE, Digest, Error, ImageSource, MOUNT_POINTS, OciLayout, unpack};

fn text(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn absent(root: &Path, path: &str) -> bool {
    fs::symlink_metadata(root.join(path)).is_err()
}

/// A layout of `layers` named `app`, and its unpacking.
fn unpacked(scratch: &Scratch, layers: &[(&str, Vec<Entry>)]) -> crate::Result<crate::Image> {
    let layout = scratch.path().join("layout");
    let mut builder = LayoutBuilder::new(&layout);
    for (media_type, entries) in layers {
        builder = builder.layer(media_type, entries);
    }
    builder.finish(Some("app"));
    let source = OciLayout::open(&layout)?;
    unpack(&source, "app", &scratch.path().join("cache"))
}

fn tar_layer(entries: Vec<Entry>) -> (&'static str, Vec<Entry>) {
    (media::TAR, entries)
}

fn refused(result: crate::Result<crate::Image>) -> String {
    result.expect_err("the image is refused").to_string()
}

/// IM-T1: a reference is a digest or a name in `index.json`; one that names nothing says what the
/// layout names.
#[test]
fn im_t1_a_layout_resolves_its_references() {
    let scratch = Scratch::new("im_t1");
    let layout = scratch.path().join("layout");
    let digest = LayoutBuilder::new(&layout)
        .layer(media::TAR, &[Entry::file("a", b"1")])
        .finish(Some("app:1"));
    let source = OciLayout::open(&layout).unwrap();
    assert_eq!(source.manifest("app:1").unwrap().digest, digest);
    assert_eq!(source.manifest(&digest.to_string()).unwrap().digest, digest);
    let e = source.manifest("other").unwrap_err().to_string();
    assert!(
        e.contains("no image is named `other`") && e.contains("app:1"),
        "{e}"
    );
    let e = source
        .manifest(&format!("sha256:{}", "0".repeat(64)))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("blobs/sha256/000"),
        "a digest the layout does not hold: {e}"
    );

    // A layout that is not one, and one of another version.
    let e = OciLayout::open(scratch.path().join("nowhere"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("oci-layout"), "{e}");
    fs::write(
        layout.join("oci-layout"),
        br#"{"imageLayoutVersion":"2.0.0"}"#,
    )
    .unwrap();
    let e = OciLayout::open(&layout).unwrap_err().to_string();
    assert!(e.contains("only 1.0.0 is read"), "{e}");
    fs::write(layout.join("oci-layout"), b"not json").unwrap();
    assert!(OciLayout::open(&layout).is_err());

    // An index naming two images alike is ambiguous; one with no names says to use a digest.
    fs::write(
        layout.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let entry = |name: &str| {
        serde_json::json!({"mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest.to_string(), "size": 1,
            "annotations": {"org.opencontainers.image.ref.name": name}})
    };
    fs::write(
        layout.join("index.json"),
        serde_json::json!({"schemaVersion": 2, "manifests": [entry("x"), entry("x")]}).to_string(),
    )
    .unwrap();
    let e = OciLayout::open(&layout)
        .unwrap()
        .manifest("x")
        .unwrap_err()
        .to_string();
    assert!(e.contains("2 images are named `x`"), "{e}");
    fs::write(
        layout.join("index.json"),
        serde_json::json!({"schemaVersion": 2, "manifests": [{"digest": digest.to_string()}]})
            .to_string(),
    )
    .unwrap();
    let e = OciLayout::open(&layout)
        .unwrap()
        .manifest("x")
        .unwrap_err()
        .to_string();
    assert!(e.contains("name an image by its digest"), "{e}");
    fs::write(layout.join("index.json"), b"{").unwrap();
    assert!(OciLayout::open(&layout).unwrap().manifest("x").is_err());
}

/// IM-T1: a manifest that is an index of images is refused, a digest that is not a digest is
/// refused, and a manifest that is not JSON of its shape is refused, each naming itself.
#[test]
fn im_t1_a_manifest_is_an_image_manifest() {
    let index = serde_json::json!({"mediaType": "application/vnd.oci.image.index.v1+json",
        "config": {"mediaType": "x", "digest": format!("sha256:{}", "a".repeat(64)), "size": 1},
        "layers": []})
    .to_string();
    let digest = Digest::of(index.as_bytes());
    let e = crate::Manifest::parse(digest.clone(), index.as_bytes())
        .unwrap_err()
        .to_string();
    assert!(e.contains("is not an image manifest"), "{e}");
    let e = crate::Manifest::parse(Digest::of(b"{}"), b"{}")
        .unwrap_err()
        .to_string();
    assert!(e.contains("not an image manifest"), "{e}");
    for bad in [
        "md5:abc",
        "sha256:ABC",
        "sha256:",
        &format!("sha256:{}", "g".repeat(64)),
    ] {
        let e = bad.parse::<Digest>().unwrap_err().to_string();
        assert!(e.contains("is not a digest"), "{bad}: {e}");
    }
    assert!(Digest::is_digest(&format!("sha256:{}", "0".repeat(64))));
    assert!(!Digest::is_digest("app"));
    let d = Digest::of(b"abc");
    assert_eq!(
        d.to_string(),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// IM-T2: layers are laid over one another in order, whatever their compression: a later file
/// replaces an earlier one, directories merge, a file replaces a directory and the reverse, and
/// the directories the sandbox mounts onto exist.
#[test]
fn im_t2_layers_lay_over_one_another() {
    let scratch = Scratch::new("im_t2");
    let image = unpacked(
        &scratch,
        &[
            (
                media::TAR,
                vec![
                    Entry::dir("etc"),
                    Entry::file("etc/a", b"lower a"),
                    Entry::file("etc/b", b"lower b"),
                    Entry::dir("swap"),
                    Entry::file("swap/inside", b"x"),
                    Entry::file("flip", b"a file"),
                ],
            ),
            (
                media::GZIP,
                vec![
                    Entry::file("etc/a", b"upper a"),
                    Entry::file("etc/c", b"upper c"),
                    Entry::file("swap", b"now a file"),
                    Entry::dir("flip"),
                    Entry::file("flip/inside", b"y"),
                ],
            ),
            (media::ZSTD, vec![Entry::file("z", b"zstd")]),
            (media::DOCKER_GZIP, vec![Entry::file("d", b"docker")]),
        ],
    )
    .unwrap();
    let root = &image.rootfs;
    assert_eq!(text(root, "etc/a"), "upper a");
    assert_eq!(text(root, "etc/b"), "lower b");
    assert_eq!(text(root, "etc/c"), "upper c");
    assert_eq!(text(root, "swap"), "now a file");
    assert_eq!(text(root, "flip/inside"), "y");
    assert_eq!(text(root, "z"), "zstd");
    assert_eq!(text(root, "d"), "docker");
    for name in MOUNT_POINTS {
        assert!(root.join(name).is_dir(), "{name}");
    }
    assert!(
        image
            .rootfs
            .starts_with(scratch.path().join("cache").join(image.digest.hex()))
    );
    assert!(
        scratch
            .path()
            .join("cache")
            .join(image.digest.hex())
            .join(COMPLETE)
            .is_file()
    );
}

/// IM-T2: a parent the archive never made is made, `./` and `a/./b` paths mean what they say, and
/// a root entry `.` makes nothing.
#[test]
fn im_t2_paths_are_normalised() {
    let scratch = Scratch::new("im_t2_paths");
    let image = unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::dir("./"),
            Entry::dir("."),
            Entry::file("./usr//lib/./x", b"deep"),
            Entry::Dir("usr/lib/locked", 0o555),
            Entry::file(
                "usr/lib/locked/f",
                b"in a directory the layer made read only",
            ),
        ])],
    )
    .unwrap();
    assert_eq!(text(&image.rootfs, "usr/lib/x"), "deep");
    assert_eq!(
        text(&image.rootfs, "usr/lib/locked/f"),
        "in a directory the layer made read only"
    );
}

/// IM-T3: `.wh.<name>` removes a file the layers below made, and not the file this layer made
/// under the same name.
#[test]
fn im_t3_a_whiteout_removes_a_file() {
    let scratch = Scratch::new("im_t3");
    let image = unpacked(
        &scratch,
        &[
            tar_layer(vec![
                Entry::dir("d"),
                Entry::file("d/gone", b"1"),
                Entry::file("d/stays", b"2"),
                Entry::file("d/again", b"old"),
            ]),
            tar_layer(vec![
                Entry::file("d/.wh.gone", b""),
                Entry::file("d/.wh.never-was", b""),
                Entry::file("nowhere/.wh.x", b""),
                Entry::file("d/again", b"new"),
                Entry::file("d/.wh.again", b""),
                Entry::file("d/.wh..wh.plnk", b""),
            ]),
        ],
    )
    .unwrap();
    assert!(absent(&image.rootfs, "d/gone"));
    assert!(
        absent(&image.rootfs, "d/.wh.gone"),
        "a whiteout is never a file"
    );
    assert_eq!(text(&image.rootfs, "d/stays"), "2");
    assert_eq!(
        text(&image.rootfs, "d/again"),
        "new",
        "this layer's file outlives its own whiteout"
    );
    assert!(absent(&image.rootfs, "nowhere"));
}

/// IM-T4: `.wh.<dir>` removes a whole tree, links and directories the owner could not write
/// included.
#[test]
fn im_t4_a_whiteout_removes_a_directory() {
    let scratch = Scratch::new("im_t4");
    let image = unpacked(
        &scratch,
        &[
            tar_layer(vec![
                Entry::dir("tree"),
                Entry::dir("tree/sub"),
                Entry::file("tree/sub/f", b"1"),
                Entry::Dir("tree/sealed", 0o000),
                Entry::Symlink("tree/link", "/etc/passwd"),
                Entry::dir("keep"),
            ]),
            tar_layer(vec![Entry::file(".wh.tree", b"")]),
        ],
    )
    .unwrap();
    assert!(absent(&image.rootfs, "tree"));
    assert!(image.rootfs.join("keep").is_dir());
}

/// IM-T5: `.wh..wh..opq` empties the directory of what lower layers put there, whichever side
/// of the marker this layer's own entries come on, and leaves the directory.
#[test]
fn im_t5_an_opaque_directory_keeps_only_this_layers() {
    let scratch = Scratch::new("im_t5");
    let image = unpacked(
        &scratch,
        &[
            tar_layer(vec![
                Entry::dir("d"),
                Entry::file("d/lower", b"1"),
                Entry::dir("d/lowerdir"),
                Entry::file("d/lowerdir/f", b"2"),
                Entry::dir("e"),
                Entry::file("e/lower", b"3"),
            ]),
            tar_layer(vec![
                Entry::file("d/before", b"b"),
                Entry::file("d/.wh..wh..opq", b""),
                Entry::file("d/after", b"a"),
                Entry::dir("e"),
                Entry::file("e/.wh..wh..opq", b""),
                Entry::file("missing/.wh..wh..opq", b""),
            ]),
        ],
    )
    .unwrap();
    let root = &image.rootfs;
    assert!(absent(root, "d/lower") && absent(root, "d/lowerdir"));
    assert_eq!(text(root, "d/before"), "b");
    assert_eq!(text(root, "d/after"), "a");
    assert!(root.join("e").is_dir() && absent(root, "e/lower"));
    assert!(absent(root, "missing"));
}

/// IM-T6: a `..` component is refused naming the layer and the entry, and nothing is left in the
/// cache.
#[test]
fn im_t6_a_parent_component_is_refused() {
    for path in ["../escape", "a/../../escape", "a/b/../../../x"] {
        let scratch = Scratch::new("im_t6");
        let e = refused(unpacked(
            &scratch,
            &[tar_layer(vec![Entry::File(path, b"x".to_vec(), 0o644)])],
        ));
        assert!(e.contains(path) && e.contains("`..`"), "{e}");
        assert!(!scratch.path().join("escape").exists());
        assert_cache_is_empty(&scratch);
    }
    // In a link's target too: a hard link must name something inside the root.
    let scratch = Scratch::new("im_t6_link");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::file("f", b"1"),
            Entry::Hardlink("h", "../f"),
        ])],
    ));
    assert!(e.contains("not inside the root"), "{e}");
}

fn assert_cache_is_empty(scratch: &Scratch) {
    let cache = scratch.path().join("cache");
    let left: Vec<_> = fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        left.is_empty(),
        "a refused image leaves nothing in the cache: {left:?}"
    );
}

/// IM-T7: an absolute path is refused, for a file, a directory and a hard link's target.
#[test]
fn im_t7_an_absolute_path_is_refused() {
    for entry in [
        Entry::File("/etc/passwd", b"x".to_vec(), 0o644),
        Entry::Dir("/etc", 0o755),
        Entry::Symlink("/l", "x"),
    ] {
        let scratch = Scratch::new("im_t7");
        let e = refused(unpacked(&scratch, &[tar_layer(vec![entry])]));
        assert!(e.contains("is an absolute path"), "{e}");
        assert_cache_is_empty(&scratch);
    }
    let scratch = Scratch::new("im_t7_link");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::file("f", b"1"),
            Entry::Hardlink("h", "/f"),
        ])],
    ));
    assert!(e.contains("not inside the root"), "{e}");
}

/// IM-T8: an entry that would be written through a link out of the root is refused: a link to an
/// absolute path, one that climbs out, one that climbs out in steps; a link inside the root is
/// followed and the entry lands where the link points.
#[test]
fn im_t8_a_symlink_escape_is_refused() {
    let escapes = [("a", "/etc"), ("a", "../../etc"), ("a", "b/../../..")];
    for (name, target) in escapes {
        let scratch = Scratch::new("im_t8");
        let e = refused(unpacked(
            &scratch,
            &[
                tar_layer(vec![Entry::Symlink(name, target)]),
                tar_layer(vec![Entry::file("a/passwd", b"owned")]),
            ],
        ));
        assert!(
            e.contains("a/passwd") && e.contains("leaves the root"),
            "{target}: {e}"
        );
        assert_cache_is_empty(&scratch);
    }
    // In one layer, and through a chain of links.
    let scratch = Scratch::new("im_t8_chain");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::Symlink("one", "two"),
            Entry::Symlink("two", "/etc"),
            Entry::file("one/passwd", b"owned"),
        ])],
    ));
    assert!(e.contains("leaves the root"), "{e}");
    let scratch = Scratch::new("im_t8_loop");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::Symlink("x", "y"),
            Entry::Symlink("y", "x"),
            Entry::file("x/f", b"1"),
        ])],
    ));
    assert!(e.contains("too many symbolic links"), "{e}");
    let scratch = Scratch::new("im_t8_file");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::file("f", b"1"),
            Entry::file("f/g", b"2"),
        ])],
    ));
    assert!(e.contains("which is not one"), "{e}");

    // A link inside the root is followed.
    let scratch = Scratch::new("im_t8_inside");
    let image = unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::dir("usr"),
            Entry::dir("usr/lib"),
            Entry::Symlink("lib", "usr/lib"),
            Entry::Symlink("usr/up", "../usr"),
            Entry::file("lib/x", b"through"),
            Entry::file("usr/up/lib/y", b"through two"),
            Entry::Symlink("abs", "/an/absolute/target"),
        ])],
    )
    .unwrap();
    assert_eq!(text(&image.rootfs, "usr/lib/x"), "through");
    assert_eq!(text(&image.rootfs, "usr/lib/y"), "through two");
    assert_eq!(
        fs::read_link(image.rootfs.join("abs"))
            .unwrap()
            .to_str()
            .unwrap(),
        "/an/absolute/target",
        "a link is written as it is: its target is resolved in the sandbox's root"
    );
}

/// IM-T9: setuid, setgid and sticky are dropped from files and directories; the permission bits
/// stay.
#[test]
fn im_t9_special_mode_bits_are_dropped() {
    let scratch = Scratch::new("im_t9");
    let image = unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::File("suid", b"x".to_vec(), 0o4755),
            Entry::File("sgid", b"x".to_vec(), 0o2755),
            Entry::File("sticky", b"x".to_vec(), 0o1777),
            Entry::File("plain", b"x".to_vec(), 0o640),
            Entry::Dir("dir", 0o2755),
            Entry::Dir("tmpdir", 0o1777),
        ])],
    )
    .unwrap();
    let mode = |p: &str| {
        fs::metadata(image.rootfs.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("suid"), 0o755);
    assert_eq!(mode("sgid"), 0o755);
    assert_eq!(mode("sticky"), 0o777);
    assert_eq!(mode("plain"), 0o640);
    assert_eq!(mode("dir"), 0o755);
    assert_eq!(mode("tmpdir"), 0o777);
}

/// IM-T10: a layer, a config or a manifest that does not hash to its name is refused naming the
/// digest, and so is a blob of the wrong size; nothing is left in the cache.
#[test]
fn im_t10_a_digest_mismatch_is_refused() {
    // A layer whose bytes are another layer's.
    let scratch = Scratch::new("im_t10_layer");
    let layout = scratch.path().join("layout");
    let claimed = Digest::of(b"what the manifest says it is");
    let real = tar_of(&[Entry::file("f", b"1")]);
    LayoutBuilder::new(&layout)
        .layer_claiming(media::TAR, &claimed, &real, real.len() as u64)
        .finish(Some("app"));
    let source = OciLayout::open(&layout).unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::Digest { .. }), "{e}");
    let text = e.to_string();
    assert!(
        text.contains(&claimed.to_string()) && text.contains(&Digest::of(&real).to_string()),
        "{text}"
    );
    assert_cache_is_empty(&scratch);

    // A gzip layer that is not gzip: the digest is what is named, not the decoder's complaint.
    let scratch = Scratch::new("im_t10_gzip");
    let layout = scratch.path().join("layout");
    let claimed = Digest::of(b"a gzip layer");
    LayoutBuilder::new(&layout)
        .layer_claiming(media::GZIP, &claimed, b"this is not gzip", 16)
        .finish(Some("app"));
    let source = OciLayout::open(&layout).unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::Digest { .. }), "{e}");

    // A layer that is the bytes it is named by, and whose size is not the descriptor's.
    let scratch = Scratch::new("im_t10_size");
    let layout = scratch.path().join("layout");
    LayoutBuilder::new(&layout)
        .layer_claiming(media::TAR, &Digest::of(&real), &real, 7)
        .finish(Some("app"));
    let source = OciLayout::open(&layout).unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::Size { declared: 7, .. }), "{e}");
    assert!(e.to_string().contains("7 bytes"), "{e}");

    // A layer that is gzip of a tar the digest does not name, and a layer that is the right
    // bytes and a broken tar: the tar's own complaint.
    let scratch = Scratch::new("im_t10_tar");
    let layout = scratch.path().join("layout");
    let broken = vec![7u8; 600];
    LayoutBuilder::new(&layout)
        .layer_bytes(media::TAR, &broken)
        .finish(Some("app"));
    let source = OciLayout::open(&layout).unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::Io { .. }), "{e}");

    // The config: bytes that are not what the manifest names.
    let scratch = Scratch::new("im_t10_config");
    let layout = scratch.path().join("layout");
    let manifest = LayoutBuilder::new(&layout)
        .layer(media::TAR, &[Entry::file("f", b"1")])
        .finish(Some("app"));
    let source = OciLayout::open(&layout).unwrap();
    let config = source.manifest("app").unwrap().config;
    fs::write(
        layout.join("blobs/sha256").join(config.digest.hex()),
        br#"{"config":{"Cmd":["evil"]}}"#,
    )
    .unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::Digest { .. }), "{e}");
    assert!(e.to_string().contains(&config.digest.to_string()), "{e}");

    // The manifest: bytes that are not what the index names.
    fs::write(layout.join("blobs/sha256").join(manifest.hex()), b"{}").unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::Digest { .. }), "{e}");
    assert!(e.to_string().contains(&manifest.to_string()), "{e}");
    assert_cache_is_empty(&scratch);
}

/// IM-T11: the four layer media types are read; any other is refused naming it, before a layer
/// is read.
#[test]
fn im_t11_an_unknown_media_type_is_refused() {
    let scratch = Scratch::new("im_t11");
    let layout = scratch.path().join("layout");
    let layer = tar_of(&[Entry::file("f", b"1")]);
    LayoutBuilder::new(&layout)
        .layer(media::TAR, &[Entry::file("g", b"1")])
        .layer_bytes("application/vnd.oci.image.layer.v1.tar+lz4", &layer)
        .finish(Some("app"));
    let source = Counting::new(OciLayout::open(&layout).unwrap());
    let e = unpack(&source, "app", &scratch.path().join("cache")).unwrap_err();
    assert!(matches!(e, Error::MediaType { .. }), "{e}");
    assert!(e.to_string().contains("tar+lz4"), "{e}");
    assert_eq!(
        source.blobs.load(Ordering::SeqCst),
        0,
        "refused before any blob was read"
    );
    // Non-layer documents: a layer that is not a tar of the kinds an image holds.
    let scratch = Scratch::new("im_t11_kind");
    let mut gnu = tar::Header::new_gnu();
    gnu.set_path("odd").unwrap();
    gnu.set_entry_type(tar::EntryType::new(b'Z'));
    gnu.set_size(0);
    gnu.set_mode(0o644);
    gnu.set_cksum();
    let mut b = tar::Builder::new(Vec::new());
    b.append(&gnu, &[][..]).unwrap();
    let odd = b.into_inner().unwrap();
    let layout = scratch.path().join("layout");
    LayoutBuilder::new(&layout)
        .layer_bytes(media::GZIP, &gzip(&odd))
        .finish(Some("app"));
    let source = OciLayout::open(&layout).unwrap();
    let e = unpack(&source, "app", &scratch.path().join("cache"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("entry type"), "{e}");
}

/// IM-T12: unpacking an image the cache holds whole reads no layer and no config, and gives the
/// same image; an entry without `complete` is refused, never used.
#[test]
fn im_t12_a_second_unpack_reads_no_layer() {
    let scratch = Scratch::new("im_t12");
    let layout = scratch.path().join("layout");
    LayoutBuilder::new(&layout)
        .config(serde_json::json!({"Cmd": ["run"]}))
        .layer(media::GZIP, &[Entry::file("a", b"1"), Entry::Fifo("pipe")])
        .layer(media::TAR, &[Entry::file("b", b"2")])
        .finish(Some("app"));
    let source = Counting::new(OciLayout::open(&layout).unwrap());
    let cache = scratch.path().join("cache");
    let first = unpack(&source, "app", &cache).unwrap();
    assert_eq!(
        source.blobs.load(Ordering::SeqCst),
        3,
        "a config and two layers"
    );
    assert_eq!(first.skipped, 1);
    let second = unpack(&source, "app", &cache).unwrap();
    assert_eq!(
        source.blobs.load(Ordering::SeqCst),
        3,
        "no blob read the second time"
    );
    assert_eq!(source.manifests.load(Ordering::SeqCst), 2);
    assert_eq!(first, second);
    assert_eq!(second.config.cmd, ["run"]);
    assert_eq!(
        second.skipped, 1,
        "the count is part of what the cache keeps"
    );
    // The same cache by the digest.
    let by_digest = unpack(&source, &first.digest.to_string(), &cache).unwrap();
    assert_eq!(by_digest, first);
    assert_eq!(source.blobs.load(Ordering::SeqCst), 3);

    // An entry that is there without `complete` was not made by an unpacking that finished.
    fs::remove_file(cache.join(first.digest.hex()).join(COMPLETE)).unwrap();
    let e = unpack(&source, "app", &cache).unwrap_err().to_string();
    assert!(e.contains("no `complete` file"), "{e}");
    fs::write(cache.join(first.digest.hex()).join(COMPLETE), b"many").unwrap();
    let e = unpack(&source, "app", &cache).unwrap_err().to_string();
    assert!(e.contains("is not a count"), "{e}");
}

/// IM-T13: two unpackings of one image racing end with one entry, both succeed, and neither
/// leaves a directory of its own behind.
#[test]
fn im_t13_racing_unpacks_end_with_one_entry() {
    for round in 0..8 {
        let scratch = Scratch::new("im_t13");
        let layout = scratch.path().join("layout");
        let files: Vec<Entry> = (0..200)
            .map(|i| {
                Entry::File(
                    Box::leak(format!("f{i}").into_boxed_str()),
                    vec![b'x'; 4096],
                    0o644,
                )
            })
            .collect();
        LayoutBuilder::new(&layout)
            .layer(media::GZIP, &files)
            .finish(Some("app"));
        let cache = scratch.path().join("cache");
        let barrier = std::sync::Barrier::new(2);
        let images: Vec<crate::Image> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        let source = OciLayout::open(&layout).unwrap();
                        barrier.wait();
                        unpack(&source, "app", &cache).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(images[0], images[1], "round {round}");
        let left: Vec<_> = fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "round {round}: {left:?}");
        assert_eq!(left[0].to_str().unwrap(), images[0].digest.hex());
        assert_eq!(
            fs::read_dir(images[0].rootfs.clone()).unwrap().count(),
            200 + MOUNT_POINTS.len()
        );
    }
}

/// IM-T14: devices and fifos are skipped and counted, hard links join the files they name (and
/// a link to a link is a link), and a hard link to nothing, or to a directory, is refused.
#[test]
fn im_t14_special_files_and_hard_links() {
    let scratch = Scratch::new("im_t14");
    let image = unpacked(
        &scratch,
        &[tar_layer(vec![
            Entry::Char("null"),
            Entry::Fifo("pipe"),
            Entry::dir("bin"),
            Entry::file("bin/busybox", b"the program"),
            Entry::Hardlink("bin/sh", "bin/busybox"),
            Entry::Symlink("bin/ls", "busybox"),
            Entry::Hardlink("bin/dir", "./bin/ls"),
        ])],
    )
    .unwrap();
    assert_eq!(image.skipped, 2);
    assert!(absent(&image.rootfs, "null") && absent(&image.rootfs, "pipe"));
    assert_eq!(text(&image.rootfs, "bin/sh"), "the program");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        fs::metadata(image.rootfs.join("bin/sh")).unwrap().ino(),
        fs::metadata(image.rootfs.join("bin/busybox"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        fs::read_link(image.rootfs.join("bin/dir"))
            .unwrap()
            .to_str()
            .unwrap(),
        "busybox"
    );

    let scratch = Scratch::new("im_t14_missing");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![Entry::Hardlink("h", "nothing")])],
    ));
    assert!(e.contains("an earlier entry must make"), "{e}");
    let scratch = Scratch::new("im_t14_nowhere");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![Entry::Hardlink("h", "no/such/dir")])],
    ));
    assert!(e.contains("an earlier entry must make"), "{e}");
    let scratch = Scratch::new("im_t14_dir");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![Entry::dir("d"), Entry::Hardlink("h", "d")])],
    ));
    assert!(e.contains("hard link to a directory"), "{e}");
}

/// IM-T15: the config's entrypoint, cmd, environment, working directory and user are read and
/// nothing else; null and missing fields are empty; a mount point that is not a directory is
/// refused.
#[test]
fn im_t15_the_config_is_read() {
    let scratch = Scratch::new("im_t15");
    let layout = scratch.path().join("layout");
    LayoutBuilder::new(&layout)
        .config(serde_json::json!({
            "Entrypoint": ["/bin/app", "--serve"], "Cmd": ["--port", "80"],
            "Env": ["PATH=/bin", "LANG=C"], "WorkingDir": "/srv", "User": "1000:1000",
            "ExposedPorts": {"80/tcp": {}}, "Labels": {"a": "b"}}))
        .layer(media::TAR, &[Entry::file("a", b"1")])
        .finish(Some("app"));
    let image = unpack(
        &OciLayout::open(&layout).unwrap(),
        "app",
        &scratch.path().join("cache"),
    )
    .unwrap();
    let c = &image.config;
    assert_eq!(c.entrypoint, ["/bin/app", "--serve"]);
    assert_eq!(c.cmd, ["--port", "80"]);
    assert_eq!(c.command(), ["/bin/app", "--serve", "--port", "80"]);
    assert_eq!(c.env, ["PATH=/bin", "LANG=C"]);
    assert_eq!(
        (c.working_dir.as_str(), c.user.as_str()),
        ("/srv", "1000:1000")
    );

    let nulls =
        crate::Config::parse(br#"{"config": {"Cmd": null, "Env": null, "User": null}}"#).unwrap();
    assert_eq!(nulls, crate::Config::default());
    assert_eq!(
        crate::Config::parse(b"{}").unwrap(),
        crate::Config::default()
    );
    assert!(crate::Config::parse(b"3").is_err());

    let scratch = Scratch::new("im_t15_mount");
    let e = refused(unpacked(
        &scratch,
        &[tar_layer(vec![Entry::Symlink("proc", "x")])],
    ));
    assert!(e.contains("`/proc`, which is not a directory"), "{e}");
}

/// IM-T14: links with no target, a hard link to the root, a cache that cannot be made and a
/// global header in a layer are each named or passed over, never half done.
#[test]
fn im_t14_odd_entries_and_a_cache_that_cannot_be_made() {
    for (entry, said) in [
        (Entry::Symlink("s", ""), "symbolic link with no target"),
        (Entry::Hardlink("h", ""), "hard link with no target"),
        (Entry::Hardlink("h", "."), "an earlier entry must make"),
    ] {
        let scratch = Scratch::new("im_t14_odd");
        let e = refused(unpacked(&scratch, &[tar_layer(vec![entry])]));
        assert!(e.contains(said), "{said}: {e}");
    }

    let scratch = Scratch::new("im_t14_global");
    let mut header = tar::Header::new_gnu();
    header.set_path("pax_global_header").unwrap();
    header.set_entry_type(tar::EntryType::XGlobalHeader);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, &[][..]).unwrap();
    let mut file = tar::Header::new_gnu();
    file.set_mode(0o644);
    file.set_size(1);
    builder.append_data(&mut file, "kept", &b"1"[..]).unwrap();
    let bytes = builder.into_inner().unwrap();
    let layout = scratch.path().join("layout");
    LayoutBuilder::new(&layout)
        .layer_bytes(media::TAR, &bytes)
        .finish(Some("app"));
    let image = unpack(
        &OciLayout::open(&layout).unwrap(),
        "app",
        &scratch.path().join("cache"),
    )
    .unwrap();
    assert_eq!(text(&image.rootfs, "kept"), "1");

    // A cache that is a file cannot be made into a directory, and the refusal says where.
    let file = scratch.path().join("a-file");
    fs::write(&file, b"x").unwrap();
    let e = unpack(
        &OciLayout::open(&layout).unwrap(),
        "app",
        &file.join("cache"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("making the cache") && e.contains("a-file"),
        "{e}"
    );
}
