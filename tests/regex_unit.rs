//! Unicode regex API coverage.
#![cfg(feature = "regex")]

use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::regex::*;
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_regex_error_detail, mux_regex_error_kind};
use mux_runtime::Value;
use std::ffi::{c_void, CString};

unsafe fn result_data(result: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(result), "expected Ok result");
    let data = mux_result_data(result);
    assert!(!data.is_null());
    assert!(mux_rc_dec(result));
    data
}

fn string_value(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

unsafe fn assert_full_match(pattern: &str, flags: &str, text: &str) {
    let pattern_value = string_value(pattern);
    let flags_value = string_value(flags);
    let regex = result_data(mux_regex_from_pattern_with_flags(
        pattern_value,
        flags_value,
    ));
    assert!(mux_rc_dec(pattern_value));
    assert!(mux_rc_dec(flags_value));

    let text_value = string_value(text);
    let full = result_data(mux_regex_full_match(regex, text_value));
    assert!(matches!(&*full, Value::Bool(true)));
    assert!(mux_rc_dec(full));
    assert!(mux_rc_dec(text_value));
    assert!(mux_rc_dec(regex));
}

#[repr(C)]
struct CallbackRepr {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
}

struct ReentrantCallbackContext {
    regex: *mut Value,
    calls: usize,
    nested_match_succeeded: bool,
}

extern "C" fn reentrant_replacement(context_ptr: *mut c_void, argument: *mut Value) -> *mut Value {
    unsafe {
        let context = &mut *context_ptr.cast::<ReentrantCallbackContext>();
        context.calls += 1;

        let nested_text = string_value("nested");
        let nested_result = mux_regex_is_match(context.regex, nested_text);
        if mux_result_is_ok(nested_result) {
            let nested_data = mux_result_data(nested_result);
            context.nested_match_succeeded =
                !nested_data.is_null() && matches!(&*nested_data, Value::Bool(true));
            if !nested_data.is_null() {
                mux_rc_dec(nested_data);
            }
        }
        if !nested_result.is_null() {
            mux_rc_dec(nested_result);
        }
        mux_rc_dec(nested_text);

        let replacement = match &*argument {
            Value::String(value) => format!("<{value}>"),
            _ => String::from("<invalid>"),
        };
        mux_rc_alloc(Value::String(replacement))
    }
}

#[test]
fn regex_matches_unicode_and_named_captures() {
    unsafe {
        let pattern = string_value("(?P<word>héllo)");
        let regex = result_data(mux_regex_from_pattern(pattern));
        assert!(mux_rc_dec(pattern));

        let text = string_value("say héllo");
        let matched = result_data(mux_regex_is_match(regex, text));
        assert!(matches!(&*matched, Value::Bool(true)));
        assert!(mux_rc_dec(matched));
        let full = result_data(mux_regex_full_match(regex, text));
        assert!(matches!(&*full, Value::Bool(false)));
        assert!(mux_rc_dec(full));

        let found_optional = result_data(mux_regex_find(regex, text));
        let found = match &*found_optional {
            Value::Optional(Some(value)) => value.as_ref() as *const Value as *mut Value,
            _ => panic!("expected a match"),
        };
        let start = result_data(mux_regex_match_start(found));
        let end = result_data(mux_regex_match_end(found));
        assert!(matches!(&*start, Value::Int(4)));
        assert!(matches!(&*end, Value::Int(9)));
        assert!(mux_rc_dec(start));
        assert!(mux_rc_dec(end));
        let capture_name = string_value("word");
        let capture = result_data(mux_regex_match_capture_named(found, capture_name));
        assert!(
            matches!(&*capture, Value::Optional(Some(value)) if matches!(value.as_ref(), Value::String(text) if text == "héllo"))
        );
        assert!(mux_rc_dec(capture));
        assert!(mux_rc_dec(capture_name));
        assert!(mux_rc_dec(found_optional));
        assert!(mux_rc_dec(text));
        assert!(mux_rc_dec(regex));
    }
}

#[test]
fn regex_full_match_anchors_before_resolving_match_choice() {
    unsafe {
        assert_full_match("a|ab", "", "ab");
        assert_full_match("a.*?", "", "ab");
        assert_full_match("a.b", "s", "a\nb");
        assert_full_match("(?x)a # comment", "", "a");
        assert_full_match("(?x:(?:a # comment\n)(?-x:#))", "", "a#");
        assert_full_match("a#comment", "", "a#comment");
    }
}

#[test]
fn regex_replace_with_allows_reentrant_regex_calls() {
    unsafe {
        let pattern = string_value("[a-z]");
        let regex = result_data(mux_regex_from_pattern(pattern));
        assert!(mux_rc_dec(pattern));

        let text = string_value("a1b");
        let mut context = ReentrantCallbackContext {
            regex,
            calls: 0,
            nested_match_succeeded: false,
        };
        let callback = CallbackRepr {
            function_ptr: reentrant_replacement as *const () as *mut c_void,
            captures_ptr: (&mut context as *mut ReentrantCallbackContext).cast(),
        };

        let replaced = result_data(mux_regex_replace_with(
            regex,
            text,
            (&callback as *const CallbackRepr).cast_mut().cast(),
        ));
        assert!(matches!(&*replaced, Value::String(value) if value == "<a>1<b>"));
        assert_eq!(context.calls, 2);
        assert!(context.nested_match_succeeded);
        assert!(mux_rc_dec(replaced));
        assert!(mux_rc_dec(text));
        assert!(mux_rc_dec(regex));
    }
}

#[test]
fn regex_replacement_split_and_flags_are_checked() {
    unsafe {
        let pattern = string_value("cat");
        let flags = string_value("i");
        let regex = result_data(mux_regex_from_pattern_with_flags(pattern, flags));
        assert!(mux_rc_dec(pattern));
        assert!(mux_rc_dec(flags));

        let text = string_value("Cat dog cat");
        let replacement = string_value("fox");
        let replaced = result_data(mux_regex_replace(regex, text, replacement));
        assert!(matches!(&*replaced, Value::String(value) if value == "fox dog fox"));
        assert!(mux_rc_dec(replaced));
        let first = result_data(mux_regex_replace_first(regex, text, replacement));
        assert!(matches!(&*first, Value::String(value) if value == "fox dog cat"));
        assert!(mux_rc_dec(first));
        let pieces = result_data(mux_regex_split(regex, text));
        assert!(matches!(&*pieces, Value::List(values) if values.len() == 3));
        assert!(mux_rc_dec(pieces));
        assert!(mux_rc_dec(replacement));
        assert!(mux_rc_dec(text));
        assert!(mux_rc_dec(regex));

        let bad_flags = string_value("q");
        let pattern = string_value("cat");
        let bad_regex = mux_regex_from_pattern_with_flags(pattern, bad_flags);
        assert!(!mux_result_is_ok(bad_regex));
        let error = mux_result_data(bad_regex);
        assert_eq!(direct_i32(mux_regex_error_kind(error)), 0);
        assert!(direct_string(mux_regex_error_detail(error)).contains("flag"));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(bad_regex));
        assert!(mux_rc_dec(pattern));
        assert!(mux_rc_dec(bad_flags));

        let escaped = string_value("a+b");
        let escaped_result = mux_regex_escape(escaped);
        assert!(matches!(&*escaped_result, Value::String(value) if value == r"a\+b"));
        assert!(mux_rc_dec(escaped_result));
        assert!(mux_rc_dec(escaped));
    }
}

#[test]
fn regex_invalid_patterns_return_errors() {
    let pattern = CString::new("(").unwrap();
    unsafe {
        let pattern_value = string_value(&pattern.to_string_lossy());
        let result = mux_regex_from_pattern(pattern_value);
        assert!(!mux_result_is_ok(result));
        let error = mux_result_data(result);
        assert_eq!(direct_i32(mux_regex_error_kind(error)), 0);
        assert!(direct_string(mux_regex_error_detail(error)).contains("regex"));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(pattern_value));
    }
}

fn direct_string(value: *mut Value) -> String {
    unsafe {
        let output = match &*value {
            Value::String(value) => value.clone(),
            other => panic!("expected String, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}

fn direct_i32(value: *mut Value) -> i32 {
    unsafe {
        let output = match &*value {
            Value::Opaque(bytes) => i32::from_ne_bytes(bytes.as_ref().try_into().unwrap()),
            other => panic!("expected Opaque, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}
