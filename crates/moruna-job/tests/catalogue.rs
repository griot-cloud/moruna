//! SDD 22 k: the catalogue (JB-T1 to JB-T5).

use moruna_job::{
    Catalogue, CatalogueError, Declaration, Fact, Flow, Isolation, LISTING_VERSION, Listing, Needs,
    Role, schema,
};
use serde_json::json;

fn names(catalogue: &Catalogue, role: Role) -> Vec<&str> {
    catalogue.names(role).collect()
}

/// A declaration of a kind of `role` called `name`, with a schema of one string setting `a`.
fn declaration(role: Role, name: &str) -> Declaration {
    Declaration {
        name: name.into(),
        role,
        summary: "a kind the tests declare".into(),
        settings: json!({
            "$schema": moruna_job::SCHEMA_DIALECT,
            "type": "object",
            "additionalProperties": false,
            "properties": {"a": {"type": "string"}},
            "required": ["a"],
            "examples": [{"a": "x"}]
        }),
        inputs: if role == Role::Source {
            Flow::Files
        } else {
            Flow::Batches
        },
        outputs: if role == Role::Sink {
            Flow::Files
        } else {
            Flow::Batches
        },
        needs: Needs::nothing(),
        deterministic: Fact::Yes,
        preserves_columns: Fact::Yes,
        isolation: Isolation::Builtin,
    }
}

/// JB-T1 dependencies_are_the_four: the crate an embedder depends on names `serde`,
/// `serde_json`, `schemars` and `thiserror`, and nothing else, in its dependencies (SDD 22 JB-I1).
#[test]
fn jb_t1_dependencies_are_the_four() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("the manifest");
    let mut section = "";
    let mut found = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = if name.contains("dependencies") {
                // Keep the section's own name: `dependencies`, or any other that holds crates.
                Box::leak(name.to_string().into_boxed_str())
            } else {
                ""
            };
        } else if !section.is_empty() && !line.is_empty() && !line.starts_with('#') {
            let (name, _) = line.split_once('=').expect("a dependency line");
            found.push((section.to_string(), name.trim().to_string()));
        }
    }
    found.sort();
    let wanted: Vec<(String, String)> = ["schemars", "serde", "serde_json", "thiserror"]
        .iter()
        .map(|n| ("dependencies".to_string(), n.to_string()))
        .collect();
    assert_eq!(found, wanted);
}

/// JB-T2 embedded_catalogues: the standard kinds, by role in name order, every one a declaration
/// the registry accepts with an example; the `peql` kinds are two more and nothing else changes.
#[test]
fn jb_t2_embedded_catalogues() {
    let standard = Catalogue::standard().expect("the standard catalogue");
    assert_eq!(
        names(&standard, Role::Source),
        ["parquet", "tensor", "vortex"]
    );
    assert_eq!(names(&standard, Role::Kernel), ["package", "std"]);
    assert_eq!(
        names(&standard, Role::Sink),
        ["arrow_ipc", "parquet", "tensor", "vortex"]
    );
    let governed = Catalogue::standard_with_peql().expect("the governed catalogue");
    assert_eq!(
        names(&governed, Role::Source),
        ["datafusion", "parquet", "tensor", "vortex"]
    );
    assert_eq!(names(&governed, Role::Kernel), ["package", "std"]);
    assert_eq!(
        names(&governed, Role::Sink),
        ["arrow_ipc", "parquet", "peql", "tensor", "vortex"]
    );
    for catalogue in [&standard, &governed] {
        for role in [Role::Source, Role::Kernel, Role::Sink] {
            for declared in catalogue.declarations(role) {
                assert_eq!(declared.check(role), Ok(()), "{}", declared.name);
                assert!(!declared.examples().is_empty(), "{}", declared.name);
                assert!(std::ptr::eq(
                    declared,
                    catalogue.get(role, &declared.name).expect("by name")
                ));
            }
        }
    }
    // A name is a kind of a role: no source is called `arrow_ipc`, no sink `std`.
    assert!(standard.get(Role::Source, "arrow_ipc").is_none());
    assert!(standard.get(Role::Sink, "std").is_none());
    assert!(standard.get(Role::Source, "datafusion").is_none());
    assert_eq!(Catalogue::empty().names(Role::Kernel).count(), 0);
    // The governed kinds need contracts; the others do not (SDD 18 KD-I8).
    for (role, name) in [(Role::Source, "datafusion"), (Role::Sink, "peql")] {
        let needs = governed.get(role, name).expect("declared").needs.contracts;
        assert_eq!(needs, Fact::Yes, "{name}");
    }
}

