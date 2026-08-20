use crate::Value;
use indexmap::IndexMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// The map backing a JSON object.
///
/// Insertion-ordered, not sorted. A `BTreeMap` re-ordered keys alphabetically,
/// so a program could not read a document and write it back unchanged. That is
/// the same reasoning `ordered.rs` gives for Mux's own `map`: printed output
/// must not depend on the container's internal arrangement.
pub type JsonMap = IndexMap<String, Json>;

/// A parsed JSON document.
///
/// Integers and reals are separate cases on purpose. Collapsing both into one
/// `f64` meant `{"n":42}` re-serialized as `{"n":42.0}` and any integer past
/// 2^53 came back a different number - so a program could not read JSON and
/// write it out again without altering it.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Json>),
    Object(JsonMap),
}

impl Json {
    pub fn parse(input: &str) -> Result<Json, String> {
        match serde_json::from_str::<serde_json::Value>(input) {
            Ok(v) => Ok(convert_serde_value(&v)),
            Err(e) => Err(format!("{}", e)),
        }
    }

    pub fn stringify(&self, indent: Option<usize>) -> String {
        let v = convert_to_serde_value(self);
        if let Some(n) = indent {
            serde_json::to_string_pretty(&v)
                .unwrap_or_else(|_| String::new())
                .replace("  ", &" ".repeat(n))
        } else {
            serde_json::to_string(&v).unwrap_or_else(|_| String::new())
        }
    }
}

fn convert_serde_value(v: &serde_json::Value) -> Json {
    match v {
        serde_json::Value::Null => Json::Null,
        serde_json::Value::Bool(b) => Json::Bool(*b),
        // Ask for an integer first. serde_json already tracks whether the
        // literal was integral, and that is the distinction worth keeping; a
        // u64 above i64::MAX is the one case with no exact home, and falls back
        // to the float it would previously always have been.
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Json::Int(i),
            None => match n.as_f64() {
                Some(f) => Json::Float(f),
                None => Json::Null,
            },
        },
        serde_json::Value::String(s) => Json::String(s.clone()),
        serde_json::Value::Array(arr) => Json::Array(arr.iter().map(convert_serde_value).collect()),
        serde_json::Value::Object(map) => {
            let mut m = JsonMap::new();
            for (k, v) in map.iter() {
                m.insert(k.clone(), convert_serde_value(v));
            }
            Json::Object(m)
        }
    }
}

