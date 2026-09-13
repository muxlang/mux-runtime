#![cfg(feature = "tls")]

use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_data, mux_result_is_err};
use mux_runtime::std::{mux_tls_error_detail, mux_tls_error_kind};
use mux_runtime::tls::{
    mux_tls_accept, mux_tls_alpn_protocol, mux_tls_cipher_suite, mux_tls_config_new,
    mux_tls_config_set_alpn_protocols, mux_tls_config_set_cipher_suites,
    mux_tls_config_set_protocols, mux_tls_connect, mux_tls_connect_with_client_cert,
    mux_tls_connect_with_config, mux_tls_connect_with_roots, mux_tls_peer_certificates,
    mux_tls_protocol_version, mux_tls_read,
};
use mux_runtime::Value;
use std::net::{Shutdown, TcpListener};
use std::thread;

#[test]
fn tls_rejects_invalid_connection_inputs() {
    let server = mux_runtime::refcount::mux_rc_alloc(Value::String("example.com".to_string()));
    let address = mux_runtime::refcount::mux_rc_alloc(Value::String("not-an-address".to_string()));
    let result = unsafe { mux_tls_connect(server, address) };
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_tls_error_kind(error) };
    let detail = unsafe { mux_tls_error_detail(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes())
    );
    assert!(matches!(unsafe { &*detail }, Value::String(value) if value.contains("address")));
    unsafe {
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(address));
    }
}

#[test]
fn tls_custom_roots_require_non_empty_der_certificates() {
    let server = mux_runtime::refcount::mux_rc_alloc(Value::String("example.com".to_string()));
    let address = mux_runtime::refcount::mux_rc_alloc(Value::String("not-an-address".to_string()));
    let roots = mux_runtime::refcount::mux_rc_alloc(Value::List(vec![]));
    let result = unsafe { mux_tls_connect_with_roots(server, address, roots) };
    assert!(unsafe { mux_result_is_err(result) });
    unsafe {
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(address));
        assert!(mux_rc_dec(roots));
    }
}

#[test]
fn tls_server_requires_certificate_chain_and_key() {
    let not_stream = mux_runtime::refcount::mux_rc_alloc(Value::String("not-a-stream".to_string()));
    let certificates = mux_runtime::refcount::mux_rc_alloc(Value::List(vec![]));
    let key = mux_runtime::refcount::mux_rc_alloc(Value::Bytes(vec![1]));
    let result = unsafe { mux_tls_accept(not_stream, certificates, key) };
    assert!(unsafe { mux_result_is_err(result) });
    unsafe {
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(not_stream));
        assert!(mux_rc_dec(certificates));
        assert!(mux_rc_dec(key));
    }
}

#[test]
fn tls_client_certificate_requires_valid_inputs() {
    let server = mux_runtime::refcount::mux_rc_alloc(Value::String("example.com".to_string()));
    let address = mux_runtime::refcount::mux_rc_alloc(Value::String("not-an-address".to_string()));
    let roots = mux_runtime::refcount::mux_rc_alloc(Value::List(vec![]));
    let certificates = mux_runtime::refcount::mux_rc_alloc(Value::List(vec![]));
    let key = mux_runtime::refcount::mux_rc_alloc(Value::Bytes(vec![1]));
    let result =
        unsafe { mux_tls_connect_with_client_cert(server, address, roots, certificates, key) };
    assert!(unsafe { mux_result_is_err(result) });
    unsafe {
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(address));
        assert!(mux_rc_dec(roots));
        assert!(mux_rc_dec(certificates));
        assert!(mux_rc_dec(key));
    }
}

