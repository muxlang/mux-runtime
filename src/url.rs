//! Validated RFC 3986/HTTP URL values.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_alloc;
use crate::{Tuple, TypeId, Value};
use rust_std::collections::HashMap;
use rust_std::ffi::c_void;
use rust_std::sync::{LazyLock, Mutex};

type RustUrl = ::url::Url;

struct Entry {
    value: RustUrl,
    refs: usize,
}

static URLS: LazyLock<Mutex<HashMap<i64, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: Mutex<i64> = Mutex::new(1);
static URL_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Url",
        size_of::<i64>(),
        Some(drop_url as extern "C" fn(*mut c_void)),
        Some(copy_url as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> rust_std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(rust_std::sync::PoisonError::into_inner)
}

fn error(message: impl Into<String>) -> *mut Value {
    crate::std::url_result_err(message.into())
}

fn ok(value: Value) -> *mut Value {
    crate::std::url_result_ok(value)
}

fn next_handle() -> i64 {
    let mut next = lock(&NEXT_HANDLE);
    let handle = *next;
    *next = next.checked_add(1).unwrap_or(1);
    handle
}

fn object(handle: i64) -> *mut Value {
    let value = alloc_object(*URL_TYPE_ID);
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

fn direct_url(url: RustUrl) -> *mut Value {
    let handle = next_handle();
    lock(&URLS).insert(
        handle,
        Entry {
            value: url,
            refs: 1,
        },
    );
    let value = object(handle);
    if value.is_null() {
        lock(&URLS).remove(&handle);
    }
    value
}

fn result_url(url: RustUrl) -> *mut Value {
    let value = direct_url(url);
    if value.is_null() {
        return error("failed to allocate Url value");
    }
    let cloned = unsafe { (&*value).clone() };
    unsafe { crate::refcount::mux_rc_dec(value) };
    ok(cloned)
}

unsafe fn handle(value: *const Value) -> Result<i64, String> {
    if value.is_null() {
        return Err("Url value is null".to_string());
    }
    if unsafe { get_object_type_id(value) } != *URL_TYPE_ID {
        return Err("invalid Url value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("invalid Url value".to_string());
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle <= 0 || !lock(&URLS).contains_key(&handle) {
        return Err("invalid Url value".to_string());
    }
    Ok(handle)
}

fn url_by_handle(handle: i64) -> Result<RustUrl, String> {
    lock(&URLS)
        .get(&handle)
        .map(|entry| entry.value.clone())
        .ok_or_else(|| "invalid Url value".to_string())
}

fn url_from_value(value: *const Value) -> Result<RustUrl, String> {
    let handle = unsafe { handle(value) }?;
    url_by_handle(handle)
}

fn release(handle: i64) {
    let mut entries = lock(&URLS);
    let remove = entries.get_mut(&handle).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    });
    if remove {
        entries.remove(&handle);
    }
}

extern "C" fn copy_url(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let source_handle = unsafe { *source.cast::<i64>() };
    let mut entries = lock(&URLS);
    if let Some(entry) = entries.get_mut(&source_handle) {
        entry.refs += 1;
        unsafe { *dest.cast::<i64>() = source_handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_url(ptr: *mut c_void) {
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

fn string_result(value: Result<String, String>) -> *mut Value {
    mux_rc_alloc(Value::String(value.unwrap_or_default()))
}

fn optional_string(value: Option<String>) -> *mut Value {
    mux_rc_alloc(Value::Optional(
        value.map(|value| Box::new(Value::String(value))),
    ))
}

fn optional_int(value: Option<u16>) -> *mut Value {
    mux_rc_alloc(Value::Optional(
        value.map(|value| Box::new(Value::Int(i64::from(value)))),
    ))
}

fn url_with_change(value: *const Value, change: impl FnOnce(&mut RustUrl)) -> *mut Value {
    match url_from_value(value) {
        Ok(mut url) => {
            change(&mut url);
            result_url(url)
        }
        Err(message) => error(message),
    }
}

fn query_pairs(value: &RustUrl) -> Value {
    let pairs = value
        .query_pairs()
        .map(|(key, value)| {
            Value::Tuple(Box::new(Tuple(
                Value::String(key.into_owned()),
                Value::String(value.into_owned()),
            )))
        })
        .collect();
    Value::List(pairs)
}

fn query_pairs_input(value: *const Value) -> Result<Vec<(String, String)>, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err("query pairs must be a list".to_string());
    };
    values
        .iter()
        .map(|item| {
            let Value::Tuple(tuple) = item else {
                return Err("each query pair must be a tuple<string, string>".to_string());
            };
            let Value::String(key) = &tuple.0 else {
                return Err("query pair key must be a string".to_string());
            };
            let Value::String(value) = &tuple.1 else {
                return Err("query pair value must be a string".to_string());
            };
            Ok((key.clone(), value.clone()))
        })
        .collect()
}

fn set_query_pairs(url: &mut RustUrl, pairs: Vec<(String, String)>) {
    let mut serializer = ::url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(&key, &value);
    }
    url.set_query(Some(&serializer.finish()));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_parse(value: *const Value) -> *mut Value {
    match string_value(value).and_then(|text| RustUrl::parse(&text).map_err(|e| e.to_string())) {
        Ok(url) => result_url(url),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_from_file(value: *const Value) -> *mut Value {
    match string_value(value).and_then(|path| {
        RustUrl::from_file_path(path)
            .map_err(|()| "path cannot be represented as a file URL".to_string())
    }) {
        Ok(url) => result_url(url),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_to_string(value: *const Value) -> *mut Value {
    string_result(url_from_value(value).map(|url| url.to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_scheme(value: *const Value) -> *mut Value {
    string_result(url_from_value(value).map(|url| url.scheme().to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_username(value: *const Value) -> *mut Value {
    string_result(url_from_value(value).map(|url| url.username().to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_password(value: *const Value) -> *mut Value {
    optional_string(
        url_from_value(value)
            .ok()
            .and_then(|url| url.password().map(str::to_string)),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_host(value: *const Value) -> *mut Value {
    optional_string(
        url_from_value(value)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string)),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_host_ascii(value: *const Value) -> *mut Value {
    let result = url_from_value(value).and_then(|url| match url.host() {
        Some(::url::Host::Domain(host)) => {
            idna::domain_to_ascii(host).map_err(|error| format!("invalid IDNA host: {error:?}"))
        }
        Some(::url::Host::Ipv4(host)) => Ok(host.to_string()),
        Some(::url::Host::Ipv6(host)) => Ok(host.to_string()),
        None => Err("URL has no host".to_string()),
    });
    match result {
        Ok(host) => ok(Value::String(host)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_host_unicode(value: *const Value) -> *mut Value {
    let result = url_from_value(value).and_then(|url| match url.host() {
        Some(::url::Host::Domain(host)) => {
            let (unicode, result) = idna::domain_to_unicode(host);
            result
                .map(|()| unicode)
                .map_err(|error| format!("invalid IDNA host: {error:?}"))
        }
        Some(::url::Host::Ipv4(host)) => Ok(host.to_string()),
        Some(::url::Host::Ipv6(host)) => Ok(host.to_string()),
        None => Err("URL has no host".to_string()),
    });
    match result {
        Ok(host) => ok(Value::String(host)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_port(value: *const Value) -> *mut Value {
    optional_int(url_from_value(value).ok().and_then(|url| url.port()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_path(value: *const Value) -> *mut Value {
    string_result(url_from_value(value).map(|url| url.path().to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_query(value: *const Value) -> *mut Value {
    optional_string(
        url_from_value(value)
            .ok()
            .and_then(|url| url.query().map(str::to_string)),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_fragment(value: *const Value) -> *mut Value {
    optional_string(
        url_from_value(value)
            .ok()
            .and_then(|url| url.fragment().map(str::to_string)),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_origin(value: *const Value) -> *mut Value {
    // Delegate serialization to `url` so IPv6 hosts retain their required
    // brackets (for example, `https://[::1]`) and default ports are omitted
    // according to the URL/WHATWG origin rules. `host_str()` drops those
    // brackets, producing the ambiguous `https://::1` form.
    string_result(url_from_value(value).map(|url| url.origin().ascii_serialization()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_redacted(value: *const Value) -> *mut Value {
    string_result(url_from_value(value).map(|mut url| {
        if url.password().is_some() {
            let _ = url.set_password(Some("[REDACTED]"));
        }
        url.to_string()
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_join(value: *const Value, relative: *const Value) -> *mut Value {
    match url_from_value(value).and_then(|url| {
        string_value(relative).and_then(|relative| url.join(&relative).map_err(|e| e.to_string()))
    }) {
        Ok(url) => result_url(url),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_path(value: *const Value, path: *const Value) -> *mut Value {
    match string_value(path) {
        Ok(path) => url_with_change(value, |url| url.set_path(&path)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_query(
    value: *const Value,
    query: *const Value,
) -> *mut Value {
    match string_value(query) {
        Ok(query) => url_with_change(value, |url| url.set_query(Some(&query))),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_fragment(
    value: *const Value,
    fragment: *const Value,
) -> *mut Value {
    match string_value(fragment) {
        Ok(fragment) => url_with_change(value, |url| url.set_fragment(Some(&fragment))),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_host(value: *const Value, host: *const Value) -> *mut Value {
    match string_value(host) {
        Ok(host) => match url_from_value(value) {
            Ok(mut url) => match url.set_host(Some(&host)) {
                Ok(()) => result_url(url),
                Err(_) => error("invalid URL host"),
            },
            Err(message) => error(message),
        },
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_port(value: *const Value, port: i64) -> *mut Value {
    if !(0..=65_535).contains(&port) {
        return error("port must be between 0 and 65535");
    }
    let port = port as u16;
    match url_from_value(value) {
        Ok(mut url) => match url.set_port(Some(port)) {
            Ok(()) => result_url(url),
            Err(()) => error("URL scheme does not accept a port"),
        },
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_scheme(
    value: *const Value,
    scheme: *const Value,
) -> *mut Value {
    let scheme = match string_value(scheme) {
        Ok(scheme) => scheme,
        Err(message) => return error(message),
    };
    match url_from_value(value) {
        Ok(mut url) => match url.set_scheme(&scheme) {
            Ok(()) => result_url(url),
            Err(()) => error("invalid URL scheme"),
        },
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_username(
    value: *const Value,
    username: *const Value,
) -> *mut Value {
    let username = match string_value(username) {
        Ok(username) => username,
        Err(message) => return error(message),
    };
    match url_from_value(value) {
        Ok(mut url) => match url.set_username(&username) {
            Ok(()) => result_url(url),
            Err(()) => error("invalid URL username"),
        },
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_password(
    value: *const Value,
    password: *const Value,
) -> *mut Value {
    let password = match string_value(password) {
        Ok(password) => password,
        Err(message) => return error(message),
    };
    match url_from_value(value) {
        Ok(mut url) => match url.set_password(Some(&password)) {
            Ok(()) => result_url(url),
            Err(()) => error("invalid URL password"),
        },
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_query_pairs(value: *const Value) -> *mut Value {
    let pairs = url_from_value(value).map_or(Value::List(Vec::new()), |url| query_pairs(&url));
    mux_rc_alloc(pairs)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_with_query_pairs(
    value: *const Value,
    pairs: *const Value,
) -> *mut Value {
    match query_pairs_input(pairs) {
        Ok(pairs) => url_with_change(value, |url| set_query_pairs(url, pairs)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_to_file_path(value: *const Value) -> *mut Value {
    match url_from_value(value).and_then(|url| {
        url.to_file_path()
            .map_err(|()| "URL cannot be represented as a local path".to_string())
            .map(|path| path.to_string_lossy().into_owned())
    }) {
        Ok(path) => ok(Value::String(path)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_is_http(value: *const Value) -> bool {
    url_from_value(value).is_ok_and(|url| url.scheme() == "http")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_is_https(value: *const Value) -> bool {
    url_from_value(value).is_ok_and(|url| url.scheme() == "https")
}
