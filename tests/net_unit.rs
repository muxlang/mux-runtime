//! Unit tests for the networking layer over loopback (feature-gated behind
//! `net`). HTTP client tests use the typed request/response handles; external
//! network access is not required by the unit suite.
//!
//! TCP uses connect-into-backlog: on loopback the kernel completes the handshake
//! into the listen queue, so `accept` returns immediately after `connect` without
//! needing a second thread (raw `*mut Value` handles are not `Send`).
#![cfg(feature = "net")]
#![allow(clippy::mutable_key_type)]

mod common;

use common::{assert_err, assert_ok, ok_string};
use mux_runtime::net::*;
use mux_runtime::poller::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::mux_net_error_address;
use mux_runtime::stream::{
    mux_io_reader_from_bytes, mux_io_reader_from_tcp, mux_io_reader_read, mux_io_writer_from_tcp,
    mux_io_writer_write,
};
use mux_runtime::Value;
use std::ffi::c_void;

#[cfg(feature = "http3")]
use mux_runtime::net::{select_http_protocol, Http3ErrorKind, HttpProtocol};

fn addr_val(s: &str) -> *mut Value {
    mux_rc_alloc(Value::String(s.to_string()))
}

#[cfg(feature = "http3")]
#[test]
fn http3_selection_prefers_quic_and_rejects_unknown_protocols() {
    assert_eq!(
        select_http_protocol(&[b"h3", b"h2", b"http/1.1"]),
        Ok(HttpProtocol::Http3)
    );
    assert_eq!(
        select_http_protocol(&[b"h3", b"http/1.1"]),
        Ok(HttpProtocol::Http3)
    );
    let error = select_http_protocol(&[b"spdy/3"]).expect_err("unknown protocols must fail");
    assert_eq!(error.kind(), Http3ErrorKind::Unsupported);
}

#[cfg(feature = "http3")]
mod http3_loopback_conformance {
    use super::{
        Http3ClientTransport, Http3Error, Http3ErrorKind, Http3Response, Http3ServerTransport,
    };
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use bytes::Bytes;
    use http::{HeaderValue, Request, Response};
    use std::net::SocketAddr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    const LOCAL_CERTIFICATE: &str = include_str!("fixtures/http3_localhost_cert.der.b64");
    const LOCAL_PRIVATE_KEY: &str = include_str!("fixtures/http3_localhost_key.der.b64");
    const MAX_HTTP3_BODY_BYTES: usize = 16 * 1024 * 1024;

    fn local_credentials() -> (Vec<u8>, Vec<u8>) {
        let certificate = STANDARD
            .decode(LOCAL_CERTIFICATE.trim())
            .expect("HTTP/3 test certificate fixture must be base64");
        let private_key = STANDARD
            .decode(LOCAL_PRIVATE_KEY.trim())
            .expect("HTTP/3 test key fixture must be base64");
        (certificate, private_key)
    }

    fn local_server() -> (Http3ServerTransport, Vec<u8>) {
        let (certificate, private_key) = local_credentials();
        let server = Http3ServerTransport::bind(
            "127.0.0.1:0",
            std::slice::from_ref(&certificate),
            &private_key,
        )
        .expect("local HTTP/3 server should bind");
        (server, certificate)
    }

    fn request(uri: &str, method: &str) -> Request<()> {
        Request::builder()
            .method(method)
            .uri(uri)
            .version(http::Version::HTTP_3)
            .body(())
            .expect("HTTP/3 test request should be valid")
    }

    #[test]
    fn http3_loopback_frames_request_response_and_qpack_headers() {
        let (server, certificate) = local_server();
        let authority = server.local_addr().to_string();
        let (request_seen, request_received) = mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            server.serve_one(move |incoming| {
                assert_eq!(incoming.request.method(), "POST");
                assert_eq!(
                    incoming.request.uri().path_and_query().unwrap(),
                    "/qpack?mode=1"
                );
                let repeated = incoming
                    .request
                    .headers()
                    .get_all("x-qpack-repeat")
                    .iter()
                    .map(|value| value.to_str().expect("header value should be UTF-8"))
                    .collect::<Vec<_>>();
                assert_eq!(repeated, ["one", "two"]);
                assert_eq!(incoming.body, b"request body");
                request_seen
                    .send(())
                    .expect("request observation receiver should remain open");
                Ok(Http3Response {
                    status: 207,
                    headers: vec![
                        ("x-qpack-response".to_string(), "compressed".to_string()),
                        ("x-qpack-response".to_string(), "retained".to_string()),
                    ],
                    body: b"response body".to_vec(),
                })
            })
        });

        let client = Http3ClientTransport::connect_with_trusted_roots(
            &authority,
            std::slice::from_ref(&certificate),
        )
        .expect("HTTP/3 client should complete a verified local QUIC handshake");
        let mut request = request(&format!("https://{authority}/qpack?mode=1"), "POST");
        request
            .headers_mut()
            .append("x-qpack-repeat", HeaderValue::from_static("one"));
        request
            .headers_mut()
            .append("x-qpack-repeat", HeaderValue::from_static("two"));
        request
            .headers_mut()
            .insert("x-qpack-static", HeaderValue::from_static("header-value"));
        let response = client
            .send(
                request,
                Some(b"request body".to_vec()),
                Some(Duration::from_secs(5)),
            )
            .expect("HTTP/3 request/response should complete");

        request_received
            .recv_timeout(Duration::from_secs(1))
            .expect("server should decode the request headers and body");
        server_thread
            .join()
            .expect("HTTP/3 server thread should not panic")
            .expect("HTTP/3 server should frame the response");
        assert_eq!(response.status, 207);
        assert_eq!(response.body, b"response body");
        assert_eq!(
            response
                .headers
                .iter()
                .filter(|(name, _)| name == "x-qpack-response")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            ["compressed", "retained"]
        );
    }

    #[test]
    fn http3_loopback_rejects_oversized_request_body() {
        let (server, certificate) = local_server();
        let authority = server.local_addr().to_string();
        let server_thread = thread::spawn(move || {
            server.serve_one(move |_incoming| {
                panic!("an oversized request must be rejected before the handler runs")
            })
        });
        let client = Http3ClientTransport::connect_with_trusted_roots(
            &authority,
            std::slice::from_ref(&certificate),
        )
        .expect("HTTP/3 client should connect to the local server");
        let result = client.send(
            request(&format!("https://{authority}/too-large"), "POST"),
            Some(vec![0x5a; MAX_HTTP3_BODY_BYTES + 1]),
            Some(Duration::from_secs(60)),
        );
        let server_result = server_thread
            .join()
            .expect("HTTP/3 server thread should not panic");
        assert_eq!(
            server_result
                .expect_err("server must reject an oversized request")
                .kind(),
            Http3ErrorKind::BodyTooLarge
        );
        assert!(
            result.is_err(),
            "the cancelled request must not return a response"
        );
    }

    #[test]
    fn http3_loopback_cancels_inflight_stream() {
        let (server, certificate) = local_server();
        let authority = server.local_addr().to_string();
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            server.serve_one_with_timeout(Duration::from_secs(15), move |_incoming| {
                started_sender
                    .send(())
                    .expect("cancellation test handler should start");
                thread::sleep(Duration::from_secs(11));
                Ok(Http3Response {
                    status: 200,
                    headers: Vec::new(),
                    body: b"late response".to_vec(),
                })
            })
        });
        let client = Http3ClientTransport::connect_with_trusted_roots(
            &authority,
            std::slice::from_ref(&certificate),
        )
        .expect("HTTP/3 client should connect to the local server");
        let result_thread = thread::spawn(move || {
            client.send(
                request(&format!("https://{authority}/cancel"), "GET"),
                None,
                Some(Duration::from_secs(10)),
            )
        });
        started_receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the server must have observed the request before cancellation");
        let result = result_thread
            .join()
            .expect("HTTP/3 cancellation client thread should not panic");
        assert_eq!(
            result
                .expect_err("the response deadline must cancel the stream")
                .kind(),
            Http3ErrorKind::Timeout
        );
        let server_result = server_thread
            .join()
            .expect("HTTP/3 server thread should not panic");
        assert!(
            server_result.is_ok()
                || matches!(
                    server_result.as_ref().map_err(Http3Error::kind),
                    Err(Http3ErrorKind::Transport | Http3ErrorKind::Protocol)
                ),
            "server completion should either finish the queued response or observe cancellation: {server_result:?}"
        );
    }

    #[derive(Clone, Copy)]
    enum RawResponse {
        InvalidUtf8Header,
        OversizedBody,
    }

    fn spawn_raw_h3_server(
        certificate: Vec<u8>,
        private_key: Vec<u8>,
        response: RawResponse,
    ) -> (
        String,
        tokio::sync::oneshot::Sender<()>,
        thread::JoinHandle<Result<(), String>>,
    ) {
        let (ready_sender, ready_receiver) = mpsc::sync_channel::<Result<String, String>>(1);
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        let worker =
            thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("test runtime setup failed: {error}"))?;
                runtime.block_on(async move {
                    use quinn::crypto::rustls::QuicServerConfig;
                    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
                    use std::sync::Arc;

                    let certificates = vec![CertificateDer::from(certificate.clone())];
                    let key = PrivateKeyDer::try_from(private_key)
                        .map_err(|error| format!("test key setup failed: {error}"))?;
                    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .with_protocol_versions(&[&rustls::version::TLS13])
                    .map_err(|error| format!("test TLS setup failed: {error}"))?
                    .with_no_client_auth()
                    .with_single_cert(certificates, key)
                    .map_err(|error| format!("test certificate setup failed: {error}"))?;
                    tls.alpn_protocols = vec![b"h3".to_vec()];
                    let crypto = QuicServerConfig::try_from(tls)
                        .map_err(|error| format!("test QUIC setup failed: {error}"))?;
                    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
                    let transport = Arc::get_mut(&mut server_config.transport)
                        .ok_or_else(|| "test QUIC transport was unexpectedly shared".to_string())?;
                    transport.max_concurrent_uni_streams(16_u8.into());
                    let endpoint = quinn::Endpoint::server(
                        server_config,
                        "127.0.0.1:0"
                            .parse::<SocketAddr>()
                            .map_err(|error| format!("test address setup failed: {error}"))?,
                    )
                    .map_err(|error| format!("test endpoint setup failed: {error}"))?;
                    let authority = endpoint
                        .local_addr()
                        .map_err(|error| format!("test local address failed: {error}"))?
                        .to_string();
                    ready_sender
                        .send(Ok(authority))
                        .map_err(|_| "test caller stopped before endpoint setup".to_string())?;
                    let incoming = endpoint
                        .accept()
                        .await
                        .ok_or_else(|| "test endpoint closed before a connection".to_string())?;
                    let connection = incoming
                        .await
                        .map_err(|error| format!("test QUIC handshake failed: {error}"))?;
                    let quic = h3_quinn::Connection::new(connection);
                    let mut connection = h3::server::builder()
                        .max_field_section_size(64_u64 * 1024)
                        .build::<_, Bytes>(quic)
                        .await
                        .map_err(|error| format!("test HTTP/3 handshake failed: {error}"))?;
                    let resolver = connection
                        .accept()
                        .await
                        .map_err(|error| format!("test request accept failed: {error}"))?
                        .ok_or_else(|| "test connection closed before a request".to_string())?;
                    let (_request, mut stream) = resolver
                        .resolve_request()
                        .await
                        .map_err(|error| format!("test request resolution failed: {error}"))?;
                    match response {
                        RawResponse::InvalidUtf8Header => {
                            let mut response = Response::builder()
                                .status(200)
                                .body(())
                                .map_err(|error| format!("test response setup failed: {error}"))?;
                            response.headers_mut().insert(
                                "x-invalid-utf8",
                                HeaderValue::from_bytes(&[0xff]).map_err(|error| {
                                    format!("test header setup failed: {error}")
                                })?,
                            );
                            stream.send_response(response).await.map_err(|error| {
                                format!("test response headers failed: {error}")
                            })?;
                            stream.finish().await.map_err(|error| {
                                format!("test response completion failed: {error}")
                            })?;
                        }
                        RawResponse::OversizedBody => {
                            stream
                                .send_response(Response::builder().status(200).body(()).map_err(
                                    |error| format!("test response setup failed: {error}"),
                                )?)
                                .await
                                .map_err(|error| {
                                    format!("test response headers failed: {error}")
                                })?;
                            stream
                                .send_data(Bytes::from(vec![0x41; MAX_HTTP3_BODY_BYTES + 1]))
                                .await
                                .map_err(|error| format!("test oversized body failed: {error}"))?;
                            stream.finish().await.map_err(|error| {
                                format!("test response completion failed: {error}")
                            })?;
                        }
                    }
                    release_receiver
                        .await
                        .map_err(|_| "test caller stopped before response delivery".to_string())?;
                    Ok(())
                })
            });
        let authority = ready_receiver
            .recv()
            .expect("raw HTTP/3 server should report its address")
            .expect("raw HTTP/3 server should initialize");
        (authority, release_sender, worker)
    }

    #[test]
    fn http3_loopback_maps_peer_header_protocol_errors() {
        let (certificate, private_key) = local_credentials();
        let (authority, release_sender, server_thread) = spawn_raw_h3_server(
            certificate.clone(),
            private_key,
            RawResponse::InvalidUtf8Header,
        );
        let client = Http3ClientTransport::connect_with_trusted_roots(
            &authority,
            std::slice::from_ref(&certificate),
        )
        .expect("HTTP/3 client should connect to the local peer");
        let result = client.send(
            request(&format!("https://{authority}/invalid-header"), "GET"),
            None,
            Some(Duration::from_secs(5)),
        );
        release_sender
            .send(())
            .expect("raw HTTP/3 server should still be waiting for release");
        let error = result.expect_err("invalid response header bytes must be rejected");
        assert_eq!(error.kind(), Http3ErrorKind::Protocol);
        server_thread
            .join()
            .expect("raw HTTP/3 server thread should not panic")
            .expect("raw HTTP/3 server should deliver invalid headers");
    }

    #[test]
    fn http3_loopback_cancels_peer_after_oversized_response_body() {
        let (certificate, private_key) = local_credentials();
        let (authority, release_sender, server_thread) =
            spawn_raw_h3_server(certificate.clone(), private_key, RawResponse::OversizedBody);
        let client = Http3ClientTransport::connect_with_trusted_roots(
            &authority,
            std::slice::from_ref(&certificate),
        )
        .expect("HTTP/3 client should connect to the local peer");
        let result = client.send(
            request(&format!("https://{authority}/too-large"), "GET"),
            None,
            Some(Duration::from_secs(60)),
        );
        release_sender
            .send(())
            .expect("raw HTTP/3 server should still be waiting for release");
        let error = result.expect_err("oversized response bodies must be rejected");
        assert_eq!(error.kind(), Http3ErrorKind::BodyTooLarge);
        server_thread
            .join()
            .expect("raw HTTP/3 server thread should not panic")
            .expect("raw HTTP/3 server should deliver oversized body");
    }
}

