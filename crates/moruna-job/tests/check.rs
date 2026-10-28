//! SDD 22 k: checking a document without running it (JB-T6 to JB-T10).

use moruna_job::{
    Catalogue, ContractEntry, Job, JobSpec, KindDoc, Refusal, RefusalCode, Role, check,
};
use serde_json::{Value, json};

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn job(source: Value, kernels: Vec<Value>, sink: Value) -> JobSpec {
    JobSpec::new(
        entry(source),
        kernels.into_iter().map(entry).collect(),
        entry(sink),
    )
}

fn good() -> JobSpec {
    job(
        json!({"kind": "parquet", "url": "/in"}),
        vec![json!({"kind": "std", "name": "cast", "args": {"columns": {"a": "double"}}})],
        json!({"kind": "parquet", "url": "/out"}),
    )
}

/// A version 1 document is checked as the DAG with one path it reads as (SDD 22 e.4), each task
/// refused at the field the document wrote it under: `source`, `kernels[i]`, `sink`.
fn v1(spec: &JobSpec, catalogue: &Catalogue) -> Result<(), Vec<Refusal>> {
    check(&Job::from_v1(spec), catalogue)
}

fn refusals(job: &JobSpec) -> Vec<Refusal> {
    v1(job, &Catalogue::standard().expect("the standard catalogue")).expect_err("refused")
}

fn text(refusals: &[Refusal]) -> Vec<String> {
    refusals.iter().map(Refusal::to_string).collect()
}

fn contract() -> ContractEntry {
    ContractEntry {
        document: json!({}),
        compiled: String::new(),
        functions: Vec::new(),
        audiences: Vec::new(),
    }
}

/// JB-T6 examples_pass: every example of every declared kind is settings its kind accepts, so a
/// document made of them passes, whatever the roles around it.
#[test]
fn jb_t6_every_example_passes() {
    let governed = Catalogue::standard_with_peql().expect("the governed catalogue");
    let mut cases = 0;
    for role in [Role::Source, Role::Kernel, Role::Sink] {
        for declared in governed.declarations(role) {
            for example in declared.examples() {
                let mut mine = example.clone();
                mine["kind"] = json!(declared.name);
                let mut doc = good();
                match role {
                    Role::Source => doc.source = entry(mine),
                    Role::Kernel => doc.kernels = vec![entry(mine)],
                    Role::Sink => doc.sink = entry(mine),
                }
                doc.contracts = vec![contract()];
                assert_eq!(v1(&doc, &governed), Ok(()), "{} {example}", declared.name);
                cases += 1;
            }
        }
    }
    assert!(cases >= 20, "{cases}");
    assert_eq!(
        v1(
            &good(),
            &Catalogue::standard().expect("the standard catalogue")
        ),
        Ok(())
    );
    let copy = job(
        json!({"kind": "tensor", "url": ["/a", "/b"]}),
        Vec::new(),
        json!({"kind": "arrow_ipc", "url": "/out"}),
    );
    assert_eq!(
        v1(
            &copy,
            &Catalogue::standard().expect("the standard catalogue")
        ),
        Ok(())
    );
}

/// JB-T7 unknown_kinds: a kind no declaration of its role has is refused at `<field>.kind` with
/// the kinds the catalogue has, in the runtime's words; the same name in another role is not it.
#[test]
fn jb_t7_unknown_kinds() {
    let doc = job(
        json!({"kind": "csv", "url": "/in"}),
        vec![
            json!({"kind": "java"}),
            json!({"kind": "std", "name": "cast"}),
        ],
        json!({"kind": "std"}),
    );
    let found = refusals(&doc);
    assert_eq!(
        text(&found),
        [
            "source.kind: no source kind `csv` is declared; this catalogue reads arrow_ipc, \
             parquet, tensor, vortex",
            "kernels[0].kind: no kernel kind `java` is declared; this catalogue runs package, std",
            "sink.kind: no sink kind `std` is declared; this catalogue writes arrow_ipc, parquet, \
             tensor, vortex",
        ]
    );
    assert_eq!(
        found[0].code,
        RefusalCode::UnknownKind {
            role: Role::Source,
            known: vec![
                "arrow_ipc".into(),
                "parquet".into(),
                "tensor".into(),
                "vortex".into()
            ]
        }
    );
    assert_eq!(found[1].field, "kernels[0].kind");
    // Nothing is declared in an empty catalogue.
    let none = v1(&good(), &Catalogue::empty()).expect_err("nothing is declared");
    assert_eq!(none.len(), 3);
    assert_eq!(
        none[0].message,
        "no source kind `parquet` is declared; this catalogue has no source kinds"
    );
}