/// JB-T3 add_refusals: a declaration is taken in when it passes `Declaration::check` for its role
/// and no kind of that role has its name; the refusal names the role and the name.
#[test]
fn jb_t3_add_refusals() {
    let mut catalogue = Catalogue::standard().expect("the standard catalogue");
    catalogue
        .add(declaration(Role::Kernel, "test.scale"))
        .expect("a kind of the embedder's own");
    assert!(catalogue.get(Role::Kernel, "test.scale").is_some());
    assert!(
        Catalogue::standard()
            .expect("the standard catalogue")
            .get(Role::Kernel, "test.scale")
            .is_none()
    );

    let taken = catalogue
        .add(declaration(Role::Source, "parquet"))
        .expect_err("parquet reads already");
    assert_eq!(
        taken,
        CatalogueError::Taken {
            role: Role::Source,
            name: "parquet".into()
        }
    );
    assert_eq!(
        taken.to_string(),
        "a source kind `parquet` is declared already"
    );
    // One name may be a kind in several roles.
    catalogue
        .add(declaration(Role::Sink, "test.scale"))
        .expect("a sink of the same name");

    let mut broken = declaration(Role::Kernel, "Test");
    let refused = catalogue.add(broken.clone()).expect_err("not a kind name");
    assert!(
        matches!(&refused, CatalogueError::Refused { role: Role::Kernel, why } if why.contains("is not a kind name")),
        "{refused}"
    );
    assert!(refused.to_string().starts_with("not a kernel kind: "));
    broken.name = "fine".into();
    broken.inputs = Flow::Files;
    assert!(matches!(
        catalogue.add(broken),
        Err(CatalogueError::Refused { .. })
    ));
}

