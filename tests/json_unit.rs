use mux_runtime::json::Json;

#[test]
fn parse_primitives() {
    assert_eq!(Json::parse("null").unwrap(), Json::Null);
    assert_eq!(Json::parse("true").unwrap(), Json::Bool(true));
    assert_eq!(Json::parse("123").unwrap(), Json::Int(123));
    assert_eq!(Json::parse("-1.5").unwrap(), Json::Float(-1.5));
}

#[test]
fn stringify_roundtrip() {
    let s = r#"{"a": [1, 2, 3], "b": null}"#;
    let j = Json::parse(s).expect("parse ok");
    let compact = j.stringify(None);
    let reparsed = Json::parse(&compact).expect("reparse ok");
    assert_eq!(j, reparsed);
}

#[test]
fn pretty_indent() {
    let s = r#"{"k": 1}"#;
    let j = Json::parse(s).unwrap();
    let pretty = j.stringify(Some(4));
    assert!(pretty.contains("\n    \"k\": 1"));
}

#[test]
fn parse_strings_arrays_objects() {
    assert_eq!(
        Json::parse(r#""hi""#).unwrap(),
        Json::String("hi".to_string())
    );

    let arr = Json::parse("[1, 2, 3]").unwrap();
    match arr {
        Json::Array(items) => assert_eq!(items.len(), 3),
        other => panic!("expected array, got {other:?}"),
    }

    let obj = Json::parse(r#"{"a": 1, "b": true}"#).unwrap();
    match obj {
        Json::Object(map) => {
            assert_eq!(map.get("a"), Some(&Json::Int(1)));
            assert_eq!(map.get("b"), Some(&Json::Bool(true)));
        }
        other => panic!("expected object, got {other:?}"),
    }
}

#[test]
fn parse_rejects_malformed() {
    assert!(Json::parse("{").is_err());
    assert!(Json::parse("[1,").is_err());
    assert!(Json::parse("nul").is_err());
}

#[test]
fn parse_handles_escapes() {
    assert_eq!(
        Json::parse(r#""a\nb""#).unwrap(),
        Json::String("a\nb".to_string())
    );
}

/// Parsing then re-serializing must return the input unchanged.
///
/// This is the property that a single `f64` number case could not hold:
/// `{"n":42}` came back `{"n":42.0}`, and anything past 2^53 came back a
/// different number entirely. Asserting round-trip identity over a table covers
/// every one of those at once - it is a much stronger claim than "parses" or
/// "is a number", which is what the previous tests checked.
#[test]
fn numbers_survive_a_roundtrip() {
    let cases = [
        r#"{"n":42}"#,
        r#"{"n":0}"#,
        r#"{"n":-7}"#,
        r#"{"n":1.5}"#,
        r#"{"n":-0.25}"#,
        // Past 2^53, where an f64 silently rounds to an even neighbour.
        r#"{"n":9007199254740993}"#,
        r#"{"n":-9007199254740993}"#,
        r#"[1,2,3]"#,
        r#"{"a":true,"b":null}"#,
        r#"{"s":"hi"}"#,
        // Keys must come back in the order they were written. A sorted map
        // returned {"apple":2,"zebra":1} here. Note every other case above is
        // already alphabetical, which is how the re-ordering stayed hidden.
        r#"{"zebra":1,"apple":2}"#,
        r#"{"z":{"b":1,"a":2}}"#,
    ];

    for case in cases {
        let parsed = Json::parse(case).unwrap_or_else(|e| panic!("parse {case}: {e}"));
        assert_eq!(parsed.stringify(None), case, "round trip changed {case}");
    }
}

/// An integer and a real are distinct cases, not one number that happens to be
/// integral. `42` must not become `42.0` on the way back out.
#[test]
fn integers_and_reals_stay_distinct() {
    assert_eq!(Json::parse("42").unwrap(), Json::Int(42));
    assert_eq!(Json::parse("42.0").unwrap(), Json::Float(42.0));
    assert_eq!(Json::Int(42).stringify(None), "42");
    assert_eq!(Json::Float(42.0).stringify(None), "42.0");
}