fn bytes_val(bytes: &[u8]) -> *mut Value {
    mux_rc_alloc(Value::Bytes(bytes.to_vec()))
}

#[repr(C)]
struct TestHttpCallback {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
    capture_count: i64,
    boxed_function_ptr: *mut c_void,
}

fn callback_ok() -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(Value::Unit))))
}

extern "C" fn worker_pool_http_callback(_request: *mut Value) -> *mut Value {
    unsafe {
        let headers = mux_net_http_headers_new();
        let body = bytes_val(b"worker-pool");
        let response = mux_net_http_response_from_config(200, headers, body);
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(body));
        response
    }
}

extern "C" fn sse_connection_callback(_request: *mut Value, stream: *mut Value) -> *mut Value {
    unsafe {
        let event_name = mux_rc_alloc(Value::String("message".to_string()));
        let event_id = mux_rc_alloc(Value::String(String::new()));
        let event_data_value = mux_rc_alloc(Value::String("hello".to_string()));
        let event_result = mux_net_sse_event_from_config(event_name, event_id, 0, event_data_value);
        assert!(mux_result_is_ok(event_result));
        let event = mux_result_data(event_result);
        let sent = mux_net_sse_stream_send(stream, event);
        let flushed = mux_net_sse_stream_flush(stream);
        assert!(mux_result_is_ok(sent));
        assert!(mux_result_is_ok(flushed));
        assert!(mux_rc_dec(flushed));
        assert!(mux_rc_dec(sent));
        assert!(mux_rc_dec(event));
        assert!(mux_rc_dec(event_result));
        assert!(mux_rc_dec(event_name));
        assert!(mux_rc_dec(event_id));
        assert!(mux_rc_dec(event_data_value));
        callback_ok()
    }
}

extern "C" fn websocket_connection_callback(
    _request: *mut Value,
    session: *mut Value,
) -> *mut Value {
    unsafe {
        let received = mux_net_websocket_session_receive(session);
        assert!(mux_result_is_ok(received));
        let message = mux_result_data(received);
        let payload = mux_net_websocket_frame_payload(message);
        assert!(matches!(&*payload, Value::Bytes(bytes) if bytes == b"hello"));
        assert!(mux_rc_dec(payload));
        assert!(mux_rc_dec(message));
        assert!(mux_rc_dec(received));

        let payload = bytes_val(b"world");
        let frame = mux_net_websocket_frame_from_config(1, 1, payload, 0);
        assert!(mux_result_is_ok(frame));
        let frame_data = mux_result_data(frame);
        let sent = mux_net_websocket_session_send(session, frame_data);
        assert!(mux_result_is_ok(sent));
        assert!(mux_rc_dec(sent));
        assert!(mux_rc_dec(frame_data));
        assert!(mux_rc_dec(frame));
        assert!(mux_rc_dec(payload));
        callback_ok()
    }
}

#[test]
fn socket_failures_preserve_explicit_address_context() {
    let address = addr_val("not-an-address");
    unsafe {
        let result = mux_net_tcp_listener_bind(address);
        let Value::Result(Err(error)) = &*result else {
            panic!("expected listener bind to fail")
        };
        let address_field = mux_net_error_address((&**error) as *const Value);
        assert!(matches!(&*address_field, Value::String(value) if value == "not-an-address"));
        assert!(mux_rc_dec(address_field));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(address));
    }
}

#[test]
fn structured_http_error_preserves_category_context_and_display() {
    unsafe {
        let detail = mux_rc_alloc(Value::String("connection timed out".to_string()));
        let error = mux_http_error_from_message(detail);
        assert!(matches!(&*error, Value::Object(_)));

        let kind = mux_http_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes()));
        let status = mux_http_error_status(error);
        assert!(matches!(&*status, Value::Int(value) if *value == 0));
        let message = mux_http_error_message(error);
        assert!(matches!(&*message, Value::String(value) if value == "connection timed out"));
        let display = mux_http_error_to_string(error);
        assert!(
            matches!(&*display, Value::String(value) if value == "transport: connection timed out")
        );

        assert!(mux_rc_dec(display));
        assert!(mux_rc_dec(message));
        assert!(mux_rc_dec(status));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(detail));
    }
}

#[test]
fn oauth_oidc_boundary_validates_metadata_and_rejects_unverified_tokens() {
    unsafe {
        let router = mux_net_http_router_new();
        let issuer = addr_val("http://issuer.example");
        let audience = addr_val("mux-client");
        let jwks_url = addr_val("https://issuer.example/keys");
        let invalid = mux_net_http_router_oauth_oidc(router, issuer, audience, jwks_url);
        assert!(!mux_result_is_ok(invalid));
        let invalid_error = mux_result_data(invalid);
        let invalid_kind = mux_http_error_kind(invalid_error);
        assert!(
            matches!(&*invalid_kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes())
        );
        assert!(mux_rc_dec(invalid_kind));
        assert!(mux_rc_dec(invalid_error));
        assert!(mux_rc_dec(invalid));
        assert!(mux_rc_dec(jwks_url));
        assert!(mux_rc_dec(audience));
        assert!(mux_rc_dec(issuer));

        let issuer = addr_val("https://issuer.example");
        let audience = addr_val("mux-client");
        let jwks_url = addr_val("https://issuer.example/keys");
        let configured = mux_net_http_router_oauth_oidc(router, issuer, audience, jwks_url);
        assert!(mux_result_is_ok(configured));
        assert!(mux_rc_dec(configured));
        assert!(mux_rc_dec(jwks_url));
        assert!(mux_rc_dec(audience));
        assert!(mux_rc_dec(issuer));

        let headers = mux_net_http_headers_new();
        let method = addr_val("GET");
        let url = addr_val("/resource?scope=read");
        let body = bytes_val(&[]);
        let request = ok_data(mux_net_http_request_from_config(method, url, headers, body));
        assert!(mux_rc_dec(body));
        assert!(mux_rc_dec(url));
        assert!(mux_rc_dec(method));
        assert!(mux_rc_dec(headers));

        let missing = mux_net_http_router_handle(router, request);
        assert!(mux_result_is_ok(missing));
        let missing_response = mux_result_data(missing);
        let missing_status = ok_data(mux_net_http_response_status_value(missing_response));
        assert!(matches!(&*missing_status, Value::Int(401)));
        assert!(mux_rc_dec(missing_status));
        assert!(mux_rc_dec(missing_response));
        assert!(mux_rc_dec(missing));

        let headers = mux_net_http_request_headers_field(request);
        let name = addr_val("Authorization");
        let malformed_value = addr_val("Bearer opaque-token extra");
        let malformed_set = mux_net_http_headers_set(headers, name, malformed_value);
        assert!(mux_result_is_ok(malformed_set));
        assert!(mux_rc_dec(malformed_set));
        assert!(mux_rc_dec(malformed_value));
        let malformed = mux_net_http_router_handle(router, request);
        assert!(mux_result_is_ok(malformed));
        let malformed_response = mux_result_data(malformed);
        let malformed_status = ok_data(mux_net_http_response_status_value(malformed_response));
        assert!(matches!(&*malformed_status, Value::Int(401)));
        assert!(mux_rc_dec(malformed_status));
        assert!(mux_rc_dec(malformed_response));
        assert!(mux_rc_dec(malformed));

        let value = addr_val("Bearer opaque-token");
        let set = mux_net_http_headers_set(headers, name, value);
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        assert!(mux_rc_dec(value));
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(headers));

        let rejected = mux_net_http_router_handle(router, request);
        assert!(mux_result_is_ok(rejected));
        let rejected_response = mux_result_data(rejected);
        let rejected_status = ok_data(mux_net_http_response_status_value(rejected_response));
        assert!(matches!(&*rejected_status, Value::Int(401)));
        assert!(mux_rc_dec(rejected_status));
        assert!(mux_rc_dec(rejected_response));
        assert!(mux_rc_dec(rejected));
        assert!(mux_rc_dec(request));
        assert!(mux_rc_dec(router));
    }
}

