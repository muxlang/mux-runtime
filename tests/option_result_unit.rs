//! Unit tests for Optional and Result: pure enum cores plus the C-ABI
//! constructors/predicates (with explicit refcount cleanup to stay leak-free).

use mux_runtime::optional::{self, Optional};
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{self, MuxResult};
use mux_runtime::Value;

// --- pure enum cores ---------------------------------------------------------

#[test]
fn optional_core() {
    assert_eq!(format!("{}", Optional::some(Value::Int(1))), "Some(1)");
    assert_eq!(format!("{}", Optional::none()), "None");
}

#[test]
fn result_core() {
    assert_eq!(format!("{}", MuxResult::ok(Value::Int(1))), "Ok(1)");
    assert_eq!(
        format!("{}", MuxResult::err("boom".to_string())),
        "Err(boom)"
    );
}

// --- C-ABI optional ----------------------------------------------------------

#[test]
fn optional_extern_roundtrip() {
    let some = optional::mux_optional_some_int(5);
    assert!(optional::mux_optional_is_some(some));
    assert!(!optional::mux_optional_is_none(some));
    assert_eq!(optional::mux_value_optional_discriminant(some), 0);

    let inner = optional::mux_optional_get_value(some);
    assert!(!inner.is_null());
    unsafe {
        assert!(matches!(&*inner, Value::Int(5)));
    }
    assert!(mux_rc_dec(inner));

    let none = optional::mux_optional_none();
    assert!(optional::mux_optional_is_none(none));
    assert_eq!(optional::mux_value_optional_discriminant(none), 1);

    assert!(mux_rc_dec(some));
    assert!(mux_rc_dec(none));
}

// --- C-ABI result ------------------------------------------------------------

#[test]
fn result_extern_roundtrip() {
    let ok = result::mux_result_ok_int(7);
    assert!(result::mux_result_is_ok(ok));
    assert!(!result::mux_result_is_err(ok));
    assert_eq!(result::mux_value_result_discriminant(ok), 0);

    let data = result::mux_result_data(ok);
    assert!(!data.is_null());
    unsafe {
        assert!(matches!(&*data, Value::Int(7)));
    }
    assert!(mux_rc_dec(data));
    assert!(mux_rc_dec(ok));
}