/// JB-T8 settings_refusals: settings the kind's schema does not allow are refused at
/// `<entry>.settings.<path>`, with a JSON pointer in the code and a sentence naming the kind and
/// the value; each refusal of one entry is its own.
#[test]
fn jb_t8_settings_refusals() {
    let said = |source: Value| {
        let doc = job(
            source,
            Vec::new(),
            json!({"kind": "parquet", "url": "/out"}),
        );
        refusals(&doc)
    };
    let one = |source: Value| {
        let mut found = said(source);
        assert_eq!(found.len(), 1, "{found:?}");
        found.remove(0)
    };

    let missing = one(json!({"kind": "parquet"}));
    assert_eq!(missing.field, "source.settings.url");
    assert_eq!(
        missing.code,
        RefusalCode::Settings {
            path: "/url".into()
        }
    );
    assert_eq!(missing.message, "`parquet` settings: `url` is required");
    assert_eq!(
        missing.to_string(),
        "source.settings.url: `parquet` settings: `url` is required"
    );

    let unknown = one(json!({"kind": "parquet", "url": "/in", "bogus": 1}));
    assert_eq!(unknown.field, "source.settings.bogus");
    assert_eq!(
        unknown.message,
        "`parquet` settings: `bogus` is not a setting of `parquet`"
    );

    let typed = one(json!({"kind": "parquet", "url": 5}));
    assert_eq!(
        typed.message,
        "`parquet` settings: `url` must be a string or an array, not an integer"
    );

    let nested = one(json!({"kind": "parquet", "url": "/in", "options": {"columns": ["a", 2]}}));
    assert_eq!(nested.field, "source.settings.options.columns[1]");
    assert_eq!(
        nested.code,
        RefusalCode::Settings {
            path: "/options/columns/1".into()
        }
    );
    assert_eq!(
        nested.message,
        "`parquet` settings: `options.columns[1]` must be a string, not an integer"
    );

    let filter =
        one(json!({"kind": "parquet", "url": "/in", "options": {"filters": [["a", ">"]]}}));
    assert_eq!(filter.field, "source.settings.options.filters[0]");
    assert_eq!(
        filter.message,
        "`parquet` settings: `options.filters[0]` must have at least 3 items"
    );
    let long = one(json!({"kind": "parquet", "url": "/in",
                          "options": {"filters": [["a", ">", 1, 2]]}}));
    assert!(long.message.contains("must have at most 3 items"), "{long}");

    let sink = |settings: Value| {
        let doc = job(
            json!({"kind": "parquet", "url": "/in"}),
            Vec::new(),
            settings,
        );
        refusals(&doc)
    };
    let negative = sink(json!({"kind": "parquet", "url": "/o", "options": {"file_bytes": -1}}));
    assert_eq!(negative[0].field, "sink.settings.options.file_bytes");
    assert_eq!(
        negative[0].message,
        "`parquet` settings: `options.file_bytes` must be at least 0"
    );
    // The widest `u64` is allowed, and one past it is not a number a `u64` can hold.
    let widest = job(
        json!({"kind": "parquet", "url": "/in"}),
        Vec::new(),
        json!({"kind": "parquet", "url": "/o",
               "options": {"row_group_bytes": 18446744073709551615u64}}),
    );
    assert_eq!(
        v1(
            &widest,
            &Catalogue::standard().expect("the standard catalogue")
        ),
        Ok(())
    );
    let fractional = sink(json!({"kind": "parquet", "url": "/o", "options": {"file_bytes": 1.5}}));
    assert!(
        fractional[0]
            .message
            .contains("must be an integer or null, not a number")
    );
    let object = sink(json!({"kind": "parquet", "url": {"a": 1}}));
    assert!(object[0].message.contains("not an object"), "{}", object[0]);

    // An enum names what it allows.
    let kernel = |settings: Value| {
        let doc = job(
            json!({"kind": "parquet", "url": "/in"}),
            vec![settings],
            json!({"kind": "parquet", "url": "/out"}),
        );
        refusals(&doc)
    };
    let name = kernel(json!({"kind": "std", "name": "frobnicate"}));
    assert_eq!(name[0].field, "kernels[0].settings.name");
    assert!(
        name[0]
            .message
            .starts_with("`std` settings: `name` must be one of \"cast\", \"rename\", \"select\""),
        "{}",
        name[0]
    );
    assert!(
        name[0].message.ends_with("\"date_trunc\" and 1 more"),
        "{}",
        name[0]
    );
    // `args` is open by nature: any JSON.
    assert_eq!(
        v1(
            &job(
                json!({"kind": "parquet", "url": "/in"}),
                vec![json!({"kind": "std", "name": "cast", "args": 7})],
                json!({"kind": "parquet", "url": "/out"})
            ),
            &Catalogue::standard().expect("the standard catalogue")
        ),
        Ok(())
    );
}

