//! Verified synchronous TLS client and server streams for `std.net.tls`.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::{TypeId, Value};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, LazyLock, Mutex};

enum TlsConnection {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl Read for TlsConnection {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Client(stream) => stream.read(buffer),
            Self::Server(stream) => stream.read(buffer),
        }
    }
}

impl Write for TlsConnection {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Client(stream) => stream.write(buffer),
            Self::Server(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Client(stream) => stream.flush(),
            Self::Server(stream) => stream.flush(),
        }
    }
}
struct Entry {
    stream: Arc<Mutex<TlsConnection>>,
    names: usize,
}
#[derive(Clone)]
struct TlsPolicy {
    min_protocol: String,
    max_protocol: String,
    cipher_suites: Option<Vec<String>>,
    alpn_protocols: Vec<Vec<u8>>,
}

impl Default for TlsPolicy {
    fn default() -> Self {
        Self {
            min_protocol: "TLS1.2".to_string(),
            max_protocol: "TLS1.3".to_string(),
            cipher_suites: None,
            alpn_protocols: Vec::new(),
        }
    }
}

struct ConfigEntry {
    policy: TlsPolicy,
    names: usize,
}
static STREAMS: LazyLock<Mutex<HashMap<i64, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CONFIGS: LazyLock<Mutex<HashMap<i64, ConfigEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "TlsStream",
        size_of::<i64>(),
        Some(drop_stream),
        Some(copy_stream),
    )
});
static CONFIG_TYPE_ID: LazyLock<crate::TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "TlsConfig",
        size_of::<i64>(),
        Some(drop_config),
        Some(copy_config),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn next_id() -> i64 {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}
fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => mux_rc_alloc(Value::Result(Ok(Box::new(value)))),
        Err(error) => crate::std::tls_result_err(error),
    }
}
fn err(message: impl Into<String>) -> *mut Value {
    result(Err(message.into()))
}

fn stream_result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => mux_rc_alloc(Value::Result(Ok(Box::new(value)))),
        Err(error) => crate::std::io_result_err(error),
    }
}

fn stream_err(message: impl Into<String>) -> *mut Value {
    stream_result(Err(message.into()))
}
fn object(id: i64) -> *mut Value {
    let value = alloc_object(*TYPE_ID);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe {
        *ptr.cast::<i64>() = id;
    }
    value
}
fn config_object(id: i64) -> *mut Value {
    let value = alloc_object(*CONFIG_TYPE_ID);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe {
        *ptr.cast::<i64>() = id;
    }
    value
}
fn handle(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *TYPE_ID {
        return Err("expected TlsStream value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("TlsStream value is invalid".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    (id > 0)
        .then_some(id)
        .ok_or_else(|| "TlsStream value is invalid".to_string())
}
fn config_handle(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *CONFIG_TYPE_ID {
        return Err("expected TlsConfig value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("TlsConfig value is invalid".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    (id > 0)
        .then_some(id)
        .ok_or_else(|| "TlsConfig value is invalid".to_string())
}
extern "C" fn copy_stream(source: *mut std::ffi::c_void, dest: *mut std::ffi::c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = lock(&STREAMS).get_mut(&id) {
        entry.names += 1;
        unsafe {
            *dest.cast::<i64>() = id;
        }
    } else {
        unsafe {
            *dest.cast::<i64>() = 0;
        }
    }
}
extern "C" fn drop_stream(ptr: *mut std::ffi::c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut streams = lock(&STREAMS);
    if streams.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    }) {
        streams.remove(&id);
    }
}
extern "C" fn copy_config(source: *mut std::ffi::c_void, dest: *mut std::ffi::c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = lock(&CONFIGS).get_mut(&id) {
        entry.names += 1;
        unsafe {
            *dest.cast::<i64>() = id;
        }
    } else {
        unsafe {
            *dest.cast::<i64>() = 0;
        }
    }
}
extern "C" fn drop_config(ptr: *mut std::ffi::c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut configs = lock(&CONFIGS);
    if configs.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    }) {
        configs.remove(&id);
    }
}

