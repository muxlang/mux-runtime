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

/// Typed accessors return the DECODED value, which is the whole point: reading
/// a string out of a document used to be impossible, because `stringify` gave
/// back `"Ada"` with the quotes and nothing could strip them.
#[test]
fn accessors_return_decoded_values() {
    use mux_runtime::json::{json_to_value, mux_json_as_bool, mux_json_as_int, mux_json_as_string};
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;

    let doc = Json::parse(r#"{"name":"Ada","age":36,"active":true}"#).unwrap();
    let map = match doc {
        Json::Object(m) => m,
        other => panic!("expected object, got {other:?}"),
    };

    let name = json_to_value(map.get("name").unwrap());
    let got = mux_json_as_string(&name);
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::String("Ada".into())))),
        "as_string must yield Ada, not \"Ada\""
    );
    assert!(mux_rc_dec(got));

    let age = json_to_value(map.get("age").unwrap());
    let got = mux_json_as_int(&age);
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Int(36))))
    );
    assert!(mux_rc_dec(got));

    let active = json_to_value(map.get("active").unwrap());
    let got = mux_json_as_bool(&active);
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Bool(true))))
    );
    assert!(mux_rc_dec(got));
}

/// Asking for the wrong kind is `none`, not a wrong answer. That is why these
/// return an optional: a field holding a string where a number was expected is
/// ordinary when reading a document.
#[test]
fn accessors_reject_the_wrong_kind() {
    use mux_runtime::json::{mux_json_as_int, mux_json_as_string, mux_json_is_null};
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;

    let text = Value::String("not a number".into());
    let got = mux_json_as_int(&text);
    assert_eq!(unsafe { &*got }, &Value::Optional(None));
    assert!(mux_rc_dec(got));

    let number = Value::Int(7);
    let got = mux_json_as_string(&number);
    assert_eq!(unsafe { &*got }, &Value::Optional(None));
    assert!(mux_rc_dec(got));

    // A null is a kind of its own, not an absent value.
    assert!(mux_json_is_null(&Value::Unit));
    assert!(!mux_json_is_null(&Value::Int(0)));
    assert!(!mux_json_is_null(std::ptr::null()));
}

/// Numbers convert the way a reader expects: an integral float reads as an int
/// so `{"n": 42.0}` still works, a fractional one does not silently truncate,
/// and an int widens to float on request.
#[test]
fn number_accessors_convert_deliberately() {
    use mux_runtime::json::{mux_json_as_float, mux_json_as_int};
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;

    let integral = Value::Float(ordered_float::OrderedFloat(42.0));
    let got = mux_json_as_int(&integral);
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Int(42))))
    );
    assert!(mux_rc_dec(got));

    let fractional = Value::Float(ordered_float::OrderedFloat(1.5));
    let got = mux_json_as_int(&fractional);
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(None),
        "1.5 must not truncate to 1"
    );
    assert!(mux_rc_dec(got));

    let whole = Value::Int(3);
    let got = mux_json_as_float(&whole);
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Float(ordered_float::OrderedFloat(
            3.0
        )))))
    );
    assert!(mux_rc_dec(got));
}
