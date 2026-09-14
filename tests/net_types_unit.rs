use mux_runtime::net_types::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::std::{mux_net_error_address, mux_net_error_kind, mux_net_error_message};
use mux_runtime::Value;

fn result_value(ptr: *mut Value) -> Value {
    unsafe {
        let value = (&*ptr).clone();
        assert!(mux_rc_dec(ptr));
        value
    }
}

fn unwrap_result(ptr: *mut Value) -> Value {
    match result_value(ptr) {
        Value::Result(Ok(value)) => *value,
        Value::Result(Err(error)) => panic!("unexpected error: {error}"),
        other => panic!("expected result, got {other:?}"),
    }
}

#[test]
fn typed_addresses_and_cidr_are_validated() {
    let ip_input = mux_rc_alloc(Value::String("2001:db8::1".to_string()));
    let ip = unwrap_result(unsafe { mux_net_ip_parse(ip_input) });
    let ip_ptr = mux_rc_alloc(ip);
    assert_eq!(
        result_value(unsafe { mux_net_ip_to_string(ip_ptr) }),
        Value::String("2001:db8::1".to_string())
    );
    assert!(unsafe { mux_net_ip_is_v6(ip_ptr) });
    assert!(!unsafe { mux_net_ip_is_v4(ip_ptr) });

    let cidr_input = mux_rc_alloc(Value::String("2001:db8::/32".to_string()));
    let cidr = unwrap_result(unsafe { mux_net_cidr_parse(cidr_input) });
    let cidr_ptr = mux_rc_alloc(cidr);
    assert!(unsafe { mux_net_cidr_contains(cidr_ptr, ip_ptr) });
    assert_eq!(unsafe { mux_net_cidr_prefix(cidr_ptr) }, 32);
    assert_eq!(
        result_value(unsafe { mux_net_cidr_to_string(cidr_ptr) }),
        Value::String("2001:db8::/32".to_string())
    );
    unsafe {
        assert!(mux_rc_dec(ip_input));
        assert!(mux_rc_dec(cidr_input));
        assert!(mux_rc_dec(ip_ptr));
        assert!(mux_rc_dec(cidr_ptr));
    }
}

#[test]
fn socket_addresses_reject_bad_ports_and_normalize_cidr_networks() {
    let socket_input = mux_rc_alloc(Value::String("127.0.0.1:80".to_string()));
    let socket = unwrap_result(unsafe { mux_net_socket_addr_parse(socket_input) });
    let socket_ptr = mux_rc_alloc(socket);
    assert_eq!(unsafe { mux_net_socket_port(socket_ptr) }, 80);
    let changed = unwrap_result(unsafe { mux_net_socket_with_port(socket_ptr, 443) });
    let changed_ptr = mux_rc_alloc(changed);
    assert_eq!(
        result_value(unsafe { mux_net_socket_to_string(changed_ptr) }),
        Value::String("127.0.0.1:443".to_string())
    );
    let invalid = unsafe { mux_net_socket_with_port(socket_ptr, 65_536) };
    assert!(matches!(unsafe { &*invalid }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(invalid));
        assert!(mux_rc_dec(socket_input));
        assert!(mux_rc_dec(socket_ptr));
        assert!(mux_rc_dec(changed_ptr));
    }

    let cidr_input = mux_rc_alloc(Value::String("10.20.30.40/16".to_string()));
    let cidr = unwrap_result(unsafe { mux_net_cidr_parse(cidr_input) });
    let cidr_ptr = mux_rc_alloc(cidr);
    assert_eq!(
        result_value(unsafe { mux_net_cidr_to_string(cidr_ptr) }),
        Value::String("10.20.0.0/16".to_string())
    );
    unsafe {
        assert!(mux_rc_dec(cidr_input));
        assert!(mux_rc_dec(cidr_ptr));
    }
}

#[test]
fn socket_address_resolution_returns_typed_addresses() {
    let host = mux_rc_alloc(Value::String("localhost".to_string()));
    let resolved = result_value(unsafe { mux_net_socket_addr_resolve(host, 80) });
    match resolved {
        Value::Result(Ok(values)) => match *values {
            Value::List(values) => {
                assert!(!values.is_empty());
                for value in &values {
                    let socket = mux_rc_alloc(value.clone());
                    let rendered = result_value(unsafe { mux_net_socket_to_string(socket) });
                    unsafe {
                        assert!(mux_rc_dec(socket));
                    }
                    assert!(matches!(rendered, Value::String(text) if text.ends_with(":80")));
                }
            }
            other => panic!("expected address list, got {other:?}"),
        },
        Value::Result(Err(error)) => panic!("unexpected resolution error: {error}"),
        other => panic!("expected result, got {other:?}"),
    }

    let empty_host = mux_rc_alloc(Value::String(String::new()));
    let empty_result = unsafe { mux_net_socket_addr_resolve(empty_host, 80) };
    assert!(matches!(unsafe { &*empty_result }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(host));
        assert!(mux_rc_dec(empty_host));
        assert!(mux_rc_dec(empty_result));
    }

    let invalid_port = mux_rc_alloc(Value::String("localhost".to_string()));
    let invalid_result = unsafe { mux_net_socket_addr_resolve(invalid_port, 65_536) };
    assert!(matches!(unsafe { &*invalid_result }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(invalid_port));
        assert!(mux_rc_dec(invalid_result));
    }
}

#[test]
fn endpoint_preserves_host_port_and_resolves() {
    let host = mux_rc_alloc(Value::String("localhost".to_string()));
    let endpoint = unwrap_result(unsafe { mux_net_endpoint_from_host(host, 8080) });
    let endpoint_ptr = mux_rc_alloc(endpoint);
    assert_eq!(
        result_value(unsafe { mux_net_endpoint_to_string(endpoint_ptr) }),
        Value::String("localhost:8080".to_string())
    );
    assert_eq!(
        result_value(unsafe { mux_net_endpoint_host(endpoint_ptr) }),
        Value::String("localhost".to_string())
    );
    assert_eq!(unsafe { mux_net_endpoint_port(endpoint_ptr) }, 8080);
    let resolved = unwrap_result(unsafe { mux_net_endpoint_resolve(endpoint_ptr) });
    assert!(matches!(resolved, Value::List(values) if !values.is_empty()));

    let invalid_host = mux_rc_alloc(Value::String(String::new()));
    let invalid = unsafe { mux_net_endpoint_from_host(invalid_host, 8080) };
    assert!(matches!(unsafe { &*invalid }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(host));
        assert!(mux_rc_dec(endpoint_ptr));
        assert!(mux_rc_dec(invalid_host));
        assert!(mux_rc_dec(invalid));
    }
}

#[test]
fn address_failures_preserve_typed_network_context() {
    let invalid = mux_rc_alloc(Value::String("not-an-ip".to_string()));
    let result = unsafe { mux_net_ip_parse(invalid) };
    let Value::Result(Err(error)) = (unsafe { &*result }) else {
        panic!("expected typed network error")
    };
    assert!(matches!(&**error, Value::Object(_)));
    let error = error.as_ref() as *const Value;
    let kind = unsafe { mux_net_error_kind(error) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes())
    });
    let detail = unsafe { mux_net_error_message(error) };
    assert!(unsafe { matches!(&*detail, Value::String(value) if value.contains("invalid IP")) });
    let address = unsafe { mux_net_error_address(error) };
    assert!(unsafe { matches!(&*address, Value::String(value) if value.is_empty()) });
    unsafe {
        assert!(mux_rc_dec(address));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(invalid));
    }
}
