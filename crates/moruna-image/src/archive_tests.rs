//! A Moruna image archive written from an OCI layout and read back (items 5.4 and 3.6): the
//! layer files in place, every byte accounted for, every refusal with its reason.

use std::fs;
use std::path::Path;

use moruna_imagefs::format::PAGE;
use sha2::{Digest as _, Sha256};

use super::{Archive, INDEX_AT, MAGIC, Span, Written, is_archive, write};
use crate::testing::{Entry as Tar, LayoutBuilder, Scratch, media};
use crate::{Digest, OciLayout};

/// A layout of one image of three layers, one gzipped, one removing what one below made, under
/// the name `app`; its manifest's digest.
fn layout(root: &Path) -> Digest {
    LayoutBuilder::new(root)
        .config(serde_json::json!({"Cmd": ["/usr/bin/app"], "Env": ["PATH=/usr/bin"]}))
        .layer(
            media::TAR,
            &[
                Tar::dir("usr/"),
                Tar::dir("usr/bin/"),
                Tar::file("usr/bin/app", &vec![0x7f; 3 * PAGE + 11]),
                Tar::file("etc/gone", b"removed above"),
            ],
        )
        .layer(
            media::GZIP,
            &[
                Tar::dir("opt/"),
                Tar::file("opt/lib.py", b"print('a library')\n"),
            ],
        )
        .layer(media::TAR, &[Tar::file("etc/.wh.gone", b"")])
        .finish(Some("app"))
}

fn written(scratch: &Scratch, name: &str) -> (Written, Vec<u8>, Digest) {
    let root = scratch.path().join(format!("{name}-layout"));
    let digest = layout(&root);
    let source = OciLayout::open(&root).unwrap();
    let out = scratch.path().join(format!("{name}.archive"));
    let written = write(&source, &digest.to_string(), &out).unwrap();
    let bytes = fs::read(&out).unwrap();
    (written, bytes, digest)
}

#[test]
fn an_archive_is_its_layer_files_after_its_index() {
    let scratch = Scratch::new("archive_round_trip");
    let (written, bytes, digest) = written(&scratch, "app");
    assert_eq!(written.size, bytes.len() as u64);
    assert_eq!(written.sha256, <[u8; 32]>::from(Sha256::digest(&bytes)));
    assert_eq!(written.sha256_hex().len(), 64);
    assert_eq!(written.image, digest);
    assert!(is_archive(&bytes) && !is_archive(b"\x7fELF") && !is_archive(b"MRN"));
    assert_eq!(bytes.len() % PAGE, 0);
    // The layer files beside it are gone.
    assert!(!written.path.with_extension("layers").exists());

    let archive = Archive::parse(&bytes).unwrap();
    assert_eq!(archive.image.digest, digest);
    assert_eq!(archive.image.config.cmd, ["/usr/bin/app"]);
    assert_eq!(archive.layers.len(), 3);
    assert_eq!(archive.record.layers.len(), 3);
    // Each span is the layer file the converter makes of that layer, byte for byte.
    let source = OciLayout::open(scratch.path().join("app-layout")).unwrap();
    let converted = crate::layerfile::convert(
        &source,
        &digest.to_string(),
        &scratch.path().join("converted"),
    )
    .unwrap();
    let mut at = archive.layers[0].span.offset;
    assert_eq!(at % PAGE as u64, 0);
    for (layer, file) in archive.layers.iter().zip(&converted.layers) {
        assert_eq!(layer.span.offset, at);
        assert_eq!(layer.seal, file.id.bytes());
        let start = layer.span.offset as usize;
        let end = layer.span.end() as usize;
        assert_eq!(&bytes[start..end], fs::read(&file.path).unwrap().as_slice());
        at = layer.span.end();
    }
    assert_eq!(at, bytes.len() as u64);
}

#[test]
fn one_image_is_one_archive_bit_for_bit() {
    let scratch = Scratch::new("archive_twice");
    let (one, first, _) = written(&scratch, "one");
    let (two, second, _) = written(&scratch, "two");
    assert_eq!(first, second);
    assert_eq!(one.sha256, two.sha256);
}

#[test]
fn an_archive_is_never_written_over_and_a_failure_leaves_nothing() {
    let scratch = Scratch::new("archive_refused_write");
    let root = scratch.path().join("layout");
    let digest = layout(&root);
    let source = OciLayout::open(&root).unwrap();
    let out = scratch.path().join("there");
    fs::write(&out, b"something").unwrap();
    assert!(write(&source, &digest.to_string(), &out).is_err());
    assert_eq!(fs::read(&out).unwrap(), b"something");
    let missing = scratch.path().join("missing.archive");
    let unknown = format!("sha256:{}", "0".repeat(64));
    assert!(write(&source, &unknown, &missing).is_err());
    assert!(!missing.exists());
    assert!(!missing.with_extension("layers").exists());
}

/// The header's length word and the index, rewritten as `edit` makes it, the layers kept.
fn with_index(bytes: &[u8], edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut index: serde_json::Value =
        serde_json::from_slice(&bytes[INDEX_AT..INDEX_AT + length]).unwrap();
    edit(&mut index);
    let text = serde_json::to_vec(&index).unwrap();
    let first = index["layers"][0]["offset"].as_u64().unwrap_or(PAGE as u64) as usize;
    assert!(
        INDEX_AT + text.len() <= first,
        "the edited index fits before the layers"
    );
    let mut out = bytes.to_vec();
    out[8..12].copy_from_slice(&(text.len() as u32).to_le_bytes());
    out[INDEX_AT..first].fill(0);
    out[INDEX_AT..INDEX_AT + text.len()].copy_from_slice(&text);
    out
}

