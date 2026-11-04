//! SDD 22 k: kinds an embedder declares and serves through a port, checked without running
//! anything (JB-T41 to JB-T45).

use moruna_job::rules::{Known, Misfit};
use moruna_job::{
    Catalogue, CatalogueError, Declaration, Edge, Fact, Flow, Isolation, Job, KindDoc, Listing,
    Needs, PortDoc, Refusal, RefusalCode, Role, SchemaRules, Served, Task, check,
};
use moruna_types::{ColumnType, TypeName};
use serde_json::{Value, json};

/// A kind its embedder serves: `name`, served by the host port of the same name, whose settings
/// are one required string `prompt`, with the schema rules given.
fn served(name: &str, input: Option<Value>, output: Option<Value>) -> Declaration {
    Declaration {
        name: name.into(),
        role: Role::Kernel,
        summary: "a kind its embedder serves".into(),
        settings: json!({
            "$schema": moruna_job::SCHEMA_DIALECT,
            "type": "object",
            "additionalProperties": false,
            "properties": {"prompt": {"type": "string"}, "limit": {"type": "integer", "minimum": 1}},
            "required": ["prompt"],
            "examples": [{"prompt": "classify"}]
        }),
        inputs: Flow::Batches,
        outputs: Flow::Batches,
        needs: Needs::a_port(),
        deterministic: Fact::No,
        preserves_columns: Fact::BySettings,
        isolation: Isolation::Embedder,
        served: Some(Served {
            port: PortDoc::host(name),
            input_schema: input,
            output_schema: output,
        }),
    }
}

fn catalogue(declarations: Vec<Declaration>) -> Catalogue {
    let mut catalogue = Catalogue::standard().expect("the standard catalogue");
    for declaration in declarations {
        catalogue.add(declaration).expect("a served kind");
    }
    catalogue
}

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

/// `parquet` into each kernel in turn, in a chain, into `parquet`.
fn chain(kernels: Vec<Value>) -> Job {
    let mut tasks = vec![Task::source(
        "in",
        entry(json!({"kind": "parquet", "url": "/in"})),
    )];
    let mut edges = Vec::new();
    let mut last = "in".to_string();
    for (i, kernel) in kernels.into_iter().enumerate() {
        let id = format!("k{i}");
        tasks.push(Task::kernel(&id, entry(kernel)));
        edges.push(Edge::between(&last, &id));
        last = id;
    }
    tasks.push(Task::sink(
        "out",
        entry(json!({"kind": "parquet", "url": "/out"})),
    ));
    edges.push(Edge::between(&last, "out"));
    Job::new(tasks, edges)
}

/// A change made to a good declaration, to see it refused.
type Mutation = Box<dyn Fn(&mut Declaration)>;

fn said(refusals: &[Refusal]) -> Vec<String> {
    refusals.iter().map(Refusal::to_string).collect()
}

