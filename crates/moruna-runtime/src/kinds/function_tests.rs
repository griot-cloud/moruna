//! The `function` kind on any host (item 3.6): what a registered function's file is, by its
//! content, and an image function's process as its archive says. The stage itself runs on a
//! Moruna node, with the file `node-admission` gives the engine.

use std::sync::Arc;

use moruna_image::OciLayout;
use moruna_image::testing::{Entry as Tar, LayoutBuilder, Scratch, media};
use moruna_micro::layout::NATIVE_BASE;
use serde_json::{Map, json};

use super::*;
use crate::kinds::{Context, Entry as KindEntry, Registry, Standalone};

/// An ELF header and its program headers, (type, flags, vaddr) each, of an x86-64 static
/// executable.
fn elf(headers: &[(u32, u32, u64)]) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes());
    b[18..20].copy_from_slice(&62u16.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&(headers.len() as u16).to_le_bytes());
    for (ty, flags, vaddr) in headers {
        let mut h = [0u8; 56];
        h[..4].copy_from_slice(&ty.to_le_bytes());
        h[4..8].copy_from_slice(&flags.to_le_bytes());
        h[16..24].copy_from_slice(&vaddr.to_le_bytes());
        b.extend_from_slice(&h);
    }
    b
}

/// A program of the kernel's user target: text, read-only data, data.
fn native() -> Vec<u8> {
    elf(&[
        (1, 5, NATIVE_BASE),
        (1, 4, NATIVE_BASE + 0x10000),
        (1, 6, NATIVE_BASE + 0x20000),
    ])
}

/// A Linux program: its program headers start with the headers' own (PT_PHDR).
fn linux() -> Vec<u8> {
    elf(&[(6, 4, 0x40), (1, 5, NATIVE_BASE)])
}

/// The archive of an image of two layers whose config is `config`; its bytes.
fn archive_of(scratch: &Scratch, name: &str, config: serde_json::Value, program: &[u8]) -> Vec<u8> {
    let root = scratch.path().join(format!("{name}-layout"));
    let digest = LayoutBuilder::new(&root)
        .config(config)
        .layer(
            media::TAR,
            &[
                Tar::dir("usr/"),
                Tar::dir("usr/bin/"),
                Tar::file("usr/bin/score", program),
            ],
        )
        .layer(
            media::GZIP,
            &[Tar::dir("srv/"), Tar::file("srv/model.txt", b"weights\n")],
        )
        .finish(Some(name));
    let source = OciLayout::open(&root).unwrap();
    let out = scratch.path().join(format!("{name}.mia"));
    moruna_image::archive::write(&source, &digest.to_string(), &out).unwrap();
    std::fs::read(&out).unwrap()
}

fn config(cmd: &[&str]) -> serde_json::Value {
    json!({"Cmd": cmd, "Env": ["PATH=/usr/local/bin:/usr/bin", "MORUNA_ENTRY=scoring:Score"]})
}

#[test]
fn a_functions_file_is_what_its_content_says() {
    assert!(matches!(content("add_one", &native()), Ok(Content::Native)));
    let scratch = Scratch::new("function_content");
    let bytes = archive_of(&scratch, "score", config(&["score"]), &linux());
    let Ok(Content::Image(archive)) = content("taxi.score", &bytes) else {
        panic!("an archive is an image");
    };
    assert_eq!(archive.layers.len(), 2);
    assert!(format!("{:?}", Content::Native).contains("Native"));
}

#[test]
fn anything_else_is_refused_naming_the_function() {
    let program = content("taxi.clean", &linux()).unwrap_err();
    assert!(
        program.contains("`taxi.clean`") && program.contains("not one of the Moruna kernel's"),
        "{program}"
    );
    let zip = content("taxi.clean", b"PK\x03\x04rest").unwrap_err();
    assert!(
        zip.contains("`taxi.clean` is neither") && zip.contains("0x504b0304"),
        "{zip}"
    );
    let empty = content("taxi.clean", b"").unwrap_err();
    assert!(empty.contains("begins with nothing"), "{empty}");
    // An archive cut short is an archive that is not one.
    let scratch = Scratch::new("function_cut");
    let bytes = archive_of(&scratch, "cut", config(&["score"]), &linux());
    let cut = content("taxi.score", &bytes[..bytes.len() - 4096]).unwrap_err();
    assert!(
        cut.contains("a Moruna image archive that is not one"),
        "{cut}"
    );
}

