//! SDD 22 k: the validator of the schema subset (JB-T12 to JB-T17): each keyword of the subset
//! on its own, in the words a person reads.

use moruna_job::schema::{Step, Violation, Wrong, admit, kind_of, validate};
use serde_json::{Value, json};

/// Every violation of `value` against `schema`, which is its own root.
fn wrong(schema: Value, value: Value) -> Vec<Wrong> {
    assert_eq!(admit(&schema), Ok(()), "{schema}");
    validate(&schema, &schema, &value)
        .into_iter()
        .map(|v: Violation| v.kind)
        .collect()
}

fn fine(schema: Value, value: Value) {
    let found = wrong(schema.clone(), value.clone());
    assert!(found.is_empty(), "{schema} refused {value}: {found:?}");
}

/// JB-T12 types: `type` as a name or a list; an integer is a number without a fraction, as
/// `serde` reads one; `null` is its own type.
#[test]
fn jb_t12_types() {
    fine(json!({"type": "string"}), json!("a"));
    fine(json!({"type": ["string", "null"]}), json!(null));
    fine(json!({"type": "number"}), json!(1));
    fine(json!({"type": "number"}), json!(1.5));
    fine(json!({"type": "integer"}), json!(-3));
    fine(json!({"type": "integer"}), json!(18446744073709551615u64));
    fine(json!({"type": "boolean"}), json!(false));
    fine(json!({"type": "array"}), json!([]));
    fine(json!({"type": "object"}), json!({}));
    let not_integer = wrong(json!({"type": "integer"}), json!(5.0));
    assert_eq!(
        not_integer,
        [Wrong::Type {
            allowed: vec!["integer".into()],
            found: "a number"
        }]
    );
    assert_eq!(
        wrong(json!({"type": ["string", "array"]}), json!(true)),
        [Wrong::Type {
            allowed: vec!["string".into(), "array".into()],
            found: "a boolean"
        }]
    );
    let kinds = [
        (json!(null), "null"),
        (json!(true), "a boolean"),
        (json!(1), "an integer"),
        (json!(1.5), "a number"),
        (json!("a"), "a string"),
        (json!([]), "an array"),
        (json!({}), "an object"),
    ];
    for (value, name) in kinds {
        assert_eq!(kind_of(&value), name);
    }
}

/// JB-T13 objects: `properties`, `required`, `additionalProperties` as `false`, `true` and a
/// schema; the path of each violation is the value's.
#[test]
fn jb_t13_objects() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "object", "properties": {
            "c": {"type": "integer"}}, "required": ["c"], "additionalProperties": false}},
        "required": ["a"],
        "additionalProperties": false
    });
    fine(schema.clone(), json!({"a": "x", "b": {"c": 1}}));
    let found = validate(&schema, &schema, &json!({"b": {"d": 1}, "z": 0}));
    let at = |v: &Violation| {
        v.path
            .iter()
            .map(|s| match s {
                Step::Key(k) => k.clone(),
                Step::Index(i) => i.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/")
    };
    let seen: Vec<(String, Wrong)> = found.iter().map(|v| (at(v), v.kind.clone())).collect();
    assert_eq!(
        seen,
        [
            ("a".to_string(), Wrong::Missing),
            ("b/c".to_string(), Wrong::Missing),
            ("b/d".to_string(), Wrong::Undeclared),
            ("z".to_string(), Wrong::Undeclared),
        ]
    );
    fine(
        json!({"properties": {"a": {}}, "additionalProperties": true}),
        json!({"a": 1, "q": 2}),
    );
    fine(json!({"properties": {"a": {}}}), json!({"q": 2}));
    let open = json!({"additionalProperties": {"type": "string"}});
    fine(open.clone(), json!({"x": "a", "y": "b"}));
    assert_eq!(wrong(open, json!({"x": 1})).len(), 1);
    assert_eq!(
        wrong(json!({"properties": {"a": false}}), json!({"a": 1})),
        [Wrong::Forbidden]
    );
    fine(json!({"properties": {"a": true}}), json!({"a": 1}));
}

/// JB-T14 arrays: `items`, `prefixItems`, `minItems`, `maxItems`.
#[test]
fn jb_t14_arrays() {
    let tuple = json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "string"}, true],
                       "minItems": 3, "maxItems": 3});
    fine(tuple.clone(), json!(["a", ">", 5]));
    assert_eq!(
        wrong(tuple.clone(), json!(["a", ">"])),
        [Wrong::TooFewItems(3)]
    );
    assert_eq!(
        wrong(tuple.clone(), json!(["a", ">", 5, 6])),
        [Wrong::TooManyItems(3)]
    );
    assert_eq!(wrong(tuple, json!(["a", 1, 5])).len(), 1);
    let list = json!({"type": "array", "items": {"type": "string"}});
    fine(list.clone(), json!(["a", "b"]));
    assert_eq!(wrong(list, json!(["a", 2, 3])).len(), 2);
    // An item past the prefix is the `items` schema's; none declared allows anything.
    fine(
        json!({"prefixItems": [{"type": "string"}]}),
        json!(["a", 1, 2]),
    );
    assert_eq!(
        wrong(
            json!({"prefixItems": [{"type": "string"}], "items": {"type": "integer"}}),
            json!(["a", "b"])
        )
        .len(),
        1
    );
}