#[test]
fn typed_http_headers_are_case_insensitive_and_duplicate_preserving() {
    unsafe {
        let headers = mux_net_http_headers_new();
        assert!(!headers.is_null());
        let name = mux_rc_alloc(Value::String("Set-Cookie".to_string()));
        let first = mux_rc_alloc(Value::String("a=1".to_string()));
        let second = mux_rc_alloc(Value::String("b=2".to_string()));
        let set = mux_net_http_headers_set(headers, name, first);
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        let append = mux_net_http_headers_append(headers, name, second);
        assert!(mux_result_is_ok(append));
        assert!(mux_rc_dec(append));
        let lookup_name = mux_rc_alloc(Value::String("set-cookie".to_string()));
        let values = mux_net_http_headers_values(headers, lookup_name);
        assert!(mux_result_is_ok(values));
        let values_data = mux_result_data(values);
        assert!(
            matches!(&*values_data, Value::List(values) if values == &vec![Value::String("a=1".into()), Value::String("b=2".into())])
        );
        assert!(mux_rc_dec(values_data));
        assert!(mux_rc_dec(values));
        assert!(mux_rc_dec(lookup_name));
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(second));
        assert!(mux_rc_dec(headers));
    }
}

#[test]
fn sse_event_encoding_preserves_fields_and_multiline_data() {
    unsafe {
        let event = mux_rc_alloc(Value::String("message".to_string()));
        let id = mux_rc_alloc(Value::String("42".to_string()));
        let data = mux_rc_alloc(Value::String("first\nsecond".to_string()));
        let sse = ok_data(mux_net_sse_event_from_config(event, id, 1500, data));
        let encoded = ok_data(mux_net_sse_event_encode(sse));
        assert!(
            matches!(&*encoded, Value::Bytes(value) if value == b"event: message\nid: 42\nretry: 1500\ndata: first\ndata: second\n\n")
        );

        assert!(mux_rc_dec(encoded));
        assert!(mux_rc_dec(sse));
        assert!(mux_rc_dec(data));
        assert!(mux_rc_dec(id));
        assert!(mux_rc_dec(event));

        let invalid_event = mux_rc_alloc(Value::String("bad\nevent".to_string()));
        let empty_id = mux_rc_alloc(Value::String(String::new()));
        let empty_data = mux_rc_alloc(Value::String(String::new()));
        let rejected = mux_net_sse_event_from_config(invalid_event, empty_id, 0, empty_data);
        assert_err(rejected);
        assert!(mux_rc_dec(empty_data));
        assert!(mux_rc_dec(empty_id));
        assert!(mux_rc_dec(invalid_event));
    }
}

#[test]
fn websocket_frame_codec_round_trips_masked_binary_and_rejects_invalid_frames() {
    unsafe {
        let payload = bytes_val(&[0, 1, 2, 253, 254, 255]);
        let frame = mux_net_websocket_frame_from_config(1, 2, payload, 1);
        assert!(mux_result_is_ok(frame));
        let frame_data = mux_result_data(frame);
        let encoded = mux_net_websocket_frame_encode(frame_data);
        assert!(mux_result_is_ok(encoded));
        let wire = mux_result_data(encoded);
        let Value::Bytes(wire_bytes) = &*wire else {
            panic!("expected encoded bytes");
        };
        assert!(wire_bytes.len() >= 12);
        assert_ne!(
            wire_bytes[1] & 0x80,
            0,
            "masked frame must include mask bit"
        );

        let decoded_result = mux_net_websocket_frame_decode(wire);
        assert!(mux_result_is_ok(decoded_result));
        let decoded = mux_result_data(decoded_result);
        let decoded_payload = mux_net_websocket_frame_payload(decoded);
        assert!(
            matches!(&*decoded_payload, Value::Bytes(bytes) if bytes == &[0, 1, 2, 253, 254, 255])
        );
        let decoded_opcode = mux_net_websocket_frame_opcode(decoded);
        assert!(matches!(&*decoded_opcode, Value::Int(2)));

        assert!(mux_rc_dec(decoded_opcode));
        assert!(mux_rc_dec(decoded_payload));
        assert!(mux_rc_dec(decoded));
        assert!(mux_rc_dec(decoded_result));
        assert!(mux_rc_dec(wire));
        assert!(mux_rc_dec(encoded));
        assert!(mux_rc_dec(frame_data));
        assert!(mux_rc_dec(frame));
        assert!(mux_rc_dec(payload));

        let invalid = bytes_val(&[0x81, 0x7e, 0, 1, b'x']);
        assert_err(mux_net_websocket_frame_decode(invalid));
        assert!(mux_rc_dec(invalid));
    }
}

fn websocket_frame(fin: bool, opcode: i64, payload: &[u8], masked: bool) -> *mut Value {
    unsafe {
        let payload = bytes_val(payload);
        let result =
            mux_net_websocket_frame_from_config(i32::from(fin), opcode, payload, i32::from(masked));
        let frame = ok_data(result);
        assert!(mux_rc_dec(payload));
        frame
    }
}

fn websocket_frame_list(frames: &[*mut Value]) -> *mut Value {
    unsafe {
        mux_rc_alloc(Value::List(
            frames.iter().map(|frame| (&**frame).clone()).collect(),
        ))
    }
}

#[test]
fn websocket_frame_reassembles_bounded_fragments_and_rejects_bad_sequences() {
    unsafe {
        let first = websocket_frame(false, 1, b"hel", true);
        let ping = websocket_frame(true, 9, b"?", true);
        let last = websocket_frame(true, 0, b"lo", true);
        let frames = websocket_frame_list(&[first, ping, last]);
        let result = mux_net_websocket_frame_reassemble(frames);
        assert!(mux_result_is_ok(result));
        let message = mux_result_data(result);
        let fin = mux_net_websocket_frame_fin(message);
        assert!(matches!(&*fin, Value::Bool(true)));
        let opcode = mux_net_websocket_frame_opcode(message);
        assert!(matches!(&*opcode, Value::Int(1)));
        let payload = mux_net_websocket_frame_payload(message);
        assert!(matches!(&*payload, Value::Bytes(value) if value == b"hello"));
        let masked = mux_net_websocket_frame_masked(message);
        assert!(matches!(&*masked, Value::Bool(true)));
        assert!(mux_rc_dec(masked));
        assert!(mux_rc_dec(payload));
        assert!(mux_rc_dec(opcode));
        assert!(mux_rc_dec(fin));
        assert!(mux_rc_dec(message));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(frames));
        assert!(mux_rc_dec(last));
        assert!(mux_rc_dec(ping));
        assert!(mux_rc_dec(first));

        let incomplete_first = websocket_frame(false, 2, b"a", false);
        let incomplete_last = websocket_frame(false, 0, b"b", false);
        let incomplete = websocket_frame_list(&[incomplete_first, incomplete_last]);
        assert_err(mux_net_websocket_frame_reassemble(incomplete));
        assert!(mux_rc_dec(incomplete));
        assert!(mux_rc_dec(incomplete_last));
        assert!(mux_rc_dec(incomplete_first));

        let data_first = websocket_frame(false, 1, b"a", false);
        let new_data = websocket_frame(false, 2, b"b", false);
        let invalid = websocket_frame_list(&[data_first, new_data]);
        assert_err(mux_net_websocket_frame_reassemble(invalid));
        assert!(mux_rc_dec(invalid));
        assert!(mux_rc_dec(new_data));
        assert!(mux_rc_dec(data_first));

        let mixed_first = websocket_frame(false, 1, b"a", false);
        let mixed_last = websocket_frame(true, 0, b"b", true);
        let mixed = websocket_frame_list(&[mixed_first, mixed_last]);
        assert_err(mux_net_websocket_frame_reassemble(mixed));
        assert!(mux_rc_dec(mixed));
        assert!(mux_rc_dec(mixed_last));
        assert!(mux_rc_dec(mixed_first));

        let large_payload = vec![b'x'; 16 * 1024 * 1024];
        let bounded_first = websocket_frame(false, 2, &large_payload, false);
        let oversized_last = websocket_frame(true, 0, b"x", false);
        let oversized = websocket_frame_list(&[bounded_first, oversized_last]);
        assert_err(mux_net_websocket_frame_reassemble(oversized));
        assert!(mux_rc_dec(oversized));
        assert!(mux_rc_dec(oversized_last));
        assert!(mux_rc_dec(bounded_first));

        let utf8_first = websocket_frame(false, 1, &[0xc3], false);
        let utf8_last = websocket_frame(true, 0, &[0xa9], false);
        let utf8_frames = websocket_frame_list(&[utf8_first, utf8_last]);
        let utf8_result = mux_net_websocket_frame_reassemble(utf8_frames);
        assert!(mux_result_is_ok(utf8_result));
        let utf8_message = mux_result_data(utf8_result);
        let utf8_payload = mux_net_websocket_frame_payload(utf8_message);
        assert!(matches!(&*utf8_payload, Value::Bytes(value) if value == &[0xc3, 0xa9]));
        assert!(mux_rc_dec(utf8_payload));
        assert!(mux_rc_dec(utf8_message));
        assert!(mux_rc_dec(utf8_result));
        assert!(mux_rc_dec(utf8_frames));
        assert!(mux_rc_dec(utf8_last));
        assert!(mux_rc_dec(utf8_first));
    }
}

#[test]
fn websocket_handshake_matches_rfc6455_example() {
    unsafe {
        let key = mux_rc_alloc(Value::String("dGhlIHNhbXBsZSBub25jZQ==".to_string()));
        let protocol = mux_rc_alloc(Value::String("chat".to_string()));
        let handshake = mux_net_websocket_handshake_from_config(key, protocol);
        assert!(mux_result_is_ok(handshake));
        let handshake_data = mux_result_data(handshake);
        let accept = mux_net_websocket_handshake_accept_key(handshake_data);
        let accept_data = mux_result_data(accept);
        assert!(
            matches!(&*accept_data, Value::String(value) if value == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        // The header constructor is useful for composing the HTTP 101 response.
        let response_headers = mux_net_websocket_handshake_response_headers(handshake_data);
        assert!(mux_result_is_ok(response_headers));

        assert!(mux_rc_dec(response_headers));
        assert!(mux_rc_dec(accept_data));
        assert!(mux_rc_dec(accept));
        assert!(mux_rc_dec(handshake_data));
        assert!(mux_rc_dec(handshake));
        assert!(mux_rc_dec(protocol));
        assert!(mux_rc_dec(key));
    }
}

#[test]
fn synchronous_sse_server_hands_callback_a_live_stream() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

    unsafe {
        let bind = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind));
        assert!(mux_rc_dec(bind));
        let address = ok_string(mux_net_tcp_listener_local_addr(listener));
        let config = mux_net_http_server_config_new();
        let callback = TestHttpCallback {
            function_ptr: std::ptr::null_mut(),
            captures_ptr: std::ptr::null_mut(),
            capture_count: 0,
            boxed_function_ptr: sse_connection_callback as *const () as *mut c_void,
        };
        let listener_address = listener as usize;
        let config_address = config as usize;
        let callback_address = &callback as *const TestHttpCallback as usize;
        let server = thread::spawn(move || {
            mux_net_http_server_serve_sse(
                listener_address as *mut Value,
                config_address as *mut Value,
                callback_address as *mut c_void,
                1,
            ) as usize
        });

        let mut client = TcpStream::connect(address).expect("connect SSE server");
        client
            .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("write SSE request");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .expect("read SSE response");
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.windows(4).any(|window| window == b"\r\n\r\n"));
        assert!(!response
            .windows(15)
            .any(|window| { window.eq_ignore_ascii_case(b"content-length:") }));
        assert!(response
            .windows(b"event: message\ndata: hello\n\n".len())
            .any(|window| window == b"event: message\ndata: hello\n\n"));
        let result = server.join().expect("SSE server thread") as *mut Value;
        assert!(mux_result_is_ok(result));
        assert!(mux_rc_dec(result));
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(listener));
        assert!(mux_rc_dec(config));
    }
}

