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

/// Python that runs a generated module (`sys.argv[1]`) and checks, for each operation ID in
/// `sys.argv[2:]`, that its function and its `Operations` method make the same call and read
/// the same answer. An operation is called with what it takes: a run's ID, a job, a function's
/// registration, or nothing.
const SAME_CALLS: &str = "import sys, runpy, inspect
m = runpy.run_path(sys.argv[1])
JOB = {'id': 'run-1', 'state': 'running', 'submitted': 1, 'submitted_ms': 1500}
FUNCTION = {'name': 'gate-score', 'address': 'https://x/gate-score', 'sha256': 'ab' * 32,
            'key': '0123456789abcdef', 'registered': 1}
ANSWERS = {
    'node': {'version': '0.7.0', 'instance': 'i', 'cloud': 'none', 'cores': 1,
             'memory_bytes': 1, 'job_memory_bytes': 1, 'executors': [], 'boot_ms': 1, 'time': 1, 'health': 'ok'},
    'jobs_validate': {'ok': True}, 'jobs_submit': {'id': 'run-1'}, 'jobs_list': {'jobs': [JOB]},
    'jobs_get': JOB, 'jobs_cancel': JOB, 'jobs_report': {'rows_out': 1},
    'functions_register': FUNCTION, 'functions_list': {'functions': [FUNCTION]}}
class Recorder(m['Operations']):
    def __init__(self):
        self.calls = []
    def call(self, operation, method, path, *, token, body=None):
        self.calls.append((operation, method, path, token, body))
        return ANSWERS.get(operation, {'status': 'ok', 'records': []})
    def stream(self, operation, path, *, token):
        self.calls.append((operation, 'GET', path, token, None))
        return iter([m['ServerSentEvent'](1, 'event', {'kind': 'attempt_started'})])
def given(function):
    parameters = list(inspect.signature(function).parameters.items())[1:]
    made = {'job': lambda: m['Job'](v1={'moruna_spec': 1}),
            'functionregistration': lambda: m['FunctionRegistration'](
                name='gate-score', address='https://x/gate-score', sha256='ab' * 32,
                signature='c2ln')}
    return [made.get(name, lambda: 'run 1')()
            for name, parameter in parameters if parameter.default is inspect.Parameter.empty]
for name in sys.argv[2:]:
    by_function, by_method = Recorder(), Recorder()
    arguments = given(m[name])
    answer, other = m[name](by_function, *arguments), getattr(by_method, name)(*arguments)
    if not isinstance(answer, (dict, list)) and hasattr(answer, '__next__'):
        answer, other = list(answer), list(other)
        assert len(answer) == 1, name
    assert other == answer, name
    assert by_function.calls == by_method.calls and len(by_method.calls) == 1, name
    assert by_method.calls[0][0] == name, by_method.calls
    if 'run 1' in arguments:
        assert '/run%201' in by_method.calls[0][2], by_method.calls
    if name in ('jobs_validate', 'jobs_submit'):
        assert by_method.calls[0][4] == {'v1': {'moruna_spec': 1}}, by_method.calls
    if name == 'functions_register':
        assert by_method.calls[0][4]['name'] == 'gate-score', by_method.calls
";

