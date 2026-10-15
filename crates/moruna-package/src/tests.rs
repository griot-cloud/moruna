//! PK-T1 to PK-T6 (16 k).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::arrow::datatypes::DataType;
use moruna_kernel::declare::{ColumnDecl, SchemaDecl, TypeDecl};
use serde_json::{Value, json};

use super::*;
use crate::manifest::{schema_decl, valid_pex_entry};

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A directory unique to this process and call.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moruna-package-test-{}-{tag}-{}",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pex_manifest_value() -> Value {
    json!({
        "name": "shout",
        "entry": "function:shout",
        "declaration": {
            "input_schema": {"name": "string"},
            "output_schema": {"adds": {"loud": "string"}},
            "hints": {"preferred_rows": 8192}
        },
        "abi": moruna_kernel::declare::ABI_VERSION,
        "moruna": "0.4.3",
        "format": "pex",
        "platforms": ["cp314-manylinux_2_28_aarch64", "cp314-manylinux_2_28_x86_64"]
    })
}

fn wasm_manifest_value() -> Value {
    json!({
        "name": "echo",
        "entry": "moruna:kernel",
        "declaration": {"input_schema": {"a": "int64"}, "output_schema": {}, "hints": {}},
        "abi": moruna_kernel::declare::ABI_VERSION,
        "moruna": "0.4.3",
        "format": "wasm",
        "platforms": ["wasm32"]
    })
}

/// A `.pex` with `manifest` and a source file, written to `dir/<name>.pex`.
fn write_pex(dir: &Path, manifest: &Value) -> PathBuf {
    let name = manifest["name"].as_str().unwrap_or("x");
    let path = dir.join(format!("{name}.pex"));
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("__main__.py", opts).unwrap();
    zip.write_all(b"print(1)\n").unwrap();
    zip.start_file(MANIFEST_NAME, opts).unwrap();
    zip.write_all(serde_json::to_string(manifest).unwrap().as_bytes())
        .unwrap();
    zip.start_file("src/function.py", opts).unwrap();
    zip.write_all(b"def shout(b):\n    return b\n").unwrap();
    zip.add_directory("deps/cp314-manylinux_2_28_x86_64/", opts)
        .unwrap();
    zip.start_file("deps/cp314-manylinux_2_28_x86_64/lib.py", opts)
        .unwrap();
    zip.write_all(b"X = 1\n").unwrap();
    zip.finish().unwrap();
    path
}

/// The smallest component: no imports, no exports.
fn empty_component() -> Vec<u8> {
    wat::parse_str("(component)").unwrap()
}

fn field_of(e: PackageError) -> String {
    match e {
        PackageError::Format { field, .. } => field,
        other => panic!("expected a format refusal, got {other}"),
    }
}