fn refused(bytes: &[u8]) -> String {
    Archive::parse(bytes).unwrap_err().to_string()
}

#[test]
fn an_archive_that_is_not_one_is_refused_with_its_reason() {
    let scratch = Scratch::new("archive_refusals");
    let (_, bytes, _) = written(&scratch, "app");
    let archive = Archive::parse(&bytes).unwrap();
    let first = archive.layers[0].span;
    let second = archive.layers[1].span;

    assert!(refused(b"\x7fELF\x02\x01\x01").contains("MRNIMAG1"));
    assert!(refused(&MAGIC).contains("inside its header"));
    let mut reserved = bytes.clone();
    reserved[12] = 1;
    assert!(refused(&reserved).contains("reserved"));
    let mut long = bytes.clone();
    long[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(refused(&long).contains("index is"));
    let mut garbled = bytes.clone();
    garbled[INDEX_AT] = b'[';
    assert!(refused(&garbled).contains("index is not one"));

    let version = with_index(&bytes, |i| i["moruna_image_archive"] = 2.into());
    assert!(refused(&version).contains("version 2"));
    let record = with_index(&bytes, |i| i["record"]["moruna_image_record"] = 9.into());
    assert!(refused(&record).contains("record of version 9"));
    let fewer = with_index(&bytes, |i| {
        i["layers"].as_array_mut().unwrap().pop();
    });
    assert!(refused(&fewer).contains("places 2 layer files"));
    let moved = with_index(&bytes, |i| {
        i["layers"][1]["offset"] = (second.offset + PAGE as u64).into()
    });
    assert!(refused(&moved).contains("layer 1 is placed"));
    let empty = with_index(&bytes, |i| i["layers"][0]["pages"] = 0.into());
    assert!(refused(&empty).contains("layer 0 is 0 pages"));
    let longer = with_index(&bytes, |i| {
        i["layers"][0]["pages"] = (first.pages + 1).into()
    });
    assert!(refused(&longer).contains("index gives it"));
    let unknown = with_index(&bytes, |i| i["extra"] = true.into());
    assert!(refused(&unknown).contains("index is not one"));

    // Bytes between the index and the first layer, after the last, or missing from the end.
    let mut between = bytes.clone();
    between[first.offset as usize - 1] = 1;
    assert!(refused(&between).contains("between its index"));
    let mut trailing = bytes.clone();
    trailing.extend_from_slice(&[0; PAGE]);
    assert!(refused(&trailing).contains("last layer ends"));
    assert!(refused(&bytes[..bytes.len() - PAGE]).contains("past the archive"));

    // A layer that is not the one recorded: its header changed, or its seal.
    let mut header = bytes.clone();
    header[first.offset as usize] ^= 0xff;
    assert!(refused(&header).contains("layer 0's header"));
    let other = with_index(&bytes, |i| {
        i["record"]["layers"][2]["seal"] = "0".repeat(64).into()
    });
    assert!(refused(&other).contains("sealed"));
    // A record whose manifest names another config.
    let config = with_index(&bytes, |i| {
        let text = i["record"]["config"]
            .as_str()
            .unwrap()
            .replace("app", "ppa");
        i["record"]["config"] = text.into();
    });
    assert!(refused(&config).contains("config"));
}

#[test]
fn a_span_ends_where_its_pages_do() {
    let span = Span {
        offset: 2 * PAGE as u64,
        pages: 3,
    };
    assert_eq!(span.end(), 5 * PAGE as u64);
}

#[test]
fn an_archives_files_are_read_in_place_across_its_layers() {
    let scratch = Scratch::new("archive_files");
    let (_, bytes, _) = written(&scratch, "app");
    let archive = Archive::parse(&bytes).unwrap();
    let mut files = archive.files(&bytes).unwrap();
    assert!(format!("{files:?}").contains("layers: 3"));
    // The lowest layer's program, the middle one's file, and what the top one removed.
    assert!(files.is_file("/usr/bin/app"));
    assert!(files.is_file("/opt/lib.py"));
    assert!(!files.is_file("/etc/gone"));
    assert!(!files.is_file("/usr/bin"), "a directory is not a file");
    assert!(!files.is_file("relative"), "a path is absolute");
    assert_eq!(files.head("/usr/bin/app", 64).unwrap(), vec![0x7f; 64]);
    // A file shorter than asked is read whole, across its pages.
    assert_eq!(
        files.head("/usr/bin/app", 1 << 20).unwrap(),
        vec![0x7f; 3 * PAGE + 11]
    );
    assert_eq!(
        files.head("/opt/lib.py", 4096).unwrap(),
        b"print('a library')\n"
    );
    let gone = files.head("/etc/gone", 1).unwrap_err().to_string();
    assert!(gone.contains("/etc/gone is not in the image"), "{gone}");
    let directory = files.head("/opt", 1).unwrap_err().to_string();
    assert!(directory.contains("is not a file"), "{directory}");
    let bad = files.head("relative", 1).unwrap_err().to_string();
    assert!(bad.contains("relative"), "{bad}");

    // A page changed after the archive was read is refused when it is read, not believed.
    let mut changed = bytes.clone();
    let span = archive.layers[0].span;
    let data = (span.offset as usize..span.end() as usize)
        .step_by(PAGE)
        .find(|at| bytes[*at..*at + PAGE].iter().all(|b| *b == 0x7f))
        .expect("a whole page of the program");
    changed[data + 7] ^= 0xff;
    let mut files = archive.files(&changed).unwrap();
    assert!(files.head("/usr/bin/app", 1 << 20).is_err());
    // A layer whose header is not sealed as the index says is not mounted at all.
    let mut header = bytes.clone();
    header[archive.layers[1].span.offset as usize + 64] ^= 0xff;
    assert!(archive.files(&header).is_err());
}
