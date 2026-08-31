use mux_runtime::json::Json;

/// The `ok` payload of an accessor result, asserting it succeeded.
fn ok_payload(result: *mut mux_runtime::Value) -> mux_runtime::Value {
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::result::{mux_result_data, mux_result_is_ok};

    assert!(unsafe { mux_result_is_ok(result) }, "expected ok");
    let data = unsafe { mux_result_data(result) };
    let value = unsafe { &*data }.clone();
    assert!(unsafe { mux_rc_dec(data) });
    assert!(unsafe { mux_rc_dec(result) });
    value
}

/// The `err` message, asserting it failed.
fn err_message(result: *mut mux_runtime::Value) -> String {
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::result::{mux_result_data, mux_result_is_err};

    assert!(unsafe { mux_result_is_err(result) }, "expected err");
    let data = unsafe { mux_result_data(result) };
    let message = match unsafe { &*data } {
        mux_runtime::Value::String(s) => s.clone(),
        other => panic!("an error must carry a message, got {other:?}"),
    };
    assert!(unsafe { mux_rc_dec(data) });
    assert!(unsafe { mux_rc_dec(result) });
    message
}

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
        r"[1,2,3]",
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
    use mux_runtime::Value;

    let doc = Json::parse(r#"{"name":"Ada","age":36,"active":true}"#).unwrap();
    let map = match doc {
        Json::Object(m) => m,
        other => panic!("expected object, got {other:?}"),
    };

    let name = json_to_value(map.get("name").unwrap());
    assert_eq!(
        ok_payload(unsafe { mux_json_as_string(&raw const name) }),
        Value::String("Ada".into()),
        "as_string must yield Ada, not \"Ada\""
    );

    let age = json_to_value(map.get("age").unwrap());
    assert_eq!(
        ok_payload(unsafe { mux_json_as_int(&raw const age) }),
        Value::Int(36)
    );

    let active = json_to_value(map.get("active").unwrap());
    assert_eq!(
        ok_payload(unsafe { mux_json_as_bool(&raw const active) }),
        Value::Bool(true)
    );
}

/// Asking for the wrong kind names what was actually there.
///
/// A `result` rather than an `optional`: "not an int" is worth saying WHY. A
/// bare "no" leaves the reader unable to tell a string from an absent field
/// from something else entirely, which is exactly the information someone
/// debugging a document needs.
#[test]
fn accessors_report_the_kind_they_found() {
    use mux_runtime::json::{mux_json_as_int, mux_json_as_string, mux_json_is_null};
    use mux_runtime::Value;

    let text = Value::String("not a number".into());
    assert_eq!(
        err_message(unsafe { mux_json_as_int(&raw const text) }),
        "expected an int, found a string"
    );

    let number = Value::Int(7);
    assert_eq!(
        err_message(unsafe { mux_json_as_string(&raw const number) }),
        "expected a string, found an int"
    );

    let nothing = Value::Unit;
    assert_eq!(
        err_message(unsafe { mux_json_as_int(&raw const nothing) }),
        "expected an int, found null"
    );

    // A null pointer is not a kind, so it says so rather than guessing.
    assert_eq!(
        err_message(unsafe { mux_json_as_int(std::ptr::null()) }),
        "expected an int, found nothing"
    );

    // A null is a kind of its own, not an absent value.
    assert!(unsafe { mux_json_is_null(&Value::Unit) });
    assert!(!unsafe { mux_json_is_null(&Value::Int(0)) });
    assert!(!unsafe { mux_json_is_null(std::ptr::null()) });
}

/// Numbers convert the way a reader expects: an integral float reads as an int
/// so `{"n": 42.0}` still works, a fractional one does not silently truncate,
/// and an int widens to float on request.
#[test]
fn number_accessors_convert_deliberately() {
    use mux_runtime::json::{mux_json_as_float, mux_json_as_int};
    use mux_runtime::Value;

    let integral = Value::Float(ordered_float::OrderedFloat(42.0));
    assert_eq!(
        ok_payload(unsafe { mux_json_as_int(&raw const integral) }),
        Value::Int(42)
    );

    let fractional = Value::Float(ordered_float::OrderedFloat(1.5));
    assert_eq!(
        err_message(unsafe { mux_json_as_int(&raw const fractional) }),
        "expected an int, found a float",
        "1.5 must not truncate to 1"
    );

    // Out of i64 range. These are integral and finite, so only the range check
    // rejects them - without it `as i64` SATURATES and hands back i64::MAX, a
    // plausible number that is not the one in the document.
    for enormous in [1e30_f64, -1e30_f64, f64::MAX, f64::MIN] {
        let v = Value::Float(ordered_float::OrderedFloat(enormous));
        assert!(
            !err_message(unsafe { mux_json_as_int(&raw const v) }).is_empty(),
            "{enormous} is outside i64 and must be an error, not a saturated bound"
        );
    }

    // The largest float that still converts exactly, to pin the boundary rather
    // than only the far side of it.
    let big = Value::Float(ordered_float::OrderedFloat(9_007_199_254_740_992.0));
    assert_eq!(
        ok_payload(unsafe { mux_json_as_int(&raw const big) }),
        Value::Int(9_007_199_254_740_992)
    );

    let whole = Value::Int(3);
    assert_eq!(
        ok_payload(unsafe { mux_json_as_float(&raw const whole) }),
        Value::Float(ordered_float::OrderedFloat(3.0))
    );
}

/// A field lookup tells an ABSENT key from one explicitly set to `null`.
///
/// Typed deserialization depends on the difference: a missing required field is
/// an error, while `optional<T>` accepts either - so the primitive has to keep
/// them apart rather than collapsing both to "nothing there".
#[test]
fn field_lookup_separates_absent_from_null() {
    use mux_runtime::json::{json_to_value, mux_json_field, mux_json_is_null};
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;
    use std::ffi::CString;

    let doc = Json::parse(r#"{"name":"Ada","bio":null}"#).unwrap();
    let value = json_to_value(&doc);

    let key = |k: &str| CString::new(k).expect("no interior nul");

    // Present and a real value.
    let got = unsafe { mux_json_field(&raw const value, key("name").as_ptr()) };
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::String("Ada".into()))))
    );
    assert!(unsafe { mux_rc_dec(got) });

    // Present but null: `some`, holding the null. NOT absent.
    let got = unsafe { mux_json_field(&raw const value, key("bio").as_ptr()) };
    match unsafe { &*got } {
        Value::Optional(Some(inner)) => {
            assert!(
                unsafe { mux_json_is_null(inner.as_ref()) },
                "bio should hold JSON null"
            );
        }
        other => panic!("an explicit null must be some(null), got {other:?}"),
    }
    assert!(unsafe { mux_rc_dec(got) });

    // Absent: `none`.
    let got = unsafe { mux_json_field(&raw const value, key("missing").as_ptr()) };
    assert_eq!(unsafe { &*got }, &Value::Optional(None));
    assert!(unsafe { mux_rc_dec(got) });

    // Not an object at all, and a null key.
    let scalar = Value::Int(1);
    let got = unsafe { mux_json_field(&raw const scalar, key("any").as_ptr()) };
    assert_eq!(unsafe { &*got }, &Value::Optional(None));
    assert!(unsafe { mux_rc_dec(got) });

    let got = unsafe { mux_json_field(&raw const value, std::ptr::null()) };
    assert_eq!(unsafe { &*got }, &Value::Optional(None));
    assert!(unsafe { mux_rc_dec(got) });
}