#[test]
fn pk_t1_manifest_round_trip() {
    let manifest = Manifest::from_value(&pex_manifest_value()).unwrap();
    let text = manifest.to_json();
    assert!(text.ends_with("}\n"));
    let keys: Vec<usize> = [
        "\"name\"",
        "\"entry\"",
        "\"declaration\"",
        "\"abi\"",
        "\"moruna\"",
        "\"format\"",
        "\"platforms\"",
    ]
    .iter()
    .map(|k| text.find(k).unwrap())
    .collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "16 e.1 key order");
    assert_eq!(Manifest::parse(text.as_bytes()).unwrap(), manifest);
    let wasm = Manifest::from_value(&wasm_manifest_value()).unwrap();
    assert_eq!(Manifest::parse(wasm.to_json().as_bytes()).unwrap(), wasm);

    // Each field removed.
    for field in [
        "name",
        "entry",
        "declaration",
        "abi",
        "moruna",
        "format",
        "platforms",
    ] {
        let mut v = pex_manifest_value();
        v.as_object_mut().unwrap().remove(field);
        assert_eq!(field_of(Manifest::from_value(&v).unwrap_err()), field);
    }
    let refused = |edit: &dyn Fn(&mut Value), field: &str| {
        let mut v = pex_manifest_value();
        edit(&mut v);
        assert_eq!(field_of(Manifest::from_value(&v).unwrap_err()), field);
    };
    refused(&|v| v["extra"] = json!(1), "extra");
    refused(&|v| v["abi"] = json!(999), "abi");
    refused(&|v| v["abi"] = json!("1"), "abi");
    refused(&|v| v["format"] = json!("zip"), "format");
    refused(&|v| v["name"] = json!("9bad"), "name");
    refused(&|v| v["name"] = json!(7), "name");
    refused(&|v| v["entry"] = json!("function"), "entry");
    refused(&|v| v["platforms"] = json!([]), "platforms");
    refused(&|v| v["platforms"] = json!("x"), "platforms");
    refused(&|v| v["platforms"] = json!([1]), "platforms");
    refused(
        &|v| {
            v["platforms"] = json!([
                "cp314-manylinux_2_28_x86_64",
                "cp314-manylinux_2_28_aarch64"
            ])
        },
        "platforms",
    );
    refused(
        &|v| v["platforms"] = json!(["cp314t-manylinux_2_28_x86_64"]),
        "platforms",
    );
    refused(
        &|v| v["platforms"] = json!(["cp314-macosx_14_0_arm64"]),
        "platforms",
    );
    refused(&|v| v["declaration"] = json!([]), "declaration");
    refused(
        &|v| v["declaration"]["other"] = json!({}),
        "declaration.other",
    );
    refused(
        &|v| v["declaration"]["hints"]["fast"] = json!(true),
        "declaration.hints.fast",
    );
    refused(
        &|v| v["declaration"]["hints"] = json!([]),
        "declaration.hints",
    );
    for (hint, value) in [
        ("stateful", json!(1)),
        ("args", json!("yes")),
        ("memory_guard", json!("no")),
        ("expected_amplification", json!(0)),
        ("preferred_rows", json!(0)),
        ("state_bytes", json!(-1)),
        ("resume", json!("later")),
    ] {
        refused(
            &|v| v["declaration"]["hints"][hint] = value.clone(),
            &format!("declaration.hints.{hint}"),
        );
    }
    refused(
        &|v| v["declaration"]["input_schema"] = json!({"a": "no-such-type"}),
        "declaration.input_schema.a",
    );
    let mut w = wasm_manifest_value();
    w["entry"] = json!("function:x");
    assert_eq!(field_of(Manifest::from_value(&w).unwrap_err()), "entry");
    let mut w = wasm_manifest_value();
    w["platforms"] = json!(["cp314-manylinux_2_28_x86_64"]);
    assert_eq!(field_of(Manifest::from_value(&w).unwrap_err()), "platforms");
    let mut w = wasm_manifest_value();
    w["declaration"]["hints"]["resume"] = json!("checkpoint");
    assert_eq!(
        field_of(Manifest::from_value(&w).unwrap_err()),
        "declaration.hints.resume"
    );
    assert!(Manifest::parse(b"not json").is_err());
    assert!(Manifest::parse(b"[1]").is_err());
    assert!(Manifest::parse(&vec![b' '; manifest::MANIFEST_MAX_BYTES + 1]).is_err());

    // Every hint, round-tripped.
    let mut v = pex_manifest_value();
    v["declaration"]["hints"] = json!({
        "stateful": true, "args": true, "expected_amplification": 1.5, "preferred_rows": 10,
        "state_bytes": 0, "memory_guard": false, "resume": "checkpoint"
    });
    let m = Manifest::from_value(&v).unwrap();
    let h = m.hints();
    assert!(h.stateful && h.args && !h.memory_guard);
    assert_eq!(h.expected_amplification, Some(1.5));
    assert_eq!(h.preferred_rows, Some(10));
    assert_eq!(h.state_bytes, Some(0));
    assert_eq!(h.resume.as_deref(), Some("checkpoint"));
    assert_eq!(Manifest::parse(m.to_json().as_bytes()).unwrap(), m);
    assert!(valid_pex_entry("a.b:c.d") && !valid_pex_entry("a:") && !valid_pex_entry(":b"));
}