#[test]
fn tls_default_client_provider_handles_a_loopback_peer_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let address = listener.local_addr().expect("loopback address");
    let peer = thread::spawn(move || {
        let (socket, _) = listener.accept().expect("loopback connection");
        let _ = socket.shutdown(Shutdown::Both);
    });
    let server = mux_runtime::refcount::mux_rc_alloc(Value::String("localhost".to_string()));
    let address = mux_runtime::refcount::mux_rc_alloc(Value::String(address.to_string()));
    let connected = unsafe { mux_tls_connect(server, address) };
    assert!(!unsafe { mux_result_is_err(connected) });
    let stream = unsafe {
        match &*connected {
            Value::Result(inner) => {
                inner.as_ref().expect("connected stream").as_ref() as *const Value
            }
            _ => panic!("expected connection result"),
        }
    };
    let handshake = unsafe { mux_tls_read(stream, 1) };
    assert!(unsafe { mux_result_is_err(handshake) });
    unsafe {
        assert!(mux_rc_dec(handshake));
        assert!(mux_rc_dec(connected));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(address));
    }
    peer.join().expect("loopback peer");
}

#[test]
fn tls_inspection_rejects_invalid_stream_handles() {
    let invalid = mux_runtime::refcount::mux_rc_alloc(Value::String("not-a-stream".to_string()));
    for result in [
        unsafe { mux_tls_peer_certificates(invalid) },
        unsafe { mux_tls_protocol_version(invalid) },
        unsafe { mux_tls_cipher_suite(invalid) },
        unsafe { mux_tls_alpn_protocol(invalid) },
    ] {
        assert!(unsafe { mux_result_is_err(result) });
        unsafe {
            assert!(mux_rc_dec(result));
        }
    }
    unsafe {
        assert!(mux_rc_dec(invalid));
    }
}

#[test]
fn tls_policy_validates_and_accepts_supported_shapes() {
    let config = mux_tls_config_new();
    assert!(!unsafe { mux_result_is_err(config) });
    let config_value = unsafe {
        match &*config {
            Value::Result(inner) => inner.as_ref().expect("config value").as_ref() as *const Value,
            _ => panic!("expected result"),
        }
    };
    let minimum = mux_runtime::refcount::mux_rc_alloc(Value::String("TLS1.3".to_string()));
    let maximum = mux_runtime::refcount::mux_rc_alloc(Value::String("TLS1.2".to_string()));
    let invalid = mux_tls_config_set_protocols(config_value, minimum, maximum);
    assert!(unsafe { mux_result_is_err(invalid) });
    unsafe {
        assert!(mux_rc_dec(minimum));
        assert!(mux_rc_dec(maximum));
        assert!(mux_rc_dec(invalid));
    }
    let minimum = mux_runtime::refcount::mux_rc_alloc(Value::String("TLS1.2".to_string()));
    let maximum = mux_runtime::refcount::mux_rc_alloc(Value::String("TLS1.3".to_string()));
    let set = mux_tls_config_set_protocols(config_value, minimum, maximum);
    assert!(!unsafe { mux_result_is_err(set) });
    let suites = mux_runtime::refcount::mux_rc_alloc(Value::List(vec![Value::String(
        "not-a-suite".to_string(),
    )]));
    let set_suites = mux_tls_config_set_cipher_suites(config_value, suites);
    assert!(!unsafe { mux_result_is_err(set_suites) });
    let server = mux_runtime::refcount::mux_rc_alloc(Value::String("example.com".to_string()));
    let address = mux_runtime::refcount::mux_rc_alloc(Value::String("not-an-address".to_string()));
    let rejected = mux_tls_connect_with_config(server, address, config_value);
    assert!(unsafe { mux_result_is_err(rejected) });
    let alpn = mux_runtime::refcount::mux_rc_alloc(Value::List(vec![Value::Bytes(b"h2".to_vec())]));
    let set_alpn = mux_tls_config_set_alpn_protocols(config_value, alpn);
    assert!(!unsafe { mux_result_is_err(set_alpn) });
    unsafe {
        assert!(mux_rc_dec(set));
        assert!(mux_rc_dec(set_suites));
        assert!(mux_rc_dec(rejected));
        assert!(mux_rc_dec(set_alpn));
        assert!(mux_rc_dec(minimum));
        assert!(mux_rc_dec(maximum));
        assert!(mux_rc_dec(suites));
        assert!(mux_rc_dec(alpn));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(address));
        assert!(mux_rc_dec(config));
    }
}
