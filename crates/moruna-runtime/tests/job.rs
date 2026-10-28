//! SDD 22 k: the catalogue `moruna-job` carries is the registry's (JB-T25), the validator that
//! reads a kind's schema gives the verdict the kind itself gives (JB-T26), and the runtime
//! holds no second copy of what `moruna-job` owns (JB-T27).

// `MorunaError` is the contracts crate's error and is 128 bytes wide; every crate in the
// workspace carries the same allow rather than boxing at every boundary.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use moruna_job::schema::kind_of;
use moruna_job::{Catalogue, JobSpec, KindDoc, RefusalCode, Role, check};
use moruna_kernel::MorunaError;
use moruna_runtime::kinds::{Context, Entry, Registry, Standalone};
use serde_json::{Map, Value, json};

/// The catalogue a build of this test has: the standard kinds, and the governed ones with the
/// `peql` feature, which is what `Registry::standard()` registers.
fn catalogue() -> Catalogue {
    if cfg!(feature = "peql") {
        Catalogue::standard_with_peql().expect("the governed catalogue")
    } else {
        Catalogue::standard().expect("the standard catalogue")
    }
}

/// JB-T25 catalogue_is_the_registry: the files `moruna-job` embeds are what `moruna kinds` lists
/// for this build, declaration for declaration, so a kind added, removed or changed in the
/// runtime without the files is a failure here. When it is one, regenerate them: `moruna kinds`
/// of a build with default features is `catalogue/standard.json`, and the kinds a build with
/// `--features peql` lists beyond it are `catalogue/peql.json`.
#[test]
fn jb_t25_catalogue_is_the_registry() {
    let listing = Registry::standard().listing();
    let from_registry = Catalogue::from_listing(listing.clone()).expect("the registry's listing");
    assert_eq!(from_registry, catalogue());
    // The same kinds, listed by role in name order, as the embedded files list them.
    for (role, listed) in [
        (Role::Source, &listing.sources),
        (Role::Kernel, &listing.kernels),
        (Role::Sink, &listing.sinks),
    ] {
        let names: Vec<&str> = listed.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, catalogue().names(role).collect::<Vec<_>>());
    }
    // Without the feature the governed kinds are not the registry's, and the standard catalogue
    // is what it is.
    if !cfg!(feature = "peql") {
        assert!(from_registry.get(Role::Sink, "peql").is_none());
        assert_eq!(
            from_registry,
            Catalogue::standard().expect("the standard catalogue")
        );
    }
}

/// SplitMix64, so the mutations are the same on every machine and no crate draws them.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One step into a value.
#[derive(Clone)]
enum At {
    Key(String),
    Index(usize),
}

fn paths(value: &Value, here: &mut Vec<At>, out: &mut Vec<Vec<At>>) {
    out.push(here.clone());
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                here.push(At::Key(key.clone()));
                paths(child, here, out);
                here.pop();
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                here.push(At::Index(i));
                paths(child, here, out);
                here.pop();
            }
        }
        _ => {}
    }
}

fn at<'v>(value: &'v Value, path: &[At]) -> &'v Value {
    let mut here = value;
    for step in path {
        here = match step {
            At::Key(key) => &here[key.as_str()],
            At::Index(i) => &here[*i],
        };
    }
    here
}

fn at_mut<'v>(value: &'v mut Value, path: &[At]) -> &'v mut Value {
    let mut here = value;
    for step in path {
        here = match step {
            At::Key(key) => here.get_mut(key).expect("a key of the path"),
            At::Index(i) => here.get_mut(*i).expect("an index of the path"),
        };
    }
    here
}

/// The values a type change picks from, one of each JSON type.
fn others() -> [Value; 7] {
    [
        json!(null),
        json!(true),
        json!("a string"),
        json!(7),
        json!(7.5),
        json!([]),
        json!({}),
    ]
}