/// JB-T15 values: `enum`, `const`, `minimum`, `maximum`, `minLength`, and the integer widths a
/// `format` names; any other `format` is not checked.
#[test]
fn jb_t15_values() {
    let listed = json!({"enum": ["a", "b", 3]});
    fine(listed.clone(), json!("b"));
    fine(listed.clone(), json!(3));
    assert_eq!(
        wrong(listed, json!("c")),
        [Wrong::NotOneOf(vec![json!("a"), json!("b"), json!(3)])]
    );
    fine(json!({"const": {"a": 1}}), json!({"a": 1}));
    assert_eq!(
        wrong(json!({"const": "a"}), json!("b")),
        [Wrong::NotConst(json!("a"))]
    );

    let bounded = json!({"type": "number", "minimum": 1, "maximum": 2.5});
    fine(bounded.clone(), json!(1));
    fine(bounded.clone(), json!(2.5));
    assert_eq!(
        wrong(bounded.clone(), json!(0.5)),
        [Wrong::Below("1".into())]
    );
    assert_eq!(wrong(bounded, json!(3)), [Wrong::Above("2.5".into())]);
    let float_floor = json!({"minimum": 0.5});
    assert_eq!(wrong(float_floor, json!(0)), [Wrong::Below("0.5".into())]);

    fine(json!({"minLength": 2}), json!("ab"));
    fine(json!({"minLength": 2}), json!("é©"));
    assert_eq!(
        wrong(json!({"minLength": 2}), json!("é")),
        [Wrong::TooShort(2)]
    );
    fine(json!({"minLength": 2}), json!(5));

    let widths = [
        ("uint8", 255i64, 256i64),
        ("uint16", 65535, 65536),
        ("uint32", 4294967295, 4294967296),
        ("int8", 127, 128),
        ("int16", 32767, 32768),
        ("int32", 2147483647, 2147483648),
    ];
    for (format, top, over) in widths {
        let schema = json!({"type": "integer", "format": format});
        fine(schema.clone(), json!(top));
        assert_eq!(wrong(schema.clone(), json!(over)).len(), 1, "{format}");
        assert_eq!(wrong(schema.clone(), json!(i64::MIN)).len(), 1, "{format}");
    }
    let u64s = json!({"type": "integer", "format": "uint64", "minimum": 0});
    fine(u64s.clone(), json!(u64::MAX));
    assert_eq!(wrong(u64s, json!(-1)), [Wrong::Below("0".into())]);
    let i64s = json!({"type": "integer", "format": "int64"});
    fine(i64s.clone(), json!(i64::MIN));
    assert_eq!(
        wrong(i64s, json!(u64::MAX)),
        [Wrong::Above(i64::MAX.to_string())]
    );
    fine(json!({"format": "uri"}), json!("not checked"));
    fine(json!({"type": "number", "format": "double"}), json!(1e300));
    fine(json!({"format": "uint8"}), json!(1000.5));
}