/// JB-T41 served_declarations: a served kind is a kernel, isolated as `embedder`, that calls the
/// port its `served` names, whose rules are rules; each other shape is refused naming why
/// (SDD 18 KD-I11).
#[test]
fn jb_t40_served_declarations() {
    let good = served(
        "judge",
        Some(json!({"text": "string"})),
        Some(json!({"adds": {"verdict": "bool"}})),
    );
    good.check(Role::Kernel).expect("a served kind");
    // It reads back as itself, `served` included.
    let text = serde_json::to_string(&good).expect("json");
    assert_eq!(
        serde_json::from_str::<Declaration>(&text).expect("reads"),
        good
    );
    assert!(text.contains(r#""isolation":"embedder""#), "{text}");
    // A kind Moruna runs writes no `served`.
    let builtin = Catalogue::standard().unwrap();
    let std = builtin.get(Role::Kernel, "std").unwrap();
    assert!(!serde_json::to_string(std).unwrap().contains("served"));

    let refused = |mutate: &dyn Fn(&mut Declaration), role: Role| {
        let mut declaration = good.clone();
        mutate(&mut declaration);
        declaration.role = role;
        declaration.check(role).expect_err("refused")
    };
    let cases: Vec<(Mutation, Role, &str)> = vec![
        (
            Box::new(|d| d.served = None),
            Role::Kernel,
            "`judge` is isolated as `embedder` and names no `served`: a kind its embedder serves \
             names the port that serves it",
        ),
        (
            Box::new(|d| d.isolation = Isolation::Builtin),
            Role::Kernel,
            "`judge` names `served` and is isolated as `builtin`: a kind its embedder serves is \
             isolated as `embedder`",
        ),
        (
            Box::new(|d| {
                d.inputs = Flow::Files;
            }),
            Role::Source,
            "`judge` is a source served by a port: a served kind is a kernel, and a source or a \
             sink of the embedder's own is registered with its adapter",
        ),
        (
            Box::new(|d| {
                d.outputs = Flow::Files;
            }),
            Role::Sink,
            "`judge` is a sink served by a port: a served kind is a kernel, and a source or a \
             sink of the embedder's own is registered with its adapter",
        ),
        (
            Box::new(|d| d.needs = Needs::nothing()),
            Role::Kernel,
            "`judge` is served by a port and says it calls none: a served kind's \
             `needs.model_port` is `yes`",
        ),
        (
            Box::new(|d| d.served.as_mut().unwrap().port = PortDoc::host("two words")),
            Role::Kernel,
            "`judge`'s served.port.host: \"two words\" is not a port name",
        ),
        (
            Box::new(|d| {
                d.served.as_mut().unwrap().port.guest = Some("also".into());
            }),
            Role::Kernel,
            "`judge`'s served.port: a port goes to exactly one of `host` and `guest`",
        ),
        (
            Box::new(|d| {
                d.served.as_mut().unwrap().port = PortDoc::host("judge").with_deadline_ms(0);
            }),
            Role::Kernel,
            "`judge`'s served.port.deadline_ms: 0 is outside 1 to 86400000 ms",
        ),
        (
            Box::new(|d| {
                d.served.as_mut().unwrap().input_schema = Some(json!({"adds": {"x": "int8"}}));
            }),
            Role::Kernel,
            "`judge`'s served.input_schema: an input schema cannot be relative: adds, drops and \
             changes describe an output",
        ),
        (
            Box::new(|d| {
                d.served.as_mut().unwrap().output_schema = Some(json!({"x": "integer"}));
            }),
            Role::Kernel,
            "`judge`'s served.output_schema.x: unknown type `integer` in a schema declaration",
        ),
        (
            Box::new(|d| {
                d.served.as_mut().unwrap().output_schema = Some(json!([["x"]]));
            }),
            Role::Kernel,
            "`judge`'s served.output_schema[0]: an exact column is [name, type] or [name, type, \
             nullable]",
        ),
    ];
    for (mutate, role, want) in cases {
        assert_eq!(refused(mutate.as_ref(), role), want);
    }
    // A `served` the format does not have is refused when it is read.
    let mut value = serde_json::to_value(&good).unwrap();
    value["served"]["model"] = json!("x");
    assert!(serde_json::from_value::<Declaration>(value).is_err());
}

/// JB-T42 extended_catalogue: an embedder's served kinds join the standard catalogue; one that
/// collides with a built-in kind is refused naming it and leaves the catalogue as it was; a
/// listing with served kinds reads back as the catalogue of them (JB-I4).
#[test]
fn jb_t41_extended_catalogue() {
    let mut extended = Catalogue::standard().expect("standard");
    let before = extended.clone();
    extended
        .add(served("judge", None, None))
        .expect("a served kind");
    let kernels: Vec<&str> = extended.names(Role::Kernel).collect();
    assert_eq!(kernels, ["image", "judge", "package", "std"]);
    for builtin in ["std", "package", "image"] {
        let mut taken = extended.clone();
        assert_eq!(
            taken.add(served(builtin, None, None)),
            Err(CatalogueError::Taken {
                role: Role::Kernel,
                name: builtin.into()
            })
        );
        assert_eq!(taken, extended, "a refused declaration changes nothing");
    }
    assert_eq!(
        extended.clone().add(served("judge", None, None)),
        Err(CatalogueError::Taken {
            role: Role::Kernel,
            name: "judge".into()
        })
    );
    // A served kind whose declaration fails its check is refused with the reason.
    let mut odd = served("odd", None, None);
    odd.needs = Needs::nothing();
    assert!(matches!(
        extended.clone().add(odd),
        Err(CatalogueError::Refused { role: Role::Kernel, why }) if why.contains("model_port")
    ));
    assert_ne!(before, extended);

    let listing = Listing {
        moruna_kinds: moruna_job::LISTING_VERSION,
        moruna: "0.0.0".into(),
        sources: Vec::new(),
        kernels: vec![served("judge", Some(json!([["text", "string"]])), None)],
        sinks: Vec::new(),
    };
    let back: Listing = serde_json::from_str(&listing.to_json()).expect("reads");
    let read = Catalogue::from_listing(back).expect("a catalogue");
    assert!(read.get(Role::Kernel, "judge").unwrap().served.is_some());
}

/// JB-T43 check_against_an_extended_catalogue: a job that names an embedder's kinds passes
/// against the catalogue they extend and is refused against the standard one; its settings are
/// checked by the served kind's schema like any other kind's.
#[test]
fn jb_t42_check_against_an_extended_catalogue() {
    let extended = catalogue(vec![served("judge", None, None)]);
    let job = chain(vec![
        json!({"kind": "std", "name": "cast", "args": {"columns": {"a": "double"}}}),
        json!({"kind": "judge", "prompt": "is it spam?", "limit": 3}),
    ]);
    assert_eq!(check(&job, &extended), Ok(()));

    let refused = check(&job, &Catalogue::standard().unwrap()).expect_err("undeclared");
    assert_eq!(
        said(&refused),
        [
            "tasks[2].kind: no function kind `judge` is declared; this catalogue runs image, \
          package, std"
        ]
    );
    assert!(matches!(
        &refused[0].code,
        RefusalCode::UnknownKind { role: Role::Kernel, known } if known == &["image", "package", "std"]
    ));

    let bad = chain(vec![json!({"kind": "judge", "limit": 0, "tone": "polite"})]);
    let refused = check(&bad, &extended).expect_err("bad settings");
    assert_eq!(
        said(&refused),
        [
            "tasks[1].settings.prompt: `judge` settings: `prompt` is required",
            "tasks[1].settings.limit: `judge` settings: `limit` must be at least 1",
            "tasks[1].settings.tone: `judge` settings: `tone` is not a setting of `judge`",
        ]
    );
    let paths: Vec<&RefusalCode> = refused.iter().map(|r| &r.code).collect();
    assert_eq!(
        paths,
        [
            &RefusalCode::Settings {
                path: "/prompt".into()
            },
            &RefusalCode::Settings {
                path: "/limit".into()
            },
            &RefusalCode::Settings {
                path: "/tone".into()
            },
        ]
    );
}

/// JB-T44 columns_along_the_edges: where the job knows the columns an edge carries (a served
/// kind's output rules, from its input where they are relative), a served kind that takes other
/// columns is refused at the edge naming the column; what the job cannot know is never refused
/// (SDD 22 e.6).
#[test]
fn jb_t43_columns_along_the_edges() {
    let extract = served(
        "extract",
        None,
        Some(json!([["id", "int64"], ["text", "string", false]])),
    );
    let label = served(
        "label",
        Some(json!({"text": "utf8"})),
        Some(json!({"adds": {"label": "string"}, "drops": ["text"]})),
    );
    let exact = served(
        "exact",
        Some(json!([["id", "int64"], ["label", "string"]])),
        None,
    );
    let wants_score = served("score", Some(json!({"score": "double"})), None);
    let wants_text_int = served("count", Some(json!({"text": "int32"})), None);
    let renames = served(
        "renames",
        None,
        Some(json!({"changes": {"missing": "string"}})),
    );
    let extended = catalogue(vec![
        extract,
        label,
        exact,
        wants_score,
        wants_text_int,
        renames,
    ]);
    let k = |kind: &str| json!({"kind": kind, "prompt": "p"});

    // extract gives exactly [id, text]; label takes a `text` string (`utf8` is `string`) and
    // gives [id, label]; exact takes exactly that.
    assert_eq!(
        check(
            &chain(vec![k("extract"), k("label"), k("exact")]),
            &extended
        ),
        Ok(())
    );

    // A type that is not the type taken.
    let refused = check(&chain(vec![k("extract"), k("count")]), &extended).expect_err("type");
    assert_eq!(
        said(&refused),
        [
            "edges[1]: `k1` (`count`) does not take what `k0` gives it: column `text`: declared \
          int32, given string"
        ]
    );
    assert_eq!(
        refused[0].code,
        RefusalCode::Schema {
            column: "text".into()
        }
    );

    // A column the producer is known not to give.
    let refused = check(&chain(vec![k("extract"), k("score")]), &extended).expect_err("missing");
    assert_eq!(
        said(&refused),
        [
            "edges[1]: `k1` (`score`) does not take what `k0` gives it: column `score`: declared \
          double, not given"
        ]
    );

    // Exact rules take no other column, and in their order.
    let refused = check(&chain(vec![k("extract"), k("exact")]), &extended).expect_err("exact");
    assert_eq!(
        said(&refused),
        [
            "edges[1]: `k1` (`exact`) does not take what `k0` gives it: column `label`: declared \
             string, not given",
            "edges[1]: `k1` (`exact`) does not take what `k0` gives it: column `text`: given \
             string, not declared",
        ]
    );
    let swapped = served(
        "swapped",
        None,
        Some(json!([["label", "string"], ["id", "int64"]])),
    );
    let with_swapped = catalogue(vec![
        swapped,
        served(
            "exact",
            Some(json!([["id", "int64"], ["label", "string"]])),
            None,
        ),
    ]);
    let refused =
        check(&chain(vec![k("swapped"), k("exact")]), &with_swapped).expect_err("position");
    assert_eq!(
        said(&refused),
        [
            "edges[1]: `k1` (`exact`) does not take what `k0` gives it: column `id`: declared at \
             position 0, given at 1",
            "edges[1]: `k1` (`exact`) does not take what `k0` gives it: column `label`: declared \
             at position 1, given at 0",
        ]
    );

    // Relative rules that change a column a closed input does not have.
    let refused = check(&chain(vec![k("extract"), k("renames")]), &extended).expect_err("relative");
    assert_eq!(
        said(&refused),
        [
            "tasks[2]: `renames`'s output_schema drops or changes a column its input does not have: \
          column `missing`: named by the output, not in the input"
        ]
    );

    // Nothing is known of what a source or a kind Moruna runs gives, so nothing is refused.
    assert_eq!(check(&chain(vec![k("score")]), &extended), Ok(()));
    assert_eq!(
        check(
            &chain(vec![
                json!({"kind": "std", "name": "select", "args": {"columns": ["a"]}}),
                k("count")
            ]),
            &extended
        ),
        Ok(())
    );
    // Subset output rules are open: a column they do not name may be there.
    let open = served("open", None, Some(json!({"id": "int64"})));
    let with_open = catalogue(vec![
        open,
        served("score", Some(json!({"score": "double"})), None),
    ]);
    assert_eq!(
        check(&chain(vec![k("open"), k("score")]), &with_open),
        Ok(())
    );
    let open_typed = served("open", None, Some(json!({"score": "string"})));
    let with_open_typed = catalogue(vec![
        open_typed,
        served("score", Some(json!({"score": "double"})), None),
    ]);
    assert!(check(&chain(vec![k("open"), k("score")]), &with_open_typed).is_err());
    // Relative rules over an input that is not known are not known either.
    assert_eq!(
        check(&chain(vec![k("label"), k("score")]), &extended),
        Ok(())
    );
    // Relative rules over input rules: label takes at least `text`, drops it and adds `label`.
    let label_then_exact = check(&chain(vec![k("label"), k("exact")]), &extended);
    assert_eq!(label_then_exact, Ok(()));

    // Fan-in: each input port is judged by what feeds it.
    let tasks = vec![
        Task::source("a", entry(json!({"kind": "parquet", "url": "/a"}))),
        Task::kernel("x", entry(k("extract"))),
        Task::kernel_with_inputs("join", entry(k("count")), &["left", "right"]),
        Task::sink("out", entry(json!({"kind": "parquet", "url": "/out"}))),
    ];
    let edges = vec![
        Edge::between("a", "x"),
        Edge::new("a", "out", "join", "left"),
        Edge::new("x", "out", "join", "right"),
        Edge::between("join", "out"),
    ];
    let refused = check(&Job::new(tasks, edges), &extended).expect_err("fan-in");
    assert_eq!(
        said(&refused),
        [
            "edges[2]: `join` (`count`) does not take what `x` gives it: column `text`: declared \
          int32, given string"
        ]
    );
}

/// JB-T45 schema_rules: the spelling of a package's declaration read without Arrow, its types read
/// by the grammar, and what is known of a side from it.
#[test]
fn jb_t44_schema_rules() {
    let rules = |v: Value, output: bool| SchemaRules::parse(&v, "s", output);
    let exact = rules(json!([["a", "int64"], ["b", " any ", false]]), false).unwrap();
    let SchemaRules::Exact(columns) = &exact else {
        panic!("{exact:?}")
    };
    assert_eq!(columns[0].ty, ColumnType::Exact(TypeName::Int64));
    assert_eq!(columns[1].ty, ColumnType::Any);
    assert!(!columns[1].nullable);
    assert_eq!(
        rules(json!({}), true).unwrap(),
        SchemaRules::Subset(Vec::new())
    );
    let relative = rules(json!({"adds": {"c": "double"}, "drops": null}), true).unwrap();
    assert!(matches!(relative, SchemaRules::Relative { ref drops, .. } if drops.is_empty()));
    for (bad, field, reason) in [
        (
            json!(1),
            "s",
            "a schema is an object of column to type, or a list of [name, type] columns",
        ),
        (
            json!([1]),
            "s[0]",
            "an exact column is [name, type] or [name, type, nullable]",
        ),
        (
            json!([[1, "int64"]]),
            "s[0]",
            "the column name must be a string",
        ),
        (
            json!([["a", "int64", "yes"]]),
            "s[0]",
            "an exact column is [name, type] or [name, type, nullable]",
        ),
        (
            json!({"a": 1}),
            "s.a",
            "a type is a string of the type grammar (MH 4.9)",
        ),
        (
            json!({"adds": []}),
            "s.adds",
            "must be an object of column to type",
        ),
        (
            json!({"drops": "a"}),
            "s.drops",
            "must be a list of column names",
        ),
        (json!({"drops": [1]}), "s.drops", "names must be strings"),
        (
            json!({"a": "list<any>"}),
            "s.a",
            "unknown type `list<any>` in a schema declaration",
        ),
    ] {
        let fault = rules(bad.clone(), true).expect_err("refused");
        assert_eq!(
            (fault.field.as_str(), fault.reason.as_str()),
            (field, reason),
            "{bad}"
        );
    }

    // The types are the grammar's (`moruna_types`, tested there): an alias reads as its name.
    assert_eq!(
        rules(json!({"a": "utf8", "b": "list<float64>"}), false).unwrap(),
        rules(json!({"a": "string", "b": "list<item: double>"}), false).unwrap()
    );

    let open = Known {
        columns: Vec::new(),
        closed: false,
        ordered: false,
    };
    let relative = rules(json!({"drops": ["gone"]}), true).unwrap();
    assert_eq!(
        relative.known(Some(&open)).unwrap(),
        Some(Known {
            columns: Vec::new(),
            closed: false,
            ordered: false
        }),
        "an open input may have the column it drops"
    );
    assert_eq!(relative.known(None).unwrap(), None);
    let closed = Known {
        columns: Vec::new(),
        closed: true,
        ordered: true,
    };
    assert_eq!(
        relative.known(Some(&closed)).unwrap_err(),
        [Misfit::Unknown {
            column: "gone".into()
        }]
    );
    assert_eq!(
        relative.misfits(&closed),
        [],
        "an input's rules are never relative"
    );
    let any = rules(json!({"a": "any"}), false).unwrap();
    let given = rules(json!([["a", "string"]]), true)
        .unwrap()
        .known(None)
        .unwrap()
        .unwrap();
    assert!(any.misfits(&given).is_empty(), "any takes any type");
}