/// One mutation of `settings`: a key dropped, an unknown key added, a value changed to another
/// type, a number put out of range, a string put outside what is allowed.
fn mutate(settings: &mut Value, rng: &mut SplitMix64) {
    let mut all = Vec::new();
    paths(settings, &mut Vec::new(), &mut all);
    let operation = rng.below(5);
    let objects: Vec<&Vec<At>> = all.iter().filter(|p| at(settings, p).is_object()).collect();
    let below_root: Vec<&Vec<At>> = all.iter().filter(|p| !p.is_empty()).collect();
    match operation {
        // Drop a key.
        0 => {
            let droppable: Vec<(Vec<At>, String)> = objects
                .iter()
                .flat_map(|p| {
                    let keys: Vec<String> = at(settings, p)
                        .as_object()
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default();
                    keys.into_iter()
                        .map(|k| ((*p).clone(), k))
                        .collect::<Vec<_>>()
                })
                .collect();
            if droppable.is_empty() {
                return;
            }
            let (path, key) = &droppable[rng.below(droppable.len())];
            if let Some(object) = at_mut(settings, path).as_object_mut() {
                object.remove(key);
            }
        }
        // Add a key nothing declares.
        1 => {
            let path = objects[rng.below(objects.len())].clone();
            if let Some(object) = at_mut(settings, &path).as_object_mut() {
                object.insert("zz_unknown".into(), json!(1));
            }
        }
        // Change a value to another type.
        2 => {
            if below_root.is_empty() {
                return;
            }
            let path = below_root[rng.below(below_root.len())].clone();
            let target = at_mut(settings, &path);
            // `serde` reads a struct from a JSON array too, field by field in order, which no
            // schema says and no document means; an object made an array is the one change of
            // type the kind reads where the schema refuses, and is not made.
            let was_object = target.is_object();
            let choices: Vec<Value> = others()
                .into_iter()
                .filter(|c| kind_of(c) != kind_of(target) && !(was_object && c.is_array()))
                .collect();
            *target = choices[rng.below(choices.len())].clone();
        }
        // A number out of range, written over a number if there is one, else over any value.
        3 => {
            let numbers: Vec<&Vec<At>> = below_root
                .iter()
                .copied()
                .filter(|p| at(settings, p).is_number())
                .collect();
            let pool = if numbers.is_empty() {
                &below_root
            } else {
                &numbers
            };
            if pool.is_empty() {
                return;
            }
            let path = pool[rng.below(pool.len())].clone();
            let out_of_range = [
                json!(-1),
                json!(4294967296u64),
                json!(18446744073709551615u64),
                json!(1e300),
                json!(-1.5),
            ];
            *at_mut(settings, &path) = out_of_range[rng.below(out_of_range.len())].clone();
        }
        // A string that is not in the enum, written over a string if there is one.
        _ => {
            let strings: Vec<&Vec<At>> = below_root
                .iter()
                .copied()
                .filter(|p| at(settings, p).is_string())
                .collect();
            let pool = if strings.is_empty() {
                &below_root
            } else {
                &strings
            };
            if pool.is_empty() {
                return;
            }
            let path = pool[rng.below(pool.len())].clone();
            *at_mut(settings, &path) = json!("not-in-the-enum");
        }
    }
}

/// Where each role's entry sits in a document, and its place in `kernels`.
fn place(role: Role) -> (&'static str, Option<usize>) {
    match role {
        Role::Source => ("source", None),
        Role::Kernel => ("kernels[0]", Some(0)),
        Role::Sink => ("sink", None),
    }
}

/// The settings refusals `check` gives for the entry.
fn schema_refusals(catalogue: &Catalogue, role: Role, doc: &KindDoc) -> Vec<moruna_job::Refusal> {
    let valid = |kind: &str, url: &str| {
        KindDoc::from_value(json!({"kind": kind, "url": url})).expect("an entry")
    };
    let mut job = JobSpec::new(
        valid("parquet", "/in"),
        Vec::new(),
        valid("parquet", "/out"),
    );
    match role {
        Role::Source => job.source = doc.clone(),
        Role::Kernel => job.kernels = vec![doc.clone()],
        Role::Sink => job.sink = doc.clone(),
    }
    let (field, _) = place(role);
    // A kind that is not declared, or one that needs a contract this test does not carry, is not
    // a verdict on the settings.
    check(&job, catalogue)
        .err()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            matches!(r.code, RefusalCode::Settings { .. })
                && r.field.starts_with(&format!("{field}.settings"))
        })
        .collect()
}

