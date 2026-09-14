//! Opaque RFC 9562 UUID values.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_alloc;
use crate::std::StdErrorKind;
use crate::{Tuple, TypeId, Value};
use md5::{Digest as Md5Digest, Md5};
use rust_std::collections::HashMap;
use rust_std::ffi::{c_char, c_void, CStr};
use rust_std::sync::atomic::{AtomicU16, Ordering};
use rust_std::sync::{LazyLock, Mutex, OnceLock};
use rust_std::time::{SystemTime, UNIX_EPOCH};
use sha1::{Digest as Sha1Digest, Sha1};

type RustUuid = ::uuid::Uuid;

struct Entry {
    value: RustUuid,
    refs: usize,
}

static UUIDS: LazyLock<Mutex<HashMap<i64, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static PROCESS_NODE: OnceLock<[u8; 6]> = OnceLock::new();
static CLOCK_SEQUENCE: AtomicU16 = AtomicU16::new(0);

static UUID_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Uuid",
        size_of::<i64>(),
        Some(drop_uuid as extern "C" fn(*mut c_void)),
        Some(copy_uuid as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> rust_std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(rust_std::sync::PoisonError::into_inner)
}

fn error(message: impl Into<String>) -> *mut Value {
    crate::std::uuid_result_err(message.into())
}

fn error_kind(kind: StdErrorKind, message: impl Into<String>) -> *mut Value {
    crate::std::uuid_result_err_kind(kind, message.into())
}

fn ok(value: Value) -> *mut Value {
    crate::std::uuid_result_ok(value)
}

fn next_handle() -> i64 {
    let mut next = lock(&NEXT_HANDLE);
    let handle = *next;
    *next = next.checked_add(1).unwrap_or(1);
    handle
}

fn object(handle: i64) -> *mut Value {
    let value = alloc_object(*UUID_TYPE_ID);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { crate::refcount::mux_rc_dec(value) };
        return rust_std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = handle };
    value
}

fn direct_uuid(uuid: RustUuid) -> *mut Value {
    let handle = next_handle();
    lock(&UUIDS).insert(
        handle,
        Entry {
            value: uuid,
            refs: 1,
        },
    );
    let value = object(handle);
    if value.is_null() {
        lock(&UUIDS).remove(&handle);
    }
    value
}

fn result_uuid(uuid: RustUuid) -> *mut Value {
    let value = direct_uuid(uuid);
    if value.is_null() {
        return error("failed to allocate Uuid value");
    }
    let cloned = unsafe { (&*value).clone() };
    unsafe { crate::refcount::mux_rc_dec(value) };
    ok(cloned)
}

unsafe fn handle(value: *const Value) -> Result<i64, String> {
    if value.is_null() {
        return Err("Uuid value is null".to_string());
    }
    if unsafe { get_object_type_id(value) } != *UUID_TYPE_ID {
        return Err("invalid Uuid value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("invalid Uuid value".to_string());
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle <= 0 || !lock(&UUIDS).contains_key(&handle) {
        return Err("invalid Uuid value".to_string());
    }
    Ok(handle)
}

fn uuid_by_handle(handle: i64) -> Result<RustUuid, String> {
    lock(&UUIDS)
        .get(&handle)
        .map(|entry| entry.value)
        .ok_or_else(|| "invalid Uuid value".to_string())
}

fn uuid_from_value(value: *const Value) -> Result<RustUuid, String> {
    let handle = unsafe { handle(value) }?;
    uuid_by_handle(handle)
}

/// Convert a UUID to its canonical textual representation for portable SQL
/// parameters. SQL drivers all accept this representation as text.
pub(crate) unsafe fn sql_uuid_string(value: *const Value) -> Result<String, String> {
    uuid_from_value(value).map(|uuid| uuid.to_string())
}

/// Parse a canonical UUID SQL value into a native Uuid value.
pub(crate) fn sql_uuid_value(text: &str) -> Result<Value, String> {
    let uuid = RustUuid::parse_str(text).map_err(|error| error.to_string())?;
    let ptr = direct_uuid(uuid);
    if ptr.is_null() {
        return Err("could not allocate Uuid value".to_string());
    }
    let value = unsafe { (&*ptr).clone() };
    unsafe { crate::refcount::mux_rc_dec(ptr) };
    Ok(value)
}

/// Module-level parser used by `uuid.parse_uuid`, which receives a primitive
/// string at the ABI boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_parse_uuid(input: *const c_char) -> *mut Value {
    if input.is_null() {
        return error_kind(StdErrorKind::Invalid, "UUID input cannot be null");
    }
    let Ok(text) = std::str::from_utf8(unsafe { CStr::from_ptr(input) }.to_bytes()) else {
        return error_kind(StdErrorKind::NotUnicode, "UUID input must be valid UTF-8");
    };
    match sql_uuid_value(text) {
        Ok(value) => ok(value),
        Err(message) => error(message),
    }
}

fn release(handle: i64) {
    let mut entries = lock(&UUIDS);
    let remove = entries.get_mut(&handle).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    });
    if remove {
        entries.remove(&handle);
    }
}