#[test]
fn pk_t2_fingerprint_is_the_file() {
    let dir = scratch("t2");
    let path = write_pex(&dir, &pex_manifest_value());
    let bytes = std::fs::read(&path).unwrap();
    let package = Package::open(&path).unwrap();
    assert_eq!(package.fingerprint, fingerprint_of(&bytes));
    assert_eq!(package.bytes(), bytes.as_slice());
    let hexed = hex(&package.fingerprint);
    assert_eq!(package.fingerprint_string(), format!("sha256:{hexed}"));
    assert_eq!(package.kernel_fingerprint().0, package.fingerprint);
    package.check_pin(&format!("sha256:{hexed}")).unwrap();
    package.check_pin(&hexed.to_ascii_uppercase()).unwrap();
    let e = package.check_pin("blake3:00").unwrap_err();
    assert_eq!(field_of(e), "fingerprint");
    match package.check_pin(&format!("sha256:{}", "0".repeat(64))) {
        Err(PackageError::Pin { pinned, actual }) => {
            assert!(pinned.ends_with(&"0".repeat(64)));
            assert_eq!(actual, format!("sha256:{hexed}"));
            let text = PackageError::Pin { pinned, actual }.to_string();
            assert!(text.contains(&hexed));
        }
        other => panic!("expected a pin refusal, got {other:?}"),
    }
    // One byte changed is another fingerprint.
    let mut changed = bytes.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    assert_ne!(fingerprint_of(&changed), package.fingerprint);
    assert!(format!("{package:?}").contains(&hexed));
    let e: moruna_kernel::MorunaError = PackageError::Pin {
        pinned: "a".into(),
        actual: "b".into(),
    }
    .into();
    assert!(matches!(e, moruna_kernel::MorunaError::Plan(_)));
    let e: moruna_kernel::MorunaError = PackageError::Unsupported("x".into()).into();
    assert!(matches!(e, moruna_kernel::MorunaError::Config { .. }));

    // Not a package, not a zip, no manifest, the wrong format.
    assert!(Package::open(&dir.join("x.zip")).is_err());
    assert!(matches!(
        Package::open(&dir.join("missing.pex")),
        Err(PackageError::Io { .. })
    ));
    std::fs::write(dir.join("junk.pex"), b"junk").unwrap();
    assert_eq!(
        field_of(Package::open(&dir.join("junk.pex")).unwrap_err()),
        "pex"
    );
    let empty = dir.join("empty.pex");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&empty).unwrap());
    zip.start_file("a", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.finish().unwrap();
    assert_eq!(field_of(Package::open(&empty).unwrap_err()), MANIFEST_NAME);
    let mut w = wasm_manifest_value();
    w["name"] = json!("mixed");
    let mixed = write_pex(&dir, &w);
    assert_eq!(field_of(Package::open(&mixed).unwrap_err()), "format");
    assert!(
        PackageError::io(&dir, std::io::Error::other("x"))
            .to_string()
            .contains("x")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pk_t3_declaration_spellings() {
    let subset = schema_decl(&json!({"a": "int64", "b": "string"}), "s", false).unwrap();
    assert_eq!(
        subset,
        SchemaDecl::Subset(vec![
            ColumnDecl::new("a", DataType::Int64),
            ColumnDecl::new("b", DataType::Utf8)
        ])
    );
    let exact = schema_decl(&json!([["a", "int64"], ["b", "any", false]]), "s", false).unwrap();
    assert_eq!(
        exact,
        SchemaDecl::Exact(vec![
            ColumnDecl::new("a", DataType::Int64),
            ColumnDecl {
                name: "b".into(),
                ty: TypeDecl::Any,
                nullable: false
            }
        ])
    );
    let relative = schema_decl(
        &json!({"adds": {"c": "double"}, "drops": ["a"], "changes": {"b": "large_string"}}),
        "s",
        true,
    )
    .unwrap();
    assert_eq!(
        relative,
        SchemaDecl::Relative {
            adds: vec![ColumnDecl::new("c", DataType::Float64)],
            drops: vec!["a".into()],
            changes: vec![ColumnDecl::new("b", DataType::LargeUtf8)],
        }
    );
    // `{}` is an empty subset, not a relative declaration.
    assert_eq!(
        schema_decl(&json!({}), "s", true).unwrap(),
        SchemaDecl::Subset(vec![])
    );
    assert!(schema_decl(&json!({"adds": {}}), "s", false).is_err());
    for bad in [
        json!(1),
        json!([1]),
        json!([["a"]]),
        json!([[1, "int64"]]),
        json!([["a", "int64", "yes"]]),
        json!({"a": 1}),
        json!({"adds": []}),
        json!({"drops": "a"}),
        json!({"drops": [1]}),
    ] {
        assert!(schema_decl(&bad, "s", true).is_err(), "{bad}");
    }
    let relative_drops_null =
        schema_decl(&json!({"adds": {"x": "int8"}, "drops": null}), "s", true);
    assert!(relative_drops_null.is_ok());
    let m = Manifest::from_value(&pex_manifest_value()).unwrap();
    let declared = m.declared().unwrap();
    assert!(declared.is_checkable());
    let mut undeclared = pex_manifest_value();
    undeclared["declaration"] = json!({"input_schema": null});
    let m = Manifest::from_value(&undeclared).unwrap();
    assert!(!m.declared().unwrap().is_checkable());
    assert_eq!(m.hints(), &Hints::default());
}

#[test]
fn pk_t4_embed_and_read_section() {
    let component = empty_component();
    assert_eq!(wasm_manifest(&component).unwrap(), None);
    let manifest = Manifest::from_value(&wasm_manifest_value()).unwrap();
    let once = embed_manifest(&component, &manifest).unwrap();
    let twice = embed_manifest(&once, &manifest).unwrap();
    assert_eq!(once, twice, "embedding again replaces the section");
    let read = wasm_manifest(&twice).unwrap().unwrap();
    assert_eq!(Manifest::parse(&read).unwrap(), manifest);
    // A component that was given two sections by hand is refused.
    let mut doubled = once.clone();
    doubled.extend_from_slice(&once[8..]);
    assert_eq!(
        field_of(wasm_manifest(&doubled).unwrap_err()),
        MANIFEST_NAME
    );
    // The package reader: none is refused when opened.
    let dir = scratch("t4");
    std::fs::write(dir.join("bare.wasm"), &component).unwrap();
    assert_eq!(
        field_of(Package::open(&dir.join("bare.wasm")).unwrap_err()),
        MANIFEST_NAME
    );
    std::fs::write(dir.join("echo.wasm"), &once).unwrap();
    let package = Package::open(&dir.join("echo.wasm")).unwrap();
    assert_eq!(package.format, Format::Wasm);
    assert_eq!(package.manifest, manifest);
    // Not a component.
    let module = wat::parse_str("(module)").unwrap();
    assert!(wasm_manifest(&module).is_err());
    assert!(wasm_manifest(b"nope").is_err());
    let mut truncated = once.clone();
    truncated.truncate(once.len() - 3);
    assert!(wasm_manifest(&truncated).is_err());
    assert!(matches!(tree(&package), Err(PackageError::Format { .. })));
    assert!(write_beside(&package).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pk_t5_tree_marker() {
    let dir = scratch("t5");
    let path = write_pex(&dir, &pex_manifest_value());
    let package = Package::open(&path).unwrap();
    let first = tree(&package).unwrap();
    assert!(!first.is_private());
    assert_eq!(first.root, tree::beside(&path));
    assert_eq!(first.notes.len(), 1);
    assert!(first.src().join("function.py").is_file());
    assert!(
        first
            .deps("cp314-manylinux_2_28_x86_64")
            .join("lib.py")
            .is_file()
    );
    let marker = first.root.join(tree::MARKER);
    let written = std::fs::metadata(&marker).unwrap().modified().unwrap();
    drop(first);
    let second = tree(&package).unwrap();
    assert!(second.notes.is_empty(), "a current tree is used as it is");
    assert_eq!(
        std::fs::metadata(&marker).unwrap().modified().unwrap(),
        written
    );
    assert_eq!(write_beside(&package).unwrap(), second.root);
    // A marker naming another fingerprint is replaced.
    std::fs::write(&marker, "sha256:00 0.0.0\n").unwrap();
    let third = tree(&package).unwrap();
    assert_eq!(third.notes.len(), 1);
    assert!(tree::is_current(&third.root, &package));
    std::fs::write(&marker, "sha256:00 0.0.0\n").unwrap();
    assert_eq!(write_beside(&package).unwrap(), third.root);
    assert!(tree::is_current(&third.root, &package));

    // A directory that cannot be written: the tree goes to a private directory.
    let locked = scratch("t5-locked");
    let locked_path = write_pex(&locked, &pex_manifest_value());
    let locked_package = Package::open(&locked_path).unwrap();
    set_mode(&locked, 0o555);
    let probe = locked.join("probe");
    let writable = std::fs::write(&probe, b"x").is_ok();
    if writable {
        // The superuser writes through mode bits: the property is not constructible here, and
        // the tree is written beside the package as for any writable place.
        let _ = std::fs::remove_file(&probe);
        let t = tree(&locked_package).unwrap();
        assert!(!t.is_private());
    } else {
        let t = tree(&locked_package).unwrap();
        assert!(t.is_private());
        assert!(t.notes[0].contains("cannot be written beside the package"));
        assert!(t.src().join("function.py").is_file());
        let root = t.root.clone();
        drop(t);
        assert!(!root.exists(), "a private tree is removed with its Tree");
        assert!(write_beside(&locked_package).is_err());
    }
    set_mode(&locked, 0o755);

    // An entry that would escape the tree is refused.
    let evil = dir.join("evil.pex");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&evil).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    let mut m = pex_manifest_value();
    m["name"] = json!("evil");
    zip.start_file(MANIFEST_NAME, opts).unwrap();
    zip.write_all(serde_json::to_string(&m).unwrap().as_bytes())
        .unwrap();
    zip.start_file("../escape.py", opts).unwrap();
    zip.write_all(b"x").unwrap();
    zip.finish().unwrap();
    let evil_package = Package::open(&evil).unwrap();
    assert_eq!(field_of(tree(&evil_package).unwrap_err()), "pex");
    assert!(!dir.join("escape.py").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&locked);
}

#[cfg(unix)]
fn set_mode(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn pk_t6_platform_compatibility() {
    let host = "cp314-manylinux_2_28_x86_64";
    assert!(compatible("cp314-manylinux_2_17_x86_64", host));
    assert!(compatible("cp314-manylinux_2_28_x86_64", host));
    assert!(!compatible("cp314-manylinux_2_31_x86_64", host));
    assert!(!compatible("cp314-manylinux_2_17_aarch64", host));
    assert!(!compatible("cp313-manylinux_2_17_x86_64", host));
    assert!(!compatible("garbage", host));
    assert!(!compatible("cp314-manylinux_2_17_x86_64", "garbage"));
    assert_eq!(
        host_target(14, Some((2, 41)), "aarch64").unwrap(),
        "cp314-manylinux_2_41_aarch64"
    );
    assert!(matches!(
        host_target(14, None, "x86_64"),
        Err(PackageError::Unsupported(_))
    ));
    assert!(host_target(14, Some((2, 41)), "riscv64").is_err());
    let platforms = vec![
        "cp314-manylinux_2_28_aarch64".to_string(),
        "cp314-manylinux_2_28_x86_64".to_string(),
    ];
    assert_eq!(
        host::choose(&platforms, "cp314-manylinux_2_41_x86_64").unwrap(),
        "cp314-manylinux_2_28_x86_64"
    );
    let e = host::choose(&platforms, "cp314-manylinux_2_17_x86_64").unwrap_err();
    assert!(e.to_string().contains("cp314-manylinux_2_17_x86_64"));
    assert!(manifest::parse_target("cp3x-manylinux_2_28_x86_64").is_none());
    assert!(manifest::parse_target("cp314-manylinux_2_x_x86_64").is_none());
    assert!(manifest::parse_target("cp3014-manylinux_2_28_x86_64").is_none());
    assert!(manifest::parse_target("cp314-manylinux_02_28_x86_64").is_none());
    assert_eq!(Format::Pex.as_str(), "pex");
    assert!(WIT.contains("export process: func(batch: list<u8>) -> result<list<u8>, string>;"));
    assert!(WIT.contains("import read-asset: func(name: string) -> result<list<u8>, string>;"));
}

#[test]
fn pk_t17_inputs_are_checked_against_the_manifest() {
    let m = Manifest::from_value(&pex_manifest_value()).unwrap();
    Inputs::default().check(&m).unwrap();
    let with_args = Inputs {
        args: Some(json!({"k": 1})),
        assets: None,
    };
    assert_eq!(field_of(with_args.check(&m).unwrap_err()), "args");
    let mut v = pex_manifest_value();
    v["declaration"]["hints"] = json!({"args": true});
    let m = Manifest::from_value(&v).unwrap();
    with_args.check(&m).unwrap();
    assert_eq!(with_args.args_json(), r#"{"k":1}"#);
    assert_eq!(Inputs::default().args_json(), "{}");
    let not_object = Inputs {
        args: Some(json!([1])),
        assets: None,
    };
    assert_eq!(field_of(not_object.check(&m).unwrap_err()), "args");
    let dir = scratch("t17");
    let assets = Inputs {
        args: None,
        assets: Some(dir.clone()),
    };
    assets.check(&m).unwrap();
    let missing = Inputs {
        args: None,
        assets: Some(dir.join("missing")),
    };
    assert_eq!(field_of(missing.check(&m).unwrap_err()), "assets");
    let _ = std::fs::remove_dir_all(&dir);
}