/// The operation IDs of an OpenAPI document.
fn operation_ids(document: &Path) -> Vec<String> {
    let document: Value = serde_json::from_slice(&std::fs::read(document).unwrap()).unwrap();
    let mut ids: Vec<String> = document["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|methods| methods.as_object().unwrap().values())
        .map(|operation| operation["operationId"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_committed_module_has_a_method_per_operation_that_calls_its_function() {
    let ids = operation_ids(&root().join("kernel/api/openapi.json"));
    assert_eq!(
        ids,
        [
            "functions_list",
            "functions_register",
            "health",
            "jobs_cancel",
            "jobs_events",
            "jobs_get",
            "jobs_list",
            "jobs_report",
            "jobs_submit",
            "jobs_validate",
            "node",
            "node_audit"
        ]
    );
    let module = root().join("python/moruna/api/_generated.py");
    let output = python()
        .args(["-c", SAME_CALLS])
        .arg(&module)
        .args(&ids)
        .output()
        .expect("python3 runs the module");
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn the_check_fails_when_an_operation_is_added_without_regenerating() {
    let scratch = Scratch::new();
    let changed = scratch.document(|document| {
        document["paths"]["/v1/node/ready"] = json!({"get": {
            "operationId": "node_ready",
            "summary": "Whether the node takes work.",
            "security": [{"bearer": ["observe"]}],
            "responses": {"200": {"content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/Health"}}}}}
        }});
    });
    let output = generate(&[Path::new("--check"), Path::new("--openapi"), &changed]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));

    // Regenerated, the module has the operation's function and its method on `Operations`.
    let module = scratch.0.join("_generated.py");
    let output = generate(&[
        Path::new("--openapi"),
        &changed,
        Path::new("--output"),
        &module,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = std::fs::read_to_string(&module).unwrap();
    for expected in [
        "def node_ready(client: Caller) -> Health:\n",
        "    answer = client.call(\"node_ready\", \"GET\", \"/v1/node/ready\", token=True)\n",
        "    def node_ready(self) -> Health:\n",
        "        return node_ready(self)\n",
    ] {
        assert!(text.contains(expected), "{expected:?} not in:\n{text}");
    }
    let output = python()
        .args(["-c", SAME_CALLS])
        .arg(&module)
        .args(operation_ids(&changed))
        .output()
        .expect("python3 runs the module");
    assert!(output.status.success(), "{}", stderr(&output));
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
                node["properties"]["load"] =
                    json!({"type": "object", "properties": {"one": {"type": "number"}}});
                node["required"].as_array_mut().unwrap().push(json!("load"));
            },
            "Node.load: a field schema of type 'object'",
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
                d["paths"]["/v1/node"]["get"]["parameters"] =
                    json!([{"name": "x-trace", "in": "header", "schema": {"type": "string"}}]);
            },
            "node: the parameter 'x-trace'",
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
fn an_optional_field_and_a_query_are_generated_and_read() {
    let scratch = Scratch::new();
    let document = scratch.document(|d| {
        let node = &mut d["components"]["schemas"]["Node"];
        node["properties"]["region"] = json!({"type": ["string", "null"]});
        node["properties"]["ready"] = json!({"type": "boolean"});
        // A number, as a function's measured memory has (6.3g): a float, and an integer read
        // as one.
        node["properties"]["load"] = json!({"type": "number", "format": "double"});
        d["paths"]["/v1/node"]["get"]["parameters"] =
            json!([{"name": "from", "in": "query", "schema": {"type": "integer", "minimum": 0}}]);
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
        "    region: str | None = None\n",
        "    ready: bool | None = None\n",
        "region=_maybe(data, \"region\", where, lambda at: _str(*at)),",
        "ready=_maybe(data, \"ready\", where, lambda at: _bool(*at)),",
        "    load: float | None = None\n",
        "load=_maybe(data, \"load\", where, lambda at: _float(*at)),",
        "        if self.region is not None:\n            data[\"region\"] = self.region\n",
        "def node(client: Caller, *, from_: int | None = None) -> Node:\n",
        "    answer = client.call(\"node\", \"GET\", \"/v1/node\" + _query(from_=from_), token=True)\n",
        "        return node(self, from_=from_)\n",
    ] {
        assert!(text.contains(expected), "{expected:?} not in:\n{text}");
    }
    let check = "import sys, runpy\n\
        m = runpy.run_path(sys.argv[1])\n\
        d = {'version': '0.7.0', 'instance': 'i', 'cloud': 'none', 'cores': 1,\n\
             'memory_bytes': 1, 'job_memory_bytes': 1, 'executors': [], 'boot_ms': 1, 'time': 1, 'health': 'ok'}\n\
        n = m['Node'].from_json(d)\n\
        assert n.region is None and n.ready is None and n.to_json() == d, n\n\
        assert m['Node'].from_json({**d, 'region': None}).to_json() == d\n\
        full = {**d, 'region': 'eu', 'ready': True, 'load': 0.054}\n\
        n = m['Node'].from_json(full)\n\
        assert (n.region, n.ready, n.load) == ('eu', True, 0.054) and n.to_json() == full, n\n\
        whole = m['Node'].from_json({**d, 'load': 3}).load\n\
        assert whole == 3.0 and isinstance(whole, float), whole\n\
        for bad in ({**d, 'ready': 1}, {**d, 'region': 7}, {**d, 'load': True}, {**d, 'load': '1'}):\n\
        \x20   try:\n\
        \x20       m['Node'].from_json(bad)\n\
        \x20   except ValueError:\n\
        \x20       continue\n\
        \x20   raise AssertionError(bad)\n\
        assert m['_query'](from_=None) == '' and m['_query'](from_=7) == '?from=7'\n\
        assert m['_segment']('a/b c') == 'a%2Fb%20c'\n";
    let output = python()
        .args(["-c", check])
        .arg(&module)
        .output()
        .expect("python3 runs the module");
    assert!(output.status.success(), "{}", stderr(&output));
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
             'memory_bytes': 1, 'job_memory_bytes': 1, 'executors': [], 'boot_ms': 1, 'time': 1, 'health': 'ok',\n\
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
