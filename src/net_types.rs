//! Typed IP, socket-address, and CIDR values for `std.net`.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::std::StdErrorKind;
use crate::{TypeId, Value};
use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Debug)]
enum NetValue {
    Ip(IpAddr),
    Socket(SocketAddr),
    Cidr { network: IpAddr, prefix: u8 },
    Endpoint { host: String, port: u16 },
}

struct Entry {
    value: NetValue,
    names: usize,
}
static VALUES: LazyLock<Mutex<HashMap<i64, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static IP_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy("IpAddr", 8, Some(drop_value), Some(copy_value))
});
static SOCKET_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy("SocketAddr", 8, Some(drop_value), Some(copy_value))
});
static CIDR_TYPE_ID: LazyLock<TypeId> =
    LazyLock::new(|| register_object_type_with_copy("Cidr", 8, Some(drop_value), Some(copy_value)));
static ENDPOINT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy("Endpoint", 8, Some(drop_value), Some(copy_value))
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => crate::std::net_result_ok(value),
        Err(error) => crate::std::net_result_err(error),
    }
}
fn err(message: impl Into<String>) -> *mut Value {
    result(Err(message.into()))
}

fn err_kind(kind: StdErrorKind, message: impl Into<String>) -> *mut Value {
    crate::std::net_result_err_kind(kind, message.into())
}

extern "C" fn copy_value(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let mut values = lock(&VALUES);
    if let Some(entry) = values.get_mut(&id) {
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

extern "C" fn drop_value(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut values = lock(&VALUES);
    let remove = values.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        values.remove(&id);
    }
}

fn object(type_id: TypeId, value: NetValue) -> *mut Value {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    lock(&VALUES).insert(id, Entry { value, names: 1 });
    let ptr = alloc_object(type_id);
    if ptr.is_null() {
        lock(&VALUES).remove(&id);
        return std::ptr::null_mut();
    }
    let data = unsafe { get_object_ptr(ptr) };
    if data.is_null() {
        unsafe {
            mux_rc_dec(ptr);
        }
        lock(&VALUES).remove(&id);
        return std::ptr::null_mut();
    }
    unsafe {
        *data.cast::<i64>() = id;
    }
    ptr
}

fn object_result(ptr: *mut Value, name: &str) -> *mut Value {
    if ptr.is_null() {
        return err(format!("could not allocate {name}"));
    }
    let value = if let Value::Object(reference) = unsafe { &*ptr } {
        Value::Object(reference.clone())
    } else {
        unsafe {
            mux_rc_dec(ptr);
        }
        return err(format!("could not allocate {name}"));
    };
    unsafe {
        mux_rc_dec(ptr);
    }
    result(Ok(value))
}

unsafe fn id(value: *const Value, type_id: TypeId, name: &str) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return Err(format!("expected {name} value"));
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err(format!("{name} value is invalid"));
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if lock(&VALUES).contains_key(&id) {
        Ok(id)
    } else {
        Err(format!("{name} value is closed"))
    }
}

