//! URL parsing, validated components, and query-pair preservation.
#![cfg(feature = "url")]

use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_url_error_detail, mux_url_error_kind, mux_url_error_url};
use mux_runtime::url::*;
use mux_runtime::{Tuple, Value};

fn string_value(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

unsafe fn result_data(result: *mut Value) -> *mut Value {
    assert!(!result.is_null(), "URL runtime returned a null result");
    if !mux_result_is_ok(result) {
        let error = mux_result_data(result);
        let message = if error.is_null() {
            "non-result value".to_string()
        } else {
            format!("{:?}", &*error)
        };
        panic!("expected Ok result, got {message}");
    }
    let data = mux_result_data(result);
    assert!(!data.is_null());
    assert!(mux_rc_dec(result));
    data
}

unsafe fn result_string(result: *mut Value) -> String {
    let data = result_data(result);
    let value = match &*data {
        Value::String(value) => value.clone(),
        other => panic!("expected string, got {other:?}"),
    };
    assert!(mux_rc_dec(data));
    value
}

unsafe fn direct_string(value: *mut Value) -> String {
    let result = match &*value {
        Value::String(value) => value.clone(),
        other => panic!("expected string, got {other:?}"),
    };
    assert!(mux_rc_dec(value));
    result
}

#[test]
fn parse_components_join_and_redaction_are_validated() {
    unsafe {
        let source = string_value("https://alice:secret@example.com:8443/a%20b?x=1&x=two#frag");
        let url = result_data(mux_url_parse(source));
        assert!(mux_rc_dec(source));

        assert_eq!(direct_string(mux_url_scheme(url)), "https");
        assert_eq!(direct_string(mux_url_username(url)), "alice");
        assert_eq!(direct_string(mux_url_path(url)), "/a%20b");
        assert_eq!(
            direct_string(mux_url_origin(url)),
            "https://example.com:8443"
        );
        assert_eq!(
            direct_string(mux_url_redacted(url)),
            "https://alice:%5BREDACTED%5D@example.com:8443/a%20b?x=1&x=two#frag"
        );
        assert!(mux_url_is_https(url));
        assert!(!mux_url_is_http(url));

        let relative = string_value("../next?q=ok");
        let joined = result_data(mux_url_join(url, relative));
        assert!(mux_rc_dec(relative));
        assert_eq!(
            direct_string(mux_url_to_string(joined)),
            "https://alice:secret@example.com:8443/next?q=ok"
        );
        assert!(mux_rc_dec(joined));
        assert!(mux_rc_dec(url));
    }
}

#[test]
fn origin_serialization_brackets_ipv6_hosts() {
    unsafe {
        let source = string_value("https://[::1]:8443/resource");
        let url = result_data(mux_url_parse(source));
        assert!(mux_rc_dec(source));
        assert_eq!(direct_string(mux_url_origin(url)), "https://[::1]:8443");
        assert!(mux_rc_dec(url));
    }
}

#[test]
fn duplicate_query_pairs_round_trip_in_order() {
    unsafe {
        let source = string_value("https://example.com/search?a=1&a=two");
        let url = result_data(mux_url_parse(source));
        assert!(mux_rc_dec(source));

        let pairs = mux_url_query_pairs(url);
        assert!(matches!(&*pairs, Value::List(values) if values.len() == 2));
        assert!(mux_rc_dec(pairs));

        let replacement = mux_rc_alloc(Value::List(vec![
            Value::Tuple(Box::new(Tuple(
                Value::String("q".to_string()),
                Value::String("hello world".to_string()),
            ))),
            Value::Tuple(Box::new(Tuple(
                Value::String("q".to_string()),
                Value::String("again".to_string()),
            ))),
        ]));
        let updated = result_data(mux_url_with_query_pairs(url, replacement));
        assert!(mux_rc_dec(replacement));
        assert_eq!(
            direct_string(mux_url_to_string(updated)),
            "https://example.com/search?q=hello+world&q=again"
        );
        assert!(mux_rc_dec(updated));
        assert!(mux_rc_dec(url));
    }
}

#[test]
fn file_urls_and_invalid_inputs_return_results() {
    unsafe {
        let file_path = std::env::temp_dir().join("mux-url-test.txt");
        let file_path_text = file_path.to_string_lossy().into_owned();
        let path = string_value(&file_path_text);
        let file = result_data(mux_url_from_file(path));
        assert!(mux_rc_dec(path));
        assert_eq!(direct_string(mux_url_scheme(file)), "file");
        let round_trip = result_string(mux_url_to_file_path(file));
        assert_eq!(round_trip, file_path_text);
        assert!(mux_rc_dec(file));

        let invalid = string_value("://not a URL");
        let result = mux_url_parse(invalid);
        assert!(!mux_result_is_ok(result));
        let error = mux_result_data(result);
        assert!(matches!(&*error, Value::Object(_)));
        let kind = mux_url_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 2_i32.to_ne_bytes()));
        let detail = mux_url_error_detail(error);
        assert!(matches!(&*detail, Value::String(value) if !value.is_empty()));
        let url = mux_url_error_url(error);
        assert!(matches!(&*url, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(url));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(invalid));
    }
}

#[test]
fn idna_and_host_port_mutation_are_explicit() {
    unsafe {
        let source = string_value("https://bücher.example/path");
        let url = result_data(mux_url_parse(source));
        assert!(mux_rc_dec(source));
        assert_eq!(
            result_string(mux_url_host_ascii(url)),
            "xn--bcher-kva.example"
        );
        assert_eq!(result_string(mux_url_host_unicode(url)), "bücher.example");

        let replacement = string_value("example.org");
        let updated = result_data(mux_url_with_host(url, replacement));
        assert!(mux_rc_dec(replacement));
        let updated_port = result_data(mux_url_with_port(updated, 8443));
        let user = string_value("alice");
        let updated_user = result_data(mux_url_with_username(updated_port, user));
        assert!(mux_rc_dec(user));
        let password = string_value("secret");
        let updated_credentials = result_data(mux_url_with_password(updated_user, password));
        assert!(mux_rc_dec(password));
        assert_eq!(
            direct_string(mux_url_to_string(updated_credentials)),
            "https://alice:secret@example.org:8443/path"
        );
        assert!(mux_rc_dec(updated_credentials));
        assert!(mux_rc_dec(updated_user));
        assert!(mux_rc_dec(updated_port));
        assert!(mux_rc_dec(updated));
        assert!(mux_rc_dec(url));
    }
}

#[cfg(feature = "uuid")]
#[test]
fn url_rejects_handles_from_other_opaque_types() {
    unsafe {
        let uuid = mux_runtime::uuid::mux_uuid_nil();
        let result = mux_url_with_port(uuid, 443);
        assert!(!mux_result_is_ok(result));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(uuid));
    }
}