fn config_policy(handle: i64) -> Result<TlsPolicy, String> {
    lock(&CONFIGS)
        .get(&handle)
        .map(|entry| entry.policy.clone())
        .ok_or_else(|| "TlsConfig is closed".to_string())
}

fn protocol_versions(
    policy: &TlsPolicy,
) -> Result<Vec<&'static rustls::SupportedProtocolVersion>, String> {
    let version = |name: &str| match name {
        "TLS1.2" => Some(&rustls::version::TLS12),
        "TLS1.3" => Some(&rustls::version::TLS13),
        _ => None,
    };
    let min = version(&policy.min_protocol).ok_or_else(|| {
        format!(
            "unsupported TLS minimum protocol '{}'; use TLS1.2 or TLS1.3",
            policy.min_protocol
        )
    })?;
    let max = version(&policy.max_protocol).ok_or_else(|| {
        format!(
            "unsupported TLS maximum protocol '{}'; use TLS1.2 or TLS1.3",
            policy.max_protocol
        )
    })?;
    let min_rank = if std::ptr::eq(min, &rustls::version::TLS12) {
        12
    } else {
        13
    };
    let max_rank = if std::ptr::eq(max, &rustls::version::TLS12) {
        12
    } else {
        13
    };
    if min_rank > max_rank {
        return Err("TLS minimum protocol must not exceed maximum protocol".to_string());
    }
    Ok(if min_rank == 12 && max_rank == 13 {
        vec![&rustls::version::TLS12, &rustls::version::TLS13]
    } else if min_rank == 12 {
        vec![&rustls::version::TLS12]
    } else {
        vec![&rustls::version::TLS13]
    })
}

fn configured_provider(policy: &TlsPolicy) -> Result<Arc<rustls::crypto::CryptoProvider>, String> {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    if let Some(names) = &policy.cipher_suites {
        provider.cipher_suites.retain(|suite| {
            let name = format!("{:?}", suite.suite());
            names.iter().any(|requested| requested == &name)
        });
        if provider.cipher_suites.is_empty() {
            return Err("TLS cipher suite policy did not select any supported suites".to_string());
        }
    }
    Ok(Arc::new(provider))
}

fn build_client_config(
    policy: &TlsPolicy,
    roots: RootCertStore,
    client_auth: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
) -> Result<ClientConfig, String> {
    let versions = protocol_versions(policy)?;
    let provider = configured_provider(policy)?;
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions)
        .map_err(|error| format!("TLS protocol policy is invalid: {error}"))?
        .with_root_certificates(roots);
    let mut config = match client_auth {
        Some((certificates, key)) => builder
            .with_client_auth_cert(certificates, key)
            .map_err(|error| format!("TLS client configuration failed: {error}"))?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = policy.alpn_protocols.clone();
    Ok(config)
}

fn build_server_config(
    policy: &TlsPolicy,
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig, String> {
    let versions = protocol_versions(policy)?;
    let provider = configured_provider(policy)?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions)
        .map_err(|error| format!("TLS protocol policy is invalid: {error}"))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| format!("TLS server configuration failed: {error}"))?;
    config.alpn_protocols = policy.alpn_protocols.clone();
    Ok(config)
}

fn store_tls_config() -> Result<Value, String> {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    lock(&CONFIGS).insert(
        id,
        ConfigEntry {
            policy: TlsPolicy::default(),
            names: 1,
        },
    );
    let value = config_object(id);
    if value.is_null() {
        lock(&CONFIGS).remove(&id);
        return Err("could not allocate TlsConfig".to_string());
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        lock(&CONFIGS).remove(&id);
        return Err("could not allocate TlsConfig".to_string());
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    Ok(owned)
}

fn protocol_name(value: *const Value, label: &str) -> Result<String, String> {
    match unsafe { value.as_ref() } {
        Some(Value::String(name)) if matches!(name.as_str(), "TLS1.2" | "TLS1.3") => {
            Ok(name.clone())
        }
        Some(Value::String(_)) => Err(format!("{label} must be TLS1.2 or TLS1.3")),
        _ => Err(format!("{label} must be a string")),
    }
}

fn string_list(value: *const Value, label: &str) -> Result<Vec<String>, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err(format!("{label} must be a list<string>"));
    };
    if values.is_empty() || values.len() > 64 {
        return Err(format!("{label} must contain between 1 and 64 entries"));
    }
    values
        .iter()
        .map(|value| match value {
            Value::String(name) if !name.is_empty() => Ok(name.clone()),
            _ => Err(format!("{label} must contain non-empty strings")),
        })
        .collect()
}