/// JB-T16 combinators: `allOf` holds every form, `anyOf` at least one, `oneOf` exactly one; the
/// refusal says what the forms were; `$ref` follows `$defs`, nested.
#[test]
fn jb_t16_combinators_and_references() {
    let all = json!({"allOf": [{"type": "integer"}, {"minimum": 3}]});
    fine(all.clone(), json!(5));
    assert_eq!(wrong(all, json!(1)), [Wrong::Below("3".into())]);

    let any =
        json!({"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]});
    fine(any.clone(), json!("a"));
    fine(any.clone(), json!(["a"]));
    assert_eq!(
        wrong(any, json!(3)),
        [Wrong::NoForm {
            forms: vec!["a string".into(), "an array".into()],
            found: "an integer"
        }]
    );

    let one = json!({"oneOf": [{"type": "integer"}, {"type": "number", "minimum": 0}]});
    fine(one.clone(), json!(-1));
    fine(one.clone(), json!(0.5));
    assert_eq!(wrong(one.clone(), json!(2)), [Wrong::ManyForms]);
    assert!(matches!(wrong(one, json!("x"))[0], Wrong::NoForm { .. }));
    let described = json!({"anyOf": [{"const": "a"}, {"$ref": "#/$defs/n"}, {"enum": [1, 2]}, {"type": ["integer", "null"]}],
                           "$defs": {"n": {"type": "null"}}});
    assert_eq!(
        wrong(described, json!(true)),
        [Wrong::NoForm {
            forms: vec![
                "\"a\"".into(),
                "null".into(),
                "another form".into(),
                "an integer or null".into()
            ],
            found: "a boolean"
        }]
    );

    let schema = json!({
        "type": "object",
        "properties": {"f": {"$ref": "#/$defs/Filter", "description": "sibling keywords stay"}},
        "$defs": {"Filter": {"type": "array", "items": {"$ref": "#/$defs/Cell"}},
                  "Cell": {"type": "string"}}
    });
    fine(schema.clone(), json!({"f": ["a", "b"]}));
    assert_eq!(wrong(schema.clone(), json!({"f": ["a", 2]})).len(), 1);
    assert_eq!(wrong(schema, json!({"f": "a"})).len(), 1);
    let escaped = json!({"$ref": "#/$defs/a~1b~0c", "$defs": {"a/b~c": {"type": "string"}}});
    fine(escaped.clone(), json!("x"));
    assert_eq!(wrong(escaped, json!(1)).len(), 1);
}

/// JB-T17 admit: what is not in the subset, or not shaped as its keyword needs, is refused where
/// it is; and a schema that refers to itself ends.
#[test]
fn jb_t17_admit_and_depth() {
    let fault = admit(&json!({"properties": {"a": {"pattern": "x"}}})).expect_err("pattern");
    assert_eq!(fault.at, "/properties/a/pattern");
    assert!(
        fault
            .to_string()
            .starts_with("at `/properties/a/pattern`: the keyword `pattern`")
    );
    let root = admit(&json!(3)).expect_err("not a schema");
    assert_eq!(root.at, "");
    assert!(
        root.to_string()
            .starts_with("a schema is an object or a boolean"),
        "{root}"
    );
    assert_eq!(admit(&json!(true)), Ok(()));
    let key = admit(&json!({"properties": {"a/b": {"nope": 1}}})).expect_err("escaped");
    assert_eq!(key.at, "/properties/a~1b/nope");
    // A definition that refers to itself without end is refused at validation, not looped on.
    let cycle = json!({"$ref": "#/$defs/a", "$defs": {"a": {"$ref": "#/$defs/a"}}});
    assert_eq!(admit(&cycle), Ok(()));
    assert_eq!(validate(&cycle, &cycle, &json!(1)).len(), 1);
}
