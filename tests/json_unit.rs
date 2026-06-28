use mux_runtime::json::Json;

#[test]
fn parse_primitives() {
    assert_eq!(Json::parse("null").unwrap(), Json::Null);
    assert_eq!(Json::parse("true").unwrap(), Json::Bool(true));
    assert_eq!(Json::parse("123").unwrap(), Json::Number(123.0));
    assert_eq!(Json::parse("-1.5").unwrap(), Json::Number(-1.5));
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
            assert_eq!(map.get("a"), Some(&Json::Number(1.0)));
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
