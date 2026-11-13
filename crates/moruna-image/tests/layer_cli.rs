//! `moruna-layer` driven as its users drive it: each subcommand on a real OCI layout, and the
//! refusals a mistyped invocation gets.

use std::path::Path;
use std::process::{Command, Output};

use moruna_image::testing::{Entry, LayoutBuilder, Scratch, media};
use serde_json::json;

fn layer(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_moruna-layer"))
        .args(args)
        .output()
        .expect("moruna-layer runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn layout(root: &Path) -> String {
    LayoutBuilder::new(root)
        .config(json!({"Cmd": ["/bin/x"], "Env": ["PATH=/bin"]}))
        .layer(
            media::TAR,
            &[Entry::dir("bin/"), Entry::file("bin/x", b"#!")],
        )
        .layer(media::GZIP, &[Entry::file("etc/motd", b"hello")])
        .finish(Some("echo"));
    root.display().to_string()
}

#[test]
fn usage_is_refused_with_exit_two() {
    for args in [
        &[][..],
        &["convert"][..],
        &["derive", "a", "b", "c", "d", "e"][..],
        &["nope", "x"][..],
    ] {
        let out = layer(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage: moruna-layer"));
    }
}

#[test]
fn convert_writes_one_file_per_layer_lowest_first() {
    let scratch = Scratch::new("layer-cli");
    let src = layout(&scratch.path().join("layout"));
    let out_dir = scratch.path().join("converted");
    let out = layer(&["convert", &src, "echo", out_dir.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let lines: Vec<String> = text(&out.stdout).lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    for line in &lines {
        let path = line.rsplit(' ').next().unwrap();
        assert!(Path::new(path).is_file(), "{line}");
        assert!(line.contains(" pages "), "{line}");
    }
}

#[test]
fn stack_names_devices_from_first_and_writes_the_record_and_digest() {
    let scratch = Scratch::new("layer-cli");
    let src = layout(&scratch.path().join("layout"));
    let out_dir = scratch.path().join("stack");
    let out = layer(&["stack", &src, "echo", out_dir.to_str().unwrap(), "3"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let list = std::fs::read_to_string(out_dir.join("stack.layers")).unwrap();
    assert!(list.starts_with("moruna-layers 1\n"));
    assert!(
        list.contains("layer block3 ") && list.contains("layer block4 "),
        "{list}"
    );
    assert!(out_dir.join("stack-1.img").is_file() && out_dir.join("stack-2.img").is_file());
    assert!(
        !out_dir.join("converted").exists(),
        "the staging directory is removed"
    );
    let digest = std::fs::read_to_string(out_dir.join("image.digest")).unwrap();
    assert!(text(&out.stdout).contains(&format!("image {}", digest.trim())));
    let record = std::fs::read(out_dir.join("image.record")).unwrap();
    assert!(!record.is_empty());
    assert_ne!(
        record.len() % 4096,
        0,
        "a whole-page record gains a trailing newline"
    );
}

#[test]
fn stack_refuses_a_non_numeric_device_and_a_missing_layout() {
    let scratch = Scratch::new("layer-cli");
    let src = layout(&scratch.path().join("layout"));
    let out_dir = scratch.path().join("stack");
    let bad = layer(&["stack", &src, "echo", out_dir.to_str().unwrap(), "x1"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        text(&bad.stderr).contains("is not a device number"),
        "{}",
        text(&bad.stderr)
    );
    let missing = scratch.path().join("absent");
    let gone = layer(&[
        "convert",
        missing.to_str().unwrap(),
        "echo",
        out_dir.to_str().unwrap(),
    ]);
    assert_eq!(gone.status.code(), Some(1));
    assert!(text(&gone.stderr).starts_with("moruna-layer: "));
    let unknown = layer(&["stack", &src, "no-such-ref", out_dir.to_str().unwrap(), "1"]);
    assert_eq!(unknown.status.code(), Some(1));
}

#[test]
fn derive_adds_a_layer_and_prints_the_new_manifest_digest() {
    let scratch = Scratch::new("layer-cli");
    let base = scratch.path().join("base");
    LayoutBuilder::new(&base)
        .config_document(json!({"config": {"Cmd": ["sh"], "Env": ["PATH=/bin"]},
                                "rootfs": {"type": "layers", "diff_ids": []}}))
        .layer(media::TAR, &[Entry::file("tool", b"tool")])
        .finish(Some("echo"));
    let src = base.display().to_string();
    let tree = scratch.path().join("tree");
    std::fs::create_dir_all(tree.join("app")).unwrap();
    std::fs::write(tree.join("app/run"), b"go").unwrap();
    let out_dir = scratch.path().join("derived");
    let out = layer(&[
        "derive",
        &src,
        "echo",
        tree.to_str().unwrap(),
        "derived",
        out_dir.to_str().unwrap(),
        "/app/run",
        "--flag",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let digest = text(&out.stdout);
    assert!(digest.trim().starts_with("sha256:"), "{digest}");
    // The derived layout is itself convertible under its new name.
    let again = scratch.path().join("again");
    let converted = layer(&[
        "convert",
        out_dir.to_str().unwrap(),
        "derived",
        again.to_str().unwrap(),
    ]);
    assert!(converted.status.success(), "{}", text(&converted.stderr));
    assert_eq!(text(&converted.stdout).lines().count(), 2);
}

#[test]
fn fixture_writes_the_boot_layers_and_their_lists() {
    let scratch = Scratch::new("layer-cli");
    let out_dir = scratch.path().join("fixture");
    let out = layer(&["fixture", out_dir.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    for line in text(&out.stdout).lines() {
        let path = line.rsplit(' ').next().unwrap();
        assert!(Path::new(path).exists(), "{line}");
    }
}