#[test]
fn synchronous_websocket_server_runs_session_callback_and_closes_cleanly() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

    unsafe {
        let bind = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind));
        assert!(mux_rc_dec(bind));
        let address = ok_string(mux_net_tcp_listener_local_addr(listener));
        let config = mux_net_http_server_config_new();
        let callback = TestHttpCallback {
            function_ptr: std::ptr::null_mut(),
            captures_ptr: std::ptr::null_mut(),
            capture_count: 0,
            boxed_function_ptr: websocket_connection_callback as *const () as *mut c_void,
        };
        let listener_address = listener as usize;
        let config_address = config as usize;
        let callback_address = &callback as *const TestHttpCallback as usize;
        let server = thread::spawn(move || {
            mux_net_http_server_serve_websocket(
                listener_address as *mut Value,
                config_address as *mut Value,
                callback_address as *mut c_void,
                1,
            ) as usize
        });

        let mut client = TcpStream::connect(address).expect("connect WebSocket server");
        client
            .write_all(
                b"GET /socket HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .expect("write WebSocket handshake");
        let mut headers = Vec::new();
        let mut byte = [0_u8; 1];
        while !headers.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).expect("read handshake");
            headers.push(byte[0]);
        }
        let header_text = String::from_utf8(headers).expect("handshake headers are UTF-8");
        assert!(header_text.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(header_text
            .to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="));

        let mask = [1_u8, 2, 3, 4];
        let mut frame = vec![0x81_u8, 0x85, mask[0], mask[1], mask[2], mask[3]];
        frame.extend(
            b"hello"
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        client
            .write_all(&frame)
            .expect("write masked WebSocket frame");

        let mut response_header = [0_u8; 2];
        client
            .read_exact(&mut response_header)
            .expect("read WebSocket response frame");
        assert_eq!(response_header, [0x81, 5]);
        let mut response_payload = [0_u8; 5];
        client
            .read_exact(&mut response_payload)
            .expect("read WebSocket response payload");
        assert_eq!(&response_payload, b"world");
        let mut close = [0_u8; 2];
        client.read_exact(&mut close).expect("read close frame");
        assert_eq!(close, [0x88, 0]);

        let result = server.join().expect("WebSocket server thread") as *mut Value;
        assert!(mux_result_is_ok(result));
        assert!(mux_rc_dec(result));
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(listener));
        assert!(mux_rc_dec(config));
    }
}

#[test]
fn typed_http_request_builder_owns_url_headers_and_body() {
    unsafe {
        let url = mux_rc_alloc(Value::String("http://127.0.0.1:1/".to_string()));

        let request = mux_net_http_request_new();

        let default_method = mux_net_http_request_method(request);
        assert!(matches!(&*default_method, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(default_method));
        let default_url = mux_net_http_request_url(request);
        assert!(matches!(&*default_url, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(default_url));
        let default_proxy = mux_net_http_request_proxy(request);
        assert!(matches!(&*default_proxy, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(default_proxy));
        let default_body = mux_net_http_request_body(request);
        assert!(matches!(&*default_body, Value::Bytes(value) if value.is_empty()));
        assert!(mux_rc_dec(default_body));
        let default_headers = mux_net_http_request_headers_field(request);
        assert!(matches!(&*default_headers, Value::Object(_)));
        assert!(mux_rc_dec(default_headers));
        let default_connect_timeout = mux_net_http_request_connect_timeout(request);
        assert!(matches!(&*default_connect_timeout, Value::Int(10_000)));
        assert!(mux_rc_dec(default_connect_timeout));
        let default_timeout = mux_net_http_request_timeout(request);
        assert!(matches!(&*default_timeout, Value::Int(30_000)));
        assert!(mux_rc_dec(default_timeout));
        let default_redirects = mux_net_http_request_max_redirects(request);
        assert!(matches!(&*default_redirects, Value::Int(10)));
        assert!(mux_rc_dec(default_redirects));
        let default_retries = mux_net_http_request_retries(request);
        assert!(matches!(&*default_retries, Value::Int(0)));
        assert!(mux_rc_dec(default_retries));

        let missing_url = mux_net_http_request_send(request);
        assert!(!mux_result_is_ok(missing_url));
        assert!(mux_rc_dec(missing_url));
        let method = mux_rc_alloc(Value::String("post".to_string()));
        let method_set = mux_net_http_request_set_method_field(request, method);
        assert!(mux_result_is_ok(method_set));
        assert!(mux_rc_dec(method_set));
        assert!(mux_rc_dec(method));
        let url_set = mux_net_http_request_set_url_field(request, url);
        assert!(mux_result_is_ok(url_set));
        assert!(mux_rc_dec(url_set));
        assert!(mux_rc_dec(url));

        let proxy = mux_rc_alloc(Value::String("http://127.0.0.1:9".to_string()));
        let proxy_set = mux_net_http_request_set_proxy_field(request, proxy);
        assert!(mux_result_is_ok(proxy_set));
        assert!(mux_rc_dec(proxy_set));
        let proxy_value = mux_net_http_request_proxy(request);
        assert!(matches!(&*proxy_value, Value::String(value) if value == "http://127.0.0.1:9"));
        assert!(mux_rc_dec(proxy_value));
        assert!(mux_rc_dec(proxy));
        let clear_proxy = mux_rc_alloc(Value::String(String::new()));
        let clear_result = mux_net_http_request_set_proxy_field(request, clear_proxy);
        assert!(mux_result_is_ok(clear_result));
        assert!(mux_rc_dec(clear_result));
        let cleared_proxy = mux_net_http_request_proxy(request);
        assert!(matches!(&*cleared_proxy, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(cleared_proxy));
        assert!(mux_rc_dec(clear_proxy));

        let timeout = mux_rc_alloc(Value::Int(5_000));
        let timeout_set = mux_net_http_request_set_timeout_field(request, timeout);
        assert!(mux_result_is_ok(timeout_set));
        assert!(mux_rc_dec(timeout_set));
        assert!(mux_rc_dec(timeout));
        let retries = mux_rc_alloc(Value::Int(2));
        let retries_set = mux_net_http_request_set_retries_field(request, retries);
        assert!(mux_result_is_ok(retries_set));
        assert!(mux_rc_dec(retries_set));
        assert!(mux_rc_dec(retries));
        let invalid_redirects = mux_rc_alloc(Value::Int(101));
        let invalid_set = mux_net_http_request_set_max_redirects_field(request, invalid_redirects);
        assert!(!mux_result_is_ok(invalid_set));
        assert!(mux_rc_dec(invalid_set));
        assert!(mux_rc_dec(invalid_redirects));

        let headers = mux_net_http_headers_new();
        assert!(!headers.is_null());
        let name = mux_rc_alloc(Value::String("Content-Type".to_string()));
        let value = mux_rc_alloc(Value::String("application/octet-stream".to_string()));
        let set = mux_net_http_headers_set(headers, name, value);
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        let body = bytes_val(&[0, 1, 255]);
        let headers_set = mux_net_http_request_set_headers_field(request, headers);
        assert!(mux_result_is_ok(headers_set));
        assert!(mux_rc_dec(headers_set));
        let body_set = mux_net_http_request_set_body_field(request, body);
        assert!(mux_result_is_ok(body_set));
        assert!(mux_rc_dec(body_set));
        assert!(mux_rc_dec(body));

        // The mutable field setter enforces the same buffered-body bound as
        // the explicit setter and constructor, before changing the request.
        let oversized = bytes_val(&vec![0_u8; 16 * 1024 * 1024 + 1]);
        assert_err(mux_net_http_request_set_body_field(request, oversized));
        assert!(mux_rc_dec(oversized));
        let retained_body = mux_net_http_request_body(request);
        assert!(matches!(&*retained_body, Value::Bytes(bytes) if bytes == &[0, 1, 255]));
        assert!(mux_rc_dec(retained_body));
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(value));
        assert!(mux_rc_dec(headers));

        let reader_input = bytes_val(b"streamed");
        let reader = mux_io_reader_from_bytes(reader_input);
        let reader_body = mux_net_http_request_set_body_reader(request, reader);
        assert!(mux_result_is_ok(reader_body));
        assert!(mux_rc_dec(reader_body));
        let body_value = mux_net_http_request_body(request);
        // A reader-backed request keeps the ordinary byte field empty and
        // streams the retained reader during send; it is not materialized by
        // the field accessor.
        assert!(matches!(&*body_value, Value::Bytes(bytes) if bytes.is_empty()));
        assert!(mux_rc_dec(body_value));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(reader_input));

        // Port 1 is intentionally unreachable in the test environment; this
        // verifies the typed send path returns a transport error, not a panic.
        let sent = mux_net_http_request_send(request);
        assert!(!mux_result_is_ok(sent));
        assert!(mux_rc_dec(sent));
        assert!(mux_rc_dec(request));
    }
}

#[test]
fn request_id_field_round_trips_and_rejects_header_injection() {
    unsafe {
        let request = mux_net_http_request_new();
        let initial = mux_net_http_request_id(request);
        assert!(matches!(&*initial, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(initial));

        let request_id = mux_rc_alloc(Value::String("trace-123".to_string()));
        let set = mux_net_http_request_set_id_field(request, request_id);
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        let current = mux_net_http_request_id(request);
        assert!(matches!(&*current, Value::String(value) if value == "trace-123"));
        assert!(mux_rc_dec(current));
        assert!(mux_rc_dec(request_id));

        let invalid = mux_rc_alloc(Value::String("bad\r\nid".to_string()));
        let set = mux_net_http_request_set_id_field(request, invalid);
        assert!(!mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        assert!(mux_rc_dec(invalid));
        assert!(mux_rc_dec(request));
    }
}

#[test]
fn server_access_log_defaults_off_and_is_mutable() {
    unsafe {
        let config = mux_net_http_server_config_new();
        let default_value = mux_net_http_server_config_access_log(config);
        assert!(matches!(&*default_value, Value::Bool(false)));
        assert!(mux_rc_dec(default_value));

        let enabled = mux_rc_alloc(Value::Bool(true));
        let set = mux_net_http_server_config_set_access_log(config, enabled);
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        let current = mux_net_http_server_config_access_log(config);
        assert!(matches!(&*current, Value::Bool(true)));
        assert!(mux_rc_dec(current));
        assert!(mux_rc_dec(enabled));

        let origins = mux_rc_alloc(Value::List(vec![Value::String(
            "https://example.test".to_string(),
        )]));
        let set_origins = mux_net_http_server_config_set_cors_origins(config, origins);
        assert!(mux_result_is_ok(set_origins));
        assert!(mux_rc_dec(set_origins));
        let current_origins = mux_net_http_server_config_cors_origins(config);
        assert!(
            matches!(&*current_origins, Value::List(values) if values == &vec![Value::String("https://example.test".to_string())])
        );
        assert!(mux_rc_dec(current_origins));
        assert!(mux_rc_dec(origins));

        let credentials = mux_rc_alloc(Value::Bool(true));
        let set_credentials =
            mux_net_http_server_config_set_cors_allow_credentials(config, credentials);
        assert!(mux_result_is_ok(set_credentials));
        assert!(mux_rc_dec(set_credentials));
        let current_credentials = mux_net_http_server_config_cors_allow_credentials(config);
        assert!(matches!(&*current_credentials, Value::Bool(true)));
        assert!(mux_rc_dec(current_credentials));
        assert!(mux_rc_dec(credentials));

        let wildcard = mux_rc_alloc(Value::List(vec![Value::String("*".to_string())]));
        let rejected = mux_net_http_server_config_set_cors_origins(config, wildcard);
        assert!(!mux_result_is_ok(rejected));
        assert!(mux_rc_dec(rejected));
        assert!(mux_rc_dec(wildcard));

        let root = mux_rc_alloc(Value::String("/srv/mux-static".to_string()));
        let set_root = mux_net_http_server_config_set_static_root(config, root);
        assert!(mux_result_is_ok(set_root));
        assert!(mux_rc_dec(set_root));
        let current_root = mux_net_http_server_config_static_root(config);
        assert!(matches!(&*current_root, Value::String(value) if value == "/srv/mux-static"));
        assert!(mux_rc_dec(current_root));
        assert!(mux_rc_dec(root));
        assert!(mux_rc_dec(config));
    }
}

#[test]
fn server_worker_count_is_typed_and_bounded() {
    unsafe {
        let config = mux_net_http_server_config_new();
        let default_value = mux_net_http_server_config_worker_count(config);
        assert!(matches!(&*default_value, Value::Int(value) if *value == 1));
        assert!(mux_rc_dec(default_value));

        let workers = mux_rc_alloc(Value::Int(4));
        let set = mux_net_http_server_config_set_worker_count(config, workers);
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));
        let current = mux_net_http_server_config_worker_count(config);
        assert!(matches!(&*current, Value::Int(value) if *value == 4));
        assert!(mux_rc_dec(current));
        assert!(mux_rc_dec(workers));

        for invalid in [0, 257] {
            let value = mux_rc_alloc(Value::Int(invalid));
            assert_err(mux_net_http_server_config_set_worker_count(config, value));
            assert!(mux_rc_dec(value));
        }
        let not_an_int = mux_rc_alloc(Value::Bool(true));
        assert_err(mux_net_http_server_config_set_worker_count(
            config, not_an_int,
        ));
        assert!(mux_rc_dec(not_an_int));
        assert!(mux_rc_dec(config));
    }
}

#[test]
fn http_server_pool_keeps_sockets_out_of_worker_jobs() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

    unsafe {
        let bind = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind));
        assert!(mux_rc_dec(bind));
        let address = ok_string(mux_net_tcp_listener_local_addr(listener));
        let config = mux_net_http_server_config_new();
        let workers = mux_rc_alloc(Value::Int(2));
        assert_ok(mux_net_http_server_config_set_worker_count(config, workers));
        assert!(mux_rc_dec(workers));
        let callback = TestHttpCallback {
            function_ptr: std::ptr::null_mut(),
            captures_ptr: std::ptr::null_mut(),
            capture_count: 0,
            boxed_function_ptr: worker_pool_http_callback as *const () as *mut c_void,
        };
        let listener_address = listener as usize;
        let config_address = config as usize;
        let callback_address = &callback as *const TestHttpCallback as usize;
        let server = thread::spawn(move || {
            mux_net_http_server_serve(
                listener_address as *mut Value,
                config_address as *mut Value,
                callback_address as *mut c_void,
                2,
            ) as usize
        });

        let mut clients = Vec::new();
        for _ in 0..2 {
            let mut client = TcpStream::connect(&address).expect("connect worker pool");
            client
                .write_all(b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("write worker pool request");
            clients.push(client);
        }
        for mut client in clients {
            let mut response = Vec::new();
            client
                .read_to_end(&mut response)
                .expect("read worker pool response");
            assert!(response
                .windows(b"worker-pool".len())
                .any(|part| part == b"worker-pool"));
        }
        let result = server.join().expect("worker pool server thread");
        assert!(mux_result_is_ok(result as *mut Value));
        assert!(mux_rc_dec(result as *mut Value));
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(listener));
        assert!(mux_rc_dec(config));
    }
}