extern "C" fn copy_uuid(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let source_handle = unsafe { *source.cast::<i64>() };
    let mut entries = lock(&UUIDS);
    if let Some(entry) = entries.get_mut(&source_handle) {
        entry.refs += 1;
        unsafe { *dest.cast::<i64>() = source_handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_uuid(ptr: *mut c_void) {
    if !ptr.is_null() {
        release(unsafe { *ptr.cast::<i64>() });
    }
}

fn string_value(value: *const Value) -> Result<String, String> {
    match unsafe { value.as_ref() } {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) | None => Err("expected string".to_string()),
    }
}

fn bytes_value(value: *const Value) -> Result<Vec<u8>, String> {
    match unsafe { value.as_ref() } {
        Some(Value::Bytes(value)) => Ok(value.clone()),
        Some(_) | None => Err("expected bytes".to_string()),
    }
}

fn parse_bytes(bytes: Vec<u8>) -> Result<[u8; 16], String> {
    bytes
        .try_into()
        .map_err(|_| "Uuid bytes must contain exactly 16 octets".to_string())
}

fn node() -> [u8; 6] {
    *PROCESS_NODE.get_or_init(|| {
        let random = RustUuid::new_v4();
        let mut node = [0; 6];
        node.copy_from_slice(&random.as_bytes()[..6]);
        node[0] |= 0x01;
        node
    })
}

fn timestamp_uuid(sorted: bool) -> RustUuid {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ticks = elapsed
        .as_secs()
        .saturating_add(12_219_292_800)
        .saturating_mul(10_000_000)
        .saturating_add(u64::from(elapsed.subsec_nanos() / 100));
    let sequence = CLOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    if sorted {
        ::uuid::Builder::from_sorted_gregorian_timestamp(ticks, sequence, &node()).into_uuid()
    } else {
        ::uuid::Builder::from_gregorian_timestamp(ticks, sequence, &node()).into_uuid()
    }
}

fn namespace_hash(namespace: RustUuid, name: &[u8], sha: bool) -> RustUuid {
    let mut bytes = [0; 16];
    if sha {
        // UUID version 5 is defined by RFC 4122 to use SHA-1.
        let mut hasher = Sha1::new(); // NOSONAR
        hasher.update(namespace.as_bytes());
        hasher.update(name);
        bytes.copy_from_slice(&hasher.finalize()[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
    } else {
        // UUID version 3 is defined by RFC 4122 to use MD5.
        let mut hasher = Md5::new(); // NOSONAR
        hasher.update(namespace.as_bytes());
        hasher.update(name);
        bytes.copy_from_slice(&hasher.finalize());
        bytes[6] = (bytes[6] & 0x0f) | 0x30;
    }
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RustUuid::from_bytes(bytes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_parse(value: *const Value) -> *mut Value {
    match string_value(value).and_then(|text| RustUuid::parse_str(&text).map_err(|e| e.to_string()))
    {
        Ok(uuid) => result_uuid(uuid),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_from_bytes(value: *const Value) -> *mut Value {
    match bytes_value(value)
        .and_then(parse_bytes)
        .map(RustUuid::from_bytes)
    {
        Ok(uuid) => result_uuid(uuid),
        Err(message) => error_kind(StdErrorKind::Invalid, message),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_nil() -> *mut Value {
    direct_uuid(RustUuid::nil())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_max() -> *mut Value {
    direct_uuid(RustUuid::max())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_v1() -> *mut Value {
    direct_uuid(timestamp_uuid(false))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_v3(namespace: *const Value, name: *const Value) -> *mut Value {
    let result = uuid_from_value(namespace).and_then(|namespace| {
        string_value(name).map(|name| namespace_hash(namespace, name.as_bytes(), false))
    });
    result.map_or_else(error, result_uuid)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_v4() -> *mut Value {
    direct_uuid(RustUuid::new_v4())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_v5(namespace: *const Value, name: *const Value) -> *mut Value {
    let result = uuid_from_value(namespace).and_then(|namespace| {
        string_value(name).map(|name| namespace_hash(namespace, name.as_bytes(), true))
    });
    result.map_or_else(error, result_uuid)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_v6() -> *mut Value {
    direct_uuid(timestamp_uuid(true))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_v7() -> *mut Value {
    direct_uuid(RustUuid::now_v7())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_v8(value: *const Value) -> *mut Value {
    match bytes_value(value)
        .and_then(parse_bytes)
        .map(RustUuid::new_v8)
    {
        Ok(uuid) => result_uuid(uuid),
        Err(message) => error(message),
    }
}

fn string_result(value: Result<String, String>) -> *mut Value {
    mux_rc_alloc(Value::String(value.unwrap_or_default()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_to_string(value: *const Value) -> *mut Value {
    string_result(uuid_from_value(value).map(|uuid| uuid.to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_to_compact(value: *const Value) -> *mut Value {
    string_result(uuid_from_value(value).map(|uuid| uuid.simple().to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_to_braced(value: *const Value) -> *mut Value {
    string_result(uuid_from_value(value).map(|uuid| uuid.braced().to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_to_urn(value: *const Value) -> *mut Value {
    string_result(uuid_from_value(value).map(|uuid| uuid.urn().to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_to_bytes(value: *const Value) -> *mut Value {
    mux_rc_alloc(
        uuid_from_value(value)
            .map(|uuid| Value::Bytes(uuid.as_bytes().to_vec()))
            .unwrap_or(Value::Bytes(Vec::new())),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_to_parts(value: *const Value) -> *mut Value {
    let parts = uuid_from_value(value)
        .map(|uuid| {
            let number = uuid.as_u128();
            Tuple(
                Value::Int((number >> 64) as u64 as i64),
                Value::Int(number as u64 as i64),
            )
        })
        .unwrap_or(Tuple(Value::Int(0), Value::Int(0)));
    mux_rc_alloc(Value::Tuple(Box::new(parts)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_is_nil(value: *const Value) -> bool {
    uuid_from_value(value).is_ok_and(|uuid| uuid.is_nil())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_is_max(value: *const Value) -> bool {
    uuid_from_value(value).is_ok_and(|uuid| uuid.is_max())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_version(value: *const Value) -> *mut Value {
    let version = uuid_from_value(value)
        .map(|uuid| Value::Optional(Some(Box::new(Value::Int(uuid.get_version_num() as i64)))))
        .unwrap_or(Value::Optional(None));
    mux_rc_alloc(version)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_variant(value: *const Value) -> *mut Value {
    let variant = uuid_from_value(value).map(|uuid| {
        let name = match uuid.get_variant() {
            ::uuid::Variant::NCS => "ncs",
            ::uuid::Variant::RFC4122 => "rfc4122",
            ::uuid::Variant::Microsoft => "microsoft",
            ::uuid::Variant::Future => "future",
            _ => "unknown",
        };
        Value::String(name.to_string())
    });
    mux_rc_alloc(variant.unwrap_or(Value::String(String::new())))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_uuid_from_parts(high: i64, low: i64) -> *mut Value {
    let number = (u128::from(high as u64) << 64) | u128::from(low as u64);
    result_uuid(RustUuid::from_u128(number))
}