fn bytes_list(value: *const Value, label: &str) -> Result<Vec<Vec<u8>>, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err(format!("{label} must be a list<bytes>"));
    };
    if values.len() > 64 {
        return Err(format!("{label} must contain at most 64 entries"));
    }
    values
        .iter()
        .map(|value| match value {
            Value::Bytes(protocol) if !protocol.is_empty() && protocol.len() <= 255 => {
                Ok(protocol.clone())
            }
            _ => Err(format!(
                "{label} must contain non-empty byte strings up to 255 bytes"
            )),
        })
        .collect()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_config_new() -> *mut Value {
    match store_tls_config() {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_config_set_protocols(
    config: *const Value,
    minimum: *const Value,
    maximum: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    let minimum = match protocol_name(minimum, "TLS minimum protocol") {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let maximum = match protocol_name(maximum, "TLS maximum protocol") {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let min_rank = if minimum == "TLS1.2" { 12 } else { 13 };
    let max_rank = if maximum == "TLS1.2" { 12 } else { 13 };
    if min_rank > max_rank {
        return err("TLS minimum protocol must not exceed maximum protocol");
    }
    let mut configs = lock(&CONFIGS);
    let Some(entry) = configs.get_mut(&handle) else {
        return err("TlsConfig is closed");
    };
    entry.policy.min_protocol = minimum;
    entry.policy.max_protocol = maximum;
    result(Ok(Value::Unit))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_config_set_cipher_suites(
    config: *const Value,
    suites: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    let suites = match string_list(suites, "TLS cipher suites") {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let mut configs = lock(&CONFIGS);
    let Some(entry) = configs.get_mut(&handle) else {
        return err("TlsConfig is closed");
    };
    entry.policy.cipher_suites = Some(suites);
    result(Ok(Value::Unit))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_config_set_alpn_protocols(
    config: *const Value,
    protocols: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    let protocols = match bytes_list(protocols, "TLS ALPN protocols") {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let mut configs = lock(&CONFIGS);
    let Some(entry) = configs.get_mut(&handle) else {
        return err("TlsConfig is closed");
    };
    entry.policy.alpn_protocols = protocols;
    result(Ok(Value::Unit))
}

fn bundled_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

fn roots_from_value(value: *const Value) -> Result<RootCertStore, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err("TLS roots must be a list of DER certificate bytes".to_string());
    };
    if values.is_empty() {
        return Err("TLS roots must not be empty".to_string());
    }
    let mut store = RootCertStore::empty();
    for value in values {
        let Value::Bytes(bytes) = value else {
            return Err("TLS roots must be a non-empty list of DER certificate bytes".to_string());
        };
        if bytes.is_empty() {
            return Err("TLS roots must be a non-empty list of DER certificate bytes".to_string());
        }
        store
            .add(CertificateDer::from(bytes.clone()))
            .map_err(|error| format!("TLS root certificate is invalid: {error}"))?;
    }
    Ok(store)
}

fn configured_client_stream(
    server_name: *const Value,
    address: *const Value,
    policy: TlsPolicy,
    roots: RootCertStore,
) -> Result<Value, String> {
    let server_name = match unsafe { server_name.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return Err("TLS server name must be a non-empty string".to_string()),
    };
    let address = match unsafe { address.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return Err("TLS address must be a non-empty string".to_string()),
    };
    let name = ServerName::try_from(server_name)
        .map_err(|_| "TLS server name is not a valid DNS name".to_string())?;
    let config = build_client_config(&policy, roots, None)?;
    let socket =
        TcpStream::connect(&address).map_err(|error| format!("TLS TCP connect failed: {error}"))?;
    let connection = ClientConnection::new(Arc::new(config), name)
        .map_err(|error| format!("TLS configuration failed: {error}"))?;
    store_tls_stream(TlsConnection::Client(StreamOwned::new(connection, socket)))
}

fn configured_client_cert_stream(
    server_name: *const Value,
    address: *const Value,
    roots: RootCertStore,
    certificates: *const Value,
    private_key: *const Value,
    policy: TlsPolicy,
) -> Result<Value, String> {
    let server_name = match unsafe { server_name.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return Err("TLS server name must be a non-empty string".to_string()),
    };
    let address = match unsafe { address.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return Err("TLS address must be a non-empty string".to_string()),
    };
    let certificates = certificates_from_value(certificates)?;
    let key = match unsafe { private_key.as_ref() } {
        Some(Value::Bytes(bytes)) if !bytes.is_empty() => PrivateKeyDer::try_from(bytes.clone())
            .map_err(|error| format!("TLS client private key is invalid: {error}"))?,
        _ => return Err("TLS client private key must be non-empty DER bytes".to_string()),
    };
    let name = ServerName::try_from(server_name)
        .map_err(|_| "TLS server name is not a valid DNS name".to_string())?;
    let config = build_client_config(&policy, roots, Some((certificates, key)))?;
    let socket =
        TcpStream::connect(&address).map_err(|error| format!("TLS TCP connect failed: {error}"))?;
    let connection = ClientConnection::new(Arc::new(config), name)
        .map_err(|error| format!("TLS configuration failed: {error}"))?;
    store_tls_stream(TlsConnection::Client(StreamOwned::new(connection, socket)))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_connect_with_config(
    server_name: *const Value,
    address: *const Value,
    config: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    match config_policy(handle)
        .and_then(|policy| configured_client_stream(server_name, address, policy, bundled_roots()))
    {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_connect_with_roots_config(
    server_name: *const Value,
    address: *const Value,
    roots: *const Value,
    config: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    let roots = match roots_from_value(roots) {
        Ok(roots) => roots,
        Err(error) => return err(error),
    };
    match config_policy(handle)
        .and_then(|policy| configured_client_stream(server_name, address, policy, roots))
    {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_connect_with_client_cert_config(
    server_name: *const Value,
    address: *const Value,
    roots: *const Value,
    certificates: *const Value,
    private_key: *const Value,
    config: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    let roots = match roots_from_value(roots) {
        Ok(roots) => roots,
        Err(error) => return err(error),
    };
    match config_policy(handle).and_then(|policy| {
        configured_client_cert_stream(
            server_name,
            address,
            roots,
            certificates,
            private_key,
            policy,
        )
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

fn store_tls_stream(connection: TlsConnection) -> Result<Value, String> {
    let id = next_id();
    lock(&STREAMS).insert(
        id,
        Entry {
            stream: Arc::new(Mutex::new(connection)),
            names: 1,
        },
    );
    let value = object(id);
    if value.is_null() {
        lock(&STREAMS).remove(&id);
        return Err("could not allocate TlsStream".to_string());
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return Err("could not allocate TlsStream".to_string());
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    Ok(owned)
}

fn stream_mutex(id: i64) -> Result<Arc<Mutex<TlsConnection>>, String> {
    lock(&STREAMS)
        .get(&id)
        .map(|entry| Arc::clone(&entry.stream))
        .ok_or_else(|| "TlsStream is closed".to_string())
}

fn with_stream<R>(
    id: i64,
    f: impl FnOnce(&mut TlsConnection) -> Result<R, String>,
) -> Result<R, String> {
    let stream_mutex = stream_mutex(id)?;
    let mut stream = lock(&stream_mutex);
    f(&mut stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_connect(
    server_name: *const Value,
    address: *const Value,
) -> *mut Value {
    let server_name = match unsafe { server_name.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return err("TLS server name must be a non-empty string"),
    };
    let address = match unsafe { address.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return err("TLS address must be a non-empty string"),
    };
    let socket = match TcpStream::connect(&address) {
        Ok(socket) => socket,
        Err(error) => return err(format!("TLS TCP connect failed: {error}")),
    };
    let Ok(name) = ServerName::try_from(server_name.clone()) else {
        return err("TLS server name is not a valid DNS name");
    };
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = match build_client_config(&TlsPolicy::default(), roots, None) {
        Ok(config) => config,
        Err(error) => return err(error),
    };
    let connection = match ClientConnection::new(Arc::new(config), name) {
        Ok(connection) => connection,
        Err(error) => return err(format!("TLS configuration failed: {error}")),
    };
    match store_tls_stream(TlsConnection::Client(StreamOwned::new(connection, socket))) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Connects with caller-provided DER trust anchors, without consulting the
/// bundled public trust store. Each list element contains one DER certificate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_connect_with_roots(
    server_name: *const Value,
    address: *const Value,
    roots_value: *const Value,
) -> *mut Value {
    let server_name = match unsafe { server_name.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return err("TLS server name must be a non-empty string"),
    };
    let address = match unsafe { address.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return err("TLS address must be a non-empty string"),
    };
    let roots = match unsafe { roots_value.as_ref() } {
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::Bytes(bytes) if !bytes.is_empty() => Ok(CertificateDer::from(bytes.clone())),
                _ => Err("TLS roots must be a non-empty list of DER certificate bytes".to_string()),
            })
            .collect::<Result<Vec<_>, _>>(),
        _ => Err("TLS roots must be a list of DER certificate bytes".to_string()),
    };
    let roots = match roots {
        Ok(roots) if !roots.is_empty() => roots,
        Ok(_) => return err("TLS roots must not be empty"),
        Err(error) => return err(error),
    };
    let socket = match TcpStream::connect(&address) {
        Ok(socket) => socket,
        Err(error) => return err(format!("TLS TCP connect failed: {error}")),
    };
    let Ok(name) = ServerName::try_from(server_name) else {
        return err("TLS server name is not a valid DNS name");
    };
    let mut store = RootCertStore::empty();
    for certificate in roots {
        if let Err(error) = store.add(certificate) {
            return err(format!("TLS root certificate is invalid: {error}"));
        }
    }
    let config = match build_client_config(&TlsPolicy::default(), store, None) {
        Ok(config) => config,
        Err(error) => return err(error),
    };
    let connection = match ClientConnection::new(Arc::new(config), name) {
        Ok(connection) => connection,
        Err(error) => return err(format!("TLS configuration failed: {error}")),
    };
    match store_tls_stream(TlsConnection::Client(StreamOwned::new(connection, socket))) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Connect with a caller-supplied trust store and client certificate chain.
/// Certificates and key are DER; the key may be any encoding supported by the
/// active rustls crypto provider (PKCS#8 is the portable choice).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_connect_with_client_cert(
    server_name: *const Value,
    address: *const Value,
    roots_value: *const Value,
    certificates_value: *const Value,
    private_key: *const Value,
) -> *mut Value {
    let server_name = match unsafe { server_name.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return err("TLS server name must be a non-empty string"),
    };
    let address = match unsafe { address.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return err("TLS address must be a non-empty string"),
    };
    let roots = match unsafe { roots_value.as_ref() } {
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::Bytes(bytes) if !bytes.is_empty() => Ok(CertificateDer::from(bytes.clone())),
                _ => Err("TLS roots must be a non-empty list of DER certificate bytes".to_string()),
            })
            .collect::<Result<Vec<_>, _>>(),
        _ => Err("TLS roots must be a list of DER certificate bytes".to_string()),
    };
    let roots = match roots {
        Ok(roots) if !roots.is_empty() => roots,
        Ok(_) => return err("TLS roots must not be empty"),
        Err(error) => return err(error),
    };
    let certificates = match unsafe { certificates_value.as_ref() } {
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::Bytes(bytes) if !bytes.is_empty() => Ok(CertificateDer::from(bytes.clone())),
                _ => Err(
                    "TLS client certificates must be a non-empty list of DER certificate bytes"
                        .to_string(),
                ),
            })
            .collect::<Result<Vec<_>, _>>(),
        _ => Err("TLS client certificates must be a list of DER certificate bytes".to_string()),
    };
    let certificates = match certificates {
        Ok(certificates) if !certificates.is_empty() => certificates,
        Ok(_) => return err("TLS client certificates must not be empty"),
        Err(error) => return err(error),
    };
    let private_key = match unsafe { private_key.as_ref() } {
        Some(Value::Bytes(bytes)) if !bytes.is_empty() => bytes.clone(),
        _ => return err("TLS client private key must be non-empty DER bytes"),
    };
    let socket = match TcpStream::connect(&address) {
        Ok(socket) => socket,
        Err(error) => return err(format!("TLS TCP connect failed: {error}")),
    };
    let Ok(name) = ServerName::try_from(server_name) else {
        return err("TLS server name is not a valid DNS name");
    };
    let mut store = RootCertStore::empty();
    for certificate in roots {
        if let Err(error) = store.add(certificate) {
            return err(format!("TLS root certificate is invalid: {error}"));
        }
    }
    let key = PrivateKeyDer::try_from(private_key)
        .map_err(|error| format!("TLS client private key is invalid: {error}"));
    let key = match key {
        Ok(key) => key,
        Err(error) => return err(error),
    };
    let config = match build_client_config(&TlsPolicy::default(), store, Some((certificates, key)))
    {
        Ok(config) => config,
        Err(error) => return err(error),
    };
    let connection = match ClientConnection::new(Arc::new(config), name) {
        Ok(connection) => connection,
        Err(error) => return err(format!("TLS configuration failed: {error}")),
    };
    match store_tls_stream(TlsConnection::Client(StreamOwned::new(connection, socket))) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Accept a TCP stream as a verified TLS server using DER certificates and a
/// PKCS#8 private key. The first certificate is the leaf; remaining
/// certificates form the chain sent to clients.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_accept(
    stream: *const Value,
    certificates: *const Value,
    private_key: *const Value,
) -> *mut Value {
    let certificates = match unsafe { certificates.as_ref() } {
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::Bytes(bytes) if !bytes.is_empty() => Ok(CertificateDer::from(bytes.clone())),
                _ => Err(
                    "TLS certificates must be a non-empty list of DER certificate bytes"
                        .to_string(),
                ),
            })
            .collect::<Result<Vec<_>, _>>(),
        _ => Err("TLS certificates must be a list of DER certificate bytes".to_string()),
    };
    let certificates = match certificates {
        Ok(values) if !values.is_empty() => values,
        Ok(_) => return err("TLS certificates must not be empty"),
        Err(error) => return err(error),
    };
    let key = match unsafe { private_key.as_ref() } {
        Some(Value::Bytes(bytes)) if !bytes.is_empty() => bytes.clone(),
        _ => return err("TLS private key must be non-empty PKCS#8 DER bytes"),
    };
    let socket = match crate::net::clone_tcp_stream(stream) {
        Ok(socket) => socket,
        Err(error) => return err(error),
    };
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key));
    let config = match build_server_config(&TlsPolicy::default(), certificates, key) {
        Ok(config) => config,
        Err(error) => return err(error),
    };
    let connection = match ServerConnection::new(Arc::new(config)) {
        Ok(connection) => connection,
        Err(error) => return err(format!("TLS server handshake setup failed: {error}")),
    };
    match store_tls_stream(TlsConnection::Server(StreamOwned::new(connection, socket))) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

fn certificates_from_value(value: *const Value) -> Result<Vec<CertificateDer<'static>>, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err("TLS certificates must be a list of DER certificate bytes".to_string());
    };
    if values.is_empty() {
        return Err("TLS certificates must not be empty".to_string());
    }
    values
        .iter()
        .map(|value| match value {
            Value::Bytes(bytes) if !bytes.is_empty() => Ok(CertificateDer::from(bytes.clone())),
            _ => Err(
                "TLS certificates must be a non-empty list of DER certificate bytes".to_string(),
            ),
        })
        .collect()
}

fn configured_server_stream(
    stream: *const Value,
    certificates: *const Value,
    private_key: *const Value,
    policy: TlsPolicy,
) -> Result<Value, String> {
    let certificates = certificates_from_value(certificates)?;
    let key = match unsafe { private_key.as_ref() } {
        Some(Value::Bytes(bytes)) if !bytes.is_empty() => {
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes.clone()))
        }
        _ => return Err("TLS private key must be non-empty PKCS#8 DER bytes".to_string()),
    };
    let socket = crate::net::clone_tcp_stream(stream)?;
    let config = build_server_config(&policy, certificates, key)?;
    let connection = ServerConnection::new(Arc::new(config))
        .map_err(|error| format!("TLS server handshake setup failed: {error}"))?;
    store_tls_stream(TlsConnection::Server(StreamOwned::new(connection, socket)))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_tls_accept_with_config(
    stream: *const Value,
    certificates: *const Value,
    private_key: *const Value,
    config: *const Value,
) -> *mut Value {
    let handle = match config_handle(config) {
        Ok(handle) => handle,
        Err(error) => return err(error),
    };
    match config_policy(handle)
        .and_then(|policy| configured_server_stream(stream, certificates, private_key, policy))
    {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_read(stream: *const Value, size: i64) -> *mut Value {
    if !(0..=16 * 1024 * 1024).contains(&size) {
        return stream_err("TLS read size must be between 0 and 16 MiB");
    }
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return stream_err(error),
    };
    match with_stream(id, |stream| {
        let mut bytes = vec![0_u8; size as usize];
        match stream.read(&mut bytes) {
            Ok(count) => {
                bytes.truncate(count);
                Ok(Value::Bytes(bytes))
            }
            Err(error) => Err(format!("TLS read failed: {error}")),
        }
    }) {
        Ok(value) => stream_result(Ok(value)),
        Err(error) => stream_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_write(stream: *const Value, bytes: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return stream_err(error),
    };
    let Some(Value::Bytes(bytes)) = (unsafe { bytes.as_ref() }) else {
        return stream_err("TLS write input must be bytes");
    };
    match with_stream(id, |stream| match stream.write(bytes) {
        Ok(count) => Ok(Value::Int(count as i64)),
        Err(error) => Err(format!("TLS write failed: {error}")),
    }) {
        Ok(value) => stream_result(Ok(value)),
        Err(error) => stream_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_flush(stream: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match with_stream(id, |stream| {
        stream
            .flush()
            .map_err(|error| format!("TLS flush failed: {error}"))
            .map(|()| Value::Unit)
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_shutdown(stream: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match with_stream(id, |stream| {
        let shutdown_result = match stream {
            TlsConnection::Client(client) => client.get_ref().shutdown(std::net::Shutdown::Both),
            TlsConnection::Server(server) => server.get_ref().shutdown(std::net::Shutdown::Both),
        };
        shutdown_result
            .map_err(|error| format!("TLS shutdown failed: {error}"))
            .map(|()| Value::Unit)
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Returns the peer certificate chain negotiated by this stream.  The TLS
/// handshake is driven lazily by reads and writes, so inspection before the
/// handshake completes reports an explicit error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_peer_certificates(stream: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match with_stream(id, |stream| {
        let certificates = match stream {
            TlsConnection::Client(client) => client.conn.peer_certificates(),
            TlsConnection::Server(server) => server.conn.peer_certificates(),
        };
        let Some(certificates) = certificates else {
            return Err("TLS peer certificates are not available before handshake".to_string());
        };
        Ok(Value::List(
            certificates
                .iter()
                .map(|certificate| Value::Bytes(certificate.as_ref().to_vec()))
                .collect(),
        ))
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Returns the negotiated TLS protocol version (for example, `TLSv1_3`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_protocol_version(stream: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match with_stream(id, |stream| {
        let version = match stream {
            TlsConnection::Client(client) => client.conn.protocol_version(),
            TlsConnection::Server(server) => server.conn.protocol_version(),
        };
        let Some(version) = version else {
            return Err("TLS protocol version is not available before handshake".to_string());
        };
        Ok(Value::String(format!("{version:?}")))
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Returns the negotiated cipher suite name (for example,
/// `TLS13_AES_128_GCM_SHA256`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_cipher_suite(stream: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match with_stream(id, |stream| {
        let suite = match stream {
            TlsConnection::Client(client) => client.conn.negotiated_cipher_suite(),
            TlsConnection::Server(server) => server.conn.negotiated_cipher_suite(),
        };
        let Some(suite) = suite else {
            return Err("TLS cipher suite is not available before handshake".to_string());
        };
        Ok(Value::String(format!("{:?}", suite.suite())))
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

/// Returns the negotiated ALPN protocol as bytes, or an error when no ALPN
/// protocol was selected (or the handshake has not completed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_tls_alpn_protocol(stream: *const Value) -> *mut Value {
    let id = match handle(stream) {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match with_stream(id, |stream| {
        let protocol = match stream {
            TlsConnection::Client(client) => client.conn.alpn_protocol(),
            TlsConnection::Server(server) => server.conn.alpn_protocol(),
        };
        let Some(protocol) = protocol else {
            return Err("TLS ALPN protocol is not available".to_string());
        };
        Ok(Value::Bytes(protocol.to_vec()))
    }) {
        Ok(value) => result(Ok(value)),
        Err(error) => err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn tls_entry_lock_does_not_block_registry_progress_for_a_copied_handle() -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let socket = TcpStream::connect(address).map_err(|error| error.to_string())?;
        let (_peer, _) = listener.accept().map_err(|error| error.to_string())?;
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
                .map_err(|error| format!("client protocol setup failed: {error}"))?
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth();
        let name = ServerName::try_from("localhost".to_string())
            .map_err(|error| format!("server name setup failed: {error}"))?;
        let connection = ClientConnection::new(Arc::new(config), name)
            .map_err(|error| format!("client setup failed: {error}"))?;
        let id = next_id();
        lock(&STREAMS).insert(
            id,
            Entry {
                stream: Arc::new(Mutex::new(TlsConnection::Client(StreamOwned::new(
                    connection, socket,
                )))),
                names: 1,
            },
        );
        let mut source = id;
        let mut copied = 0;
        copy_stream(
            (&mut source as *mut i64).cast(),
            (&mut copied as *mut i64).cast(),
        );
        if copied != id {
            lock(&STREAMS).remove(&id);
            return Err("copying the TLS handle did not retain the stream".to_string());
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let operation = thread::spawn(move || {
            with_stream(copied, |_stream| {
                entered_tx
                    .send(())
                    .map_err(|error| format!("TLS test synchronization failed: {error}"))?;
                thread::sleep(Duration::from_millis(250));
                Ok(())
            })
        });
        entered_rx
            .recv_timeout(Duration::from_millis(100))
            .map_err(|error| format!("TLS entry lock was not acquired: {error}"))?;

        let (progress_tx, progress_rx) = mpsc::channel();
        let registry_probe = thread::spawn(move || {
            let _streams = lock(&STREAMS);
            progress_tx
                .send(())
                .map_err(|error| format!("TLS registry synchronization failed: {error}"))
        });
        let progressed = progress_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        operation
            .join()
            .map_err(|_| "TLS operation thread panicked".to_string())??;
        registry_probe
            .join()
            .map_err(|_| "TLS registry probe panicked".to_string())??;
        lock(&STREAMS).remove(&id);
        if !progressed {
            return Err("TLS registry remained locked during entry work".to_string());
        }
        Ok(())
    }
}