/// What building a kind's part from an entry came to.
#[derive(Debug, PartialEq, Eq)]
enum Built {
    /// It was built.
    Part,
    /// The kind could not read the entry's settings as its type.
    SettingsRefused,
    /// The kind read them and refused later: its own judgement of a value (a kernel's
    /// arguments, a file that is not there, a contract the document does not carry).
    RefusedLater,
}

fn build(registry: &Registry, role: Role, doc: &KindDoc) -> Built {
    let (field, index) = place(role);
    let entry = Entry::new(field, index, doc);
    let mut cx = Context::new(Arc::new(Standalone));
    let built = match role {
        Role::Source => registry
            .source(field, &doc.kind)
            .and_then(|(_, kind)| kind.source(&entry, &mut cx).map(|_| ())),
        Role::Kernel => registry
            .kernel(field, &doc.kind)
            .and_then(|(_, kind)| kind.stage(&entry, &mut cx).map(|_| ())),
        Role::Sink => registry
            .sink(field, &doc.kind)
            .and_then(|(_, kind)| kind.sink(&entry, &mut cx).map(|_| ())),
    };
    match built {
        Ok(()) => Built::Part,
        Err(MorunaError::Config { name: "spec", msg })
            if msg.contains(&format!("`{}` settings: ", doc.kind)) =>
        {
            Built::SettingsRefused
        }
        Err(_) => Built::RefusedLater,
    }
}

/// How many mutations of each example the agreement test makes.
const MUTATIONS: usize = 200;

/// JB-T26 validator_agrees_with_the_runtime: for every declared kind, every one of its examples
/// is accepted by both, and each of 200 seeded mutations of each example (a key dropped, a key
/// added, a type changed, a number out of range, a string outside the enum, one to three at a
/// time) is accepted by both or refused by both. A disagreement names the kind and the settings.
#[test]
fn jb_t26_validator_agrees_with_the_runtime() {
    let registry = Registry::standard();
    let catalogue = catalogue();
    let mut compared = 0;
    for role in [Role::Source, Role::Kernel, Role::Sink] {
        for declared in catalogue.declarations(role) {
            let mut accepted = 0;
            let mut refused = 0;
            for (n, example) in declared.examples().into_iter().enumerate() {
                let Value::Object(base) = example else {
                    panic!("`{}` has an example that is not an object", declared.name);
                };
                let seed = declared
                    .name
                    .bytes()
                    .fold(0x4D4F_5255_4E41u64 + n as u64, |s, b| {
                        s.wrapping_mul(31).wrapping_add(u64::from(b))
                    });
                let mut rng = SplitMix64(seed);
                for i in 0..=MUTATIONS {
                    let mut settings = Value::Object(base.clone());
                    if i > 0 {
                        for _ in 0..=rng.below(3) {
                            mutate(&mut settings, &mut rng);
                        }
                    }
                    let Value::Object(map) = settings else {
                        panic!("the settings stayed an object");
                    };
                    let doc = KindDoc::new(declared.name.clone(), Map::from_iter(map));
                    let by_schema = schema_refusals(&catalogue, role, &doc);
                    let by_kind = build(&registry, role, &doc);
                    let shown = serde_json::to_string(&doc).unwrap_or_default();
                    let agrees = match (&by_kind, by_schema.is_empty()) {
                        (Built::Part, true) | (Built::SettingsRefused, false) => true,
                        // The kind's own judgement of a value the schema leaves open.
                        (Built::RefusedLater, true) => true,
                        // The one constraint a schema has that its type does not: an enum the
                        // adapter adds (`std`'s kernel names), which the kind then refuses by
                        // its own check.
                        (Built::RefusedLater, false) => by_schema
                            .iter()
                            .all(|r| r.message.contains("must be one of")),
                        (Built::Part, false) | (Built::SettingsRefused, true) => false,
                    };
                    assert!(
                        agrees,
                        "the {role} kind `{}`: the kind says {by_kind:?}, the schema says {by_schema:?}, of {shown}",
                        declared.name
                    );
                    let by_schema = by_schema.is_empty();
                    if i == 0 {
                        assert!(by_schema, "`{}` refuses its own example", declared.name);
                    }
                    if by_schema {
                        accepted += 1;
                    } else {
                        refused += 1;
                    }
                    compared += 1;
                }
            }
            // The mutations reach both verdicts, or the test would agree about nothing.
            assert!(
                refused > 0,
                "no mutation of `{}` was refused",
                declared.name
            );
            assert!(
                accepted > 0,
                "no mutation of `{}` was accepted",
                declared.name
            );
        }
    }
    assert!(compared >= 11 * 2 * MUTATIONS / 2, "{compared}");
}