fn parse_cidr(value: &str) -> Result<(IpAddr, u8), String> {
    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| "CIDR must contain '/'".to_string())?;
    let ip: IpAddr = address
        .parse()
        .map_err(|_| "invalid IP address".to_string())?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| "CIDR prefix must be an integer".to_string())?;
    let bits = if ip.is_ipv4() { 32 } else { 128 };
    if prefix > bits {
        return Err(format!("CIDR prefix must be between 0 and {bits}"));
    }
    let network = match ip {
        IpAddr::V4(ip) => {
            let raw = u32::from(ip);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            IpAddr::V4(Ipv4Addr::from(raw & mask))
        }
        IpAddr::V6(ip) => {
            let raw = u128::from(ip);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            IpAddr::V6(Ipv6Addr::from(raw & mask))
        }
    };
    Ok((network, prefix))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_ip_parse(value: *const Value) -> *mut Value {
    let Some(Value::String(value)) = value.as_ref() else {
        return err_kind(StdErrorKind::Invalid, "IP address must be a string");
    };
    match value.parse::<IpAddr>() {
        Ok(ip) => object_result(object(*IP_TYPE_ID, NetValue::Ip(ip)), "IpAddr"),
        Err(_) => err_kind(StdErrorKind::Invalid, "invalid IP address"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_socket_addr_parse(value: *const Value) -> *mut Value {
    let Some(Value::String(value)) = value.as_ref() else {
        return err("socket address must be a string");
    };
    match value.parse::<SocketAddr>() {
        Ok(address) => object_result(
            object(*SOCKET_TYPE_ID, NetValue::Socket(address)),
            "SocketAddr",
        ),
        Err(_) => err("invalid socket address"),
    }
}

/// Resolve a host and port into normalized socket addresses. Results are
/// sorted by their textual form so callers receive deterministic ordering
/// across resolver implementations where possible.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_socket_addr_resolve(host: *const Value, port: i64) -> *mut Value {
    let Some(Value::String(host)) = host.as_ref() else {
        return err("socket host must be a string");
    };
    if host.is_empty() {
        return err("socket host must not be empty");
    }
    let Ok(port) = u16::try_from(port) else {
        return err("socket port must be between 0 and 65535");
    };
    let addresses = match (host.as_str(), port).to_socket_addrs() {
        Ok(addresses) => addresses.collect::<Vec<_>>(),
        Err(error) => return err(format!("socket address resolution failed: {error}")),
    };
    let mut addresses = addresses;
    addresses.sort_unstable_by_key(ToString::to_string);
    let mut values = Vec::with_capacity(addresses.len());
    for address in addresses {
        let pointer = object(*SOCKET_TYPE_ID, NetValue::Socket(address));
        if pointer.is_null() {
            return err("could not allocate SocketAddr");
        }
        let Value::Object(reference) = (unsafe { &*pointer }) else {
            unsafe { mux_rc_dec(pointer) };
            return err("could not allocate SocketAddr");
        };
        values.push(Value::Object(reference.clone()));
        unsafe { mux_rc_dec(pointer) };
    }
    result(Ok(Value::List(values)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_cidr_parse(value: *const Value) -> *mut Value {
    let Some(Value::String(value)) = value.as_ref() else {
        return err("CIDR must be a string");
    };
    match parse_cidr(value) {
        Ok((network, prefix)) => object_result(
            object(*CIDR_TYPE_ID, NetValue::Cidr { network, prefix }),
            "Cidr",
        ),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_endpoint_from_host(host: *const Value, port: i64) -> *mut Value {
    let Some(Value::String(host)) = host.as_ref() else {
        return err("endpoint host must be a string");
    };
    if host.is_empty() {
        return err("endpoint host must not be empty");
    }
    let Ok(port) = u16::try_from(port) else {
        return err("endpoint port must be between 0 and 65535");
    };
    object_result(
        object(
            *ENDPOINT_TYPE_ID,
            NetValue::Endpoint {
                host: host.clone(),
                port,
            },
        ),
        "Endpoint",
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_endpoint_from_socket_addr(value: *const Value) -> *mut Value {
    match net_entry(value, *SOCKET_TYPE_ID, "SocketAddr") {
        Ok(NetValue::Socket(address)) => object_result(
            object(
                *ENDPOINT_TYPE_ID,
                NetValue::Endpoint {
                    host: address.ip().to_string(),
                    port: address.port(),
                },
            ),
            "Endpoint",
        ),
        _ => err("invalid SocketAddr"),
    }
}

fn net_entry(value: *const Value, type_id: TypeId, name: &str) -> Result<NetValue, String> {
    let id = unsafe { id(value, type_id, name) }?;
    lock(&VALUES)
        .get(&id)
        .map(|entry| entry.value.clone())
        .ok_or_else(|| format!("{name} value is closed"))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_ip_to_string(value: *const Value) -> *mut Value {
    match net_entry(value, *IP_TYPE_ID, "IpAddr") {
        Ok(NetValue::Ip(ip)) => mux_rc_alloc(Value::String(ip.to_string())),
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_ip_is_v4(value: *const Value) -> bool {
    matches!(
        net_entry(value, *IP_TYPE_ID, "IpAddr"),
        Ok(NetValue::Ip(IpAddr::V4(_)))
    )
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_ip_is_v6(value: *const Value) -> bool {
    matches!(
        net_entry(value, *IP_TYPE_ID, "IpAddr"),
        Ok(NetValue::Ip(IpAddr::V6(_)))
    )
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_ip_octets(value: *const Value) -> *mut Value {
    let bytes = match net_entry(value, *IP_TYPE_ID, "IpAddr") {
        Ok(NetValue::Ip(ip)) => match ip {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        },
        _ => Vec::new(),
    };
    mux_rc_alloc(Value::Bytes(bytes))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_socket_to_string(value: *const Value) -> *mut Value {
    match net_entry(value, *SOCKET_TYPE_ID, "SocketAddr") {
        Ok(NetValue::Socket(address)) => mux_rc_alloc(Value::String(address.to_string())),
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_socket_port(value: *const Value) -> i64 {
    match net_entry(value, *SOCKET_TYPE_ID, "SocketAddr") {
        Ok(NetValue::Socket(address)) => i64::from(address.port()),
        _ => -1,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_socket_ip(value: *const Value) -> *mut Value {
    match net_entry(value, *SOCKET_TYPE_ID, "SocketAddr") {
        Ok(NetValue::Socket(address)) => {
            let ptr = object(*IP_TYPE_ID, NetValue::Ip(address.ip()));
            if ptr.is_null() {
                mux_rc_alloc(Value::Unit)
            } else {
                ptr
            }
        }
        _ => mux_rc_alloc(Value::Unit),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_socket_with_port(value: *const Value, port: i64) -> *mut Value {
    if !(0..=65535).contains(&port) {
        return err("socket port must be between 0 and 65535");
    }
    match net_entry(value, *SOCKET_TYPE_ID, "SocketAddr") {
        Ok(NetValue::Socket(address)) => object_result(
            object(
                *SOCKET_TYPE_ID,
                NetValue::Socket(SocketAddr::new(address.ip(), port as u16)),
            ),
            "SocketAddr",
        ),
        _ => err("invalid SocketAddr"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_cidr_to_string(value: *const Value) -> *mut Value {
    match net_entry(value, *CIDR_TYPE_ID, "Cidr") {
        Ok(NetValue::Cidr { network, prefix }) => {
            mux_rc_alloc(Value::String(format!("{network}/{prefix}")))
        }
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_cidr_prefix(value: *const Value) -> i64 {
    match net_entry(value, *CIDR_TYPE_ID, "Cidr") {
        Ok(NetValue::Cidr { prefix, .. }) => i64::from(prefix),
        _ => -1,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_cidr_network(value: *const Value) -> *mut Value {
    match net_entry(value, *CIDR_TYPE_ID, "Cidr") {
        Ok(NetValue::Cidr { network, .. }) => object(*IP_TYPE_ID, NetValue::Ip(network)),
        _ => mux_rc_alloc(Value::Unit),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_cidr_contains(cidr: *const Value, ip: *const Value) -> bool {
    let Ok(NetValue::Cidr { network, prefix }) = net_entry(cidr, *CIDR_TYPE_ID, "Cidr") else {
        return false;
    };
    let Ok(NetValue::Ip(candidate)) = net_entry(ip, *IP_TYPE_ID, "IpAddr") else {
        return false;
    };
    match (network, candidate) {
        (IpAddr::V4(network), IpAddr::V4(candidate)) => {
            let n = u32::from(network);
            let c = u32::from(candidate);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            (n & mask) == (c & mask)
        }
        (IpAddr::V6(network), IpAddr::V6(candidate)) => {
            let n = u128::from(network);
            let c = u128::from(candidate);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            (n & mask) == (c & mask)
        }
        _ => false,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_endpoint_to_string(value: *const Value) -> *mut Value {
    match net_entry(value, *ENDPOINT_TYPE_ID, "Endpoint") {
        Ok(NetValue::Endpoint { host, port }) => {
            let rendered = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            mux_rc_alloc(Value::String(rendered))
        }
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_endpoint_host(value: *const Value) -> *mut Value {
    match net_entry(value, *ENDPOINT_TYPE_ID, "Endpoint") {
        Ok(NetValue::Endpoint { host, .. }) => mux_rc_alloc(Value::String(host)),
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_endpoint_port(value: *const Value) -> i64 {
    match net_entry(value, *ENDPOINT_TYPE_ID, "Endpoint") {
        Ok(NetValue::Endpoint { port, .. }) => i64::from(port),
        _ => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_endpoint_resolve(value: *const Value) -> *mut Value {
    let Ok(NetValue::Endpoint { host, port }) = net_entry(value, *ENDPOINT_TYPE_ID, "Endpoint")
    else {
        return err("invalid Endpoint");
    };
    let addresses = match (host.as_str(), port).to_socket_addrs() {
        Ok(addresses) => addresses.collect::<Vec<_>>(),
        Err(error) => return err(format!("endpoint resolution failed: {error}")),
    };
    let mut addresses = addresses;
    addresses.sort_unstable_by_key(ToString::to_string);
    let mut values = Vec::with_capacity(addresses.len());
    for address in addresses {
        let pointer = object(*SOCKET_TYPE_ID, NetValue::Socket(address));
        if pointer.is_null() {
            return err("could not allocate SocketAddr");
        }
        let Value::Object(reference) = (unsafe { &*pointer }) else {
            unsafe { mux_rc_dec(pointer) };
            return err("could not allocate SocketAddr");
        };
        values.push(Value::Object(reference.clone()));
        unsafe { mux_rc_dec(pointer) };
    }
    result(Ok(Value::List(values)))
}