/// JB-T9 every_refusal_is_collected: all of a document's refusals come back, in the order the
/// document is read: each task, then the edges, the ports, the graph and the contracts.
#[test]
fn jb_t9_every_refusal_is_collected() {
    let doc = job(
        json!({"kind": "parquet"}),
        vec![
            json!({"kind": "java"}),
            json!({"kind": "std"}),
            json!({"kind": "std", "name": "cast", "x": 1, "y": 2}),
        ],
        json!({"kind": "vortex", "url": 3}),
    );
    let fields: Vec<String> = refusals(&doc).into_iter().map(|r| r.field).collect();
    assert_eq!(
        fields,
        [
            "source.settings.url",
            "kernels[0].kind",
            "kernels[1].settings.name",
            "kernels[2].settings.x",
            "kernels[2].settings.y",
            "sink.settings.url",
        ]
    );
}

/// JB-T10 contracts_are_needed: a kind whose `needs.contracts` is `yes` is refused when the job
/// carries none, once for each such entry, at `contracts` and naming the entry; carried, it
/// passes. A kind that needs none is not refused for carrying none.
#[test]
fn jb_t10_contracts_are_needed() {
    let governed = Catalogue::standard_with_peql().expect("the governed catalogue");
    let example = |role: Role, name: &str| {
        let mut settings = governed
            .get(role, name)
            .expect("declared")
            .examples()
            .remove(0);
        settings["kind"] = json!(name);
        settings
    };
    let mut doc = job(
        example(Role::Source, "datafusion"),
        Vec::new(),
        example(Role::Sink, "peql"),
    );
    let found = v1(&doc, &governed).expect_err("no contract is carried");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .all(|r| r.code == RefusalCode::Contract && r.field == "contracts")
    );
    assert_eq!(
        found[0].message,
        "`datafusion` at `source` reads or writes under a contract, and the job carries none: \
         put it in `contracts`"
    );
    assert!(
        found[1].message.starts_with("`peql` at `sink` "),
        "{}",
        found[1]
    );
    doc.contracts = vec![contract()];
    assert_eq!(v1(&doc, &governed), Ok(()));
    // The standard catalogue does not have them at all.
    assert!(
        v1(
            &doc,
            &Catalogue::standard().expect("the standard catalogue")
        )
        .is_err()
    );
    assert_eq!(v1(&good(), &governed), Ok(()));
}
