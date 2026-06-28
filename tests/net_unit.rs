//! Unit tests for the networking layer over loopback (feature-gated behind
//! `net`). The HTTP client (`mux_net_http_request`) talks to external hosts and
//! is not exercised here.
//!
//! TCP uses connect-into-backlog: on loopback the kernel completes the handshake
//! into the listen queue, so `accept` returns immediately after `connect` without
//! needing a second thread (raw `*mut Value` handles are not `Send`).
#![cfg(feature = "net")]
#![allow(clippy::mutable_key_type)]

mod common;

use std::collections::BTreeMap;

use common::{assert_err, ok_string};
use mux_runtime::net::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::Value;

fn addr_val(s: &str) -> *mut Value {
    mux_rc_alloc(Value::String(s.to_string()))
}

fn bytes_val(bytes: &[u8]) -> *mut Value {
    mux_rc_alloc(Value::List(bytes.iter().map(|b| Value::Int(*b as i64)).collect()))
}

fn ok_data(r: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    assert!(!data.is_null());
    assert!(mux_rc_dec(r));
    data
}

#[test]
fn tcp_roundtrip() {
    let bind_addr = addr_val("127.0.0.1:0");
    let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
    assert!(mux_rc_dec(bind_addr));

    let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
    let connect_addr = addr_val(&addr);
    let client = ok_data(mux_net_tcp_connect(connect_addr));
    assert!(mux_rc_dec(connect_addr));

    let server = ok_data(mux_net_tcp_listener_accept(listener));

    let payload = bytes_val(b"hi");
    let written = mux_net_tcp_write(client, payload);
    assert!(mux_result_is_ok(written));
    assert!(mux_rc_dec(written));
    assert!(mux_rc_dec(payload));

    let read = mux_net_tcp_read(server, 2);
    assert!(mux_result_is_ok(read));
    assert!(mux_rc_dec(read));

    // address + option accessors
    let peer = mux_net_tcp_peer_addr(client);
    assert!(mux_result_is_ok(peer));
    assert!(mux_rc_dec(peer));
    let local = mux_net_tcp_local_addr(client);
    assert!(mux_result_is_ok(local));
    assert!(mux_rc_dec(local));
    let nb = mux_net_tcp_set_nonblocking(server, 1);
    assert!(mux_result_is_ok(nb));
    assert!(mux_rc_dec(nb));

    mux_net_tcp_close(client);
    mux_net_tcp_close(server);
    mux_net_tcp_listener_close(listener);
    assert!(mux_rc_dec(client));
    assert!(mux_rc_dec(server));
    assert!(mux_rc_dec(listener));
}

#[test]
fn udp_roundtrip() {
    let a_bind = addr_val("127.0.0.1:0");
    let b_bind = addr_val("127.0.0.1:0");
    let a = ok_data(mux_net_udp_bind(a_bind));
    let b = ok_data(mux_net_udp_bind(b_bind));
    assert!(mux_rc_dec(a_bind));
    assert!(mux_rc_dec(b_bind));

    let b_addr = ok_string(mux_net_udp_local_addr(b));
    let dest = addr_val(&b_addr);
    let payload = bytes_val(b"ping");

    let sent = mux_net_udp_send_to(a, payload, dest);
    assert!(mux_result_is_ok(sent));
    assert!(mux_rc_dec(sent));

    let recv = mux_net_udp_recv_from(b, 16);
    assert!(mux_result_is_ok(recv));
    assert!(mux_rc_dec(recv));

    assert!(mux_rc_dec(payload));
    assert!(mux_rc_dec(dest));
    mux_net_udp_close(a);
    mux_net_udp_close(b);
    assert!(mux_rc_dec(a));
    assert!(mux_rc_dec(b));
}

#[test]
fn invalid_addresses_error() {
    let bad = addr_val("definitely not an address");
    assert_err(mux_net_tcp_listener_bind(bad));
    assert!(mux_rc_dec(bad));

    let bad2 = addr_val("definitely not an address");
    assert_err(mux_net_udp_bind(bad2));
    assert!(mux_rc_dec(bad2));
}

