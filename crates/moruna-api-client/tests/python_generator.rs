//! The Python client's generated module (item 2.5, "Drift test"): `tools/api/gen_python.py`
//! regenerates `python/moruna/api/_generated.py` from `kernel/api/openapi.json` exactly, and its
//! `--check` fails when the document changes without regenerating.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The interpreter: `python3`, or what `MORUNA_API_TEST_PYTHON` names.
fn python() -> Command {
    let python = std::env::var("MORUNA_API_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut command = Command::new(python);
    command.env("PYTHONDONTWRITEBYTECODE", "1");
    command
}

/// The generator with these arguments.
fn generate(args: &[&Path]) -> Output {
    let mut command = python();
    command.arg(root().join("tools/api/gen_python.py"));
    command.args(args);
    command.output().expect("python3 runs the generator")
}

/// A directory of this test's own, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "moruna-gen-python-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// The committed OpenAPI document, changed by `change`, written here.
    fn document(&self, change: impl FnOnce(&mut Value)) -> PathBuf {
        let text = std::fs::read_to_string(root().join("kernel/api/openapi.json")).unwrap();
        let mut document: Value = serde_json::from_str(&text).unwrap();
        change(&mut document);
        let path = self.0.join("openapi.json");
        std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_committed_module_is_what_the_document_generates() {
    let output = generate(&[Path::new("--check")]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn the_check_fails_when_the_document_changes_without_regenerating() {
    let scratch = Scratch::new();
    let changed = scratch.document(|document| {
        let node = &mut document["components"]["schemas"]["Node"];
        node["properties"]["region"] = json!({"type": "string", "description": "Where it runs."});
        node["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("region"));
    });
    let output = generate(&[Path::new("--check"), Path::new("--openapi"), &changed]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("_generated.py is not what")
            && stderr(&output).contains("run tools/api/gen_python.py"),
        "{}",
        stderr(&output)
    );

    // Regenerated, the module has the new field, and its check passes.
    let module = scratch.0.join("_generated.py");
    let output = generate(&[
        Path::new("--openapi"),
        &changed,
        Path::new("--output"),
        &module,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = std::fs::read_to_string(&module).unwrap();
    assert!(
        text.contains("    region: str\n    \"\"\"Where it runs.\"\"\"\n"),
        "{text}"
    );
    assert!(
        text.contains("region=_str(*_at(data, \"region\", where)),"),
        "{text}"
    );
    let check = [
        Path::new("--check"),
        Path::new("--openapi"),
        &changed,
        Path::new("--output"),
        &module,
    ];
    assert!(generate(&check).status.success());
}

#[test]
fn the_check_fails_when_the_module_is_missing() {
    let scratch = Scratch::new();
    let missing = scratch.0.join("missing.py");
    let output = generate(&[Path::new("--check"), Path::new("--output"), &missing]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[test]
fn a_shape_the_generator_does_not_read_stops_it_by_name() {
    type Change = fn(&mut Value);
    let cases: [(Change, &str); 5] = [
        (
            |d| {
                let node = &mut d["components"]["schemas"]["Node"];
                node["properties"]["flag"] = json!({"type": "boolean"});
                node["required"].as_array_mut().unwrap().push(json!("flag"));
            },
            "Node.flag: a field schema of type 'boolean'",
        ),
        (
            |d| {
                d["components"]["schemas"]["Node"]["properties"]["cloud"] =
                    json!({"$ref": "#/definitions/Cloud"})
            },
            "Node.cloud: a reference outside the document's schemas: #/definitions/Cloud",
        ),
        (
            |d| {
                d["components"]["schemas"]["Node"]["properties"]["region"] =
                    json!({"type": "string"});
            },
            "Node: an optional field",
        ),
        (
            |d| d["components"]["schemas"]["Region"] = json!({"type": "string"}),
            "Region: neither an object nor a set of string constants",
        ),
        (
            |d| {
                let node = &mut d["components"]["schemas"]["Node"];
                node["properties"]["class"] = json!({"type": "string"});
                node["required"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("class"));
            },
            "Node.class: not a Python name",
        ),
    ];
    for (change, said) in cases {
        let scratch = Scratch::new();
        let document = scratch.document(change);
        let module = scratch.0.join("_generated.py");
        let output = generate(&[
            Path::new("--openapi"),
            &document,
            Path::new("--output"),
            &module,
        ]);
        assert_eq!(output.status.code(), Some(1), "{said}: {}", stderr(&output));
        assert!(
            stderr(&output).contains(&format!("unsupported schema: {said}")),
            "{said}: {}",
            stderr(&output)
        );
        assert!(!module.exists(), "{said}: a module was written");
    }
}

#[test]
fn names_and_descriptions_are_made_safe_for_python() {
    let scratch = Scratch::new();
    let document = scratch.document(|d| {
        let schemas = &mut d["components"]["schemas"];
        schemas["Factor"] = json!({
            "description": "A second factor.\n\nIts \\ backslash and \"\"\" quotes stay text.",
            "oneOf": [{"const": "2fa", "type": "string"}, {"const": "web-authn", "type": "string"}]
        });
        let node = &mut schemas["Node"];
        node["properties"]["factors"] =
            json!({"type": "array", "items": {"$ref": "#/components/schemas/Factor"}});
        node["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("factors"));
    });
    let module = scratch.0.join("_generated.py");
    let output = generate(&[
        Path::new("--openapi"),
        &document,
        Path::new("--output"),
        &module,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = std::fs::read_to_string(&module).unwrap();
    for expected in [
        "    _2FA = \"2fa\"\n",
        "    WEB_AUTHN = \"web-authn\"\n",
        "    \"\"\"A second factor.\n\n    Its \\\\ backslash and \\\"\\\"\\\" quotes stay text.\n    \"\"\"\n",
        "factors=[_enum(Factor, *item) for item in _items(*_at(data, \"factors\", where))],",
        "\"factors\": [item.value for item in self.factors],",
    ] {
        assert!(text.contains(expected), "{expected:?} not in:\n{text}");
    }
    // The module runs, and reads and writes the new field.
    let check = "import sys, runpy\n\
        m = runpy.run_path(sys.argv[1])\n\
        d = {'version': '0.7.0', 'instance': 'i', 'cloud': 'none', 'cores': 1,\n\
             'memory_bytes': 1, 'executors': [], 'boot_ms': 1, 'time': 1, 'health': 'ok',\n\
             'factors': ['2fa']}\n\
        n = m['Node'].from_json(d)\n\
        assert n.factors == [m['Factor']._2FA] and n.to_json() == d, n\n\
        assert 'backslash' in m['Factor'].__doc__\n";
    let output = python()
        .args(["-c", check])
        .arg(&module)
        .output()
        .expect("python3 runs the module");
    assert!(output.status.success(), "{}", stderr(&output));
}