#[test]
fn typed_http_request_from_config_accepts_explicit_fields() {
    unsafe {
        let method = mux_rc_alloc(Value::String("post".to_string()));
        let url = mux_rc_alloc(Value::String("http://127.0.0.1:1/".to_string()));
        let headers = mux_net_http_headers_new();
        let body = bytes_val(&[1, 2, 3]);

        let request_result = mux_net_http_request_from_config(method, url, headers, body);
        let request = ok_data(request_result);
        let method_value = mux_net_http_request_method(request);
        assert!(matches!(&*method_value, Value::String(value) if value == "POST"));
        assert!(mux_rc_dec(method_value));
        let url_value = mux_net_http_request_url(request);
        assert!(matches!(&*url_value, Value::String(value) if value == "http://127.0.0.1:1/"));
        assert!(mux_rc_dec(url_value));
        let body_value = mux_net_http_request_body(request);
        assert!(matches!(&*body_value, Value::Bytes(value) if value == &[1, 2, 3]));
        assert!(mux_rc_dec(body_value));

        assert!(mux_rc_dec(request));
        assert!(mux_rc_dec(method));
        assert!(mux_rc_dec(url));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(body));
    }
}

#[test]
fn typed_http_response_constructors_preserve_fields() {
    unsafe {
        let default = mux_net_http_response_new();
        let status = ok_data(mux_net_http_response_status_value(default));
        assert!(matches!(&*status, Value::Int(200)));
        assert!(mux_rc_dec(status));
        assert!(mux_rc_dec(default));

        let status = 201_i64;
        let headers = mux_net_http_headers_new();
        let body = bytes_val(b"created");
        let response = ok_data(mux_net_http_response_from_config(status, headers, body));
        let actual_status = ok_data(mux_net_http_response_status_value(response));
        assert!(matches!(&*actual_status, Value::Int(201)));
        assert!(mux_rc_dec(actual_status));
        let actual_body = ok_data(mux_net_http_response_read_bytes(response, 64));
        assert!(matches!(&*actual_body, Value::Bytes(bytes) if bytes == b"created"));
        assert!(mux_rc_dec(actual_body));
        assert!(mux_rc_dec(response));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(body));

        let headers = mux_net_http_headers_new();
        let body = bytes_val(b"stream");
        let response = ok_data(mux_net_http_response_from_config(200, headers, body));
        let reader = ok_data(mux_net_http_response_reader(response, 64));
        let streamed = ok_data(mux_io_reader_read(reader, 64));
        assert!(matches!(&*streamed, Value::Bytes(bytes) if bytes == b"stream"));
        assert!(mux_rc_dec(streamed));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(response));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(body));
    }
}

#[test]
fn typed_http_response_fields_are_mutable_for_server_construction() {
    unsafe {
        let response = mux_net_http_response_new();
        let body = mux_net_http_response_body_field(response);
        assert!(matches!(&*body, Value::Bytes(bytes) if bytes.is_empty()));
        assert!(mux_rc_dec(body));

        let body_value = bytes_val(b"server body");
        let set_body = mux_net_http_response_set_body_field(response, body_value);
        assert!(mux_result_is_ok(set_body));
        assert!(mux_rc_dec(set_body));
        let body_field = mux_net_http_response_body_field(response);
        assert!(matches!(&*body_field, Value::Bytes(bytes) if bytes == b"server body"));
        assert!(mux_rc_dec(body_field));
        let consumed = ok_data(mux_net_http_response_read_bytes(response, 6));
        assert!(matches!(&*consumed, Value::Bytes(bytes) if bytes == b"server"));
        assert!(mux_rc_dec(consumed));
        let remaining = mux_net_http_response_body_field(response);
        assert!(matches!(&*remaining, Value::Bytes(bytes) if bytes == b" body"));
        assert!(mux_rc_dec(remaining));

        assert!(mux_rc_dec(body_value));
        assert!(mux_rc_dec(response));
    }
}

#[test]
fn http_header_mutation_rejects_oversized_collections_before_mutation() {
    unsafe {
        let headers = mux_net_http_headers_new();
        for index in 0..128 {
            let name = mux_rc_alloc(Value::String(format!("x-{index}")));
            let value = mux_rc_alloc(Value::String("value".to_string()));
            assert_ok(mux_net_http_headers_append(headers, name, value));
            assert!(mux_rc_dec(name));
            assert!(mux_rc_dec(value));
        }
        let rejected_name = mux_rc_alloc(Value::String("x-128".to_string()));
        let rejected_value = mux_rc_alloc(Value::String("value".to_string()));
        assert_err(mux_net_http_headers_append(
            headers,
            rejected_name,
            rejected_value,
        ));
        assert!(mux_rc_dec(rejected_name));
        assert!(mux_rc_dec(rejected_value));

        let rejected_name = mux_rc_alloc(Value::String("x-large".to_string()));
        let rejected_value = mux_rc_alloc(Value::String("a".repeat(64 * 1024)));
        assert_err(mux_net_http_headers_set(
            headers,
            rejected_name,
            rejected_value,
        ));
        assert!(mux_rc_dec(rejected_name));
        assert!(mux_rc_dec(rejected_value));

        let request = mux_net_http_request_new();
        assert_ok(mux_net_http_request_set_headers_field(request, headers));
        let request_headers = mux_net_http_request_headers_field(request);
        let request_header_name = mux_rc_alloc(Value::String("x-0".to_string()));
        let request_lookup = mux_net_http_headers_get(request_headers, request_header_name);
        assert!(mux_result_is_ok(request_lookup));
        let request_lookup_data = mux_result_data(request_lookup);
        assert!(matches!(&*request_lookup_data, Value::Optional(Some(_))));
        assert!(mux_rc_dec(request_lookup_data));
        assert!(mux_rc_dec(request_lookup));
        assert!(mux_rc_dec(request_header_name));
        assert!(mux_rc_dec(request_headers));
        assert!(mux_rc_dec(request));

        let response = mux_net_http_response_new();
        assert_ok(mux_net_http_response_set_headers_field(response, headers));
        let response_headers = mux_net_http_response_headers_field(response);
        let response_header_name = mux_rc_alloc(Value::String("x-0".to_string()));
        let response_lookup = mux_net_http_headers_get(response_headers, response_header_name);
        assert!(mux_result_is_ok(response_lookup));
        let response_lookup_data = mux_result_data(response_lookup);
        assert!(matches!(&*response_lookup_data, Value::Optional(Some(_))));
        assert!(mux_rc_dec(response_lookup_data));
        assert!(mux_rc_dec(response_lookup));
        assert!(mux_rc_dec(response_header_name));
        assert!(mux_rc_dec(response_headers));
        assert!(mux_rc_dec(response));
        assert!(mux_rc_dec(headers));
    }
}