fn convert_to_serde_value(j: &Json) -> serde_json::Value {
    match j {
        Json::Null => serde_json::Value::Null,
        Json::Bool(b) => serde_json::Value::Bool(*b),
        Json::Int(i) => serde_json::Value::Number((*i).into()),
        // NaN and infinity have no JSON representation, so `from_f64` returns
        // None and the value becomes null.
        Json::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Json::String(s) => serde_json::Value::String(s.clone()),
        Json::Array(a) => serde_json::Value::Array(a.iter().map(convert_to_serde_value).collect()),
        Json::Object(m) => {
            let map = m
                .iter()
                .map(|(k, v)| (k.clone(), convert_to_serde_value(v)))
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

// Expose simple runtime helpers to convert between Json and Value
#[allow(clippy::mutable_key_type)]
pub fn json_to_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Unit,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::Float(f) => Value::Float(ordered_float::OrderedFloat(*f)),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
        Json::Object(m) => {
            let mut map = crate::ordered::OrderedMap::new();
            for (k, v) in m.iter() {
                map.insert(Value::String(k.clone()), json_to_value(v));
            }
            Value::Map(map)
        }
    }
}

pub fn value_to_json(v: &Value) -> Result<Json, String> {
    match v {
        Value::Unit => Ok(Json::Null),
        Value::Bool(b) => Ok(Json::Bool(*b)),
        Value::Int(i) => Ok(Json::Int(*i)),
        Value::Float(f) => {
            let float_val = f.into_inner();
            if float_val.is_finite() {
                Ok(Json::Float(float_val))
            } else if float_val.is_nan() {
                Err("cannot serialize NaN to JSON".to_string())
            } else {
                Err("cannot serialize infinity to JSON".to_string())
            }
        }
        Value::String(s) => Ok(Json::String(s.clone())),
        Value::List(list) => {
            let items = list
                .iter()
                .map(value_to_json)
                .collect::<Result<Vec<Json>, String>>()?;
            Ok(Json::Array(items))
        }
        Value::Map(map) => {
            let mut m = JsonMap::new();
            for (k, v) in map.iter() {
                // only string keys allowed in JSON
                if let Value::String(key_str) = k {
                    m.insert(key_str.clone(), value_to_json(v)?);
                } else {
                    return Err("map contains non-string key, cannot convert to JSON".to_string());
                }
            }
            Ok(Json::Object(m))
        }
        _ => Err("unsupported value type for JSON conversion".to_string()),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_parse(input: *const c_char) -> *mut Value {
    if input.is_null() {
        let msg = CString::new("null input").unwrap();
        unsafe {
            return crate::result::mux_result_err_str(msg.as_ptr());
        }
    }
    let s = unsafe { CStr::from_ptr(input) }
        .to_string_lossy()
        .into_owned();
    match Json::parse(&s) {
        Ok(j) => {
            let v = json_to_value(&j);
            // Wrap the value directly. Going through mux_result_ok_value would
            // clone `v` into the Result without consuming the intermediate
            // ref-counted allocation, leaking it.
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(v))))
        }
        Err(e) => {
            let cmsg = CString::new(e).unwrap();
            unsafe { crate::result::mux_result_err_str(cmsg.as_ptr()) }
        }
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_stringify(val: *const Value, indent_opt: *mut Value) -> *mut Value {
    if val.is_null() {
        let msg = CString::new("null input").unwrap();
        unsafe {
            return crate::result::mux_result_err_str(msg.as_ptr());
        }
    }
    let v = unsafe { &*val };
    let indent = if indent_opt.is_null() {
        None
    } else {
        unsafe {
            match &*indent_opt {
                Value::Optional(Some(boxed)) => match boxed.as_ref() {
                    Value::Int(i) => Some(*i as usize),
                    _ => None,
                },
                _ => None,
            }
        }
    };

    match value_to_json(v) {
        Ok(j) => {
            let s = j.stringify(indent);
            let result_value = Value::String(s);
            // Wrap directly to avoid leaking the intermediate allocation (see
            // mux_json_parse).
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(result_value))))
        }
        Err(e) => {
            let cmsg = CString::new(e).unwrap();
            unsafe { crate::result::mux_result_err_str(cmsg.as_ptr()) }
        }
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_from_map(val: *const Value) -> *mut Value {
    if val.is_null() {
        let msg = CString::new("null input").unwrap();
        unsafe { return crate::result::mux_result_err_str(msg.as_ptr()) }
    }
    let v = unsafe { &*val };
    // Expect a Map at runtime
    match v {
        Value::Map(map) => {
            // Convert each value to Json to validate
            let mut jmap = JsonMap::new();
            for (k, vv) in map.iter() {
                if let Value::String(key_str) = k {
                    match value_to_json(vv) {
                        Ok(jv) => {
                            jmap.insert(key_str.clone(), jv);
                        }
                        Err(e) => {
                            let cmsg = CString::new(e).unwrap();
                            unsafe { return crate::result::mux_result_err_str(cmsg.as_ptr()) }
                        }
                    }
                } else {
                    let cmsg = CString::new("map contains non-string key, cannot convert to JSON")
                        .unwrap();
                    unsafe { return crate::result::mux_result_err_str(cmsg.as_ptr()) }
                }
            }
            let j = Json::Object(jmap);
            let v = json_to_value(&j);
            // Wrap directly to avoid leaking the intermediate allocation (see
            // mux_json_parse).
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(v))))
        }
        _ => {
            let cmsg = CString::new("value is not a map").unwrap();
            unsafe { crate::result::mux_result_err_str(cmsg.as_ptr()) }
        }
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_to_map(val: *const Value) -> *mut Value {
    if val.is_null() {
        let msg = CString::new("null input").unwrap();
        unsafe { return crate::result::mux_result_err_str(msg.as_ptr()) }
    }
    let v = unsafe { &*val };
    match value_to_json(v) {
        Ok(Json::Object(m)) => {
            let mv = json_to_value(&Json::Object(m));
            // Wrap directly to avoid leaking the intermediate allocation (see
            // mux_json_parse).
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(mv))))
        }
        Ok(_) => {
            let cmsg = CString::new("json value is not an object").unwrap();
            unsafe { crate::result::mux_result_err_str(cmsg.as_ptr()) }
        }
        Err(e) => {
            let cmsg = CString::new(e).unwrap();
            unsafe { crate::result::mux_result_err_str(cmsg.as_ptr()) }
        }
    }
}

// Typed accessors.
//
// `stringify` was the only method a `Json` had, so reading a value meant
// serializing it back to JSON text: a string field came out as `"Ada"` WITH the
// quotes, and undoing that needs string operations Mux does not have yet
// (mux-compiler#389). So a string could not be read out of a document at all,
// and a number took `stringify` then `to_float` then `to_int` because
// `"36.0".to_int()` fails.
//
// Each returns `optional<T>`, `none` when the value is a different kind.
// `optional` rather than `result` because "this field held a string, not a
// number" is ordinary control flow when reading a document, not an error with
// something to report - the same reasoning that makes `list.get` an optional.
//
// `mux_json_parse` already converts to a native `Value`, so these are variant
// checks rather than a second representation to keep in step.