#[test]
fn http_request_response_loopback() {
    let bind_addr = addr_val("127.0.0.1:0");
    let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
    assert!(mux_rc_dec(bind_addr));
    let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
    let connect_addr = addr_val(&addr);
    let client = ok_data(mux_net_tcp_connect(connect_addr));
    assert!(mux_rc_dec(connect_addr));
    let server = ok_data(mux_net_tcp_listener_accept(listener));

    // Client sends a complete HTTP request; server parses it.
    let req = bytes_val(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let written = mux_net_tcp_write(client, req);
    assert!(mux_result_is_ok(written));
    assert!(mux_rc_dec(written));
    assert!(mux_rc_dec(req));

    let parsed = mux_net_http_read_request(server);
    assert!(mux_result_is_ok(parsed));
    assert!(mux_rc_dec(parsed));

    // Server writes a JSON response.
    let mut resp = BTreeMap::new();
    resp.insert(Value::String("status".into()), Value::Int(200));
    let mut headers = BTreeMap::new();
    headers.insert(Value::String("X-Test".into()), Value::String("yes".into()));
    resp.insert(Value::String("headers".into()), Value::Map(headers));
    resp.insert(Value::String("body".into()), Value::String("hello".into()));
    let resp_val = mux_rc_alloc(Value::Map(resp));
    let wrote = mux_net_http_write_response(server, resp_val);
    assert!(mux_result_is_ok(wrote));
    assert!(mux_rc_dec(wrote));
    assert!(mux_rc_dec(resp_val));

    // A non-object response is an error.
    let bad = mux_rc_alloc(Value::Int(1));
    assert_err(mux_net_http_write_response(server, bad));
    assert!(mux_rc_dec(bad));

    mux_net_tcp_close(client);
    mux_net_tcp_close(server);
    mux_net_tcp_listener_close(listener);
    assert!(mux_rc_dec(client));
    assert!(mux_rc_dec(server));
    assert!(mux_rc_dec(listener));
}

#[test]
fn http_null_inputs_error() {
    assert_err(mux_net_http_read_request(std::ptr::null_mut()));
    assert_err(mux_net_http_request(std::ptr::null()));
}

#[test]
fn http_client_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A tiny canned-response server on its own thread (std sockets are Send).
    // It fully drains each request (headers + Content-Length body) before
    // responding, so the client never sees a reset mid-write under load.
    fn serve_one(listener: &TcpListener) {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut data = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let Ok(n) = stream.read(&mut buf) else { return };
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&data[..pos]).to_lowercase();
                let content_len = header
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while data.len() < pos + 4 + content_len {
                    let Ok(n) = stream.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                break;
            }
        }
        let body = b"{\"ok\":true}";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        serve_one(&listener); // GET
        serve_one(&listener); // POST
    });

    let url = format!("http://{}/", server_addr);

    // GET
    let mut get = BTreeMap::new();
    get.insert(Value::String("method".into()), Value::String("GET".into()));
    get.insert(Value::String("url".into()), Value::String(url.clone()));
    let get_val = mux_rc_alloc(Value::Map(get));
    let get_res = mux_net_http_request(get_val);
    assert!(mux_result_is_ok(get_res));
    assert!(mux_rc_dec(get_res));
    assert!(mux_rc_dec(get_val));

    // POST with a body (exercises send_string + Content-Type defaulting)
    let mut post = BTreeMap::new();
    post.insert(Value::String("method".into()), Value::String("POST".into()));
    post.insert(Value::String("url".into()), Value::String(url));
    post.insert(Value::String("body".into()), Value::String("payload".into()));
    let post_val = mux_rc_alloc(Value::Map(post));
    let post_res = mux_net_http_request(post_val);
    assert!(mux_result_is_ok(post_res));
    assert!(mux_rc_dec(post_res));
    assert!(mux_rc_dec(post_val));

    handle.join().unwrap();
}

#[test]
fn http_request_errors() {
    // missing required fields
    let mut no_url = BTreeMap::new();
    no_url.insert(Value::String("method".into()), Value::String("GET".into()));
    let no_url_val = mux_rc_alloc(Value::Map(no_url));
    assert_err(mux_net_http_request(no_url_val));
    assert!(mux_rc_dec(no_url_val));

    // transport failure: nothing is listening on port 1
    let mut refused = BTreeMap::new();
    refused.insert(Value::String("method".into()), Value::String("GET".into()));
    refused.insert(Value::String("url".into()), Value::String("http://127.0.0.1:1/".into()));
    let refused_val = mux_rc_alloc(Value::Map(refused));
    assert_err(mux_net_http_request(refused_val));
    assert!(mux_rc_dec(refused_val));
}

