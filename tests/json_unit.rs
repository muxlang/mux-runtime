use mux_runtime::json::Json;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::mux_result_is_ok;
use mux_runtime::stream::{
    mux_io_reader_close, mux_io_reader_from_bytes, mux_io_writer_bytes, mux_io_writer_close,
    mux_io_writer_new,
};

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
    use mux_runtime::std::mux_json_error_message;

    assert!(unsafe { mux_result_is_err(result) }, "expected err");
    let data = unsafe { mux_result_data(result) };
    let message_value = unsafe { mux_json_error_message(data) };
    let message = match unsafe { &*message_value } {
        mux_runtime::Value::String(s) => s.clone(),
        other => panic!("an error message must be a string, got {other:?}"),
    };
    assert!(unsafe { mux_rc_dec(message_value) });
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
fn duplicate_key_policies_preserve_selected_exact_numbers() {
    use mux_runtime::json::JsonDuplicatePolicy;

    let input = r#"{"a":1,"nested":{"a":1e+2,"a":9007199254740993},"a":2.50}"#;
    let rejected = Json::parse(input).expect_err("default policy must reject duplicates");
    assert!(rejected.contains("duplicate object key 'a'"));

    let first = Json::parse_with_policy(input, JsonDuplicatePolicy::First).unwrap();
    assert_eq!(first.stringify(None), r#"{"a":1,"nested":{"a":1e+2}}"#);

    let last = Json::parse_with_policy(input, JsonDuplicatePolicy::Last).unwrap();
    assert_eq!(
        last.stringify(None),
        r#"{"a":2.50,"nested":{"a":9007199254740993}}"#
    );

    let invalid_discarded = r#"{"a":1,"a":{"broken":}}"#;
    assert!(Json::parse_with_policy(invalid_discarded, JsonDuplicatePolicy::First).is_err());
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
fn reader_writer_bridge_is_bounded_and_keeps_handles_open() {
    let input = mux_rc_alloc(mux_runtime::Value::Bytes(br#"{"ok":true}"#.to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    assert!(!reader.is_null());
    assert!(unsafe { mux_rc_dec(input) });

    let parsed = unsafe { mux_runtime::json::mux_json_parse_reader(reader, 1024) };
    let document = ok_payload(parsed);
    let document = mux_rc_alloc(document);
    let writer = mux_io_writer_new();
    let indent = mux_rc_alloc(mux_runtime::Value::Optional(None));
    let written = unsafe { mux_runtime::json::mux_json_stringify_to(document, writer, indent) };
    assert!(unsafe { mux_result_is_ok(written) });
    assert!(unsafe { mux_rc_dec(written) });
    assert!(unsafe { mux_rc_dec(indent) });
    assert!(unsafe { mux_rc_dec(document) });

    let bytes = unsafe { mux_io_writer_bytes(writer) };
    assert_eq!(
        ok_payload(bytes),
        mux_runtime::Value::Bytes(br#"{"ok":true}"#.to_vec())
    );
    unsafe {
        mux_io_reader_close(reader);
        mux_io_writer_close(writer);
    }
    assert!(unsafe { mux_rc_dec(reader) });
    assert!(unsafe { mux_rc_dec(writer) });
}

#[test]
fn token_reader_yields_exact_tokens_and_scalar_values() {
    use mux_runtime::json::{
        mux_json_number_to_string, mux_json_token_kind, mux_json_token_reader_close,
        mux_json_token_reader_from_reader, mux_json_token_reader_next, mux_json_token_text,
        mux_json_token_value,
    };
    use mux_runtime::Value;

    let input = mux_rc_alloc(Value::Bytes(br#"{"a":[1.50,true,null]}"#.to_vec()));
    let source = unsafe { mux_io_reader_from_bytes(input) };
    assert!(!source.is_null());
    assert!(unsafe { mux_rc_dec(input) });
    let reader_result = unsafe { mux_json_token_reader_from_reader(source, 1024) };
    let reader = mux_rc_alloc(ok_payload(reader_result));

    let expected = [
        (0_i32, "{"),
        (6, r#""a""#),
        (4, ":"),
        (2, "["),
        (7, "1.50"),
        (5, ","),
        (8, "true"),
        (5, ","),
        (9, "null"),
        (3, "]"),
        (1, "}"),
    ];
    for (index, (kind, text)) in expected.iter().enumerate() {
        let optional = ok_payload(unsafe { mux_json_token_reader_next(reader) });
        let Value::Optional(Some(token)) = optional else {
            panic!("expected token {index}");
        };
        let token = mux_rc_alloc(token.as_ref().clone());
        assert_eq!(
            ok_payload(unsafe { mux_json_token_kind(token) }),
            Value::Opaque((*kind).to_ne_bytes().to_vec().into_boxed_slice())
        );
        assert_eq!(
            ok_payload(unsafe { mux_json_token_text(token) }),
            Value::String((*text).into())
        );
        if *kind == 7 {
            let value = ok_payload(unsafe { mux_json_token_value(token) });
            let number = mux_rc_alloc(value);
            let rendered = unsafe { mux_json_number_to_string(number) };
            assert_eq!(unsafe { &*rendered }, &Value::String("1.50".into()));
            assert!(unsafe { mux_rc_dec(rendered) });
            assert!(unsafe { mux_rc_dec(number) });
        }
        assert!(unsafe { mux_rc_dec(token) });
    }
    assert!(matches!(
        ok_payload(unsafe { mux_json_token_reader_next(reader) }),
        Value::Optional(None)
    ));
    unsafe { mux_json_token_reader_close(reader) };
    assert!(unsafe { mux_rc_dec(reader) });
    unsafe { mux_io_reader_close(source) };
    assert!(unsafe { mux_rc_dec(source) });
}

#[test]
fn token_reader_close_invalidates_aliases_without_panicking() {
    use mux_runtime::json::{
        mux_json_token_reader_close, mux_json_token_reader_from_reader, mux_json_token_reader_next,
    };
    use mux_runtime::result::mux_result_is_err;
    use mux_runtime::Value;

    let input = mux_rc_alloc(Value::Bytes(br"{}".to_vec()));
    let source = unsafe { mux_io_reader_from_bytes(input) };
    assert!(unsafe { mux_rc_dec(input) });
    let reader_result = unsafe { mux_json_token_reader_from_reader(source, 1024) };
    let reader = mux_rc_alloc(ok_payload(reader_result));
    let alias = mux_rc_alloc(unsafe { (&*reader).clone() });

    unsafe { mux_json_token_reader_close(reader) };
    let failed = unsafe { mux_json_token_reader_next(alias) };
    assert!(unsafe { mux_result_is_err(failed) });
    assert!(unsafe { mux_rc_dec(failed) });
    assert!(unsafe { mux_rc_dec(alias) });
    assert!(unsafe { mux_rc_dec(reader) });
    unsafe { mux_io_reader_close(source) };
    assert!(unsafe { mux_rc_dec(source) });
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
    let duplicate = Json::parse(r#"{"a":1,"a":2}"#).expect_err("duplicate keys must be rejected");
    assert!(
        duplicate.contains("duplicate"),
        "unexpected error: {duplicate}"
    );
}

#[test]
fn parse_rejects_documents_over_the_token_budget() {
    // Each array element contributes a number token, and each separator is a
    // token too.  This stays well below the byte limit while exceeding the
    // one-million-token limit enforced by every JSON parser entry point.
    let input = format!("[{}0]", "0,".repeat(500_000));
    let error = Json::parse(&input).expect_err("oversized token streams must be rejected");
    assert!(
        error.contains("one-million-token"),
        "unexpected error: {error}"
    );
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

#[test]
fn noncanonical_numbers_are_lossless_and_convert_on_request() {
    use mux_runtime::json::{
        json_to_value, mux_json_number_as_float, mux_json_number_as_int, mux_json_number_to_string,
    };
    let source = r#"{"scale":1.50,"exponent":1e3,"negative_zero":-0,"large":9223372036854775808}"#;
    let parsed = Json::parse(source).expect("parse ok");
    assert_eq!(parsed.stringify(None), source);

    let Json::Object(fields) = &parsed else {
        panic!("expected object");
    };
    for (field, token) in [
        ("scale", "1.50"),
        ("exponent", "1e3"),
        ("negative_zero", "-0"),
        ("large", "9223372036854775808"),
    ] {
        let Json::Number(number) = fields.get(field).expect("number field") else {
            panic!("{field} should use JsonNumber");
        };
        assert_eq!(number.as_str(), token);
    }

    let value = mux_rc_alloc(json_to_value(fields.get("scale").expect("scale")));
    let rendered = unsafe { mux_json_number_to_string(value) };
    assert_eq!(
        unsafe { &*rendered },
        &mux_runtime::Value::String("1.50".into())
    );
    assert!(unsafe { mux_rc_dec(rendered) });
    assert_eq!(
        ok_payload(unsafe { mux_json_number_as_float(value) }),
        mux_runtime::Value::Float(ordered_float::OrderedFloat(1.5))
    );
    assert_eq!(
        err_message(unsafe { mux_json_number_as_int(value) }),
        "JSON number is not an integer in the Mux int range"
    );
    assert!(unsafe { mux_rc_dec(value) });
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

#[test]
fn mutable_json_objects_and_arrays_update_in_place() {
    use mux_runtime::json::{mux_json_push, mux_json_set_field};
    use mux_runtime::ordered::OrderedMap;
    use mux_runtime::Value;

    let mut object = Value::Map(OrderedMap::new());
    let key = Value::String("answer".into());
    let value = Value::Int(42);
    let result = unsafe { mux_json_set_field(&raw mut object, &raw const key, &raw const value) };
    assert_eq!(ok_payload(result), Value::Unit);
    let Value::Map(map) = &object else {
        panic!("expected object");
    };
    assert_eq!(map.get(&key), Some(&value));

    let mut array = Value::List(Vec::new());
    let item = Value::String("ok".into());
    let result = unsafe { mux_json_push(&raw mut array, &raw const item) };
    assert_eq!(ok_payload(result), Value::Unit);
    assert_eq!(array, Value::List(vec![item]));
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

#[test]
fn json_pointers_read_replace_append_and_remove() {
    use mux_runtime::json::{
        json_to_value, mux_json_at_pointer, mux_json_remove_pointer, mux_json_set_pointer,
    };
    use mux_runtime::Value;
    use std::ffi::CString;

    let document = Json::parse(r#"{"users":[{"name":"Ada"}],"a/b":1}"#).unwrap();
    let mut value = json_to_value(&document);
    let pointer = |text: &str| CString::new(text).expect("pointer has no NUL");

    let name = pointer("/users/0/name");
    assert_eq!(
        ok_payload(unsafe { mux_json_at_pointer(&raw const value, name.as_ptr()) }),
        Value::String("Ada".into())
    );

    let replacement = Value::String("Grace".into());
    let name = pointer("/users/0/name");
    assert_eq!(
        ok_payload(unsafe {
            mux_json_set_pointer(&raw mut value, name.as_ptr(), &raw const replacement)
        }),
        Value::Unit
    );
    let append = pointer("/users/-");
    let user = Value::Map({
        let mut map = mux_runtime::ordered::OrderedMap::new();
        map.insert(Value::String("name".into()), Value::String("Lin".into()));
        map
    });
    assert_eq!(
        ok_payload(unsafe {
            mux_json_set_pointer(&raw mut value, append.as_ptr(), &raw const user)
        }),
        Value::Unit
    );

    let escaped = pointer("/a~1b");
    assert_eq!(
        ok_payload(unsafe { mux_json_at_pointer(&raw const value, escaped.as_ptr()) }),
        Value::Int(1)
    );
    let removed = pointer("/users/0/name");
    assert_eq!(
        ok_payload(unsafe { mux_json_remove_pointer(&raw mut value, removed.as_ptr()) }),
        Value::String("Grace".into())
    );
    assert!(
        err_message(unsafe { mux_json_at_pointer(&raw const value, removed.as_ptr()) })
            .contains("missing")
    );
}

#[test]
fn canonical_json_sorts_object_keys_recursively() {
    use mux_runtime::json::{json_to_value, mux_json_canonical};
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;

    let document = Json::parse(r#"{"z":2,"a":{"y":1,"b":0},"m":[{"d":4,"c":3}]}"#).unwrap();
    assert_eq!(
        document.canonical(),
        r#"{"a":{"b":0,"y":1},"m":[{"c":3,"d":4}],"z":2}"#
    );

    let value = json_to_value(&document);
    let rendered = unsafe { mux_json_canonical(&raw const value) };
    let Value::String(text) = (unsafe { &*rendered }) else {
        panic!("canonical serializer must return a string");
    };
    assert_eq!(text, r#"{"a":{"b":0,"y":1},"m":[{"c":3,"d":4}],"z":2}"#);
    assert!(unsafe { mux_rc_dec(rendered) });
}

#[test]
fn merge_patch_updates_nested_objects_and_deletes_null_fields() {
    use mux_runtime::json::{json_to_value, mux_json_merge_patch};
    use mux_runtime::Value;

    let document = Json::parse(r#"{"keep":1,"drop":2,"nested":{"old":true,"same":1}}"#).unwrap();
    let patch = Json::parse(r#"{"drop":null,"nested":{"old":null,"new":3}}"#).unwrap();
    let mut value = json_to_value(&document);
    let patch_value = json_to_value(&patch);
    let result = unsafe { mux_json_merge_patch(&raw mut value, &raw const patch_value) };
    if unsafe { matches!(&*result, Value::Result(Err(_))) } {
        panic!("merge patch failed: {}", err_message(result));
    }
    assert_eq!(ok_payload(result), Value::Unit);
    let rendered = unsafe { mux_runtime::json::mux_json_to_string(&raw const value) };
    assert_eq!(
        unsafe { &*rendered },
        &Value::String(r#"{"keep":1,"nested":{"same":1,"new":3}}"#.into())
    );
    assert!(unsafe { mux_runtime::refcount::mux_rc_dec(rendered) });
}

#[test]
fn json_patch_applies_operations_atomically() {
    use mux_runtime::json::{json_to_value, mux_json_apply_patch};
    use mux_runtime::Value;

    let document = Json::parse(r#"{"name":"Ada","items":[1,2],"nested":{"old":true}}"#).unwrap();
    let patch = Json::parse(
        r#"[
            {"op":"replace","path":"/name","value":"Grace"},
            {"op":"add","path":"/items/1","value":9},
            {"op":"copy","from":"/name","path":"/alias"},
            {"op":"move","from":"/nested/old","path":"/nested/new"},
            {"op":"test","path":"/items/0","value":1}
        ]"#,
    )
    .unwrap();
    let mut value = json_to_value(&document);
    let patch_value = json_to_value(&patch);
    let result = unsafe { mux_json_apply_patch(&raw mut value, &raw const patch_value) };
    assert_eq!(ok_payload(result), Value::Unit);
    assert_eq!(
        value,
        json_to_value(
            &Json::parse(
                r#"{"name":"Grace","items":[1,9,2],"nested":{"new":true},"alias":"Grace"}"#,
            )
            .unwrap()
        )
    );

    let bad_patch = json_to_value(&Json::parse(
        r#"[{"op":"replace","path":"/name","value":"broken"},{"op":"test","path":"/missing","value":1}]"#,
    )
    .unwrap());
    let failed = unsafe { mux_json_apply_patch(&raw mut value, &raw const bad_patch) };
    assert!(err_message(failed).contains("missing"));
    assert!(
        matches!(&value, Value::Map(fields) if fields.get(&Value::String("name".into())) == Some(&Value::String("Grace".into())))
    );
}

#[test]
fn json_lines_parse_and_render_with_line_errors() {
    use mux_runtime::json::{mux_json_parse_lines, mux_json_stringify_lines};
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::result::{mux_result_data, mux_result_is_ok};
    use std::ffi::CString;

    let input = CString::new("{\"a\":1}\n\n[2,3]\n").unwrap();
    let parsed = unsafe { mux_json_parse_lines(input.as_ptr()) };
    assert!(unsafe { mux_result_is_ok(parsed) });
    let list = unsafe { mux_result_data(parsed) };
    assert!(unsafe { mux_rc_dec(parsed) });
    assert!(unsafe { matches!(&*list, mux_runtime::Value::List(values) if values.len() == 2) });
    let rendered = unsafe { mux_json_stringify_lines(list) };
    assert_eq!(
        ok_payload(rendered),
        mux_runtime::Value::String("{\"a\":1}\n[2,3]\n".to_string())
    );
    assert!(unsafe { mux_rc_dec(list) });

    let bad = CString::new("1\nnot-json\n").unwrap();
    let failed = unsafe { mux_json_parse_lines(bad.as_ptr()) };
    assert!(err_message(failed).contains("line 2"));
}

#[test]
fn json_lines_render_rejects_unbounded_total_output() {
    use mux_runtime::json::mux_json_stringify_lines;
    use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};

    let line = "x".repeat(9 * 1024 * 1024);
    let values = mux_rc_alloc(mux_runtime::Value::List(vec![
        mux_runtime::Value::String(line.clone()),
        mux_runtime::Value::String(line),
    ]));
    let result = unsafe { mux_json_stringify_lines(values) };
    assert!(err_message(result).contains("16 MiB"));
    unsafe {
        assert!(mux_rc_dec(values));
    }
}

#[test]
fn json_pointer_paths_reject_input_and_token_limits() {
    use mux_runtime::json::{json_to_value, mux_json_at_pointer};
    use std::ffi::CString;

    let document = mux_rc_alloc(json_to_value(&Json::parse("{}").expect("empty object")));
    let oversized =
        CString::new(format!("/{}", "x".repeat(16 * 1024 * 1024))).expect("pointer has no NUL");
    let error = unsafe { mux_json_at_pointer(document, oversized.as_ptr()) };
    assert!(err_message(error).contains("16 MiB"));

    let token_count = 1_000_001;
    let token_limit = CString::new(format!("/{}", "x/".repeat(token_count - 1) + "x"))
        .expect("pointer has no NUL");
    let error = unsafe { mux_json_at_pointer(document, token_limit.as_ptr()) };
    assert!(err_message(error).contains("one-million-token"));
    unsafe {
        assert!(mux_rc_dec(document));
    }
}