#[test]
fn typed_http_server_roundtrip_uses_request_and_response_handles() {
    unsafe {
        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));
        let address = ok_string(mux_net_tcp_listener_local_addr(listener));
        let connect_addr = addr_val(&address);
        let client = ok_data(mux_net_tcp_connect(connect_addr));
        assert!(mux_rc_dec(connect_addr));
        let server = ok_data(mux_net_tcp_listener_accept(listener));

        let raw = bytes_val(b"POST /upload HTTP/1.1\r\nHost: localhost\r\nX-Trace: a\r\nX-Trace: b\r\nContent-Length: 3\r\n\r\nabc");
        let written = mux_net_tcp_write(client, raw);
        assert!(mux_result_is_ok(written));
        assert!(mux_rc_dec(written));
        assert!(mux_rc_dec(raw));
        let request = ok_data(mux_net_http_request_read(server));
        let method = mux_net_http_request_method(request);
        assert!(matches!(&*method, Value::String(value) if value == "POST"));
        assert!(mux_rc_dec(method));
        let url = mux_net_http_request_url(request);
        assert!(matches!(&*url, Value::String(value) if value == "/upload"));
        assert!(mux_rc_dec(url));
        let body = mux_net_http_request_body(request);
        assert!(matches!(&*body, Value::Bytes(value) if value == b"abc"));
        assert!(mux_rc_dec(body));
        let headers = mux_net_http_request_headers_field(request);
        let trace = mux_rc_alloc(Value::String("x-trace".to_string()));
        let values = mux_net_http_headers_values(headers, trace);
        assert!(mux_result_is_ok(values));
        let values_data = mux_result_data(values);
        assert!(
            matches!(&*values_data, Value::List(values) if values == &vec![Value::String("a".into()), Value::String("b".into())])
        );
        assert!(mux_rc_dec(values_data));
        assert!(mux_rc_dec(values));
        assert!(mux_rc_dec(trace));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(request));

        let status = 201_i64;
        let response_headers = mux_net_http_headers_new();
        let body = bytes_val(b"accepted");
        let response = ok_data(mux_net_http_response_from_config(
            status,
            response_headers,
            body,
        ));
        let written_response = mux_net_http_response_write(server, response);
        assert!(mux_result_is_ok(written_response));
        assert!(mux_rc_dec(written_response));
        let wire = ok_data(mux_net_tcp_read(client, 1024));
        assert!(
            matches!(&*wire, Value::Bytes(value) if value.windows(11).any(|part| part == b"201 Created"))
        );
        assert!(mux_rc_dec(wire));
        assert!(mux_rc_dec(response));
        assert!(mux_rc_dec(response_headers));
        assert!(mux_rc_dec(body));
        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

fn ok_data(r: *mut Value) -> *mut Value {
    let data = unsafe {
        if !mux_result_is_ok(r) {
            eprintln!("network operation failed: {:?}", mux_result_data(r));
        }
        assert!(mux_result_is_ok(r), "expected Ok result");
        mux_result_data(r)
    };
    assert!(!data.is_null());
    assert!(unsafe { mux_rc_dec(r) });
    data
}

#[test]
fn tcp_roundtrip() {
    unsafe {
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
        let read_data = mux_result_data(read);
        assert!(matches!(
            &*read_data,
            Value::Bytes(bytes) if bytes.as_slice() == b"hi"
        ));
        assert!(mux_rc_dec(read_data));
        assert!(mux_rc_dec(read));

        // std.io adapters can consume and produce bytes on cloned TCP streams.
        let adapted_input = bytes_val(b"read");
        let adapted_input_written = mux_net_tcp_write(client, adapted_input);
        assert!(mux_result_is_ok(adapted_input_written));
        assert!(mux_rc_dec(adapted_input_written));
        assert!(mux_rc_dec(adapted_input));
        let reader = ok_data(mux_io_reader_from_tcp(server));
        let adapted_read = mux_io_reader_read(reader, 4);
        assert!(mux_result_is_ok(adapted_read));
        let adapted_read_data = mux_result_data(adapted_read);
        assert!(matches!(&*adapted_read_data, Value::Bytes(bytes) if bytes == b"read"));
        assert!(mux_rc_dec(adapted_read_data));
        assert!(mux_rc_dec(adapted_read));
        assert!(mux_rc_dec(reader));
        let writer = ok_data(mux_io_writer_from_tcp(server));
        let adapted_payload = bytes_val(b"ok");
        let adapted_written = mux_io_writer_write(writer, adapted_payload);
        assert!(mux_result_is_ok(adapted_written));
        assert!(mux_rc_dec(adapted_written));
        assert!(mux_rc_dec(adapted_payload));
        let adapted_wire = ok_data(mux_net_tcp_read(client, 2));
        assert!(matches!(&*adapted_wire, Value::Bytes(bytes) if bytes == b"ok"));
        assert!(mux_rc_dec(adapted_wire));
        assert!(mux_rc_dec(writer));

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
        for timeout in [
            mux_net_tcp_set_read_timeout(client, 100),
            mux_net_tcp_set_write_timeout(client, 100),
            mux_net_tcp_set_read_timeout(client, 0),
            mux_net_tcp_set_write_timeout(client, 0),
        ] {
            assert!(mux_result_is_ok(timeout));
            assert!(mux_rc_dec(timeout));
        }
        let nodelay_set = mux_net_tcp_set_nodelay(client, 1);
        assert!(mux_result_is_ok(nodelay_set));
        assert!(mux_rc_dec(nodelay_set));
        let nodelay = mux_net_tcp_nodelay(client);
        assert!(mux_result_is_ok(nodelay));
        let nodelay_data = mux_result_data(nodelay);
        assert!(matches!(&*nodelay_data, Value::Bool(true)));
        assert!(mux_rc_dec(nodelay_data));
        assert!(mux_rc_dec(nodelay));
        let keepalive_set = mux_net_tcp_set_keepalive(client, 1);
        assert!(mux_result_is_ok(keepalive_set));
        assert!(mux_rc_dec(keepalive_set));
        let keepalive = mux_net_tcp_keepalive(client);
        assert!(mux_result_is_ok(keepalive));
        let keepalive_data = mux_result_data(keepalive);
        assert!(matches!(&*keepalive_data, Value::Bool(true)));
        assert!(mux_rc_dec(keepalive_data));
        assert!(mux_rc_dec(keepalive));
        for update in [
            mux_net_tcp_set_recv_buffer_size(client, 4096),
            mux_net_tcp_set_send_buffer_size(client, 4096),
        ] {
            assert!(mux_result_is_ok(update));
            assert!(mux_rc_dec(update));
        }
        let recv_buffer = mux_net_tcp_recv_buffer_size(client);
        assert!(mux_result_is_ok(recv_buffer));
        let recv_buffer_data = mux_result_data(recv_buffer);
        assert!(matches!(&*recv_buffer_data, Value::Int(size) if *size >= 4096));
        assert!(mux_rc_dec(recv_buffer_data));
        assert!(mux_rc_dec(recv_buffer));
        let send_buffer = mux_net_tcp_send_buffer_size(client);
        assert!(mux_result_is_ok(send_buffer));
        let send_buffer_data = mux_result_data(send_buffer);
        assert!(matches!(&*send_buffer_data, Value::Int(size) if *size >= 4096));
        assert!(mux_rc_dec(send_buffer_data));
        assert!(mux_rc_dec(send_buffer));
        let ttl_set = mux_net_tcp_set_ttl(client, 64);
        assert!(mux_result_is_ok(ttl_set));
        assert!(mux_rc_dec(ttl_set));
        let ttl = mux_net_tcp_ttl(client);
        assert!(mux_result_is_ok(ttl));
        let ttl_data = mux_result_data(ttl);
        assert!(matches!(&*ttl_data, Value::Int(64)));
        assert!(mux_rc_dec(ttl_data));
        assert!(mux_rc_dec(ttl));
        let shutdown_write = mux_net_tcp_shutdown_write(client);
        assert!(mux_result_is_ok(shutdown_write));
        assert!(mux_rc_dec(shutdown_write));
        let shutdown_read = mux_net_tcp_shutdown_read(server);
        assert!(mux_result_is_ok(shutdown_read));
        assert!(mux_rc_dec(shutdown_read));

        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

#[cfg(unix)]
#[test]
fn local_stream_roundtrip_uses_unix_socket_and_cleans_path() {
    use std::time::{SystemTime, UNIX_EPOCH};

    unsafe {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mux-local-{suffix}.sock"));
        let path_text = path.to_string_lossy().into_owned();
        let path_value = addr_val(&path_text);
        let listener = ok_data(mux_net_local_listener_bind(path_value));
        assert!(mux_rc_dec(path_value));
        let nonblocking = mux_net_local_listener_set_nonblocking(listener, 0);
        assert!(mux_result_is_ok(nonblocking));
        assert!(mux_rc_dec(nonblocking));

        let connect_path = addr_val(&path_text);
        let client = ok_data(mux_net_local_connect(connect_path));
        assert!(mux_rc_dec(connect_path));
        let nonblocking = mux_net_local_set_nonblocking(client, 0);
        assert!(mux_result_is_ok(nonblocking));
        assert!(mux_rc_dec(nonblocking));
        let invalid_timeout = mux_net_local_set_read_timeout(client, -1);
        assert!(!mux_result_is_ok(invalid_timeout));
        assert!(mux_rc_dec(invalid_timeout));
        let server = ok_data(mux_net_local_listener_accept(listener));

        let payload = bytes_val(b"local");
        let written = mux_net_local_write(client, payload);
        assert!(mux_result_is_ok(written));
        assert!(mux_rc_dec(written));
        assert!(mux_rc_dec(payload));

        let read = mux_net_local_read(server, 5);
        let read_data = mux_result_data(read);
        assert!(matches!(&*read_data, Value::Bytes(bytes) if bytes == b"local"));
        assert!(mux_rc_dec(read_data));
        assert!(mux_rc_dec(read));

        let reply = bytes_val(b"reply");
        let written = mux_net_local_write(server, reply);
        assert!(mux_result_is_ok(written));
        assert!(mux_rc_dec(written));
        assert!(mux_rc_dec(reply));
        let read = mux_net_local_read(client, 5);
        let read_data = mux_result_data(read);
        assert!(matches!(&*read_data, Value::Bytes(bytes) if bytes == b"reply"));
        assert!(mux_rc_dec(read_data));
        assert!(mux_rc_dec(read));

        let shutdown = mux_net_local_shutdown_write(client);
        assert!(mux_result_is_ok(shutdown));
        assert!(mux_rc_dec(shutdown));
        let shutdown = mux_net_local_shutdown_read(server);
        assert!(mux_result_is_ok(shutdown));
        assert!(mux_rc_dec(shutdown));

        mux_net_local_close(client);
        mux_net_local_close(server);
        mux_net_local_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
        assert!(!path.exists());
    }
}

#[test]
fn poller_reports_listener_readiness_and_deregisters() {
    unsafe {
        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));
        let address = ok_string(mux_net_tcp_listener_local_addr(listener));
        let poller = ok_data(mux_poller_new());
        let registration = ok_data(mux_poller_register_listener(poller, listener));
        assert!(matches!(&*registration, Value::Int(token) if *token > 0));
        let token = match &*registration {
            Value::Int(token) => *token,
            _ => unreachable!(),
        };
        assert!(mux_rc_dec(registration));

        let before = ok_data(mux_poller_poll(poller, 0));
        assert!(matches!(&*before, Value::List(values) if values.is_empty()));
        assert!(mux_rc_dec(before));

        let connect_addr = addr_val(&address);
        let client = ok_data(mux_net_tcp_connect(connect_addr));
        assert!(mux_rc_dec(connect_addr));
        let events = ok_data(mux_poller_poll(poller, 100));
        let event = match &*events {
            Value::List(values) => values.first().expect("listener readiness event"),
            _ => panic!("expected event list"),
        };
        let event_token = ok_data(mux_poll_event_token(event));
        assert!(matches!(&*event_token, Value::Int(value) if *value == token));
        assert!(mux_rc_dec(event_token));
        let readable = ok_data(mux_poll_event_readable(event));
        assert!(matches!(&*readable, Value::Bool(true)));
        assert!(mux_rc_dec(readable));
        assert!(mux_rc_dec(events));
        let server = ok_data(mux_net_tcp_listener_accept(listener));

        let deregistered = mux_poller_deregister(poller, token);
        assert!(mux_result_is_ok(deregistered));
        assert!(mux_rc_dec(deregistered));
        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
        assert!(mux_rc_dec(poller));
    }
}

