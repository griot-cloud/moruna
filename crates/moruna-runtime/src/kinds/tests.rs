//! SDD 18 k: the registry, the declaration, the suites and the standard kinds that need no
//! fixture (KD-T1 to KD-T4, KD-T6, KD-T10, KD-T11). KD-T5 runs a kind added by a test in
//! `tests/kinds.rs`; the suites over the package and governed kinds run with their fixtures in
//! `tests/package.rs` and `tests/governed.rs`.

use std::sync::Arc;

use moruna_kernel::{Kernel, MorunaError};
use moruna_testkit::FakeKernel;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::conformance::{self, Harness, refused_field};
use super::*;
use crate::job::KindDoc;

fn doc(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn refusal(result: Result<impl Sized>) -> MorunaError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("resolved"),
    }
}

/// A kind for the tests: a runnable kind of any name, declaration and fuse behaviour, that
/// reads `{"tag": <string>, "fingerprint"?: <string>}` and runs a fake kernel.
#[derive(Clone)]
struct Probe {
    name: &'static str,
    isolation: Isolation,
    contracts: Fact,
    fuses: bool,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProbeSettings {
    tag: String,
    #[serde(default)]
    fingerprint: Option<String>,
}

impl Probe {
    fn named(name: &'static str) -> Probe {
        Probe {
            name,
            isolation: Isolation::Builtin,
            contracts: Fact::No,
            fuses: false,
        }
    }
}

struct ProbeFuse;

impl Fuse for ProbeFuse {
    fn fuse(&self, next: &dyn Fuse) -> Option<Stage> {
        next.as_any().downcast_ref::<ProbeFuse>()?;
        let mut joined = Stage::of(Arc::new(FakeKernel::new()) as Arc<dyn Kernel>);
        joined.fuse = Some(Arc::new(ProbeFuse));
        Some(joined)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl RunnableKind for Probe {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: self.name.into(),
            role: Role::Kernel,
            summary: "a kind the tests register".into(),
            settings: settings_schema::<ProbeSettings>(self.name, vec![json!({"tag": "a"})]),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs {
                contracts: self.contracts,
                ..Needs::nothing()
            },
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: self.isolation,
        }
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> moruna_kernel::Result<Stage> {
        let settings: ProbeSettings = entry.settings()?;
        let kernel = FakeKernel::new();
        check_pin(entry, settings.fingerprint.as_deref(), &kernel)?;
        let mut stage = Stage::of(Arc::new(kernel) as Arc<dyn Kernel>);
        if self.fuses {
            stage.fuse = Some(Arc::new(ProbeFuse));
        }
        Ok(stage)
    }
}

/// KD-T1 resolve_or_refuse (KD-I1): a name resolves to the adapter registered under it for that
/// role; any other is refused at `<field>.kind` naming the kinds of the role this build has.
#[test]
fn kd_t1_resolve_or_refuse() {
    let kinds = Registry::standard();
    let (declared, _) = kinds.source("source", "parquet").expect("parquet reads");
    assert_eq!(
        (declared.name.as_str(), declared.role),
        ("parquet", Role::Source)
    );
    let (declared, _) = kinds.sink("sink", "parquet").expect("parquet writes");
    assert_eq!(declared.role, Role::Sink);
    let (declared, _) = kinds.kernel("kernels[0]", "std").expect("std runs");
    assert_eq!(declared.role, Role::Kernel);
    // A name is resolved within its role: no source is called `arrow_ipc`, no sink `std`.
    let e = refusal(kinds.source("source", "arrow_ipc"));
    let sources = if cfg!(feature = "peql") {
        "datafusion, parquet, tensor, vortex"
    } else {
        "parquet, tensor, vortex"
    };
    assert_eq!(
        e.to_string(),
        MorunaError::from(SpecError::new(
            "source.kind",
            format!("no source kind `arrow_ipc` is registered; this build reads {sources}"),
        ))
        .to_string()
    );
    let e = refusal(kinds.sink("sink", "std"));
    assert_eq!(refused_field(&e).as_deref(), Some("sink.kind"));
    assert!(
        e.to_string()
            .contains("this build writes arrow_ipc, parquet"),
        "{e}"
    );
    let e = refusal(kinds.kernel("kernels[3]", "java"));
    assert_eq!(refused_field(&e).as_deref(), Some("kernels[3].kind"));
    let empty = Registry::empty();
    let e = refusal(empty.kernel("kernels[0]", "std"));
    assert!(
        e.to_string()
            .contains("no kernel kind `std` is registered; this build has no kernel kinds"),
        "{e}"
    );
}

/// KD-T2 rust_is_not_registered (KD-I7): a `rust` entry, as v1 spells one, is refused by the
/// standard registry as a kind it does not have, with the kernel kinds it does.
#[test]
fn kd_t2_rust_is_not_registered() {
    let kinds = Registry::standard();
    let e = refusal(kinds.kernel("kernels[0]", "rust"));
    assert_eq!(
        e.to_string(),
        MorunaError::from(SpecError::new(
            "kernels[0].kind",
            "no kernel kind `rust` is registered; this build runs image, package, std",
        ))
        .to_string()
    );
}

/// KD-T3 registration_refusals (KD-I2): a declaration the registry cannot accept is refused when
/// it is registered, naming why; so is a second kind of one name and role.
#[test]
fn kd_t3_registration_refusals() {
    struct Declares(Declaration);
    impl RunnableKind for Declares {
        fn declaration(&self) -> Declaration {
            self.0.clone()
        }
        fn stage(&self, _: &Entry<'_>, _: &mut Context) -> moruna_kernel::Result<Stage> {
            Ok(Stage::of(Arc::new(FakeKernel::new()) as Arc<dyn Kernel>))
        }
    }
    let good = Probe::named("probe").declaration();
    let mut kinds = Registry::empty();
    kinds
        .register_kernel(Arc::new(Declares(good.clone())))
        .expect("registers");
    let refused = |kinds: &mut Registry, d: Declaration| -> String {
        match kinds.register_kernel(Arc::new(Declares(d))) {
            Err(MorunaError::Config { name: "kinds", msg }) => msg,
            Err(other) => panic!("refused as {other}"),
            Ok(()) => panic!("registered"),
        }
    };
    assert!(refused(&mut kinds, good.clone()).contains("registered already"));
    let with = |change: &dyn Fn(&mut Declaration)| {
        let mut d = good.clone();
        d.name = "other".into();
        change(&mut d);
        d
    };
    let cases: Vec<(Declaration, &str)> = vec![
        (with(&|d| d.name = "Upper".into()), "not a kind name"),
        (with(&|d| d.name = "9lives".into()), "not a kind name"),
        (with(&|d| d.name = "x".repeat(65)), "not a kind name"),
        (with(&|d| d.role = Role::Sink), "registered as a kernel"),
        (with(&|d| d.summary = " ".into()), "no summary"),
        (with(&|d| d.inputs = Flow::Files), "which a kernel does not"),
        (
            with(&|d| d.settings["$schema"] = json!("draft-07")),
            "dialect",
        ),
        (with(&|d| d.settings["type"] = json!("array")), "an object"),
        (
            with(&|d| d.settings["additionalProperties"] = json!(true)),
            "not closed",
        ),
        (
            with(&|d| d.settings["properties"]["kind"] = json!({"type": "string"})),
            "property `kind`",
        ),
        (
            with(&|d| d.settings["examples"] = json!([])),
            "no `examples`",
        ),
        (
            with(&|d| d.settings["examples"] = json!([3])),
            "no `examples`",
        ),
    ];
    for (declaration, why) in cases {
        let msg = refused(&mut kinds, declaration);
        assert!(msg.contains(why), "{why}: {msg}");
    }
    // The role's flows: a source gives batches, a sink takes them.
    let mut source = with(&|_| {});
    source.role = Role::Source;
    assert!(
        source.check(Role::Source).is_err(),
        "a source takes nothing"
    );
    source.inputs = Flow::Nothing;
    assert!(source.check(Role::Source).is_ok());
    let mut sink = with(&|_| {});
    sink.role = Role::Sink;
    assert!(
        sink.check(Role::Sink).is_err(),
        "a sink gives files or nothing"
    );
    sink.outputs = Flow::Files;
    assert!(sink.check(Role::Sink).is_ok());
    // Every port refuses a second kind of one name.
    let mut kinds = Registry::standard();
    assert!(kinds.register_source(Arc::new(parquet::Parquet)).is_err());
    assert!(kinds.register_sink(Arc::new(parquet::Parquet)).is_err());
    assert!(kinds.register_kernel(Arc::new(standard::Std)).is_err());
    // Names are lowercase ASCII words with `_`, `-` and `.` in them.
    for name in ["parquet", "arrow_ipc", "test.double", "a-b", "x1"] {
        assert!(is_kind_name(name), "{name}");
    }
    for name in ["", "_x", "A", "x y", "é"] {
        assert!(!is_kind_name(name), "{name}");
    }
}

/// KD-T4 standard_conformance (KD-I5): every standard kind whose examples build without a
/// fixture passes its port's suite; the package and governed kinds pass theirs with their
/// fixtures (`tests/package.rs`, `tests/governed.rs`).
#[test]
fn kd_t4_standard_conformance() {
    let harness = Harness::new();
    let sources: [&dyn SourceKind; 3] = [&parquet::Parquet, &vortex::Vortex, &tensor::Tensor];
    for kind in sources {
        let passed = conformance::source(kind, &harness).unwrap_or_else(|v| panic!("{v}"));
        assert!(passed.cases > 4, "{passed:?}");
    }
    let sinks: [&dyn SinkKind; 4] = [
        &parquet::Parquet,
        &vortex::Vortex,
        &tensor::Tensor,
        &arrow_ipc::ArrowIpc,
    ];
    for kind in sinks {
        let passed = conformance::sink(kind, &harness).unwrap_or_else(|v| panic!("{v}"));
        assert!(passed.cases > 4, "{passed:?}");
    }
    let passed = conformance::kernel(&standard::Std, &harness).unwrap_or_else(|v| panic!("{v}"));
    assert_eq!(passed.properties.last(), Some(&"KD-C7"));
    // A probe passes the suite too: the suite is the port's, not the standard kinds'.
    let passed =
        conformance::kernel(&Probe::named("probe"), &harness).unwrap_or_else(|v| panic!("{v}"));
    assert!(passed.properties.contains(&"KD-C7"));
}

/// IM-T16 the_image_kind_is_declared (SDD 19 d.2, e.5): the declaration is one the registry
/// accepts and says what an image is; its settings are read, and refused, before anything is
/// unpacked, on every platform; where there is no sandbox the stage is refused by name, and where
/// there is, without a cgroup it says what it needs. The conformance suite runs over a real layout
/// in `crates/moruna-image/tests/image_kind.rs`, on Linux.
#[test]
fn im_t16_the_image_kind_is_declared() {
    let kind = image::Image;
    let declared = kind.declaration();
    assert_eq!(declared.check(Role::Kernel), Ok(()));
    assert_eq!(declared, kind.declaration());
    assert_eq!(declared.name, "image");
    assert_eq!(declared.isolation, Isolation::Confined);
    assert_eq!(declared.needs, Needs::nothing());
    assert_eq!(declared.deterministic, Fact::No);
    assert_eq!(declared.preserves_columns, Fact::No);
    assert_eq!(
        (declared.inputs, declared.outputs),
        (Flow::Batches, Flow::Batches)
    );
    assert_eq!(declared.examples().len(), 1);
    assert!(Registry::standard().kernel("kernels[0]", "image").is_ok());

    let build = |value: Value| {
        let doc = doc(value);
        let mut cx = Context::new(Arc::new(Standalone));
        refusal(kind.stage(&Entry::new("kernels[0]", Some(0), &doc), &mut cx)).to_string()
    };
    let e = build(json!({"kind": "image", "layout": "/l"}));
    assert!(e.contains("kernels[0]") && e.contains("reference"), "{e}");
    let e = build(json!({"kind": "image", "layout": "/l", "reference": "r", "volume": "/v"}));
    assert!(e.contains("kernels[0]") && e.contains("volume"), "{e}");
    let e = build(json!({"kind": "image", "layout": "/l", "reference": "r", "command": "ls"}));
    assert!(e.contains("kernels[0]"), "{e}");
    let e = build(json!({"kind": "image", "layout": "/l", "reference": "r", "env": {"A": 1}}));
    assert!(e.contains("kernels[0]"), "{e}");
    let e = build(json!({"kind": "image", "layout": "/l", "reference": "r"}));
    #[cfg(not(target_os = "linux"))]
    assert!(e.contains("needs Linux"), "{e}");
    #[cfg(target_os = "linux")]
    assert!(e.contains("--sandbox-cgroup"), "{e}");
}

/// KD-T6 listing (e.3): every declaration, by role, in name order; it reads back as itself, and
/// every standard declaration is one the registry accepts.
#[test]
fn kd_t6_listing() {
    let listing = Registry::standard().listing();
    assert_eq!(listing.moruna_kinds, LISTING_VERSION);
    assert_eq!(listing.moruna, env!("CARGO_PKG_VERSION"));
    let names =
        |list: &[Declaration]| -> Vec<String> { list.iter().map(|d| d.name.clone()).collect() };
    assert_eq!(names(&listing.kernels), ["image", "package", "std"]);
    let mut sinks = vec!["arrow_ipc", "parquet", "tensor", "vortex"];
    let mut sources = vec!["parquet", "tensor", "vortex"];
    if cfg!(feature = "peql") {
        sinks.insert(2, "peql");
        sources.insert(0, "datafusion");
    }
    assert_eq!(names(&listing.sinks), sinks);
    assert_eq!(names(&listing.sources), sources);
    for (role, list) in [
        (Role::Source, &listing.sources),
        (Role::Kernel, &listing.kernels),
        (Role::Sink, &listing.sinks),
    ] {
        for declared in list {
            declared.check(role).unwrap_or_else(|e| panic!("{e}"));
            assert!(!declared.examples().is_empty());
        }
    }
    let text = listing.to_json();
    let back: Listing = serde_json::from_str(&text).expect("reads back");
    assert_eq!(back, listing);
    let json: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(json["kernels"][1]["settings"]["$schema"], SCHEMA_DIALECT);
    assert_eq!(
        json["kernels"][1]["settings"]["properties"]["name"]["enum"],
        json!(moruna_kernels::NAMES)
    );
    assert_eq!(
        json["sources"][0]["needs"]["network"],
        json!(if cfg!(feature = "peql") {
            "no"
        } else {
            "by_settings"
        })
    );
}

/// KD-T10 suite_catches_violations (f.4): the suites have teeth. An adapter that breaks a
/// property fails on that property, by id.
#[test]
fn kd_t10_suite_catches_violations() {
    /// Reads its settings as it likes: what each case breaks is chosen by `fault`.
    struct Faulty {
        fault: &'static str,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[derive(Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Strict {
        tag: String,
        #[serde(default)]
        fingerprint: Option<String>,
    }
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Loose {
        #[serde(default)]
        tag: Value,
    }
    impl RunnableKind for Faulty {
        fn declaration(&self) -> Declaration {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut d = Probe::named("faulty").declaration();
            d.settings = settings_schema::<Strict>("faulty", vec![json!({"tag": "a"})]);
            match self.fault {
                "KD-C1" => d.settings["examples"] = json!([]),
                "KD-C2" if n > 0 => d.summary = format!("asked {n} times"),
                _ => {}
            }
            d
        }
        fn stage(&self, entry: &Entry<'_>, _: &mut Context) -> moruna_kernel::Result<Stage> {
            match self.fault {
                "KD-C3" => return Err(entry.refuse("tag", "never")),
                "KD-C4" | "KD-C5" | "KD-C6" => {
                    let _: Loose = entry.settings()?;
                }
                "KD-C4-elsewhere" => {
                    if entry
                        .doc()
                        .settings
                        .contains_key("moruna_conformance_unknown")
                    {
                        return Err(SpecError::new("budget", "not the entry").into());
                    }
                }
                _ => {
                    let _: Strict = entry.settings()?;
                }
            }
            Ok(Stage::of(Arc::new(FakeKernel::new()) as Arc<dyn Kernel>))
        }
    }
    let harness = Harness::new();
    for (fault, broken) in [
        ("KD-C1", "KD-C1"),
        ("KD-C2", "KD-C2"),
        ("KD-C3", "KD-C3"),
        ("KD-C4", "KD-C4"),
        ("KD-C4-elsewhere", "KD-C4"),
        ("KD-C7", "KD-C7"),
    ] {
        let kind = Faulty {
            fault,
            calls: Default::default(),
        };
        let violation = conformance::kernel(&kind, &harness).expect_err(fault);
        assert_eq!(violation.property, broken, "{fault}: {violation}");
        assert!(
            violation.to_string().starts_with("`faulty` breaks "),
            "{violation}"
        );
    }
    // A kind that reads loosely is caught by the undeclared setting first; with that allowed,
    // by the type of a declared one, and then by a missing required one.
    let loose_types = Harness::new();
    struct TypesOnly;
    impl RunnableKind for TypesOnly {
        fn declaration(&self) -> Declaration {
            Probe::named("types").declaration()
        }
        fn stage(&self, entry: &Entry<'_>, _: &mut Context) -> moruna_kernel::Result<Stage> {
            #[derive(Deserialize)]
            #[allow(dead_code)]
            struct Typed {
                #[serde(default)]
                tag: Option<String>,
                #[serde(default)]
                fingerprint: Option<String>,
            }
            let mut settings = entry.doc().settings.clone();
            if settings.remove("moruna_conformance_unknown").is_some() {
                return Err(entry.refuse("moruna_conformance_unknown", "undeclared"));
            }
            let typed: Typed = serde_json::from_value(Value::Object(settings))
                .map_err(|e| SpecError::new(entry.field().to_string(), e.to_string()))?;
            let kernel = FakeKernel::new();
            check_pin(entry, typed.fingerprint.as_deref(), &kernel)?;
            Ok(Stage::of(Arc::new(kernel) as Arc<dyn Kernel>))
        }
    }
    let violation = conformance::kernel(&TypesOnly, &loose_types).expect_err("required");
    assert_eq!(violation.property, "KD-C6", "{violation}");
    // Examples a harness gives replace the declared ones; one that is not an object is skipped.
    let harness = Harness::default().examples(vec![json!({"tag": "b"}), json!(3)]);
    let passed = conformance::kernel(&Probe::named("probe"), &harness).expect("passes");
    assert!(passed.cases >= 4, "{passed:?}");
    assert_eq!(refused_field(&MorunaError::Cancelled), None);
}

/// KD-T11 fusion_through_the_port (MH 4.9): adjacent stages join when the earlier stage's fuse
/// says so, and only then; the build's stage list says which entries each stage carries.
#[test]
fn kd_t11_fusion_through_the_port() {
    let mut kinds = Registry::standard();
    kinds
        .register_kernel(Arc::new(Probe {
            fuses: true,
            ..Probe::named("probe")
        }))
        .expect("registers");
    let mut job = crate::job::JobSpec::new(
        doc(json!({"kind": "parquet", "url": "/in"})),
        vec![
            doc(json!({"kind": "probe", "tag": "a"})),
            doc(json!({"kind": "probe", "tag": "b"})),
            doc(json!({"kind": "std", "name": "cast", "args": {"columns": {"a": "double"}}})),
            doc(json!({"kind": "std", "name": "select", "args": {"columns": ["a"]}})),
            doc(json!({"kind": "probe", "tag": "c"})),
        ],
        doc(json!({"kind": "parquet", "url": "/out"})),
    );
    let opts = || crate::job::BuildOptions {
        strict: false,
        env: &|_| None,
        notes: Vec::new(),
        packages: crate::packaged::PackageHost::default(),
        host_ports: None,
    };
    let built = crate::job::build(&job, &kinds, opts()).expect("builds");
    assert_eq!(built.stage_kernels, vec![vec![0, 1], vec![2, 3], vec![4]]);
    assert_eq!(built.spec.kernels.len(), 3);
    job.kernels.swap(1, 2);
    let built = crate::job::build(&job, &kinds, opts()).expect("builds");
    assert_eq!(
        built.stage_kernels.len(),
        5,
        "no two neighbours join: {:?}",
        built.stage_kernels
    );
}

/// The context's shared values are one per build and per type.
#[test]
fn a_context_shares_one_value_per_type() {
    let mut cx = Context::new(Arc::new(Standalone));
    assert_eq!(cx.shared(|| 7u32), 7);
    assert_eq!(cx.shared(|| 9u32), 7, "made once");
    assert_eq!(cx.shared(|| "x".to_string()), "x");
    cx.notes().push("a".into());
    assert_eq!(cx.take_notes(), ["a"]);
    assert!(cx.take_notes().is_empty());
    assert!(cx.carried_contracts().is_empty());
    assert!(cx.host_port_hub().is_none());
    assert_eq!(cx.package_host(), &crate::packaged::PackageHost::default());
    assert!(cx.embedding().guest_port("anything").is_none());
    let contract = crate::job::ContractEntry {
        document: json!({}),
        compiled: String::new(),
        functions: Vec::new(),
        audiences: Vec::new(),
    };
    assert!(cx.embedding().admit(0, &contract).is_ok());
    // A url is one string or several, and is written back as it came.
    let one: Urls = serde_json::from_value(json!("/a")).expect("one");
    let many: Urls = serde_json::from_value(json!(["/a", "/b"])).expect("many");
    assert_eq!(serde_json::to_value(&one).expect("one"), json!("/a"));
    assert_eq!(
        serde_json::to_value(&many).expect("many"),
        json!(["/a", "/b"])
    );
    assert_eq!(Role::Sink.as_str(), "sink");
}
