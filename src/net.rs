#![allow(clippy::missing_safety_doc)]

use crate::json::{json_to_value, value_to_json, Json};
use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
    register_shared_object_type,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::std::StdErrorKind;
use crate::stream::ReaderAdapter;
use crate::sync_primitives::cancellation_requested;
use crate::{TypeId, Value};
use base64::{
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD as BASE64_URL_SAFE},
    Engine as _,
};
#[cfg(any(feature = "http2", feature = "http3"))]
use bytes::{Buf, Bytes};
use cap_std::ambient_authority;
use cap_std::fs::Dir as CapabilityDir;
#[cfg(windows)]
use named_pipe::{ConnectingServer, PipeClient, PipeOptions, PipeServer};
use ring::digest::{digest, SHA256};
use ring::signature::{RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256};
#[cfg(feature = "http2")]
use rustls::pki_types::ServerName;
#[cfg(feature = "http2")]
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use sha1::{Digest as Sha1Digest, Sha1};
use socket2::SockRef;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::fs;
use std::io::{Cursor, Read, Write};
#[cfg(feature = "http2")]
use std::net::ToSocketAddrs;
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, TcpListener as StdTcpListener, TcpStream as StdTcpStream,
    UdpSocket as StdUdpSocket,
};
#[cfg(unix)]
use std::os::unix::net::{UnixListener as StdLocalListener, UnixStream as StdLocalStream};
use std::path::{Path, PathBuf};
#[cfg(feature = "http2")]
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
#[cfg(feature = "http2")]
use std::sync::Weak;
use std::sync::{mpsc, Arc, Condvar, LazyLock, Mutex};
#[cfg(feature = "http2")]
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(feature = "http2")]
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct HeaderData {
    values: Vec<(String, String)>,
}

struct HeaderEntry {
    data: Arc<Mutex<HeaderData>>,
    names: usize,
}

struct HttpRequestEntry {
    method: String,
    url: String,
    request_id: String,
    proxy: Option<String>,
    headers: Arc<Mutex<HeaderData>>,
    body: Option<Vec<u8>>,
    body_reader: Option<ReaderAdapter>,
    path_params: Arc<Mutex<HashMap<String, String>>>,
    connect_timeout_ms: i64,
    timeout_ms: i64,
    max_redirects: i64,
    retries: i64,
    retry_backoff_ms: i64,
    names: usize,
}

struct HttpResponseEntry {
    status: i64,
    headers: Arc<Mutex<HeaderData>>,
    body: Vec<u8>,
    body_reader: Option<Box<dyn Read + Send>>,
    streamed_bytes: usize,
    position: usize,
    names: usize,
}

#[derive(Clone)]
struct HttpServerConfigEntry {
    max_header_bytes: i64,
    max_body_bytes: i64,
    max_headers: i64,
    read_timeout_ms: i64,
    access_log: bool,
    cors_origins: Vec<String>,
    cors_allow_credentials: bool,
    static_root: String,
    worker_count: i64,
    heartbeat_interval_ms: i64,
    names: usize,
}

struct HttpRouteEntry {
    method: String,
    segments: Vec<HttpRouteSegment>,
    specificity: Vec<i32>,
    // Opaque callback addresses are stored as integers so the router registry
    // can be shared safely across threads. They are converted back only at
    // the synchronous invocation boundary.
    handler: usize,
}

#[derive(Clone)]
enum HttpRouteSegment {
    Literal(String),
    Parameter(String),
    CatchAll(String),
}

/// Built-in authentication middleware is stored as data instead of a
/// compiler callback. This keeps the policy synchronous and lets the router
/// compose it with ordinary `HttpNext` middleware without exposing secrets to
/// logs or to an extra closure ABI.
enum HttpMiddlewareEntry {
    Callback(usize),
    Basic {
        username: String,
        password: String,
    },
    Bearer {
        token: String,
    },
    OAuthOidc {
        issuer: String,
        audience: String,
        jwks_url: String,
    },
}

struct HttpRouterData {
    routes: Vec<HttpRouteEntry>,
    middleware: Vec<HttpMiddlewareEntry>,
}

struct HttpRouterEntry {
    data: Arc<Mutex<HttpRouterData>>,
    names: usize,
}

struct HttpNextEntry {
    router: i64,
    stage: usize,
    names: usize,
}

/// One bounded Server-Sent Event. The event is encoded into an ordinary
/// `bytes` body so callers can compose it with `HttpResponse` or send it
/// through an owned `SseStream` without a second callback ABI.
struct SseEventEntry {
    event: String,
    id: String,
    retry_ms: i64,
    data: String,
    names: usize,
}

/// One RFC 6455 WebSocket frame. The codec handles one complete frame at a
/// time, while `reassemble` provides a bounded stateless operation for a
/// caller-supplied fragment sequence. A `WebSocketSession` owns connection
/// lifecycle and incremental frame I/O.
struct WebSocketFrameEntry {
    fin: bool,
    opcode: i64,
    payload: Vec<u8>,
    masked: bool,
    names: usize,
}

/// Values used to compose a WebSocket opening handshake with HttpRequest and
/// HttpResponse. The transport remains explicit: this type does not hide an
/// HTTP request or create a long-lived/async connection.
struct WebSocketHandshakeEntry {
    key: String,
    protocol: String,
    names: usize,
}

struct HttpResponseData {
    status: i64,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_reader: Option<Box<dyn Read + Send>>,
}

#[cfg(feature = "http2")]
const HTTP2_REQUEST_QUEUE_CAPACITY: usize = 64;

#[cfg(feature = "http2")]
const HTTP2_IO_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(feature = "http3")]
const HTTP3_REQUEST_QUEUE_CAPACITY: usize = 64;

#[cfg(feature = "http3")]
const HTTP3_SERVER_COMMAND_QUEUE_CAPACITY: usize = 1;

#[cfg(feature = "http3")]
const HTTP3_SERVER_SESSION_CAPACITY: usize = 64;

#[cfg(feature = "http3")]
const HTTP3_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(feature = "http2")]
struct Http2RequestMessage {
    request: http::Request<()>,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
    cancelled: Arc<AtomicBool>,
    reply: std::sync::mpsc::SyncSender<Result<HttpResponseData, String>>,
}

#[cfg(feature = "http2")]
struct Http2ConnectionActor {
    requests: tokio::sync::mpsc::Sender<Http2RequestMessage>,
}

#[cfg(feature = "http2")]
impl Http2ConnectionActor {
    fn request(
        &self,
        request: http::Request<()>,
        body: Option<Vec<u8>>,
        timeout: Option<Duration>,
    ) -> Result<HttpResponseData, String> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.requests
            .blocking_send(Http2RequestMessage {
                request,
                body,
                timeout,
                cancelled: Arc::clone(&cancelled),
                reply,
            })
            .map_err(|_| "HTTP/2 connection actor is closed".to_string())?;
        timeout.map_or_else(
            || {
                result
                    .recv()
                    .map_err(|_| "HTTP/2 connection actor stopped".to_string())
            },
            |timeout| {
                result.recv_timeout(timeout).map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        cancelled.store(true, Ordering::Release);
                        "HTTP/2 request timed out".to_string()
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        "HTTP/2 connection actor stopped".to_string()
                    }
                })
            },
        )?
    }
}

#[cfg(feature = "http3")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Http3ErrorKind {
    Invalid,
    Resolve,
    Transport,
    Timeout,
    Protocol,
    BodyTooLarge,
    Unsupported,
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Http3Error {
    kind: Http3ErrorKind,
    detail: String,
    dispatched: bool,
}

#[cfg(feature = "http3")]
impl Http3Error {
    fn new(kind: Http3ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            dispatched: false,
        }
    }

    fn mark_dispatched(mut self) -> Self {
        self.dispatched = true;
        self
    }

    #[must_use]
    fn was_dispatched(&self) -> bool {
        self.dispatched
    }

    #[must_use]
    pub const fn kind(&self) -> Http3ErrorKind {
        self.kind
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Http3Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(feature = "http3")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpProtocol {
    Http3,
    Http2,
    Http1,
}

/// Select a supported protocol from ALPN names in preference order.
///
/// The names are considered in the order supplied by the peer or transport.
/// The caller gets a typed error when no supported protocol remains.
#[cfg(feature = "http3")]
pub fn select_http_protocol(alpn_names: &[&[u8]]) -> Result<HttpProtocol, Http3Error> {
    for name in alpn_names {
        match *name {
            b"h3" => return Ok(HttpProtocol::Http3),
            b"h2" => return Ok(HttpProtocol::Http2),
            b"http/1.1" => return Ok(HttpProtocol::Http1),
            _ => {}
        }
    }
    Err(Http3Error::new(
        Http3ErrorKind::Unsupported,
        "no supported HTTP protocol was advertised",
    ))
}

#[cfg(feature = "http3")]
struct Http3RequestMessage {
    request: http::Request<()>,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
    cancelled: Arc<AtomicBool>,
    dispatched: Arc<AtomicBool>,
    reply: std::sync::mpsc::SyncSender<Result<Http3Response, Http3Error>>,
}

#[cfg(feature = "http3")]
pub struct Http3ClientTransport {
    requests: tokio::sync::mpsc::Sender<Http3RequestMessage>,
    #[cfg(test)]
    actor_done: Arc<AtomicBool>,
}

#[cfg(feature = "http3")]
impl Http3ClientTransport {
    pub fn connect(authority: &str) -> Result<Self, Http3Error> {
        Self::connect_with_timeout(
            authority,
            Duration::from_millis(u64::try_from(DEFAULT_HTTP_CONNECT_TIMEOUT_MS).unwrap_or(10_000)),
        )
    }

    /// Connect using caller-supplied DER certificates as trust anchors.
    ///
    /// The default [`Self::connect`] path uses the bundled public Web PKI
    /// roots. This variant is for private deployments and local test servers;
    /// certificates are still verified for the requested authority.
    pub fn connect_with_trusted_roots(
        authority: &str,
        roots: &[Vec<u8>],
    ) -> Result<Self, Http3Error> {
        Self::connect_with_timeout_and_roots(
            authority,
            Duration::from_millis(u64::try_from(DEFAULT_HTTP_CONNECT_TIMEOUT_MS).unwrap_or(10_000)),
            Some(roots.to_vec()),
        )
    }

    fn connect_with_timeout(authority: &str, timeout: Duration) -> Result<Self, Http3Error> {
        Self::connect_with_timeout_and_roots(authority, timeout, None)
    }

    fn connect_with_timeout_and_roots(
        authority: &str,
        timeout: Duration,
        trusted_roots: Option<Vec<Vec<u8>>>,
    ) -> Result<Self, Http3Error> {
        let (host, port) = parse_http3_authority(authority)?;
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let thread_authority = authority.to_string();
        #[cfg(test)]
        let actor_done = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let actor_done_for_thread = Arc::clone(&actor_done);
        thread::Builder::new()
            .name("mux-http3-connection".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_sender.send(Err(Http3Error::new(
                            Http3ErrorKind::Transport,
                            format!("HTTP/3 runtime setup failed: {error}"),
                        )));
                        #[cfg(test)]
                        actor_done_for_thread.store(true, Ordering::Release);
                        return;
                    }
                };
                let setup_sender = ready_sender.clone();
                #[cfg(test)]
                let actor_done_for_setup = Arc::clone(&actor_done_for_thread);
                let result = runtime.block_on(async move {
                    let setup = async move {
                        let mut addresses = tokio::net::lookup_host((host.as_str(), port))
                            .await
                            .map_err(|error| {
                                Http3Error::new(
                                    Http3ErrorKind::Resolve,
                                    format!("HTTP/3 address lookup failed for {thread_authority}: {error}"),
                                )
                            })?;
                        let address = addresses.next().ok_or_else(|| {
                            Http3Error::new(
                                Http3ErrorKind::Resolve,
                                format!("HTTP/3 address lookup returned no addresses for {thread_authority}"),
                            )
                        })?;
                        let client_config = http3_client_config(trusted_roots.as_deref())?;
                        let mut endpoint = quinn::Endpoint::client(
                            "0.0.0.0:0".parse().map_err(|error| {
                                Http3Error::new(
                                    Http3ErrorKind::Transport,
                                    format!("HTTP/3 client endpoint address failed: {error}"),
                                )
                            })?,
                        )
                        .map_err(|error| {
                            Http3Error::new(
                                Http3ErrorKind::Transport,
                                format!("HTTP/3 client endpoint setup failed: {error}"),
                            )
                        })?;
                        endpoint.set_default_client_config(client_config);
                        let connecting = endpoint.connect(address, &host).map_err(|error| {
                            Http3Error::new(
                                Http3ErrorKind::Transport,
                                format!("HTTP/3 connection setup failed: {error}"),
                            )
                        })?;
                        let connection = connecting.await.map_err(|error| {
                            Http3Error::new(
                                Http3ErrorKind::Transport,
                                format!("HTTP/3 QUIC connection failed: {error}"),
                            )
                        })?;
                        let quic = h3_quinn::Connection::new(connection);
                        let (mut driver, sender) = h3::client::builder()
                            .max_field_section_size(MAX_HTTP_HEADER_BYTES as u64)
                            .build::<_, _, Bytes>(quic)
                            .await
                            .map_err(|error| {
                                Http3Error::new(
                                    Http3ErrorKind::Protocol,
                                    format!("HTTP/3 client handshake failed: {error}"),
                                )
                            })?;
                        let (requests, incoming) =
                            tokio::sync::mpsc::channel(HTTP3_REQUEST_QUEUE_CAPACITY);
                        setup_sender
                            .send(Ok(Http3ClientTransport {
                                requests,
                                #[cfg(test)]
                                actor_done: actor_done_for_setup,
                            }))
                            .map_err(|_| {
                                Http3Error::new(
                                    Http3ErrorKind::Transport,
                                    "HTTP/3 caller stopped during connection setup",
                                )
                        })?;
                        let _driver = tokio::spawn(async move {
                            let _ =
                                std::future::poll_fn(|context| driver.poll_close(context)).await;
                        });
                        Ok::<_, Http3Error>((endpoint, incoming, sender))
                    };
                    let (endpoint, mut incoming, mut sender) = tokio::time::timeout(timeout, setup)
                        .await
                        .map_err(|_| {
                            Http3Error::new(
                                Http3ErrorKind::Timeout,
                                "HTTP/3 connection timed out",
                            )
                        })??;
                    while let Some(message) = incoming.recv().await {
                            let result = execute_http3_stream(
                                &mut sender,
                                message.request,
                                message.body,
                                message.timeout,
                                message.cancelled,
                                message.dispatched,
                            )
                            .await;
                            let _ = message.reply.send(result);
                    }
                    endpoint.close(0u32.into(), b"HTTP/3 client closed");
                    Ok::<(), Http3Error>(())
                });
                if let Err(error) = result {
                    let _ = ready_sender.send(Err(error));
                }
                #[cfg(test)]
                actor_done_for_thread.store(true, Ordering::Release);
            })
            .map_err(|error| {
                Http3Error::new(
                    Http3ErrorKind::Transport,
                    format!("could not start HTTP/3 connection actor: {error}"),
                )
            })?;
        ready_receiver.recv().map_err(|_| {
            Http3Error::new(
                Http3ErrorKind::Transport,
                "HTTP/3 connection actor stopped during startup",
            )
        })?
    }

    pub fn send(
        &self,
        request: http::Request<()>,
        body: Option<Vec<u8>>,
        timeout: Option<Duration>,
    ) -> Result<Http3Response, Http3Error> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let dispatched = Arc::new(AtomicBool::new(false));
        let message = Http3RequestMessage {
            request,
            body,
            timeout,
            cancelled: Arc::clone(&cancelled),
            dispatched: Arc::clone(&dispatched),
            reply,
        };
        match timeout {
            Some(timeout) => {
                let deadline = Instant::now()
                    .checked_add(timeout)
                    .unwrap_or_else(Instant::now);
                let mut message = message;
                loop {
                    match self.requests.try_send(message) {
                        Ok(()) => break,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(next)) => {
                            if Instant::now() >= deadline {
                                cancelled.store(true, Ordering::Release);
                                return Err(Http3Error::new(
                                    Http3ErrorKind::Timeout,
                                    "HTTP/3 request queue timed out",
                                ));
                            }
                            message = next;
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            return Err(Http3Error::new(
                                Http3ErrorKind::Transport,
                                "HTTP/3 connection actor is closed",
                            ));
                        }
                    }
                }
            }
            None => self.requests.blocking_send(message).map_err(|_| {
                Http3Error::new(
                    Http3ErrorKind::Transport,
                    "HTTP/3 connection actor is closed",
                )
            })?,
        }
        let result = match timeout {
            Some(timeout) => result.recv_timeout(timeout).map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    cancelled.store(true, Ordering::Release);
                    Http3Error::new(Http3ErrorKind::Timeout, "HTTP/3 request timed out")
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    Http3Error::new(Http3ErrorKind::Transport, "HTTP/3 connection actor stopped")
                }
            }),
            None => result.recv().map_err(|_| {
                Http3Error::new(Http3ErrorKind::Transport, "HTTP/3 connection actor stopped")
            }),
        };
        result.and_then(|result| result).map_err(|error| {
            if dispatched.load(Ordering::Acquire) {
                error.mark_dispatched()
            } else {
                error
            }
        })
    }
}

/// A bounded synchronous HTTP/3 server listener.
///
/// The listener owns its QUIC endpoint on a private runtime thread. Callers
/// interact with it through blocking methods, while h3 and Quinn remain an
/// implementation detail. Each `serve_one` call accepts one QUIC connection,
/// handles one request, sends its response, and returns.
#[cfg(feature = "http3")]
pub struct Http3ServerTransport {
    commands: Option<tokio::sync::mpsc::Sender<Http3ServerCommand>>,
    local_addr: String,
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(feature = "http3")]
struct Http3ServerCommand {
    timeout: Duration,
    handler: Http3ServerHandler,
    reply: std::sync::mpsc::SyncSender<Result<(), Http3Error>>,
}

#[cfg(feature = "http3")]
type Http3ServerHandler =
    Box<dyn FnOnce(Http3ServerRequest) -> Result<Http3Response, Http3Error> + Send + 'static>;

#[cfg(feature = "http3")]
type Http3ServerConnection = h3::server::Connection<h3_quinn::Connection, Bytes>;

/// The decoded request passed to a synchronous HTTP/3 server handler.
#[cfg(feature = "http3")]
pub struct Http3ServerRequest {
    pub request: http::Request<()>,
    pub body: Vec<u8>,
}

#[cfg(feature = "http3")]
impl Http3ServerTransport {
    /// Bind an HTTP/3 listener using DER certificates and a DER private key.
    ///
    /// TLS 1.3 and the `h3` ALPN identifier are configured automatically.
    pub fn bind(
        bind_addr: &str,
        cert_chain: &[Vec<u8>],
        private_key: &[u8],
    ) -> Result<Self, Http3Error> {
        let address = bind_addr.parse().map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Invalid,
                format!("invalid HTTP/3 bind address '{bind_addr}': {error}"),
            )
        })?;
        if cert_chain.is_empty() {
            return Err(Http3Error::new(
                Http3ErrorKind::Invalid,
                "HTTP/3 server certificate chain must not be empty",
            ));
        }
        if private_key.is_empty() {
            return Err(Http3Error::new(
                Http3ErrorKind::Invalid,
                "HTTP/3 server private key must not be empty",
            ));
        }

        let certificates = cert_chain.to_vec();
        let key = private_key.to_vec();
        let (commands, mut command_receiver) =
            tokio::sync::mpsc::channel(HTTP3_SERVER_COMMAND_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("mux-http3-server".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_sender.send(Err(Http3Error::new(
                            Http3ErrorKind::Transport,
                            format!("HTTP/3 server runtime setup failed: {error}"),
                        )));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let result = setup_http3_server_endpoint(address, certificates, key);
                    let (endpoint, local_addr) = match result {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = ready_sender.send(Err(error));
                            return;
                        }
                    };
                    if ready_sender.send(Ok(local_addr.to_string())).is_err() {
                        endpoint.close(0u32.into(), b"HTTP/3 server caller stopped");
                        return;
                    }
                    let mut sessions = VecDeque::with_capacity(HTTP3_SERVER_SESSION_CAPACITY);
                    while let Some(command) = command_receiver.recv().await {
                        let Http3ServerCommand {
                            timeout,
                            handler,
                            reply,
                        } = command;
                        let result = match serve_http3_request(&endpoint, timeout, handler).await {
                            Ok(session) => {
                                if sessions.len() == HTTP3_SERVER_SESSION_CAPACITY {
                                    sessions.pop_front();
                                }
                                sessions.push_back(session);
                                Ok(())
                            }
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(result);
                    }
                    endpoint.close(0u32.into(), b"HTTP/3 server closed");
                });
            })
            .map_err(|error| {
                Http3Error::new(
                    Http3ErrorKind::Transport,
                    format!("could not start HTTP/3 server actor: {error}"),
                )
            })?;

        let local_addr = ready_receiver.recv().map_err(|_| {
            Http3Error::new(
                Http3ErrorKind::Transport,
                "HTTP/3 server actor stopped during startup",
            )
        })??;
        Ok(Self {
            commands: Some(commands),
            local_addr,
            worker: Some(worker),
        })
    }

    /// Return the address selected by the operating system.
    #[must_use]
    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }

    /// Accept one QUIC connection, handle one request, and send one response.
    pub fn serve_one<F>(&self, handler: F) -> Result<(), Http3Error>
    where
        F: FnOnce(Http3ServerRequest) -> Result<Http3Response, Http3Error> + Send + 'static,
    {
        self.serve_one_with_timeout(HTTP3_SERVER_REQUEST_TIMEOUT, handler)
    }

    /// Synchronous `serve_one` variant with an explicit total deadline.
    pub fn serve_one_with_timeout<F>(&self, timeout: Duration, handler: F) -> Result<(), Http3Error>
    where
        F: FnOnce(Http3ServerRequest) -> Result<Http3Response, Http3Error> + Send + 'static,
    {
        if timeout.is_zero() {
            return Err(Http3Error::new(
                Http3ErrorKind::Invalid,
                "HTTP/3 server request timeout must be greater than zero",
            ));
        }
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| Http3Error::new(Http3ErrorKind::Transport, "HTTP/3 server is closed"))?;
        commands
            .blocking_send(Http3ServerCommand {
                timeout,
                handler: Box::new(handler),
                reply,
            })
            .map_err(|_| {
                Http3Error::new(Http3ErrorKind::Transport, "HTTP/3 server actor is closed")
            })?;
        result.recv().map_err(|_| {
            Http3Error::new(
                Http3ErrorKind::Transport,
                "HTTP/3 server actor stopped while serving a request",
            )
        })?
    }
}

#[cfg(feature = "http3")]
impl Drop for Http3ServerTransport {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(feature = "http3")]
fn setup_http3_server_endpoint(
    address: std::net::SocketAddr,
    cert_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
) -> Result<(quinn::Endpoint, std::net::SocketAddr), Http3Error> {
    use quinn::crypto::rustls::QuicServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certificates = cert_chain.into_iter().map(CertificateDer::from).collect();
    let private_key = PrivateKeyDer::try_from(private_key).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Invalid,
            format!("invalid HTTP/3 server private key: {error}"),
        )
    })?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 server TLS protocol setup failed: {error}"),
        )
    })?
    .with_no_client_auth()
    .with_single_cert(certificates, private_key)
    .map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Invalid,
            format!("invalid HTTP/3 server certificate configuration: {error}"),
        )
    })?;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = QuicServerConfig::try_from(tls).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 server QUIC TLS setup failed: {error}"),
        )
    })?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let transport = Arc::get_mut(&mut server_config.transport).ok_or_else(|| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            "HTTP/3 server transport configuration is unexpectedly shared",
        )
    })?;
    // HTTP/3 uses unidirectional control and QPACK streams during setup.
    transport.max_concurrent_uni_streams(16_u8.into());
    let endpoint = quinn::Endpoint::server(server_config, address).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 server endpoint setup failed: {error}"),
        )
    })?;
    let local_addr = endpoint.local_addr().map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 server local address failed: {error}"),
        )
    })?;
    Ok((endpoint, local_addr))
}

#[cfg(feature = "http3")]
async fn serve_http3_request(
    endpoint: &quinn::Endpoint,
    timeout: Duration,
    handler: Http3ServerHandler,
) -> Result<Http3ServerConnection, Http3Error> {
    tokio::time::timeout(timeout, serve_http3_request_inner(endpoint, handler))
        .await
        .map_err(|_| Http3Error::new(Http3ErrorKind::Timeout, "HTTP/3 server request timed out"))?
}

#[cfg(feature = "http3")]
async fn serve_http3_request_inner(
    endpoint: &quinn::Endpoint,
    handler: Http3ServerHandler,
) -> Result<Http3ServerConnection, Http3Error> {
    let incoming = endpoint.accept().await.ok_or_else(|| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            "HTTP/3 server endpoint is closed",
        )
    })?;
    let connection = incoming.await.map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 server QUIC handshake failed: {error}"),
        )
    })?;
    let quic = h3_quinn::Connection::new(connection);
    let mut connection = h3::server::builder()
        .max_field_section_size(MAX_HTTP_HEADER_BYTES as u64)
        .build::<_, Bytes>(quic)
        .await
        .map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Protocol,
                format!("HTTP/3 server handshake failed: {error}"),
            )
        })?;
    let resolver = connection
        .accept()
        .await
        .map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Protocol,
                format!("HTTP/3 request accept failed: {error}"),
            )
        })?
        .ok_or_else(|| {
            Http3Error::new(
                Http3ErrorKind::Protocol,
                "HTTP/3 connection closed before a request arrived",
            )
        })?;
    let (request, mut stream) = resolver.resolve_request().await.map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Protocol,
            format!("HTTP/3 request headers failed: {error}"),
        )
    })?;
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 request body failed: {error}"),
        )
    })? {
        let remaining = chunk.remaining();
        if body
            .len()
            .checked_add(remaining)
            .is_none_or(|size| size > MAX_HTTP_BODY_BYTES)
        {
            stream.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
            return Err(Http3Error::new(
                Http3ErrorKind::BodyTooLarge,
                format!("HTTP/3 request body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"),
            ));
        }
        body.extend_from_slice(&chunk.copy_to_bytes(remaining));
    }
    let response = handler(Http3ServerRequest { request, body })?;
    validate_http_buffered_body(&response.body).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::BodyTooLarge,
            format!("invalid HTTP/3 response body: {error}"),
        )
    })?;
    validate_http_header_budget(&response.headers).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Invalid,
            format!("invalid HTTP/3 response: {error}"),
        )
    })?;
    let status = http::StatusCode::from_u16(response.status).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Invalid,
            format!(
                "invalid HTTP/3 response status {}: {error}",
                response.status
            ),
        )
    })?;
    let mut builder = http::Response::builder().status(status);
    for (name, value) in response.headers {
        builder = builder.header(name, value);
    }
    let http_response = builder.body(()).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Invalid,
            format!("invalid HTTP/3 response headers: {error}"),
        )
    })?;
    stream.send_response(http_response).await.map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 response headers failed: {error}"),
        )
    })?;
    if !response.body.is_empty() {
        stream
            .send_data(Bytes::from(response.body))
            .await
            .map_err(|error| {
                Http3Error::new(
                    Http3ErrorKind::Transport,
                    format!("HTTP/3 response body failed: {error}"),
                )
            })?;
    }
    stream.finish().await.map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 response completion failed: {error}"),
        )
    })?;
    Ok(connection)
}

#[cfg(feature = "http3")]
fn parse_http3_authority(authority: &str) -> Result<(String, u16), Http3Error> {
    let url = url::Url::parse(&format!("https://{authority}")).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Invalid,
            format!("invalid HTTP/3 authority '{authority}': {error}"),
        )
    })?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Http3Error::new(
            Http3ErrorKind::Invalid,
            "HTTP/3 authority must contain only a host and optional port",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Http3Error::new(Http3ErrorKind::Invalid, "HTTP/3 authority has no host"))?;
    let port = url.port().unwrap_or(443);
    Ok((host.to_string(), port))
}

#[cfg(feature = "http3")]
fn http3_client_config(
    trusted_roots: Option<&[Vec<u8>]>,
) -> Result<quinn::ClientConfig, Http3Error> {
    let roots = if let Some(trusted_roots) = trusted_roots {
        let mut roots = rustls::RootCertStore::empty();
        for root in trusted_roots {
            roots
                .add(rustls::pki_types::CertificateDer::from(root.as_slice()))
                .map_err(|error| {
                    Http3Error::new(
                        Http3ErrorKind::Invalid,
                        format!("invalid HTTP/3 trust anchor: {error}"),
                    )
                })?;
        }
        if roots.is_empty() {
            return Err(Http3Error::new(
                Http3ErrorKind::Invalid,
                "HTTP/3 trust anchors must not be empty",
            ));
        }
        roots
    } else {
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
    };
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 TLS protocol setup failed: {error}"),
        )
    })?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|error| {
        Http3Error::new(
            Http3ErrorKind::Transport,
            format!("HTTP/3 TLS configuration failed: {error}"),
        )
    })?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_uni_streams(16_u8.into());
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[cfg(feature = "http3")]
async fn execute_http3_stream<T>(
    sender: &mut h3::client::SendRequest<T, Bytes>,
    request: http::Request<()>,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
    cancelled: Arc<AtomicBool>,
    dispatched: Arc<AtomicBool>,
) -> Result<Http3Response, Http3Error>
where
    T: h3::quic::OpenStreams<Bytes>,
{
    let operation = async {
        if cancelled.load(Ordering::Acquire) {
            return Err(Http3Error::new(
                Http3ErrorKind::Timeout,
                "HTTP/3 request timed out before dispatch",
            ));
        }
        let mut stream = sender.send_request(request).await.map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Protocol,
                format!("HTTP/3 request headers failed: {error}"),
            )
        })?;
        dispatched.store(true, Ordering::Release);
        if let Some(body) = body {
            stream.send_data(Bytes::from(body)).await.map_err(|error| {
                Http3Error::new(
                    Http3ErrorKind::Transport,
                    format!("HTTP/3 request body failed: {error}"),
                )
            })?;
        }
        stream.finish().await.map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Transport,
                format!("HTTP/3 request completion failed: {error}"),
            )
        })?;
        let response = stream.recv_response().await.map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Protocol,
                format!("HTTP/3 response headers failed: {error}"),
            )
        })?;
        let status = response.status().as_u16();
        let mut headers = Vec::new();
        for (name, value) in response.headers() {
            let value = value.to_str().map_err(|error| {
                Http3Error::new(
                    Http3ErrorKind::Protocol,
                    format!("invalid UTF-8 in HTTP/3 response header '{name}': {error}"),
                )
            })?;
            headers.push((name.as_str().to_ascii_lowercase(), value.to_string()));
        }
        validate_http_header_budget(&headers).map_err(|error| {
            Http3Error::new(Http3ErrorKind::BodyTooLarge, format!("HTTP/3 {error}"))
        })?;
        let mut response_body = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.map_err(|error| {
            Http3Error::new(
                Http3ErrorKind::Transport,
                format!("HTTP/3 response body failed: {error}"),
            )
        })? {
            let remaining = chunk.remaining();
            if response_body
                .len()
                .checked_add(remaining)
                .is_none_or(|size| size > MAX_HTTP_BODY_BYTES)
            {
                stream.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
                return Err(Http3Error::new(
                    Http3ErrorKind::BodyTooLarge,
                    format!("HTTP/3 response body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"),
                ));
            }
            response_body.extend_from_slice(&chunk.copy_to_bytes(remaining));
        }
        Ok::<Http3Response, Http3Error>(Http3Response {
            status,
            headers,
            body: response_body,
        })
    };
    let cancellation = async {
        loop {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::pin!(cancellation);
    match timeout {
        Some(timeout) => {
            tokio::select! {
                result = tokio::time::timeout(timeout, operation) => result
                    .map_err(|_| Http3Error::new(Http3ErrorKind::Timeout, "HTTP/3 request timed out"))?,
                () = &mut cancellation => Err(Http3Error::new(
                    Http3ErrorKind::Timeout,
                    "HTTP/3 request timed out",
                )),
            }
        }
        None => {
            tokio::select! {
                result = operation => result,
                () = &mut cancellation => Err(Http3Error::new(
                    Http3ErrorKind::Timeout,
                    "HTTP/3 request timed out",
                )),
            }
        }
    }
}

#[derive(Clone)]
struct HttpErrorEntry {
    kind: HttpErrorKind,
    detail: String,
    status: i64,
    method: String,
    url: String,
}

/// The stable, package-specific category exposed by `HttpError.kind`.
///
/// This intentionally is not the shared runtime error taxonomy. HTTP callers
/// should be able to compare the field as an enum without depending on a
/// diagnostic string, while the runtime can still map lower-level failures
/// into the small set of categories meaningful to HTTP.
#[derive(Clone, Copy)]
#[repr(i32)]
enum HttpErrorKind {
    Invalid = 0,
    Transport = 1,
    Timeout = 2,
    Resolve = 3,
    Protocol = 4,
    Status = 5,
    Unsupported = 6,
}

impl HttpErrorKind {
    fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid
            | StdErrorKind::Parse
            | StdErrorKind::Range
            | StdErrorKind::Overflow => Self::Invalid,
            StdErrorKind::Timeout => Self::Timeout,
            StdErrorKind::Resolve => Self::Resolve,
            StdErrorKind::Protocol => Self::Protocol,
            StdErrorKind::Status => Self::Status,
            StdErrorKind::Unsupported => Self::Unsupported,
            _ => Self::Transport,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Resolve => "resolve",
            Self::Protocol => "protocol",
            Self::Status => "status",
            Self::Unsupported => "unsupported",
        }
    }
}

struct UdpDatagramEntry {
    payload: Vec<u8>,
    address: String,
    truncated: bool,
    names: usize,
}

type HttpRequestHeaderParts = (String, String, String, Vec<(String, String)>);
type HttpResponseParts = (i64, Vec<(String, String)>, Vec<u8>);
type HttpRouteMatch = (usize, HashMap<String, String>);

type HeaderMap = Mutex<HashMap<i64, HeaderEntry>>;
type HttpRequestMap = Mutex<HashMap<i64, HttpRequestEntry>>;
type HttpResponseMap = Mutex<HashMap<i64, HttpResponseEntry>>;
type HttpServerConfigMap = Mutex<HashMap<i64, HttpServerConfigEntry>>;
type HttpRouterMap = Mutex<HashMap<i64, HttpRouterEntry>>;
type HttpNextMap = Mutex<HashMap<i64, HttpNextEntry>>;
type SseEventMap = Mutex<HashMap<i64, SseEventEntry>>;
type WebSocketFrameMap = Mutex<HashMap<i64, WebSocketFrameEntry>>;
type WebSocketHandshakeMap = Mutex<HashMap<i64, WebSocketHandshakeEntry>>;

const STREAMING_COMMAND_QUEUE_CAPACITY: usize = 8;
const STREAMING_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HTTP_HEARTBEAT_INTERVAL_MS: i64 = 15_000;
const MAX_HTTP_HEARTBEAT_INTERVAL_MS: i64 = 300_000;

#[derive(Clone, Copy)]
enum StreamingHeartbeat {
    Sse,
    WebSocket,
}

enum StreamingSocketCommand {
    Write {
        bytes: Vec<u8>,
        flush: bool,
        response: mpsc::SyncSender<Result<(), String>>,
    },
    ReadWebSocket {
        response: mpsc::SyncSender<Result<WebSocketFrameEntry, String>>,
    },
}

/// Owns the accepted socket for one long-lived server response.
///
/// Mux callbacks stay synchronous. Each handle method submits one bounded
/// command and waits for its result, while this thread is the only code that
/// reads or writes the socket. The shutdown clone only interrupts a blocked
/// system call when another handle closes the actor.
struct StreamingSocketActor {
    commands: mpsc::SyncSender<StreamingSocketCommand>,
    cancelled: Arc<AtomicBool>,
    shutdown_socket: StdTcpStream,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl StreamingSocketActor {
    fn new(
        socket: StdTcpStream,
        timeout_ms: i64,
        heartbeat: Option<(StreamingHeartbeat, Duration)>,
    ) -> Result<Arc<Self>, String> {
        let timeout = if timeout_ms > 0 {
            Duration::from_millis(timeout_ms as u64)
        } else {
            STREAMING_COMMAND_TIMEOUT
        };
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("HTTP streaming read timeout failed: {error}"))?;
        socket
            .set_write_timeout(Some(timeout))
            .map_err(|error| format!("HTTP streaming write timeout failed: {error}"))?;
        let shutdown_socket = socket
            .try_clone()
            .map_err(|error| format!("HTTP streaming socket clone failed: {error}"))?;
        let (commands, receiver) = mpsc::sync_channel(STREAMING_COMMAND_QUEUE_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::Builder::new()
            .name("mux-http-stream".to_string())
            .spawn(move || {
                streaming_socket_actor_loop(socket, receiver, worker_cancelled, heartbeat);
            })
            .map_err(|error| format!("HTTP streaming actor start failed: {error}"))?;
        Ok(Arc::new(Self {
            commands,
            cancelled,
            shutdown_socket,
            worker: Mutex::new(Some(worker)),
        }))
    }

    fn deadline() -> Instant {
        Instant::now() + STREAMING_COMMAND_TIMEOUT
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.shutdown_socket.shutdown(Shutdown::Both);
    }

    fn write(&self, bytes: Vec<u8>, flush: bool) -> Result<(), String> {
        if bytes.len() > MAX_HTTP_BODY_BYTES {
            return Err(format!(
                "HTTP streaming write exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"
            ));
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err("HTTP streaming connection is closed".to_string());
        }
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        let command = StreamingSocketCommand::Write {
            bytes,
            flush,
            response: response_sender,
        };
        self.enqueue(command, Self::deadline())?;
        match response_receiver.recv_timeout(STREAMING_COMMAND_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.cancel();
                Err("HTTP streaming write timed out".to_string())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("HTTP streaming actor stopped".to_string())
            }
        }
    }

    fn read_websocket(&self) -> Result<WebSocketFrameEntry, String> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err("WebSocket connection is closed".to_string());
        }
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.enqueue(
            StreamingSocketCommand::ReadWebSocket {
                response: response_sender,
            },
            Self::deadline(),
        )?;
        match response_receiver.recv_timeout(STREAMING_COMMAND_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.cancel();
                Err("WebSocket receive timed out".to_string())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("HTTP streaming actor stopped".to_string())
            }
        }
    }

    fn enqueue(
        &self,
        mut command: StreamingSocketCommand,
        deadline: Instant,
    ) -> Result<(), String> {
        loop {
            match self.commands.try_send(command) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(next)) => {
                    if self.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                        self.cancel();
                        return Err("HTTP streaming command queue is full".to_string());
                    }
                    command = next;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("HTTP streaming actor stopped".to_string());
                }
            }
        }
    }

    fn close(&self) {
        self.cancel();
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for StreamingSocketActor {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

fn streaming_socket_actor_loop(
    mut socket: StdTcpStream,
    receiver: mpsc::Receiver<StreamingSocketCommand>,
    cancelled: Arc<AtomicBool>,
    heartbeat: Option<(StreamingHeartbeat, Duration)>,
) {
    let mut fragments = None;
    let mut next_heartbeat = heartbeat.map(|(_, interval)| Instant::now() + interval);
    while !cancelled.load(Ordering::Acquire) {
        let wait = streaming_socket_wait(next_heartbeat);
        let command = match receiver.recv_timeout(wait) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(next) =
                    write_due_streaming_heartbeat(&mut socket, heartbeat, next_heartbeat)
                {
                    next_heartbeat = next;
                    continue;
                }
                cancelled.store(true, Ordering::Release);
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        if !handle_streaming_socket_command(&mut socket, &mut fragments, command, &cancelled) {
            break;
        }
    }
}

fn streaming_socket_wait(next_heartbeat: Option<Instant>) -> Duration {
    next_heartbeat
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .map_or(Duration::from_millis(20), |remaining| {
            remaining.min(Duration::from_millis(20))
        })
}

fn write_due_streaming_heartbeat(
    socket: &mut StdTcpStream,
    heartbeat: Option<(StreamingHeartbeat, Duration)>,
    next_heartbeat: Option<Instant>,
) -> Result<Option<Instant>, ()> {
    let Some((kind, interval)) = heartbeat else {
        return Ok(next_heartbeat);
    };
    let Some(deadline) = next_heartbeat else {
        return Ok(None);
    };
    if Instant::now() < deadline {
        return Ok(Some(deadline));
    }

    let bytes = match kind {
        StreamingHeartbeat::Sse => Ok(b": heartbeat\n\n".to_vec()),
        StreamingHeartbeat::WebSocket => encode_websocket_frame(&WebSocketFrameEntry {
            fin: true,
            opcode: 9,
            payload: Vec::new(),
            masked: false,
            names: 1,
        }),
    };
    let bytes = bytes.map_err(|_| ())?;
    socket.write_all(&bytes).map_err(|_| ())?;
    socket.flush().map_err(|_| ())?;
    Ok(Some(deadline + interval))
}

fn handle_streaming_socket_command(
    socket: &mut StdTcpStream,
    fragments: &mut Option<(u8, Vec<u8>)>,
    command: StreamingSocketCommand,
    cancelled: &AtomicBool,
) -> bool {
    match command {
        StreamingSocketCommand::Write {
            bytes,
            flush,
            response,
        } => {
            let result = socket
                .write_all(&bytes)
                .and_then(|()| if flush { socket.flush() } else { Ok(()) })
                .map_err(|error| format!("HTTP streaming write failed: {error}"));
            let failed = result.is_err();
            let _ = response.send(result);
            if failed {
                cancelled.store(true, Ordering::Release);
            }
            !failed
        }
        StreamingSocketCommand::ReadWebSocket { response } => {
            let result = read_websocket_message(socket, fragments.take());
            let failed = result.is_err();
            if failed {
                cancelled.store(true, Ordering::Release);
            }
            let _ = response.send(result);
            !failed
        }
    }
}

struct SseStreamEntry {
    actor: Arc<StreamingSocketActor>,
    names: usize,
}

struct WebSocketSessionEntry {
    actor: Arc<StreamingSocketActor>,
    names: usize,
}

#[derive(Clone)]
struct OAuthRsaKey {
    kid: String,
    modulus: Vec<u8>,
    exponent: Vec<u8>,
}

/// Configuration and discovered endpoints for one OAuth 2.0/OIDC relying
/// party. The client is a synchronous handle. Network calls happen only when
/// a method explicitly performs discovery or a token operation.
#[derive(Clone)]
struct OAuthClientEntry {
    issuer: String,
    client_id: String,
    redirect_uri: String,
    scopes: Vec<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    introspection_endpoint: Option<String>,
    names: usize,
}

/// Tokens held by an OAuth client after a successful token exchange. The
/// session owns its copies and never exposes a secret through diagnostics.
struct OAuthSessionEntry {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_at: Option<Instant>,
    names: usize,
}

type OAuthJwksCache = HashMap<String, (Instant, Vec<OAuthRsaKey>)>;
type OAuthClientMap = Mutex<HashMap<i64, OAuthClientEntry>>;
type OAuthSessionMap = Mutex<HashMap<i64, OAuthSessionEntry>>;

type SseStreamMap = Mutex<HashMap<i64, SseStreamEntry>>;
type WebSocketSessionMap = Mutex<HashMap<i64, WebSocketSessionEntry>>;

static HEADERS: LazyLock<HeaderMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static HTTP_REQUESTS: LazyLock<HttpRequestMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static HTTP_RESPONSES: LazyLock<HttpResponseMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static HTTP_SERVER_CONFIGS: LazyLock<HttpServerConfigMap> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static HTTP_ROUTERS: LazyLock<HttpRouterMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static HTTP_NEXTS: LazyLock<HttpNextMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SSE_EVENTS: LazyLock<SseEventMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WEBSOCKET_FRAMES: LazyLock<WebSocketFrameMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WEBSOCKET_HANDSHAKES: LazyLock<WebSocketHandshakeMap> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SSE_STREAMS: LazyLock<SseStreamMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WEBSOCKET_SESSIONS: LazyLock<WebSocketSessionMap> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OAUTH_JWKS_CACHE: LazyLock<Mutex<OAuthJwksCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OAUTH_CLIENTS: LazyLock<OAuthClientMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static OAUTH_SESSIONS: LazyLock<OAuthSessionMap> = LazyLock::new(|| Mutex::new(HashMap::new()));
static HTTP_ERRORS: LazyLock<Mutex<HashMap<i64, HttpErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static UDP_DATAGRAMS: LazyLock<Mutex<HashMap<i64, UdpDatagramEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(feature = "http2")]
static HTTP2_CONNECTIONS: LazyLock<Mutex<HashMap<String, Weak<Http2ConnectionActor>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static HEADERS_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Headers",
        std::mem::size_of::<i64>(),
        Some(drop_headers as extern "C" fn(*mut c_void)),
        Some(copy_headers as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static HTTP_REQUEST_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "HttpRequest",
        std::mem::size_of::<i64>(),
        Some(drop_http_request as extern "C" fn(*mut c_void)),
        Some(copy_http_request as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static HTTP_RESPONSE_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "HttpResponse",
        std::mem::size_of::<i64>(),
        Some(drop_http_response as extern "C" fn(*mut c_void)),
        Some(copy_http_response as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static HTTP_SERVER_CONFIG_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "HttpServerConfig",
        std::mem::size_of::<i64>(),
        Some(drop_http_server_config as extern "C" fn(*mut c_void)),
        Some(copy_http_server_config as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static HTTP_ROUTER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "HttpRouter",
        std::mem::size_of::<i64>(),
        Some(drop_http_router as extern "C" fn(*mut c_void)),
        Some(copy_http_router as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static HTTP_NEXT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "HttpNext",
        std::mem::size_of::<i64>(),
        Some(drop_http_next as extern "C" fn(*mut c_void)),
        Some(copy_http_next as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static SSE_EVENT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "SseEvent",
        std::mem::size_of::<i64>(),
        Some(drop_sse_event as extern "C" fn(*mut c_void)),
        Some(copy_sse_event as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static WEBSOCKET_FRAME_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "WebSocketFrame",
        std::mem::size_of::<i64>(),
        Some(drop_websocket_frame as extern "C" fn(*mut c_void)),
        Some(copy_websocket_frame as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static WEBSOCKET_HANDSHAKE_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "WebSocketHandshake",
        std::mem::size_of::<i64>(),
        Some(drop_websocket_handshake as extern "C" fn(*mut c_void)),
        Some(copy_websocket_handshake as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static SSE_STREAM_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "SseStream",
        std::mem::size_of::<i64>(),
        Some(drop_sse_stream as extern "C" fn(*mut c_void)),
        Some(copy_sse_stream as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static WEBSOCKET_SESSION_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "WebSocketSession",
        std::mem::size_of::<i64>(),
        Some(drop_websocket_session as extern "C" fn(*mut c_void)),
        Some(copy_websocket_session as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static HTTP_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "HttpError",
        std::mem::size_of::<i64>(),
        Some(drop_http_error as extern "C" fn(*mut c_void)),
    )
});
static OAUTH_CLIENT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "OAuthClient",
        std::mem::size_of::<i64>(),
        Some(drop_oauth_client as extern "C" fn(*mut c_void)),
        Some(copy_oauth_client as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static OAUTH_SESSION_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "OAuthSession",
        std::mem::size_of::<i64>(),
        Some(drop_oauth_session as extern "C" fn(*mut c_void)),
        Some(copy_oauth_session as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

/// A live socket, and how many Mux values name it.
///
/// A socket is a resource, not a value, so copying a `TcpListener` cannot mean
/// opening a second one - both names have to mean the same socket, and the
/// socket closes when the last one goes away. That is the same rule every other
/// heap value in the language follows, so the count lives here rather than
/// leaving a copy to fail (see `copy_socket_handle`).
struct SocketEntry<T> {
    socket: Arc<Mutex<T>>,
    /// Number of Mux values holding this handle. Never zero while in the map:
    /// the entry is removed at the drop that takes it there.
    names: usize,
}

type SocketMap<T> = Mutex<HashMap<i64, SocketEntry<T>>>;

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static NEXT_HTTP_REQUEST_ID: AtomicI64 = AtomicI64::new(1);

static TCP_STREAMS: LazyLock<SocketMap<StdTcpStream>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TCP_LISTENERS: LazyLock<SocketMap<StdTcpListener>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static UDP_SOCKETS: LazyLock<SocketMap<StdUdpSocket>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(unix)]
type LocalStreamNative = StdLocalStream;
#[cfg(windows)]
enum LocalStreamNative {
    Client(PipeClient),
    Server(PipeServer),
}

#[cfg(windows)]
impl Read for LocalStreamNative {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Client(stream) => stream.read(buf),
            Self::Server(stream) => stream.read(buf),
        }
    }
}

#[cfg(windows)]
impl Write for LocalStreamNative {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Client(stream) => stream.write(buf),
            Self::Server(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Client(stream) => stream.flush(),
            Self::Server(stream) => stream.flush(),
        }
    }
}

#[cfg(unix)]
type LocalListenerNative = StdLocalListener;
#[cfg(windows)]
type LocalListenerNative = ConnectingServer;

struct LocalListenerEntry {
    listener: Arc<Mutex<LocalListenerNative>>,
    path: String,
    names: usize,
}

type LocalListenerMap = Mutex<HashMap<i64, LocalListenerEntry>>;

static LOCAL_STREAMS: LazyLock<SocketMap<LocalStreamNative>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LOCAL_LISTENERS: LazyLock<LocalListenerMap> = LazyLock::new(|| Mutex::new(HashMap::new()));

static TCP_STREAM_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "TcpStream",
        std::mem::size_of::<i64>(),
        Some(drop_tcp_stream as extern "C" fn(*mut c_void)),
        Some(copy_tcp_stream as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static TCP_LISTENER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "TcpListener",
        std::mem::size_of::<i64>(),
        Some(drop_tcp_listener as extern "C" fn(*mut c_void)),
        Some(copy_tcp_listener as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static UDP_SOCKET_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "UdpSocket",
        std::mem::size_of::<i64>(),
        Some(drop_udp_socket as extern "C" fn(*mut c_void)),
        Some(copy_udp_socket as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static UDP_DATAGRAM_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "UdpDatagram",
        std::mem::size_of::<i64>(),
        Some(drop_udp_datagram as extern "C" fn(*mut c_void)),
        Some(copy_udp_datagram as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static LOCAL_STREAM_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "LocalStream",
        std::mem::size_of::<i64>(),
        Some(drop_local_stream as extern "C" fn(*mut c_void)),
        Some(copy_local_stream as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static LOCAL_LISTENER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "LocalListener",
        std::mem::size_of::<i64>(),
        Some(drop_local_listener as extern "C" fn(*mut c_void)),
        Some(copy_local_listener as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

extern "C" fn drop_tcp_stream(ptr: *mut c_void) {
    drop_socket_handle(&TCP_STREAMS, ptr);
}

extern "C" fn drop_tcp_listener(ptr: *mut c_void) {
    drop_socket_handle(&TCP_LISTENERS, ptr);
}

extern "C" fn drop_udp_socket(ptr: *mut c_void) {
    drop_socket_handle(&UDP_SOCKETS, ptr);
}

extern "C" fn copy_udp_datagram(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let mut datagrams = UDP_DATAGRAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = datagrams.get_mut(&id) {
        entry.names = entry.names.saturating_add(1);
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_udp_datagram(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut datagrams = UDP_DATAGRAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = datagrams.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        datagrams.remove(&id);
    }
}

extern "C" fn drop_local_stream(ptr: *mut c_void) {
    drop_socket_handle(&LOCAL_STREAMS, ptr);
}

extern "C" fn copy_local_stream(src: *mut c_void, dest: *mut c_void) {
    copy_socket_handle(&LOCAL_STREAMS, src, dest);
}

extern "C" fn drop_local_listener(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    let mut listeners = LOCAL_LISTENERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = listeners.get_mut(&handle).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        if let Some(entry) = listeners.remove(&handle) {
            #[cfg(unix)]
            let _ = std::fs::remove_file(entry.path);
            #[cfg(windows)]
            let _ = entry.path;
        }
    }
}

extern "C" fn copy_local_listener(src: *mut c_void, dest: *mut c_void) {
    if dest.is_null() {
        return;
    }
    let copied = handle_at(src)
        .filter(|handle| {
            let mut listeners = LOCAL_LISTENERS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = listeners.get_mut(handle) {
                entry.names = entry.names.saturating_add(1);
                true
            } else {
                false
            }
        })
        .unwrap_or(0);
    unsafe { *dest.cast::<i64>() = copied };
}

extern "C" fn copy_tcp_stream(src: *mut c_void, dest: *mut c_void) {
    copy_socket_handle(&TCP_STREAMS, src, dest);
}

extern "C" fn copy_tcp_listener(src: *mut c_void, dest: *mut c_void) {
    copy_socket_handle(&TCP_LISTENERS, src, dest);
}

extern "C" fn copy_udp_socket(src: *mut c_void, dest: *mut c_void) {
    copy_socket_handle(&UDP_SOCKETS, src, dest);
}

fn lock_map<T>(map: &SocketMap<T>) -> std::sync::MutexGuard<'_, HashMap<i64, SocketEntry<T>>> {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_headers() -> std::sync::MutexGuard<'static, HashMap<i64, HeaderEntry>> {
    HEADERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_requests() -> std::sync::MutexGuard<'static, HashMap<i64, HttpRequestEntry>> {
    HTTP_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_responses() -> std::sync::MutexGuard<'static, HashMap<i64, HttpResponseEntry>> {
    HTTP_RESPONSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_http_server_configs() -> std::sync::MutexGuard<'static, HashMap<i64, HttpServerConfigEntry>>
{
    HTTP_SERVER_CONFIGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_http_routers() -> std::sync::MutexGuard<'static, HashMap<i64, HttpRouterEntry>> {
    HTTP_ROUTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_http_nexts() -> std::sync::MutexGuard<'static, HashMap<i64, HttpNextEntry>> {
    HTTP_NEXTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_sse_events() -> std::sync::MutexGuard<'static, HashMap<i64, SseEventEntry>> {
    SSE_EVENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_websocket_frames() -> std::sync::MutexGuard<'static, HashMap<i64, WebSocketFrameEntry>> {
    WEBSOCKET_FRAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_websocket_handshakes(
) -> std::sync::MutexGuard<'static, HashMap<i64, WebSocketHandshakeEntry>> {
    WEBSOCKET_HANDSHAKES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_sse_streams() -> std::sync::MutexGuard<'static, HashMap<i64, SseStreamEntry>> {
    SSE_STREAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_websocket_sessions() -> std::sync::MutexGuard<'static, HashMap<i64, WebSocketSessionEntry>>
{
    WEBSOCKET_SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn copy_resource<T>(
    map: &Mutex<HashMap<i64, T>>,
    source: *mut c_void,
    dest: *mut c_void,
    add_ref: impl Fn(&mut T),
) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    let mut entries = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = entries.get_mut(&handle) {
        add_ref(entry);
        unsafe { *dest.cast::<i64>() = handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

fn drop_resource<T>(
    map: &Mutex<HashMap<i64, T>>,
    ptr: *mut c_void,
    names: impl Fn(&mut T) -> bool,
) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    let mut entries = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = entries.get_mut(&handle).is_some_and(names);
    if remove {
        entries.remove(&handle);
    }
}

extern "C" fn copy_headers(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&HEADERS, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_http_request(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&HTTP_REQUESTS, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_http_response(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&HTTP_RESPONSES, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_http_server_config(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&HTTP_SERVER_CONFIGS, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_http_router(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&HTTP_ROUTERS, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_http_next(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&HTTP_NEXTS, source, dest, |entry| {
        entry.names += 1;
        retain_http_router_handle(entry.router);
    });
}

extern "C" fn copy_sse_event(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&SSE_EVENTS, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_websocket_frame(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&WEBSOCKET_FRAMES, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_websocket_handshake(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&WEBSOCKET_HANDSHAKES, source, dest, |entry| {
        entry.names += 1;
    });
}

extern "C" fn copy_sse_stream(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&SSE_STREAMS, source, dest, |entry| {
        entry.names += 1;
    });
}

extern "C" fn copy_websocket_session(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&WEBSOCKET_SESSIONS, source, dest, |entry| {
        entry.names += 1;
    });
}

extern "C" fn copy_oauth_client(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&OAUTH_CLIENTS, source, dest, |entry| entry.names += 1);
}

extern "C" fn copy_oauth_session(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&OAUTH_SESSIONS, source, dest, |entry| entry.names += 1);
}

extern "C" fn drop_headers(ptr: *mut c_void) {
    drop_resource(&HEADERS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_http_request(ptr: *mut c_void) {
    drop_resource(&HTTP_REQUESTS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_http_response(ptr: *mut c_void) {
    drop_resource(&HTTP_RESPONSES, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_http_server_config(ptr: *mut c_void) {
    drop_resource(&HTTP_SERVER_CONFIGS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_http_router(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    let entry = {
        let mut routers = lock_http_routers();
        let remove = routers.get_mut(&handle).is_some_and(|entry| {
            entry.names = entry.names.saturating_sub(1);
            entry.names == 0
        });
        remove.then(|| routers.remove(&handle)).flatten()
    };
    if let Some(entry) = entry {
        let data = entry
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for route in &data.routes {
            unsafe { crate::closure::mux_closure_release(route.handler as *mut c_void) };
        }
        for middleware in &data.middleware {
            if let HttpMiddlewareEntry::Callback(callback) = middleware {
                unsafe { crate::closure::mux_closure_release(*callback as *mut c_void) };
            }
        }
    }
}

extern "C" fn drop_http_next(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    let router = {
        let mut nexts = lock_http_nexts();
        let Some(entry) = nexts.get_mut(&handle) else {
            return;
        };
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            let router = entry.router;
            nexts.remove(&handle);
            Some(router)
        } else {
            None
        }
    };
    if let Some(router) = router {
        release_http_router_handle(router);
    }
}

extern "C" fn drop_sse_event(ptr: *mut c_void) {
    drop_resource(&SSE_EVENTS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_websocket_frame(ptr: *mut c_void) {
    drop_resource(&WEBSOCKET_FRAMES, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_websocket_handshake(ptr: *mut c_void) {
    drop_resource(&WEBSOCKET_HANDSHAKES, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_sse_stream(ptr: *mut c_void) {
    drop_resource(&SSE_STREAMS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            entry.actor.close();
            true
        } else {
            false
        }
    });
}

extern "C" fn drop_websocket_session(ptr: *mut c_void) {
    drop_resource(&WEBSOCKET_SESSIONS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            entry.actor.close();
            true
        } else {
            false
        }
    });
}

extern "C" fn drop_oauth_client(ptr: *mut c_void) {
    drop_resource(&OAUTH_CLIENTS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_oauth_session(ptr: *mut c_void) {
    drop_resource(&OAUTH_SESSIONS, ptr, |entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
}

extern "C" fn drop_http_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        HTTP_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn resource_value(type_id: TypeId, handle: i64) -> *mut Value {
    let value = alloc_object(type_id);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = handle };
    value
}

fn insert_headers(data: Arc<Mutex<HeaderData>>) -> *mut Value {
    let handle = next_handle();
    lock_headers().insert(handle, HeaderEntry { data, names: 1 });
    let value = resource_value(*HEADERS_TYPE_ID, handle);
    if value.is_null() {
        lock_headers().remove(&handle);
    }
    value
}

fn insert_http_request(entry: HttpRequestEntry) -> *mut Value {
    let handle = next_handle();
    lock_requests().insert(handle, entry);
    let value = resource_value(*HTTP_REQUEST_TYPE_ID, handle);
    if value.is_null() {
        lock_requests().remove(&handle);
    }
    value
}

fn insert_http_response(entry: HttpResponseEntry) -> *mut Value {
    let handle = next_handle();
    lock_responses().insert(handle, entry);
    let value = resource_value(*HTTP_RESPONSE_TYPE_ID, handle);
    if value.is_null() {
        lock_responses().remove(&handle);
    }
    value
}

fn insert_http_server_config(entry: HttpServerConfigEntry) -> *mut Value {
    let handle = next_handle();
    lock_http_server_configs().insert(handle, entry);
    let value = resource_value(*HTTP_SERVER_CONFIG_TYPE_ID, handle);
    if value.is_null() {
        lock_http_server_configs().remove(&handle);
    }
    value
}

fn retain_http_router_handle(handle: i64) {
    if let Some(entry) = lock_http_routers().get_mut(&handle) {
        entry.names += 1;
    }
}

fn release_http_router_handle(handle: i64) {
    let entry = {
        let mut routers = lock_http_routers();
        let remove = routers.get_mut(&handle).is_some_and(|entry| {
            entry.names = entry.names.saturating_sub(1);
            entry.names == 0
        });
        remove.then(|| routers.remove(&handle)).flatten()
    };
    if let Some(entry) = entry {
        let data = entry
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for route in &data.routes {
            unsafe { crate::closure::mux_closure_release(route.handler as *mut c_void) };
        }
        for middleware in &data.middleware {
            if let HttpMiddlewareEntry::Callback(callback) = middleware {
                unsafe { crate::closure::mux_closure_release(*callback as *mut c_void) };
            }
        }
    }
}

fn insert_http_router(data: HttpRouterData) -> *mut Value {
    let handle = next_handle();
    lock_http_routers().insert(
        handle,
        HttpRouterEntry {
            data: Arc::new(Mutex::new(data)),
            names: 1,
        },
    );
    let value = resource_value(*HTTP_ROUTER_TYPE_ID, handle);
    if value.is_null() {
        lock_http_routers().remove(&handle);
    }
    value
}

fn insert_oauth_client(entry: OAuthClientEntry) -> *mut Value {
    let handle = next_handle();
    OAUTH_CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(handle, entry);
    let value = resource_value(*OAUTH_CLIENT_TYPE_ID, handle);
    if value.is_null() {
        OAUTH_CLIENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
    value
}

fn insert_oauth_session(entry: OAuthSessionEntry) -> *mut Value {
    let handle = next_handle();
    OAUTH_SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(handle, entry);
    let value = resource_value(*OAUTH_SESSION_TYPE_ID, handle);
    if value.is_null() {
        OAUTH_SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
    value
}

fn insert_http_next(router: i64, stage: usize) -> *mut Value {
    retain_http_router_handle(router);
    let handle = next_handle();
    lock_http_nexts().insert(
        handle,
        HttpNextEntry {
            router,
            stage,
            names: 1,
        },
    );
    let value = resource_value(*HTTP_NEXT_TYPE_ID, handle);
    if value.is_null() {
        lock_http_nexts().remove(&handle);
        release_http_router_handle(router);
    }
    value
}

fn insert_sse_event(entry: SseEventEntry) -> *mut Value {
    let handle = next_handle();
    lock_sse_events().insert(handle, entry);
    let value = resource_value(*SSE_EVENT_TYPE_ID, handle);
    if value.is_null() {
        lock_sse_events().remove(&handle);
    }
    value
}

fn insert_websocket_frame(entry: WebSocketFrameEntry) -> *mut Value {
    let handle = next_handle();
    lock_websocket_frames().insert(handle, entry);
    let value = resource_value(*WEBSOCKET_FRAME_TYPE_ID, handle);
    if value.is_null() {
        lock_websocket_frames().remove(&handle);
    }
    value
}

fn insert_websocket_handshake(entry: WebSocketHandshakeEntry) -> *mut Value {
    let handle = next_handle();
    lock_websocket_handshakes().insert(handle, entry);
    let value = resource_value(*WEBSOCKET_HANDSHAKE_TYPE_ID, handle);
    if value.is_null() {
        lock_websocket_handshakes().remove(&handle);
    }
    value
}

fn insert_sse_stream(actor: Arc<StreamingSocketActor>) -> *mut Value {
    let handle = next_handle();
    lock_sse_streams().insert(handle, SseStreamEntry { actor, names: 1 });
    let value = resource_value(*SSE_STREAM_TYPE_ID, handle);
    if value.is_null() {
        lock_sse_streams().remove(&handle);
    }
    value
}

fn insert_websocket_session(actor: Arc<StreamingSocketActor>) -> *mut Value {
    let handle = next_handle();
    lock_websocket_sessions().insert(handle, WebSocketSessionEntry { actor, names: 1 });
    let value = resource_value(*WEBSOCKET_SESSION_TYPE_ID, handle);
    if value.is_null() {
        lock_websocket_sessions().remove(&handle);
    }
    value
}

fn resource_handle(value: *const Value, type_id: TypeId) -> Option<i64> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn sse_event_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *SSE_EVENT_TYPE_ID)
        .ok_or_else(|| "expected SseEvent value".to_string())
        .and_then(|handle| {
            if lock_sse_events().contains_key(&handle) {
                Ok(handle)
            } else {
                Err("SseEvent value is closed".to_string())
            }
        })
}

fn websocket_frame_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *WEBSOCKET_FRAME_TYPE_ID)
        .ok_or_else(|| "expected WebSocketFrame value".to_string())
        .and_then(|handle| {
            if lock_websocket_frames().contains_key(&handle) {
                Ok(handle)
            } else {
                Err("WebSocketFrame value is closed".to_string())
            }
        })
}

fn websocket_handshake_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *WEBSOCKET_HANDSHAKE_TYPE_ID)
        .ok_or_else(|| "expected WebSocketHandshake value".to_string())
        .and_then(|handle| {
            if lock_websocket_handshakes().contains_key(&handle) {
                Ok(handle)
            } else {
                Err("WebSocketHandshake value is closed".to_string())
            }
        })
}

fn sse_stream_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *SSE_STREAM_TYPE_ID)
        .ok_or_else(|| "expected SseStream value".to_string())
        .and_then(|handle| {
            if lock_sse_streams().contains_key(&handle) {
                Ok(handle)
            } else {
                Err("SseStream value is closed".to_string())
            }
        })
}

fn websocket_session_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *WEBSOCKET_SESSION_TYPE_ID)
        .ok_or_else(|| "expected WebSocketSession value".to_string())
        .and_then(|handle| {
            if lock_websocket_sessions().contains_key(&handle) {
                Ok(handle)
            } else {
                Err("WebSocketSession value is closed".to_string())
            }
        })
}

fn take_resource_value(value: *mut Value) -> Result<Value, String> {
    if value.is_null() {
        return Err("failed to allocate HTTP resource".to_string());
    }
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    Ok(cloned)
}

fn next_handle() -> i64 {
    loop {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
        if handle > 0 {
            return handle;
        }
        // Overflow occurred, atomically reset counter to 1
        let _ = NEXT_HANDLE.compare_exchange(handle, 1, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn insert_socket<T>(map: &SocketMap<T>, socket: T) -> i64 {
    let handle = next_handle();
    lock_map(map).insert(
        handle,
        SocketEntry {
            socket: Arc::new(Mutex::new(socket)),
            names: 1,
        },
    );
    handle
}

/// Close the socket outright, whatever else still names it.
///
/// This is `close()`, an explicit act by the program, so it is deliberately not
/// the reference-counted path: the remaining names get "invalid handle" on
/// their next call, which is the honest answer to using a socket someone closed.
fn remove_socket<T>(map: &SocketMap<T>, handle: i64) {
    lock_map(map).remove(&handle);
}

/// Release one name for `handle`, closing the socket at the last one.
fn release_socket<T>(map: &SocketMap<T>, handle: i64) {
    let mut guard = lock_map(map);
    let Some(entry) = guard.get_mut(&handle) else {
        return;
    };
    entry.names = entry.names.saturating_sub(1);
    if entry.names == 0 {
        guard.remove(&handle);
    }
}

fn get_socket_entry<T>(map: &SocketMap<T>, handle: i64) -> Option<Arc<Mutex<T>>> {
    lock_map(map).get(&handle).map(|entry| entry.socket.clone())
}

/// Read a handle out of an object's data, if it holds a live one.
fn handle_at(ptr: *mut c_void) -> Option<i64> {
    if ptr.is_null() {
        return None;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle == 0 {
        None
    } else {
        Some(handle)
    }
}

fn drop_socket_handle<T>(map: &SocketMap<T>, ptr: *mut c_void) {
    let Some(handle) = handle_at(ptr) else {
        return;
    };
    release_socket(map, handle);
}

/// Copy a socket handle into a second Mux value naming the same socket.
///
/// Without this the type registered no copy callback at all, so `copy_object`
/// returned null and `auto keep = listener` produced a value whose handle was
/// zero - every later call on it answered "invalid tcp listener". A socket
/// cannot be duplicated, so the copy shares it and the count decides when it
/// closes.
///
/// A handle whose entry is already gone copies as zero rather than as a
/// dangling id, so the failure stays "invalid handle" instead of becoming a
/// live socket that the copy does not own.
fn copy_socket_handle<T>(map: &SocketMap<T>, src: *mut c_void, dest: *mut c_void) {
    if dest.is_null() {
        return;
    }
    let copied = handle_at(src)
        .filter(|handle| {
            let mut guard = lock_map(map);
            match guard.get_mut(handle) {
                Some(entry) => {
                    entry.names += 1;
                    true
                }
                None => false,
            }
        })
        .unwrap_or(0);
    unsafe { *dest.cast::<i64>() = copied };
}

fn socket_entry_or_err<T>(
    map: &SocketMap<T>,
    handle: i64,
    label: &str,
) -> Result<Arc<Mutex<T>>, String> {
    get_socket_entry(map, handle).ok_or_else(|| format!("invalid {label} handle"))
}

fn with_socket<T, R, F>(map: &SocketMap<T>, handle: i64, label: &str, op: F) -> Result<R, String>
where
    F: FnOnce(&mut T) -> Result<R, String>,
{
    let entry = socket_entry_or_err(map, handle, label)?;
    let mut guard = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    op(&mut guard)
}

fn with_tcp_stream<R, F>(handle: i64, op: F) -> Result<R, String>
where
    F: FnOnce(&mut StdTcpStream) -> Result<R, String>,
{
    with_socket(&TCP_STREAMS, handle, "tcp stream", op)
}

fn with_udp_socket<R, F>(handle: i64, op: F) -> Result<R, String>
where
    F: FnOnce(&mut StdUdpSocket) -> Result<R, String>,
{
    with_socket(&UDP_SOCKETS, handle, "udp socket", op)
}

fn with_tcp_listener<R, F>(handle: i64, op: F) -> Result<R, String>
where
    F: FnOnce(&mut StdTcpListener) -> Result<R, String>,
{
    with_socket(&TCP_LISTENERS, handle, "tcp listener", op)
}

fn with_local_stream<R, F>(handle: i64, op: F) -> Result<R, String>
where
    F: FnOnce(&mut LocalStreamNative) -> Result<R, String>,
{
    with_socket(&LOCAL_STREAMS, handle, "local stream", op)
}

fn store_tcp_stream(stream: StdTcpStream) -> i64 {
    insert_socket(&TCP_STREAMS, stream)
}

fn store_tcp_listener(listener: StdTcpListener) -> i64 {
    insert_socket(&TCP_LISTENERS, listener)
}

fn store_udp_socket(socket: StdUdpSocket) -> i64 {
    insert_socket(&UDP_SOCKETS, socket)
}

fn store_local_stream(stream: LocalStreamNative) -> i64 {
    insert_socket(&LOCAL_STREAMS, stream)
}

fn store_local_listener(listener: LocalListenerNative, path: String) -> i64 {
    let handle = next_handle();
    LOCAL_LISTENERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            LocalListenerEntry {
                listener: Arc::new(Mutex::new(listener)),
                path,
                names: 1,
            },
        );
    handle
}

fn remove_tcp_stream(handle: i64) {
    remove_socket(&TCP_STREAMS, handle);
}

fn remove_tcp_listener(handle: i64) {
    remove_socket(&TCP_LISTENERS, handle);
}

fn remove_udp_socket(handle: i64) {
    remove_socket(&UDP_SOCKETS, handle);
}

fn remove_local_stream(handle: i64) {
    remove_socket(&LOCAL_STREAMS, handle);
}

fn remove_local_listener(handle: i64) {
    let entry = LOCAL_LISTENERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&handle);
    if let Some(entry) = entry {
        #[cfg(unix)]
        let _ = std::fs::remove_file(entry.path);
        #[cfg(windows)]
        let _ = entry.path;
    }
}

fn socket_handle(value: *const Value, type_id: TypeId) -> Option<i64> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn require_handle(value: *const Value, type_id: TypeId, label: &str) -> Result<i64, String> {
    socket_handle(value, type_id)
        .filter(|&handle| handle != 0)
        .ok_or_else(|| format!("invalid {label}"))
}

fn tcp_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *TCP_STREAM_TYPE_ID, "tcp stream")
}

#[cfg(feature = "net")]
pub(crate) fn clone_tcp_stream(value: *const Value) -> Result<StdTcpStream, String> {
    let handle = tcp_handle(value)?;
    {
        let entry = socket_entry_or_err(&TCP_STREAMS, handle, "tcp stream")?;
        let clone = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_clone();
        clone.map_err(|error| format!("failed to clone tcp stream: {error}"))
    }
}

#[cfg(feature = "net")]
pub(crate) fn clone_tcp_listener(value: *const Value) -> Result<StdTcpListener, String> {
    let handle = tcp_listener_handle(value)?;
    {
        let entry = socket_entry_or_err(&TCP_LISTENERS, handle, "tcp listener")?;
        let clone = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_clone();
        clone.map_err(|error| format!("failed to clone tcp listener: {error}"))
    }
}

#[cfg(feature = "net")]
pub(crate) fn clone_udp_socket(value: *const Value) -> Result<StdUdpSocket, String> {
    let handle = udp_handle(value)?;
    {
        let entry = socket_entry_or_err(&UDP_SOCKETS, handle, "udp socket")?;
        let clone = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_clone();
        clone.map_err(|error| format!("failed to clone udp socket: {error}"))
    }
}

fn udp_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *UDP_SOCKET_TYPE_ID, "udp socket")
}

fn tcp_listener_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *TCP_LISTENER_TYPE_ID, "tcp listener")
}

fn http_server_config_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *HTTP_SERVER_CONFIG_TYPE_ID, "HTTP server config")
}

fn local_stream_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *LOCAL_STREAM_TYPE_ID, "local stream")
}

fn local_listener_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *LOCAL_LISTENER_TYPE_ID, "local listener")
}

// `sockaddr_un.sun_path` includes its terminating NUL on the native Unix
// APIs. Linux reserves 108 bytes (107 bytes for the name), while Darwin's
// `sun_path` is 104 bytes (103 bytes for the name). Keep the validation
// platform-specific so a name accepted on Linux is not handed to macOS only
// to fail during bind/connect.
#[cfg(target_os = "linux")]
const LOCAL_SOCKET_PATH_MAX: usize = 107;
#[cfg(target_os = "macos")]
const LOCAL_SOCKET_PATH_MAX: usize = 103;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const LOCAL_SOCKET_PATH_MAX: usize = 103;

fn write_handle(value: *mut Value, handle: i64) {
    if value.is_null() {
        return;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return;
    }
    unsafe { *ptr.cast::<i64>() = handle };
}

fn local_path(value: *mut Value) -> Result<String, String> {
    let path = value_to_string(value)?;
    if path.is_empty() {
        return Err("local socket path must not be empty".to_string());
    }
    if path.as_bytes().contains(&0) {
        return Err("local socket path must not contain NUL".to_string());
    }
    #[cfg(unix)]
    if path.len() > LOCAL_SOCKET_PATH_MAX {
        return Err(format!(
            "local socket path must be at most {LOCAL_SOCKET_PATH_MAX} bytes"
        ));
    }
    #[cfg(windows)]
    {
        let path = if path.starts_with(r"\\.\pipe\") {
            path
        } else {
            format!(r"\\.\pipe\{path}")
        };
        if path.len() > 32_767 {
            return Err("named pipe path is too long".to_string());
        }
        return Ok(path);
    }
    #[cfg(unix)]
    Ok(path)
}

fn create_socket_value(handle: i64, type_id: TypeId) -> Result<Value, String> {
    let obj_ptr = alloc_object(type_id);
    if obj_ptr.is_null() {
        return Err("could not allocate socket handle".to_string());
    }
    let data_ptr = unsafe { get_object_ptr(obj_ptr) };
    if data_ptr.is_null() {
        unsafe { mux_rc_dec(obj_ptr) };
        return Err("could not initialize socket handle".to_string());
    }
    unsafe { *data_ptr.cast::<i64>() = handle };
    let value = unsafe { (*obj_ptr).clone() };
    unsafe { mux_rc_dec(obj_ptr) };
    Ok(value)
}

fn net_result_socket(handle: i64, type_id: TypeId) -> *mut Value {
    match create_socket_value(handle, type_id) {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

fn value_to_string(value: *mut Value) -> Result<String, String> {
    if value.is_null() {
        return Err("string pointer is null".to_string());
    }
    let val = unsafe { &*value };
    if let Value::String(s) = val {
        Ok(s.clone())
    } else {
        Err("expected string".to_string())
    }
}

fn value_to_bytes(value: *mut Value) -> Result<Vec<u8>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    match unsafe { &*value } {
        Value::Bytes(bytes) => Ok(bytes.clone()),
        _ => Err("expected bytes".to_string()),
    }
}

fn value_to_ipv4(value: *mut Value, label: &str) -> Result<Ipv4Addr, String> {
    let text = value_to_string(value)?;
    text.parse::<Ipv4Addr>()
        .map_err(|error| format!("{label} must be an IPv4 address: {error}"))
}

fn value_to_ipv6(value: *mut Value, label: &str) -> Result<Ipv6Addr, String> {
    let text = value_to_string(value)?;
    text.parse::<Ipv6Addr>()
        .map_err(|error| format!("{label} must be an IPv6 address: {error}"))
}

fn udp_datagram_value(payload: Vec<u8>, address: String, truncated: bool) -> Result<Value, String> {
    let id = next_handle();
    UDP_DATAGRAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            id,
            UdpDatagramEntry {
                payload,
                address,
                truncated,
                names: 1,
            },
        );
    let object = alloc_object(*UDP_DATAGRAM_TYPE_ID);
    if object.is_null() {
        UDP_DATAGRAMS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        return Err("could not allocate UdpDatagram".to_string());
    }
    let data = unsafe { get_object_ptr(object) };
    if data.is_null() {
        unsafe { mux_rc_dec(object) };
        UDP_DATAGRAMS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        return Err("could not initialize UdpDatagram".to_string());
    }
    unsafe { *data.cast::<i64>() = id };
    let Value::Object(reference) = (unsafe { &*object }) else {
        unsafe { mux_rc_dec(object) };
        UDP_DATAGRAMS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        return Err("could not initialize UdpDatagram".to_string());
    };
    let value = Value::Object(reference.clone());
    unsafe { mux_rc_dec(object) };
    Ok(value)
}

fn udp_datagram_handle(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *UDP_DATAGRAM_TYPE_ID {
        return Err("expected UdpDatagram value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("UdpDatagram value is invalid".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if UDP_DATAGRAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&id)
    {
        Ok(id)
    } else {
        Err("UdpDatagram value is closed".to_string())
    }
}

fn udp_datagram_entry(value: *const Value) -> Result<UdpDatagramEntry, String> {
    let id = udp_datagram_handle(value)?;
    UDP_DATAGRAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .map(|entry| UdpDatagramEntry {
            payload: entry.payload.clone(),
            address: entry.address.clone(),
            truncated: entry.truncated,
            names: entry.names,
        })
        .ok_or_else(|| "UdpDatagram value is closed".to_string())
}

fn net_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn net_result_err(msg: String) -> *mut Value {
    // The compiler exposes network fallible operations as `NetError`, so keep
    // the runtime representation at that same typed boundary.  Formatting is
    // deliberately deferred to `NetError.message()`/`to_string()`; no caller
    // should have to recover structure by matching diagnostic text.
    crate::std::net_result_err(msg)
}

fn net_result_err_address(msg: String, address: impl Into<String>) -> *mut Value {
    crate::std::net_result_err_kind_address(StdErrorKind::Io, msg, address)
}

fn stream_result_ok(value: Value) -> *mut Value {
    crate::std::io_result_ok(value)
}

fn stream_result_err(message: impl Into<String>) -> *mut Value {
    crate::std::io_result_err(message.into())
}

fn http_error_value(
    kind: StdErrorKind,
    message: String,
    status: i64,
    method: String,
    url: String,
) -> Value {
    let handle = next_handle();
    HTTP_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            HttpErrorEntry {
                kind: HttpErrorKind::from_std_kind(kind),
                detail: message,
                status,
                method,
                url,
            },
        );
    let value = resource_value(*HTTP_ERROR_TYPE_ID, handle);
    if value.is_null() {
        HTTP_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate HTTP error".to_string());
    }
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

fn http_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn http_result_err(message: String) -> *mut Value {
    http_result_err_with_kind(
        StdErrorKind::Transport,
        message,
        0,
        String::new(),
        String::new(),
    )
}

fn http_result_err_with_context(
    message: String,
    status: i64,
    method: String,
    url: String,
) -> *mut Value {
    http_result_err_with_kind(StdErrorKind::Transport, message, status, method, url)
}

fn http_result_err_with_kind(
    kind: StdErrorKind,
    message: String,
    status: i64,
    method: String,
    url: String,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(http_error_value(
        kind, message, status, method, url,
    )))))
}

#[derive(Clone, Copy)]
enum HttpErrorField {
    Kind,
    Detail,
    Status,
    Method,
    Url,
}

fn http_error_field(error: *const Value, field: HttpErrorField) -> Result<Value, String> {
    let handle = resource_handle(error, *HTTP_ERROR_TYPE_ID)
        .ok_or_else(|| "invalid HttpError handle".to_string())?;
    let errors = HTTP_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = errors
        .get(&handle)
        .ok_or_else(|| "invalid HttpError handle".to_string())?;
    match field {
        HttpErrorField::Kind => Ok(http_error_kind_value(entry.kind)),
        HttpErrorField::Detail => Ok(Value::String(entry.detail.clone())),
        HttpErrorField::Status => Ok(Value::Int(entry.status)),
        HttpErrorField::Method => Ok(Value::String(entry.method.clone())),
        HttpErrorField::Url => Ok(Value::String(entry.url.clone())),
    }
}

fn http_error_text(error: *const Value, decorated: bool) -> String {
    let handle = resource_handle(error, *HTTP_ERROR_TYPE_ID);
    let errors = HTTP_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = handle.and_then(|handle| errors.get(&handle)) else {
        return "invalid HttpError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail.clone()
    }
}

const MAX_SSE_EVENT_BYTES: usize = MAX_HTTP_BODY_BYTES;

fn validate_sse_text(field: &str, value: &str) -> Result<(), String> {
    if value.contains(['\r', '\n']) {
        return Err(format!("SSE {field} must not contain CR or LF"));
    }
    Ok(())
}

fn validate_sse_retry(retry_ms: i64) -> Result<(), String> {
    if !(0..=MAX_HTTP_TIMEOUT_MS).contains(&retry_ms) {
        return Err(format!(
            "SSE retry_ms must be between 0 and {MAX_HTTP_TIMEOUT_MS} milliseconds"
        ));
    }
    Ok(())
}

fn encode_sse_event(entry: &SseEventEntry) -> Result<Vec<u8>, String> {
    validate_sse_text("event", &entry.event)?;
    validate_sse_text("id", &entry.id)?;
    validate_sse_retry(entry.retry_ms)?;

    let normalized = entry.data.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::new();
    if !entry.event.is_empty() {
        output.push_str("event: ");
        output.push_str(&entry.event);
        output.push('\n');
    }
    if !entry.id.is_empty() {
        output.push_str("id: ");
        output.push_str(&entry.id);
        output.push('\n');
    }
    if entry.retry_ms > 0 {
        output.push_str("retry: ");
        output.push_str(&entry.retry_ms.to_string());
        output.push('\n');
    }
    if normalized.is_empty() {
        output.push_str("data:\n");
    } else {
        for line in normalized.split('\n') {
            output.push_str("data: ");
            output.push_str(line);
            output.push('\n');
        }
    }
    output.push('\n');
    if output.len() > MAX_SSE_EVENT_BYTES {
        return Err(format!(
            "encoded SSE event exceeds {MAX_SSE_EVENT_BYTES} bytes"
        ));
    }
    Ok(output.into_bytes())
}

fn sse_event_field_value(event: *const Value, field: SseEventField) -> Result<Value, String> {
    let handle = sse_event_handle(event)?;
    let events = lock_sse_events();
    let entry = events
        .get(&handle)
        .ok_or_else(|| "invalid SseEvent handle".to_string())?;
    match field {
        SseEventField::Event => Ok(Value::String(entry.event.clone())),
        SseEventField::Id => Ok(Value::String(entry.id.clone())),
        SseEventField::RetryMs => Ok(Value::Int(entry.retry_ms)),
        SseEventField::Data => Ok(Value::String(entry.data.clone())),
    }
}

#[derive(Clone, Copy)]
enum SseEventField {
    Event,
    Id,
    RetryMs,
    Data,
}

fn set_sse_event_field(
    event: *const Value,
    field: SseEventField,
    value: *const Value,
) -> Result<(), String> {
    let handle = sse_event_handle(event)?;
    let value =
        unsafe { value.as_ref() }.ok_or_else(|| "SSE field value is missing".to_string())?;
    let mut events = lock_sse_events();
    let entry = events
        .get_mut(&handle)
        .ok_or_else(|| "invalid SseEvent handle".to_string())?;
    match field {
        SseEventField::Event => {
            let Value::String(value) = value else {
                return Err("SSE event must be a string".to_string());
            };
            validate_sse_text("event", value)?;
            entry.event = value.clone();
        }
        SseEventField::Id => {
            let Value::String(value) = value else {
                return Err("SSE id must be a string".to_string());
            };
            validate_sse_text("id", value)?;
            entry.id = value.clone();
        }
        SseEventField::RetryMs => {
            let Value::Int(value) = value else {
                return Err("SSE retry_ms must be an int".to_string());
            };
            validate_sse_retry(*value)?;
            entry.retry_ms = *value;
        }
        SseEventField::Data => {
            let Value::String(value) = value else {
                return Err("SSE data must be a string".to_string());
            };
            entry.data = value.clone();
        }
    }
    Ok(())
}

/// Create an empty event. Callers assign fields directly, then call `encode`
/// to produce one standards-compliant SSE event as bytes.
#[unsafe(no_mangle)]
pub extern "C" fn mux_net_sse_event_new() -> *mut Value {
    insert_sse_event(SseEventEntry {
        event: String::new(),
        id: String::new(),
        retry_ms: 0,
        data: String::new(),
        names: 1,
    })
}

/// Build an event from explicit values. `retry_ms = 0` omits the retry field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_sse_event_from_config(
    event: *const Value,
    id: *const Value,
    retry_ms: i64,
    data: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(event)) = event.as_ref() else {
            return Err("SSE event must be a string".to_string());
        };
        let Some(Value::String(id)) = id.as_ref() else {
            return Err("SSE id must be a string".to_string());
        };
        let Some(Value::String(data)) = data.as_ref() else {
            return Err("SSE data must be a string".to_string());
        };
        validate_sse_text("event", event)?;
        validate_sse_text("id", id)?;
        validate_sse_retry(retry_ms)?;
        let entry = SseEventEntry {
            event: event.clone(),
            id: id.clone(),
            retry_ms,
            data: data.clone(),
            names: 1,
        };
        encode_sse_event(&entry)?;
        take_resource_value(insert_sse_event(entry))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Encode one event into a bounded bytes value for use as an HTTP response
/// body. The caller controls the response status and headers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_sse_event_encode(event: *const Value) -> *mut Value {
    let result = sse_event_handle(event).and_then(|handle| {
        let events = lock_sse_events();
        let entry = events
            .get(&handle)
            .ok_or_else(|| "invalid SseEvent handle".to_string())?;
        encode_sse_event(entry)
    });
    match result {
        Ok(value) => http_result_ok(Value::Bytes(value)),
        Err(error) => http_result_err(error),
    }
}

macro_rules! sse_event_accessors {
    ($getter:ident, $setter:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $getter(event: *const Value) -> *mut Value {
            mux_rc_alloc(sse_event_field_value(event, $field).unwrap_or(Value::Unit))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $setter(event: *const Value, value: *const Value) -> *mut Value {
            http_result_unit(set_sse_event_field(event, $field, value))
        }
    };
}

sse_event_accessors!(
    mux_net_sse_event_event,
    mux_net_sse_event_set_event,
    SseEventField::Event
);
sse_event_accessors!(
    mux_net_sse_event_id,
    mux_net_sse_event_set_id,
    SseEventField::Id
);
sse_event_accessors!(
    mux_net_sse_event_retry_ms,
    mux_net_sse_event_set_retry_ms,
    SseEventField::RetryMs
);
sse_event_accessors!(
    mux_net_sse_event_data,
    mux_net_sse_event_set_data,
    SseEventField::Data
);

const MAX_WEBSOCKET_FRAME_BYTES: usize = 16 * 1024 * 1024;
const WEBSOCKET_ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn validate_websocket_opcode(opcode: i64) -> Result<u8, String> {
    let opcode =
        u8::try_from(opcode).map_err(|_| "WebSocket opcode must fit in a byte".to_string())?;
    if !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) {
        return Err(format!("unsupported WebSocket opcode {opcode}"));
    }
    Ok(opcode)
}

fn validate_websocket_frame(entry: &WebSocketFrameEntry) -> Result<u8, String> {
    let opcode = validate_websocket_opcode(entry.opcode)?;
    if entry.payload.len() > MAX_WEBSOCKET_FRAME_BYTES {
        return Err(format!(
            "WebSocket payload exceeds {MAX_WEBSOCKET_FRAME_BYTES} bytes"
        ));
    }
    if opcode >= 8 {
        if !entry.fin {
            return Err("WebSocket control frames must have FIN set".to_string());
        }
        if entry.payload.len() > 125 {
            return Err("WebSocket control frame payload exceeds 125 bytes".to_string());
        }
    }
    // UTF-8 is a message-level property. A non-final text fragment may split
    // a code point across continuation frames, so validate it only once the
    // complete data message has been assembled.
    if opcode == 1 && entry.fin && std::str::from_utf8(&entry.payload).is_err() {
        return Err("WebSocket text payload must be valid UTF-8".to_string());
    }
    if opcode == 8 && entry.payload.len() == 1 {
        return Err("WebSocket close payload cannot contain one byte".to_string());
    }
    Ok(opcode)
}

fn encode_websocket_frame(entry: &WebSocketFrameEntry) -> Result<Vec<u8>, String> {
    let opcode = validate_websocket_frame(entry)?;
    let payload_len = entry.payload.len();
    let mut output = Vec::with_capacity(payload_len + 14);
    output.push((u8::from(entry.fin) << 7) | opcode);
    let mask_bit = if entry.masked { 0x80 } else { 0 };
    match payload_len {
        0..=125 => output.push(mask_bit | payload_len as u8),
        126..=65_535 => {
            output.push(mask_bit | 126);
            output.extend_from_slice(&(payload_len as u16).to_be_bytes());
        }
        _ => {
            output.push(mask_bit | 127);
            output.extend_from_slice(&(payload_len as u64).to_be_bytes());
        }
    }
    let mut payload = entry.payload.clone();
    if entry.masked {
        let mut key = [0_u8; 4];
        getrandom::fill(&mut key)
            .map_err(|error| format!("could not generate WebSocket mask: {error}"))?;
        output.extend_from_slice(&key);
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[index % key.len()];
        }
    }
    output.extend_from_slice(&payload);
    Ok(output)
}

fn decode_websocket_frame(bytes: &[u8]) -> Result<WebSocketFrameEntry, String> {
    if bytes.len() < 2 {
        return Err("WebSocket frame is truncated".to_string());
    }
    let first = bytes[0];
    if first & 0x70 != 0 {
        return Err("WebSocket reserved bits are not supported".to_string());
    }
    let fin = first & 0x80 != 0;
    let opcode = i64::from(first & 0x0f);
    validate_websocket_opcode(opcode)?;
    let second = bytes[1];
    let masked = second & 0x80 != 0;
    let length_code = second & 0x7f;
    let mut offset = 2;
    let payload_len = match length_code {
        0..=125 => usize::from(length_code),
        126 => {
            if bytes.len() < offset + 2 {
                return Err("WebSocket frame length is truncated".to_string());
            }
            let length = usize::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
            offset += 2;
            if length < 126 {
                return Err("WebSocket frame uses a non-canonical length".to_string());
            }
            length
        }
        127 => {
            if bytes.len() < offset + 8 {
                return Err("WebSocket frame length is truncated".to_string());
            }
            let length = u64::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
            offset += 8;
            if length & (1_u64 << 63) != 0 {
                return Err("WebSocket frame length has its high bit set".to_string());
            }
            usize::try_from(length)
                .map_err(|_| "WebSocket frame length does not fit in this platform".to_string())?
        }
        _ => return Err("WebSocket frame length code is invalid".to_string()),
    };
    if payload_len > MAX_WEBSOCKET_FRAME_BYTES {
        return Err(format!(
            "WebSocket payload exceeds {MAX_WEBSOCKET_FRAME_BYTES} bytes"
        ));
    }
    let mask_len = if masked { 4 } else { 0 };
    let payload_end = offset
        .checked_add(mask_len)
        .and_then(|value| value.checked_add(payload_len))
        .ok_or_else(|| "WebSocket frame length overflows".to_string())?;
    if bytes.len() != payload_end {
        return Err("WebSocket frame contains trailing or truncated bytes".to_string());
    }
    let key = if masked {
        let key = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        offset += 4;
        key
    } else {
        [0; 4]
    };
    let mut payload = bytes[offset..].to_vec();
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[index % key.len()];
        }
    }
    let entry = WebSocketFrameEntry {
        fin,
        opcode,
        payload,
        masked,
        names: 1,
    };
    validate_websocket_frame(&entry)?;
    Ok(entry)
}

/// Reassemble one complete RFC 6455 data message from its decoded frames.
///
/// This is deliberately a bounded value operation rather than a connection
/// loop. Control frames may appear between data fragments and are validated
/// but are not included in the returned data frame. The caller can then use
/// the existing frame encoder to emit the complete message.
fn reassemble_websocket_frames(values: &[Value]) -> Result<WebSocketFrameEntry, String> {
    if values.is_empty() {
        return Err("WebSocket message must contain at least one frame".to_string());
    }

    let entries = websocket_frame_entries(values)?;

    let first = &entries[0];
    let first_opcode = validate_websocket_opcode(first.opcode)?;
    if !matches!(first_opcode, 1 | 2) {
        return Err("WebSocket message must start with a text or binary frame".to_string());
    }

    // A final data frame is already a complete message. Do not consume
    // another frame here: the caller may be holding the beginning of the next
    // message in the same decoded batch.
    if first.fin {
        if entries.len() != 1 {
            return Err("WebSocket complete message has trailing frames".to_string());
        }
        return Ok(WebSocketFrameEntry {
            fin: true,
            opcode: first.opcode,
            payload: first.payload.clone(),
            masked: first.masked,
            names: 1,
        });
    }

    let mut payload = first.payload.clone();
    let mut total = first.payload.len();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(output) =
            append_websocket_fragment(first, entry, index, entries.len(), &mut payload, &mut total)?
        {
            return Ok(output);
        }
    }

    Err("WebSocket fragmented message is missing a final continuation".to_string())
}

fn websocket_frame_entries(values: &[Value]) -> Result<Vec<WebSocketFrameEntry>, String> {
    values
        .iter()
        .map(|value| {
            let handle = websocket_frame_handle(value)?;
            let frames = lock_websocket_frames();
            let entry = frames
                .get(&handle)
                .ok_or_else(|| "invalid WebSocketFrame handle".to_string())?;
            let entry = WebSocketFrameEntry {
                fin: entry.fin,
                opcode: entry.opcode,
                payload: entry.payload.clone(),
                masked: entry.masked,
                names: 1,
            };
            validate_websocket_frame(&entry)?;
            Ok(entry)
        })
        .collect()
}

fn append_websocket_fragment(
    first: &WebSocketFrameEntry,
    entry: &WebSocketFrameEntry,
    index: usize,
    entry_count: usize,
    payload: &mut Vec<u8>,
    total: &mut usize,
) -> Result<Option<WebSocketFrameEntry>, String> {
    let opcode = validate_websocket_opcode(entry.opcode)?;
    if index == 0 {
        if entry.fin {
            return Err("WebSocket fragmented message starts as final".to_string());
        }
        return Ok(None);
    }
    if entry.masked != first.masked {
        return Err("WebSocket message frames must use one masking state".to_string());
    }
    if opcode >= 8 {
        return Ok(None);
    }
    if opcode != 0 {
        return Err("WebSocket fragmented message contains a new data frame".to_string());
    }
    *total = total
        .checked_add(entry.payload.len())
        .ok_or_else(|| "WebSocket message payload length overflows".to_string())?;
    if *total > MAX_WEBSOCKET_FRAME_BYTES {
        return Err(format!(
            "WebSocket message payload exceeds {MAX_WEBSOCKET_FRAME_BYTES} bytes"
        ));
    }
    payload.extend_from_slice(&entry.payload);
    if !entry.fin {
        return Ok(None);
    }
    if index + 1 != entry_count {
        return Err("WebSocket complete message has trailing frames".to_string());
    }
    let output = WebSocketFrameEntry {
        fin: true,
        opcode: first.opcode,
        payload: std::mem::take(payload),
        masked: first.masked,
        names: 1,
    };
    validate_websocket_frame(&output)?;
    Ok(Some(output))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_frame_reassemble(frames: *const Value) -> *mut Value {
    let result = (|| {
        let Some(Value::List(values)) = frames.as_ref() else {
            return Err("WebSocket message frames must be a list".to_string());
        };
        let entry = reassemble_websocket_frames(values)?;
        take_resource_value(insert_websocket_frame(entry))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

fn websocket_frame_field_value(
    frame: *const Value,
    field: WebSocketFrameField,
) -> Result<Value, String> {
    let handle = websocket_frame_handle(frame)?;
    let frames = lock_websocket_frames();
    let entry = frames
        .get(&handle)
        .ok_or_else(|| "invalid WebSocketFrame handle".to_string())?;
    Ok(match field {
        WebSocketFrameField::Fin => Value::Bool(entry.fin),
        WebSocketFrameField::Opcode => Value::Int(entry.opcode),
        WebSocketFrameField::Payload => Value::Bytes(entry.payload.clone()),
        WebSocketFrameField::Masked => Value::Bool(entry.masked),
    })
}

#[derive(Clone, Copy)]
enum WebSocketFrameField {
    Fin,
    Opcode,
    Payload,
    Masked,
}

fn set_websocket_frame_field(
    frame: *const Value,
    field: WebSocketFrameField,
    value: *const Value,
) -> Result<(), String> {
    let handle = websocket_frame_handle(frame)?;
    let value =
        unsafe { value.as_ref() }.ok_or_else(|| "WebSocket field value is missing".to_string())?;
    let mut frames = lock_websocket_frames();
    let entry = frames
        .get_mut(&handle)
        .ok_or_else(|| "invalid WebSocketFrame handle".to_string())?;
    match field {
        WebSocketFrameField::Fin | WebSocketFrameField::Masked => {
            let Value::Bool(value) = value else {
                return Err("WebSocket fin/masked fields must be bools".to_string());
            };
            if matches!(field, WebSocketFrameField::Fin) {
                entry.fin = *value;
            } else {
                entry.masked = *value;
            }
        }
        WebSocketFrameField::Opcode => {
            let Value::Int(value) = value else {
                return Err("WebSocket opcode must be an int".to_string());
            };
            validate_websocket_opcode(*value)?;
            entry.opcode = *value;
        }
        WebSocketFrameField::Payload => {
            let Value::Bytes(value) = value else {
                return Err("WebSocket payload must be bytes".to_string());
            };
            entry.payload = value.clone();
        }
    }
    validate_websocket_frame(entry).map(|_| ())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_net_websocket_frame_new() -> *mut Value {
    insert_websocket_frame(WebSocketFrameEntry {
        fin: true,
        opcode: 1,
        payload: Vec::new(),
        masked: false,
        names: 1,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_frame_from_config(
    fin: i32,
    opcode: i64,
    payload: *const Value,
    masked: i32,
) -> *mut Value {
    let result = (|| {
        let Some(Value::Bytes(payload)) = payload.as_ref() else {
            return Err("WebSocket payload must be bytes".to_string());
        };
        let entry = WebSocketFrameEntry {
            fin: fin != 0,
            opcode,
            payload: payload.clone(),
            masked: masked != 0,
            names: 1,
        };
        validate_websocket_frame(&entry)?;
        take_resource_value(insert_websocket_frame(entry))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_frame_encode(frame: *const Value) -> *mut Value {
    match websocket_frame_handle(frame).and_then(|handle| {
        lock_websocket_frames()
            .get(&handle)
            .map(encode_websocket_frame)
            .ok_or_else(|| "invalid WebSocketFrame handle".to_string())?
    }) {
        Ok(value) => http_result_ok(Value::Bytes(value)),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_frame_decode(bytes: *const Value) -> *mut Value {
    let result = (|| {
        let Some(Value::Bytes(bytes)) = bytes.as_ref() else {
            return Err("WebSocket frame input must be bytes".to_string());
        };
        let entry = decode_websocket_frame(bytes)?;
        take_resource_value(insert_websocket_frame(entry))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

macro_rules! websocket_frame_accessors {
    ($getter:ident, $setter:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $getter(frame: *const Value) -> *mut Value {
            mux_rc_alloc(websocket_frame_field_value(frame, $field).unwrap_or(Value::Unit))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $setter(frame: *const Value, value: *const Value) -> *mut Value {
            http_result_unit(set_websocket_frame_field(frame, $field, value))
        }
    };
}

websocket_frame_accessors!(
    mux_net_websocket_frame_fin,
    mux_net_websocket_frame_set_fin,
    WebSocketFrameField::Fin
);
websocket_frame_accessors!(
    mux_net_websocket_frame_opcode,
    mux_net_websocket_frame_set_opcode,
    WebSocketFrameField::Opcode
);
websocket_frame_accessors!(
    mux_net_websocket_frame_payload,
    mux_net_websocket_frame_set_payload,
    WebSocketFrameField::Payload
);
websocket_frame_accessors!(
    mux_net_websocket_frame_masked,
    mux_net_websocket_frame_set_masked,
    WebSocketFrameField::Masked
);

fn websocket_accept_key(key: &str) -> Result<String, String> {
    let decoded = BASE64_STANDARD
        .decode(key.as_bytes())
        .map_err(|_| "WebSocket key must be standard base64".to_string())?;
    if decoded.len() != 16 {
        return Err("WebSocket key must decode to exactly 16 bytes".to_string());
    }
    // WebSocket RFC 6455 requires SHA-1 for this non-secret handshake value.
    let mut hasher = Sha1::new(); // NOSONAR
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_ACCEPT_GUID.as_bytes());
    Ok(BASE64_STANDARD.encode(hasher.finalize()))
}

fn websocket_handshake_field_value(
    handshake: *const Value,
    field: WebSocketHandshakeField,
) -> Result<Value, String> {
    let handle = websocket_handshake_handle(handshake)?;
    let handshakes = lock_websocket_handshakes();
    let entry = handshakes
        .get(&handle)
        .ok_or_else(|| "invalid WebSocketHandshake handle".to_string())?;
    Ok(match field {
        WebSocketHandshakeField::Key => Value::String(entry.key.clone()),
        WebSocketHandshakeField::Protocol => Value::String(entry.protocol.clone()),
    })
}

#[derive(Clone, Copy)]
enum WebSocketHandshakeField {
    Key,
    Protocol,
}

fn set_websocket_handshake_field(
    handshake: *const Value,
    field: WebSocketHandshakeField,
    value: *const Value,
) -> Result<(), String> {
    let handle = websocket_handshake_handle(handshake)?;
    let Some(Value::String(value)) = (unsafe { value.as_ref() }) else {
        return Err("WebSocket handshake fields must be strings".to_string());
    };
    if value.contains(['\r', '\n']) {
        return Err("WebSocket handshake fields must not contain CR or LF".to_string());
    }
    let mut handshakes = lock_websocket_handshakes();
    let entry = handshakes
        .get_mut(&handle)
        .ok_or_else(|| "invalid WebSocketHandshake handle".to_string())?;
    match field {
        WebSocketHandshakeField::Key => entry.key = value.clone(),
        WebSocketHandshakeField::Protocol => entry.protocol = value.clone(),
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_net_websocket_handshake_new() -> *mut Value {
    insert_websocket_handshake(WebSocketHandshakeEntry {
        key: String::new(),
        protocol: String::new(),
        names: 1,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_handshake_from_config(
    key: *const Value,
    protocol: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(key)) = key.as_ref() else {
            return Err("WebSocket key must be a string".to_string());
        };
        let Some(Value::String(protocol)) = protocol.as_ref() else {
            return Err("WebSocket protocol must be a string".to_string());
        };
        if key.contains(['\r', '\n']) || protocol.contains(['\r', '\n']) {
            return Err("WebSocket handshake fields must not contain CR or LF".to_string());
        }
        websocket_accept_key(key)?;
        take_resource_value(insert_websocket_handshake(WebSocketHandshakeEntry {
            key: key.clone(),
            protocol: protocol.clone(),
            names: 1,
        }))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_net_websocket_handshake_request_key() -> *mut Value {
    let result = (|| {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("could not generate WebSocket key: {error}"))?;
        Ok(BASE64_STANDARD.encode(bytes))
    })();
    match result {
        Ok(value) => http_result_ok(Value::String(value)),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_handshake_accept_key(
    handshake: *const Value,
) -> *mut Value {
    let result = websocket_handshake_handle(handshake).and_then(|handle| {
        let key = lock_websocket_handshakes()
            .get(&handle)
            .map(|entry| entry.key.clone())
            .ok_or_else(|| "invalid WebSocketHandshake handle".to_string())?;
        websocket_accept_key(&key)
    });
    match result {
        Ok(value) => http_result_ok(Value::String(value)),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_handshake_response_headers(
    handshake: *const Value,
) -> *mut Value {
    let result = websocket_handshake_handle(handshake).and_then(|handle| {
        let entry = lock_websocket_handshakes()
            .get(&handle)
            .map(|entry| (entry.key.clone(), entry.protocol.clone()))
            .ok_or_else(|| "invalid WebSocketHandshake handle".to_string())?;
        let accept = websocket_accept_key(&entry.0)?;
        let mut values = vec![
            ("upgrade".to_string(), "websocket".to_string()),
            ("connection".to_string(), "Upgrade".to_string()),
            ("sec-websocket-accept".to_string(), accept),
        ];
        if !entry.1.is_empty() {
            values.push(("sec-websocket-protocol".to_string(), entry.1));
        }
        take_resource_value(insert_headers(Arc::new(Mutex::new(HeaderData { values }))))
    });
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

macro_rules! websocket_handshake_accessors {
    ($getter:ident, $setter:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $getter(handshake: *const Value) -> *mut Value {
            mux_rc_alloc(websocket_handshake_field_value(handshake, $field).unwrap_or(Value::Unit))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $setter(
            handshake: *const Value,
            value: *const Value,
        ) -> *mut Value {
            http_result_unit(set_websocket_handshake_field(handshake, $field, value))
        }
    };
}

websocket_handshake_accessors!(
    mux_net_websocket_handshake_key,
    mux_net_websocket_handshake_set_key,
    WebSocketHandshakeField::Key
);
websocket_handshake_accessors!(
    mux_net_websocket_handshake_protocol,
    mux_net_websocket_handshake_set_protocol,
    WebSocketHandshakeField::Protocol
);

fn close_sse_stream_handle(handle: i64) {
    if let Some(entry) = lock_sse_streams().remove(&handle) {
        entry.actor.close();
    }
}

fn close_websocket_session_handle(handle: i64) {
    if let Some(entry) = lock_websocket_sessions().remove(&handle) {
        entry.actor.close();
    }
}

fn streaming_handler_result(result: *mut Value) -> Result<(), String> {
    let outcome = unsafe { result.as_ref() }
        .ok_or_else(|| "HTTP streaming handler returned null".to_string())?;
    let error = match outcome {
        Value::Result(Ok(_)) => None,
        Value::Result(Err(error)) => Some(http_error_text(error.as_ref(), true)),
        _ => Some("HTTP streaming handler must return result<Unit, HttpError>".to_string()),
    };
    unsafe { mux_rc_dec(result) };
    error.map_or(Ok(()), Err)
}

unsafe fn invoke_streaming_handler(
    handler: *mut c_void,
    request: *mut Value,
    session: *mut Value,
) -> Result<(), String> {
    let result = unsafe { invoke_http_callback(handler, &[request, session]) }?;
    streaming_handler_result(result)
}

fn http_request_method(request: *const Value) -> Result<String, String> {
    request_handle(request).and_then(|handle| {
        lock_requests()
            .get(&handle)
            .map(|entry| entry.method.clone())
            .ok_or_else(|| "invalid HttpRequest handle".to_string())
    })
}

fn http_request_id_value(request: *const Value) -> String {
    request_handle(request)
        .ok()
        .and_then(|handle| {
            lock_requests()
                .get(&handle)
                .map(|entry| entry.request_id.clone())
        })
        .unwrap_or_default()
}

fn request_has_header_token(request: *const Value, name: &str, expected: &str) -> bool {
    http_request_header(request, name).is_some_and(|value| {
        value
            .split(',')
            .map(str::trim)
            .any(|token| token.eq_ignore_ascii_case(expected))
    })
}

fn write_streaming_rejection(
    stream: &mut StdTcpStream,
    request: *const Value,
    status: i64,
    body: &[u8],
) -> Result<(), String> {
    let headers = response_headers_with_request_id(
        vec![(
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        )],
        &http_request_id_value(request),
    );
    write_typed_http_response_for_request(
        stream,
        status,
        &headers,
        body,
        http_request_method(request).ok().as_deref(),
    )
}

fn sse_headers(request: *const Value, limits: &HttpServerLimits) -> Vec<(String, String)> {
    let mut headers = vec![
        ("cache-control".to_string(), "no-cache".to_string()),
        ("content-type".to_string(), "text/event-stream".to_string()),
        ("connection".to_string(), "keep-alive".to_string()),
    ];
    headers = cors_response_headers(headers, request, limits);
    response_headers_with_request_id(headers, &http_request_id_value(request))
}

fn websocket_upgrade_is_valid(request: *const Value) -> Result<String, String> {
    if !http_request_method(request)?.eq_ignore_ascii_case("GET") {
        return Err("WebSocket handshake requires GET".to_string());
    }
    if !request_has_header_token(request, "upgrade", "websocket") {
        return Err("WebSocket handshake is missing Upgrade: websocket".to_string());
    }
    if !request_has_header_token(request, "connection", "upgrade") {
        return Err("WebSocket handshake is missing Connection: Upgrade".to_string());
    }
    if http_request_header(request, "sec-websocket-version").as_deref() != Some("13") {
        return Err("WebSocket handshake requires version 13".to_string());
    }
    let key = http_request_header(request, "sec-websocket-key")
        .ok_or_else(|| "WebSocket handshake is missing Sec-WebSocket-Key".to_string())?;
    websocket_accept_key(&key)?;
    Ok(key)
}

fn websocket_headers(
    request: *const Value,
    limits: &HttpServerLimits,
    accept: String,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("upgrade".to_string(), "websocket".to_string()),
        ("connection".to_string(), "Upgrade".to_string()),
        ("sec-websocket-accept".to_string(), accept),
    ];
    headers = cors_response_headers(headers, request, limits);
    response_headers_with_request_id(headers, &http_request_id_value(request))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_sse_stream_from_tcp(stream: *mut Value) -> *mut Value {
    let result = (|| {
        let tcp_handle = tcp_handle(stream)?;
        let socket = with_tcp_stream(tcp_handle, |socket| {
            socket
                .try_clone()
                .map_err(|error| format!("SSE stream socket clone failed: {error}"))
        })?;
        let actor = StreamingSocketActor::new(socket, 0, None)?;
        take_resource_value(insert_sse_stream(actor))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_sse_stream_send(
    stream: *const Value,
    event: *const Value,
) -> *mut Value {
    let result = (|| {
        let stream_handle = sse_stream_handle(stream)?;
        let event_handle = sse_event_handle(event)?;
        let bytes = {
            let events = lock_sse_events();
            let entry = events
                .get(&event_handle)
                .ok_or_else(|| "invalid SseEvent handle".to_string())?;
            encode_sse_event(entry)?
        };
        let actor = lock_sse_streams()
            .get(&stream_handle)
            .map(|entry| Arc::clone(&entry.actor))
            .ok_or_else(|| "invalid SseStream handle".to_string())?;
        actor.write(bytes, false)
    })();
    http_result_unit(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_sse_stream_flush(stream: *const Value) -> *mut Value {
    let result = sse_stream_handle(stream).and_then(|handle| {
        let actor = lock_sse_streams()
            .get(&handle)
            .map(|entry| Arc::clone(&entry.actor))
            .ok_or_else(|| "invalid SseStream handle".to_string())?;
        actor.write(Vec::new(), true)
    });
    http_result_unit(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_sse_stream_close(stream: *mut Value) {
    if let Ok(handle) = sse_stream_handle(stream) {
        close_sse_stream_handle(handle);
        write_handle(stream, 0);
    }
}

fn read_websocket_frame(socket: &mut StdTcpStream) -> Result<WebSocketFrameEntry, String> {
    let mut header = [0_u8; 2];
    socket
        .read_exact(&mut header)
        .map_err(|error| format!("WebSocket read failed: {error}"))?;
    let first = header[0];
    if first & 0x70 != 0 {
        return Err("WebSocket reserved bits are not supported".to_string());
    }
    let fin = first & 0x80 != 0;
    let opcode = i64::from(first & 0x0f);
    validate_websocket_opcode(opcode)?;
    let masked = header[1] & 0x80 != 0;
    if !masked {
        return Err("client WebSocket frames must be masked".to_string());
    }
    let length_code = header[1] & 0x7f;
    let payload_len = match length_code {
        0..=125 => usize::from(length_code),
        126 => {
            let mut bytes = [0_u8; 2];
            socket
                .read_exact(&mut bytes)
                .map_err(|error| format!("WebSocket length read failed: {error}"))?;
            let length = usize::from(u16::from_be_bytes(bytes));
            if length < 126 {
                return Err("WebSocket frame uses a non-canonical length".to_string());
            }
            length
        }
        127 => {
            let mut bytes = [0_u8; 8];
            socket
                .read_exact(&mut bytes)
                .map_err(|error| format!("WebSocket length read failed: {error}"))?;
            let length = u64::from_be_bytes(bytes);
            if length & (1_u64 << 63) != 0 {
                return Err("WebSocket frame length has its high bit set".to_string());
            }
            usize::try_from(length)
                .map_err(|_| "WebSocket frame length does not fit in this platform".to_string())?
        }
        _ => return Err("WebSocket frame length code is invalid".to_string()),
    };
    if payload_len > MAX_WEBSOCKET_FRAME_BYTES {
        return Err(format!(
            "WebSocket payload exceeds {MAX_WEBSOCKET_FRAME_BYTES} bytes"
        ));
    }
    let mut key = [0_u8; 4];
    if masked {
        socket
            .read_exact(&mut key)
            .map_err(|error| format!("WebSocket mask read failed: {error}"))?;
    }
    let mut payload = vec![0_u8; payload_len];
    socket
        .read_exact(&mut payload)
        .map_err(|error| format!("WebSocket payload read failed: {error}"))?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[index % key.len()];
        }
    }
    let entry = WebSocketFrameEntry {
        fin,
        opcode,
        payload,
        masked: false,
        names: 1,
    };
    validate_websocket_frame(&entry)?;
    Ok(entry)
}

fn read_websocket_message(
    socket: &mut StdTcpStream,
    mut fragments: Option<(u8, Vec<u8>)>,
) -> Result<WebSocketFrameEntry, String> {
    loop {
        let frame = read_websocket_frame(socket)?;
        match frame.opcode as u8 {
            9 => {
                let pong = encode_websocket_frame(&WebSocketFrameEntry {
                    fin: true,
                    opcode: 10,
                    payload: frame.payload,
                    masked: false,
                    names: 1,
                })?;
                socket
                    .write_all(&pong)
                    .map_err(|error| format!("WebSocket pong write failed: {error}"))?;
                socket
                    .flush()
                    .map_err(|error| format!("WebSocket pong flush failed: {error}"))?;
            }
            10 => {}
            8 => {
                let close = encode_websocket_frame(&WebSocketFrameEntry {
                    fin: true,
                    opcode: 8,
                    payload: frame.payload.clone(),
                    masked: false,
                    names: 1,
                })?;
                socket
                    .write_all(&close)
                    .and_then(|()| socket.flush())
                    .map_err(|error| format!("WebSocket close write failed: {error}"))?;
                return Ok(frame);
            }
            0 => {
                let Some((opcode, mut payload)) = fragments.take() else {
                    return Err("WebSocket continuation has no open message".to_string());
                };
                payload.extend_from_slice(&frame.payload);
                if payload.len() > MAX_WEBSOCKET_FRAME_BYTES {
                    return Err(format!(
                        "WebSocket message exceeds {MAX_WEBSOCKET_FRAME_BYTES} bytes"
                    ));
                }
                if frame.fin {
                    let complete = WebSocketFrameEntry {
                        fin: true,
                        opcode: i64::from(opcode),
                        payload,
                        masked: false,
                        names: 1,
                    };
                    validate_websocket_frame(&complete)?;
                    return Ok(complete);
                }
                fragments = Some((opcode, payload));
            }
            1 | 2 if !frame.fin => {
                if fragments.is_some() {
                    return Err("WebSocket message starts before the previous one ends".to_string());
                }
                fragments = Some((frame.opcode as u8, frame.payload));
            }
            1 | 2 if fragments.is_some() => {
                return Err("WebSocket message starts before the previous one ends".to_string());
            }
            _ => return Ok(frame),
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_session_from_tcp(stream: *mut Value) -> *mut Value {
    let result = (|| {
        let tcp_handle = tcp_handle(stream)?;
        let socket = with_tcp_stream(tcp_handle, |socket| {
            socket
                .try_clone()
                .map_err(|error| format!("WebSocket session socket clone failed: {error}"))
        })?;
        let actor = StreamingSocketActor::new(socket, 0, None)?;
        take_resource_value(insert_websocket_session(actor))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_session_receive(session: *const Value) -> *mut Value {
    let result = (|| {
        let session_handle = websocket_session_handle(session)?;
        let actor = lock_websocket_sessions()
            .get(&session_handle)
            .map(|entry| Arc::clone(&entry.actor))
            .ok_or_else(|| "invalid WebSocketSession handle".to_string())?;
        let frame = actor.read_websocket()?;
        take_resource_value(insert_websocket_frame(frame))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_session_send(
    session: *const Value,
    frame: *const Value,
) -> *mut Value {
    let result = (|| {
        let session_handle = websocket_session_handle(session)?;
        let frame_handle = websocket_frame_handle(frame)?;
        let bytes = {
            let frames = lock_websocket_frames();
            let entry = frames
                .get(&frame_handle)
                .ok_or_else(|| "invalid WebSocketFrame handle".to_string())?;
            encode_websocket_frame(entry)?
        };
        let actor = lock_websocket_sessions()
            .get(&session_handle)
            .map(|entry| Arc::clone(&entry.actor))
            .ok_or_else(|| "invalid WebSocketSession handle".to_string())?;
        actor.write(bytes, true)
    })();
    http_result_unit(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_websocket_session_close(session: *mut Value) {
    if let Ok(handle) = websocket_session_handle(session) {
        if let Some(actor) = lock_websocket_sessions()
            .get(&handle)
            .map(|entry| Arc::clone(&entry.actor))
        {
            let close = encode_websocket_frame(&WebSocketFrameEntry {
                fin: true,
                opcode: 8,
                payload: Vec::new(),
                masked: false,
                names: 1,
            });
            if let Ok(close) = close {
                let _ = actor.write(close, true);
            }
            actor.close();
        }
        close_websocket_session_handle(handle);
        write_handle(session, 0);
    }
}

/// Box an HTTP category using the same raw `{ i32 discriminant }` layout that
/// codegen uses for the payload-less `HttpErrorKind` enum. The compiler
/// unboxes this value immediately on a `error.kind` field access, and the
/// temporary Mux value remains owned by the normal statement cleanup path.
fn http_error_kind_value(kind: HttpErrorKind) -> Value {
    Value::Opaque((kind as i32).to_ne_bytes().to_vec().into_boxed_slice())
}

fn http_result_unit(result: Result<(), String>) -> *mut Value {
    match result {
        Ok(()) => http_result_ok(Value::Unit),
        Err(error) => http_result_err(error),
    }
}

fn http_result_unit_with_kind(result: Result<(), String>, kind: StdErrorKind) -> *mut Value {
    match result {
        Ok(()) => http_result_ok(Value::Unit),
        Err(error) => http_result_err_with_kind(kind, error, 0, String::new(), String::new()),
    }
}

fn net_result_unit(result: Result<(), String>) -> *mut Value {
    match result {
        Ok(()) => net_result_ok(Value::Unit),
        Err(err) => net_result_err(err),
    }
}

fn net_result_string(result: Result<String, String>) -> *mut Value {
    match result {
        Ok(value) => net_result_ok(Value::String(value)),
        Err(err) => net_result_err(err),
    }
}

fn timeout_from_millis(timeout_ms: i64, label: &str) -> Result<Option<Duration>, String> {
    if timeout_ms < 0 {
        return Err(format!("{label} must be non-negative milliseconds"));
    }
    if timeout_ms == 0 {
        Ok(None)
    } else {
        Ok(Some(Duration::from_millis(timeout_ms as u64)))
    }
}

/// Keep a single socket read from turning an untrusted Mux integer into an
/// unbounded allocation. Callers can read larger streams in multiple chunks.
const MAX_SOCKET_READ_BYTES: usize = 16 * 1024 * 1024;
const MAX_UDP_DATAGRAM_BYTES: usize = 65_535;

fn socket_read_size(size: i64) -> Result<usize, String> {
    let size = usize::try_from(size).map_err(|_| "invalid buffer size".to_string())?;
    if size == 0 || size > MAX_SOCKET_READ_BYTES {
        return Err(format!(
            "buffer size must be between 1 and {MAX_SOCKET_READ_BYTES} bytes"
        ));
    }
    Ok(size)
}

fn socket_buffer_size(size: i64, label: &str) -> Result<usize, String> {
    let size = usize::try_from(size).map_err(|_| format!("{label} must be positive"))?;
    if size == 0 || size > MAX_SOCKET_READ_BYTES {
        return Err(format!(
            "{label} must be between 1 and {MAX_SOCKET_READ_BYTES} bytes"
        ));
    }
    Ok(size)
}

fn socket_ttl_value(ttl: i64, label: &str) -> Result<u32, String> {
    u32::try_from(ttl).map_err(|_| format!("{label} must be between 0 and {}", u32::MAX))
}

fn byte_count_value(count: usize, operation: &str) -> Result<Value, String> {
    i64::try_from(count)
        .map(Value::Int)
        .map_err(|_| format!("{operation} byte count exceeds the Mux integer range"))
}

/// Preserve the response body as a reader for the typed HTTP surface. The
/// reader owns the response body and is `'static`, so it can outlive the
/// request call while remaining protected by the response handle's mutex.
fn stream_http_response_data(
    response: ureq::http::Response<ureq::Body>,
) -> Result<HttpResponseData, String> {
    let status = i64::from(response.status().as_u16());
    let mut response_headers = Vec::new();
    for name in response.headers().keys() {
        let values = response.headers().get_all(name);
        for value in &values {
            let value = value
                .to_str()
                .map_err(|error| format!("invalid UTF-8 in response header '{name}': {error}"))?;
            response_headers.push((name.as_str().to_ascii_lowercase(), value.to_string()));
        }
    }
    validate_http_header_budget(&response_headers)?;
    let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
    Ok(HttpResponseData {
        status,
        headers: response_headers,
        body: Vec::new(),
        body_reader: Some(reader),
    })
}

const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
const HTTP_SERVER_POOL_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_HTTP_HEADERS_COUNT: usize = 128;
const DEFAULT_HTTP_CONNECT_TIMEOUT_MS: i64 = 10_000;
const DEFAULT_HTTP_TIMEOUT_MS: i64 = 30_000;
const DEFAULT_HTTP_MAX_REDIRECTS: i64 = 10;
const DEFAULT_HTTP_RETRY_BACKOFF_MS: i64 = 100;
const MAX_HTTP_TIMEOUT_MS: i64 = 86_400_000;
const MAX_HTTP_REDIRECTS: i64 = 100;
const MAX_HTTP_RETRIES: i64 = 10;
const MAX_HTTP_RETRY_BACKOFF_MS: i64 = 60_000;
const DEFAULT_HTTP_SERVER_MAX_HEADER_BYTES: i64 = MAX_HTTP_HEADER_BYTES as i64;
const DEFAULT_HTTP_SERVER_MAX_BODY_BYTES: i64 = MAX_HTTP_BODY_BYTES as i64;
const DEFAULT_HTTP_SERVER_MAX_HEADERS: i64 = MAX_HTTP_HEADERS_COUNT as i64;
const MAX_HTTP_SERVER_LIMIT_BYTES: i64 = MAX_SOCKET_READ_BYTES as i64;
const MAX_HTTP_SERVER_HEADERS: i64 = 1_024;
const MAX_HTTP_SERVER_TIMEOUT_MS: i64 = 86_400_000;
const DEFAULT_HTTP_SERVER_WORKER_COUNT: i64 = 1;
const MAX_HTTP_SERVER_WORKER_COUNT: i64 = 256;

fn validate_http_buffered_body(body: &[u8]) -> Result<(), String> {
    if body.len() > MAX_HTTP_BODY_BYTES {
        Err(format!(
            "HTTP buffered body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"
        ))
    } else {
        Ok(())
    }
}

/// Bound header collections before they cross an HTTP transport boundary.
///
/// The byte budget counts each field as `name: value\\r\\n` plus the final
/// empty line. It intentionally does not include a request/status line because
/// this helper validates the field section shared by requests and responses.
fn validate_http_header_budget(headers: &[(String, String)]) -> Result<(), String> {
    if headers.len() > MAX_HTTP_HEADERS_COUNT {
        return Err(format!(
            "HTTP header field count exceeds the {MAX_HTTP_HEADERS_COUNT}-field limit"
        ));
    }
    let mut total = 2_usize;
    for (name, value) in headers {
        let field_size = name
            .len()
            .checked_add(2)
            .and_then(|size| size.checked_add(value.len()))
            .and_then(|size| size.checked_add(2))
            .ok_or_else(|| "HTTP header block size overflowed".to_string())?;
        total = total
            .checked_add(field_size)
            .ok_or_else(|| "HTTP header block size overflowed".to_string())?;
    }
    if total > MAX_HTTP_HEADER_BYTES {
        return Err(format!(
            "HTTP header block exceeds the {MAX_HTTP_HEADER_BYTES}-byte limit"
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct HttpServerLimits {
    max_header_bytes: usize,
    max_body_bytes: usize,
    max_headers: usize,
    read_timeout_ms: i64,
    access_log: bool,
    cors_origins: Vec<String>,
    cors_allow_credentials: bool,
    static_root: String,
    worker_count: usize,
    heartbeat_interval_ms: i64,
}

fn default_http_server_limits() -> HttpServerLimits {
    HttpServerLimits {
        max_header_bytes: DEFAULT_HTTP_SERVER_MAX_HEADER_BYTES as usize,
        max_body_bytes: DEFAULT_HTTP_SERVER_MAX_BODY_BYTES as usize,
        max_headers: DEFAULT_HTTP_SERVER_MAX_HEADERS as usize,
        read_timeout_ms: DEFAULT_HTTP_TIMEOUT_MS,
        access_log: false,
        cors_origins: Vec::new(),
        cors_allow_credentials: false,
        static_root: String::new(),
        worker_count: DEFAULT_HTTP_SERVER_WORKER_COUNT as usize,
        heartbeat_interval_ms: DEFAULT_HTTP_HEARTBEAT_INTERVAL_MS,
    }
}

fn http_server_limits(entry: &HttpServerConfigEntry) -> Result<HttpServerLimits, String> {
    let max_header_bytes = usize::try_from(entry.max_header_bytes)
        .map_err(|_| "HTTP server max_header_bytes must be positive".to_string())?;
    let max_body_bytes = usize::try_from(entry.max_body_bytes)
        .map_err(|_| "HTTP server max_body_bytes must be positive".to_string())?;
    let max_headers = usize::try_from(entry.max_headers)
        .map_err(|_| "HTTP server max_headers must be positive".to_string())?;
    let worker_count = usize::try_from(entry.worker_count)
        .map_err(|_| "HTTP server worker_count must be positive".to_string())?;
    if !(1..=MAX_HTTP_SERVER_LIMIT_BYTES).contains(&entry.max_header_bytes)
        || !(1..=MAX_HTTP_SERVER_LIMIT_BYTES).contains(&entry.max_body_bytes)
    {
        return Err(format!(
            "HTTP server byte limits must be between 1 and {MAX_HTTP_SERVER_LIMIT_BYTES}"
        ));
    }
    if !(1..=MAX_HTTP_SERVER_HEADERS).contains(&entry.max_headers) {
        return Err(format!(
            "HTTP server max_headers must be between 1 and {MAX_HTTP_SERVER_HEADERS}"
        ));
    }
    if !(0..=MAX_HTTP_SERVER_TIMEOUT_MS).contains(&entry.read_timeout_ms) {
        return Err(format!(
            "HTTP server read_timeout_ms must be between 0 and {MAX_HTTP_SERVER_TIMEOUT_MS}"
        ));
    }
    if !(1..=MAX_HTTP_SERVER_WORKER_COUNT).contains(&entry.worker_count) {
        return Err(format!(
            "HTTP server worker_count must be between 1 and {MAX_HTTP_SERVER_WORKER_COUNT}"
        ));
    }
    if !(0..=MAX_HTTP_HEARTBEAT_INTERVAL_MS).contains(&entry.heartbeat_interval_ms) {
        return Err(format!(
            "HTTP server heartbeat_interval_ms must be between 0 and {MAX_HTTP_HEARTBEAT_INTERVAL_MS}"
        ));
    }
    for origin in &entry.cors_origins {
        if origin.is_empty() || origin.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
            return Err("HTTP CORS origins must be non-empty and contain no CR/LF".to_string());
        }
    }
    if entry.cors_allow_credentials && entry.cors_origins.iter().any(|origin| origin == "*") {
        return Err("HTTP CORS wildcard origin cannot be used with credentials".to_string());
    }
    if entry
        .static_root
        .bytes()
        .any(|byte| byte == b'\r' || byte == b'\n')
    {
        return Err("HTTP static_root must not contain CR/LF".to_string());
    }
    Ok(HttpServerLimits {
        max_header_bytes,
        max_body_bytes,
        max_headers,
        read_timeout_ms: entry.read_timeout_ms,
        access_log: entry.access_log,
        cors_origins: entry.cors_origins.clone(),
        cors_allow_credentials: entry.cors_allow_credentials,
        static_root: entry.static_root.clone(),
        worker_count,
        heartbeat_interval_ms: entry.heartbeat_interval_ms,
    })
}

#[derive(Clone, Copy)]
struct HttpRequestOptions {
    connect_timeout_ms: i64,
    timeout_ms: i64,
    max_redirects: i64,
    retries: i64,
    retry_backoff_ms: i64,
}

fn default_http_request_options() -> HttpRequestOptions {
    HttpRequestOptions {
        connect_timeout_ms: DEFAULT_HTTP_CONNECT_TIMEOUT_MS,
        timeout_ms: DEFAULT_HTTP_TIMEOUT_MS,
        max_redirects: DEFAULT_HTTP_MAX_REDIRECTS,
        retries: 0,
        retry_backoff_ms: DEFAULT_HTTP_RETRY_BACKOFF_MS,
    }
}

fn validate_http_request_options(
    connect_timeout_ms: i64,
    timeout_ms: i64,
    max_redirects: i64,
    retries: i64,
    retry_backoff_ms: i64,
) -> Result<(), String> {
    if !(0..=MAX_HTTP_TIMEOUT_MS).contains(&connect_timeout_ms) {
        return Err(format!(
            "HTTP connect timeout must be between 0 and {MAX_HTTP_TIMEOUT_MS} milliseconds"
        ));
    }
    if !(0..=MAX_HTTP_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "HTTP timeout must be between 0 and {MAX_HTTP_TIMEOUT_MS} milliseconds"
        ));
    }
    if !(0..=MAX_HTTP_REDIRECTS).contains(&max_redirects) {
        return Err(format!(
            "HTTP max redirects must be between 0 and {MAX_HTTP_REDIRECTS}"
        ));
    }
    if !(0..=MAX_HTTP_RETRIES).contains(&retries) {
        return Err(format!(
            "HTTP retries must be between 0 and {MAX_HTTP_RETRIES}"
        ));
    }
    if !(0..=MAX_HTTP_RETRY_BACKOFF_MS).contains(&retry_backoff_ms) {
        return Err(format!(
            "HTTP retry backoff must be between 0 and {MAX_HTTP_RETRY_BACKOFF_MS} milliseconds"
        ));
    }
    Ok(())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Find the end of an HTTP header block without charging bytes that were
/// already read from the socket for the request body against the header
/// limit. A single TCP read commonly contains both sections.
fn http_header_end_within_limit(
    buffer: &[u8],
    max_header_bytes: usize,
) -> Result<Option<usize>, String> {
    if let Some(position) = find_double_crlf(buffer) {
        let header_end = position + 4;
        if header_end > max_header_bytes {
            return Err("http headers too large".to_string());
        }
        return Ok(Some(header_end));
    }
    if buffer.len() > max_header_bytes {
        return Err("http headers too large".to_string());
    }
    Ok(None)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "Unknown",
    }
}

fn read_http_request_headers(
    stream: &mut StdTcpStream,
    max_header_bytes: usize,
) -> Result<(Vec<u8>, usize), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let count = stream
            .read(&mut chunk)
            .map_err(|e| format!("http read failed: {e}"))?;
        if count == 0 {
            return Err("connection closed before request headers".to_string());
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(header_end) = http_header_end_within_limit(&buffer, max_header_bytes)? {
            return Ok((buffer, header_end));
        }
    }
}

fn parse_http_request_header_pairs(
    header_slice: &[u8],
    max_headers: usize,
) -> Result<HttpRequestHeaderParts, String> {
    let mut parsed_headers = vec![httparse::EMPTY_HEADER; max_headers];
    let mut request = httparse::Request::new(&mut parsed_headers);
    let parse_status = request
        .parse(header_slice)
        .map_err(|e| format!("invalid http request: {e}"))?;
    if parse_status.is_partial() {
        return Err("incomplete http request headers".to_string());
    }

    let method = request
        .method
        .ok_or_else(|| "http request missing method".to_string())?
        .to_string();
    let raw_target = request
        .path
        .ok_or_else(|| "http request missing target".to_string())?
        .to_string();
    let version = match request.version {
        Some(0) => "HTTP/1.0".to_string(),
        Some(1) => "HTTP/1.1".to_string(),
        Some(v) => return Err(format!("unsupported http version {v}")),
        None => return Err("http request missing version".to_string()),
    };

    let mut headers = Vec::new();
    for header in request.headers.iter() {
        let value = std::str::from_utf8(header.value)
            .map_err(|_| format!("header '{}' contains invalid utf-8", header.name))?;
        let trimmed_value = value.trim();
        let normalized_name = header_name(header.name)?;
        header_value(trimmed_value)?;
        headers.push((normalized_name, trimmed_value.to_string()));
    }
    validate_http_request_host(&version, &headers)?;
    Ok((method, raw_target, version, headers))
}

/// HTTP/1.1 requires exactly one non-empty Host field. Rejecting duplicate
/// values for every HTTP version also prevents an intermediary and this
/// parser from choosing different authorities for the same request.
fn validate_http_request_host(version: &str, headers: &[(String, String)]) -> Result<(), String> {
    let hosts = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
        .collect::<Vec<_>>();
    if hosts.len() > 1 {
        return Err("HTTP request must contain exactly one Host header".to_string());
    }
    if hosts.first().is_some_and(|(_, value)| value.is_empty()) {
        return Err("HTTP request Host header must not be empty".to_string());
    }
    if let Some((_, value)) = hosts.first() {
        validate_http_host_value(value)?;
    }
    if version == "HTTP/1.1" && hosts.is_empty() {
        return Err("HTTP/1.1 request is missing the Host header".to_string());
    }
    Ok(())
}

/// Validate the Host field as a URI authority. Header-wide validation permits
/// spaces because they are legal in many field values, but Host is narrower:
/// accepting `Host: example.test attacker.test` would leave downstream
/// authority parsing with an ambiguous value. Parsing an HTTP authority with
/// the URL library also covers bracketed IPv6 literals and numeric ports.
fn validate_http_host_value(value: &str) -> Result<(), String> {
    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err("HTTP request Host header must be a URI authority".to_string());
    }
    let candidate = format!("http://{value}/");
    let parsed = ::url::Url::parse(&candidate)
        .map_err(|_| "HTTP request Host header must be a URI authority".to_string())?;
    if parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("HTTP request Host header must be a URI authority".to_string());
    }
    Ok(())
}

fn read_http_request_body(
    stream: &mut StdTcpStream,
    initial_body: &[u8],
    content_length: usize,
    max_body_bytes: usize,
) -> Result<Vec<u8>, String> {
    if content_length > max_body_bytes {
        return Err("request body too large".to_string());
    }
    let mut body = initial_body.to_vec();
    let mut chunk = [0u8; 1024];
    while body.len() < content_length {
        let count = stream
            .read(&mut chunk)
            .map_err(|e| format!("http read failed: {e}"))?;
        if count == 0 {
            return Err("connection closed before request body complete".to_string());
        }
        body.extend_from_slice(&chunk[..count]);
        if body.len() > content_length {
            body.truncate(content_length);
            break;
        }
    }
    body.truncate(content_length);
    Ok(body)
}

fn header_content_length_pairs(headers: &[(String, String)]) -> Result<Option<usize>, String> {
    let mut length = None;
    for raw_value in headers
        .iter()
        .filter_map(|(name, value)| name.eq_ignore_ascii_case("content-length").then_some(value))
    {
        let parsed = raw_value
            .trim()
            .parse::<usize>()
            .map_err(|_| "invalid Content-Length header".to_string())?;
        if length.is_some_and(|known| known != parsed) {
            return Err("conflicting Content-Length headers".to_string());
        }
        length = Some(parsed);
    }
    let Some(len) = length else {
        return Ok(None);
    };
    if len > MAX_HTTP_BODY_BYTES {
        return Err("request body too large".to_string());
    }
    Ok(Some(len))
}

/// Return whether a request uses the one transfer coding understood by the
/// HTTP/1.x parser.  Treating a list that merely contains `chunked` as
/// chunked would make unsupported codings invisible to the parser (for
/// example, `gzip, chunked`), which can cause different intermediaries to
/// disagree about where the request body ends.  Until the server has a
/// decoder for another coding, require exactly one `chunked` token.
fn request_uses_chunked_transfer_encoding(headers: &[(String, String)]) -> Result<bool, String> {
    let mut chunked = false;
    for value in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .map(|(_, value)| value)
    {
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() {
                return Err("invalid Transfer-Encoding header".to_string());
            }
            if !token.eq_ignore_ascii_case("chunked") {
                return Err(format!(
                    "unsupported Transfer-Encoding '{token}'; only chunked is supported"
                ));
            }
            if chunked {
                return Err(
                    "request must contain exactly one chunked transfer encoding".to_string()
                );
            }
            chunked = true;
        }
    }
    Ok(chunked)
}

/// Validate one HTTP/1.x chunked trailer field. Trailers are not surfaced on
/// the current request value, but they still cross an untrusted wire boundary
/// and must not smuggle a second message-framing decision into a downstream
/// component.
fn validate_http_request_trailer(line: &[u8]) -> Result<(), String> {
    let separator = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| "invalid HTTP request trailer field".to_string())?;
    let (name, value) = line.split_at(separator);
    let value = &value[1..];
    let name = std::str::from_utf8(name)
        .map_err(|_| "HTTP request trailer name is not utf-8".to_string())?;
    let value = std::str::from_utf8(value)
        .map_err(|_| "HTTP request trailer value is not utf-8".to_string())?;
    let name = header_name(name)?;
    let value = value.trim();
    header_value(value)?;
    if name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("transfer-encoding")
    {
        return Err(format!("HTTP request trailer '{name}' is not permitted"));
    }
    Ok(())
}

fn read_more_http_bytes(
    stream: &mut StdTcpStream,
    buffer: &mut Vec<u8>,
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> Result<(), String> {
    let mut chunk = [0u8; 1024];
    let count = stream
        .read(&mut chunk)
        .map_err(|error| format!("http read failed: {error}"))?;
    if count == 0 {
        return Err("connection closed before chunked request body complete".to_string());
    }
    buffer.extend_from_slice(&chunk[..count]);
    if buffer.len() > max_body_bytes.saturating_add(max_header_bytes) {
        return Err("request body too large".to_string());
    }
    Ok(())
}

fn read_chunked_http_request_body(
    stream: &mut StdTcpStream,
    initial_body: &[u8],
    max_header_bytes: usize,
    max_body_bytes: usize,
    max_trailers: usize,
) -> Result<Vec<u8>, String> {
    let mut buffer = initial_body.to_vec();
    let mut cursor = 0;
    let mut body = Vec::new();
    loop {
        let size = read_chunk_size(
            stream,
            &mut buffer,
            &mut cursor,
            max_header_bytes,
            max_body_bytes,
        )?;
        if size == 0 {
            read_chunked_request_trailers(
                stream,
                &mut buffer,
                &mut cursor,
                max_header_bytes,
                max_body_bytes,
                max_trailers,
            )?;
            return Ok(body);
        }
        if body.len().saturating_add(size) > max_body_bytes {
            return Err("request body too large".to_string());
        }
        read_chunk_data(
            stream,
            &mut buffer,
            &mut cursor,
            &mut body,
            size,
            max_header_bytes,
            max_body_bytes,
        )?;
    }
}

fn read_chunk_size(
    stream: &mut StdTcpStream,
    buffer: &mut Vec<u8>,
    cursor: &mut usize,
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> Result<usize, String> {
    let line_end = read_http_line_end(
        stream,
        buffer,
        *cursor,
        max_header_bytes,
        max_body_bytes,
        Some("chunk size line too large"),
    )?;
    let line = std::str::from_utf8(&buffer[*cursor..line_end])
        .map_err(|_| "chunk size line is not utf-8".to_string())?;
    *cursor = line_end + 2;
    usize::from_str_radix(line.split(';').next().unwrap_or_default().trim(), 16)
        .map_err(|_| "invalid chunk size".to_string())
}

fn read_http_line_end(
    stream: &mut StdTcpStream,
    buffer: &mut Vec<u8>,
    cursor: usize,
    max_header_bytes: usize,
    max_body_bytes: usize,
    too_large_message: Option<&str>,
) -> Result<usize, String> {
    loop {
        if let Some(relative) = buffer[cursor..]
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
        {
            return Ok(cursor + relative);
        }
        if let Some(message) = too_large_message {
            if buffer.len().saturating_sub(cursor) > max_header_bytes {
                return Err(message.to_string());
            }
        }
        read_more_http_bytes(stream, buffer, max_header_bytes, max_body_bytes)?;
    }
}

fn read_chunked_request_trailers(
    stream: &mut StdTcpStream,
    buffer: &mut Vec<u8>,
    cursor: &mut usize,
    max_header_bytes: usize,
    max_body_bytes: usize,
    max_trailers: usize,
) -> Result<(), String> {
    let mut trailer_bytes = 0_usize;
    let mut trailer_count = 0_usize;
    loop {
        let trailer_end = read_http_line_end(
            stream,
            buffer,
            *cursor,
            max_header_bytes,
            max_body_bytes,
            None,
        )?;
        let trailer_line = &buffer[*cursor..trailer_end];
        *cursor = trailer_end + 2;
        if trailer_line.is_empty() {
            return Ok(());
        }
        trailer_bytes = trailer_bytes
            .saturating_add(trailer_line.len())
            .saturating_add(2);
        if trailer_bytes > max_header_bytes {
            return Err("chunked request trailers too large".to_string());
        }
        trailer_count = trailer_count.saturating_add(1);
        if trailer_count > max_trailers {
            return Err("too many HTTP request trailer fields".to_string());
        }
        validate_http_request_trailer(trailer_line)?;
    }
}

fn read_chunk_data(
    stream: &mut StdTcpStream,
    buffer: &mut Vec<u8>,
    cursor: &mut usize,
    body: &mut Vec<u8>,
    size: usize,
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> Result<(), String> {
    while buffer.len().saturating_sub(*cursor) < size.saturating_add(2) {
        read_more_http_bytes(stream, buffer, max_header_bytes, max_body_bytes)?;
    }
    body.extend_from_slice(&buffer[*cursor..*cursor + size]);
    *cursor += size;
    if buffer.get(*cursor..*cursor + 2) != Some(b"\r\n") {
        return Err("chunk is missing its trailing CRLF".to_string());
    }
    *cursor += 2;
    Ok(())
}

fn read_typed_http_request_with_limits(
    stream: &mut StdTcpStream,
    limits: &HttpServerLimits,
) -> Result<HttpRequestEntry, String> {
    let (buffer, header_end) = read_http_request_headers(stream, limits.max_header_bytes)?;
    let (method, raw_target, _version, headers) =
        parse_http_request_header_pairs(&buffer[..header_end], limits.max_headers)?;
    if !is_http_token(&method) {
        return Err("invalid HTTP request method".to_string());
    }
    let initial_body = &buffer[header_end..];
    let content_length = header_content_length_pairs(&headers)?;
    let chunked = request_uses_chunked_transfer_encoding(&headers)?;
    if chunked && content_length.is_some() {
        return Err("request cannot use both Transfer-Encoding and Content-Length".to_string());
    }
    let body = if chunked {
        read_chunked_http_request_body(
            stream,
            initial_body,
            limits.max_header_bytes,
            limits.max_body_bytes,
            limits.max_headers.saturating_sub(headers.len()),
        )?
    } else {
        let length = match content_length {
            Some(length) => length,
            None if initial_body.is_empty() => 0,
            None => return Err("request body present without Content-Length header".to_string()),
        };
        read_http_request_body(stream, initial_body, length, limits.max_body_bytes)?
    };
    let header_data = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect::<Vec<_>>();
    let request_id = http_request_id(&header_data);
    let options = default_http_request_options();
    Ok(HttpRequestEntry {
        method: method.to_ascii_uppercase(),
        url: raw_target,
        request_id,
        proxy: None,
        headers: Arc::new(Mutex::new(HeaderData {
            values: header_data,
        })),
        body: Some(body),
        body_reader: None,
        path_params: Arc::new(Mutex::new(HashMap::new())),
        connect_timeout_ms: options.connect_timeout_ms,
        timeout_ms: options.timeout_ms,
        max_redirects: options.max_redirects,
        retries: options.retries,
        retry_backoff_ms: options.retry_backoff_ms,
        names: 1,
    })
}

fn read_typed_http_request(stream: &mut StdTcpStream) -> Result<HttpRequestEntry, String> {
    let limits = default_http_server_limits();
    read_typed_http_request_with_limits(stream, &limits)
}

/// Return a bounded request identifier for server-side requests. A caller's
/// identifier is retained only when it is a visible ASCII token; malformed or
/// oversized values are replaced so the value is safe to put back in a header
/// and in an access log.
fn http_request_id(headers: &[(String, String)]) -> String {
    if let Some(value) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
        .map(|(_, value)| value.trim())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        })
    {
        return value.to_string();
    }
    format!(
        "mux-{}",
        NEXT_HTTP_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn response_headers_with_request_id(
    mut headers: Vec<(String, String)>,
    request_id: &str,
) -> Vec<(String, String)> {
    if !request_id.is_empty()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
    {
        headers.push(("x-request-id".to_string(), request_id.to_string()));
    }
    headers
}

fn log_http_access(request_id: &str, method: &str, url: &str, status: i64) {
    eprintln!(
        "mux http request_id={} method={} path={} status={}",
        sanitize_http_log_field(request_id),
        sanitize_http_log_field(method),
        sanitize_http_log_field(url),
        status
    );
}

/// Keep access-log records single-line even when a low-level parser or a
/// manually constructed request contains control characters. Request IDs are
/// validated before they are accepted from the wire, but sanitizing every
/// field here also protects direct router use and future parser changes from
/// log-forging output.
fn sanitize_http_log_field(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() || character == '\u{7f}' {
            sanitized.extend(character.escape_default());
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}

struct HttpResponseHeaderState {
    wire_headers: Vec<(String, String)>,
    declared_content_length: Option<usize>,
    has_content_length: bool,
    has_connection: bool,
    connection_closes: bool,
    connection_keep_alive: bool,
    has_transfer_encoding: bool,
}

fn collect_http_response_headers(
    headers: &[(String, String)],
) -> Result<HttpResponseHeaderState, String> {
    // Reject an oversized caller-owned collection before reserving capacity or
    // walking it. The final validation below also accounts for the writer's
    // automatically added framing fields.
    validate_http_header_budget(headers)?;
    let mut state = HttpResponseHeaderState {
        wire_headers: Vec::with_capacity(headers.len() + 2),
        declared_content_length: None,
        has_content_length: false,
        has_connection: false,
        connection_closes: false,
        connection_keep_alive: false,
        has_transfer_encoding: false,
    };

    for (name, value) in headers {
        header_name(name)?;
        header_value(value)?;
        if name.eq_ignore_ascii_case("content-length") {
            state.has_content_length = true;
            let length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| "invalid Content-Length header".to_string())?;
            if state
                .declared_content_length
                .is_some_and(|known| known != length)
            {
                return Err("conflicting Content-Length headers".to_string());
            }
            state.declared_content_length = Some(length);
        }
        if name.eq_ignore_ascii_case("connection") {
            state.has_connection = true;
            for token in value.split(',').map(str::trim) {
                state.connection_closes |= token.eq_ignore_ascii_case("close");
                state.connection_keep_alive |= token.eq_ignore_ascii_case("keep-alive");
            }
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            state.has_transfer_encoding = true;
        }
        state.wire_headers.push((name.clone(), value.clone()));
    }
    Ok(state)
}

fn validate_http_response_header_state(
    state: &HttpResponseHeaderState,
    status_code: u16,
    body_len: usize,
) -> Result<(), String> {
    if state.has_transfer_encoding {
        return Err(
            "Transfer-Encoding is not supported by the HTTP/1.x response writer".to_string(),
        );
    }
    if state.has_connection && (!state.connection_closes || state.connection_keep_alive) {
        return Err(
            "HTTP/1.x response writer always closes the connection; Connection must include close and must not advertise keep-alive".to_string(),
        );
    }
    if state
        .declared_content_length
        .is_some_and(|length| length != body_len)
    {
        return Err("Content-Length does not match the response body".to_string());
    }
    validate_bodyless_http_response(state, status_code, body_len)?;
    validate_reset_http_response(state, status_code, body_len)
}

fn validate_bodyless_http_response(
    state: &HttpResponseHeaderState,
    status_code: u16,
    body_len: usize,
) -> Result<(), String> {
    let bodyless_status = (100..200).contains(&status_code) || matches!(status_code, 204 | 304);
    if !bodyless_status {
        return Ok(());
    }
    if body_len != 0 {
        return Err(format!(
            "HTTP status {status_code} must not include a response body"
        ));
    }
    if state.has_content_length {
        return Err(format!(
            "HTTP status {status_code} must not include Content-Length"
        ));
    }
    Ok(())
}

fn validate_reset_http_response(
    state: &HttpResponseHeaderState,
    status_code: u16,
    body_len: usize,
) -> Result<(), String> {
    if status_code != 205 {
        return Ok(());
    }
    if body_len != 0 {
        return Err("HTTP status 205 must not include a response body".to_string());
    }
    if state
        .declared_content_length
        .is_some_and(|length| length != 0)
    {
        return Err("HTTP status 205 requires Content-Length: 0".to_string());
    }
    Ok(())
}

fn append_http_response_framing(
    state: &mut HttpResponseHeaderState,
    status_code: u16,
    body_len: usize,
) {
    let bodyless_status = (100..200).contains(&status_code) || matches!(status_code, 204 | 304);
    if !state.has_content_length {
        if status_code == 205 {
            state
                .wire_headers
                .push(("content-length".to_string(), "0".to_string()));
        } else if !bodyless_status {
            state
                .wire_headers
                .push(("content-length".to_string(), body_len.to_string()));
        }
    }
    if !state.has_connection {
        state
            .wire_headers
            .push(("connection".to_string(), "close".to_string()));
    }
}

fn validated_http_response_headers(
    headers: &[(String, String)],
    status_code: u16,
    body_len: usize,
) -> Result<Vec<(String, String)>, String> {
    let mut state = collect_http_response_headers(headers)?;
    validate_http_response_header_state(&state, status_code, body_len)?;
    append_http_response_framing(&mut state, status_code, body_len);
    validate_http_header_budget(&state.wire_headers)?;
    Ok(state.wire_headers)
}

fn write_typed_http_response(
    stream: &mut StdTcpStream,
    status: i64,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(), String> {
    write_typed_http_response_to(stream, status, headers, body, None)
}

fn write_typed_http_response_for_request(
    stream: &mut StdTcpStream,
    status: i64,
    headers: &[(String, String)],
    body: &[u8],
    request_method: Option<&str>,
) -> Result<(), String> {
    write_typed_http_response_to(stream, status, headers, body, request_method)
}

/// Write only the header block for a response whose body is produced later by
/// a protocol handle. This is deliberately separate from the ordinary writer:
/// a streaming response cannot advertise Content-Length or the close-delimited
/// framing used by `HttpResponse`.
fn streaming_http_headers(status: i64, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    let status_code = u16::try_from(status).map_err(|_| "HTTP status must fit in u16 range")?;
    if !(100..=999).contains(&status_code) {
        return Err("HTTP status must be between 100 and 999".to_string());
    }
    validate_http_header_budget(headers)?;
    for (name, value) in headers {
        header_name(name)?;
        header_value(value)?;
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return Err(
                "streaming HTTP responses must not set Content-Length or Transfer-Encoding"
                    .to_string(),
            );
        }
    }
    let mut message = format!(
        "HTTP/1.1 {} {}\r\n",
        status_code,
        reason_phrase(status_code)
    );
    for (name, value) in headers {
        message.push_str(name);
        message.push_str(": ");
        message.push_str(value);
        message.push_str("\r\n");
    }
    message.push_str("\r\n");
    Ok(message.into_bytes())
}

fn write_typed_http_response_to<W: Write>(
    stream: &mut W,
    status: i64,
    headers: &[(String, String)],
    body: &[u8],
    request_method: Option<&str>,
) -> Result<(), String> {
    // Every response reaches this common writer, including the plain-text
    // response synthesized for a handler error. Validate here as well as at
    // `HttpResponse` construction so error bodies and future callers cannot
    // bypass the buffered-response limit.
    validate_http_buffered_body(body)?;
    let omit_body = request_method.is_some_and(|method| method.eq_ignore_ascii_case("HEAD"));
    let status_code = u16::try_from(status).map_err(|_| "HTTP status must fit in u16 range")?;
    if !(100..=999).contains(&status_code) {
        return Err("HTTP status must be between 100 and 999".to_string());
    }
    let wire_headers = validated_http_response_headers(headers, status_code, body.len())?;
    let mut message = format!(
        "HTTP/1.1 {} {}\r\n",
        status_code,
        reason_phrase(status_code)
    );
    for (name, value) in wire_headers {
        message.push_str(&name);
        message.push_str(": ");
        message.push_str(&value);
        message.push_str("\r\n");
    }
    message.push_str("\r\n");
    stream
        .write_all(message.as_bytes())
        .map_err(|error| format!("http write failed: {error}"))?;
    if !omit_body {
        stream
            .write_all(body)
            .map_err(|error| format!("http write failed: {error}"))?;
    }
    stream
        .flush()
        .map_err(|error| format!("http flush failed: {error}"))
}

/// Closure representation emitted by the Mux compiler for callbacks.
///
/// HTTP server handlers are synchronous and return an `HttpResponse`, so the
/// boxed function pointer is the callback entry point we need here. Keep this
/// layout in sync with the compiler's closure representation and `sync.rs`.
#[repr(C)]
struct HttpServerClosureRepr {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
    capture_count: i64,
    boxed_function_ptr: *mut c_void,
}

unsafe fn validate_http_server_handler(handler: *mut c_void) -> Result<(), String> {
    if handler.is_null() {
        return Err("HTTP handler is null".to_string());
    }
    let repr = unsafe { &*(handler as *const HttpServerClosureRepr) };
    if repr.boxed_function_ptr.is_null() {
        return Err("HTTP handler must return result<HttpResponse, HttpError>".to_string());
    }
    Ok(())
}

unsafe fn validate_http_stream_handler(handler: *mut c_void) -> Result<(), String> {
    if handler.is_null() {
        return Err("HTTP streaming handler is null".to_string());
    }
    let repr = unsafe { &*(handler as *const HttpServerClosureRepr) };
    if repr.boxed_function_ptr.is_null() {
        return Err("HTTP streaming handler must return result<Unit, HttpError>".to_string());
    }
    Ok(())
}

unsafe fn invoke_http_server_handler(
    handler: *mut c_void,
    request: *mut Value,
) -> Result<*mut Value, String> {
    unsafe { validate_http_server_handler(handler)? };
    let repr = unsafe { &*(handler as *const HttpServerClosureRepr) };
    let response = if repr.captures_ptr.is_null() {
        let function: extern "C" fn(*mut Value) -> *mut Value =
            unsafe { std::mem::transmute(repr.boxed_function_ptr) };
        function(request)
    } else {
        let function: extern "C" fn(*mut c_void, *mut Value) -> *mut Value =
            unsafe { std::mem::transmute(repr.boxed_function_ptr) };
        function(repr.captures_ptr, request)
    };
    if response.is_null() {
        return Err("HTTP handler returned null".to_string());
    }
    Ok(response)
}

unsafe fn invoke_http_callback(
    callback: *mut c_void,
    args: &[*mut Value],
) -> Result<*mut Value, String> {
    if callback.is_null() {
        return Err("HTTP callback is null".to_string());
    }
    let repr = unsafe { &*(callback as *const HttpServerClosureRepr) };
    if repr.boxed_function_ptr.is_null() {
        return Err("HTTP callback must return result<HttpResponse, HttpError>".to_string());
    }
    let result = match (repr.captures_ptr.is_null(), args) {
        (true, [first]) => {
            let function: extern "C" fn(*mut Value) -> *mut Value =
                unsafe { std::mem::transmute(repr.boxed_function_ptr) };
            function(*first)
        }
        (false, [first]) => {
            let function: extern "C" fn(*mut c_void, *mut Value) -> *mut Value =
                unsafe { std::mem::transmute(repr.boxed_function_ptr) };
            function(repr.captures_ptr, *first)
        }
        (true, [first, second]) => {
            let function: extern "C" fn(*mut Value, *mut Value) -> *mut Value =
                unsafe { std::mem::transmute(repr.boxed_function_ptr) };
            function(*first, *second)
        }
        (false, [first, second]) => {
            let function: extern "C" fn(*mut c_void, *mut Value, *mut Value) -> *mut Value =
                unsafe { std::mem::transmute(repr.boxed_function_ptr) };
            function(repr.captures_ptr, *first, *second)
        }
        _ => return Err("unsupported HTTP callback arity".to_string()),
    };
    if result.is_null() {
        Err("HTTP callback returned null".to_string())
    } else {
        Ok(result)
    }
}

fn cors_origin_allowed(limits: &HttpServerLimits, origin: &str) -> bool {
    limits
        .cors_origins
        .iter()
        .any(|allowed| allowed == origin || (allowed == "*" && !limits.cors_allow_credentials))
}

fn cors_response_headers(
    mut headers: Vec<(String, String)>,
    request: *const Value,
    limits: &HttpServerLimits,
) -> Vec<(String, String)> {
    let Some(origin) = http_request_header(request, "origin") else {
        return headers;
    };
    if !cors_origin_allowed(limits, &origin) {
        return headers;
    }
    let value = if limits.cors_origins.iter().any(|allowed| allowed == "*")
        && !limits.cors_allow_credentials
    {
        "*".to_string()
    } else {
        origin
    };
    headers.push(("access-control-allow-origin".to_string(), value));
    headers.push(("vary".to_string(), "Origin".to_string()));
    if limits.cors_allow_credentials {
        headers.push((
            "access-control-allow-credentials".to_string(),
            "true".to_string(),
        ));
    }
    headers
}

fn preflight_response(
    request: *const Value,
    limits: &HttpServerLimits,
) -> Option<HttpResponseParts> {
    let method = http_request_header(request, "access-control-request-method")?;
    let origin = http_request_header(request, "origin")?;
    if !cors_origin_allowed(limits, &origin) || !is_http_token(&method) {
        return Some((403, Vec::new(), b"CORS origin denied".to_vec()));
    }
    let mut headers = vec![
        (
            "access-control-allow-origin".to_string(),
            if limits.cors_origins.iter().any(|allowed| allowed == "*")
                && !limits.cors_allow_credentials
            {
                "*".to_string()
            } else {
                origin
            },
        ),
        ("access-control-allow-methods".to_string(), method),
        ("vary".to_string(), "Origin".to_string()),
    ];
    if let Some(request_headers) = http_request_header(request, "access-control-request-headers") {
        if request_headers
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n')
        {
            return Some((403, Vec::new(), b"CORS headers denied".to_vec()));
        }
        headers.push(("access-control-allow-headers".to_string(), request_headers));
    }
    if limits.cors_allow_credentials {
        headers.push((
            "access-control-allow-credentials".to_string(),
            "true".to_string(),
        ));
    }
    Some((204, headers, Vec::new()))
}

fn static_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("css") => "text/css; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("gif") => "image/gif",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("jpeg" | "jpg") => "image/jpeg",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn static_segments_are_safe(segments: &[String]) -> bool {
    segments.iter().all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && !segment
                .chars()
                .any(|character| matches!(character, '/' | '\\' | ':' | '\0'))
    })
}

fn open_static_file(
    root: &Path,
    segments: &[String],
) -> Result<Option<(fs::File, PathBuf)>, String> {
    if segments.is_empty() || !static_segments_are_safe(segments) {
        return Ok(None);
    }
    let relative = segments.iter().fold(PathBuf::new(), |mut path, segment| {
        path.push(segment);
        path
    });
    let root = CapabilityDir::open_ambient_dir(root, ambient_authority())
        .map_err(|error| format!("HTTP static_root is not accessible: {error}"))?;
    let file = match root.open(&relative) {
        Ok(file) => file.into_std(),
        Err(_) => return Ok(None),
    };
    Ok(Some((file, relative)))
}

fn static_file_response(
    request: *const Value,
    limits: &HttpServerLimits,
) -> Option<Result<HttpResponseParts, String>> {
    if limits.static_root.is_empty() {
        return None;
    }
    let method = request_handle(request).ok().and_then(|handle| {
        lock_requests()
            .get(&handle)
            .map(|entry| entry.method.clone())
    })?;
    if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
        return None;
    }
    let url = request_handle(request)
        .ok()
        .and_then(|handle| lock_requests().get(&handle).map(|entry| entry.url.clone()))?;
    let result = (|| {
        let root = Path::new(&limits.static_root);
        let segments = request_path_segments(&url).map_err(|_| "not found".to_string())?;
        let Some((file, relative)) = open_static_file(root, &segments)? else {
            return Ok((404, Vec::new(), b"not found".to_vec()));
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("HTTP static file metadata failed: {error}"))?;
        if !metadata.is_file() {
            return Ok((404, Vec::new(), b"not found".to_vec()));
        }
        let length = usize::try_from(metadata.len())
            .map_err(|_| "HTTP static file is too large".to_string())?;
        if length > limits.max_body_bytes {
            return Ok((413, Vec::new(), b"static file too large".to_vec()));
        }
        let read_limit = u64::try_from(limits.max_body_bytes)
            .map_err(|_| "HTTP static file body limit is not representable".to_string())?
            .saturating_add(1);
        let mut body = Vec::with_capacity(length);
        file.take(read_limit)
            .read_to_end(&mut body)
            .map_err(|error| format!("HTTP static file read failed: {error}"))?;
        if body.len() > limits.max_body_bytes {
            return Ok((413, Vec::new(), b"static file too large".to_vec()));
        }
        let etag = format!(
            "\"{}\"",
            BASE64_URL_SAFE.encode(digest(&SHA256, &body).as_ref())
        );
        if http_request_header(request, "if-none-match")
            .is_some_and(|value| if_none_match_matches(&value, &etag))
        {
            return Ok((304, vec![("etag".to_string(), etag)], Vec::new()));
        }
        Ok((
            200,
            vec![
                (
                    "content-type".to_string(),
                    static_content_type(&relative).to_string(),
                ),
                ("etag".to_string(), etag),
            ],
            body,
        ))
    })();
    Some(result)
}

fn if_none_match_matches(header: &str, etag: &str) -> bool {
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate).trim() == etag
    })
}

fn http_request_header(request: *const Value, name: &str) -> Option<String> {
    let handle = request_handle(request).ok()?;
    let headers = lock_requests().get(&handle)?.headers.clone();
    let headers = headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    headers
        .values
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// Compare credentials without making a valid prefix observable. The length
/// is necessarily part of the comparison, but every byte of the longer input
/// is still visited and no credential value is ever included in diagnostics.
fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..max_len {
        difference |= usize::from(left.get(index).copied().unwrap_or(0))
            ^ usize::from(right.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn basic_auth_matches(request: *const Value, username: &str, password: &str) -> bool {
    let Some(header) = http_request_header(request, "authorization") else {
        return false;
    };
    let mut parts = header.split_ascii_whitespace();
    let Some(scheme) = parts.next() else {
        return false;
    };
    let Some(encoded) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Ok(decoded) = BASE64_STANDARD.decode(encoded) else {
        return false;
    };
    let expected = format!("{username}:{password}");
    constant_time_equal(&decoded, expected.as_bytes())
}

fn bearer_auth_matches(request: *const Value, token: &str) -> bool {
    let Some(header) = http_request_header(request, "authorization") else {
        return false;
    };
    let mut parts = header.split_ascii_whitespace();
    let Some(scheme) = parts.next() else {
        return false;
    };
    let Some(candidate) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("Bearer") {
        return false;
    }
    constant_time_equal(candidate.as_bytes(), token.as_bytes())
}

fn bearer_token(request: *const Value) -> Option<String> {
    let header = http_request_header(request, "authorization")?;
    let mut parts = header.split_ascii_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    (parts.next().is_none() && scheme.eq_ignore_ascii_case("Bearer") && valid_bearer_token(token))
        .then(|| token.to_string())
}

fn valid_bearer_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
                || byte == b'='
        })
}

fn validate_oauth_metadata_url(value: &str, field: &str) -> Result<(), String> {
    validate_auth_secret(value, field)?;
    let parsed = url::Url::parse(value)
        .map_err(|_| format!("HTTP OAuth/OIDC {field} must be an absolute URL"))?;
    if parsed.scheme() != "https" {
        return Err(format!("HTTP OAuth/OIDC {field} must use https"));
    }
    if parsed.host_str().is_none() {
        return Err(format!("HTTP OAuth/OIDC {field} must include a host"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "HTTP OAuth/OIDC {field} must not include user info"
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(format!(
            "HTTP OAuth/OIDC {field} must not include a query or fragment"
        ));
    }
    Ok(())
}

fn validate_oauth_metadata(issuer: &str, audience: &str, jwks_url: &str) -> Result<(), String> {
    validate_oauth_metadata_url(issuer, "issuer")?;
    validate_auth_secret(audience, "audience")?;
    if audience.len() > 4096 {
        return Err("HTTP OAuth/OIDC audience is too long".to_string());
    }
    validate_oauth_metadata_url(jwks_url, "JWKS URL")
}

const MAX_OAUTH_CLIENT_ID_BYTES: usize = 256;
const MAX_OAUTH_SCOPE_BYTES: usize = 128;
const MAX_OAUTH_TOKEN_BYTES: usize = 16 * 1024;
const MAX_OAUTH_RESPONSE_BYTES: usize = 1_048_576;

fn validate_oauth_client_redirect_uri(value: &str) -> Result<(), String> {
    let parsed = url::Url::parse(value)
        .map_err(|_| "OAuth/OIDC redirect URI must be an absolute URL".to_string())?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("OAuth/OIDC redirect URI must not include user info".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("OAuth/OIDC redirect URI must not include a query or fragment".to_string());
    }
    let secure = parsed.scheme().eq_ignore_ascii_case("https");
    let loopback = parsed.scheme().eq_ignore_ascii_case("http")
        && parsed.host().is_some_and(|host| {
            matches!(host, url::Host::Ipv4(address) if address.is_loopback())
                || matches!(host, url::Host::Ipv6(address) if address.is_loopback())
        });
    if !secure && !loopback {
        return Err(
            "OAuth/OIDC redirect URI must use https, or http on a loopback address".to_string(),
        );
    }
    Ok(())
}

fn validate_oauth_client_text(value: &str, field: &str, limit: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > limit {
        return Err(format!("OAuth/OIDC {field} is empty or too long"));
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(format!(
            "OAuth/OIDC {field} contains whitespace or control characters"
        ));
    }
    Ok(())
}

fn validate_oauth_client_config(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
) -> Result<Vec<String>, String> {
    validate_oauth_metadata_url(issuer, "issuer")?;
    validate_oauth_client_text(client_id, "client ID", MAX_OAUTH_CLIENT_ID_BYTES)?;
    validate_oauth_client_redirect_uri(redirect_uri)?;
    let parsed_scopes = scopes
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if parsed_scopes.iter().any(|scope| {
        scope.len() > MAX_OAUTH_SCOPE_BYTES
            || scope
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b'"' | b'\\'))
    }) {
        return Err("OAuth/OIDC scope contains an invalid or oversized value".to_string());
    }
    if parsed_scopes.len() > 128 {
        return Err("OAuth/OIDC scope list is too large".to_string());
    }
    Ok(parsed_scopes)
}

fn oauth_client_json_string(document: &Json, field: &str) -> Result<Option<String>, String> {
    let Json::Object(values) = document else {
        return Err("OAuth/OIDC discovery response must be a JSON object".to_string());
    };
    let Some(value) = values.get(field) else {
        return Ok(None);
    };
    let Json::String(value) = value else {
        return Err(format!(
            "OAuth/OIDC discovery field '{field}' must be a string"
        ));
    };
    Ok(Some(value.clone()))
}

fn validate_oauth_endpoint(value: &str, field: &str) -> Result<String, String> {
    validate_oauth_metadata_url(value, field)?;
    Ok(value.to_string())
}

fn oauth_well_known_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

fn oauth_form(fields: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

fn oauth_http_json(method: &str, endpoint: &str, fields: &[(&str, &str)]) -> Result<Json, String> {
    validate_oauth_endpoint(endpoint, "endpoint")?;
    let form = oauth_form(fields);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let response = if method.eq_ignore_ascii_case("GET") {
        agent
            .get(endpoint)
            .header("accept", "application/json")
            .call()
            .map_err(|error| format!("OAuth/OIDC request failed: {error}"))?
    } else {
        let request = ureq::http::Request::builder()
            .method(method)
            .uri(endpoint)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(ureq::SendBody::from_owned_reader(Cursor::new(
                form.into_bytes(),
            )))
            .map_err(|error| format!("OAuth/OIDC request could not be built: {error}"))?;
        agent
            .run(request)
            .map_err(|error| format!("OAuth/OIDC request failed: {error}"))?
    };
    let status = response.status().as_u16();
    let mut body = Vec::new();
    response
        .into_body()
        .into_reader()
        .take((MAX_OAUTH_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("OAuth/OIDC response could not be read: {error}"))?;
    if body.len() > MAX_OAUTH_RESPONSE_BYTES {
        return Err("OAuth/OIDC response exceeds the 1 MiB limit".to_string());
    }
    let text = std::str::from_utf8(&body)
        .map_err(|_| "OAuth/OIDC response is not valid UTF-8".to_string())?;
    let document = if text.trim().is_empty() {
        Json::Null
    } else {
        Json::parse(text)
            .map_err(|error| format!("OAuth/OIDC response is not valid JSON: {error}"))?
    };
    if !(200..300).contains(&status) {
        let description = oauth_client_json_string(&document, "error_description")?
            .or_else(|| oauth_client_json_string(&document, "error").ok().flatten())
            .unwrap_or_else(|| "OAuth/OIDC endpoint returned an error".to_string());
        return Err(format!(
            "OAuth/OIDC endpoint returned HTTP {status}: {description}"
        ));
    }
    Ok(document)
}

fn oauth_client_snapshot(handle: i64) -> Result<OAuthClientEntry, String> {
    OAUTH_CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle)
        .cloned()
        .ok_or_else(|| "invalid OAuthClient handle".to_string())
}

fn oauth_client_result_error(kind: StdErrorKind, error: String) -> *mut Value {
    http_result_err_with_kind(kind, error, 0, String::new(), String::new())
}

fn oauth_client_result_json(result: Result<Json, (StdErrorKind, String)>) -> *mut Value {
    match result {
        Ok(document) => http_result_ok(json_to_value(&document)),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

fn oauth_require_object(document: Json, operation: &str) -> Result<Json, (StdErrorKind, String)> {
    if matches!(document, Json::Object(_)) {
        Ok(document)
    } else {
        Err((
            StdErrorKind::Protocol,
            format!("OAuth/OIDC {operation} response must be a JSON object"),
        ))
    }
}

fn oauth_client_endpoint(
    handle: i64,
    endpoint: Option<String>,
    operation: &str,
) -> Result<(OAuthClientEntry, String), (StdErrorKind, String)> {
    let client = oauth_client_snapshot(handle).map_err(|error| (StdErrorKind::Invalid, error))?;
    let endpoint = endpoint.ok_or_else(|| {
        (
            StdErrorKind::Unsupported,
            format!("OAuth/OIDC discovery did not provide an {operation} endpoint"),
        )
    })?;
    Ok((client, endpoint))
}

/// Allocate an empty OAuth/OIDC client configuration.
#[unsafe(no_mangle)]
pub extern "C" fn mux_net_oauth_client_new() -> *mut Value {
    insert_oauth_client(OAuthClientEntry {
        issuer: String::new(),
        client_id: String::new(),
        redirect_uri: String::new(),
        scopes: Vec::new(),
        authorization_endpoint: None,
        token_endpoint: None,
        revocation_endpoint: None,
        introspection_endpoint: None,
        names: 1,
    })
}

/// Create a configured OAuth/OIDC client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_from_config(
    issuer: *const Value,
    client_id: *const Value,
    redirect_uri: *const Value,
    scopes: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(issuer)) = issuer.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC issuer must be a string".to_string(),
            ));
        };
        let Some(Value::String(client_id)) = client_id.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC client ID must be a string".to_string(),
            ));
        };
        let Some(Value::String(redirect_uri)) = redirect_uri.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC redirect URI must be a string".to_string(),
            ));
        };
        let Some(Value::String(scopes)) = scopes.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC scopes must be a string".to_string(),
            ));
        };
        let scope_values = validate_oauth_client_config(issuer, client_id, redirect_uri, scopes)
            .map_err(|error| (StdErrorKind::Invalid, error))?;
        take_resource_value(insert_oauth_client(OAuthClientEntry {
            issuer: issuer.clone(),
            client_id: client_id.clone(),
            redirect_uri: redirect_uri.clone(),
            scopes: scope_values,
            authorization_endpoint: None,
            token_endpoint: None,
            revocation_endpoint: None,
            introspection_endpoint: None,
            names: 1,
        }))
        .map_err(|error| (StdErrorKind::Transport, error))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

/// Configure an already allocated client. This is useful to code generators
/// that construct class values before invoking their static configuration
/// helper, while the public Mux API uses `OAuthClient.from_config` directly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_configure(
    client: *const Value,
    issuer: *const Value,
    client_id: *const Value,
    redirect_uri: *const Value,
    scopes: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let Some(Value::String(issuer)) = issuer.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC issuer must be a string".to_string(),
            ));
        };
        let Some(Value::String(client_id)) = client_id.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC client ID must be a string".to_string(),
            ));
        };
        let Some(Value::String(redirect_uri)) = redirect_uri.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC redirect URI must be a string".to_string(),
            ));
        };
        let Some(Value::String(scopes)) = scopes.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC scopes must be a string".to_string(),
            ));
        };
        let scope_values = validate_oauth_client_config(issuer, client_id, redirect_uri, scopes)
            .map_err(|error| (StdErrorKind::Invalid, error))?;
        let mut clients = OAUTH_CLIENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = clients.get_mut(&handle).ok_or_else(|| {
            (
                StdErrorKind::Invalid,
                "invalid OAuthClient handle".to_string(),
            )
        })?;
        entry.issuer = issuer.clone();
        entry.client_id = client_id.clone();
        entry.redirect_uri = redirect_uri.clone();
        entry.scopes = scope_values;
        entry.authorization_endpoint = None;
        entry.token_endpoint = None;
        entry.revocation_endpoint = None;
        entry.introspection_endpoint = None;
        Ok(())
    })();
    match result {
        Ok(()) => http_result_ok(Value::Unit),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

/// Fetch and validate the provider's OIDC discovery document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_discover(client: *const Value) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let snapshot =
            oauth_client_snapshot(handle).map_err(|error| (StdErrorKind::Invalid, error))?;
        if snapshot.issuer.is_empty() {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC client is not configured".to_string(),
            ));
        }
        let document = oauth_http_json("GET", &oauth_well_known_url(&snapshot.issuer), &[])
            .map_err(|error| (StdErrorKind::Transport, error))?;
        if let Some(discovered_issuer) = oauth_client_json_string(&document, "issuer")
            .map_err(|error| (StdErrorKind::Protocol, error))?
        {
            if discovered_issuer != snapshot.issuer {
                return Err((
                    StdErrorKind::Protocol,
                    "OAuth/OIDC discovery issuer does not match the configured issuer".to_string(),
                ));
            }
        }
        let authorization_endpoint = oauth_client_json_string(&document, "authorization_endpoint")
            .map_err(|error| (StdErrorKind::Protocol, error))?
            .ok_or_else(|| {
                (
                    StdErrorKind::Protocol,
                    "OAuth/OIDC discovery has no authorization_endpoint".to_string(),
                )
            })
            .and_then(|endpoint| {
                validate_oauth_endpoint(&endpoint, "authorization endpoint")
                    .map_err(|error| (StdErrorKind::Protocol, error))
            })?;
        let token_endpoint = oauth_client_json_string(&document, "token_endpoint")
            .map_err(|error| (StdErrorKind::Protocol, error))?
            .ok_or_else(|| {
                (
                    StdErrorKind::Protocol,
                    "OAuth/OIDC discovery has no token_endpoint".to_string(),
                )
            })
            .and_then(|endpoint| {
                validate_oauth_endpoint(&endpoint, "token endpoint")
                    .map_err(|error| (StdErrorKind::Protocol, error))
            })?;
        let revocation_endpoint = oauth_client_json_string(&document, "revocation_endpoint")
            .map_err(|error| (StdErrorKind::Protocol, error))?
            .map(|endpoint| validate_oauth_endpoint(&endpoint, "revocation endpoint"))
            .transpose()
            .map_err(|error| (StdErrorKind::Protocol, error))?;
        let introspection_endpoint = oauth_client_json_string(&document, "introspection_endpoint")
            .map_err(|error| (StdErrorKind::Protocol, error))?
            .map(|endpoint| validate_oauth_endpoint(&endpoint, "introspection endpoint"))
            .transpose()
            .map_err(|error| (StdErrorKind::Protocol, error))?;
        let mut clients = OAUTH_CLIENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = clients.get_mut(&handle).ok_or_else(|| {
            (
                StdErrorKind::Invalid,
                "invalid OAuthClient handle".to_string(),
            )
        })?;
        entry.authorization_endpoint = Some(authorization_endpoint);
        entry.token_endpoint = Some(token_endpoint);
        entry.revocation_endpoint = revocation_endpoint;
        entry.introspection_endpoint = introspection_endpoint;
        Ok(document)
    })();
    oauth_client_result_json(result)
}

/// Build an OAuth authorization-code URL with S256 PKCE and an OIDC nonce.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_authorization_url(
    client: *const Value,
    state: *const Value,
    code_verifier: *const Value,
    nonce: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let snapshot =
            oauth_client_snapshot(handle).map_err(|error| (StdErrorKind::Invalid, error))?;
        let endpoint = snapshot.authorization_endpoint.ok_or_else(|| {
            (
                StdErrorKind::Unsupported,
                "OAuth/OIDC discovery has not provided an authorization endpoint".to_string(),
            )
        })?;
        let Some(Value::String(state)) = state.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC state must be a string".to_string(),
            ));
        };
        let Some(Value::String(code_verifier)) = code_verifier.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC code verifier must be a string".to_string(),
            ));
        };
        let Some(Value::String(nonce)) = nonce.as_ref() else {
            return Err((
                StdErrorKind::Invalid,
                "OIDC nonce must be a string".to_string(),
            ));
        };
        validate_oauth_client_text(state, "state", 4096)
            .map_err(|error| (StdErrorKind::Invalid, error))?;
        validate_oauth_client_text(code_verifier, "code verifier", 128)
            .map_err(|error| (StdErrorKind::Invalid, error))?;
        if !(43..=128).contains(&code_verifier.len()) {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC code verifier must contain 43 to 128 characters".to_string(),
            ));
        }
        if !code_verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
        {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC code verifier must use PKCE unreserved characters".to_string(),
            ));
        }
        validate_oauth_client_text(nonce, "nonce", 4096)
            .map_err(|error| (StdErrorKind::Invalid, error))?;
        let challenge = BASE64_URL_SAFE.encode(digest(&SHA256, code_verifier.as_bytes()).as_ref());
        let scope = snapshot.scopes.join(" ");
        let mut authorization_url = endpoint;
        authorization_url.push('?');
        let mut serializer = url::form_urlencoded::Serializer::new(authorization_url);
        serializer.append_pair("response_type", "code");
        serializer.append_pair("client_id", &snapshot.client_id);
        serializer.append_pair("redirect_uri", &snapshot.redirect_uri);
        if !scope.is_empty() {
            serializer.append_pair("scope", &scope);
        }
        serializer.append_pair("state", state);
        serializer.append_pair("code_challenge", &challenge);
        serializer.append_pair("code_challenge_method", "S256");
        serializer.append_pair("nonce", nonce);
        let value = serializer.finish();
        if value.len() > 64 * 1024 {
            return Err((
                StdErrorKind::Overflow,
                "OAuth/OIDC authorization URL is too long".to_string(),
            ));
        }
        Ok(value)
    })();
    match result {
        Ok(value) => http_result_ok(Value::String(value)),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

fn oauth_token_field(value: *const Value, field: &str) -> Result<String, (StdErrorKind, String)> {
    let Some(Value::String(value)) = (unsafe { value.as_ref() }) else {
        return Err((
            StdErrorKind::Invalid,
            format!("OAuth/OIDC {field} must be a string"),
        ));
    };
    validate_oauth_client_text(value, field, MAX_OAUTH_TOKEN_BYTES)
        .map_err(|error| (StdErrorKind::Invalid, error))?;
    Ok(value.clone())
}

/// Exchange an authorization code for the provider's token document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_exchange_code(
    client: *const Value,
    code: *const Value,
    code_verifier: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let (snapshot, endpoint) = oauth_client_endpoint(
            handle,
            oauth_client_snapshot(handle)
                .map_err(|error| (StdErrorKind::Invalid, error))?
                .token_endpoint,
            "token",
        )?;
        let code = oauth_token_field(code, "authorization code")?;
        let verifier = oauth_token_field(code_verifier, "code verifier")?;
        let document = oauth_http_json(
            "POST",
            &endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &snapshot.redirect_uri),
                ("client_id", &snapshot.client_id),
                ("code_verifier", &verifier),
            ],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        oauth_require_object(document, "token")
    })();
    oauth_client_result_json(result)
}

/// Exchange a refresh token for a new token document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_refresh(
    client: *const Value,
    refresh_token: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let (snapshot, endpoint) = oauth_client_endpoint(
            handle,
            oauth_client_snapshot(handle)
                .map_err(|error| (StdErrorKind::Invalid, error))?
                .token_endpoint,
            "token",
        )?;
        let refresh_token = oauth_token_field(refresh_token, "refresh token")?;
        let document = oauth_http_json(
            "POST",
            &endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", &snapshot.client_id),
            ],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        oauth_require_object(document, "refresh")
    })();
    oauth_client_result_json(result)
}

/// Revoke a token when the provider advertises RFC 7009 support.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_revoke(
    client: *const Value,
    token: *const Value,
    token_type: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let (snapshot, endpoint) = oauth_client_endpoint(
            handle,
            oauth_client_snapshot(handle)
                .map_err(|error| (StdErrorKind::Invalid, error))?
                .revocation_endpoint,
            "revocation",
        )?;
        let token = oauth_token_field(token, "token")?;
        let token_type = oauth_token_field(token_type, "token type")?;
        if !matches!(token_type.as_str(), "access_token" | "refresh_token") {
            return Err((
                StdErrorKind::Invalid,
                "OAuth/OIDC token type must be access_token or refresh_token".to_string(),
            ));
        }
        oauth_http_json(
            "POST",
            &endpoint,
            &[
                ("token", &token),
                ("token_type_hint", &token_type),
                ("client_id", &snapshot.client_id),
            ],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        Ok(())
    })();
    match result {
        Ok(()) => http_result_ok(Value::Unit),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

/// Query a provider's RFC 7662 introspection endpoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_client_introspect(
    client: *const Value,
    token: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let (snapshot, endpoint) = oauth_client_endpoint(
            handle,
            oauth_client_snapshot(handle)
                .map_err(|error| (StdErrorKind::Invalid, error))?
                .introspection_endpoint,
            "introspection",
        )?;
        let token = oauth_token_field(token, "token")?;
        let document = oauth_http_json(
            "POST",
            &endpoint,
            &[("token", &token), ("client_id", &snapshot.client_id)],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        oauth_require_object(document, "introspection")
    })();
    oauth_client_result_json(result)
}

fn oauth_session_json_string(document: &Json, field: &str) -> Result<Option<String>, String> {
    let Json::Object(values) = document else {
        return Err("OAuth/OIDC token response must be a JSON object".to_string());
    };
    let Some(value) = values.get(field) else {
        return Ok(None);
    };
    let Json::String(value) = value else {
        return Err(format!("OAuth/OIDC token field '{field}' must be a string"));
    };
    Ok(Some(value.clone()))
}

fn oauth_session_from_json(document: &Json) -> Result<OAuthSessionEntry, String> {
    if !matches!(document, Json::Object(_)) {
        return Err("OAuth/OIDC token response must be a JSON object".to_string());
    }
    let access_token = oauth_session_json_string(document, "access_token")?
        .ok_or_else(|| "OAuth/OIDC token response has no access_token".to_string())?;
    validate_oauth_client_text(&access_token, "access token", MAX_OAUTH_TOKEN_BYTES)?;
    let refresh_token = oauth_session_json_string(document, "refresh_token")?;
    if let Some(refresh_token) = &refresh_token {
        validate_oauth_client_text(refresh_token, "refresh token", MAX_OAUTH_TOKEN_BYTES)?;
    }
    let id_token = oauth_session_json_string(document, "id_token")?;
    if let Some(id_token) = &id_token {
        validate_oauth_client_text(id_token, "ID token", MAX_OAUTH_TOKEN_BYTES)?;
    }
    let token_type =
        oauth_session_json_string(document, "token_type")?.unwrap_or_else(|| "Bearer".to_string());
    validate_oauth_client_text(&token_type, "token type", 64)?;
    let expires_at = match document {
        Json::Object(values) => match values.get("expires_in") {
            None => None,
            Some(Json::Int(seconds)) if *seconds >= 0 => Some(
                Instant::now()
                    .checked_add(Duration::from_secs(*seconds as u64))
                    .ok_or_else(|| "OAuth/OIDC expires_in is too large".to_string())?,
            ),
            Some(Json::Number(number)) => {
                let seconds = number.as_str().parse::<u64>().map_err(|_| {
                    "OAuth/OIDC expires_in must be a non-negative integer".to_string()
                })?;
                Some(
                    Instant::now()
                        .checked_add(Duration::from_secs(seconds))
                        .ok_or_else(|| "OAuth/OIDC expires_in is too large".to_string())?,
                )
            }
            Some(_) => {
                return Err("OAuth/OIDC expires_in must be a non-negative integer".to_string());
            }
        },
        _ => None,
    };
    Ok(OAuthSessionEntry {
        access_token,
        refresh_token,
        id_token,
        token_type,
        expires_at,
        names: 1,
    })
}

fn oauth_session_result(result: Result<Value, (StdErrorKind, String)>) -> *mut Value {
    match result {
        Ok(value) => http_result_ok(value),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

/// Create an owned token session from a successful OAuth token response.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_from_token_response(
    response: *const Value,
) -> *mut Value {
    let result = (|| {
        let response = response.as_ref().ok_or_else(|| {
            (
                StdErrorKind::Invalid,
                "OAuth/OIDC token response is null".to_string(),
            )
        })?;
        let document = value_to_json(response).map_err(|error| {
            (
                StdErrorKind::Invalid,
                format!("invalid OAuth/OIDC token response: {error}"),
            )
        })?;
        let entry =
            oauth_session_from_json(&document).map_err(|error| (StdErrorKind::Protocol, error))?;
        take_resource_value(insert_oauth_session(entry))
            .map_err(|error| (StdErrorKind::Transport, error))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err((kind, error)) => oauth_client_result_error(kind, error),
    }
}

/// Return the access token held by a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_access_token(session: *const Value) -> *mut Value {
    let result = oauth_session_handle(session)
        .and_then(|handle| {
            OAUTH_SESSIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&handle)
                .map(|entry| entry.access_token.clone())
                .ok_or_else(|| "invalid OAuthSession handle".to_string())
        })
        .map(Value::String)
        .map_err(|error| (StdErrorKind::Invalid, error));
    oauth_session_result(result)
}

/// Return the optional refresh token held by a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_refresh_token(session: *const Value) -> *mut Value {
    let result = oauth_session_handle(session)
        .and_then(|handle| {
            OAUTH_SESSIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&handle)
                .map(|entry| {
                    Value::Optional(
                        entry
                            .refresh_token
                            .clone()
                            .map(|value| Box::new(Value::String(value))),
                    )
                })
                .ok_or_else(|| "invalid OAuthSession handle".to_string())
        })
        .map_err(|error| (StdErrorKind::Invalid, error));
    oauth_session_result(result)
}

/// Return the optional OIDC ID token held by a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_id_token(session: *const Value) -> *mut Value {
    let result = oauth_session_handle(session)
        .and_then(|handle| {
            OAUTH_SESSIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&handle)
                .map(|entry| {
                    Value::Optional(
                        entry
                            .id_token
                            .clone()
                            .map(|value| Box::new(Value::String(value))),
                    )
                })
                .ok_or_else(|| "invalid OAuthSession handle".to_string())
        })
        .map_err(|error| (StdErrorKind::Invalid, error));
    oauth_session_result(result)
}

/// Return the token type recorded in a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_token_type(session: *const Value) -> *mut Value {
    let result = oauth_session_handle(session)
        .and_then(|handle| {
            OAUTH_SESSIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&handle)
                .map(|entry| entry.token_type.clone())
                .ok_or_else(|| "invalid OAuthSession handle".to_string())
        })
        .map(Value::String)
        .map_err(|error| (StdErrorKind::Invalid, error));
    oauth_session_result(result)
}

/// Return whether a session has passed its provider-supplied expiry time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_is_expired(session: *const Value) -> *mut Value {
    let result = oauth_session_handle(session)
        .and_then(|handle| {
            OAUTH_SESSIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&handle)
                .map(|entry| {
                    Value::Bool(entry.expires_at.is_some_and(|time| Instant::now() >= time))
                })
                .ok_or_else(|| "invalid OAuthSession handle".to_string())
        })
        .map_err(|error| (StdErrorKind::Invalid, error));
    oauth_session_result(result)
}

/// Refresh a session and return a new session containing the provider response.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_refresh(
    session: *const Value,
    client: *const Value,
) -> *mut Value {
    let result = (|| {
        let session_handle =
            oauth_session_handle(session).map_err(|error| (StdErrorKind::Invalid, error))?;
        let client_handle =
            oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let refresh_token = OAUTH_SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .and_then(|entry| entry.refresh_token.clone())
            .ok_or_else(|| {
                (
                    StdErrorKind::Unsupported,
                    "OAuthSession has no refresh token".to_string(),
                )
            })?;
        let snapshot =
            oauth_client_snapshot(client_handle).map_err(|error| (StdErrorKind::Invalid, error))?;
        let endpoint = snapshot.token_endpoint.ok_or_else(|| {
            (
                StdErrorKind::Unsupported,
                "OAuth/OIDC discovery has not provided a token endpoint".to_string(),
            )
        })?;
        let document = oauth_http_json(
            "POST",
            &endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", &snapshot.client_id),
            ],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        let entry =
            oauth_session_from_json(&document).map_err(|error| (StdErrorKind::Protocol, error))?;
        let value = take_resource_value(insert_oauth_session(entry))
            .map_err(|error| (StdErrorKind::Transport, error))?;
        Ok(value)
    })();
    oauth_session_result(result)
}

/// Revoke one of the tokens held by a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_revoke(
    session: *const Value,
    client: *const Value,
    token_type: *const Value,
) -> *mut Value {
    let result = (|| {
        let session_handle =
            oauth_session_handle(session).map_err(|error| (StdErrorKind::Invalid, error))?;
        let client_handle =
            oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let token_type = oauth_token_field(token_type, "token type")?;
        let token = OAUTH_SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .and_then(|entry| match token_type.as_str() {
                "access_token" => Some(entry.access_token.clone()),
                "refresh_token" => entry.refresh_token.clone(),
                _ => None,
            })
            .ok_or_else(|| {
                (
                    StdErrorKind::Invalid,
                    "OAuth/OIDC token type is invalid or the token is unavailable".to_string(),
                )
            })?;
        let snapshot =
            oauth_client_snapshot(client_handle).map_err(|error| (StdErrorKind::Invalid, error))?;
        let endpoint = snapshot.revocation_endpoint.ok_or_else(|| {
            (
                StdErrorKind::Unsupported,
                "OAuth/OIDC discovery has not provided a revocation endpoint".to_string(),
            )
        })?;
        oauth_http_json(
            "POST",
            &endpoint,
            &[
                ("token", &token),
                ("token_type_hint", &token_type),
                ("client_id", &snapshot.client_id),
            ],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        Ok(Value::Unit)
    })();
    oauth_session_result(result)
}

/// Introspect the access token held by a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_introspect(
    session: *const Value,
    client: *const Value,
) -> *mut Value {
    let result = (|| {
        let session_handle =
            oauth_session_handle(session).map_err(|error| (StdErrorKind::Invalid, error))?;
        let client_handle =
            oauth_client_handle(client).map_err(|error| (StdErrorKind::Invalid, error))?;
        let token = OAUTH_SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .map(|entry| entry.access_token.clone())
            .ok_or_else(|| {
                (
                    StdErrorKind::Invalid,
                    "invalid OAuthSession handle".to_string(),
                )
            })?;
        let snapshot =
            oauth_client_snapshot(client_handle).map_err(|error| (StdErrorKind::Invalid, error))?;
        let endpoint = snapshot.introspection_endpoint.ok_or_else(|| {
            (
                StdErrorKind::Unsupported,
                "OAuth/OIDC discovery has not provided an introspection endpoint".to_string(),
            )
        })?;
        let document = oauth_http_json(
            "POST",
            &endpoint,
            &[("token", &token), ("client_id", &snapshot.client_id)],
        )
        .map_err(|error| (StdErrorKind::Authentication, error))?;
        oauth_require_object(document, "introspection")
    })();
    oauth_client_result_json(result)
}

/// Close a session and invalidate all values naming its handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_oauth_session_close(session: *mut Value) {
    if let Ok(handle) = oauth_session_handle(session) {
        OAUTH_SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        write_handle(session, 0);
    }
}

fn oauth_oidc_transport_response(method: &str, url: &str, detail: String) -> *mut Value {
    http_result_err_with_kind(
        StdErrorKind::Transport,
        detail,
        503,
        method.to_string(),
        url.to_string(),
    )
}

fn oauth_oidc_jwks(jwks_url: &str) -> Result<Vec<OAuthRsaKey>, String> {
    const CACHE_TTL: Duration = Duration::from_secs(300);
    if let Some((_fetched, keys)) = OAUTH_JWKS_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(jwks_url)
        .filter(|(fetched, _)| fetched.elapsed() < CACHE_TTL)
    {
        return Ok(keys.clone());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let response = agent
        .get(jwks_url)
        .call()
        .map_err(|error| format!("OAuth/OIDC JWKS request failed: {error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("OAuth/OIDC JWKS endpoint returned HTTP {status}"));
    }
    let mut body = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(1_048_577)
        .read_to_end(&mut body)
        .map_err(|error| format!("OAuth/OIDC JWKS response could not be read: {error}"))?;
    if body.len() > 1_048_576 {
        return Err("OAuth/OIDC JWKS response exceeds the 1 MiB limit".to_string());
    }
    let document: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| format!("OAuth/OIDC JWKS response is not valid JSON: {error}"))?;
    let key_values = document
        .get("keys")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "OAuth/OIDC JWKS response has no keys array".to_string())?;
    let mut keys = Vec::new();
    for key in key_values {
        if key.get("kty").and_then(serde_json::Value::as_str) != Some("RSA")
            || key
                .get("use")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|use_value| use_value != "sig")
            || key
                .get("alg")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|algorithm| algorithm != "RS256")
        {
            continue;
        }
        let Some(kid) = key.get("kid").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(modulus) = key.get("n").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(exponent) = key.get("e").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(modulus) = BASE64_URL_SAFE.decode(modulus) else {
            continue;
        };
        let Ok(exponent) = BASE64_URL_SAFE.decode(exponent) else {
            continue;
        };
        if modulus.len() < 256 || exponent.is_empty() || modulus.len() > 1024 {
            continue;
        }
        keys.push(OAuthRsaKey {
            kid: kid.to_string(),
            modulus,
            exponent,
        });
    }
    if keys.is_empty() {
        return Err("OAuth/OIDC JWKS response contains no usable RS256 keys".to_string());
    }
    OAUTH_JWKS_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(jwks_url.to_string(), (Instant::now(), keys.clone()));
    Ok(keys)
}

fn oauth_oidc_token_is_valid(
    token: &str,
    issuer: &str,
    audience: &str,
    jwks_url: &str,
) -> Result<bool, String> {
    let mut parts = token.split('.');
    let Some(encoded_header) = parts.next() else {
        return Ok(false);
    };
    let Some(encoded_claims) = parts.next() else {
        return Ok(false);
    };
    let Some(encoded_signature) = parts.next() else {
        return Ok(false);
    };
    if parts.next().is_some() {
        return Ok(false);
    }
    let decode = |part: &str| {
        BASE64_URL_SAFE
            .decode(part)
            .map_err(|error| format!("OAuth/OIDC JWT encoding is invalid: {error}"))
    };
    let Ok(header_bytes) = decode(encoded_header) else {
        return Ok(false);
    };
    let Ok(claims_bytes) = decode(encoded_claims) else {
        return Ok(false);
    };
    let header: serde_json::Value = match serde_json::from_slice(&header_bytes) {
        Ok(header) => header,
        Err(_) => return Ok(false),
    };
    let claims: serde_json::Value = match serde_json::from_slice(&claims_bytes) {
        Ok(claims) => claims,
        Err(_) => return Ok(false),
    };
    if header.get("alg").and_then(serde_json::Value::as_str) != Some("RS256") {
        return Ok(false);
    }
    let Some(kid) = header.get("kid").and_then(serde_json::Value::as_str) else {
        return Ok(false);
    };
    if claims.get("iss").and_then(serde_json::Value::as_str) != Some(issuer) {
        return Ok(false);
    }
    let audience_matches = claims.get("aud").is_some_and(|value| {
        value.as_str().is_some_and(|value| value == audience)
            || value.as_array().is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value.as_str().is_some_and(|value| value == audience))
            })
    });
    if !audience_matches {
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("OAuth/OIDC clock is before the Unix epoch: {error}"))?
        .as_secs();
    let Some(expires_at) = claims.get("exp").and_then(serde_json::Value::as_u64) else {
        return Ok(false);
    };
    if expires_at <= now
        || claims
            .get("nbf")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|not_before| not_before > now)
    {
        return Ok(false);
    }
    let Ok(signature) = decode(encoded_signature) else {
        return Ok(false);
    };
    let keys = oauth_oidc_jwks(jwks_url)?;
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    Ok(keys.iter().filter(|key| key.kid == kid).any(|key| {
        RsaPublicKeyComponents {
            n: &key.modulus,
            e: &key.exponent,
        }
        .verify(
            &RSA_PKCS1_2048_8192_SHA256,
            signing_input.as_bytes(),
            &signature,
        )
        .is_ok()
    }))
}

fn unauthorized_response(scheme: &str, method: &str, url: &str) -> *mut Value {
    let response = insert_http_response(HttpResponseEntry {
        status: 401,
        headers: Arc::new(Mutex::new(HeaderData {
            values: vec![
                (
                    "www-authenticate".to_string(),
                    format!("{scheme} realm=\"Mux\""),
                ),
                (
                    "content-type".to_string(),
                    "text/plain; charset=utf-8".to_string(),
                ),
            ],
        })),
        body: b"unauthorized".to_vec(),
        body_reader: None,
        streamed_bytes: 0,
        position: 0,
        names: 1,
    });
    if response.is_null() {
        return http_result_err("failed to allocate HTTP authentication response".to_string());
    }
    let value = unsafe { (&*response).clone() };
    unsafe { mux_rc_dec(response) };
    let _ = (method, url);
    http_result_ok(value)
}

fn valid_route_capture_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn route_path_segments(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty() || !path.starts_with('/') {
        return Err("HTTP route path must start with '/'".to_string());
    }
    Ok(if path == "/" {
        Vec::new()
    } else {
        path[1..].split('/').collect()
    })
}

fn percent_decode_route_segment(segment: &str) -> Result<String, String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err("HTTP route path contains malformed percent-encoding".to_string());
        }
        let high = (bytes[index + 1] as char)
            .to_digit(16)
            .ok_or_else(|| "HTTP route path contains malformed percent-encoding".to_string())?;
        let low = (bytes[index + 2] as char)
            .to_digit(16)
            .ok_or_else(|| "HTTP route path contains malformed percent-encoding".to_string())?;
        decoded.push(((high << 4) | low) as u8);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| "HTTP route path contains invalid UTF-8".to_string())
}

fn compile_http_route(path: &str) -> Result<(Vec<HttpRouteSegment>, Vec<i32>), String> {
    if path.contains('?') {
        return Err("HTTP route patterns must not contain a query string".to_string());
    }
    let raw_segments = route_path_segments(path)?;
    let mut names = HashSet::new();
    let mut segments = Vec::with_capacity(raw_segments.len());
    let mut specificity = Vec::with_capacity(raw_segments.len());
    let mut has_catch_all = false;
    for (index, raw) in raw_segments.iter().enumerate() {
        let segment = if raw.starts_with('{') || raw.ends_with('}') {
            if !raw.starts_with('{') || !raw.ends_with('}') || raw.len() < 3 {
                return Err("HTTP route capture has invalid braces".to_string());
            }
            let name = &raw[1..raw.len() - 1];
            let (is_catch_all, name) = name
                .strip_prefix("...")
                .map_or((false, name), |name| (true, name));
            if !valid_route_capture_name(name) {
                return Err(format!("HTTP route capture name is invalid: {name}"));
            }
            if !names.insert(name.to_string()) {
                return Err(format!("HTTP route capture name is duplicated: {name}"));
            }
            if is_catch_all && index + 1 != raw_segments.len() {
                return Err("HTTP catch-all capture must be the final path segment".to_string());
            }
            if is_catch_all {
                has_catch_all = true;
                specificity.push(0);
                HttpRouteSegment::CatchAll(name.to_string())
            } else {
                specificity.push(1);
                HttpRouteSegment::Parameter(name.to_string())
            }
        } else {
            if raw.contains('{') || raw.contains('}') {
                return Err("HTTP route literal contains an unmatched brace".to_string());
            }
            specificity.push(2);
            HttpRouteSegment::Literal(percent_decode_route_segment(raw)?)
        };
        segments.push(segment);
    }
    if !has_catch_all {
        // An exact end is more specific than a catch-all that happens to
        // capture zero segments after the same prefix.
        specificity.push(3);
    }
    Ok((segments, specificity))
}

fn request_path_segments(url: &str) -> Result<Vec<String>, String> {
    let path = url.split('?').next().unwrap_or_default();
    let raw_segments = route_path_segments(path)?;
    raw_segments
        .into_iter()
        .map(percent_decode_route_segment)
        .collect()
}

fn http_route_match(
    route: &HttpRouteEntry,
    path_segments: &[String],
) -> Option<HashMap<String, String>> {
    let mut captures = HashMap::new();
    let mut path_index = 0;
    for segment in &route.segments {
        match segment {
            HttpRouteSegment::Literal(expected) => {
                if path_segments.get(path_index) != Some(expected) {
                    return None;
                }
                path_index += 1;
            }
            HttpRouteSegment::Parameter(name) => {
                let value = path_segments.get(path_index)?.clone();
                captures.insert(name.clone(), value);
                path_index += 1;
            }
            HttpRouteSegment::CatchAll(name) => {
                captures.insert(name.clone(), path_segments[path_index..].join("/"));
                path_index = path_segments.len();
            }
        }
    }
    (path_index == path_segments.len()).then_some(captures)
}

fn http_router_route_match(
    router: i64,
    method: &str,
    url: &str,
) -> Result<Option<HttpRouteMatch>, String> {
    let path_segments = request_path_segments(url)?;
    let routers = lock_http_routers();
    let entry = routers
        .get(&router)
        .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
    let data = entry
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut selected: Option<(usize, HashMap<String, String>)> = None;
    for (index, route) in data.routes.iter().enumerate() {
        if !route.method.eq_ignore_ascii_case(method) {
            continue;
        }
        let Some(captures) = http_route_match(route, &path_segments) else {
            continue;
        };
        let is_better = selected.as_ref().is_none_or(|(selected_index, _)| {
            route.specificity > data.routes[*selected_index].specificity
        });
        if is_better {
            selected = Some((index, captures));
        }
    }
    Ok(selected)
}

fn http_router_dispatch(router: i64, request: *mut Value, stage: usize) -> *mut Value {
    let (method, url) = match http_router_request_data(request) {
        Ok(data) => data,
        Err(error) => return http_result_err(error),
    };
    let route_match = match http_router_route_match(router, &method, &url) {
        Ok(route_match) => route_match,
        Err(error) => {
            return http_result_err_with_kind(StdErrorKind::Invalid, error, 400, method, url);
        }
    };
    if stage == 0 {
        if let Err(error) = bind_http_route_captures(request, router, &route_match) {
            return http_result_err(error);
        }
    }
    let middleware = match http_router_middleware(router, stage) {
        Ok(middleware) => middleware,
        Err(error) => return http_result_err(error),
    };
    if let Some(result) =
        apply_http_router_middleware(router, request, stage, middleware, &method, &url)
    {
        return result;
    }

    let callback = match http_router_callback(router, &route_match) {
        Ok(callback) => callback,
        Err(error) => return http_result_err(error),
    };
    if let Some(callback) = callback {
        return match unsafe { invoke_http_callback(callback, &[request]) } {
            Ok(result) => result,
            Err(error) => http_result_err(error),
        };
    }

    http_result_err_with_kind(
        StdErrorKind::Status,
        "HTTP route not found".to_string(),
        404,
        method,
        url,
    )
}

fn http_router_request_data(request: *mut Value) -> Result<(String, String), String> {
    let handle = request_handle(request)?;
    let requests = lock_requests();
    let entry = requests
        .get(&handle)
        .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
    Ok((entry.method.to_ascii_uppercase(), entry.url.clone()))
}

fn bind_http_route_captures(
    request: *mut Value,
    router: i64,
    route_match: &Option<HttpRouteMatch>,
) -> Result<(), String> {
    let request_handle = request_handle(request)?;
    let path_params = {
        let requests = lock_requests();
        requests
            .get(&request_handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .path_params
            .clone()
    };
    let mut path_params = path_params
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    path_params.clear();
    let Some((route_index, captures)) = route_match else {
        return Ok(());
    };
    let routers = lock_http_routers();
    let Some(router_entry) = routers.get(&router) else {
        return Ok(());
    };
    let data = router_entry
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if data.routes.get(*route_index).is_some() {
        path_params.extend(
            captures
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }
    Ok(())
}

fn clone_http_middleware(entry: &HttpMiddlewareEntry) -> HttpMiddlewareEntry {
    match entry {
        HttpMiddlewareEntry::Callback(callback) => HttpMiddlewareEntry::Callback(*callback),
        HttpMiddlewareEntry::Basic { username, password } => HttpMiddlewareEntry::Basic {
            username: username.clone(),
            password: password.clone(),
        },
        HttpMiddlewareEntry::Bearer { token } => HttpMiddlewareEntry::Bearer {
            token: token.clone(),
        },
        HttpMiddlewareEntry::OAuthOidc {
            issuer,
            audience,
            jwks_url,
        } => HttpMiddlewareEntry::OAuthOidc {
            issuer: issuer.clone(),
            audience: audience.clone(),
            jwks_url: jwks_url.clone(),
        },
    }
}

fn http_router_middleware(
    router: i64,
    stage: usize,
) -> Result<Option<HttpMiddlewareEntry>, String> {
    let routers = lock_http_routers();
    let entry = routers
        .get(&router)
        .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
    let data = entry
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(data.middleware.get(stage).map(clone_http_middleware))
}

fn apply_http_router_middleware(
    router: i64,
    request: *mut Value,
    stage: usize,
    middleware: Option<HttpMiddlewareEntry>,
    method: &str,
    url: &str,
) -> Option<*mut Value> {
    let middleware = middleware?;
    let next_stage = stage + 1;
    Some(match middleware {
        HttpMiddlewareEntry::Basic { username, password } => {
            if basic_auth_matches(request, &username, &password) {
                http_router_dispatch(router, request, next_stage)
            } else {
                unauthorized_response("Basic", method, url)
            }
        }
        HttpMiddlewareEntry::Bearer { token } => {
            if bearer_auth_matches(request, &token) {
                http_router_dispatch(router, request, next_stage)
            } else {
                unauthorized_response("Bearer", method, url)
            }
        }
        HttpMiddlewareEntry::OAuthOidc {
            issuer,
            audience,
            jwks_url,
        } => {
            let Some(token) = bearer_token(request) else {
                return Some(unauthorized_response("Bearer", method, url));
            };
            match oauth_oidc_token_is_valid(&token, &issuer, &audience, &jwks_url) {
                Ok(true) => http_router_dispatch(router, request, next_stage),
                Ok(false) => unauthorized_response("Bearer", method, url),
                Err(error) => oauth_oidc_transport_response(method, url, error),
            }
        }
        HttpMiddlewareEntry::Callback(callback) => {
            invoke_http_router_middleware_callback(router, request, callback, next_stage)
        }
    })
}

fn invoke_http_router_middleware_callback(
    router: i64,
    request: *mut Value,
    callback: usize,
    next_stage: usize,
) -> *mut Value {
    let next = insert_http_next(router, next_stage);
    if next.is_null() {
        return http_result_err("failed to allocate HttpNext handle".to_string());
    }
    let result = unsafe { invoke_http_callback(callback as *mut c_void, &[request, next]) };
    unsafe { mux_rc_dec(next) };
    match result {
        Ok(result) => result,
        Err(error) => http_result_err(error),
    }
}

fn http_router_callback(
    router: i64,
    route_match: &Option<HttpRouteMatch>,
) -> Result<Option<*mut c_void>, String> {
    let routers = lock_http_routers();
    let entry = routers
        .get(&router)
        .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
    let data = entry
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(route_match
        .as_ref()
        .and_then(|(index, _)| data.routes.get(*index))
        .map(|route| route.handler as *mut c_void))
}

struct HttpRequestExecution<'a> {
    method: &'a str,
    url: &'a str,
    proxy: Option<&'a str>,
    headers: &'a [(String, String)],
    raw_body: Option<&'a [u8]>,
    body_reader: Option<ReaderAdapter>,
    body_json: Option<&'a Json>,
    options: HttpRequestOptions,
}

#[cfg(feature = "http2")]
struct BlockingAsync<T>(T);

#[cfg(feature = "http2")]
impl<T: Read + Unpin> AsyncRead for BlockingAsync<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let destination = buffer.initialize_unfilled();
        match self.0.read(destination) {
            Ok(0) => Poll::Ready(Ok(())),
            Ok(read) => {
                buffer.advance(read);
                Poll::Ready(Ok(()))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

#[cfg(feature = "http2")]
impl<T: Write + Unpin> AsyncWrite for BlockingAsync<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(self.0.write(buffer))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(self.0.flush())
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(self.0.flush())
    }
}

#[cfg(feature = "http2")]
async fn execute_http2_request(
    sender: h2::client::SendRequest<Bytes>,
    message: Http2RequestMessage,
) -> Result<(), String> {
    let Http2RequestMessage {
        request,
        body,
        timeout,
        cancelled,
        reply,
    } = message;
    let request_future = async {
        if cancelled.load(Ordering::Acquire) {
            return Err("HTTP/2 request was cancelled before dispatch".to_string());
        }
        let end_stream = body.as_ref().is_none_or(Vec::is_empty);
        let mut sender = sender
            .ready()
            .await
            .map_err(|error| format!("HTTP/2 request readiness failed: {error}"))?;
        let (response, mut send_stream) = sender
            .send_request(request, end_stream)
            .map_err(|error| format!("HTTP/2 request send failed: {error}"))?;
        if let Some(body) = body {
            if !body.is_empty() {
                send_stream
                    .send_data(Bytes::from(body), true)
                    .map_err(|error| format!("HTTP/2 request body failed: {error}"))?;
            }
        }
        let response = response
            .await
            .map_err(|error| format!("HTTP/2 response headers failed: {error}"))?;
        let status = i64::from(response.status().as_u16());
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                let value = value
                    .to_str()
                    .map_err(|error| format!("invalid HTTP/2 response header: {error}"))?;
                Ok((name.as_str().to_string(), value.to_string()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut receive_stream = response.into_body();
        let mut response_body = Vec::new();
        while let Some(chunk) = receive_stream.data().await {
            if cancelled.load(Ordering::Acquire) {
                return Err("HTTP/2 request was cancelled".to_string());
            }
            let chunk = chunk.map_err(|error| format!("HTTP/2 response body failed: {error}"))?;
            if response_body
                .len()
                .checked_add(chunk.len())
                .is_none_or(|length| length > MAX_HTTP_BODY_BYTES)
            {
                return Err(format!(
                    "HTTP response body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"
                ));
            }
            let length = chunk.len();
            response_body.extend_from_slice(&chunk);
            receive_stream
                .flow_control()
                .release_capacity(length)
                .map_err(|error| format!("HTTP/2 flow-control update failed: {error}"))?;
        }
        Ok(HttpResponseData {
            status,
            headers,
            body: response_body,
            body_reader: None,
        })
    };
    let cancellation = async {
        loop {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(HTTP2_IO_POLL_INTERVAL).await;
        }
    };
    tokio::pin!(cancellation);
    let result = match timeout {
        Some(timeout) => {
            tokio::select! {
                result = tokio::time::timeout(timeout, request_future) => if let Ok(result) = result {
                    result
                } else {
                    cancelled.store(true, Ordering::Release);
                    Err("HTTP/2 request timed out".to_string())
                },
                () = &mut cancellation => Err("HTTP/2 request timed out".to_string()),
            }
        }
        None => {
            tokio::select! {
                result = request_future => result,
                () = &mut cancellation => Err("HTTP/2 request was cancelled".to_string()),
            }
        }
    };
    let _ = reply.send(result);
    Ok(())
}

#[cfg(feature = "http2")]
fn start_http2_connection_with_transport<T>(
    transport: T,
) -> Result<Arc<Http2ConnectionActor>, String>
where
    T: Read + Write + Send + Unpin + 'static,
{
    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name("mux-http2-connection".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_sender.send(Err(format!("HTTP/2 runtime setup failed: {error}")));
                    return;
                }
            };
            let startup_error_sender = ready_sender.clone();
            let result = runtime.block_on(async move {
                let (sender, connection) = h2::client::handshake(BlockingAsync(transport))
                    .await
                    .map_err(|error| format!("HTTP/2 handshake failed: {error}"))?;
                let (requests, mut incoming) =
                    tokio::sync::mpsc::channel(HTTP2_REQUEST_QUEUE_CAPACITY);
                let actor = Arc::new(Http2ConnectionActor { requests });
                ready_sender
                    .send(Ok(Arc::clone(&actor)))
                    .map_err(|_| "HTTP/2 actor caller stopped during startup".to_string())?;
                let _connection_task = tokio::spawn(connection);
                while let Some(message) = incoming.recv().await {
                    let request_sender = sender.clone();
                    tokio::spawn(async move {
                        let _ = execute_http2_request(request_sender, message).await;
                    });
                }
                Ok::<(), String>(())
            });
            if let Err(error) = result {
                let _ = startup_error_sender.send(Err(error));
            }
        })
        .map_err(|error| format!("could not start HTTP/2 connection actor: {error}"))?;
    ready_receiver
        .recv()
        .map_err(|_| "HTTP/2 connection actor stopped during startup".to_string())?
}

#[cfg(feature = "http2")]
fn start_http2_connection(
    tls: StreamOwned<ClientConnection, StdTcpStream>,
) -> Result<Arc<Http2ConnectionActor>, String> {
    start_http2_connection_with_transport(tls)
}

#[cfg(feature = "http2")]
fn http2_connection_actor(
    authority: &str,
    tls: StreamOwned<ClientConnection, StdTcpStream>,
) -> Result<Arc<Http2ConnectionActor>, String> {
    if let Some(actor) = HTTP2_CONNECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(authority)
        .and_then(Weak::upgrade)
    {
        return Ok(actor);
    }
    let actor = start_http2_connection(tls)?;
    let mut connections = HTTP2_CONNECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = connections.get(authority).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    connections.insert(authority.to_string(), Arc::downgrade(&actor));
    Ok(actor)
}

/// Try one private HTTP/2 client connection. Returning `None` means ALPN
/// selected HTTP/1.1 and lets the normal client retry over its existing
/// transport. HTTP/2 connections are retained only through weak actor leases;
/// once the last request finishes, the actor and its socket can shut down.
#[cfg(feature = "http2")]
fn execute_http2_response(
    request: &HttpRequestExecution<'_>,
) -> Result<Option<HttpResponseData>, String> {
    let timeout = (request.options.timeout_ms != 0)
        .then(|| Duration::from_millis(u64::try_from(request.options.timeout_ms).unwrap_or(0)));
    let environment_proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    if request.body_reader.is_some()
        || request.options.max_redirects != 0
        || request.options.retries != 0
        || (request.proxy.is_none()
            && environment_proxy
            && should_use_environment_proxy(request.url))
        || request.headers.iter().any(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
            )
        })
    {
        return Ok(None);
    }
    let url = url::Url::parse(request.url).map_err(|error| format!("invalid HTTP URL: {error}"))?;
    if !url.scheme().eq_ignore_ascii_case("https") {
        return Ok(None);
    }
    let host = url
        .host_str()
        .ok_or_else(|| "HTTPS URL has no host".to_string())?;
    let port = url.port().unwrap_or(443);
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let connect_timeout = (request.options.connect_timeout_ms != 0).then(|| {
        Duration::from_millis(u64::try_from(request.options.connect_timeout_ms).unwrap_or(0))
    });
    let stream = authority
        .to_socket_addrs()
        .map_err(|error| format!("HTTP/2 address resolution failed: {error}"))?
        .find_map(|address| {
            let result = connect_timeout.map_or_else(
                || StdTcpStream::connect(address),
                |timeout| StdTcpStream::connect_timeout(&address, timeout),
            );
            result.ok()
        })
        .ok_or_else(|| "HTTP/2 connection failed".to_string())?;
    let socket_timeout = (request.options.timeout_ms != 0)
        .then(|| Duration::from_millis(u64::try_from(request.options.timeout_ms).unwrap_or(0)));
    let read_timeout = socket_timeout.map_or(HTTP2_IO_POLL_INTERVAL, |timeout| {
        timeout.min(HTTP2_IO_POLL_INTERVAL)
    });
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|error| format!("HTTP/2 read timeout failed: {error}"))?;
    stream
        .set_write_timeout(socket_timeout)
        .map_err(|error| format!("HTTP/2 write timeout failed: {error}"))?;

    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = http2_client_config(roots)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|error| format!("invalid HTTPS server name: {error}"))?;
    let connection = ClientConnection::new(Arc::new(config), server_name)
        .map_err(|error| format!("HTTP/2 TLS setup failed: {error}"))?;
    let mut tls = StreamOwned::new(connection, stream);
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|error| format!("HTTP/2 TLS handshake failed: {error}"))?;
    }
    if tls.conn.alpn_protocol() != Some(b"h2".as_slice()) {
        return Ok(None);
    }

    let mut headers = request.headers.to_vec();
    let has_body = request.raw_body.is_some() || request.body_json.is_some();
    if has_body
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        headers.push((
            "content-type".to_string(),
            if request.body_json.is_some() {
                "application/json".to_string()
            } else {
                "application/octet-stream".to_string()
            },
        ));
    }
    validate_http_header_budget(&headers)?;
    let body = request.raw_body.map_or_else(
        || {
            request
                .body_json
                .map(|value| value.stringify(None).into_bytes())
        },
        |value| Some(value.to_vec()),
    );
    if let Some(body) = body.as_deref() {
        validate_http_buffered_body(body)?;
    }
    let request_uri = http2_request_uri(&url, &authority);
    let mut builder = http::Request::builder()
        .method(request.method)
        .uri(request_uri)
        .version(http::Version::HTTP_2);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(())
        .map_err(|error| format!("HTTP/2 request failed: {error}"))?;
    let actor = http2_connection_actor(&authority, tls)?;
    actor.request(request, body, timeout).map(Some)
}

#[cfg(feature = "http2")]
fn http2_client_config(roots: RootCertStore) -> Result<ClientConfig, String> {
    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("HTTP/2 TLS protocol setup failed: {error}"))?;
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

#[cfg(feature = "http2")]
fn http2_request_uri(url: &url::Url, authority: &str) -> String {
    let mut request_uri = format!(
        "https://{authority}{}",
        if url.path().is_empty() {
            "/"
        } else {
            url.path()
        }
    );
    if let Some(query) = url.query() {
        request_uri.push('?');
        request_uri.push_str(query);
    }
    request_uri
}

fn execute_http_response(
    mut request: HttpRequestExecution<'_>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    validate_http_request_options(
        request.options.connect_timeout_ms,
        request.options.timeout_ms,
        request.options.max_redirects,
        request.options.retries,
        request.options.retry_backoff_ms,
    )?;
    if request.body_reader.is_some() && request.options.retries != 0 {
        return Err(
            "HTTP retries are not supported with a single-use request body reader; use bytes for replayable requests".to_string(),
        );
    }
    let has_body =
        request.raw_body.is_some() || request.body_reader.is_some() || request.body_json.is_some();
    let mut effective_headers = request.headers.to_vec();
    if has_body
        && !request
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        let content_type = if request.body_json.is_some() {
            "application/json"
        } else {
            "application/octet-stream"
        };
        effective_headers.push(("content-type".to_string(), content_type.to_string()));
    }
    validate_http_header_budget(&effective_headers)?;
    let request_started = Instant::now();
    let deadline = (request.options.timeout_ms != 0).then(|| {
        request_started
            + Duration::from_millis(u64::try_from(request.options.timeout_ms).unwrap_or(0))
    });
    let connect_timeout = (request.options.connect_timeout_ms != 0).then(|| {
        Duration::from_millis(
            u64::try_from(request.options.connect_timeout_ms).map_or(0, |value| value),
        )
    });
    let attempts = request.options.retries.saturating_add(1);
    for attempt in 0..attempts {
        let remaining = http_timeout_remaining(deadline, Instant::now());
        if remaining.is_some_and(|value| value.is_zero()) {
            return Err("HTTP request timed out".to_string());
        }
        let attempt_timeout = remaining.or_else(|| {
            (request.options.timeout_ms != 0).then(|| {
                Duration::from_millis(u64::try_from(request.options.timeout_ms).unwrap_or(0))
            })
        });
        let attempt_connect_timeout = connect_timeout
            .map(|value| attempt_timeout.map_or(value, |remaining| value.min(remaining)));
        let proxy = match request.proxy.filter(|value| !value.is_empty()) {
            Some(value) => Some(
                ureq::Proxy::new(value)
                    .map_err(|error| format!("invalid HTTP proxy URL: {error}"))?,
            ),
            None if should_use_environment_proxy(request.url) => ureq::Proxy::try_from_env(),
            None => None,
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(u32::try_from(request.options.max_redirects).map_or(0, |value| value))
            .proxy(proxy)
            // Keep HTTP useful for ordinary clear-text endpoints while refusing a
            // redirect from an HTTPS origin back to HTTP. `ureq` applies this
            // setting to redirects as well as the initial request.
            .https_only(is_https_url(request.url))
            .timeout_connect(attempt_connect_timeout)
            .timeout_global(attempt_timeout)
            .build()
            .into();
        let mut builder = ureq::http::Request::builder()
            .method(request.method)
            .uri(request.url);
        for (header_name, header_value) in &effective_headers {
            builder = builder.header(header_name, header_value);
        }
        let request_body = if let Some(payload) = request.raw_body {
            builder
                .body(ureq::SendBody::from_owned_reader(Cursor::new(
                    payload.to_vec(),
                )))
                .map_err(|error| format!("http request failed: {error}"))?
        } else if let Some(reader) = request.body_reader.take() {
            builder
                .body(ureq::SendBody::from_owned_reader(reader))
                .map_err(|error| format!("http request failed: {error}"))?
        } else if let Some(body_json) = request.body_json {
            builder
                .body(ureq::SendBody::from_owned_reader(Cursor::new(
                    body_json.stringify(None).into_bytes(),
                )))
                .map_err(|error| format!("http request failed: {error}"))?
        } else {
            builder
                .body(ureq::SendBody::from_owned_reader(Cursor::new(
                    Vec::<u8>::new(),
                )))
                .map_err(|error| format!("http request failed: {error}"))?
        };
        match agent.run(request_body) {
            Ok(response) => return Ok(response),
            Err(error) if attempt < request.options.retries => {
                let shift = u32::try_from(attempt).map_or(0, |value| value.min(10));
                let factor = 1_i64.checked_shl(shift).map_or(1, |value| value);
                let delay = request
                    .options
                    .retry_backoff_ms
                    .saturating_mul(factor)
                    .min(MAX_HTTP_RETRY_BACKOFF_MS);
                if delay > 0 {
                    let delay =
                        Duration::from_millis(u64::try_from(delay).map_or(0, |value| value));
                    if http_timeout_remaining(deadline, Instant::now())
                        .is_some_and(|remaining| remaining <= delay)
                    {
                        return Err("HTTP request timed out".to_string());
                    }
                    std::thread::sleep(delay);
                }
                let _ = error;
            }
            Err(error) => return Err(format!("http request failed: {error}")),
        }
    }
    Err("http request failed after retries".to_string())
}

fn execute_http_parts_streaming(
    request: HttpRequestExecution<'_>,
) -> Result<HttpResponseData, String> {
    #[cfg(feature = "http3")]
    if let Some(response) = execute_http3_response(&request)? {
        return Ok(response);
    }
    #[cfg(feature = "http2")]
    if let Some(response) = execute_http2_response(&request)? {
        return Ok(response);
    }
    execute_http_response(request).and_then(stream_http_response_data)
}

#[cfg(feature = "http3")]
fn execute_http3_response(
    request: &HttpRequestExecution<'_>,
) -> Result<Option<HttpResponseData>, String> {
    if request.body_reader.is_some()
        || request.options.max_redirects != 0
        || request.options.retries != 0
        || request.proxy.is_some()
        || request.headers.iter().any(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
            )
        })
    {
        return Ok(None);
    }
    let url = url::Url::parse(request.url).map_err(|error| format!("invalid HTTP URL: {error}"))?;
    if !url.scheme().eq_ignore_ascii_case("https") {
        return Ok(None);
    }
    let host = url
        .host_str()
        .ok_or_else(|| "HTTPS URL has no host".to_string())?;
    let port = url.port().unwrap_or(443);
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut headers = request.headers.to_vec();
    let has_body = request.raw_body.is_some() || request.body_json.is_some();
    if has_body
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        headers.push((
            "content-type".to_string(),
            if request.body_json.is_some() {
                "application/json".to_string()
            } else {
                "application/octet-stream".to_string()
            },
        ));
    }
    validate_http_header_budget(&headers)?;
    let body = request.raw_body.map_or_else(
        || {
            request
                .body_json
                .map(|value| value.stringify(None).into_bytes())
        },
        |value| Some(value.to_vec()),
    );
    if let Some(body) = body.as_deref() {
        validate_http_buffered_body(body)?;
    }
    let request_uri = http3_request_uri(&url, &authority);
    let mut builder = http::Request::builder()
        .method(request.method)
        .uri(request_uri)
        .version(http::Version::HTTP_3);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let http_request = builder
        .body(())
        .map_err(|error| format!("HTTP/3 request failed: {error}"))?;
    let connect_timeout = (request.options.connect_timeout_ms != 0).then(|| {
        Duration::from_millis(u64::try_from(request.options.connect_timeout_ms).unwrap_or(0))
    });
    let request_timeout = (request.options.timeout_ms != 0)
        .then(|| Duration::from_millis(u64::try_from(request.options.timeout_ms).unwrap_or(0)));
    let transport_result = match connect_timeout {
        Some(timeout) => Http3ClientTransport::connect_with_timeout(&authority, timeout),
        None => Http3ClientTransport::connect(&authority),
    };
    let transport = match transport_result {
        Ok(transport) => transport,
        Err(error) if http3_connection_error_can_fallback(&error) => return Ok(None),
        Err(error) => return Err(error.detail().to_string()),
    };
    match transport.send(http_request, body, request_timeout) {
        Ok(response) => Ok(Some(HttpResponseData {
            status: i64::from(response.status),
            headers: response.headers,
            body: response.body,
            body_reader: None,
        })),
        Err(error) if http3_pre_dispatch_connection_error_can_fallback(&error) => Ok(None),
        Err(error) => Err(error.detail().to_string()),
    }
}

#[cfg(feature = "http3")]
fn http3_connection_error_can_fallback(error: &Http3Error) -> bool {
    matches!(
        error.kind(),
        Http3ErrorKind::Transport | Http3ErrorKind::Resolve | Http3ErrorKind::Timeout
    )
}

#[cfg(feature = "http3")]
fn http3_pre_dispatch_connection_error_can_fallback(error: &Http3Error) -> bool {
    !error.was_dispatched() && error.kind() == Http3ErrorKind::Transport
}

#[cfg(feature = "http3")]
fn http3_request_uri(url: &url::Url, authority: &str) -> String {
    let mut request_uri = format!(
        "https://{authority}{}",
        if url.path().is_empty() {
            "/"
        } else {
            url.path()
        }
    );
    if let Some(query) = url.query() {
        request_uri.push('?');
        request_uri.push_str(query);
    }
    request_uri
}

fn is_https_url(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

fn should_use_environment_proxy(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return true;
    };
    let Some(host) = url.host_str() else {
        return true;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    host.parse::<IpAddr>()
        .map_or(true, |address| !address.is_loopback())
}

fn http_timeout_remaining(deadline: Option<Instant>, now: Instant) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(now))
}

fn header_name(name: &str) -> Result<String, String> {
    if !is_http_token(name) {
        return Err("header name must be a non-empty HTTP token".to_string());
    }
    Ok(name.to_ascii_lowercase())
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn header_value(value: &str) -> Result<(), String> {
    if value.bytes().any(|byte| {
        byte == b'\r' || byte == b'\n' || (byte < 0x20 && byte != b'\t') || byte == 0x7f
    }) {
        Err("header value cannot contain control characters".to_string())
    } else {
        Ok(())
    }
}

fn headers_handle(value: *const Value) -> Result<i64, String> {
    let handle = resource_handle(value, *HEADERS_TYPE_ID)
        .ok_or_else(|| "invalid Headers handle".to_string())?;
    if lock_headers().contains_key(&handle) {
        Ok(handle)
    } else {
        Err("invalid Headers handle".to_string())
    }
}

fn request_handle(value: *const Value) -> Result<i64, String> {
    let handle = resource_handle(value, *HTTP_REQUEST_TYPE_ID)
        .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
    if lock_requests().contains_key(&handle) {
        Ok(handle)
    } else {
        Err("invalid HttpRequest handle".to_string())
    }
}

fn response_handle(value: *const Value) -> Result<i64, String> {
    let handle = resource_handle(value, *HTTP_RESPONSE_TYPE_ID)
        .ok_or_else(|| "invalid HttpResponse handle".to_string())?;
    if lock_responses().contains_key(&handle) {
        Ok(handle)
    } else {
        Err("invalid HttpResponse handle".to_string())
    }
}

fn http_router_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *HTTP_ROUTER_TYPE_ID)
        .filter(|handle| lock_http_routers().contains_key(handle))
        .ok_or_else(|| "invalid HttpRouter handle".to_string())
}

fn http_next_handle(value: *const Value) -> Result<i64, String> {
    resource_handle(value, *HTTP_NEXT_TYPE_ID)
        .filter(|handle| lock_http_nexts().contains_key(handle))
        .ok_or_else(|| "invalid HttpNext handle".to_string())
}

fn oauth_client_handle(value: *const Value) -> Result<i64, String> {
    let handle = resource_handle(value, *OAUTH_CLIENT_TYPE_ID)
        .ok_or_else(|| "invalid OAuthClient handle".to_string())?;
    if OAUTH_CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&handle)
    {
        Ok(handle)
    } else {
        Err("invalid OAuthClient handle".to_string())
    }
}

fn oauth_session_handle(value: *const Value) -> Result<i64, String> {
    let handle = resource_handle(value, *OAUTH_SESSION_TYPE_ID)
        .ok_or_else(|| "invalid OAuthSession handle".to_string())?;
    if OAUTH_SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&handle)
    {
        Ok(handle)
    } else {
        Err("invalid OAuthSession handle".to_string())
    }
}

fn response_entry_from_data(data: HttpResponseData) -> HttpResponseEntry {
    HttpResponseEntry {
        status: data.status,
        headers: Arc::new(Mutex::new(HeaderData {
            values: data.headers,
        })),
        body: data.body,
        body_reader: data.body_reader,
        streamed_bytes: 0,
        position: 0,
        names: 1,
    }
}

/// Construct a mutable, case-insensitive header collection.
#[unsafe(no_mangle)]
pub extern "C" fn mux_net_http_headers_new() -> *mut Value {
    insert_headers(Arc::new(Mutex::new(HeaderData::default())))
}

/// Add or replace all values for a header name.
///
/// # Safety
/// All pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_headers_set(
    headers: *const Value,
    name: *const Value,
    value: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = headers_handle(headers)?;
        let Some(Value::String(name)) = name.as_ref() else {
            return Err("header name must be a string".to_string());
        };
        let Some(Value::String(value)) = value.as_ref() else {
            return Err("header value must be a string".to_string());
        };
        let name = header_name(name)?;
        header_value(value)?;
        let data = lock_headers()
            .get(&handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let mut data = data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut replacement = data
            .values
            .iter()
            .filter(|(existing, _)| existing != &name)
            .cloned()
            .collect::<Vec<_>>();
        replacement.push((name, value.clone()));
        validate_http_header_budget(&replacement)?;
        data.values = replacement;
        Ok(())
    })();
    http_result_unit(result)
}

/// Append one value for a header name.
///
/// # Safety
/// All pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_headers_append(
    headers: *const Value,
    name: *const Value,
    value: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = headers_handle(headers)?;
        let Some(Value::String(name)) = name.as_ref() else {
            return Err("header name must be a string".to_string());
        };
        let Some(Value::String(value)) = value.as_ref() else {
            return Err("header value must be a string".to_string());
        };
        let name = header_name(name)?;
        header_value(value)?;
        let data = lock_headers()
            .get(&handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let mut data = data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut replacement = data.values.clone();
        replacement.push((name, value.clone()));
        validate_http_header_budget(&replacement)?;
        data.values = replacement;
        Ok(())
    })();
    http_result_unit(result)
}

/// Return the first value for a header name, if present.
///
/// # Safety
/// Both pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_headers_get(
    headers: *const Value,
    name: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = headers_handle(headers)?;
        let Some(Value::String(name)) = name.as_ref() else {
            return Err("header name must be a string".to_string());
        };
        let name = header_name(name)?;
        let data = lock_headers()
            .get(&handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let value = data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .iter()
            .find(|(existing, _)| existing == &name)
            .map(|(_, value)| Value::String(value.clone()));
        Ok(Value::Optional(value.map(Box::new)))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Return every value for a header name, preserving append order.
///
/// # Safety
/// Both pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_headers_values(
    headers: *const Value,
    name: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = headers_handle(headers)?;
        let Some(Value::String(name)) = name.as_ref() else {
            return Err("header name must be a string".to_string());
        };
        let name = header_name(name)?;
        let data = lock_headers()
            .get(&handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let values = data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .iter()
            .filter(|(existing, _)| existing == &name)
            .map(|(_, value)| Value::String(value.clone()))
            .collect();
        Ok(Value::List(values))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Remove every value for a header name.
///
/// # Safety
/// Both pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_headers_remove(
    headers: *const Value,
    name: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = headers_handle(headers)?;
        let Some(Value::String(name)) = name.as_ref() else {
            return Err("header name must be a string".to_string());
        };
        let name = header_name(name)?;
        let data = lock_headers()
            .get(&handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        data.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .retain(|(existing, _)| existing != &name);
        Ok(())
    })();
    http_result_unit(result)
}

/// Create a default request builder.
///
/// All request fields start at their empty/default values; callers assign the
/// method, URL, headers, and body before sending.
///
/// # Safety
/// This function does not dereference any caller-provided pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_new() -> *mut Value {
    let options = default_http_request_options();
    insert_http_request(HttpRequestEntry {
        method: String::new(),
        url: String::new(),
        request_id: String::new(),
        proxy: None,
        headers: Arc::new(Mutex::new(HeaderData::default())),
        // Keep the public `body` field's value empty while distinguishing the
        // default (no entity) from an explicitly assigned empty byte string.
        // This prevents a plain GET from acquiring an unsolicited
        // application/octet-stream content type.
        body: None,
        body_reader: None,
        path_params: Arc::new(Mutex::new(HashMap::new())),
        connect_timeout_ms: options.connect_timeout_ms,
        timeout_ms: options.timeout_ms,
        max_redirects: options.max_redirects,
        retries: options.retries,
        retry_backoff_ms: options.retry_backoff_ms,
        names: 1,
    })
}

/// Set the request method field.
///
/// # Safety
/// Both pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_method_field(
    request: *const Value,
    method: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let Some(Value::String(method)) = method.as_ref() else {
            return Err("HTTP method must be a string".to_string());
        };
        lock_requests()
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .method = method.to_ascii_uppercase();
        Ok(())
    })();
    http_result_unit(result)
}

/// Build a request from explicit method, URL, headers, and body values.
///
/// # Safety
/// All pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_from_config(
    method: *const Value,
    url: *const Value,
    headers: *const Value,
    body: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(method)) = method.as_ref() else {
            return Err("HTTP method must be a string".to_string());
        };
        if !is_http_token(method) {
            return Err("HTTP method must be a non-empty HTTP token".to_string());
        }
        let Some(Value::String(url)) = url.as_ref() else {
            return Err("HTTP URL must be a string".to_string());
        };
        if url.is_empty() {
            return Err("HTTP URL must not be empty".to_string());
        }
        let Some(Value::Object(_)) = headers.as_ref() else {
            return Err("HTTP headers must be a Headers value".to_string());
        };
        let header_handle = headers_handle(headers)?;
        let header_data = lock_headers()
            .get(&header_handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let header_values = header_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .clone();
        validate_http_header_budget(&header_values)?;
        let Some(Value::Bytes(body)) = body.as_ref() else {
            return Err("HTTP request body must be bytes".to_string());
        };
        validate_http_buffered_body(body)?;
        let options = default_http_request_options();
        let value = insert_http_request(HttpRequestEntry {
            method: method.to_ascii_uppercase(),
            url: url.clone(),
            request_id: String::new(),
            proxy: None,
            headers: header_data,
            body: Some(body.clone()),
            body_reader: None,
            path_params: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout_ms: options.connect_timeout_ms,
            timeout_ms: options.timeout_ms,
            max_redirects: options.max_redirects,
            retries: options.retries,
            retry_backoff_ms: options.retry_backoff_ms,
            names: 1,
        });
        take_resource_value(value)
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

#[derive(Clone, Copy)]
enum HttpRequestField {
    Method,
    Url,
    RequestId,
    Proxy,
    Body,
    ConnectTimeout,
    Timeout,
    MaxRedirects,
    Retries,
    RetryBackoff,
}

fn request_field_value(request: *const Value, field: HttpRequestField) -> Result<Value, String> {
    let handle = request_handle(request)?;
    let requests = lock_requests();
    let entry = requests
        .get(&handle)
        .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
    match field {
        HttpRequestField::Method => Ok(Value::String(entry.method.clone())),
        HttpRequestField::Url => Ok(Value::String(entry.url.clone())),
        HttpRequestField::RequestId => Ok(Value::String(entry.request_id.clone())),
        HttpRequestField::Proxy => Ok(Value::String(entry.proxy.clone().unwrap_or_default())),
        HttpRequestField::Body => Ok(Value::Bytes(entry.body.clone().unwrap_or_default())),
        HttpRequestField::ConnectTimeout => Ok(Value::Int(entry.connect_timeout_ms)),
        HttpRequestField::Timeout => Ok(Value::Int(entry.timeout_ms)),
        HttpRequestField::MaxRedirects => Ok(Value::Int(entry.max_redirects)),
        HttpRequestField::Retries => Ok(Value::Int(entry.retries)),
        HttpRequestField::RetryBackoff => Ok(Value::Int(entry.retry_backoff_ms)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_method(request: *const Value) -> *mut Value {
    mux_rc_alloc(request_field_value(request, HttpRequestField::Method).unwrap_or(Value::Unit))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_url(request: *const Value) -> *mut Value {
    mux_rc_alloc(request_field_value(request, HttpRequestField::Url).unwrap_or(Value::Unit))
}

/// Read an immutable route capture attached by `HttpRouter.handle`.
///
/// A missing name and a request that has not matched a route both return
/// `none`. Captures are cleared before every top-level dispatch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_path_param(
    request: *const Value,
    name: *const Value,
) -> *mut Value {
    let value = (|| {
        let handle = request_handle(request)?;
        let Some(Value::String(name)) = name.as_ref() else {
            return Err("HTTP path parameter name must be a string".to_string());
        };
        let path_params = lock_requests()
            .get(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .path_params
            .clone();
        let captured = path_params
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
            .map(Value::String);
        Ok(Value::Optional(captured.map(Box::new)))
    })()
    .unwrap_or(Value::Optional(None));
    mux_rc_alloc(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_id(request: *const Value) -> *mut Value {
    mux_rc_alloc(request_field_value(request, HttpRequestField::RequestId).unwrap_or(Value::Unit))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_proxy(request: *const Value) -> *mut Value {
    mux_rc_alloc(
        request_field_value(request, HttpRequestField::Proxy)
            .unwrap_or(Value::String(String::new())),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_body(request: *const Value) -> *mut Value {
    mux_rc_alloc(
        request_field_value(request, HttpRequestField::Body).unwrap_or(Value::Bytes(Vec::new())),
    )
}

macro_rules! http_request_option_getter {
    ($name:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(request: *const Value) -> *mut Value {
            mux_rc_alloc(request_field_value(request, $field).unwrap_or(Value::Int(0)))
        }
    };
}

http_request_option_getter!(
    mux_net_http_request_connect_timeout,
    HttpRequestField::ConnectTimeout
);
http_request_option_getter!(mux_net_http_request_timeout, HttpRequestField::Timeout);
http_request_option_getter!(
    mux_net_http_request_max_redirects,
    HttpRequestField::MaxRedirects
);
http_request_option_getter!(mux_net_http_request_retries, HttpRequestField::Retries);
http_request_option_getter!(
    mux_net_http_request_retry_backoff,
    HttpRequestField::RetryBackoff
);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_headers_field(request: *const Value) -> *mut Value {
    let headers = request_handle(request)
        .and_then(|handle| {
            lock_requests()
                .get(&handle)
                .map(|entry| insert_headers(entry.headers.clone()))
                .ok_or_else(|| "invalid HttpRequest handle".to_string())
        })
        .and_then(take_resource_value)
        .unwrap_or(Value::Unit);
    mux_rc_alloc(headers)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_url_field(
    request: *const Value,
    url: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let Some(Value::String(url)) = url.as_ref() else {
            return Err("HTTP URL must be a string".to_string());
        };
        lock_requests()
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .url = url.clone();
        Ok(())
    })();
    http_result_unit(result)
}

/// Set an explicit request identifier. The value is sent as `X-Request-ID`
/// when the caller has not already supplied that header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_id_field(
    request: *const Value,
    request_id: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let Some(Value::String(request_id)) = request_id.as_ref() else {
            return Err("HTTP request_id must be a string".to_string());
        };
        if !request_id.is_empty()
            && (request_id.len() > 128
                || !request_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte)))
        {
            return Err(
                "HTTP request_id must be empty or visible ASCII up to 128 bytes".to_string(),
            );
        }
        lock_requests()
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .request_id = request_id.clone();
        Ok(())
    })();
    http_result_unit(result)
}

/// Set an explicit proxy URL for this request. An empty value restores the
/// default behavior of consulting the conventional proxy environment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_proxy_field(
    request: *const Value,
    proxy: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let Some(Value::String(proxy)) = proxy.as_ref() else {
            return Err("HTTP proxy must be a string".to_string());
        };
        lock_requests()
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .proxy = (!proxy.is_empty()).then_some(proxy.clone());
        Ok(())
    })();
    http_result_unit(result)
}

fn set_http_request_option_field(
    request: *const Value,
    field: HttpRequestField,
    raw_value: *const Value,
) -> Result<(), String> {
    let handle = request_handle(request)?;
    let Some(Value::Int(number)) = (unsafe { raw_value.as_ref() }) else {
        return Err("HTTP request option must be an int".to_string());
    };
    let mut requests = lock_requests();
    let entry = requests
        .get_mut(&handle)
        .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
    let mut options = (
        entry.connect_timeout_ms,
        entry.timeout_ms,
        entry.max_redirects,
        entry.retries,
        entry.retry_backoff_ms,
    );
    match field {
        HttpRequestField::ConnectTimeout => options.0 = *number,
        HttpRequestField::Timeout => options.1 = *number,
        HttpRequestField::MaxRedirects => options.2 = *number,
        HttpRequestField::Retries => options.3 = *number,
        HttpRequestField::RetryBackoff => options.4 = *number,
        _ => return Err("field is not an HTTP request option".to_string()),
    }
    validate_http_request_options(options.0, options.1, options.2, options.3, options.4)?;
    (
        entry.connect_timeout_ms,
        entry.timeout_ms,
        entry.max_redirects,
        entry.retries,
        entry.retry_backoff_ms,
    ) = options;
    Ok(())
}

macro_rules! http_request_option_setter {
    ($name:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(request: *const Value, value: *const Value) -> *mut Value {
            http_result_unit(set_http_request_option_field(request, $field, value))
        }
    };
}

http_request_option_setter!(
    mux_net_http_request_set_connect_timeout_field,
    HttpRequestField::ConnectTimeout
);
http_request_option_setter!(
    mux_net_http_request_set_timeout_field,
    HttpRequestField::Timeout
);
http_request_option_setter!(
    mux_net_http_request_set_max_redirects_field,
    HttpRequestField::MaxRedirects
);
http_request_option_setter!(
    mux_net_http_request_set_retries_field,
    HttpRequestField::Retries
);
http_request_option_setter!(
    mux_net_http_request_set_retry_backoff_field,
    HttpRequestField::RetryBackoff
);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_headers_field(
    request: *const Value,
    headers: *const Value,
) -> *mut Value {
    let result = (|| {
        let request_handle = request_handle(request)?;
        let headers_handle = headers_handle(headers)?;
        let data = lock_headers()
            .get(&headers_handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let values = data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .clone();
        validate_http_header_budget(&values)?;
        lock_requests()
            .get_mut(&request_handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .headers = data;
        Ok(())
    })();
    http_result_unit(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_body_field(
    request: *const Value,
    body: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let Some(Value::Bytes(body)) = body.as_ref() else {
            return Err("HTTP request body must be bytes".to_string());
        };
        validate_http_buffered_body(body)?;
        let mut requests = lock_requests();
        let entry = requests
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
        entry.body = Some(body.clone());
        entry.body_reader = None;
        Ok(())
    })();
    http_result_unit(result)
}

/// Return the mutable header collection used by a request.
///
/// # Safety
/// `request` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_headers(request: *const Value) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let headers = lock_requests()
            .get(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?
            .headers
            .clone();
        Ok(insert_headers(headers))
    })();
    match result.and_then(take_resource_value) {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Set or clear a request body. The request owns a copy of the bytes.
///
/// # Safety
/// Both pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_body(
    request: *const Value,
    body: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let Some(Value::Bytes(body)) = body.as_ref() else {
            return Err("HTTP request body must be bytes".to_string());
        };
        validate_http_buffered_body(body)?;
        let mut requests = lock_requests();
        let entry = requests
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
        entry.body = Some(body.clone());
        entry.body_reader = None;
        Ok(())
    })();
    http_result_unit(result)
}

/// Attach a reader as the request body. The reader is retained and consumed
/// incrementally by `send`, so the complete body is not materialized first.
/// A reader body is single-use and cannot be combined with request retries.
/// # Safety
/// `request` and `reader` must be null or point to live `Value`s for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_set_body_reader(
    request: *const Value,
    reader: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = request_handle(request)?;
        let reader = ReaderAdapter::from_value(reader)?;
        let mut requests = lock_requests();
        let entry = requests
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
        entry.body = None;
        entry.body_reader = Some(reader);
        Ok(())
    })();
    http_result_unit(result)
}

/// Send the request and return a single-use response handle.
///
/// # Safety
/// `request` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_send(request: *const Value) -> *mut Value {
    let mut context = (String::new(), String::new());
    let result = (|| {
        let handle = request_handle(request)?;
        let (method, url, proxy, mut headers, body, body_reader, request_id, options) = {
            let mut requests = lock_requests();
            let entry = requests
                .get_mut(&handle)
                .ok_or_else(|| "invalid HttpRequest handle".to_string())?;
            if entry.url.is_empty() {
                return Err("HTTP request URL has not been set".to_string());
            }
            if entry.method.is_empty() {
                return Err("HTTP request method has not been set".to_string());
            }
            let url = entry.url.clone();
            context = (entry.method.clone(), url.clone());
            let headers = entry
                .headers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values
                .clone();
            (
                entry.method.clone(),
                url,
                entry.proxy.clone(),
                headers,
                entry.body.clone(),
                entry.body_reader.take(),
                entry.request_id.clone(),
                HttpRequestOptions {
                    connect_timeout_ms: entry.connect_timeout_ms,
                    timeout_ms: entry.timeout_ms,
                    max_redirects: entry.max_redirects,
                    retries: entry.retries,
                    retry_backoff_ms: entry.retry_backoff_ms,
                },
            )
        };
        if let Some(body) = body.as_deref() {
            validate_http_buffered_body(body)?;
        }
        if !request_id.is_empty()
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
        {
            headers.push(("x-request-id".to_string(), request_id));
        }
        let response = execute_http_parts_streaming(HttpRequestExecution {
            method: &method,
            url: &url,
            proxy: proxy.as_deref(),
            headers: &headers,
            raw_body: body.as_deref(),
            body_reader,
            body_json: None,
            options,
        })?;
        let value = insert_http_response(response_entry_from_data(response));
        take_resource_value(value)
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err_with_context(error, 0, context.0, context.1),
    }
}

/// Create a response suitable for a server or test double.
#[unsafe(no_mangle)]
pub extern "C" fn mux_net_http_response_new() -> *mut Value {
    insert_http_response(HttpResponseEntry {
        status: 200,
        headers: Arc::new(Mutex::new(HeaderData::default())),
        body: Vec::new(),
        body_reader: None,
        streamed_bytes: 0,
        position: 0,
        names: 1,
    })
}

/// Build a response from an explicit status, headers, and bytes body.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_from_config(
    status: i64,
    headers: *const Value,
    body: *const Value,
) -> *mut Value {
    let result = (|| {
        let status =
            u16::try_from(status).map_err(|_| "HTTP status must fit in u16 range".to_string())?;
        if !(100..=999).contains(&status) {
            return Err("HTTP status must be between 100 and 999".to_string());
        }
        let header_handle = headers_handle(headers)?;
        let header_data = lock_headers()
            .get(&header_handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let header_values = header_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .clone();
        validate_http_header_budget(&header_values)?;
        let Some(Value::Bytes(body)) = body.as_ref() else {
            return Err("HTTP response body must be bytes".to_string());
        };
        validate_http_buffered_body(body)?;
        take_resource_value(insert_http_response(HttpResponseEntry {
            status: i64::from(status),
            headers: header_data,
            body: body.clone(),
            body_reader: None,
            streamed_bytes: 0,
            position: 0,
            names: 1,
        }))
    })();
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Return the response status code.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_status_value(response: *const Value) -> *mut Value {
    match response_handle(response).and_then(|handle| {
        lock_responses()
            .get(&handle)
            .map(|entry| Value::Int(entry.status))
            .ok_or_else(|| "invalid HttpResponse handle".to_string())
    }) {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Return the response headers.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_headers_value(response: *const Value) -> *mut Value {
    match response_handle(response)
        .and_then(|handle| {
            lock_responses()
                .get(&handle)
                .map(|entry| insert_headers(entry.headers.clone()))
                .ok_or_else(|| "invalid HttpResponse handle".to_string())
        })
        .and_then(take_resource_value)
    {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Read a mutable server-response field without wrapping it in a Result.
///
/// Response fields mirror request fields: they are ordinary values on the
/// typed handle, while operations that can fail continue to return Result.
/// Invalid handles produce a unit value here; the compiler only emits this
/// accessor after semantic field validation, and the fallible response
/// methods remain available for callers that need an explicit error.
#[derive(Clone, Copy)]
enum HttpResponseField {
    Status,
    Body,
}

fn response_field_value(response: *const Value, field: HttpResponseField) -> Result<Value, String> {
    let handle = response_handle(response)?;
    let responses = lock_responses();
    let entry = responses
        .get(&handle)
        .ok_or_else(|| "invalid HttpResponse handle".to_string())?;
    match field {
        HttpResponseField::Status => Ok(Value::Int(entry.status)),
        HttpResponseField::Body => Ok(Value::Bytes(entry.body[entry.position..].to_vec())),
    }
}

/// Return the response status field as a plain Mux value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_status_field(response: *const Value) -> *mut Value {
    mux_rc_alloc(response_field_value(response, HttpResponseField::Status).unwrap_or(Value::Unit))
}

/// Return the response headers field as a shared Headers handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_headers_field(response: *const Value) -> *mut Value {
    let headers = response_handle(response)
        .and_then(|handle| {
            lock_responses()
                .get(&handle)
                .map(|entry| insert_headers(entry.headers.clone()))
                .ok_or_else(|| "invalid HttpResponse handle".to_string())
        })
        .and_then(take_resource_value)
        .unwrap_or(Value::Unit);
    mux_rc_alloc(headers)
}

/// Return the unread response body as a bytes field. Reading through this
/// field does not consume the response; the explicit read methods do.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_body_field(response: *const Value) -> *mut Value {
    mux_rc_alloc(
        response_field_value(response, HttpResponseField::Body).unwrap_or(Value::Bytes(Vec::new())),
    )
}

/// Set a validated response status field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_set_status_field(
    response: *const Value,
    status: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = response_handle(response)?;
        let Some(Value::Int(status)) = status.as_ref() else {
            return Err("HTTP response status must be an int".to_string());
        };
        let status = u16::try_from(*status)
            .map_err(|_| "HTTP response status must fit in u16 range".to_string())?;
        if !(100..=999).contains(&status) {
            return Err("HTTP response status must be between 100 and 999".to_string());
        }
        lock_responses()
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpResponse handle".to_string())?
            .status = i64::from(status);
        Ok(())
    })();
    http_result_unit(result)
}

/// Replace the response's duplicate-aware header collection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_set_headers_field(
    response: *const Value,
    headers: *const Value,
) -> *mut Value {
    let result = (|| {
        let response_handle = response_handle(response)?;
        let headers_handle = headers_handle(headers)?;
        let data = lock_headers()
            .get(&headers_handle)
            .ok_or_else(|| "invalid Headers handle".to_string())?
            .data
            .clone();
        let values = data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .clone();
        validate_http_header_budget(&values)?;
        lock_responses()
            .get_mut(&response_handle)
            .ok_or_else(|| "invalid HttpResponse handle".to_string())?
            .headers = data;
        Ok(())
    })();
    http_result_unit(result)
}

/// Replace the response body and reset its single-use read cursor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_set_body_field(
    response: *const Value,
    body: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = response_handle(response)?;
        let Some(Value::Bytes(body)) = body.as_ref() else {
            return Err("HTTP response body must be bytes".to_string());
        };
        validate_http_buffered_body(body)?;
        let mut responses = lock_responses();
        let entry = responses
            .get_mut(&handle)
            .ok_or_else(|| "invalid HttpResponse handle".to_string())?;
        entry.body = body.clone();
        entry.body_reader = None;
        entry.streamed_bytes = 0;
        entry.position = 0;
        Ok(())
    })();
    http_result_unit(result)
}

/// Return this response when its status is 2xx, otherwise return a status
/// error. The response handle remains valid in either case.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_error_for_status(
    response: *const Value,
) -> *mut Value {
    let result = response_handle(response).and_then(|handle| {
        let status = lock_responses()
            .get(&handle)
            .map(|entry| entry.status)
            .ok_or_else(|| "invalid HttpResponse handle".to_string())?;
        if (200..=299).contains(&status) {
            Ok(unsafe { (&*response).clone() })
        } else {
            let reason = u16::try_from(status).ok().map_or_else(
                || "Unknown".to_string(),
                |status| reason_phrase(status).to_string(),
            );
            Err(format!("HTTP status {status}: {reason}"))
        }
    });
    match result {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

fn response_read(response: *const Value, limit: i64) -> Result<Vec<u8>, String> {
    let limit = socket_read_size(limit)?;
    let handle = response_handle(response)?;
    let mut responses = lock_responses();
    let entry = responses
        .get_mut(&handle)
        .ok_or_else(|| "invalid HttpResponse handle".to_string())?;
    if let Some(reader) = entry.body_reader.as_mut() {
        if entry.streamed_bytes >= MAX_HTTP_BODY_BYTES {
            let mut extra = [0_u8; 1];
            let count = reader
                .read(&mut extra)
                .map_err(|error| format!("failed to read response body: {error}"))?;
            if count != 0 {
                return Err(format!(
                    "response body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"
                ));
            }
            return Ok(Vec::new());
        }
        let allowed = limit.min(MAX_HTTP_BODY_BYTES - entry.streamed_bytes);
        let mut bytes = vec![0_u8; allowed];
        let count = reader
            .read(&mut bytes)
            .map_err(|error| format!("failed to read response body: {error}"))?;
        bytes.truncate(count);
        entry.streamed_bytes = entry.streamed_bytes.saturating_add(count);
        return Ok(bytes);
    }
    let end = entry.position.saturating_add(limit).min(entry.body.len());
    let bytes = entry.body[entry.position..end].to_vec();
    entry.position = end;
    Ok(bytes)
}

/// Consume the complete remaining response body without ever allocating more
/// than the HTTP body limit. This is also the bridge used by operations that
/// need a complete payload, such as `save` and server serialization.
fn response_read_to_end(response: *const Value) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let chunk = response_read(response, 64 * 1024)?;
        if chunk.is_empty() {
            break;
        }
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
            return Err(format!(
                "response body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Consume up to `limit` remaining response bytes.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_read_bytes(
    response: *const Value,
    limit: i64,
) -> *mut Value {
    match response_read(response, limit) {
        Ok(bytes) => http_result_ok(Value::Bytes(bytes)),
        Err(error) => http_result_err(error),
    }
}

/// Consume up to `limit` response bytes into a single-use `io.Reader`.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_reader(
    response: *const Value,
    limit: i64,
) -> *mut Value {
    let bytes = match response_read(response, limit) {
        Ok(bytes) => bytes,
        Err(error) => return http_result_err(error),
    };
    let bytes_value = Value::Bytes(bytes);
    let reader = crate::stream::mux_io_reader_from_bytes(&bytes_value);
    if reader.is_null() {
        return http_result_err("could not allocate response reader".to_string());
    }
    let owned = unsafe { (*reader).clone() };
    unsafe { mux_rc_dec(reader) };
    http_result_ok(owned)
}

/// Consume and decode up to `limit` remaining response bytes as UTF-8.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_read_text(
    response: *const Value,
    limit: i64,
) -> *mut Value {
    match response_read(response, limit).and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| "response body is not valid UTF-8".to_string())
    }) {
        Ok(text) => http_result_ok(Value::String(text)),
        Err(error) => http_result_err(error),
    }
}

/// Consume and parse up to `limit` remaining response bytes as JSON.
///
/// # Safety
/// `response` must be null or point to a live `Value` for the duration.
#[cfg(feature = "json")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_read_json(
    response: *const Value,
    limit: i64,
) -> *mut Value {
    match response_read(response, limit)
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| "response body is not valid UTF-8".to_string())
        })
        .and_then(|text| {
            Json::parse(&text).map_err(|error| format!("invalid JSON response: {error}"))
        })
        .map(|json| json_to_value(&json))
    {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Consume and save remaining response bytes to a file.
///
/// # Safety
/// Both pointers must be null or point to live `Value`s for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_save(
    response: *const Value,
    path: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(path)) = path.as_ref() else {
            return Err("path must be a string".to_string());
        };
        let bytes = response_read_to_end(response)?;
        std::fs::write(path, bytes).map_err(|error| format!("failed to save response: {error}"))
    })();
    http_result_unit(result)
}

/// Create a typed HTTP error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_http_error_from_message(message: *const Value) -> *mut Value {
    let detail = value_to_string(message as *mut Value)
        .unwrap_or_else(|_| "invalid HTTP error detail".to_string());
    mux_rc_alloc(http_error_value(
        StdErrorKind::Transport,
        detail,
        0,
        String::new(),
        String::new(),
    ))
}

macro_rules! http_error_field_getter {
    ($name:ident, $field:expr) => {
        /// Read one field from a typed HTTP error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the
        /// duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(
                http_error_field(error, $field)
                    .unwrap_or(Value::String("invalid HttpError handle".to_string())),
            )
        }
    };
}

http_error_field_getter!(mux_http_error_kind, HttpErrorField::Kind);
http_error_field_getter!(mux_http_error_detail, HttpErrorField::Detail);
http_error_field_getter!(mux_http_error_method, HttpErrorField::Method);
http_error_field_getter!(mux_http_error_url, HttpErrorField::Url);

/// Read the HTTP status associated with an error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_http_error_status(error: *const Value) -> *mut Value {
    mux_rc_alloc(http_error_field(error, HttpErrorField::Status).unwrap_or(Value::Int(0)))
}

/// Return the detail message from a typed HTTP error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_http_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(http_error_text(error, false)))
}

/// Return a decorated string representation of a typed HTTP error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_http_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(http_error_text(error, true)))
}

#[unsafe(no_mangle)]
/// # Safety
/// `addr` must be null or a valid, live string `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_listener_bind(addr: *mut Value) -> *mut Value {
    let address = match value_to_string(addr) {
        Ok(address) => address,
        Err(err) => return net_result_err(err),
    };
    match StdTcpListener::bind(&address).map_err(|e| format!("tcp listener bind failed: {e}")) {
        Ok(listener) => {
            let handle = store_tcp_listener(listener);
            net_result_socket(handle, *TCP_LISTENER_TYPE_ID)
        }
        Err(err) => net_result_err_address(err, address),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `listener` must be null or a valid, live listener `Value` pointer returned
/// by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_listener_accept(listener: *mut Value) -> *mut Value {
    let handle = match tcp_listener_handle(listener) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_tcp_listener(handle, |socket| {
        socket
            .accept()
            .map(|(stream, _)| stream)
            .map_err(|e| format!("tcp listener accept failed: {e}"))
    }) {
        Ok(stream) => {
            let stream_handle = store_tcp_stream(stream);
            net_result_socket(stream_handle, *TCP_STREAM_TYPE_ID)
        }
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `listener` must be null or a valid, live listener `Value` pointer returned
/// by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_listener_set_nonblocking(
    listener: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match tcp_listener_handle(listener) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_listener(handle, |socket| {
        socket
            .set_nonblocking(enabled != 0)
            .map_err(|e| format!("tcp listener set_nonblocking failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `listener` must be null or a valid, live listener `Value` pointer returned
/// by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_listener_local_addr(listener: *mut Value) -> *mut Value {
    let result = tcp_listener_handle(listener).and_then(|handle| {
        with_tcp_listener(handle, |socket| {
            socket
                .local_addr()
                .map(|addr| addr.to_string())
                .map_err(|e| format!("tcp listener local_addr failed: {e}"))
        })
    });
    net_result_string(result)
}

#[unsafe(no_mangle)]
/// # Safety
/// `listener` must be null or a valid, live listener `Value` pointer returned
/// by `mux_rc_alloc`. The handle is closed and invalidated by this call.
pub unsafe extern "C" fn mux_net_tcp_listener_close(listener: *mut Value) {
    if let Ok(handle) = tcp_listener_handle(listener) {
        remove_tcp_listener(handle);
        write_handle(listener, 0);
    }
}

/// Bind a local IPC listener. Unix builds use a filesystem Unix-domain socket;
/// Windows builds use a byte-mode named pipe (the `\\.\pipe\` prefix is added
/// when callers provide only a logical pipe name).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_listener_bind(path: *mut Value) -> *mut Value {
    let path = match local_path(path) {
        Ok(path) => path,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let listener = match StdLocalListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            return net_result_err_address(format!("local listener bind failed: {error}"), path)
        }
    };
    #[cfg(windows)]
    let listener = {
        let mut options = PipeOptions::new(&path);
        match options.single() {
            Ok(listener) => listener,
            Err(error) => {
                return net_result_err_address(format!("local listener bind failed: {error}"), path)
            }
        }
    };
    let handle = store_local_listener(listener, path);
    net_result_socket(handle, *LOCAL_LISTENER_TYPE_ID)
}

/// Connect to a local IPC listener.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_connect(path: *mut Value) -> *mut Value {
    let path = match local_path(path) {
        Ok(path) => path,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let stream = match StdLocalStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            return net_result_err_address(format!("local stream connect failed: {error}"), path)
        }
    };
    #[cfg(windows)]
    let stream = match PipeClient::connect(&path) {
        Ok(stream) => LocalStreamNative::Client(stream),
        Err(error) => {
            return net_result_err_address(format!("local stream connect failed: {error}"), path)
        }
    };
    let handle = store_local_stream(stream);
    net_result_socket(handle, *LOCAL_STREAM_TYPE_ID)
}

/// Accept one local IPC connection. The listener remains available for later
/// accepts; Windows creates the next named-pipe instance before returning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_listener_accept(listener: *mut Value) -> *mut Value {
    let handle = match local_listener_handle(listener) {
        Ok(handle) => handle,
        Err(error) => return net_result_err(error),
    };
    let entry = {
        let listeners = LOCAL_LISTENERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = listeners.get(&handle) else {
            return net_result_err("invalid local listener handle".to_string());
        };
        entry.listener.clone()
    };
    #[cfg(windows)]
    let path = {
        let listeners = LOCAL_LISTENERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = listeners.get(&handle) else {
            return net_result_err("invalid local listener handle".to_string());
        };
        entry.path.clone()
    };
    #[cfg(unix)]
    let guard = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(windows)]
    let mut guard = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(unix)]
    let stream = match guard.accept() {
        Ok((stream, _)) => stream,
        Err(error) => return net_result_err(format!("local listener accept failed: {error}")),
    };
    #[cfg(windows)]
    let stream = {
        let pending = std::mem::replace(&mut *guard, {
            let mut options = PipeOptions::new(&path);
            match options.first(false).single() {
                Ok(next) => next,
                Err(error) => {
                    return net_result_err(format!("local listener prepare failed: {error}"));
                }
            }
        });
        match pending.wait() {
            Ok(server) => LocalStreamNative::Server(server),
            Err(error) => return net_result_err(format!("local listener accept failed: {error}")),
        }
    };
    let stream_handle = store_local_stream(stream);
    net_result_socket(stream_handle, *LOCAL_STREAM_TYPE_ID)
}

/// Read bytes from a local IPC stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_read(stream: *mut Value, size: i64) -> *mut Value {
    let size = match socket_read_size(size) {
        Ok(size) => size,
        Err(error) => return stream_result_err(error),
    };
    let handle = match local_stream_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return stream_result_err(error),
    };
    let result = with_local_stream(handle, |socket| {
        let mut bytes = vec![0u8; size];
        let count = socket
            .read(&mut bytes)
            .map_err(|error| format!("local stream read failed: {error}"))?;
        bytes.truncate(count);
        Ok(bytes)
    });
    match result {
        Ok(bytes) => stream_result_ok(Value::Bytes(bytes)),
        Err(error) => stream_result_err(error),
    }
}

/// Write bytes to a local IPC stream, returning the number written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_write(stream: *mut Value, data: *mut Value) -> *mut Value {
    let bytes = match value_to_bytes(data) {
        Ok(bytes) => bytes,
        Err(error) => return stream_result_err(error),
    };
    let handle = match local_stream_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return stream_result_err(error),
    };
    let result = with_local_stream(handle, |socket| {
        socket
            .write(&bytes)
            .map(|count| count as i64)
            .map_err(|error| format!("local stream write failed: {error}"))
    });
    match result {
        Ok(count) => stream_result_ok(Value::Int(count)),
        Err(error) => stream_result_err(error),
    }
}

fn local_timeout(timeout_ms: i64) -> Result<Option<Duration>, String> {
    if timeout_ms < 0 {
        return Err("local stream timeout must not be negative".to_string());
    }
    Ok((timeout_ms > 0).then(|| Duration::from_millis(timeout_ms as u64)))
}

/// Set or clear the local stream read timeout. A zero value clears it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_set_read_timeout(
    stream: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let timeout = match local_timeout(timeout_ms) {
        Ok(timeout) => timeout,
        Err(error) => return net_result_err(error),
    };
    let handle = match local_stream_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let result = with_local_stream(handle, |socket| {
        socket
            .set_read_timeout(timeout)
            .map_err(|error| format!("local stream read timeout failed: {error}"))
    });
    #[cfg(windows)]
    let result: Result<(), String> =
        Err("local stream read timeout is unsupported on Windows".to_string());
    net_result_unit(result)
}

/// Set or clear the local stream write timeout. A zero value clears it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_set_write_timeout(
    stream: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let timeout = match local_timeout(timeout_ms) {
        Ok(timeout) => timeout,
        Err(error) => return net_result_err(error),
    };
    let handle = match local_stream_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let result = with_local_stream(handle, |socket| {
        socket
            .set_write_timeout(timeout)
            .map_err(|error| format!("local stream write timeout failed: {error}"))
    });
    #[cfg(windows)]
    let result: Result<(), String> =
        Err("local stream write timeout is unsupported on Windows".to_string());
    net_result_unit(result)
}

/// Toggle nonblocking mode on a local stream. Unix-domain streams support the
/// same mode as TCP; Windows named pipes use their own overlapped model and
/// report this operation as unsupported.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_set_nonblocking(
    stream: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match local_stream_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let result = with_local_stream(handle, |socket| {
        socket
            .set_nonblocking(enabled != 0)
            .map_err(|error| format!("local stream set_nonblocking failed: {error}"))
    });
    #[cfg(windows)]
    let result = {
        let _ = enabled;
        Err("local named pipes do not support nonblocking mode".to_string())
    };
    net_result_unit(result)
}

/// Toggle nonblocking mode on a local listener. Unix-domain listeners support
/// this mode; Windows named-pipe accept waits are managed by the pipe runtime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_listener_set_nonblocking(
    listener: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match local_listener_handle(listener) {
        Ok(handle) => handle,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let result = {
        let entry = {
            let listeners = LOCAL_LISTENERS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            listeners.get(&handle).map(|entry| entry.listener.clone())
        };
        match entry {
            Some(entry) => entry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_nonblocking(enabled != 0)
                .map_err(|error| format!("local listener set_nonblocking failed: {error}")),
            None => Err("invalid local listener handle".to_string()),
        }
    };
    #[cfg(windows)]
    let result = {
        let _ = enabled;
        Err("local named-pipe listeners do not support nonblocking mode".to_string())
    };
    net_result_unit(result)
}

/// Shut down one direction of a local stream. Named pipes are full-duplex but
/// do not expose a portable half-close operation, so Windows reports this
/// explicitly instead of pretending the request succeeded.
fn local_shutdown(stream: *mut Value, shutdown: Shutdown) -> *mut Value {
    let handle = match local_stream_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return net_result_err(error),
    };
    #[cfg(unix)]
    let result = with_local_stream(handle, |socket| {
        socket.shutdown(shutdown).or_else(|error| {
            // macOS can report ENOTCONN when the peer has already observed
            // the opposite half-close. The requested state is already true,
            // so make shutdown idempotent across Unix implementations.
            if error.kind() == std::io::ErrorKind::NotConnected {
                Ok(())
            } else {
                Err(format!("local stream shutdown failed: {error}"))
            }
        })
    });
    #[cfg(windows)]
    let result = {
        let _ = shutdown;
        Err("local named pipes do not support half-close".to_string())
    };
    net_result_unit(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_shutdown_read(stream: *mut Value) -> *mut Value {
    local_shutdown(stream, Shutdown::Read)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_shutdown_write(stream: *mut Value) -> *mut Value {
    local_shutdown(stream, Shutdown::Write)
}

/// Close a local stream and invalidate all aliases to its handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_close(stream: *mut Value) {
    if let Ok(handle) = local_stream_handle(stream) {
        remove_local_stream(handle);
        write_handle(stream, 0);
    }
}

/// Close a local listener and remove its Unix socket path when applicable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_local_listener_close(listener: *mut Value) {
    if let Ok(handle) = local_listener_handle(listener) {
        remove_local_listener(handle);
        write_handle(listener, 0);
    }
}

/// Read one HTTP/1.x request into the typed `HttpRequest` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_request_read(stream: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return http_result_err(error),
    };
    match with_tcp_stream(handle, |socket| {
        read_typed_http_request(socket)
            .and_then(|entry| take_resource_value(insert_http_request(entry)))
    }) {
        Ok(value) => http_result_ok(value),
        Err(error) => http_result_err(error),
    }
}

/// Accept one TCP connection, dispatch one typed request to a synchronous Mux
/// handler, and write the handler's typed response before closing the
/// connection. The listener remains usable for a caller that wants to loop.
///
/// `handler` must point to a compiler-produced closure whose boxed callback
/// takes `HttpRequest` and returns `result<HttpResponse, HttpError>`. Handler
/// errors are rendered as bounded plain-text responses, using their typed
/// status when valid and 500 otherwise; middleware can later translate typed
/// errors before they reach this boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_server_config_new() -> *mut Value {
    let defaults = default_http_server_limits();
    let entry = HttpServerConfigEntry {
        max_header_bytes: defaults.max_header_bytes as i64,
        max_body_bytes: defaults.max_body_bytes as i64,
        max_headers: defaults.max_headers as i64,
        read_timeout_ms: defaults.read_timeout_ms,
        access_log: defaults.access_log,
        cors_origins: defaults.cors_origins,
        cors_allow_credentials: defaults.cors_allow_credentials,
        static_root: defaults.static_root,
        worker_count: defaults.worker_count as i64,
        heartbeat_interval_ms: defaults.heartbeat_interval_ms,
        names: 1,
    };
    insert_http_server_config(entry)
}

#[derive(Clone, Copy)]
enum HttpServerConfigField {
    MaxHeaderBytes,
    MaxBodyBytes,
    MaxHeaders,
    ReadTimeoutMs,
    AccessLog,
    CorsOrigins,
    CorsAllowCredentials,
    StaticRoot,
    WorkerCount,
    HeartbeatIntervalMs,
}

fn http_server_config_field_value(
    config: *const Value,
    field: HttpServerConfigField,
) -> Result<Value, String> {
    let handle = http_server_config_handle(config)?;
    let configs = lock_http_server_configs();
    let entry = configs
        .get(&handle)
        .ok_or_else(|| "invalid HTTP server config handle".to_string())?;
    let value = match field {
        HttpServerConfigField::MaxHeaderBytes => entry.max_header_bytes,
        HttpServerConfigField::MaxBodyBytes => entry.max_body_bytes,
        HttpServerConfigField::MaxHeaders => entry.max_headers,
        HttpServerConfigField::ReadTimeoutMs => entry.read_timeout_ms,
        HttpServerConfigField::AccessLog => return Ok(Value::Bool(entry.access_log)),
        HttpServerConfigField::CorsOrigins => {
            return Ok(Value::List(
                entry
                    .cors_origins
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ))
        }
        HttpServerConfigField::CorsAllowCredentials => {
            return Ok(Value::Bool(entry.cors_allow_credentials));
        }
        HttpServerConfigField::StaticRoot => return Ok(Value::String(entry.static_root.clone())),
        HttpServerConfigField::WorkerCount => entry.worker_count,
        HttpServerConfigField::HeartbeatIntervalMs => entry.heartbeat_interval_ms,
    };
    Ok(Value::Int(value))
}

fn set_http_server_config_field(
    config: *const Value,
    field: HttpServerConfigField,
    value: *const Value,
) -> Result<(), String> {
    let handle = http_server_config_handle(config)?;
    let raw_value = unsafe { value.as_ref() }
        .ok_or_else(|| "HTTP server config field value must be an int or bool".to_string())?;
    let mut configs = lock_http_server_configs();
    let entry = configs
        .get_mut(&handle)
        .ok_or_else(|| "invalid HTTP server config handle".to_string())?;
    let mut candidate = entry.clone();
    match field {
        HttpServerConfigField::MaxHeaderBytes => {
            let Value::Int(value) = raw_value else {
                return Err("HTTP server config byte fields must be ints".to_string());
            };
            candidate.max_header_bytes = *value;
        }
        HttpServerConfigField::MaxBodyBytes => {
            let Value::Int(value) = raw_value else {
                return Err("HTTP server config byte fields must be ints".to_string());
            };
            candidate.max_body_bytes = *value;
        }
        HttpServerConfigField::MaxHeaders => {
            let Value::Int(value) = raw_value else {
                return Err("HTTP server config max_headers must be an int".to_string());
            };
            candidate.max_headers = *value;
        }
        HttpServerConfigField::ReadTimeoutMs => {
            let Value::Int(value) = raw_value else {
                return Err("HTTP server config read_timeout_ms must be an int".to_string());
            };
            candidate.read_timeout_ms = *value;
        }
        HttpServerConfigField::AccessLog => {
            let Value::Bool(value) = raw_value else {
                return Err("HTTP server access_log must be a bool".to_string());
            };
            candidate.access_log = *value;
        }
        HttpServerConfigField::CorsOrigins => {
            let Value::List(values) = raw_value else {
                return Err("HTTP cors_origins must be a list of strings".to_string());
            };
            candidate.cors_origins = values
                .iter()
                .map(|value| match value {
                    Value::String(origin) => Ok(origin.clone()),
                    _ => Err("HTTP cors_origins must be a list of strings".to_string()),
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        HttpServerConfigField::CorsAllowCredentials => {
            let Value::Bool(value) = raw_value else {
                return Err("HTTP cors_allow_credentials must be a bool".to_string());
            };
            candidate.cors_allow_credentials = *value;
        }
        HttpServerConfigField::StaticRoot => {
            let Value::String(value) = raw_value else {
                return Err("HTTP static_root must be a string".to_string());
            };
            candidate.static_root = value.clone();
        }
        HttpServerConfigField::WorkerCount => {
            let Value::Int(value) = raw_value else {
                return Err("HTTP server worker_count must be an int".to_string());
            };
            candidate.worker_count = *value;
        }
        HttpServerConfigField::HeartbeatIntervalMs => {
            let Value::Int(value) = raw_value else {
                return Err("HTTP server heartbeat_interval_ms must be an int".to_string());
            };
            candidate.heartbeat_interval_ms = *value;
        }
    }
    http_server_limits(&candidate)?;
    *entry = candidate;
    Ok(())
}

macro_rules! http_server_config_accessors {
    ($getter:ident, $setter:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $getter(config: *const Value) -> *mut Value {
            mux_rc_alloc(http_server_config_field_value(config, $field).unwrap_or(Value::Unit))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $setter(config: *const Value, value: *const Value) -> *mut Value {
            http_result_unit(set_http_server_config_field(config, $field, value))
        }
    };
}

http_server_config_accessors!(
    mux_net_http_server_config_max_header_bytes,
    mux_net_http_server_config_set_max_header_bytes,
    HttpServerConfigField::MaxHeaderBytes
);
http_server_config_accessors!(
    mux_net_http_server_config_max_body_bytes,
    mux_net_http_server_config_set_max_body_bytes,
    HttpServerConfigField::MaxBodyBytes
);
http_server_config_accessors!(
    mux_net_http_server_config_max_headers,
    mux_net_http_server_config_set_max_headers,
    HttpServerConfigField::MaxHeaders
);
http_server_config_accessors!(
    mux_net_http_server_config_read_timeout_ms,
    mux_net_http_server_config_set_read_timeout_ms,
    HttpServerConfigField::ReadTimeoutMs
);
http_server_config_accessors!(
    mux_net_http_server_config_access_log,
    mux_net_http_server_config_set_access_log,
    HttpServerConfigField::AccessLog
);
http_server_config_accessors!(
    mux_net_http_server_config_cors_origins,
    mux_net_http_server_config_set_cors_origins,
    HttpServerConfigField::CorsOrigins
);
http_server_config_accessors!(
    mux_net_http_server_config_cors_allow_credentials,
    mux_net_http_server_config_set_cors_allow_credentials,
    HttpServerConfigField::CorsAllowCredentials
);
http_server_config_accessors!(
    mux_net_http_server_config_static_root,
    mux_net_http_server_config_set_static_root,
    HttpServerConfigField::StaticRoot
);
http_server_config_accessors!(
    mux_net_http_server_config_worker_count,
    mux_net_http_server_config_set_worker_count,
    HttpServerConfigField::WorkerCount
);
http_server_config_accessors!(
    mux_net_http_server_config_heartbeat_interval_ms,
    mux_net_http_server_config_set_heartbeat_interval_ms,
    HttpServerConfigField::HeartbeatIntervalMs
);

/// Create an empty exact-match HTTP router. Routes and middleware retain their
/// compiler-produced closures until the last router handle is dropped.
#[unsafe(no_mangle)]
pub extern "C" fn mux_net_http_router_new() -> *mut Value {
    insert_http_router(HttpRouterData {
        routes: Vec::new(),
        middleware: Vec::new(),
    })
}

/// Add a method/path route to a router.
///
/// `{name}` captures one decoded path segment. `{...name}` captures all
/// remaining decoded segments, joined with `/`, and may match none. Literal
/// routes outrank captures; otherwise the specificity vector and then
/// registration order determine the winner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_router_route(
    router: *mut Value,
    method: *const Value,
    path: *const Value,
    handler: *mut c_void,
) -> *mut Value {
    let result = (|| {
        let handle = http_router_handle(router)?;
        let Some(Value::String(method)) = method.as_ref() else {
            return Err("HTTP route method must be a string".to_string());
        };
        let Some(Value::String(path)) = path.as_ref() else {
            return Err("HTTP route path must be a string".to_string());
        };
        if method.is_empty() {
            return Err("HTTP route method must not be empty".to_string());
        }
        if !is_http_token(method) {
            return Err("HTTP route method must be an HTTP token".to_string());
        }
        let (segments, specificity) = compile_http_route(path)?;
        if handler.is_null() {
            return Err("HTTP route handler is null".to_string());
        }
        let repr = unsafe { &*(handler as *const HttpServerClosureRepr) };
        if repr.boxed_function_ptr.is_null() {
            return Err(
                "HTTP route handler must return result<HttpResponse, HttpError>".to_string(),
            );
        }
        unsafe { crate::closure::mux_closure_retain(handler) };
        let routers = lock_http_routers();
        let entry = routers
            .get(&handle)
            .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
        let mut data = entry
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if data.routes.iter().any(|route| {
            route.method.eq_ignore_ascii_case(method)
                && route.specificity == specificity
                && route
                    .segments
                    .iter()
                    .zip(segments.iter())
                    .all(|(existing, new)| match (existing, new) {
                        (HttpRouteSegment::Literal(left), HttpRouteSegment::Literal(right)) => {
                            left == right
                        }
                        (HttpRouteSegment::Parameter(_), HttpRouteSegment::Parameter(_))
                        | (HttpRouteSegment::CatchAll(_), HttpRouteSegment::CatchAll(_)) => true,
                        _ => false,
                    })
        }) {
            return Err("HTTP route is duplicate or ambiguous with an existing route".to_string());
        }
        data.routes.push(HttpRouteEntry {
            method: method.to_ascii_uppercase(),
            segments,
            specificity,
            handler: handler as usize,
        });
        Ok(())
    })();
    http_result_unit(result)
}

/// Add a synchronous middleware callback. Middleware receives the request and
/// an `HttpNext`; calling `next.handle(request)` continues to the next
/// middleware or the matching route.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_router_use(
    router: *mut Value,
    middleware: *mut c_void,
) -> *mut Value {
    let result = (|| {
        let handle = http_router_handle(router)?;
        if middleware.is_null() {
            return Err("HTTP middleware is null".to_string());
        }
        let repr = unsafe { &*(middleware as *const HttpServerClosureRepr) };
        if repr.boxed_function_ptr.is_null() {
            return Err("HTTP middleware must return result<HttpResponse, HttpError>".to_string());
        }
        unsafe { crate::closure::mux_closure_retain(middleware) };
        let routers = lock_http_routers();
        let entry = routers
            .get(&handle)
            .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
        entry
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .middleware
            .push(HttpMiddlewareEntry::Callback(middleware as usize));
        Ok(())
    })();
    http_result_unit(result)
}

fn add_http_auth_middleware(
    router: *mut Value,
    middleware: HttpMiddlewareEntry,
) -> Result<(), String> {
    let handle = http_router_handle(router)?;
    let routers = lock_http_routers();
    let entry = routers
        .get(&handle)
        .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
    entry
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .middleware
        .push(middleware);
    Ok(())
}

fn validate_auth_secret(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("HTTP {field} must not be empty"));
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(format!(
            "HTTP {field} must not contain whitespace or control characters"
        ));
    }
    Ok(())
}

/// Require an RFC 7617 Basic authorization header before later middleware or
/// the matching route runs. The configured password is never placed in an
/// error, response body, or access-log field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_router_basic_auth(
    router: *mut Value,
    username: *const Value,
    password: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(username)) = username.as_ref() else {
            return Err("HTTP Basic auth username must be a string".to_string());
        };
        let Some(Value::String(password)) = password.as_ref() else {
            return Err("HTTP Basic auth password must be a string".to_string());
        };
        validate_auth_secret(username, "Basic auth username")?;
        validate_auth_secret(password, "Basic auth password")?;
        if username.contains(':') {
            return Err("HTTP Basic auth username must not contain ':'".to_string());
        }
        add_http_auth_middleware(
            router,
            HttpMiddlewareEntry::Basic {
                username: username.clone(),
                password: password.clone(),
            },
        )
    })();
    http_result_unit(result)
}

/// Require an RFC 6750 Bearer authorization header before later middleware or
/// the matching route runs. Tokens are compared exactly and never included in
/// diagnostics or response bodies.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_router_bearer_auth(
    router: *mut Value,
    token: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(token)) = token.as_ref() else {
            return Err("HTTP Bearer auth token must be a string".to_string());
        };
        validate_auth_secret(token, "Bearer auth token")?;
        add_http_auth_middleware(
            router,
            HttpMiddlewareEntry::Bearer {
                token: token.clone(),
            },
        )
    })();
    http_result_unit(result)
}

/// Register an OAuth 2.0 bearer and OIDC metadata boundary.
///
/// This validates the issuer, audience, and JWKS endpoint and verifies RS256
/// bearer tokens against the endpoint's cached signing keys.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_router_oauth_oidc(
    router: *mut Value,
    issuer: *const Value,
    audience: *const Value,
    jwks_url: *const Value,
) -> *mut Value {
    let result = (|| {
        let Some(Value::String(issuer)) = issuer.as_ref() else {
            return Err("HTTP OAuth/OIDC issuer must be a string".to_string());
        };
        let Some(Value::String(audience)) = audience.as_ref() else {
            return Err("HTTP OAuth/OIDC audience must be a string".to_string());
        };
        let Some(Value::String(jwks_url)) = jwks_url.as_ref() else {
            return Err("HTTP OAuth/OIDC JWKS URL must be a string".to_string());
        };
        validate_oauth_metadata(issuer, audience, jwks_url)?;
        let handle = http_router_handle(router)?;
        let routers = lock_http_routers();
        let entry = routers
            .get(&handle)
            .ok_or_else(|| "invalid HttpRouter handle".to_string())?;
        entry
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .middleware
            .push(HttpMiddlewareEntry::OAuthOidc {
                issuer: issuer.clone(),
                audience: audience.clone(),
                jwks_url: jwks_url.clone(),
            });
        Ok(())
    })();
    match result {
        Ok(()) => http_result_ok(Value::Unit),
        Err(error) => http_result_unit_with_kind(Err(error), StdErrorKind::Invalid),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_router_handle(
    router: *mut Value,
    request: *mut Value,
) -> *mut Value {
    match http_router_handle(router) {
        Ok(handle) => http_router_dispatch(handle, request, 0),
        Err(error) => http_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_next_handle(
    next: *mut Value,
    request: *mut Value,
) -> *mut Value {
    let handle = match http_next_handle(next) {
        Ok(handle) => handle,
        Err(error) => return http_result_err(error),
    };
    let (router, stage) = {
        let nexts = lock_http_nexts();
        let Some(entry) = nexts.get(&handle) else {
            return http_result_err("invalid HttpNext handle".to_string());
        };
        (entry.router, entry.stage)
    };
    http_router_dispatch(router, request, stage)
}

struct HttpServerRequestSnapshot {
    method: String,
    url: String,
    request_id: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpServerRequestSnapshot {
    fn from_entry(entry: HttpRequestEntry) -> Self {
        let headers = entry
            .headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .clone();
        Self {
            method: entry.method,
            url: entry.url,
            request_id: entry.request_id,
            headers,
            body: entry.body.unwrap_or_default(),
        }
    }

    fn into_entry(self) -> HttpRequestEntry {
        let options = default_http_request_options();
        HttpRequestEntry {
            method: self.method,
            url: self.url,
            request_id: self.request_id,
            proxy: None,
            headers: Arc::new(Mutex::new(HeaderData {
                values: self.headers,
            })),
            body: Some(self.body),
            body_reader: None,
            path_params: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout_ms: options.connect_timeout_ms,
            timeout_ms: options.timeout_ms,
            max_redirects: options.max_redirects,
            retries: options.retries,
            retry_backoff_ms: options.retry_backoff_ms,
            names: 1,
        }
    }
}

struct HttpServerResponseSnapshot {
    status: i64,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct HttpServerJob {
    request: HttpServerRequestSnapshot,
    limits: HttpServerLimits,
    response_sender: mpsc::SyncSender<Result<HttpServerResponseSnapshot, String>>,
}

struct HttpClosureReleaseGuard(usize);

impl Drop for HttpClosureReleaseGuard {
    fn drop(&mut self) {
        unsafe { crate::closure::mux_closure_release(self.0 as *mut c_void) };
    }
}

struct HttpServerListenerModeGuard {
    listener: StdTcpListener,
    restored: bool,
}

impl HttpServerListenerModeGuard {
    fn new(listener: StdTcpListener) -> Result<Self, String> {
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("http server listener nonblocking setup failed: {error}"))?;
        Ok(Self {
            listener,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<(), String> {
        self.listener
            .set_nonblocking(false)
            .map_err(|error| format!("http server listener blocking restore failed: {error}"))?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for HttpServerListenerModeGuard {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.listener.set_nonblocking(false);
        }
    }
}

fn build_http_server_response(
    snapshot: HttpServerRequestSnapshot,
    limits: &HttpServerLimits,
    handler: *mut c_void,
) -> Result<HttpServerResponseSnapshot, String> {
    let request_context = (
        snapshot.request_id.clone(),
        snapshot.method.clone(),
        snapshot.url.clone(),
    );
    let request = insert_http_request(snapshot.into_entry());
    if request.is_null() {
        return Err("failed to allocate HTTP request".to_string());
    }

    let result = (|| {
        let is_preflight = request_context.1.eq_ignore_ascii_case("OPTIONS")
            && http_request_header(request, "access-control-request-method").is_some();
        if is_preflight {
            let (status, headers, body) = preflight_response(request, limits)
                .ok_or_else(|| "invalid CORS preflight request".to_string())?;
            let headers = response_headers_with_request_id(headers, &request_context.0);
            return Ok(HttpServerResponseSnapshot {
                status,
                headers,
                body,
            });
        }

        let handler_result = unsafe { invoke_http_server_handler(handler, request) }?;
        let response = (|| match unsafe { handler_result.as_ref() } {
            Some(Value::Result(Ok(response))) => {
                let response = response.as_ref() as *const Value;
                let response_handle_value = response_handle(response)?;
                let (status, headers) = {
                    let responses = lock_responses();
                    let entry = responses
                        .get(&response_handle_value)
                        .ok_or_else(|| "invalid HttpResponse handle".to_string())?;
                    let headers = entry
                        .headers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .values
                        .clone();
                    (entry.status, headers)
                };
                let body = response_read_to_end(response)?;
                let static_response = if status == 404 {
                    static_file_response(request, limits).transpose()?
                } else {
                    None
                };
                let (status, headers, body) = static_response.unwrap_or((status, headers, body));
                let headers = cors_response_headers(headers, request, limits);
                let headers = response_headers_with_request_id(headers, &request_context.0);
                Ok(HttpServerResponseSnapshot {
                    status,
                    headers,
                    body,
                })
            }
            Some(Value::Result(Err(error))) => {
                let error = error.as_ref() as *const Value;
                let status = http_error_field(error, HttpErrorField::Status)
                    .ok()
                    .and_then(|value| match value {
                        Value::Int(status) if (100..=999).contains(&status) => Some(status),
                        _ => None,
                    })
                    .unwrap_or(500);
                let message = http_error_text(error, true);
                let headers = cors_response_headers(
                    vec![(
                        "content-type".to_string(),
                        "text/plain; charset=utf-8".to_string(),
                    )],
                    request,
                    limits,
                );
                let headers = response_headers_with_request_id(headers, &request_context.0);
                Ok(HttpServerResponseSnapshot {
                    status,
                    headers,
                    body: message.into_bytes(),
                })
            }
            Some(_) => Err("HTTP handler returned an invalid result".to_string()),
            None => Err("HTTP handler returned null".to_string()),
        })();
        unsafe { mux_rc_dec(handler_result) };
        response
    })();
    unsafe { mux_rc_dec(request) };
    result
}

fn configure_http_server_socket_timeouts(
    stream: &StdTcpStream,
    read_timeout_ms: i64,
) -> Result<(), String> {
    if read_timeout_ms > 0 {
        let timeout = Duration::from_millis(read_timeout_ms as u64);
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("http server read timeout failed: {error}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| format!("http server write timeout failed: {error}"))?;
    }
    Ok(())
}

unsafe fn serve_http_connection(
    mut stream: StdTcpStream,
    limits: HttpServerLimits,
    handler: *mut c_void,
) -> Result<(), String> {
    configure_http_server_socket_timeouts(&stream, limits.read_timeout_ms)?;
    let request = HttpServerRequestSnapshot::from_entry(read_typed_http_request_with_limits(
        &mut stream,
        &limits,
    )?);
    let request_method = request.method.clone();
    let request_id = request.request_id.clone();
    let request_url = request.url.clone();
    let response = build_http_server_response(request, &limits, handler)?;
    write_typed_http_response_for_request(
        &mut stream,
        response.status,
        &response.headers,
        &response.body,
        Some(&request_method),
    )?;
    if limits.access_log {
        log_http_access(&request_id, &request_method, &request_url, response.status);
    }
    Ok(())
}

fn report_http_server_pool_error(
    error: String,
    cancelled: &AtomicBool,
    first_error: &Mutex<Option<String>>,
) {
    if !cancelled.swap(true, Ordering::AcqRel) {
        let mut first_error = first_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

fn release_http_server_permit(permits: &Arc<(Mutex<usize>, Condvar)>) {
    let (count, available) = &**permits;
    let mut count = count
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *count = count.saturating_sub(1);
    available.notify_all();
}

fn wait_for_http_server_actors(permits: &Arc<(Mutex<usize>, Condvar)>) {
    let (count, available) = &**permits;
    let mut count = count
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while *count != 0 {
        count = available
            .wait(count)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn reap_finished_http_server_actors(
    actors: &mut Vec<thread::JoinHandle<()>>,
    cancelled: &AtomicBool,
    first_error: &Mutex<Option<String>>,
) {
    let mut index = 0;
    while index < actors.len() {
        if actors[index].is_finished() {
            let actor = actors.swap_remove(index);
            if actor.join().is_err() {
                report_http_server_pool_error(
                    "HTTP server connection actor panicked".to_string(),
                    cancelled,
                    first_error,
                );
            }
        } else {
            index += 1;
        }
    }
}

fn send_http_server_job(
    sender: &mpsc::SyncSender<HttpServerJob>,
    mut job: HttpServerJob,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("HTTP server worker pool cancelled".to_string());
        }
        match sender.try_send(job) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(returned)) => {
                job = returned;
                thread::sleep(HTTP_SERVER_POOL_POLL_INTERVAL);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err("HTTP server worker pool is closed".to_string());
            }
        }
    }
}

fn receive_http_server_job(
    receiver: &Mutex<mpsc::Receiver<HttpServerJob>>,
    cancelled: &AtomicBool,
) -> Option<HttpServerJob> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let result = {
            let receiver = receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            receiver.recv_timeout(HTTP_SERVER_POOL_POLL_INTERVAL)
        };
        match result {
            Ok(job) => return Some(job),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn receive_http_server_response(
    receiver: &mpsc::Receiver<Result<HttpServerResponseSnapshot, String>>,
    cancelled: &AtomicBool,
) -> Result<HttpServerResponseSnapshot, String> {
    loop {
        match receiver.recv_timeout(HTTP_SERVER_POOL_POLL_INTERVAL) {
            Ok(response) => return response,
            Err(mpsc::RecvTimeoutError::Timeout) if cancelled.load(Ordering::Acquire) => {
                return Err("HTTP server worker pool cancelled".to_string());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("HTTP server worker stopped before responding".to_string());
            }
        }
    }
}

fn serve_http_connection_actor(
    mut stream: StdTcpStream,
    limits: HttpServerLimits,
    sender: mpsc::SyncSender<HttpServerJob>,
    cancelled: Arc<AtomicBool>,
    first_error: Arc<Mutex<Option<String>>>,
    permits: Arc<(Mutex<usize>, Condvar)>,
) {
    let result = (|| {
        configure_http_server_socket_timeouts(&stream, limits.read_timeout_ms)?;
        let request = HttpServerRequestSnapshot::from_entry(read_typed_http_request_with_limits(
            &mut stream,
            &limits,
        )?);
        let request_method = request.method.clone();
        let request_id = request.request_id.clone();
        let request_url = request.url.clone();
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        send_http_server_job(
            &sender,
            HttpServerJob {
                request,
                limits: limits.clone(),
                response_sender,
            },
            &cancelled,
        )?;
        let response = receive_http_server_response(&response_receiver, &cancelled)?;
        write_typed_http_response_for_request(
            &mut stream,
            response.status,
            &response.headers,
            &response.body,
            Some(&request_method),
        )?;
        if limits.access_log {
            log_http_access(&request_id, &request_method, &request_url, response.status);
        }
        Ok(())
    })();
    if let Err(error) = result {
        if (error == "HTTP server worker pool is closed"
            || error == "HTTP server worker stopped before responding")
            && !cancelled.load(Ordering::Acquire)
        {
            report_http_server_pool_error(error, &cancelled, &first_error);
        }
    }
    release_http_server_permit(&permits);
}

struct HttpServerPoolAdmission {
    actors: Vec<thread::JoinHandle<()>>,
    permits: Arc<(Mutex<usize>, Condvar)>,
    accepted: i64,
    accept_error: Option<String>,
    cancelled_by_caller: bool,
}

struct HttpServerPoolContext<'a> {
    listener: &'a mut HttpServerListenerModeGuard,
    limits: &'a HttpServerLimits,
    sender: &'a mpsc::SyncSender<HttpServerJob>,
    cancelled: &'a Arc<AtomicBool>,
    first_error: &'a Arc<Mutex<Option<String>>>,
    queue_capacity: usize,
}

fn spawn_http_server_workers(
    worker_count: usize,
    handler: *mut c_void,
    receiver: &Arc<Mutex<mpsc::Receiver<HttpServerJob>>>,
    cancelled: &Arc<AtomicBool>,
    first_error: &Arc<Mutex<Option<String>>>,
) -> Result<Vec<thread::JoinHandle<()>>, String> {
    let mut worker_handlers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let worker_handler =
            unsafe { crate::sync_primitives::snapshot_sendable_callback(handler) }?;
        worker_handlers.push(HttpClosureReleaseGuard(worker_handler as usize));
    }

    let mut workers = Vec::with_capacity(worker_count);
    for (index, worker_handler) in worker_handlers.into_iter().enumerate() {
        let worker_receiver = Arc::clone(receiver);
        let worker_cancelled = Arc::clone(cancelled);
        let first_error = Arc::clone(first_error);
        let worker_handler_address = worker_handler.0;
        let worker = thread::Builder::new()
            .name(format!("mux-http-server-{index}"))
            .spawn(move || {
                let _handler = worker_handler;
                let worker_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while let Some(job) =
                        receive_http_server_job(&worker_receiver, &worker_cancelled)
                    {
                        if worker_cancelled.load(Ordering::Acquire) {
                            let _ = job
                                .response_sender
                                .try_send(Err("HTTP server worker pool cancelled".to_string()));
                            break;
                        }
                        let result = build_http_server_response(
                            job.request,
                            &job.limits,
                            worker_handler_address as *mut c_void,
                        );
                        // A request-specific failure belongs to that client.
                        let _ = job.response_sender.try_send(result);
                        if worker_cancelled.load(Ordering::Acquire) {
                            break;
                        }
                    }
                }));
                if worker_result.is_err() {
                    report_http_server_pool_error(
                        "HTTP server worker panicked".to_string(),
                        &worker_cancelled,
                        &first_error,
                    );
                }
            })
            .map_err(|error| format!("HTTP server worker start failed: {error}"));
        match worker {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                for worker in workers {
                    let _ = worker.join();
                }
                return Err(error);
            }
        }
    }
    Ok(workers)
}

fn http_server_pool_is_full(permits: &Arc<(Mutex<usize>, Condvar)>, queue_capacity: usize) -> bool {
    let (count, _) = &**permits;
    let count = count
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *count >= queue_capacity
}

fn pool_cancellation_requested(cancellation: Option<usize>) -> Result<bool, String> {
    cancellation.map_or(Ok(false), |cancellation| {
        cancellation_requested(cancellation as *const Value)
    })
}

fn accept_http_server_connection(
    listener: &mut HttpServerListenerModeGuard,
    limits: &HttpServerLimits,
    sender: &mpsc::SyncSender<HttpServerJob>,
    cancelled: &Arc<AtomicBool>,
    first_error: &Arc<Mutex<Option<String>>>,
    permits: &Arc<(Mutex<usize>, Condvar)>,
    connection_index: i64,
) -> Result<Option<thread::JoinHandle<()>>, String> {
    let (stream, _) = match listener.listener.accept() {
        Ok(connection) => connection,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(error) => return Err(format!("http server accept failed: {error}")),
    };
    {
        let (count, _) = &**permits;
        let mut count = count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count += 1;
    }
    let permits_for_actor = Arc::clone(permits);
    let cancelled_for_actor = Arc::clone(cancelled);
    let first_error_for_actor = Arc::clone(first_error);
    let sender_for_actor = sender.clone();
    let actor_limits = limits.clone();
    let actor = thread::Builder::new()
        .name(format!("mux-http-connection-{connection_index}"))
        .spawn(move || {
            let actor_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                serve_http_connection_actor(
                    stream,
                    actor_limits,
                    sender_for_actor,
                    Arc::clone(&cancelled_for_actor),
                    Arc::clone(&first_error_for_actor),
                    Arc::clone(&permits_for_actor),
                );
            }));
            if actor_result.is_err() {
                report_http_server_pool_error(
                    "HTTP server connection actor panicked".to_string(),
                    &cancelled_for_actor,
                    &first_error_for_actor,
                );
                release_http_server_permit(&permits_for_actor);
            }
        })
        .map_err(|error| format!("HTTP server connection start failed: {error}"));
    match actor {
        Ok(actor) => Ok(Some(actor)),
        Err(error) => {
            release_http_server_permit(permits);
            Err(error)
        }
    }
}

fn admit_http_server_pool_connections(
    context: &mut HttpServerPoolContext<'_>,
    max_requests: Option<i64>,
    cancellation: Option<usize>,
) -> HttpServerPoolAdmission {
    let permits = Arc::new((Mutex::new(0_usize), Condvar::new()));
    let mut actors = Vec::new();
    let mut accepted = 0_i64;
    let mut accept_error = None;
    let mut cancelled_by_caller = false;
    while max_requests.is_none_or(|limit| accepted < limit) {
        reap_finished_http_server_actors(&mut actors, context.cancelled, context.first_error);
        if context.cancelled.load(Ordering::Acquire) {
            accept_error = Some("HTTP server worker pool cancelled".to_string());
            break;
        }
        match pool_cancellation_requested(cancellation) {
            Ok(true) => {
                cancelled_by_caller = true;
                break;
            }
            Ok(false) => {}
            Err(error) => {
                accept_error = Some(error);
                context.cancelled.store(true, Ordering::Release);
                break;
            }
        }
        if http_server_pool_is_full(&permits, context.queue_capacity) {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        match accept_http_server_connection(
            context.listener,
            context.limits,
            context.sender,
            context.cancelled,
            context.first_error,
            &permits,
            accepted,
        ) {
            Ok(Some(actor)) => {
                actors.push(actor);
                reap_finished_http_server_actors(
                    &mut actors,
                    context.cancelled,
                    context.first_error,
                );
                accepted += 1;
            }
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                accept_error = Some(error);
                context.cancelled.store(true, Ordering::Release);
                break;
            }
        }
    }
    HttpServerPoolAdmission {
        actors,
        permits,
        accepted,
        accept_error,
        cancelled_by_caller,
    }
}

fn finish_http_server_pool(
    listener: &mut HttpServerListenerModeGuard,
    admission: HttpServerPoolAdmission,
    workers: Vec<thread::JoinHandle<()>>,
    cancelled: &Arc<AtomicBool>,
    first_error: &Arc<Mutex<Option<String>>>,
    max_requests: Option<i64>,
) -> Result<(), String> {
    wait_for_http_server_actors(&admission.permits);
    for actor in admission.actors {
        if actor.join().is_err() {
            report_http_server_pool_error(
                "HTTP server connection actor panicked".to_string(),
                cancelled,
                first_error,
            );
        }
    }
    for worker in workers {
        if worker.join().is_err() {
            cancelled.store(true, Ordering::Release);
            let mut first_error = first_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if first_error.is_none() {
                *first_error = Some("HTTP server worker panicked".to_string());
            }
        }
    }

    let restore_error = listener.restore().err();
    if let Some(error) = first_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return Err(error);
    }
    if let Some(error) = admission.accept_error {
        return Err(error);
    }
    if let Some(error) = restore_error {
        return Err(error);
    }
    if !admission.cancelled_by_caller
        && max_requests.is_some_and(|limit| admission.accepted != limit)
    {
        return Err("HTTP server stopped before serving max_requests".to_string());
    }
    Ok(())
}

fn serve_http_server_pool(
    listener: *const Value,
    limits: HttpServerLimits,
    handler: *mut c_void,
    max_requests: Option<i64>,
    cancellation: Option<usize>,
) -> Result<(), String> {
    let worker_count = limits.worker_count;
    let queue_capacity = worker_count
        .checked_mul(2)
        .ok_or_else(|| "HTTP server worker queue size overflowed".to_string())?;
    let mut listener = HttpServerListenerModeGuard::new(clone_tcp_listener(listener)?)?;
    let (sender, receiver) = mpsc::sync_channel::<HttpServerJob>(queue_capacity);
    let receiver = Arc::new(Mutex::new(receiver));
    let cancelled = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(None::<String>));
    let workers =
        match spawn_http_server_workers(worker_count, handler, &receiver, &cancelled, &first_error)
        {
            Ok(workers) => workers,
            Err(error) => {
                drop(sender);
                drop(receiver);
                return Err(error);
            }
        };
    drop(receiver);
    let mut context = HttpServerPoolContext {
        listener: &mut listener,
        limits: &limits,
        sender: &sender,
        cancelled: &cancelled,
        first_error: &first_error,
        queue_capacity,
    };
    let admission = admit_http_server_pool_connections(&mut context, max_requests, cancellation);
    drop(sender);
    finish_http_server_pool(
        &mut listener,
        admission,
        workers,
        &cancelled,
        &first_error,
        max_requests,
    )
}

fn serve_http_server_until_cancelled_single(
    listener: *const Value,
    limits: HttpServerLimits,
    handler: *mut c_void,
    cancellation: *const Value,
) -> Result<(), String> {
    let mut listener = HttpServerListenerModeGuard::new(clone_tcp_listener(listener)?)?;
    let mut accept_error = None;
    loop {
        match cancellation_requested(cancellation) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                accept_error = Some(error);
                break;
            }
        }
        match listener.listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) =
                    unsafe { serve_http_connection(stream, limits.clone(), handler) }
                {
                    accept_error = Some(error);
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                accept_error = Some(format!("http server accept failed: {error}"));
                break;
            }
        }
    }
    let restore_error = listener.restore().err();
    if let Some(error) = accept_error {
        return Err(error);
    }
    if let Some(error) = restore_error {
        return Err(error);
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_server_serve_once(
    listener: *mut Value,
    config: *mut Value,
    handler: *mut c_void,
) -> *mut Value {
    if let Err(error) = unsafe { validate_http_server_handler(handler) } {
        return http_result_err(error);
    }
    let result = (|| {
        let listener_handle = tcp_listener_handle(listener)?;
        let config_handle = http_server_config_handle(config)?;
        let config_entry = {
            let configs = lock_http_server_configs();
            configs
                .get(&config_handle)
                .ok_or_else(|| "invalid HTTP server config handle".to_string())?
                .clone()
        };
        let limits = http_server_limits(&config_entry)?;
        let stream = with_tcp_listener(listener_handle, |socket| {
            socket
                .accept()
                .map(|(stream, _)| stream)
                .map_err(|error| format!("http server accept failed: {error}"))
        })?;
        unsafe { serve_http_connection(stream, limits, handler) }
    })();
    http_result_unit(result)
}

/// Serve a bounded number of requests. A worker count of one keeps the
/// deterministic synchronous loop. Larger counts use a bounded queue and
/// join every worker before returning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_server_serve(
    listener: *mut Value,
    config: *mut Value,
    handler: *mut c_void,
    max_requests: i64,
) -> *mut Value {
    if !(1..=1_000_000).contains(&max_requests) {
        return http_result_err(
            "HTTP server max_requests must be between 1 and 1000000".to_string(),
        );
    }
    if let Err(error) = unsafe { validate_http_server_handler(handler) } {
        return http_result_err(error);
    }
    let config_handle = match http_server_config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return http_result_err(error),
    };
    let config_entry = {
        let configs = lock_http_server_configs();
        match configs.get(&config_handle) {
            Some(entry) => entry.clone(),
            None => return http_result_err("invalid HTTP server config handle".to_string()),
        }
    };
    let limits = match http_server_limits(&config_entry) {
        Ok(limits) => limits,
        Err(error) => return http_result_err(error),
    };
    if limits.worker_count > 1 {
        return http_result_unit(serve_http_server_pool(
            listener,
            limits,
            handler,
            Some(max_requests),
            None,
        ));
    }
    for _ in 0..max_requests {
        let outcome = unsafe { mux_net_http_server_serve_once(listener, config, handler) };
        let failed = unsafe {
            outcome
                .as_ref()
                .is_some_and(|value| matches!(value, Value::Result(Err(_))))
        };
        if failed {
            return outcome;
        }
        unsafe { mux_rc_dec(outcome) };
    }
    http_result_ok(Value::Unit)
}

/// Serve until the caller cancels the supplied token. Accepted work drains
/// before this function returns, so cancellation stops admission without
/// abandoning requests already queued for a worker.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_server_serve_until_cancelled(
    listener: *mut Value,
    config: *mut Value,
    handler: *mut c_void,
    cancellation: *const Value,
) -> *mut Value {
    if let Err(error) = unsafe { validate_http_server_handler(handler) } {
        return http_result_err(error);
    }
    let result = (|| {
        cancellation_requested(cancellation)?;
        let config_handle = http_server_config_handle(config)?;
        let config_entry = {
            let configs = lock_http_server_configs();
            configs
                .get(&config_handle)
                .ok_or_else(|| "invalid HTTP server config handle".to_string())?
                .clone()
        };
        let limits = http_server_limits(&config_entry)?;
        if limits.worker_count == 1 {
            serve_http_server_until_cancelled_single(listener, limits, handler, cancellation)
        } else {
            serve_http_server_pool(listener, limits, handler, None, Some(cancellation as usize))
        }
    })();
    http_result_unit(result)
}

enum StreamingHttpProtocol {
    Sse,
    WebSocket,
}

fn serve_streaming_http_connection(
    mut stream: StdTcpStream,
    limits: &HttpServerLimits,
    handler: *mut c_void,
    protocol: &StreamingHttpProtocol,
) -> Result<(), String> {
    let request_timeout = if limits.read_timeout_ms > 0 {
        Duration::from_millis(limits.read_timeout_ms as u64)
    } else {
        STREAMING_COMMAND_TIMEOUT
    };
    stream
        .set_read_timeout(Some(request_timeout))
        .map_err(|error| format!("HTTP streaming read timeout failed: {error}"))?;
    let request = insert_http_request(read_typed_http_request_with_limits(&mut stream, limits)?);
    if request.is_null() {
        return Err("failed to allocate HTTP request".to_string());
    }
    let result = (|| {
        let origin_denied = http_request_header(request, "origin")
            .is_some_and(|origin| !cors_origin_allowed(limits, &origin));
        if origin_denied {
            return write_streaming_rejection(&mut stream, request, 403, b"CORS origin denied");
        }
        match protocol {
            StreamingHttpProtocol::Sse => {
                if !http_request_method(request)?.eq_ignore_ascii_case("GET") {
                    return write_streaming_rejection(
                        &mut stream,
                        request,
                        405,
                        b"SSE requires GET",
                    );
                }
                let actor = StreamingSocketActor::new(
                    stream,
                    limits.read_timeout_ms,
                    heartbeat_for(limits, StreamingHeartbeat::Sse),
                )?;
                let headers = streaming_http_headers(200, &sse_headers(request, limits))?;
                actor.write(headers, true)?;
                let session = insert_sse_stream(actor);
                if session.is_null() {
                    return Err("failed to allocate SseStream handle".to_string());
                }
                let callback = unsafe { invoke_streaming_handler(handler, request, session) };
                unsafe { mux_net_sse_stream_close(session) };
                unsafe { mux_rc_dec(session) };
                callback
            }
            StreamingHttpProtocol::WebSocket => {
                let key = match websocket_upgrade_is_valid(request) {
                    Ok(key) => key,
                    Err(error) => {
                        write_streaming_rejection(&mut stream, request, 400, error.as_bytes())?;
                        return Ok(());
                    }
                };
                if key.is_empty() {
                    return Err("WebSocket handshake key must not be empty".to_string());
                }
                let actor = StreamingSocketActor::new(
                    stream,
                    limits.read_timeout_ms,
                    heartbeat_for(limits, StreamingHeartbeat::WebSocket),
                )?;
                let headers = streaming_http_headers(
                    101,
                    &websocket_headers(request, limits, websocket_accept_key(&key)?),
                )?;
                actor.write(headers, true)?;
                let session = insert_websocket_session(actor);
                if session.is_null() {
                    return Err("failed to allocate WebSocketSession handle".to_string());
                }
                let callback = unsafe { invoke_streaming_handler(handler, request, session) };
                unsafe { mux_net_websocket_session_close(session) };
                unsafe { mux_rc_dec(session) };
                callback
            }
        }
    })();
    unsafe { mux_rc_dec(request) };
    result
}

fn heartbeat_for(
    limits: &HttpServerLimits,
    kind: StreamingHeartbeat,
) -> Option<(StreamingHeartbeat, Duration)> {
    if limits.heartbeat_interval_ms == 0 {
        return None;
    }
    u64::try_from(limits.heartbeat_interval_ms)
        .ok()
        .map(|milliseconds| (kind, Duration::from_millis(milliseconds)))
}

fn serve_streaming_http_server(
    listener: *const Value,
    limits: HttpServerLimits,
    handler: *mut c_void,
    protocol: StreamingHttpProtocol,
    max_requests: i64,
) -> Result<(), String> {
    let listener = clone_tcp_listener(listener)?;
    for _ in 0..max_requests {
        let (stream, _) = listener
            .accept()
            .map_err(|error| format!("HTTP streaming accept failed: {error}"))?;
        serve_streaming_http_connection(stream, &limits, handler, &protocol)?;
    }
    Ok(())
}

fn serve_streaming_http_entry(
    listener: *mut Value,
    config: *mut Value,
    handler: *mut c_void,
    protocol: StreamingHttpProtocol,
    max_requests: i64,
) -> *mut Value {
    if !(1..=1_000_000).contains(&max_requests) {
        return http_result_err(
            "HTTP streaming max_requests must be between 1 and 1000000".to_string(),
        );
    }
    if let Err(error) = unsafe { validate_http_stream_handler(handler) } {
        return http_result_err(error);
    }
    let config_handle = match http_server_config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return http_result_err(error),
    };
    let config_entry = {
        let configs = lock_http_server_configs();
        match configs.get(&config_handle) {
            Some(entry) => entry.clone(),
            None => return http_result_err("invalid HTTP server config handle".to_string()),
        }
    };
    let limits = match http_server_limits(&config_entry) {
        Ok(limits) => limits,
        Err(error) => return http_result_err(error),
    };
    http_result_unit(serve_streaming_http_server(
        listener,
        limits,
        handler,
        protocol,
        max_requests,
    ))
}

/// Serve bounded synchronous SSE connections. The callback receives the
/// parsed `HttpRequest` and an already-started `SseStream`, and must return
/// `result<Unit, HttpError>`. The callback owns the connection until it
/// returns; there is no hidden task, async runtime, or public Future.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_server_serve_sse(
    listener: *mut Value,
    config: *mut Value,
    handler: *mut c_void,
    max_requests: i64,
) -> *mut Value {
    serve_streaming_http_entry(
        listener,
        config,
        handler,
        StreamingHttpProtocol::Sse,
        max_requests,
    )
}

/// Serve bounded synchronous WebSocket connections. The callback receives the
/// parsed `HttpRequest` and an accepted `WebSocketSession`, and must return
/// `result<Unit, HttpError>`. Runtime code handles masking, fragmentation,
/// ping/pong, and the close handshake before the callback returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_server_serve_websocket(
    listener: *mut Value,
    config: *mut Value,
    handler: *mut c_void,
    max_requests: i64,
) -> *mut Value {
    serve_streaming_http_entry(
        listener,
        config,
        handler,
        StreamingHttpProtocol::WebSocket,
        max_requests,
    )
}

/// Write a typed `HttpResponse` to a connected TCP stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_http_response_write(
    stream: *mut Value,
    response: *const Value,
) -> *mut Value {
    let stream_handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(error) => return http_result_err(error),
    };
    let response_handle = match response_handle(response) {
        Ok(handle) => handle,
        Err(error) => return http_result_err(error),
    };
    let body = match response_read_to_end(response) {
        Ok(body) => body,
        Err(error) => return http_result_err(error),
    };
    let (status, headers) = {
        let mut responses = lock_responses();
        let Some(entry) = responses.get_mut(&response_handle) else {
            return http_result_err("invalid HttpResponse handle".to_string());
        };
        let headers = entry
            .headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .clone();
        (entry.status, headers)
    };
    let result = with_tcp_stream(stream_handle, |socket| {
        write_typed_http_response(socket, status, &headers, &body)
    });
    http_result_unit(result)
}

#[unsafe(no_mangle)]
/// # Safety
/// `addr` must be null or a valid, live string `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_connect(addr: *mut Value) -> *mut Value {
    let address = match value_to_string(addr) {
        Ok(address) => address,
        Err(err) => return net_result_err(err),
    };
    match StdTcpStream::connect(&address).map_err(|e| format!("failed to connect: {e}")) {
        Ok(stream) => {
            let handle = store_tcp_stream(stream);
            net_result_socket(handle, *TCP_STREAM_TYPE_ID)
        }
        Err(err) => net_result_err_address(err, address),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_read(stream: *mut Value, size: i64) -> *mut Value {
    let size = match socket_read_size(size) {
        Ok(size) => size,
        Err(err) => return stream_result_err(err),
    };
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return stream_result_err(err),
    };
    let result = with_tcp_stream(handle, |socket| {
        let mut buf = vec![0u8; size];
        let count = socket
            .read(&mut buf)
            .map_err(|e| format!("tcp read failed: {e}"))?;
        buf.truncate(count);
        Ok(buf)
    });
    match result {
        Ok(bytes) => stream_result_ok(Value::Bytes(bytes)),
        Err(err) => stream_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` and `data` must be null or valid, live `Value` pointers returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_write(stream: *mut Value, data: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return stream_result_err(err),
    };
    let payload = match value_to_bytes(data) {
        Ok(bytes) => bytes,
        Err(err) => return stream_result_err(err),
    };
    let result = with_tcp_stream(handle, |socket| {
        socket
            .write(&payload)
            .map_err(|e| format!("tcp write failed: {e}"))
    });
    match result {
        Ok(written) => match byte_count_value(written, "tcp write") {
            Ok(value) => stream_result_ok(value),
            Err(err) => stream_result_err(err),
        },
        Err(err) => stream_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc`. The handle is closed and invalidated by this call.
pub unsafe extern "C" fn mux_net_tcp_close(stream: *mut Value) {
    if let Ok(handle) = tcp_handle(stream) {
        remove_tcp_stream(handle);
        write_handle(stream, 0);
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_set_nonblocking(
    stream: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .set_nonblocking(enabled != 0)
            .map_err(|e| format!("tcp set_nonblocking failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_set_read_timeout(
    stream: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let timeout = match timeout_from_millis(timeout_ms, "tcp read timeout") {
        Ok(timeout) => timeout,
        Err(err) => return net_result_err(err),
    };
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .set_read_timeout(timeout)
            .map_err(|e| format!("tcp set_read_timeout failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_set_write_timeout(
    stream: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let timeout = match timeout_from_millis(timeout_ms, "tcp write timeout") {
        Ok(timeout) => timeout,
        Err(err) => return net_result_err(err),
    };
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .set_write_timeout(timeout)
            .map_err(|e| format!("tcp set_write_timeout failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_set_nodelay(stream: *mut Value, enabled: i32) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .set_nodelay(enabled != 0)
            .map_err(|e| format!("tcp set_nodelay failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_nodelay(stream: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_tcp_stream(handle, |socket| {
        socket
            .nodelay()
            .map_err(|e| format!("tcp nodelay failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Bool(value)),
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_tcp_set_keepalive(stream: *mut Value, enabled: i32) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        SockRef::from(&*socket)
            .set_keepalive(enabled != 0)
            .map_err(|e| format!("tcp set_keepalive failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_tcp_keepalive(stream: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_tcp_stream(handle, |socket| {
        SockRef::from(&*socket)
            .keepalive()
            .map_err(|e| format!("tcp keepalive failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Bool(value)),
        Err(err) => net_result_err(err),
    }
}

fn tcp_buffer_size(stream: *mut Value, size: Option<i64>, receive: bool) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    if let Some(size) = size {
        let label = if receive {
            "tcp receive buffer size"
        } else {
            "tcp send buffer size"
        };
        let size = match socket_buffer_size(size, label) {
            Ok(size) => size,
            Err(err) => return net_result_err(err),
        };
        return net_result_unit(with_tcp_stream(handle, |socket| {
            let sock = SockRef::from(&*socket);
            let result = if receive {
                sock.set_recv_buffer_size(size)
            } else {
                sock.set_send_buffer_size(size)
            };
            result.map_err(|e| format!("tcp buffer size update failed: {e}"))
        }));
    }
    match with_tcp_stream(handle, |socket| {
        let sock = SockRef::from(&*socket);
        let size = if receive {
            sock.recv_buffer_size()
        } else {
            sock.send_buffer_size()
        };
        size.map_err(|e| format!("tcp buffer size query failed: {e}"))
    }) {
        Ok(size) => match i64::try_from(size) {
            Ok(size) => net_result_ok(Value::Int(size)),
            Err(_) => net_result_err("tcp buffer size exceeds the Mux integer range".to_string()),
        },
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_tcp_set_recv_buffer_size(
    stream: *mut Value,
    size: i64,
) -> *mut Value {
    tcp_buffer_size(stream, Some(size), true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_tcp_recv_buffer_size(stream: *mut Value) -> *mut Value {
    tcp_buffer_size(stream, None, true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_tcp_set_send_buffer_size(
    stream: *mut Value,
    size: i64,
) -> *mut Value {
    tcp_buffer_size(stream, Some(size), false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_tcp_send_buffer_size(stream: *mut Value) -> *mut Value {
    tcp_buffer_size(stream, None, false)
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_set_ttl(stream: *mut Value, ttl: i64) -> *mut Value {
    let ttl = match socket_ttl_value(ttl, "tcp ttl") {
        Ok(ttl) => ttl,
        Err(err) => return net_result_err(err),
    };
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .set_ttl(ttl)
            .map_err(|e| format!("tcp set_ttl failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_ttl(stream: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_tcp_stream(handle, |socket| {
        socket.ttl().map_err(|e| format!("tcp ttl failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Int(i64::from(value))),
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_shutdown_read(stream: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .shutdown(Shutdown::Read)
            .map_err(|e| format!("tcp read shutdown failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_shutdown_write(stream: *mut Value) -> *mut Value {
    let handle = match tcp_handle(stream) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_tcp_stream(handle, |socket| {
        socket
            .shutdown(Shutdown::Write)
            .map_err(|e| format!("tcp write shutdown failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_peer_addr(stream: *mut Value) -> *mut Value {
    let result = tcp_handle(stream).and_then(|handle| {
        with_tcp_stream(handle, |socket| {
            socket
                .peer_addr()
                .map(|addr| addr.to_string())
                .map_err(|e| format!("tcp peer_addr failed: {e}"))
        })
    });
    net_result_string(result)
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be null or a valid, live stream `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_tcp_local_addr(stream: *mut Value) -> *mut Value {
    let result = tcp_handle(stream).and_then(|handle| {
        with_tcp_stream(handle, |socket| {
            socket
                .local_addr()
                .map(|addr| addr.to_string())
                .map_err(|e| format!("tcp local_addr failed: {e}"))
        })
    });
    net_result_string(result)
}

#[unsafe(no_mangle)]
/// # Safety
/// `addr` must be null or a valid, live string `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_bind(addr: *mut Value) -> *mut Value {
    let address = match value_to_string(addr) {
        Ok(address) => address,
        Err(err) => return net_result_err(err),
    };
    match StdUdpSocket::bind(&address).map_err(|e| format!("udp bind failed: {e}")) {
        Ok(socket) => {
            let handle = store_udp_socket(socket);
            net_result_socket(handle, *UDP_SOCKET_TYPE_ID)
        }
        Err(err) => net_result_err_address(err, address),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket`, `data`, and `addr` must be null or valid, live `Value` pointers
/// returned by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_send_to(
    socket: *mut Value,
    data: *mut Value,
    addr: *mut Value,
) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    let payload = match value_to_bytes(data) {
        Ok(bytes) => bytes,
        Err(err) => return net_result_err(err),
    };
    let destination = match value_to_string(addr) {
        Ok(addr) => addr,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        sock.send_to(&payload, destination.clone())
            .map_err(|e| format!("udp send failed: {e}"))
    }) {
        Ok(written) => match byte_count_value(written, "udp send") {
            Ok(value) => net_result_ok(value),
            Err(err) => net_result_err(err),
        },
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_recv_from(socket: *mut Value, size: i64) -> *mut Value {
    let size = match socket_read_size(size) {
        Ok(size) => size,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        // Windows reports WSAEMSGSIZE when the receive buffer is smaller than
        // the datagram. Read into the maximum UDP payload and truncate after
        // the syscall so truncation has the same semantics on every platform.
        let mut buf = vec![0u8; MAX_UDP_DATAGRAM_BYTES];
        let received = sock
            .recv_from(&mut buf)
            .map_err(|e| format!("udp recv failed: {e}"))?;
        let truncated = received.0 > size;
        buf.truncate(received.0.min(size));
        udp_datagram_value(buf, received.1.to_string(), truncated)
    }) {
        Ok(value) => net_result_ok(value),
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// Return the payload from a received UDP datagram.
pub unsafe extern "C" fn mux_net_udp_datagram_bytes(datagram: *const Value) -> *mut Value {
    match udp_datagram_entry(datagram) {
        Ok(entry) => net_result_ok(Value::Bytes(entry.payload)),
        Err(error) => net_result_err(error),
    }
}

#[unsafe(no_mangle)]
/// Return the sender address from a received UDP datagram.
pub unsafe extern "C" fn mux_net_udp_datagram_address(datagram: *const Value) -> *mut Value {
    match udp_datagram_entry(datagram) {
        Ok(entry) => net_result_ok(Value::String(entry.address)),
        Err(error) => net_result_err(error),
    }
}

#[unsafe(no_mangle)]
/// Report whether the received datagram exceeded the requested receive size.
pub unsafe extern "C" fn mux_net_udp_datagram_truncated(datagram: *const Value) -> *mut Value {
    match udp_datagram_entry(datagram) {
        Ok(entry) => net_result_ok(Value::Bool(entry.truncated)),
        Err(error) => net_result_err(error),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc`. The handle is closed and invalidated by this call.
pub unsafe extern "C" fn mux_net_udp_close(socket: *mut Value) {
    if let Ok(handle) = udp_handle(socket) {
        remove_udp_socket(handle);
        write_handle(socket, 0);
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_nonblocking(
    socket: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_nonblocking(enabled != 0)
            .map_err(|e| format!("udp set_nonblocking failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_read_timeout(
    socket: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let timeout = match timeout_from_millis(timeout_ms, "udp read timeout") {
        Ok(timeout) => timeout,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_read_timeout(timeout)
            .map_err(|e| format!("udp set_read_timeout failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_write_timeout(
    socket: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let timeout = match timeout_from_millis(timeout_ms, "udp write timeout") {
        Ok(timeout) => timeout,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_write_timeout(timeout)
            .map_err(|e| format!("udp set_write_timeout failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_ttl(socket: *mut Value, ttl: i64) -> *mut Value {
    let ttl = match socket_ttl_value(ttl, "udp ttl") {
        Ok(ttl) => ttl,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_ttl(ttl)
            .map_err(|e| format!("udp set_ttl failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_ttl(socket: *mut Value) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        sock.ttl().map_err(|e| format!("udp ttl failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Int(i64::from(value))),
        Err(err) => net_result_err(err),
    }
}

fn udp_buffer_size(socket: *mut Value, size: Option<i64>, receive: bool) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    if let Some(size) = size {
        let label = if receive {
            "udp receive buffer size"
        } else {
            "udp send buffer size"
        };
        let size = match socket_buffer_size(size, label) {
            Ok(size) => size,
            Err(err) => return net_result_err(err),
        };
        return net_result_unit(with_udp_socket(handle, |socket| {
            let sock = SockRef::from(&*socket);
            let result = if receive {
                sock.set_recv_buffer_size(size)
            } else {
                sock.set_send_buffer_size(size)
            };
            result.map_err(|e| format!("udp buffer size update failed: {e}"))
        }));
    }
    match with_udp_socket(handle, |socket| {
        let sock = SockRef::from(&*socket);
        let size = if receive {
            sock.recv_buffer_size()
        } else {
            sock.send_buffer_size()
        };
        size.map_err(|e| format!("udp buffer size query failed: {e}"))
    }) {
        Ok(size) => match i64::try_from(size) {
            Ok(size) => net_result_ok(Value::Int(size)),
            Err(_) => net_result_err("udp buffer size exceeds the Mux integer range".to_string()),
        },
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_udp_set_recv_buffer_size(
    socket: *mut Value,
    size: i64,
) -> *mut Value {
    udp_buffer_size(socket, Some(size), true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_udp_recv_buffer_size(socket: *mut Value) -> *mut Value {
    udp_buffer_size(socket, None, true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_udp_set_send_buffer_size(
    socket: *mut Value,
    size: i64,
) -> *mut Value {
    udp_buffer_size(socket, Some(size), false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_udp_send_buffer_size(socket: *mut Value) -> *mut Value {
    udp_buffer_size(socket, None, false)
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_broadcast(socket: *mut Value, enabled: i32) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_broadcast(enabled != 0)
            .map_err(|e| format!("udp set_broadcast failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_broadcast(socket: *mut Value) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        sock.broadcast()
            .map_err(|e| format!("udp broadcast failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Bool(value)),
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_multicast_loop_v4(
    socket: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_multicast_loop_v4(enabled != 0)
            .map_err(|e| format!("udp set_multicast_loop_v4 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_multicast_loop_v4(socket: *mut Value) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        sock.multicast_loop_v4()
            .map_err(|e| format!("udp multicast_loop_v4 failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Bool(value)),
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_multicast_ttl_v4(
    socket: *mut Value,
    ttl: i64,
) -> *mut Value {
    let Ok(ttl) = u32::try_from(ttl) else {
        return net_result_err("udp multicast ttl must fit an unsigned 32-bit integer".to_string());
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_multicast_ttl_v4(ttl)
            .map_err(|e| format!("udp set_multicast_ttl_v4 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_multicast_ttl_v4(socket: *mut Value) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        sock.multicast_ttl_v4()
            .map_err(|e| format!("udp multicast_ttl_v4 failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Int(i64::from(value))),
        Err(err) => net_result_err(err),
    }
}

fn udp_multicast_args(
    group: *mut Value,
    interface: *mut Value,
) -> Result<(Ipv4Addr, Ipv4Addr), String> {
    Ok((
        value_to_ipv4(group, "multicast group")?,
        value_to_ipv4(interface, "multicast interface")?,
    ))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket`, `group`, and `interface` must be null or valid, live `Value`
/// pointers returned by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_join_multicast_v4(
    socket: *mut Value,
    group: *mut Value,
    interface: *mut Value,
) -> *mut Value {
    let (group, interface) = match udp_multicast_args(group, interface) {
        Ok(values) => values,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.join_multicast_v4(&group, &interface)
            .map_err(|e| format!("udp join_multicast_v4 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket`, `group`, and `interface` must be null or valid, live `Value`
/// pointers returned by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_leave_multicast_v4(
    socket: *mut Value,
    group: *mut Value,
    interface: *mut Value,
) -> *mut Value {
    let (group, interface) = match udp_multicast_args(group, interface) {
        Ok(values) => values,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.leave_multicast_v4(&group, &interface)
            .map_err(|e| format!("udp leave_multicast_v4 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_multicast_loop_v6(
    socket: *mut Value,
    enabled: i32,
) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.set_multicast_loop_v6(enabled != 0)
            .map_err(|e| format!("udp set_multicast_loop_v6 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_multicast_loop_v6(socket: *mut Value) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        sock.multicast_loop_v6()
            .map_err(|e| format!("udp multicast_loop_v6 failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Bool(value)),
        Err(err) => net_result_err(err),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_set_multicast_hops_v6(
    socket: *mut Value,
    hops: i64,
) -> *mut Value {
    let Ok(hops) = u32::try_from(hops) else {
        return net_result_err(
            "udp multicast hops must fit an unsigned 32-bit integer".to_string(),
        );
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        SockRef::from(&*sock)
            .set_multicast_hops_v6(hops)
            .map_err(|e| format!("udp set_multicast_hops_v6 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_multicast_hops_v6(socket: *mut Value) -> *mut Value {
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    match with_udp_socket(handle, |sock| {
        SockRef::from(&*sock)
            .multicast_hops_v6()
            .map_err(|e| format!("udp multicast_hops_v6 failed: {e}"))
    }) {
        Ok(value) => net_result_ok(Value::Int(i64::from(value))),
        Err(err) => net_result_err(err),
    }
}

fn udp_multicast_args_v6(group: *mut Value, interface: i64) -> Result<(Ipv6Addr, u32), String> {
    let group = value_to_ipv6(group, "multicast group")?;
    let interface = u32::try_from(interface)
        .map_err(|_| "multicast interface index must fit an unsigned 32-bit integer".to_string())?;
    Ok((group, interface))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` and `group` must be null or valid, live `Value` pointers returned
/// by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_join_multicast_v6(
    socket: *mut Value,
    group: *mut Value,
    interface: i64,
) -> *mut Value {
    let (group, interface) = match udp_multicast_args_v6(group, interface) {
        Ok(values) => values,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.join_multicast_v6(&group, interface)
            .map_err(|e| format!("udp join_multicast_v6 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` and `group` must be null or valid, live `Value` pointers returned
/// by `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_leave_multicast_v6(
    socket: *mut Value,
    group: *mut Value,
    interface: i64,
) -> *mut Value {
    let (group, interface) = match udp_multicast_args_v6(group, interface) {
        Ok(values) => values,
        Err(err) => return net_result_err(err),
    };
    let handle = match udp_handle(socket) {
        Ok(handle) => handle,
        Err(err) => return net_result_err(err),
    };
    net_result_unit(with_udp_socket(handle, |sock| {
        sock.leave_multicast_v6(&group, interface)
            .map_err(|e| format!("udp leave_multicast_v6 failed: {e}"))
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_peer_addr(socket: *mut Value) -> *mut Value {
    let result = udp_handle(socket).and_then(|handle| {
        with_udp_socket(handle, |sock| {
            sock.peer_addr()
                .map(|addr| addr.to_string())
                .map_err(|e| format!("udp peer_addr failed: {e}"))
        })
    });
    net_result_string(result)
}

#[unsafe(no_mangle)]
/// # Safety
/// `socket` must be null or a valid, live socket `Value` pointer returned by
/// `mux_rc_alloc` for the duration of this call.
pub unsafe extern "C" fn mux_net_udp_local_addr(socket: *mut Value) -> *mut Value {
    let result = udp_handle(socket).and_then(|handle| {
        with_udp_socket(handle, |sock| {
            sock.local_addr()
                .map(|addr| addr.to_string())
                .map_err(|e| format!("udp local_addr failed: {e}"))
        })
    });
    net_result_string(result)
}

#[cfg(test)]
mod tests {
    use super::{
        basic_auth_matches, bearer_auth_matches, configure_http_server_socket_timeouts,
        constant_time_equal, cors_response_headers, default_http_request_options,
        default_http_server_limits, execute_http_response, http_header_end_within_limit,
        http_timeout_remaining, insert_http_request, insert_sse_event, insert_sse_stream,
        is_https_url, lock_requests, mux_net_http_server_config_heartbeat_interval_ms,
        mux_net_http_server_config_new, mux_net_http_server_config_set_heartbeat_interval_ms,
        mux_net_http_server_serve_until_cancelled, mux_net_sse_stream_close,
        mux_net_sse_stream_send, mux_net_tcp_listener_bind, oauth_oidc_token_is_valid,
        parse_http_request_header_pairs, reap_finished_http_server_actors, receive_http_server_job,
        request_handle, request_uses_chunked_transfer_encoding, response_read,
        response_read_to_end, sanitize_http_log_field, send_http_server_job,
        should_use_environment_proxy, static_file_response, stream_http_response_data,
        validate_http_buffered_body, validate_http_header_budget, validate_http_request_host,
        validate_http_request_trailer, validated_http_response_headers,
        write_typed_http_response_to, HeaderData, HttpRequestEntry, HttpRequestExecution,
        HttpResponseEntry, HttpServerJob, HttpServerRequestSnapshot, OAuthClientEntry, OAuthRsaKey,
        OAuthSessionEntry, SseEventEntry, StreamingHeartbeat, StreamingSocketActor,
        BASE64_URL_SAFE, HTTP_SERVER_POOL_POLL_INTERVAL, MAX_HTTP_BODY_BYTES,
        MAX_HTTP_HEADERS_COUNT, MAX_HTTP_HEADER_BYTES, OAUTH_JWKS_CACHE,
    };
    #[cfg(feature = "http3")]
    use super::{
        http3_connection_error_can_fallback, http3_pre_dispatch_connection_error_can_fallback,
        Http3ClientTransport, Http3Error, Http3ErrorKind, Http3Response, Http3ServerTransport,
        BASE64_STANDARD,
    };
    use crate::Value;
    use base64::Engine as _;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::fs;
    use std::io::{Cursor, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(feature = "http2")]
    use super::http2_request_uri;

    #[cfg(feature = "http2")]
    use super::start_http2_connection_with_transport;

    #[cfg(feature = "http2")]
    use bytes::Bytes;

    fn tampered_oauth_signature(token: &str) -> String {
        let (signing_input, encoded_signature) = token
            .rsplit_once('.')
            .expect("signed JWT should have three parts");
        let mut signature = BASE64_URL_SAFE
            .decode(encoded_signature)
            .expect("signed JWT should contain a base64url signature");
        signature[0] ^= 1;
        format!("{signing_input}.{}", BASE64_URL_SAFE.encode(signature))
    }

    fn oauth_token_with_claims(token: &str, claims: serde_json::Value) -> String {
        let mut parts = token.split('.');
        let header = parts.next().expect("JWT should have a header");
        let _claims = parts.next().expect("JWT should have claims");
        let signature = parts.next().expect("JWT should have a signature");
        assert!(
            parts.next().is_none(),
            "JWT should have exactly three parts"
        );
        let encoded_claims = BASE64_URL_SAFE
            .encode(serde_json::to_vec(&claims).expect("JWT claims should serialize"));
        format!("{header}.{encoded_claims}.{signature}")
    }

    fn oauth_token_with_header(token: &str, header: serde_json::Value) -> String {
        let mut parts = token.split('.');
        let _header = parts.next().expect("JWT should have a header");
        let claims = parts.next().expect("JWT should have claims");
        let signature = parts.next().expect("JWT should have a signature");
        assert!(
            parts.next().is_none(),
            "JWT should have exactly three parts"
        );
        let encoded_header = BASE64_URL_SAFE
            .encode(serde_json::to_vec(&header).expect("JWT header should serialize"));
        format!("{encoded_header}.{claims}.{signature}")
    }

    fn oauth_string(value: &str) -> *mut Value {
        crate::refcount::mux_rc_alloc(Value::String(value.to_string()))
    }

    fn take_mux_result(value: *mut Value) -> Value {
        let result = unsafe { (&*value).clone() };
        unsafe { crate::refcount::mux_rc_dec(value) };
        result
    }

    #[test]
    fn oauth_client_builds_pkce_authorization_url_and_keeps_config_private() {
        let client = super::insert_oauth_client(OAuthClientEntry {
            issuer: "https://issuer.example".to_string(),
            client_id: "mux-client".to_string(),
            redirect_uri: "https://app.example/callback".to_string(),
            scopes: vec!["openid".to_string(), "profile".to_string()],
            authorization_endpoint: Some("https://issuer.example/authorize".to_string()),
            token_endpoint: Some("https://issuer.example/token".to_string()),
            revocation_endpoint: None,
            introspection_endpoint: None,
            names: 1,
        });
        let state = oauth_string("state-123");
        let verifier =
            oauth_string("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc");
        let nonce = oauth_string("nonce-123");
        let result = unsafe {
            super::mux_net_oauth_client_authorization_url(client, state, verifier, nonce)
        };
        let value = take_mux_result(result);
        let Value::Result(Ok(value)) = value else {
            panic!("authorization URL should be built")
        };
        let Value::String(url) = *value else {
            panic!("authorization result should be a string")
        };
        let parsed = url::Url::parse(&url).expect("authorization URL should parse");
        assert_eq!(parsed.path(), "/authorize");
        let query = parsed.query_pairs().collect::<HashMap<_, _>>();
        let query_value = |name: &str| query.get(name).map(ToString::to_string);
        assert_eq!(query_value("response_type"), Some("code".to_string()));
        assert_eq!(query_value("client_id"), Some("mux-client".to_string()));
        assert_eq!(query_value("scope"), Some("openid profile".to_string()));
        assert_eq!(
            query_value("code_challenge_method"),
            Some("S256".to_string())
        );
        assert!(query
            .get("code_challenge")
            .is_some_and(|value| value.len() == 43));
        unsafe {
            crate::refcount::mux_rc_dec(client);
            crate::refcount::mux_rc_dec(state);
            crate::refcount::mux_rc_dec(verifier);
            crate::refcount::mux_rc_dec(nonce);
        }
    }

    #[test]
    fn oauth_client_configuration_rejects_insecure_non_loopback_redirects() {
        let error = super::validate_oauth_client_config(
            "https://issuer.example",
            "client",
            "http://app.example/callback",
            "openid profile",
        )
        .expect_err("non-loopback HTTP redirects must be rejected");
        assert!(error.contains("loopback"));
        assert!(super::validate_oauth_client_config(
            "https://issuer.example",
            "client",
            "http://127.0.0.1:8080/callback",
            "openid profile",
        )
        .is_ok());
    }

    #[test]
    fn oauth_session_owns_tokens_and_reports_expiry_without_leaking_values() {
        let document = super::Json::parse(
            r#"{"access_token":"access-value","refresh_token":"refresh-value","id_token":"id-value","token_type":"Bearer","expires_in":0}"#,
        )
        .expect("token fixture should parse");
        let entry =
            super::oauth_session_from_json(&document).expect("token fixture should convert");
        assert_eq!(entry.access_token, "access-value");
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh-value"));
        assert_eq!(entry.id_token.as_deref(), Some("id-value"));
        assert!(entry.expires_at.is_some_and(|time| Instant::now() >= time));

        let session = super::insert_oauth_session(OAuthSessionEntry {
            access_token: entry.access_token,
            refresh_token: entry.refresh_token,
            id_token: entry.id_token,
            token_type: entry.token_type,
            expires_at: entry.expires_at,
            names: 1,
        });
        let access = unsafe { super::mux_net_oauth_session_access_token(session) };
        let access = take_mux_result(access);
        let Value::Result(Ok(access)) = access else {
            panic!("session access token should be readable")
        };
        assert_eq!(*access, Value::String("access-value".to_string()));
        unsafe {
            super::mux_net_oauth_session_close(session);
            assert!(super::oauth_session_handle(session).is_err());
            crate::refcount::mux_rc_dec(session);
        }
    }

    #[cfg(feature = "http3")]
    #[test]
    fn http3_server_rejects_invalid_configuration_before_binding() {
        let Err(invalid_address) = Http3ServerTransport::bind("not-an-address", &[], &[]) else {
            panic!("invalid address must fail before endpoint setup")
        };
        assert_eq!(invalid_address.kind(), Http3ErrorKind::Invalid);

        let Err(missing_certificate) = Http3ServerTransport::bind("127.0.0.1:0", &[], &[1, 2, 3])
        else {
            panic!("an empty certificate chain must be rejected")
        };
        assert_eq!(missing_certificate.kind(), Http3ErrorKind::Invalid);
    }

    #[cfg(feature = "http3")]
    #[test]
    fn http3_client_deadline_only_bounds_setup_and_drop_closes_actor() {
        let certificate = BASE64_STANDARD
            .decode(include_str!("../tests/fixtures/http3_localhost_cert.der.b64").trim())
            .expect("HTTP/3 test certificate fixture must be base64");
        let private_key = BASE64_STANDARD
            .decode(include_str!("../tests/fixtures/http3_localhost_key.der.b64").trim())
            .expect("HTTP/3 test key fixture must be base64");
        let server = Http3ServerTransport::bind(
            "127.0.0.1:0",
            std::slice::from_ref(&certificate),
            &private_key,
        )
        .expect("HTTP/3 test server should bind");
        let authority = server.local_addr().to_string();
        let server_thread = thread::spawn(move || {
            server.serve_one(|_| {
                Ok(Http3Response {
                    status: 200,
                    headers: Vec::new(),
                    body: b"ok".to_vec(),
                })
            })
        });

        let client = Http3ClientTransport::connect_with_timeout_and_roots(
            &authority,
            Duration::from_millis(100),
            Some(vec![certificate]),
        )
        .expect("HTTP/3 client should finish setup before its deadline");
        thread::sleep(Duration::from_millis(150));
        let response = client
            .send(
                http::Request::builder()
                    .method("GET")
                    .uri(format!("https://{authority}/"))
                    .version(http::Version::HTTP_3)
                    .body(())
                    .expect("HTTP/3 test request should be valid"),
                None,
                Some(Duration::from_secs(1)),
            )
            .expect("an established HTTP/3 actor must outlive setup timeout");
        assert_eq!(response.body, b"ok");
        server_thread
            .join()
            .expect("HTTP/3 server thread should not panic")
            .expect("HTTP/3 server should serve the request");

        let actor_done = Arc::clone(&client.actor_done);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !actor_done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            actor_done.load(Ordering::Acquire),
            "dropping the final request sender must stop the HTTP/3 actor"
        );
    }

    #[test]
    fn oauth_oidc_rs256_verification_uses_cached_jwks_without_network() {
        let modulus = BASE64_URL_SAFE
            .decode("yKeFAKWiUNuO02yFuNz4PEvhlTEU-qrHYW4Ookki-mt6sB-FWCyBXMO9617UZ2K8U2rMqotycFsAzvMWsuxQj7lpckG540I4QZzM9zOe64sGIUevT1ky9hPZvArnC_bVbUQy6D4TdnWHUxv6ndVlMXQSRL516LySJrn6RLS4oQE1jX6Lt10MckpPEeznd3YmP67-eWEusdcWRud-iYKGa-FADq_DWA0xObQaqnOAGHNy8i41vVWyiElhZciB7RVNWBEkXFLVbMCdSRbU8qULz1rgomN_TPpr-dqvwRPbqDg7bdfabdjbIthRCo0xFZgzCJCaGgMyUXqlXoluFUJJsw")
            .expect("RSA modulus fixture should be base64url");
        let jwks_url = "https://oauth-rs256.test/keys";
        let issuer = "https://oauth-rs256.test";
        let audience = "mux-client";
        OAUTH_JWKS_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                jwks_url.to_string(),
                (
                    Instant::now(),
                    vec![OAuthRsaKey {
                        kid: "mux-test-key".to_string(),
                        modulus,
                        exponent: vec![1, 0, 1],
                    }],
                ),
            );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock should be after Unix epoch")
            .as_secs();
        let valid = "eyJhbGciOiJSUzI1NiIsImtpZCI6Im11eC10ZXN0LWtleSIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJodHRwczovL29hdXRoLXJzMjU2LnRlc3QiLCJhdWQiOlsibXV4LWNsaWVudCJdLCJleHAiOjQxMDI0NDQ4MDAsIm5iZiI6MTcwMDAwMDAwMH0.NqXynPolsIxGB0dmKAO6Qii79RQk8XvSyWg12FwykWVzB9ttjz9ZvRPKQZBwzONNasyZsiZTirxgt1yYkvhVdP_jzRfwfkOqj8ld23lrNOX9nE6D03hsOR-tzGVhOeVx48xwtvgiij2nbSi476S_ntZMh-6IbSbpW0tT7un3-mHvZROb6Reld6KrVegcszo54EjzphHog5OQhSUR9ORRMjHPxgRkszanVJTOZy5GWX18KFoQbYiOMjY7QwTgNInmZ1eEP30IcZ9o4H0Nhz3l-lnYvQg-_IyAInN9dvyNtqU0C5kEjcE7uwE9o7neLtBmM9mDLAckJFjArVt1na8F_w";
        assert_eq!(
            oauth_oidc_token_is_valid(valid, issuer, audience, jwks_url),
            Ok(true)
        );
        assert_eq!(
            oauth_oidc_token_is_valid(&tampered_oauth_signature(valid), issuer, audience, jwks_url,),
            Ok(false)
        );

        let wrong_issuer = serde_json::json!({
            "iss": "https://other-issuer.test",
            "aud": audience,
            "exp": 4_102_444_800_u64,
        });
        assert_eq!(
            oauth_oidc_token_is_valid(
                &oauth_token_with_claims(valid, wrong_issuer),
                issuer,
                audience,
                jwks_url,
            ),
            Ok(false)
        );

        let wrong_audience = serde_json::json!({
            "iss": issuer,
            "aud": "another-client",
            "exp": now + 3600,
        });
        assert_eq!(
            oauth_oidc_token_is_valid(
                &oauth_token_with_claims(valid, wrong_audience),
                issuer,
                audience,
                jwks_url,
            ),
            Ok(false)
        );

        let expired = serde_json::json!({"iss": issuer, "aud": audience, "exp": 1});
        assert_eq!(
            oauth_oidc_token_is_valid(
                &oauth_token_with_claims(valid, expired),
                issuer,
                audience,
                jwks_url,
            ),
            Ok(false)
        );

        let not_yet_valid = serde_json::json!({
            "iss": issuer,
            "aud": audience,
            "exp": 4_102_444_800_u64,
            "nbf": 4_102_444_800_u64,
        });
        assert_eq!(
            oauth_oidc_token_is_valid(
                &oauth_token_with_claims(valid, not_yet_valid),
                issuer,
                audience,
                jwks_url,
            ),
            Ok(false)
        );

        let wrong_algorithm = serde_json::json!({
            "alg": "HS256",
            "kid": "mux-test-key",
            "typ": "JWT"
        });
        assert_eq!(
            oauth_oidc_token_is_valid(
                &oauth_token_with_header(valid, wrong_algorithm),
                issuer,
                audience,
                jwks_url,
            ),
            Ok(false)
        );

        let unknown_key = serde_json::json!({
            "alg": "RS256",
            "kid": "unknown-key",
            "typ": "JWT"
        });
        assert_eq!(
            oauth_oidc_token_is_valid(
                &oauth_token_with_header(valid, unknown_key),
                issuer,
                audience,
                jwks_url,
            ),
            Ok(false)
        );

        OAUTH_JWKS_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(jwks_url);
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_client_config_selects_ring_without_panicking() {
        let result =
            std::panic::catch_unwind(|| super::http2_client_config(rustls::RootCertStore::empty()));
        assert!(result.is_ok(), "HTTP/2 TLS config must not panic");
        assert!(result.expect("HTTP/2 TLS config panic result").is_ok());
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_request_uri_preserves_query_and_drops_fragment() {
        let url = url::Url::parse("https://example.test/items/a%2Fb?q=1%2F2#ignored").unwrap();
        assert_eq!(
            http2_request_uri(&url, "example.test:443"),
            "https://example.test:443/items/a%2Fb?q=1%2F2"
        );
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_actor_multiplexes_streams_and_reuses_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let slow_release = Arc::new(tokio::sync::Notify::new());
        let server_release = Arc::clone(&slow_release);
        let response_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_response_complete = Arc::clone(&response_complete);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(super::HTTP2_IO_POLL_INTERVAL))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(async move {
                    let mut connection = h2::server::handshake(super::BlockingAsync(stream))
                        .await
                        .map_err(|error| format!("server handshake failed: {error}"))?;
                    let mut responses = Vec::new();
                    for _ in 0..3 {
                        let Some(result) = connection.accept().await else {
                            return Err(
                                "server connection closed before three requests".to_string()
                            );
                        };
                        let (request, mut respond) =
                            result.map_err(|error| format!("server request failed: {error}"))?;
                        let path = request.uri().path().to_string();
                        let mut body = request.into_body();
                        while let Some(chunk) = body.data().await {
                            let chunk =
                                chunk.map_err(|error| format!("request body failed: {error}"))?;
                            let length = chunk.len();
                            body.flow_control()
                                .release_capacity(length)
                                .map_err(|error| {
                                    format!("request flow-control update failed: {error}")
                                })?;
                        }
                        let release = Arc::clone(&server_release);
                        responses.push(tokio::spawn(async move {
                            if path == "/slow" {
                                release.notified().await;
                            }
                            let body = if path == "/large" {
                                vec![b'x'; 128 * 1024]
                            } else {
                                path.trim_start_matches('/').as_bytes().to_vec()
                            };
                            let response = http::Response::builder()
                                .status(200)
                                .header("x-fixture-path", &path)
                                .body(())
                                .map_err(|error| format!("response build failed: {error}"))?;
                            let mut send = respond
                                .send_response(response, false)
                                .map_err(|error| format!("response headers failed: {error}"))?;
                            send.send_data(Bytes::from(body), true)
                                .map_err(|error| format!("response body failed: {error}"))?;
                            Ok::<(), String>(())
                        }));
                    }
                    while !server_response_complete.load(std::sync::atomic::Ordering::Acquire) {
                        match tokio::time::timeout(Duration::from_millis(100), connection.accept())
                            .await
                        {
                            Ok(None) => break,
                            Ok(Some(Err(error))) => {
                                return Err(format!("server drain failed: {error}"));
                            }
                            Ok(Some(Ok((_request, _respond)))) => {}
                            Err(_) => {}
                        }
                    }
                    for response in responses {
                        response
                            .await
                            .map_err(|error| format!("response task failed: {error}"))??;
                    }
                    Ok::<(), String>(())
                })
                .unwrap();
        });

        let client_stream = TcpStream::connect(address).unwrap();
        client_stream
            .set_read_timeout(Some(super::HTTP2_IO_POLL_INTERVAL))
            .unwrap();
        let actor = start_http2_connection_with_transport(client_stream)
            .expect("fixture actor should start");
        let slow_actor = Arc::clone(&actor);
        let slow = std::thread::spawn(move || {
            slow_actor.request(
                http::Request::builder()
                    .method("GET")
                    .uri("https://fixture.test/slow")
                    .version(http::Version::HTTP_2)
                    .body(())
                    .unwrap(),
                None,
                Some(Duration::from_secs(2)),
            )
        });
        let fast_actor = Arc::clone(&actor);
        let fast = std::thread::spawn(move || {
            fast_actor.request(
                http::Request::builder()
                    .method("GET")
                    .uri("https://fixture.test/fast")
                    .version(http::Version::HTTP_2)
                    .body(())
                    .unwrap(),
                None,
                Some(Duration::from_secs(2)),
            )
        });
        let fast_response = fast.join().unwrap().expect("fast stream response");
        assert_eq!(fast_response.status, 200);
        assert_eq!(fast_response.body, b"fast");
        slow_release.notify_one();
        let slow_response = slow.join().unwrap().expect("slow stream response");
        assert_eq!(slow_response.status, 200);
        assert_eq!(slow_response.body, b"slow");

        let large_response = actor
            .request(
                http::Request::builder()
                    .method("GET")
                    .uri("https://fixture.test/large")
                    .version(http::Version::HTTP_2)
                    .body(())
                    .unwrap(),
                None,
                Some(Duration::from_secs(2)),
            )
            .expect("reused stream response");
        assert_eq!(large_response.status, 200);
        assert_eq!(large_response.body.len(), 128 * 1024);
        assert!(large_response.body.iter().all(|byte| *byte == b'x'));
        response_complete.store(true, std::sync::atomic::Ordering::Release);
        drop(actor);
        server.join().unwrap();
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_actor_cancels_an_in_flight_response_and_keeps_the_connection_usable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(async move {
                    let mut connection = h2::server::handshake(super::BlockingAsync(stream))
                        .await
                        .map_err(|error| format!("server handshake failed: {error}"))?;
                    let Some(result) = connection.accept().await else {
                        return Err("client closed before the request arrived".to_string());
                    };
                    let (request, mut first_respond) =
                        result.map_err(|error| format!("server request failed: {error}"))?;
                    let mut body = request.into_body();
                    while let Some(chunk) = body.data().await {
                        let Ok(chunk) = chunk else {
                            // The client resets the timed-out stream. That is
                            // the expected cancellation path for this fixture;
                            // it must not prevent the second stream from
                            // proving that the connection remains usable.
                            break;
                        };
                        body.flow_control()
                            .release_capacity(chunk.len())
                            .map_err(|error| {
                                format!("request flow-control update failed: {error}")
                            })?;
                    }
                    let Some(result) = connection.accept().await else {
                        return Err("client closed before the second request arrived".to_string());
                    };
                    let (second_request, mut second_respond) =
                        result.map_err(|error| format!("second request failed: {error}"))?;
                    let mut second_body = second_request.into_body();
                    while let Some(chunk) = second_body.data().await {
                        let chunk = chunk
                            .map_err(|error| format!("second request body failed: {error}"))?;
                        second_body
                            .flow_control()
                            .release_capacity(chunk.len())
                            .map_err(|error| {
                                format!("second request flow-control update failed: {error}")
                            })?;
                    }
                    let response = http::Response::builder()
                        .status(200)
                        .body(())
                        .map_err(|error| format!("response build failed: {error}"))?;
                    second_respond
                        .send_response(response, true)
                        .map_err(|error| format!("second response headers failed: {error}"))?;
                    let first_response = http::Response::builder()
                        .status(200)
                        .body(())
                        .map_err(|error| format!("first response build failed: {error}"))?;
                    let _ = first_respond.send_response(first_response, true);
                    let _ = tokio::time::timeout(Duration::from_secs(1), async {
                        while let Some(Ok((_request, _respond))) = connection.accept().await {}
                    })
                    .await;
                    Ok::<(), String>(())
                })
                .unwrap();
        });

        let client_stream = TcpStream::connect(address).unwrap();
        client_stream
            .set_read_timeout(Some(super::HTTP2_IO_POLL_INTERVAL))
            .unwrap();
        let actor = start_http2_connection_with_transport(client_stream)
            .expect("fixture actor should start");
        let started = Instant::now();
        let Err(error) = actor.request(
            http::Request::builder()
                .method("GET")
                .uri("https://fixture.test/cancel")
                .version(http::Version::HTTP_2)
                .body(())
                .unwrap(),
            None,
            Some(Duration::from_millis(50)),
        ) else {
            panic!("the pending response must be cancelled")
        };
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        let response = actor
            .request(
                http::Request::builder()
                    .method("GET")
                    .uri("https://fixture.test/after-cancel")
                    .version(http::Version::HTTP_2)
                    .body(())
                    .unwrap(),
                None,
                Some(Duration::from_secs(1)),
            )
            .expect("the actor must accept a request after cancellation");
        assert_eq!(response.status, 200);
        drop(actor);
        server.join().unwrap();
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_actor_maps_a_connection_protocol_error_to_a_typed_message() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            // A valid empty SETTINGS frame followed by DATA on stream zero.
            // The latter is a connection-level HTTP/2 protocol error.
            stream
                .write_all(&[
                    0, 0, 0, 4, 0, 0, 0, 0, 0, // SETTINGS
                    0, 0, 0, 0, 0, 0, 0, 0, 0, // DATA on stream 0
                ])
                .unwrap();
        });
        let client_stream = TcpStream::connect(address).unwrap();
        client_stream
            .set_read_timeout(Some(super::HTTP2_IO_POLL_INTERVAL))
            .unwrap();
        let actor = start_http2_connection_with_transport(client_stream)
            .expect("HTTP/2 handshake should receive the peer settings");
        let Err(error) = actor.request(
            http::Request::builder()
                .method("GET")
                .uri("https://fixture.test/protocol-error")
                .version(http::Version::HTTP_2)
                .body(())
                .unwrap(),
            None,
            Some(Duration::from_secs(1)),
        ) else {
            panic!("stream creation must observe the peer protocol error")
        };
        assert!(error.contains("HTTP/2"));
        drop(actor);
        server.join().unwrap();
    }

    #[test]
    fn parameterized_routes_decode_segments_without_decoding_slashes_first() {
        let (segments, specificity) =
            super::compile_http_route("/users/{id}/files/{...rest}").expect("route pattern");
        let route = super::HttpRouteEntry {
            method: "GET".to_string(),
            segments,
            specificity,
            handler: 1,
        };
        let path = super::request_path_segments("/users/a%2Fb/files/one%2Ftwo/three?ignored=yes")
            .expect("request path");
        let captures = super::http_route_match(&route, &path).expect("route match");
        assert_eq!(captures.get("id"), Some(&"a/b".to_string()));
        assert_eq!(captures.get("rest"), Some(&"one/two/three".to_string()));
    }

    #[test]
    fn catch_all_routes_capture_zero_remaining_segments() {
        let (segments, specificity) = super::compile_http_route("/files/{...rest}").unwrap();
        let route = super::HttpRouteEntry {
            method: "GET".to_string(),
            segments,
            specificity,
            handler: 1,
        };
        let path = super::request_path_segments("/files").unwrap();
        let captures = super::http_route_match(&route, &path).unwrap();
        assert_eq!(captures.get("rest"), Some(&String::new()));
    }

    #[test]
    fn route_registration_rejects_ambiguous_patterns() {
        let first = super::compile_http_route("/users/{id}").unwrap();
        let second = super::compile_http_route("/users/{name}").unwrap();
        assert_eq!(first.1, second.1);
        assert!(first
            .0
            .iter()
            .zip(second.0.iter())
            .all(|(left, right)| match (left, right) {
                (
                    super::HttpRouteSegment::Literal(left),
                    super::HttpRouteSegment::Literal(right),
                ) => left == right,
                (super::HttpRouteSegment::Parameter(_), super::HttpRouteSegment::Parameter(_)) =>
                    true,
                _ => false,
            }));
        assert!(super::compile_http_route("/users/{id}/{id}").is_err());
        assert!(super::compile_http_route("/users/{...rest}/tail").is_err());
    }

    #[test]
    fn malformed_route_paths_are_rejected() {
        for path in ["users/{id}", "/users/%", "/users/%zz", "/users/%ff"] {
            assert!(
                super::request_path_segments(path).is_err()
                    || super::compile_http_route(path).is_err(),
                "accepted malformed route input {path:?}"
            );
        }
    }

    #[test]
    fn websocket_session_reader_owns_ping_pong_and_reassembles_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept test client");
            let message = super::read_websocket_message(&mut socket, None).expect("read message");
            assert_eq!(message.opcode, 1);
            assert_eq!(message.payload, b"hello");
        });

        let mut client = TcpStream::connect(address).expect("connect test client");
        let ping = super::encode_websocket_frame(&super::WebSocketFrameEntry {
            fin: true,
            opcode: 9,
            payload: b"ping".to_vec(),
            masked: true,
            names: 1,
        })
        .expect("encode ping");
        let first = super::encode_websocket_frame(&super::WebSocketFrameEntry {
            fin: false,
            opcode: 1,
            payload: b"hel".to_vec(),
            masked: true,
            names: 1,
        })
        .expect("encode first fragment");
        let second = super::encode_websocket_frame(&super::WebSocketFrameEntry {
            fin: true,
            opcode: 0,
            payload: b"lo".to_vec(),
            masked: true,
            names: 1,
        })
        .expect("encode continuation");
        client
            .write_all(&ping)
            .and_then(|()| client.write_all(&first))
            .and_then(|()| client.write_all(&second))
            .expect("write websocket frames");
        let mut pong = [0_u8; 6];
        client.read_exact(&mut pong).expect("read automatic pong");
        assert_eq!(pong, [0x8a, 0x04, b'p', b'i', b'n', b'g']);
        server.join().expect("server thread");
    }

    #[test]
    fn cors_and_static_file_policy_are_bounded_and_origin_aware() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("mux-http-static-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).expect("create static test root");
        fs::write(root.join("index.html"), b"hello").expect("write static test file");
        fs::create_dir(root.join("nested")).expect("create static test directory");
        let request = insert_http_request(HttpRequestEntry {
            method: "GET".to_string(),
            url: "/index.html?cache=1".to_string(),
            request_id: String::new(),
            proxy: None,
            headers: Arc::new(Mutex::new(HeaderData {
                values: vec![("origin".to_string(), "https://app.example".to_string())],
            })),
            body: None,
            body_reader: None,
            path_params: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout_ms: 0,
            timeout_ms: 0,
            max_redirects: 0,
            retries: 0,
            retry_backoff_ms: 0,
            names: 1,
        });
        let mut limits = default_http_server_limits();
        limits.static_root = root.to_string_lossy().into_owned();
        limits.cors_origins = vec!["https://app.example".to_string()];
        limits.cors_allow_credentials = true;

        let (status, headers, body) = static_file_response(request, &limits)
            .expect("static policy should be enabled")
            .expect("static file should be readable");
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
        assert!(headers.iter().any(|(name, value)| {
            name == "content-type" && value == "text/html; charset=utf-8"
        }));
        let etag = headers
            .iter()
            .find(|(name, _)| name == "etag")
            .map(|(_, value)| value.clone())
            .expect("static response should have an ETag");
        let cors = cors_response_headers(Vec::new(), request, &limits);
        assert!(cors.iter().any(|(name, value)| {
            name == "access-control-allow-origin" && value == "https://app.example"
        }));

        let request_id = request_handle(request).expect("request handle");
        lock_requests()
            .get_mut(&request_id)
            .expect("request entry")
            .headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .push(("if-none-match".to_string(), format!("\"stale\", W/{etag}")));
        let (status, headers, body) = static_file_response(request, &limits)
            .expect("static policy should remain enabled")
            .expect("conditional static request should be readable");
        assert_eq!(status, 304);
        assert!(body.is_empty());
        assert!(headers
            .iter()
            .any(|(name, value)| name == "etag" && value == &etag));

        fs::write(root.join("index.html"), b"world").expect("replace static test file");
        let (status, headers, body) = static_file_response(request, &limits)
            .expect("static policy should remain enabled")
            .expect("changed static file should be readable");
        assert_eq!(status, 200);
        assert_eq!(body, b"world");
        let changed_etag = headers
            .iter()
            .find(|(name, _)| name == "etag")
            .map(|(_, value)| value)
            .expect("changed static response should have an ETag");
        assert_ne!(changed_etag, &etag);

        lock_requests()
            .get_mut(&request_id)
            .expect("request entry")
            .url = "/../index.html".to_string();
        let (_, _, body) = static_file_response(request, &limits)
            .expect("static policy should remain enabled")
            .expect("traversal should be represented as a response");
        assert_eq!(body, b"not found");

        for path in [
            "/",
            "/nested",
            "/%2e%2e/index.html",
            "/%2fetc/passwd",
            "/%5cetc%5cpasswd",
        ] {
            lock_requests()
                .get_mut(&request_id)
                .expect("request entry")
                .url = path.to_string();
            let (_, _, body) = static_file_response(request, &limits)
                .expect("static policy should remain enabled")
                .expect("unsafe static path should be represented as a response");
            assert_eq!(body, b"not found", "static path should be rejected: {path}");
        }

        #[cfg(unix)]
        {
            let outside = root.with_file_name(format!("mux-http-static-outside-{nonce}"));
            fs::write(&outside, b"outside").expect("write symlink target");
            std::os::unix::fs::symlink(&outside, root.join("escape.txt"))
                .expect("create static escape symlink");
            lock_requests()
                .get_mut(&request_id)
                .expect("request entry")
                .url = "/escape.txt".to_string();
            let (_, _, body) = static_file_response(request, &limits)
                .expect("static policy should remain enabled")
                .expect("symlink escape should be represented as a response");
            assert_eq!(body, b"not found");
            fs::remove_file(outside).expect("remove symlink target");
        }

        unsafe { crate::refcount::mux_rc_dec(request) };
        fs::remove_dir_all(root).expect("remove static test root");
    }

    #[test]
    fn worker_pool_channel_operations_notice_cancellation() {
        let (sender, _receiver) = mpsc::sync_channel(0);
        let (response_sender, _response_receiver) = mpsc::sync_channel(1);
        let job = HttpServerJob {
            request: HttpServerRequestSnapshot {
                method: "GET".to_string(),
                url: "/".to_string(),
                request_id: String::new(),
                headers: Vec::new(),
                body: Vec::new(),
            },
            limits: default_http_server_limits(),
            response_sender,
        };
        let cancelled = AtomicBool::new(true);
        assert!(send_http_server_job(&sender, job, &cancelled).is_err());

        let (job_sender, job_receiver) = mpsc::sync_channel(1);
        let receiver = Arc::new(Mutex::new(job_receiver));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (finished_sender, finished_receiver) = mpsc::sync_channel(1);
        let receiver_for_worker = Arc::clone(&receiver);
        let cancelled_for_worker = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            let result = receive_http_server_job(&receiver_for_worker, &cancelled_for_worker);
            finished_sender
                .send(result.is_none())
                .expect("test receiver");
        });
        std::thread::sleep(HTTP_SERVER_POOL_POLL_INTERVAL + HTTP_SERVER_POOL_POLL_INTERVAL);
        cancelled.store(true, Ordering::Release);
        assert!(finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should observe cancellation"));
        worker.join().expect("worker should exit cleanly");
        drop(job_sender);
    }

    #[test]
    fn finished_http_server_actor_handles_are_reaped_incrementally() {
        let cancelled = AtomicBool::new(false);
        let first_error = Mutex::new(None);
        let (finished_sender, finished_receiver) = mpsc::sync_channel(1);
        let actor = thread::spawn(move || {
            finished_sender.send(()).expect("signal actor completion");
        });
        finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("actor should finish");
        while !actor.is_finished() {
            thread::yield_now();
        }
        let mut actors = vec![actor];

        reap_finished_http_server_actors(&mut actors, &cancelled, &first_error);

        assert!(actors.is_empty());
        assert!(first_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
    }

    #[test]
    fn http_server_zero_timeout_does_not_change_write_policy() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind timeout test listener");
        let address = listener
            .local_addr()
            .expect("timeout test listener address");
        let client = TcpStream::connect(address).expect("connect timeout test client");
        let (server, _) = listener.accept().expect("accept timeout test client");
        let existing = Duration::from_secs(1);
        server
            .set_write_timeout(Some(existing))
            .expect("set existing write timeout");

        configure_http_server_socket_timeouts(&server, 0).expect("zero timeout policy");
        assert_eq!(
            server.write_timeout().expect("read write timeout"),
            Some(existing)
        );

        configure_http_server_socket_timeouts(&server, 25).expect("positive timeout policy");
        assert!(server
            .write_timeout()
            .expect("read configured write timeout")
            .is_some());
        drop(client);
    }

    #[cfg(feature = "http3")]
    #[test]
    fn http3_fallback_is_limited_to_pre_dispatch_connection_errors() {
        let pre_dispatch = Http3Error::new(Http3ErrorKind::Transport, "connection closed");
        assert!(http3_connection_error_can_fallback(&pre_dispatch));
        assert!(http3_pre_dispatch_connection_error_can_fallback(
            &pre_dispatch
        ));

        let post_dispatch = pre_dispatch.mark_dispatched();
        assert!(!http3_pre_dispatch_connection_error_can_fallback(
            &post_dispatch
        ));

        let protocol = Http3Error::new(Http3ErrorKind::Protocol, "invalid request");
        assert!(!http3_connection_error_can_fallback(&protocol));
    }

    #[test]
    fn sse_stream_writes_encoded_events_and_releases_tcp_lease() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE test listener");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept SSE client");
            let mut bytes = vec![0_u8; 32];
            let count = socket.read(&mut bytes).expect("read SSE event");
            bytes.truncate(count);
            assert_eq!(bytes, b"event: message\ndata: hello\n\n");
        });

        let client = TcpStream::connect(address).expect("connect SSE client");
        let actor = StreamingSocketActor::new(client, 1_000, None).expect("start SSE actor");
        let sse = insert_sse_stream(actor);
        assert!(!sse.is_null());
        let event = insert_sse_event(SseEventEntry {
            event: "message".to_string(),
            id: String::new(),
            retry_ms: 0,
            data: "hello".to_string(),
            names: 1,
        });
        assert!(!event.is_null());
        let result = unsafe { mux_net_sse_stream_send(sse, event) };
        unsafe { crate::refcount::mux_rc_dec(result) };
        unsafe { mux_net_sse_stream_close(sse) };
        unsafe { crate::refcount::mux_rc_dec(sse) };
        unsafe { crate::refcount::mux_rc_dec(event) };
        server.join().expect("SSE server thread");
    }

    #[test]
    fn streaming_actor_emits_configured_heartbeats() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind heartbeat listener");
        let address = listener.local_addr().expect("heartbeat listener address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept heartbeat client");
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("set heartbeat read timeout");
            let mut sse = [0_u8; 13];
            socket.read_exact(&mut sse).expect("read SSE heartbeat");
            assert_eq!(&sse, b": heartbeat\n\n");
        });
        let client = TcpStream::connect(address).expect("connect heartbeat client");
        let _actor = StreamingSocketActor::new(
            client,
            1_000,
            Some((StreamingHeartbeat::Sse, Duration::from_millis(10))),
        )
        .expect("start heartbeat actor");
        server.join().expect("heartbeat server thread");
    }

    #[test]
    fn heartbeat_interval_rejects_negative_and_oversized_values() {
        let config = unsafe { mux_net_http_server_config_new() };
        let negative = crate::refcount::mux_rc_alloc(Value::Int(-1));
        let result =
            unsafe { mux_net_http_server_config_set_heartbeat_interval_ms(config, negative) };
        assert!(matches!(
            unsafe { result.as_ref() },
            Some(Value::Result(Err(_)))
        ));
        unsafe {
            crate::refcount::mux_rc_dec(result);
            crate::refcount::mux_rc_dec(negative);
        }

        let oversized = crate::refcount::mux_rc_alloc(Value::Int(300_001));
        let result =
            unsafe { mux_net_http_server_config_set_heartbeat_interval_ms(config, oversized) };
        assert!(matches!(
            unsafe { result.as_ref() },
            Some(Value::Result(Err(_)))
        ));
        unsafe {
            crate::refcount::mux_rc_dec(result);
            crate::refcount::mux_rc_dec(oversized);
        }

        let current = unsafe { mux_net_http_server_config_heartbeat_interval_ms(config) };
        assert!(matches!(
            unsafe { current.as_ref() },
            Some(Value::Int(15_000))
        ));
        unsafe {
            crate::refcount::mux_rc_dec(current);
            crate::refcount::mux_rc_dec(config);
        }
    }

    #[test]
    fn cancelled_server_stops_before_accepting_connections() {
        let address = crate::refcount::mux_rc_alloc(Value::String("127.0.0.1:0".to_string()));
        let listener_result = unsafe { mux_net_tcp_listener_bind(address) };
        let listener_result = take_mux_result(listener_result);
        let Value::Result(Ok(listener)) = listener_result else {
            panic!("listener should bind")
        };
        let listener = crate::refcount::mux_rc_alloc(*listener);
        unsafe { crate::refcount::mux_rc_dec(address) };

        let config = unsafe { mux_net_http_server_config_new() };
        let cancellation = crate::sync_primitives::mux_cancellation_new();
        let cancelled = unsafe { crate::sync_primitives::mux_cancellation_cancel(cancellation) };
        let cancelled = take_mux_result(cancelled);
        assert!(matches!(cancelled, Value::Result(Ok(_))));

        let mut handler = super::HttpServerClosureRepr {
            function_ptr: std::ptr::null_mut(),
            captures_ptr: std::ptr::null_mut(),
            capture_count: 0,
            boxed_function_ptr: std::ptr::dangling_mut::<c_void>(),
        };
        let result = unsafe {
            mux_net_http_server_serve_until_cancelled(
                listener,
                config,
                (&mut handler as *mut super::HttpServerClosureRepr).cast(),
                cancellation,
            )
        };
        let result = take_mux_result(result);
        assert!(matches!(result, Value::Result(Ok(_))));

        unsafe {
            crate::refcount::mux_rc_dec(listener);
            crate::refcount::mux_rc_dec(config);
            crate::refcount::mux_rc_dec(cancellation);
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_socket_path_limit_matches_native_sun_path() {
        let accepted = crate::refcount::mux_rc_alloc(crate::Value::String(
            "a".repeat(super::LOCAL_SOCKET_PATH_MAX),
        ));
        assert!(super::local_path(accepted).is_ok());
        unsafe { crate::refcount::mux_rc_dec(accepted) };

        let rejected = crate::refcount::mux_rc_alloc(crate::Value::String(
            "a".repeat(super::LOCAL_SOCKET_PATH_MAX + 1),
        ));
        assert!(super::local_path(rejected).is_err());
        unsafe { crate::refcount::mux_rc_dec(rejected) };
    }

    #[test]
    fn redirect_policy_is_case_insensitive_and_origin_aware() {
        assert!(is_https_url("https://example.com/path"));
        assert!(is_https_url("HTTPS://example.com/path"));
        assert!(!is_https_url("http://example.com/path"));
        assert!(!is_https_url("https:example.com/path"));
        assert!(!is_https_url("https"));
    }

    #[test]
    fn environment_proxy_is_skipped_for_loopback_urls() {
        for url in [
            "http://localhost/",
            "http://LOCALHOST:8080/",
            "http://127.0.0.1/",
            "http://127.255.255.255/",
            "https://[::1]/",
        ] {
            assert!(!should_use_environment_proxy(url), "proxy used for {url}");
        }
        for url in [
            "http://localhost.example/",
            "http://192.0.2.1/",
            "https://[2001:db8::1]/",
            "not a URL",
        ] {
            assert!(should_use_environment_proxy(url), "proxy skipped for {url}");
        }
    }

    #[test]
    fn total_http_timeout_remaining_never_resets_between_attempts() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        assert_eq!(
            http_timeout_remaining(Some(deadline), started),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            http_timeout_remaining(Some(deadline), started + Duration::from_millis(150)),
            Some(Duration::ZERO)
        );
        assert_eq!(http_timeout_remaining(None, started), None);
    }

    #[test]
    fn typed_response_reads_are_incremental_and_bounded() {
        let response = super::insert_http_response(HttpResponseEntry {
            status: 200,
            headers: Arc::new(Mutex::new(super::HeaderData::default())),
            body: Vec::new(),
            body_reader: Some(Box::new(Cursor::new(b"streamed body".to_vec()))),
            streamed_bytes: 0,
            position: 0,
            names: 1,
        });
        assert!(!response.is_null());
        assert_eq!(response_read(response, 4).expect("first chunk"), b"stre");
        assert_eq!(
            response_read_to_end(response).expect("remaining chunks"),
            b"amed body"
        );
        assert!(response_read(response, 1).expect("EOF").is_empty());
        unsafe { crate::refcount::mux_rc_dec(response) };

        let response = super::insert_http_response(HttpResponseEntry {
            status: 200,
            headers: Arc::new(Mutex::new(super::HeaderData::default())),
            body: Vec::new(),
            body_reader: Some(Box::new(Cursor::new(vec![0_u8; MAX_HTTP_BODY_BYTES + 1]))),
            streamed_bytes: 0,
            position: 0,
            names: 1,
        });
        assert!(!response.is_null());
        let error = response_read_to_end(response).expect_err("oversized body");
        assert!(error.contains("exceeds"));
        unsafe { crate::refcount::mux_rc_dec(response) };
    }

    #[test]
    fn auth_middleware_accepts_only_exact_credentials() {
        let request = insert_http_request(HttpRequestEntry {
            method: "GET".to_string(),
            url: "/private".to_string(),
            request_id: String::new(),
            proxy: None,
            headers: Arc::new(Mutex::new(HeaderData {
                values: vec![(
                    "authorization".to_string(),
                    "Basic YWRtaW46cHc=".to_string(),
                )],
            })),
            body: None,
            body_reader: None,
            path_params: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout_ms: 0,
            timeout_ms: 0,
            max_redirects: 0,
            retries: 0,
            retry_backoff_ms: 0,
            names: 1,
        });
        assert!(basic_auth_matches(request, "admin", "pw"));
        assert!(!basic_auth_matches(request, "admin", "wrong"));
        assert!(!bearer_auth_matches(request, "service-token"));
        assert!(constant_time_equal(b"same", b"same"));
        assert!(!constant_time_equal(b"same", b"same-prefix"));
        unsafe { crate::refcount::mux_rc_dec(request) };
    }

    #[test]
    fn access_log_fields_escape_control_characters() {
        assert_eq!(
            sanitize_http_log_field("/search?q=one\r\ntwo\t\u{7f}"),
            r"/search?q=one\r\ntwo\t\u{7f}"
        );
        assert_eq!(sanitize_http_log_field("/café"), "/café");
    }

    #[test]
    fn header_values_reject_wire_control_characters_but_allow_horizontal_tabs() {
        assert!(super::header_value("one\ttwo").is_ok());
        for value in ["one\0two", "one\u{000b}two", "one\u{007f}two", "one\ntwo"] {
            assert!(super::header_value(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn transfer_encoding_rejects_unsupported_or_ambiguous_codings() {
        let chunked = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert_eq!(request_uses_chunked_transfer_encoding(&chunked), Ok(true));
        assert_eq!(request_uses_chunked_transfer_encoding(&[]), Ok(false));

        for value in [
            "gzip",
            "gzip, chunked",
            "chunked, gzip",
            "chunked, chunked",
            "chunked,",
        ] {
            let headers = vec![("transfer-encoding".to_string(), value.to_string())];
            assert!(
                request_uses_chunked_transfer_encoding(&headers).is_err(),
                "accepted unsupported or ambiguous transfer encoding {value:?}"
            );
        }

        let repeated = vec![
            ("transfer-encoding".to_string(), "chunked".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
        ];
        assert!(request_uses_chunked_transfer_encoding(&repeated).is_err());
    }

    #[test]
    fn chunked_request_trailers_are_validated_as_header_fields() {
        assert!(validate_http_request_trailer(b"Digest: sha-256=abc").is_ok());
        assert!(validate_http_request_trailer(b"Bad Name: value").is_err());
        assert!(validate_http_request_trailer(b"missing-colon").is_err());
        assert!(validate_http_request_trailer(b"Content-Length: 4").is_err());
        assert!(validate_http_request_trailer(b"Transfer-Encoding: chunked").is_err());
        assert!(validate_http_request_trailer(b"X-Trace: value\r\nforged: yes").is_err());
    }

    #[test]
    fn http11_requires_one_nonempty_host_header() {
        let valid = b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n";
        assert!(parse_http_request_header_pairs(valid, 8).is_ok());

        let missing = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n";
        let error = parse_http_request_header_pairs(missing, 8).expect_err("missing Host");
        assert!(error.contains("Host"));

        let duplicate = b"GET / HTTP/1.1\r\nHost: one.test\r\nHost: two.test\r\n\r\n";
        let error = parse_http_request_header_pairs(duplicate, 8).expect_err("duplicate Host");
        assert!(error.contains("exactly one Host"));

        let empty = b"GET / HTTP/1.1\r\nHost: \r\n\r\n";
        let error = parse_http_request_header_pairs(empty, 8).expect_err("empty Host");
        assert!(error.contains("must not be empty"));

        let http10 = b"GET / HTTP/1.0\r\n\r\n";
        assert!(parse_http_request_header_pairs(http10, 8).is_ok());
    }

    #[test]
    fn host_header_accepts_authorities_and_rejects_ambiguous_values() {
        let valid = [
            ("host".to_string(), "example.test:8443".to_string()),
            ("host".to_string(), "[2001:db8::1]".to_string()),
        ];
        assert!(validate_http_request_host("HTTP/1.1", &valid[..1]).is_ok());
        assert!(validate_http_request_host("HTTP/1.1", &valid[1..]).is_ok());

        for value in [
            "example.test attacker.test",
            "user@example.test",
            "example.test/path",
            "example.test:not-a-port",
            "[2001:db8::1",
        ] {
            let headers = vec![("host".to_string(), value.to_string())];
            assert!(
                validate_http_request_host("HTTP/1.1", &headers).is_err(),
                "accepted invalid Host authority {value:?}"
            );
        }
    }

    #[test]
    fn response_headers_match_body_and_status_framing() {
        let headers = vec![("Content-Length".to_string(), "3".to_string())];
        assert!(validated_http_response_headers(&headers, 200, 3).is_ok());
        assert!(validated_http_response_headers(&headers, 200, 2).is_err());

        let no_body = Vec::new();
        let headers_204 = validated_http_response_headers(&no_body, 204, 0).unwrap();
        assert!(headers_204
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));
        assert!(validated_http_response_headers(&no_body, 204, 1).is_err());
        assert!(validated_http_response_headers(&headers, 204, 0).is_err());
        let headers_304 = validated_http_response_headers(&no_body, 304, 0).unwrap();
        assert!(headers_304
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));
        assert!(validated_http_response_headers(&no_body, 304, 1).is_err());
        let headers_100 = validated_http_response_headers(&no_body, 100, 0).unwrap();
        assert!(headers_100
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));
        let headers_205 = validated_http_response_headers(&no_body, 205, 0).unwrap();
        assert!(headers_205
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("content-length") && value == "0"));
        let zero_length = vec![("Content-Length".to_string(), "0".to_string())];
        assert!(validated_http_response_headers(&zero_length, 205, 0).is_ok());
        assert!(validated_http_response_headers(&headers, 205, 0).is_err());
        let transfer_encoded = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(validated_http_response_headers(&transfer_encoded, 200, 0).is_err());

        let keep_alive = vec![("Connection".to_string(), "keep-alive".to_string())];
        assert!(validated_http_response_headers(&keep_alive, 200, 0).is_err());
        let contradictory = vec![("Connection".to_string(), "close, keep-alive".to_string())];
        assert!(validated_http_response_headers(&contradictory, 200, 0).is_err());
        let duplicate_contradictory = vec![
            ("Connection".to_string(), "close".to_string()),
            ("connection".to_string(), "keep-alive".to_string()),
        ];
        assert!(validated_http_response_headers(&duplicate_contradictory, 200, 0).is_err());
        let closes = vec![("Connection".to_string(), "close".to_string())];
        assert!(validated_http_response_headers(&closes, 200, 0).is_ok());
    }

    #[test]
    fn head_response_omits_wire_body_but_reports_representation_length() {
        let mut wire = Vec::new();
        write_typed_http_response_to(&mut wire, 200, &[], b"hello", Some("HEAD"))
            .expect("HEAD response should be writable");
        assert_eq!(
            wire,
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\n"
        );
    }

    #[test]
    fn response_writer_rejects_oversized_buffered_bodies_before_writing() {
        let body = vec![0_u8; MAX_HTTP_BODY_BYTES + 1];
        let mut wire = Vec::new();
        let error = write_typed_http_response_to(&mut wire, 200, &[], &body, None)
            .expect_err("response writer must enforce the buffered body limit");
        assert!(error.contains("buffered body"));
        assert!(wire.is_empty(), "writer must fail before emitting headers");
    }

    #[test]
    fn response_headers_reject_invalid_names_and_conflicting_lengths() {
        let invalid_name = vec![("Bad Name".to_string(), "value".to_string())];
        assert!(validated_http_response_headers(&invalid_name, 200, 5).is_err());
        let conflicting = vec![
            ("content-length".to_string(), "5".to_string()),
            ("Content-Length".to_string(), "6".to_string()),
        ];
        assert!(validated_http_response_headers(&conflicting, 200, 5).is_err());
    }

    #[test]
    fn http_header_budget_rejects_oversized_count_and_bytes() {
        let too_many = (0..=MAX_HTTP_HEADERS_COUNT)
            .map(|index| (format!("x-{index}"), "value".to_string()))
            .collect::<Vec<_>>();
        let error = validate_http_header_budget(&too_many).expect_err("header count limit");
        assert!(error.contains("field count"));

        let too_large = vec![("x-body".to_string(), "a".repeat(MAX_HTTP_HEADER_BYTES))];
        let error = validate_http_header_budget(&too_large).expect_err("header byte limit");
        assert!(error.contains("byte limit"));

        let mut wire = Vec::new();
        let error = write_typed_http_response_to(&mut wire, 200, &too_many, &[], None)
            .expect_err("response writer must reject headers before writing");
        assert!(error.contains("field count"));
        assert!(
            wire.is_empty(),
            "header validation must precede response output"
        );
    }

    #[test]
    fn client_response_header_budget_is_checked_before_body_reader_storage() {
        let mut builder = ureq::http::Response::builder().status(200);
        for index in 0..=MAX_HTTP_HEADERS_COUNT {
            builder = builder.header(format!("x-{index}"), "value");
        }
        let response = builder
            .body(ureq::Body::builder().data(Vec::<u8>::new()))
            .expect("test response should be constructible");
        let Err(error) = stream_http_response_data(response) else {
            panic!("client response header count must be bounded")
        };
        assert!(error.contains("field count"));
    }

    #[test]
    fn outgoing_request_header_budget_is_checked_before_transport_setup() {
        let too_many = (0..=MAX_HTTP_HEADERS_COUNT)
            .map(|index| (format!("x-{index}"), "value".to_string()))
            .collect::<Vec<_>>();
        let error = execute_http_response(HttpRequestExecution {
            method: "GET",
            url: "http://127.0.0.1:1/",
            proxy: None,
            headers: &too_many,
            raw_body: None,
            body_reader: None,
            body_json: None,
            options: default_http_request_options(),
        })
        .expect_err("outgoing request header count must be bounded");
        assert!(error.contains("field count"));
    }

    #[test]
    fn buffered_request_bodies_are_bounded_but_streaming_is_separate() {
        assert!(validate_http_buffered_body(&[0_u8; 4]).is_ok());
        assert!(validate_http_buffered_body(&vec![0_u8; MAX_HTTP_BODY_BYTES + 1]).is_err());
    }

    #[test]
    fn header_limit_excludes_body_bytes_already_in_the_same_read() {
        let packet = b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\nbody";
        let header_end = http_header_end_within_limit(packet, 39)
            .expect("header limit should accept the header")
            .expect("complete header");
        assert_eq!(
            &packet[..header_end],
            b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n"
        );
        assert_eq!(&packet[header_end..], b"body");

        assert!(http_header_end_within_limit(packet, header_end - 1).is_err());
        assert!(http_header_end_within_limit(b"GET / HTTP/1.1\r\n", 64).is_ok());
    }
}