#[test]
fn udp_roundtrip() {
    unsafe {
        let a_bind = addr_val("127.0.0.1:0");
        let b_bind = addr_val("127.0.0.1:0");
        let a = ok_data(mux_net_udp_bind(a_bind));
        let b = ok_data(mux_net_udp_bind(b_bind));
        assert!(mux_rc_dec(a_bind));
        assert!(mux_rc_dec(b_bind));

        let b_addr = ok_string(mux_net_udp_local_addr(b));
        let a_addr = ok_string(mux_net_udp_local_addr(a));
        let dest = addr_val(&b_addr);
        let payload = bytes_val(b"ping");

        let sent = mux_net_udp_send_to(a, payload, dest);
        assert!(mux_result_is_ok(sent));
        assert!(mux_rc_dec(sent));

        let recv = mux_net_udp_recv_from(b, 2);
        assert!(mux_result_is_ok(recv));
        let recv_data = mux_result_data(recv);
        let payload_result = mux_net_udp_datagram_bytes(recv_data);
        assert!(mux_result_is_ok(payload_result));
        let payload_data = mux_result_data(payload_result);
        assert!(matches!(&*payload_data, Value::Bytes(bytes) if bytes.as_slice() == b"pi"));
        assert!(mux_rc_dec(payload_data));
        assert!(mux_rc_dec(payload_result));
        let address_result = mux_net_udp_datagram_address(recv_data);
        assert!(mux_result_is_ok(address_result));
        let address_data = mux_result_data(address_result);
        assert!(matches!(&*address_data, Value::String(address) if address == &a_addr));
        assert!(mux_rc_dec(address_data));
        assert!(mux_rc_dec(address_result));
        let truncated_result = mux_net_udp_datagram_truncated(recv_data);
        assert!(mux_result_is_ok(truncated_result));
        let truncated_data = mux_result_data(truncated_result);
        assert!(matches!(&*truncated_data, Value::Bool(true)));
        assert!(mux_rc_dec(truncated_data));
        assert!(mux_rc_dec(truncated_result));
        assert!(mux_rc_dec(recv_data));
        assert!(mux_rc_dec(recv));
        let max_size = 16 * 1024 * 1024;
        assert_err(mux_net_udp_recv_from(b, -1));
        assert_err(mux_net_udp_recv_from(b, i64::MAX));
        assert_err(mux_net_udp_recv_from(b, max_size + 1));

        assert!(mux_rc_dec(payload));
        assert!(mux_rc_dec(dest));
        let ttl_set = mux_net_udp_set_ttl(a, 64);
        assert!(mux_result_is_ok(ttl_set));
        assert!(mux_rc_dec(ttl_set));
        let ttl = mux_net_udp_ttl(a);
        assert!(mux_result_is_ok(ttl));
        let ttl_data = mux_result_data(ttl);
        assert!(matches!(&*ttl_data, Value::Int(64)));
        assert!(mux_rc_dec(ttl_data));
        assert!(mux_rc_dec(ttl));
        for update in [
            mux_net_udp_set_recv_buffer_size(a, 4096),
            mux_net_udp_set_send_buffer_size(a, 4096),
        ] {
            assert!(mux_result_is_ok(update));
            assert!(mux_rc_dec(update));
        }
        let recv_buffer = mux_net_udp_recv_buffer_size(a);
        assert!(mux_result_is_ok(recv_buffer));
        let recv_buffer_data = mux_result_data(recv_buffer);
        assert!(matches!(&*recv_buffer_data, Value::Int(size) if *size >= 4096));
        assert!(mux_rc_dec(recv_buffer_data));
        assert!(mux_rc_dec(recv_buffer));
        let send_buffer = mux_net_udp_send_buffer_size(a);
        assert!(mux_result_is_ok(send_buffer));
        let send_buffer_data = mux_result_data(send_buffer);
        assert!(matches!(&*send_buffer_data, Value::Int(size) if *size >= 4096));
        assert!(mux_rc_dec(send_buffer_data));
        assert!(mux_rc_dec(send_buffer));
        let broadcast_set = mux_net_udp_set_broadcast(a, 1);
        assert!(mux_result_is_ok(broadcast_set));
        assert!(mux_rc_dec(broadcast_set));
        let broadcast = mux_net_udp_broadcast(a);
        assert!(mux_result_is_ok(broadcast));
        let broadcast_data = mux_result_data(broadcast);
        assert!(matches!(&*broadcast_data, Value::Bool(true)));
        assert!(mux_rc_dec(broadcast_data));
        assert!(mux_rc_dec(broadcast));
        let loop_set = mux_net_udp_set_multicast_loop_v4(a, 0);
        assert!(mux_result_is_ok(loop_set));
        assert!(mux_rc_dec(loop_set));
        let loopback = mux_net_udp_multicast_loop_v4(a);
        assert!(mux_result_is_ok(loopback));
        let loopback_data = mux_result_data(loopback);
        assert!(matches!(&*loopback_data, Value::Bool(false)));
        assert!(mux_rc_dec(loopback_data));
        assert!(mux_rc_dec(loopback));
        let multicast_ttl_set = mux_net_udp_set_multicast_ttl_v4(a, 8);
        assert!(mux_result_is_ok(multicast_ttl_set));
        assert!(mux_rc_dec(multicast_ttl_set));
        let multicast_ttl = mux_net_udp_multicast_ttl_v4(a);
        assert!(mux_result_is_ok(multicast_ttl));
        let multicast_ttl_data = mux_result_data(multicast_ttl);
        assert!(matches!(&*multicast_ttl_data, Value::Int(8)));
        assert!(mux_rc_dec(multicast_ttl_data));
        assert!(mux_rc_dec(multicast_ttl));
        let invalid_group = addr_val("not an IPv6 address");
        assert_err(mux_net_udp_join_multicast_v6(a, invalid_group, 0));
        assert!(mux_rc_dec(invalid_group));
        assert_err(mux_net_udp_set_multicast_hops_v6(a, -1));
        assert_err(mux_net_udp_set_multicast_hops_v6(a, i64::MAX));
        let v6_bind = addr_val("[::1]:0");
        let v6_result = mux_net_udp_bind(v6_bind);
        assert!(mux_rc_dec(v6_bind));
        if mux_result_is_ok(v6_result) {
            let v6 = ok_data(v6_result);
            assert_ok(mux_net_udp_set_multicast_loop_v6(v6, 0));
            let loopback_v6 = mux_net_udp_multicast_loop_v6(v6);
            assert!(mux_result_is_ok(loopback_v6));
            let loopback_v6_data = mux_result_data(loopback_v6);
            assert!(matches!(&*loopback_v6_data, Value::Bool(false)));
            assert!(mux_rc_dec(loopback_v6_data));
            assert!(mux_rc_dec(loopback_v6));
            assert_ok(mux_net_udp_set_multicast_hops_v6(v6, 8));
            let hops_v6 = mux_net_udp_multicast_hops_v6(v6);
            assert!(mux_result_is_ok(hops_v6));
            let hops_v6_data = mux_result_data(hops_v6);
            assert!(matches!(&*hops_v6_data, Value::Int(8)));
            assert!(mux_rc_dec(hops_v6_data));
            assert!(mux_rc_dec(hops_v6));
            mux_net_udp_close(v6);
            assert!(mux_rc_dec(v6));
        } else {
            assert!(mux_rc_dec(v6_result));
        }
        mux_net_udp_close(a);
        mux_net_udp_close(b);
        assert!(mux_rc_dec(a));
        assert!(mux_rc_dec(b));
    }
}

#[test]
fn invalid_addresses_error() {
    unsafe {
        let bad = addr_val("definitely not an address");
        assert_err(mux_net_tcp_listener_bind(bad));
        assert!(mux_rc_dec(bad));

        let bad2 = addr_val("definitely not an address");
        assert_err(mux_net_udp_bind(bad2));
        assert!(mux_rc_dec(bad2));
    }
}

#[test]
fn http_request_response_loopback() {
    unsafe {
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

        let parsed_result = mux_net_http_request_read(server);
        assert!(mux_result_is_ok(parsed_result));
        let parsed = mux_result_data(parsed_result);
        assert!(!parsed.is_null());
        assert!(mux_rc_dec(parsed));
        assert!(mux_rc_dec(parsed_result));

        // Server writes a typed response.
        let headers = mux_net_http_headers_new();
        let name = mux_rc_alloc(Value::String("X-Test".into()));
        let value = mux_rc_alloc(Value::String("yes".into()));
        let set_header = mux_net_http_headers_set(headers, name, value);
        assert!(mux_result_is_ok(set_header));
        assert!(mux_rc_dec(set_header));
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(value));
        let body = bytes_val(b"hello");
        let resp_val = ok_data(mux_net_http_response_from_config(200, headers, body));
        let wrote = mux_net_http_response_write(server, resp_val);
        assert!(mux_result_is_ok(wrote));
        assert!(mux_rc_dec(wrote));
        assert!(mux_rc_dec(resp_val));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(body));

        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

#[test]
fn http_null_inputs_error() {
    unsafe {
        assert_err(mux_net_http_request_read(std::ptr::null_mut()));
        assert_err(mux_net_http_request_send(std::ptr::null()));
    }
}

#[test]
fn http_server_serve_rejects_unbounded_request_counts() {
    unsafe {
        let zero = mux_net_http_server_serve(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        );
        assert_err(zero);

        let too_many = mux_net_http_server_serve(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            1_000_001,
        );
        assert_err(too_many);
    }
}

#[test]
fn typed_http_server_rejects_null_handler_before_accepting() {
    unsafe {
        let result = mux_net_http_server_serve_once(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_err(result);
    }
}

#[test]
fn typed_http_handles_reject_values_of_other_runtime_types() {
    unsafe {
        let not_headers = mux_rc_alloc(Value::Int(1));
        let name = mux_rc_alloc(Value::String("content-type".to_string()));
        assert_err(mux_net_http_headers_get(not_headers, name));
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(not_headers));
    }
}