#[test]
fn http_response_validation_and_udp_extras() {
    let bind_addr = addr_val("127.0.0.1:0");
    let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
    assert!(mux_rc_dec(bind_addr));
    let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
    let connect_addr = addr_val(&addr);
    let client = ok_data(mux_net_tcp_connect(connect_addr));
    assert!(mux_rc_dec(connect_addr));
    let server = ok_data(mux_net_tcp_listener_accept(listener));

    // missing status
    let no_status = mux_rc_alloc(Value::Map(BTreeMap::new()));
    assert_err(mux_net_http_write_response(server, no_status));
    assert!(mux_rc_dec(no_status));

    // status out of range
    let mut bad = BTreeMap::new();
    bad.insert(Value::String("status".into()), Value::Int(99));
    let bad_val = mux_rc_alloc(Value::Map(bad));
    assert_err(mux_net_http_write_response(server, bad_val));
    assert!(mux_rc_dec(bad_val));

    // non-string header value
    let mut hdr = BTreeMap::new();
    hdr.insert(Value::String("status".into()), Value::Int(200));
    let mut headers = BTreeMap::new();
    headers.insert(Value::String("X".into()), Value::Int(1));
    hdr.insert(Value::String("headers".into()), Value::Map(headers));
    let hdr_val = mux_rc_alloc(Value::Map(hdr));
    assert_err(mux_net_http_write_response(server, hdr_val));
    assert!(mux_rc_dec(hdr_val));

    mux_net_tcp_close(client);
    mux_net_tcp_close(server);
    mux_net_tcp_listener_close(listener);
    assert!(mux_rc_dec(client));
    assert!(mux_rc_dec(server));
    assert!(mux_rc_dec(listener));

    // UDP extras: set_nonblocking ok, peer_addr on unconnected socket errors
    let a_bind = addr_val("127.0.0.1:0");
    let a = ok_data(mux_net_udp_bind(a_bind));
    assert!(mux_rc_dec(a_bind));
    let nb = mux_net_udp_set_nonblocking(a, 1);
    assert!(mux_result_is_ok(nb));
    assert!(mux_rc_dec(nb));
    assert_err(mux_net_udp_peer_addr(a));
    mux_net_udp_close(a);
    assert!(mux_rc_dec(a));
}

#[test]
fn http_read_request_body_without_content_length_errors() {
    let bind_addr = addr_val("127.0.0.1:0");
    let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
    assert!(mux_rc_dec(bind_addr));
    let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
    let connect_addr = addr_val(&addr);
    let client = ok_data(mux_net_tcp_connect(connect_addr));
    assert!(mux_rc_dec(connect_addr));
    let server = ok_data(mux_net_tcp_listener_accept(listener));

    // A request that carries a body but no Content-Length is rejected.
    let req = bytes_val(b"POST / HTTP/1.1\r\nHost: x\r\n\r\nBODYDATA");
    let written = mux_net_tcp_write(client, req);
    assert!(mux_result_is_ok(written));
    assert!(mux_rc_dec(written));
    assert!(mux_rc_dec(req));
    mux_net_tcp_close(client);
    assert!(mux_rc_dec(client));

    assert_err(mux_net_http_read_request(server));

    mux_net_tcp_close(server);
    mux_net_tcp_listener_close(listener);
    assert!(mux_rc_dec(server));
    assert!(mux_rc_dec(listener));
}

#[test]
fn invalid_read_size_errors() {
    let bind_addr = addr_val("127.0.0.1:0");
    let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
    assert!(mux_rc_dec(bind_addr));
    let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
    let connect_addr = addr_val(&addr);
    let client = ok_data(mux_net_tcp_connect(connect_addr));
    assert!(mux_rc_dec(connect_addr));

    assert_err(mux_net_tcp_read(client, 0));

    mux_net_tcp_close(client);
    mux_net_tcp_listener_close(listener);
    assert!(mux_rc_dec(client));
    assert!(mux_rc_dec(listener));
}