#[test]
fn an_image_functions_command_is_found_in_its_own_files() {
    let scratch = Scratch::new("function_program");
    let bytes = archive_of(&scratch, "score", config(&["score", "--fast"]), &linux());
    let archive = Archive::parse(&bytes).unwrap();
    let args = Map::from_iter([("year".to_string(), json!(2024))]);
    let params = Map::from_iter([("threshold".to_string(), json!(0.5))]);
    let env = runner_env(&params, &args);
    let program = image_program("taxi.score", &bytes, &archive, &env).unwrap();
    assert_eq!(program.path, "/usr/bin/score");
    assert_eq!(program.launch.command, ["score", "--fast"]);
    assert_eq!(program.digest, archive.image.digest.to_string());
    let entries = program.launch.env_entries();
    for wanted in [
        "MORUNA_ENTRY=scoring:Score",
        "MORUNA_ROLE=function",
        "MORUNA_PARAMS={\"threshold\":0.5}",
        "MORUNA_ARGS={\"year\":2024}",
    ] {
        assert!(
            entries.iter().any(|e| e == wanted),
            "{wanted} in {entries:?}"
        );
    }
    // Each layer where the archive's index places it, sealed as it says.
    let expected: Vec<(u64, u64, [u8; 32])> = archive
        .layers
        .iter()
        .map(|l| (l.span.offset, l.span.pages, l.seal))
        .collect();
    assert_eq!(program.layers, expected);
    // An absolute command is taken as it is.
    let bytes = archive_of(&scratch, "abs", config(&["/usr/bin/score"]), &linux());
    let archive = Archive::parse(&bytes).unwrap();
    let program = image_program("taxi.score", &bytes, &archive, &env).unwrap();
    assert_eq!(program.path, "/usr/bin/score");
}

#[test]
fn an_image_function_the_personality_cannot_run_is_refused() {
    let scratch = Scratch::new("function_refusals");
    let env = runner_env(&Map::new(), &Map::new());
    assert_eq!(
        env["MORUNA_PARAMS"], "{}",
        "a function given no params has none"
    );
    let refused = |name: &str, config: serde_json::Value, program: &[u8]| {
        let bytes = archive_of(&scratch, name, config, program);
        let archive = Archive::parse(&bytes).unwrap();
        image_program("taxi.score", &bytes, &archive, &env).unwrap_err()
    };
    let missing = refused("missing", config(&["python3"]), &linux());
    assert!(
        missing.contains("`taxi.score`")
            && missing.contains("`python3` is not in the image")
            && missing.contains("/usr/local/bin/python3, /usr/bin/python3"),
        "{missing}"
    );
    let user = refused("user", json!({"Cmd": ["score"], "User": "1000"}), &linux());
    assert!(user.contains("uid 1000"), "{user}");
    let dir = refused(
        "dir",
        json!({"Cmd": ["score"], "WorkingDir": "/srv"}),
        &linux(),
    );
    assert!(dir.contains("working directory is /srv"), "{dir}");
    let native = refused("native", config(&["score"]), &native());
    assert!(
        native.contains("/usr/bin/score` is a program of the Moruna kernel's user target"),
        "{native}"
    );
    let none = refused("none", json!({}), &linux());
    assert!(none.contains("names no command"), "{none}");
}

#[test]
fn a_function_task_is_declared_and_staged_only_on_a_node() {
    let declared = Function.declaration();
    assert_eq!(
        (declared.name.as_str(), declared.role),
        (KIND, Role::Kernel)
    );
    assert!(
        Registry::standard()
            .listing()
            .kernels
            .iter()
            .any(|d| d.name == KIND),
        "the standard registry has the kind"
    );
    let doc = moruna_job::KindDoc::new(
        KIND.to_string(),
        Map::from_iter([("function".to_string(), json!("taxi.clean"))]),
    );
    let entry = KindEntry::new("kernels[0]", Some(0), &doc);
    let mut cx = Context::new(Arc::new(Standalone));
    let Err(refused) = Function.stage(&entry, &mut cx) else {
        panic!("a function task is staged on a node only");
    };
    assert!(
        refused.to_string().contains("runs on a Moruna node"),
        "{refused}"
    );
    // Settings that are not the kind's are refused as settings.
    let doc = moruna_job::KindDoc::new(
        KIND.to_string(),
        Map::from_iter([("image".to_string(), json!("taxi.clean"))]),
    );
    let entry = KindEntry::new("kernels[0]", Some(0), &doc);
    let Err(refused) = Function.stage(&entry, &mut cx) else {
        panic!("unknown settings");
    };
    assert!(
        refused.to_string().contains("`function` settings"),
        "{refused}"
    );
    // Params are the kind's, an object; anything else is refused as settings.
    let with_params: Settings =
        serde_json::from_value(json!({"function": "taxi.score", "params": {"threshold": 0.5}}))
            .unwrap();
    assert_eq!(with_params.params["threshold"], json!(0.5));
    assert_eq!(declared.examples().len(), 2);
    let doc = moruna_job::KindDoc::new(
        KIND.to_string(),
        Map::from_iter([
            ("function".to_string(), json!("taxi.score")),
            ("params".to_string(), json!([1])),
        ]),
    );
    let entry = KindEntry::new("kernels[0]", Some(0), &doc);
    let Err(refused) = Function.stage(&entry, &mut cx) else {
        panic!("params that are not an object");
    };
    assert!(
        refused.to_string().contains("`function` settings"),
        "{refused}"
    );
}