/// JB-T4 schema_subset_is_closed: a schema with a keyword outside the subset the validator reads,
/// or a `$ref` it cannot follow, is refused when it is added; the catalogue is unchanged.
#[test]
fn jb_t4_schema_subset_is_closed() {
    let mut catalogue = Catalogue::empty();
    let with = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut d = declaration(Role::Kernel, "probe");
        edit(&mut d.settings);
        d
    };
    let cases: [(&str, Declaration); 5] = [
        (
            "pattern",
            with(&|s| s["properties"]["a"]["pattern"] = json!("^x")),
        ),
        (
            "uniqueItems",
            with(&|s| s["properties"]["a"] = json!({"type": "array", "uniqueItems": true})),
        ),
        ("if", with(&|s| s["if"] = json!({}))),
        (
            "$dynamicRef",
            with(&|s| s["properties"]["a"] = json!({"$dynamicRef": "#x"})),
        ),
        (
            "patternProperties",
            with(&|s| s["patternProperties"] = json!({})),
        ),
    ];
    for (keyword, declared) in cases {
        let refused = catalogue.add(declared).expect_err(keyword);
        let CatalogueError::Schema { role, name, fault } = &refused else {
            panic!("{keyword}: {refused}");
        };
        assert_eq!((*role, name.as_str()), (Role::Kernel, "probe"));
        assert!(fault.why.contains(keyword), "{fault}");
        assert!(fault.why.contains("outside the subset"), "{fault}");
        assert!(refused.to_string().contains("cannot be checked against"));
    }
    assert!(catalogue.get(Role::Kernel, "probe").is_none());
    // The pointer says where in the schema the keyword is.
    let nested = catalogue
        .add(with(&|s| s["properties"]["a"]["pattern"] = json!("x")))
        .expect_err("nested");
    assert!(
        nested.to_string().contains("/properties/a/pattern"),
        "{nested}"
    );

    let shapes: [(&str, serde_json::Value); 9] = [
        ("type", json!({"type": "text"})),
        ("type", json!({"type": ["string", 3]})),
        ("required", json!({"required": "a"})),
        ("enum", json!({"enum": 3})),
        ("minimum", json!({"minimum": "0"})),
        ("minLength", json!({"minLength": -1})),
        ("properties", json!({"properties": []})),
        ("oneOf", json!({"oneOf": {}})),
        ("a schema", json!({"items": 3})),
    ];
    for (keyword, shape) in shapes {
        let mut d = declaration(Role::Kernel, "probe");
        d.settings["properties"]["a"] = shape;
        let refused = catalogue.add(d).expect_err(keyword);
        assert!(
            matches!(refused, CatalogueError::Schema { .. }),
            "{keyword}"
        );
    }
    for target in [
        "#/properties/a",
        "#/$defs/missing",
        "http://elsewhere/x.json",
    ] {
        let mut d = declaration(Role::Kernel, "probe");
        d.settings["properties"]["a"] = json!({"$ref": target});
        let refused = catalogue.add(d).expect_err(target);
        assert!(refused.to_string().contains("$ref"), "{refused}");
    }
    let mut d = declaration(Role::Kernel, "probe");
    d.settings["properties"]["a"] = json!({"$ref": 3});
    assert!(catalogue.add(d).is_err());
    // What the subset has is taken in, annotations and boolean schemas included.
    let mut d = declaration(Role::Kernel, "probe");
    d.settings["properties"]["a"] = json!({
        "type": ["string", "null"], "title": "t", "description": "d", "default": null,
        "examples": ["x"], "format": "uri", "minLength": 1, "const": "x"
    });
    d.settings["properties"]["b"] = json!(true);
    d.settings["properties"]["c"] = json!({"allOf": [{"type": "string"}], "enum": ["x"]});
    d.settings["properties"]["d"] =
        json!({"oneOf": [{"type": "string"}, {"type": "integer", "minimum": 0, "maximum": 9}]});
    d.settings["$defs"] = json!({"n": {"type": "null"}});
    d.settings["properties"]["e"] = json!({"$ref": "#/$defs/n"});
    d.settings["properties"]["f"] = json!({"type": "array", "items": false, "prefixItems": [true],
        "minItems": 0, "maxItems": 1});
    d.settings["properties"]["g"] =
        json!({"type": "object", "additionalProperties": {"type": "string"}});
    catalogue.add(d).expect("every keyword of the subset");
    assert!(schema::admit(&json!(3)).is_err());
}

/// JB-T5 from_listing: a listing of the format this crate reads is a catalogue of what it lists,
/// and any other format, or a declaration in the wrong list, is refused.
#[test]
fn jb_t5_from_listing() {
    let listing = Listing {
        moruna_kinds: LISTING_VERSION,
        moruna: "x".into(),
        sources: vec![declaration(Role::Source, "s")],
        kernels: vec![declaration(Role::Kernel, "k")],
        sinks: vec![declaration(Role::Sink, "t")],
    };
    let catalogue = Catalogue::from_listing(listing.clone()).expect("a listing");
    assert_eq!(names(&catalogue, Role::Source), ["s"]);
    assert_eq!(names(&catalogue, Role::Kernel), ["k"]);
    assert_eq!(names(&catalogue, Role::Sink), ["t"]);

    let later = Listing {
        moruna_kinds: LISTING_VERSION + 1,
        ..listing.clone()
    };
    let refused = Catalogue::from_listing(later).expect_err("another format");
    assert_eq!(refused, CatalogueError::Format { found: 2 });
    assert!(refused.to_string().contains("format 2"));

    let mut twice = listing.clone();
    twice.sources.push(declaration(Role::Source, "s"));
    assert!(matches!(
        Catalogue::from_listing(twice),
        Err(CatalogueError::Taken { .. })
    ));
    let mut misplaced = listing;
    misplaced.sinks.push(declaration(Role::Source, "elsewhere"));
    // A declaration listed under another role than the one it declares is refused.
    let refused = Catalogue::from_listing(misplaced).expect_err("a source in the sinks");
    assert!(
        matches!(&refused, CatalogueError::Refused { role: Role::Sink, why } if why.contains("registered as a sink")),
        "{refused}"
    );
}