#[test]
fn http_client_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn request_complete(data: &[u8]) -> Option<usize> {
        let pos = data.windows(4).position(|w| w == b"\r\n\r\n")?;
        let header = String::from_utf8_lossy(&data[..pos]).to_lowercase();
        let content_len = header
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        Some(pos + 4 + content_len)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut data = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            data.extend_from_slice(&buf[..n]);
            let Some(expected_len) = request_complete(&data) else {
                continue;
            };
            while data.len() < expected_len {
                let n = stream.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            return Ok(());
        }
    }

    fn write_response(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let body = b"{\"ok\":true}";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    }

    // A tiny canned-response server on its own thread (std sockets are Send).
    // It fully drains each request (headers + Content-Length body) before
    // responding, so the client never sees a reset mid-write under load.
    fn serve_one(listener: &TcpListener) {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        if read_request(&mut stream).is_ok() {
            let _ = write_response(&mut stream);
        }
    }

    unsafe {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            serve_one(&listener);
        });

        let url = format!("http://{server_addr}/");
        let typed_url = mux_rc_alloc(Value::String(url));
        let request = mux_net_http_request_new();
        let method = mux_rc_alloc(Value::String("GET".into()));
        let method_set = mux_net_http_request_set_method_field(request, method);
        assert!(mux_result_is_ok(method_set));
        assert!(mux_rc_dec(method_set));
        assert!(mux_rc_dec(method));
        let url_set = mux_net_http_request_set_url_field(request, typed_url);
        assert!(mux_result_is_ok(url_set));
        assert!(mux_rc_dec(url_set));
        assert!(mux_rc_dec(typed_url));
        let response = ok_data(mux_net_http_request_send(request));
        let status = ok_data(mux_net_http_response_status_value(response));
        assert!(matches!(&*status, Value::Int(200)));
        assert!(mux_rc_dec(status));
        let checked = ok_data(mux_net_http_response_error_for_status(response));
        assert!(mux_rc_dec(checked));
        let json = ok_data(mux_net_http_response_read_json(response, 1024));
        assert!(matches!(&*json, Value::Map(_)));
        assert!(mux_rc_dec(json));
        assert!(mux_rc_dec(response));
        assert!(mux_rc_dec(request));

        handle.join().unwrap();
    }
}

#[test]
fn http_request_errors() {
    unsafe {
        // A default request has no URL and cannot be sent.
        let no_url = mux_net_http_request_new();
        assert_err(mux_net_http_request_send(no_url));
        assert!(mux_rc_dec(no_url));

        // transport failure: nothing is listening on port 1
        let refused = mux_net_http_request_new();
        let method = mux_rc_alloc(Value::String("GET".into()));
        let url = mux_rc_alloc(Value::String("http://127.0.0.1:1/".into()));
        assert_ok(mux_net_http_request_set_method_field(refused, method));
        assert_ok(mux_net_http_request_set_url_field(refused, url));
        assert!(mux_rc_dec(method));
        assert!(mux_rc_dec(url));
        assert_err(mux_net_http_request_send(refused));
        assert!(mux_rc_dec(refused));

        // An explicit proxy is parsed before transport, so malformed proxy
        // configuration is reported as a request error.
        let invalid_proxy = mux_net_http_request_new();
        let method = mux_rc_alloc(Value::String("GET".into()));
        let url = mux_rc_alloc(Value::String("http://127.0.0.1:1/".into()));
        let proxy = mux_rc_alloc(Value::String("not a proxy URL".into()));
        assert_ok(mux_net_http_request_set_method_field(invalid_proxy, method));
        assert_ok(mux_net_http_request_set_url_field(invalid_proxy, url));
        assert_ok(mux_net_http_request_set_proxy_field(invalid_proxy, proxy));
        assert!(mux_rc_dec(method));
        assert!(mux_rc_dec(url));
        assert!(mux_rc_dec(proxy));
        assert_err(mux_net_http_request_send(invalid_proxy));
        assert!(mux_rc_dec(invalid_proxy));
    }
}

#[test]
fn http_response_validation_and_udp_extras() {
    unsafe {
        // status out of range
        let headers = mux_net_http_headers_new();
        let body = bytes_val(b"");
        assert_err(mux_net_http_response_from_config(99, headers, body));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(body));

        // Constructed responses use the same bounded HTTP body contract as
        // request bodies and network response reads.
        let headers = mux_net_http_headers_new();
        let oversized = bytes_val(&vec![0_u8; 16 * 1024 * 1024 + 1]);
        assert_err(mux_net_http_response_from_config(200, headers, oversized));
        assert!(mux_rc_dec(headers));
        assert!(mux_rc_dec(oversized));

        let response = mux_net_http_response_new();
        let oversized = bytes_val(&vec![0_u8; 16 * 1024 * 1024 + 1]);
        assert_err(mux_net_http_response_set_body_field(response, oversized));
        assert!(mux_rc_dec(oversized));
        assert!(mux_rc_dec(response));

        // non-string header value
        let headers = mux_net_http_headers_new();
        let name = mux_rc_alloc(Value::String("X".into()));
        let bad_value = mux_rc_alloc(Value::Int(1));
        assert_err(mux_net_http_headers_set(headers, name, bad_value));
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(bad_value));
        assert!(mux_rc_dec(headers));

        // UDP extras: set_nonblocking ok, peer_addr on unconnected socket errors
        let a_bind = addr_val("127.0.0.1:0");
        let a = ok_data(mux_net_udp_bind(a_bind));
        assert!(mux_rc_dec(a_bind));
        let nb = mux_net_udp_set_nonblocking(a, 1);
        assert!(mux_result_is_ok(nb));
        assert!(mux_rc_dec(nb));
        for timeout in [
            mux_net_udp_set_read_timeout(a, 100),
            mux_net_udp_set_write_timeout(a, 100),
            mux_net_udp_set_read_timeout(a, 0),
            mux_net_udp_set_write_timeout(a, 0),
        ] {
            assert!(mux_result_is_ok(timeout));
            assert!(mux_rc_dec(timeout));
        }
        assert_err(mux_net_udp_set_read_timeout(a, -1));
        assert_err(mux_net_udp_set_write_timeout(a, -1));
        assert_err(mux_net_udp_peer_addr(a));
        mux_net_udp_close(a);
        assert!(mux_rc_dec(a));
    }
}

#[test]
fn http_read_request_body_without_content_length_errors() {
    unsafe {
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

        assert_err(mux_net_http_request_read(server));

        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

#[test]
fn typed_http_request_rejects_conflicting_content_lengths() {
    unsafe {
        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));
        let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
        let connect_addr = addr_val(&addr);
        let client = ok_data(mux_net_tcp_connect(connect_addr));
        assert!(mux_rc_dec(connect_addr));
        let server = ok_data(mux_net_tcp_listener_accept(listener));
        let req = bytes_val(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\na",
        );
        let written = mux_net_tcp_write(client, req);
        assert!(mux_result_is_ok(written));
        assert!(mux_rc_dec(written));
        assert!(mux_rc_dec(req));
        assert_err(mux_net_http_request_read(server));
        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

#[test]
fn typed_http_request_decodes_chunked_body() {
    unsafe {
        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));
        let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
        let connect_addr = addr_val(&addr);
        let client = ok_data(mux_net_tcp_connect(connect_addr));
        assert!(mux_rc_dec(connect_addr));
        let server = ok_data(mux_net_tcp_listener_accept(listener));
        let req = bytes_val(
            b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;ext=yes\r\ndefg\r\n0\r\nX-Trailer: yes\r\n\r\n",
        );
        let written = mux_net_tcp_write(client, req);
        assert!(mux_result_is_ok(written));
        assert!(mux_rc_dec(written));
        assert!(mux_rc_dec(req));
        let request = ok_data(mux_net_http_request_read(server));
        let body = mux_net_http_request_body(request);
        assert!(matches!(&*body, Value::Bytes(value) if value == b"abcdefg"));
        assert!(mux_rc_dec(body));
        assert!(mux_rc_dec(request));
        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

#[test]
fn invalid_read_size_errors() {
    unsafe {
        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));
        let addr = ok_string(mux_net_tcp_listener_local_addr(listener));
        let connect_addr = addr_val(&addr);
        let client = ok_data(mux_net_tcp_connect(connect_addr));
        assert!(mux_rc_dec(connect_addr));
        let server = ok_data(mux_net_tcp_listener_accept(listener));

        assert_err(mux_net_tcp_read(client, 0));
        assert_err(mux_net_tcp_read(client, -1));
        assert_err(mux_net_tcp_read(client, i64::MAX));

        let payload = bytes_val(b"boundary");
        let written = mux_net_tcp_write(server, payload);
        assert!(mux_result_is_ok(written));
        assert!(mux_rc_dec(written));
        assert!(mux_rc_dec(payload));

        let max_size = 16 * 1024 * 1024;
        let at_limit = mux_net_tcp_read(client, max_size);
        assert!(mux_result_is_ok(at_limit));
        assert!(mux_rc_dec(at_limit));
        assert_err(mux_net_tcp_read(client, max_size + 1));

        mux_net_tcp_close(client);
        mux_net_tcp_close(server);
        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(client));
        assert!(mux_rc_dec(server));
        assert!(mux_rc_dec(listener));
    }
}

/// A copy of a socket names the SAME socket, and both names keep working.
///
/// The type registered a destructor but no copy callback, so `copy_object`
/// returned null and `auto keep = listener` in Mux produced a value whose handle
/// was zero - every later call on it answered "invalid tcp listener". A socket
/// cannot be duplicated, so the copy shares it.
#[test]
fn a_copied_listener_names_the_same_socket() {
    unsafe {
        use mux_runtime::refcount::mux_value_deep_clone;

        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));

        let original = ok_string(mux_net_tcp_listener_local_addr(listener));

        let copy = mux_value_deep_clone(listener);
        assert!(!copy.is_null(), "copying a listener must not yield null");

        let from_copy = ok_string(mux_net_tcp_listener_local_addr(copy));
        assert_eq!(
            from_copy, original,
            "the copy must name the same socket, not a different or absent one"
        );

        // Dropping one name leaves the other usable: the socket closes at the last
        // one, not the first.
        assert!(mux_rc_dec(copy));
        let after_drop = ok_string(mux_net_tcp_listener_local_addr(listener));
        assert_eq!(after_drop, original, "the socket closed while still named");

        mux_net_tcp_listener_close(listener);
        assert!(mux_rc_dec(listener));
    }
}

/// An explicit `close()` closes the socket for every name, which is what the
/// program asked for - unlike dropping a name, which only gives one up.
#[test]
fn close_is_not_reference_counted() {
    unsafe {
        use mux_runtime::refcount::mux_value_deep_clone;

        let bind_addr = addr_val("127.0.0.1:0");
        let listener = ok_data(mux_net_tcp_listener_bind(bind_addr));
        assert!(mux_rc_dec(bind_addr));

        let copy = mux_value_deep_clone(listener);
        assert!(!copy.is_null());

        mux_net_tcp_listener_close(listener);
        assert_err(mux_net_tcp_listener_local_addr(copy));

        assert!(mux_rc_dec(copy));
        assert!(mux_rc_dec(listener));
    }
}