/// Some(x) when `val` matches `want`, mapped through `f`; none otherwise.
///
/// # Safety contract for every accessor below
///
/// `val` must be either null or a valid pointer to a live `Value` that stays
/// alive for the duration of the call. Null is handled here and yields `none`;
/// a dangling or misaligned pointer is undefined behaviour. Each accessor
/// borrows its argument and never takes ownership - the caller still releases
/// what it passed - and returns a NEW owned optional the caller releases with
/// `mux_rc_dec`.
///
/// These stay safe `extern "C"` rather than `unsafe fn` to match the other
/// entry points in this module (`mux_json_parse`, `mux_json_stringify`,
/// `mux_json_from_map`, `mux_json_to_map`), which take the same shape of
/// argument under the same contract.
fn json_accessor<F>(val: *const Value, f: F) -> *mut Value
where
    F: FnOnce(&Value) -> Option<Value>,
{
    if val.is_null() {
        return crate::optional::mux_optional_none();
    }
    match f(unsafe { &*val }) {
        // Wrap directly rather than through mux_optional_some_value, which
        // clones its argument without consuming it and would leak the
        // intermediate allocation (see mux_json_parse).
        Some(inner) => crate::refcount::mux_rc_alloc(Value::Optional(Some(Box::new(inner)))),
        None => crate::optional::mux_optional_none(),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_as_string(val: *const Value) -> *mut Value {
    json_accessor(val, |v| match v {
        Value::String(s) => Some(Value::String(s.clone())),
        _ => None,
    })
}

/// An integer, exactly. A float is accepted only when it is integral AND fits,
/// so a document written `{"n": 42.0}` still reads as 42, while 1.5 is `none`
/// rather than silently truncated.
///
/// The range check is not redundant: `1e30` is integral and finite, and `as
/// i64` SATURATES rather than wrapping, so without it a value far outside the
/// range came back as `i64::MAX` - a plausible number that is not the one in
/// the document. Comparing against the bounds as `f64` is deliberate; casting
/// `i64::MAX` to `f64` rounds up, so `>=` on the upper bound is what excludes
/// exactly the values that would not survive the conversion.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_as_int(val: *const Value) -> *mut Value {
    json_accessor(val, |v| match v {
        Value::Int(i) => Some(Value::Int(*i)),
        Value::Float(f) => {
            let f = f.into_inner();
            let in_range = f >= (i64::MIN as f64) && f < (i64::MAX as f64);
            if f.fract() == 0.0 && f.is_finite() && in_range {
                Some(Value::Int(f as i64))
            } else {
                None
            }
        }
        _ => None,
    })
}

/// A float. An integer widens, since every JSON number is a number first and
/// asking for a float is asking how to read it, not what it was written as.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_as_float(val: *const Value) -> *mut Value {
    json_accessor(val, |v| match v {
        Value::Float(f) => Some(Value::Float(*f)),
        Value::Int(i) => Some(Value::Float(ordered_float::OrderedFloat(*i as f64))),
        _ => None,
    })
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_as_bool(val: *const Value) -> *mut Value {
    json_accessor(val, |v| match v {
        Value::Bool(b) => Some(Value::Bool(*b)),
        _ => None,
    })
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_as_list(val: *const Value) -> *mut Value {
    json_accessor(val, |v| match v {
        Value::List(items) => Some(Value::List(items.clone())),
        _ => None,
    })
}

/// The object as a map. `json.to_map` does the same thing as a free function
/// returning a `result`; this is the method form, and `none` rather than an
/// error message for a non-object.
#[allow(clippy::mutable_key_type)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_as_map(val: *const Value) -> *mut Value {
    json_accessor(val, |v| match v {
        Value::Map(m) => Some(Value::Map(m.clone())),
        _ => None,
    })
}

/// One field of a JSON object, by name.
///
/// `none` covers both "not an object" and "no such key". A field that IS
/// present but holds `null` comes back as `some(Value::Unit)` instead, which is
/// what keeps an absent field distinguishable from an explicit null - the
/// distinction typed deserialization is built on, since a missing required
/// field is an error while `optional<T>` accepts either spelling.
///
/// So this answers "is it there", and `mux_json_is_null` answers "is what is
/// there null". Neither question alone is enough.
///
/// The compiler emits one call per declared field rather than converting the
/// whole object to a Mux map first, which would clone every value including
/// the ones the class does not declare.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_field(val: *const Value, key: *const c_char) -> *mut Value {
    if key.is_null() {
        return crate::optional::mux_optional_none();
    }
    // `to_string_lossy` rather than `to_str`, matching the rest of the crate.
    // The lossy path is unreachable - a Mux string is a `String`, so the key is
    // already valid UTF-8 - and if it somehow were not, a replacement character
    // simply fails to match any field, which is the same answer.
    let name = unsafe { CStr::from_ptr(key) }
        .to_string_lossy()
        .into_owned();
    json_accessor(val, |v| match v {
        Value::Map(entries) => entries.get(&Value::String(name.clone())).cloned(),
        _ => None,
    })
}

/// JSON null. `Value::Unit` is what `json_to_value` maps it to.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_json_is_null(val: *const Value) -> bool {
    !val.is_null() && matches!(unsafe { &*val }, Value::Unit)
}