/// JB-T27 one_home: nothing `moruna-job` owns is declared or re-exported by the runtime, so there
/// is one definition of each (SDD 22 JB-I2).
#[test]
fn jb_t27_the_types_have_one_home() {
    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("a directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(files.len() > 20);
    let owned = [
        "Role",
        "Flow",
        "Fact",
        "Isolation",
        "Needs",
        "Declaration",
        "Listing",
        "Urls",
        "JobSpec",
        "KindDoc",
        "SpecError",
        "ContractEntry",
        "BudgetDoc",
        "ElasticDoc",
        "StagingDoc",
        "ObjectStoreDoc",
        "S3Doc",
        "GcsDoc",
        "AzureDoc",
        "CheckpointDoc",
        "ErrorPolicyDoc",
        "SizerDoc",
        "GuaranteeDoc",
        "HostProfileDoc",
        "ReportDoc",
        "TaskIdsDoc",
        "TaskRefDoc",
    ];
    for file in files {
        let text = std::fs::read_to_string(&file).expect("a source");
        assert!(!text.contains("pub use moruna_job"), "{}", file.display());
        for name in owned {
            for keyword in ["struct", "enum", "type"] {
                let declaration = format!("pub {keyword} {name} ");
                let generic = format!("pub {keyword} {name}<");
                let alias = format!("pub {keyword} {name};");
                assert!(
                    !text.contains(&declaration)
                        && !text.contains(&generic)
                        && !text.contains(&alias),
                    "{} declares `{name}`, which is `moruna-job`'s",
                    file.display()
                );
            }
        }
    }
}

/// JB-T28 check_job_command: `moruna check-job <file>` exits 0 for a document the binary's kinds
/// accept and prints nothing; for any other it prints each refusal as `<field>: <message>` and
/// exits with the code of a refused document; a file that is not there or not a document is
/// refused the same way; the command line takes one document.
#[test]
fn jb_t28_check_job_command() {
    use moruna_runtime::host::cli::{self, Command};
    use moruna_runtime::host::exit;

    let scratch = std::env::temp_dir().join(format!("moruna-job-t21-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("a scratch directory");
    let write = |name: &str, text: &str| {
        let path = scratch.join(name);
        std::fs::write(&path, text).expect("a document");
        path
    };
    let kinds = Registry::standard();
    let run = |path: &std::path::Path| {
        let mut out = Vec::new();
        let code = cli::check_job(path, &kinds, &mut out);
        (code, String::from_utf8(out).expect("text"))
    };

    let clean = write(
        "clean.json",
        r#"{"moruna_spec":1,"source":{"kind":"parquet","url":"/in"},
            "kernels":[{"kind":"std","name":"cast","args":{"columns":{"a":"double"}}}],
            "sink":{"kind":"parquet","url":"/out"}}"#,
    );
    assert_eq!(run(&clean), (exit::COMPLETED, String::new()));

    let bad = write(
        "bad.json",
        r#"{"moruna_spec":1,"source":{"kind":"parquet"},
            "kernels":[{"kind":"java"}],
            "sink":{"kind":"parquet","url":"/out","bogus":true}}"#,
    );
    let (code, said) = run(&bad);
    assert_eq!(code, exit::SPEC_REFUSED);
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines.len(), 3, "{said}");
    assert_eq!(
        lines[0],
        "source.settings.url: `parquet` settings: `url` is required"
    );
    assert!(lines[1].starts_with("kernels[0].kind: no kernel kind `java` is declared; "));
    assert_eq!(
        lines[2],
        "sink.settings.bogus: `parquet` settings: `bogus` is not a setting of `parquet`"
    );

    let (code, said) = run(&scratch.join("missing.json"));
    assert_eq!(code, exit::SPEC_REFUSED);
    assert!(said.starts_with("spec refused: spec: "), "{said}");
    let (code, said) = run(&write("not.json", "{"));
    assert_eq!(code, exit::SPEC_REFUSED);
    assert!(said.contains("not a JSON document"), "{said}");
    let (code, said) = run(&write("old.json", r#"{"moruna_spec":2}"#));
    assert_eq!(code, exit::SPEC_REFUSED);
    assert!(said.contains("moruna_spec"), "{said}");

    // The command line: one document.
    let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    assert_eq!(
        cli::parse(&args(&["check-job", "a.json"])),
        Ok(Command::CheckJob {
            spec: "a.json".into()
        })
    );
    assert!(cli::parse(&args(&["check-job"])).is_err());
    assert!(cli::parse(&args(&["check-job", "a.json", "b.json"])).is_err());
    // Through `main`, the exit code is the same.
    let code = |file: &std::path::Path| {
        let argv = args(&["check-job", &file.display().to_string()]);
        let env = |_: &str| None;
        cli::main_with(
            &argv,
            &kinds,
            moruna_kernel::CancelToken::new(),
            &env,
            std::time::Duration::from_secs(1),
        )
    };
    assert_eq!(code(&clean), exit::COMPLETED);
    assert_eq!(code(&bad), exit::SPEC_REFUSED);

    // A registry with a kind whose schema the validator cannot read cannot be checked against:
    // the command says so and exits 1, and does not guess.
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct OddSettings {}
    struct Odd;
    impl moruna_runtime::kinds::RunnableKind for Odd {
        fn declaration(&self) -> moruna_job::Declaration {
            let mut settings = moruna_job::settings_schema::<OddSettings>("odd", vec![json!({})]);
            settings["pattern"] = json!("^x");
            moruna_job::Declaration {
                name: "odd".into(),
                role: Role::Kernel,
                summary: "a kind whose schema uses a keyword the validator does not read".into(),
                settings,
                inputs: moruna_job::Flow::Batches,
                outputs: moruna_job::Flow::Batches,
                needs: moruna_job::Needs::nothing(),
                deterministic: moruna_job::Fact::Yes,
                preserves_columns: moruna_job::Fact::Yes,
                isolation: moruna_job::Isolation::Builtin,
            }
        }

        fn stage(
            &self,
            entry: &Entry<'_>,
            _: &mut Context,
        ) -> moruna_kernel::Result<moruna_runtime::kinds::Stage> {
            Err(entry.refuse("odd", "built by a test"))
        }
    }
    let mut odd = Registry::standard();
    odd.register_kernel(Arc::new(Odd)).expect("registers");
    let mut out = Vec::new();
    assert_eq!(cli::check_job(&clean, &odd, &mut out), exit::FAILED);
    let said = String::from_utf8(out).expect("text");
    assert!(
        said.contains("the kinds of this binary cannot be checked against"),
        "{said}"
    );
    assert!(said.contains("`odd`") && said.contains("pattern"), "{said}");

    std::fs::remove_dir_all(&scratch).expect("the scratch directory goes");
}
