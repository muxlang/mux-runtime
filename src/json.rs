use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::{TypeId, Value};
use indexmap::IndexMap;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::fmt;
use std::os::raw::c_char;
use std::sync::{LazyLock, Mutex};

/// The map backing a JSON object.
///
/// Insertion-ordered, not sorted. A `BTreeMap` re-ordered keys alphabetically,
/// so a program could not read a document and write it back unchanged. That is
/// the same reasoning `ordered.rs` gives for Mux's own `map`: printed output
/// must not depend on the container's internal arrangement.
pub type JsonMap = IndexMap<String, Json>;

/// A JSON number whose original token is retained exactly.
///
/// The ordinary `Int` and `Float` variants remain useful for values created
/// from Mux primitives. Parsed spellings that would change when represented
/// as either type use `Number` instead, preserving exponent notation,
/// trailing zeroes, and integers outside the Mux `int` range.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JsonNumber {
    raw: String,
}

impl JsonNumber {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

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
    Number(JsonNumber),
    String(String),
    Array(Vec<Json>),
    Object(JsonMap),
}

struct JsonNumberEntry {
    raw: String,
    names: usize,
}

static JSON_NUMBERS: LazyLock<Mutex<HashMap<i64, JsonNumberEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_JSON_NUMBER: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static JSON_NUMBER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "JsonNumber",
        std::mem::size_of::<i64>(),
        Some(drop_json_number as extern "C" fn(*mut c_void)),
        Some(copy_json_number as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn json_number_handle(value: *const Value) -> Option<i64> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *JSON_NUMBER_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return None;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    JSON_NUMBERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&handle)
        .then_some(handle)
}

fn json_number_raw(value: *const Value) -> Option<String> {
    let handle = json_number_handle(value)?;
    JSON_NUMBERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle)
        .map(|entry| entry.raw.clone())
}

fn json_number_value(raw: &str) -> Value {
    let mut next = NEXT_JSON_NUMBER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let handle = *next;
    *next = next.checked_add(1).unwrap_or(1);
    JSON_NUMBERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            JsonNumberEntry {
                raw: raw.to_string(),
                names: 1,
            },
        );
    let object = alloc_object(*JSON_NUMBER_TYPE_ID);
    if object.is_null() {
        JSON_NUMBERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::Unit;
    }
    let ptr = unsafe { get_object_ptr(object) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(object) };
        JSON_NUMBERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::Unit;
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let Value::Object(reference) = (unsafe { &*object }) else {
        unsafe { mux_rc_dec(object) };
        return Value::Unit;
    };
    let value = Value::Object(reference.clone());
    unsafe { mux_rc_dec(object) };
    value
}

extern "C" fn copy_json_number(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    let mut values = JSON_NUMBERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = values.get_mut(&handle) {
        entry.names += 1;
        unsafe { *dest.cast::<i64>() = handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_json_number(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    let mut values = JSON_NUMBERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = values.get_mut(&handle) {
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            values.remove(&handle);
        }
    }
}

impl Json {
    pub fn parse(input: &str) -> Result<Json, String> {
        Self::parse_with_policy(input, JsonDuplicatePolicy::Reject)
    }

    pub fn parse_with_policy(
        input: &str,
        duplicate_policy: JsonDuplicatePolicy,
    ) -> Result<Json, String> {
        if input.len() > MAX_JSON_INPUT_BYTES {
            return Err("JSON input exceeds the 16 MiB limit".to_string());
        }
        enforce_json_token_limit(input)?;
        let number_tokens = scan_json_numbers(input)?;
        let number_index = Cell::new(0);
        let mut deserializer = serde_json::Deserializer::from_str(input);
        let value = JsonParseSeed {
            duplicate_policy,
            number_index: &number_index,
        }
        .deserialize(&mut deserializer)
        .map_err(|e| e.to_string())?;
        deserializer.end().map_err(|e| e.to_string())?;
        if number_index.get() != number_tokens.len() {
            return Err("JSON number token accounting failed".to_string());
        }
        convert_parsed_value(&value, &number_tokens)
    }

    #[must_use]
    pub fn stringify(&self, indent: Option<usize>) -> String {
        render_json(self, indent, 0)
    }

    /// Serialize using deterministic recursive object-key ordering.
    /// Numbers use the same serde_json representation as `stringify`; Mux's
    /// native JSON model already separates integers from finite reals.
    #[must_use]
    pub fn canonical(&self) -> String {
        canonical_serde_value(self).to_string()
    }
}

/// Policy used when an object contains the same key more than once.
///
/// `Reject` is the default and prevents ambiguous input. `First` keeps the
/// first value and `Last` keeps the last value, matching the two common JSON
/// interoperability conventions. Object key order remains the order in which
/// the retained keys first appeared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum JsonDuplicatePolicy {
    Reject = 0,
    First = 1,
    Last = 2,
}

fn render_json(value: &Json, indent: Option<usize>, depth: usize) -> String {
    let child = |value: &Json| render_json(value, indent, depth + 1);
    match value {
        Json::Null => "null".to_string(),
        Json::Bool(value) => value.to_string(),
        Json::Int(value) => value.to_string(),
        Json::Float(value) => serde_json::Number::from_f64(*value)
            .map_or_else(|| "null".to_string(), |number| number.to_string()),
        Json::Number(number) => number.raw.clone(),
        Json::String(value) => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
        Json::Array(values) => {
            if values.is_empty() {
                return "[]".to_string();
            }
            let Some(width) = indent else {
                return format!(
                    "[{}]",
                    values.iter().map(child).collect::<Vec<_>>().join(",")
                );
            };
            let pad = " ".repeat(width * (depth + 1));
            let close_pad = " ".repeat(width * depth);
            format!(
                "[\n{}\n{}]",
                values
                    .iter()
                    .map(|value| format!("{pad}{}", child(value)))
                    .collect::<Vec<_>>()
                    .join(",\n"),
                close_pad
            )
        }
        Json::Object(values) => {
            if values.is_empty() {
                return "{}".to_string();
            }
            let separator = if indent.is_some() { ": " } else { ":" };
            let entries = values.iter().map(|(key, value)| {
                format!(
                    "{}{}{}",
                    serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                    separator,
                    child(value)
                )
            });
            let Some(width) = indent else {
                return format!("{{{}}}", entries.collect::<Vec<_>>().join(","));
            };
            let pad = " ".repeat(width * (depth + 1));
            let close_pad = " ".repeat(width * depth);
            format!(
                "{{\n{}\n{}}}",
                entries
                    .map(|entry| format!("{pad}{entry}"))
                    .collect::<Vec<_>>()
                    .join(",\n"),
                close_pad
            )
        }
    }
}

/// The parse tree used between serde's visitor and Mux's JSON model. Number
/// nodes retain their position in the source token list because serde's
/// scalar callbacks do not expose the original spelling. Keeping the index
/// also means discarded duplicate values still consume their number tokens,
/// so first/last policies cannot corrupt exact-number accounting.
enum ParsedJson {
    Null,
    Bool(bool),
    Number(usize),
    String(String),
    Array(Vec<ParsedJson>),
    Object(IndexMap<String, ParsedJson>),
}

struct JsonParseSeed<'a> {
    duplicate_policy: JsonDuplicatePolicy,
    number_index: &'a Cell<usize>,
}

impl<'de> DeserializeSeed<'de> for JsonParseSeed<'_> {
    type Value = ParsedJson;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonParseVisitor {
            duplicate_policy: self.duplicate_policy,
            number_index: self.number_index,
        })
    }
}

struct JsonParseVisitor<'a> {
    duplicate_policy: JsonDuplicatePolicy,
    number_index: &'a Cell<usize>,
}

impl<'de> Visitor<'de> for JsonParseVisitor<'_> {
    type Value = ParsedJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(ParsedJson::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        let _ = value;
        Ok(ParsedJson::Number(self.take_number()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let _ = value;
        Ok(ParsedJson::Number(self.take_number()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if !value.is_finite() {
            return Err(E::custom("JSON numbers must be finite"));
        }
        Ok(ParsedJson::Number(self.take_number()))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(ParsedJson::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(ParsedJson::String(value))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(ParsedJson::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_unit()
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        JsonParseSeed {
            duplicate_policy: self.duplicate_policy,
            number_index: self.number_index,
        }
        .deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(JsonParseSeed {
            duplicate_policy: self.duplicate_policy,
            number_index: self.number_index,
        })? {
            values.push(value);
        }
        Ok(ParsedJson::Array(values))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = IndexMap::new();
        while let Some(key) = access.next_key::<String>()? {
            let duplicate = values.contains_key(&key);
            if duplicate && self.duplicate_policy == JsonDuplicatePolicy::Reject {
                return Err(serde::de::Error::custom(format!(
                    "duplicate object key '{key}'"
                )));
            }
            let value = access.next_value_seed(JsonParseSeed {
                duplicate_policy: self.duplicate_policy,
                number_index: self.number_index,
            })?;
            match (duplicate, self.duplicate_policy) {
                (true, JsonDuplicatePolicy::First) => {}
                _ => {
                    values.insert(key, value);
                }
            }
        }
        Ok(ParsedJson::Object(values))
    }
}

impl JsonParseVisitor<'_> {
    fn take_number(&self) -> usize {
        let index = self.number_index.get();
        self.number_index.set(index + 1);
        index
    }
}

fn scan_json_numbers(input: &str) -> Result<Vec<String>, String> {
    let bytes = input.as_bytes();
    let mut numbers = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index = skip_json_string(bytes, index);
            }
            b'-' | b'0'..=b'9' => {
                let start = index;
                index = scan_json_number(bytes, index);
                numbers.push(input[start..index].to_string());
            }
            _ => index += 1,
        }
    }
    Ok(numbers)
}

fn skip_json_string(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.saturating_add(2),
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    index
}

fn scan_json_number(bytes: &[u8], start: usize) -> usize {
    let mut index = scan_json_integer(bytes, start);
    index = scan_json_fraction(bytes, index);
    scan_json_exponent(bytes, index)
}

fn scan_json_integer(bytes: &[u8], mut index: usize) -> usize {
    if bytes[index] == b'-' {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == b'0' {
        index += 1;
    } else {
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
    }
    index
}

fn scan_json_fraction(bytes: &[u8], index: usize) -> usize {
    if bytes.get(index) != Some(&b'.') {
        return index;
    }
    let fraction_start = index;
    let digits_start = index + 1;
    let end = scan_ascii_digits(bytes, digits_start);
    if digits_start == end {
        fraction_start
    } else {
        end
    }
}

fn scan_json_exponent(bytes: &[u8], index: usize) -> usize {
    if !bytes
        .get(index)
        .is_some_and(|byte| matches!(*byte, b'e' | b'E'))
    {
        return index;
    }
    let exponent_start = index;
    let mut digits_start = index + 1;
    if bytes
        .get(digits_start)
        .is_some_and(|byte| matches!(*byte, b'+' | b'-'))
    {
        digits_start += 1;
    }
    let end = scan_ascii_digits(bytes, digits_start);
    if digits_start == end {
        exponent_start
    } else {
        end
    }
}

fn scan_ascii_digits(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    index
}

/// Check the lexical token budget without allocating a token vector.  The
/// token reader has the same budget, but ordinary tree parsing used to count
/// only number tokens indirectly and could therefore materialize more than a
/// million-token document.  Counting the same lexical categories here keeps
/// all JSON entry points bounded before serde allocates the parse tree.
fn enforce_json_token_limit(input: &str) -> Result<(), String> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut count = 0;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        if count >= MAX_JSON_TOKENS {
            return Err("JSON token stream exceeds the one-million-token limit".to_string());
        }
        count += 1;
        match bytes[index] {
            b'"' => {
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        break;
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'e' | b'E' | b'+' | b'-' | b'.')
                {
                    index += 1;
                }
            }
            b't' if bytes.get(index..index + 4) == Some(b"true") => index += 4,
            b'f' if bytes.get(index..index + 5) == Some(b"false") => index += 5,
            b'n' if bytes.get(index..index + 4) == Some(b"null") => index += 4,
            _ => index += 1,
        }
    }
    Ok(())
}

fn parsed_number(raw: String) -> Json {
    if let Ok(value) = raw.parse::<i64>() {
        if value.to_string() == raw {
            return Json::Int(value);
        }
    }
    // An integral token outside the Mux `int` range must not fall through to
    // `f64`: even a finite float can silently round a large integer.
    if !raw.contains(['.', 'e', 'E']) {
        return Json::Number(JsonNumber { raw });
    }
    if let Ok(number) = raw.parse::<serde_json::Number>() {
        if let Some(value) = number.as_f64() {
            if serde_json::to_string(&number).is_ok_and(|canonical| canonical == raw) {
                return Json::Float(value);
            }
        }
    }
    Json::Number(JsonNumber { raw })
}

fn convert_parsed_value(v: &ParsedJson, number_tokens: &[String]) -> Result<Json, String> {
    match v {
        ParsedJson::Null => Ok(Json::Null),
        ParsedJson::Bool(b) => Ok(Json::Bool(*b)),
        ParsedJson::Number(index) => Ok(parsed_number(
            number_tokens
                .get(*index)
                .cloned()
                .ok_or_else(|| "JSON number token accounting failed".to_string())?,
        )),
        ParsedJson::String(s) => Ok(Json::String(s.clone())),
        ParsedJson::Array(arr) => Ok(Json::Array(
            arr.iter()
                .map(|value| convert_parsed_value(value, number_tokens))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        ParsedJson::Object(map) => {
            let mut m = JsonMap::new();
            for (k, v) in map {
                m.insert(k.clone(), convert_parsed_value(v, number_tokens)?);
            }
            Ok(Json::Object(m))
        }
    }
}

fn convert_to_serde_value(j: &Json) -> serde_json::Value {
    match j {
        Json::Null => serde_json::Value::Null,
        Json::Bool(b) => serde_json::Value::Bool(*b),
        Json::Int(i) => serde_json::Value::Number((*i).into()),
        Json::Number(number) => number
            .raw
            .parse::<serde_json::Number>()
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        // NaN and infinity have no JSON representation, so `from_f64` returns
        // None and the value becomes null.
        Json::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
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

fn canonical_serde_value(j: &Json) -> serde_json::Value {
    match j {
        Json::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            let mut object = serde_json::Map::new();
            for key in keys {
                object.insert(key.clone(), canonical_serde_value(&map[key]));
            }
            serde_json::Value::Object(object)
        }
        Json::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_serde_value).collect())
        }
        _ => convert_to_serde_value(j),
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
        Json::Number(number) => json_number_value(&number.raw),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
        Json::Object(m) => {
            let mut map = crate::ordered::OrderedMap::new();
            for (k, v) in m {
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
            for (k, v) in map {
                // only string keys allowed in JSON
                if let Value::String(key_str) = k {
                    m.insert(key_str.clone(), value_to_json(v)?);
                } else {
                    return Err("map contains non-string key, cannot convert to JSON".to_string());
                }
            }
            Ok(Json::Object(m))
        }
        Value::Object(_) => match json_number_raw(v) {
            Some(raw) => Ok(Json::Number(JsonNumber { raw })),
            None => Err("unsupported value type for JSON conversion".to_string()),
        },
        _ => Err("unsupported value type for JSON conversion".to_string()),
    }
}

const MAX_JSON_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_JSON_LINE_BYTES: usize = 16 * 1024 * 1024;

fn json_duplicate_policy_from_value(value: *const Value) -> Result<JsonDuplicatePolicy, String> {
    let Some(value) = (unsafe { value.as_ref() }) else {
        return Err("JSON duplicate-key policy is required".to_string());
    };
    let bytes = match value {
        Value::Opaque(bytes) => bytes.as_ref(),
        Value::BoxedEnum(value) => value.bytes(),
        _ => return Err("JSON duplicate-key policy must be JsonDuplicatePolicy".to_string()),
    };
    if bytes.len() < std::mem::size_of::<i32>() {
        return Err("invalid JSON duplicate-key policy".to_string());
    }
    let mut raw = [0_u8; 4];
    raw.copy_from_slice(&bytes[..4]);
    match i32::from_ne_bytes(raw) {
        0 => Ok(JsonDuplicatePolicy::Reject),
        1 => Ok(JsonDuplicatePolicy::First),
        2 => Ok(JsonDuplicatePolicy::Last),
        _ => Err("invalid JSON duplicate-key policy".to_string()),
    }
}

fn parse_json_text(input: &[u8], policy: JsonDuplicatePolicy) -> *mut Value {
    if input.len() > MAX_JSON_INPUT_BYTES {
        return json_err("JSON input exceeds the 16 MiB limit");
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return json_err("JSON input must be valid UTF-8");
    };
    match Json::parse_with_policy(text, policy) {
        Ok(j) => json_value_ok(json_to_value(&j)),
        Err(e) => json_err(e),
    }
}

#[unsafe(no_mangle)]
/// Parses a NUL-terminated C string as JSON and returns a result value.
///
/// # Safety
///
/// `input` must be null or a valid NUL-terminated C string readable for the
/// duration of this call. Null preserves the result-error behavior.
pub unsafe extern "C" fn mux_json_parse(input: *const c_char) -> *mut Value {
    if input.is_null() {
        return json_err("null input");
    }
    let bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
    parse_json_text(bytes, JsonDuplicatePolicy::Reject)
}

#[unsafe(no_mangle)]
/// Parses a JSON string with an explicit duplicate-object-key policy.
///
/// # Safety
/// `input` must be null or a valid NUL-terminated C string. `policy` must be
/// null or point to a live `JsonDuplicatePolicy` enum value for this call.
pub unsafe extern "C" fn mux_json_parse_with_policy(
    input: *const c_char,
    policy: *const Value,
) -> *mut Value {
    if input.is_null() {
        return json_err("null input");
    }
    let policy = match json_duplicate_policy_from_value(policy) {
        Ok(policy) => policy,
        Err(error) => return json_err(error),
    };
    parse_json_text(unsafe { CStr::from_ptr(input) }.to_bytes(), policy)
}

/// Parse one bounded JSON document from a byte reader. The reader is consumed
/// through EOF (or `limit` bytes) and remains open for the caller to close.
///
/// # Safety
/// `reader` must be null or point to a live `std.io.Reader` value for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_json_parse_reader(reader: *const Value, limit: i64) -> *mut Value {
    unsafe { mux_json_parse_reader_with_policy(reader, limit, std::ptr::null()) }
}

#[unsafe(no_mangle)]
/// Parse one bounded JSON document from a byte reader with an explicit
/// duplicate-object-key policy. The reader remains open for the caller.
///
/// # Safety
/// `reader` must be null or point to a live `std.io.Reader`, and `policy` must
/// be null or point to a live `JsonDuplicatePolicy` value for this call.
pub unsafe extern "C" fn mux_json_parse_reader_with_policy(
    reader: *const Value,
    limit: i64,
    policy: *const Value,
) -> *mut Value {
    if !(0..=MAX_JSON_INPUT_BYTES as i64).contains(&limit) {
        return json_err("JSON reader limit must be between 0 and 16 MiB");
    }
    let duplicate_policy = if policy.is_null() {
        JsonDuplicatePolicy::Reject
    } else {
        match json_duplicate_policy_from_value(policy) {
            Ok(policy) => policy,
            Err(error) => return json_err(error),
        }
    };
    let read = unsafe { crate::stream::mux_io_reader_read_to_end(reader, limit) };
    let bytes = match unsafe { read.as_ref() } {
        Some(Value::Result(Ok(value))) => {
            if let Value::Bytes(bytes) = value.as_ref() {
                bytes.clone()
            } else {
                unsafe { crate::refcount::mux_rc_dec(read) };
                return json_err("reader returned a non-bytes value");
            }
        }
        Some(Value::Result(Err(_))) => return json_error_from_io_result(read, "JSON reader"),
        _ => {
            unsafe { crate::refcount::mux_rc_dec(read) };
            return json_err("reader returned an invalid result");
        }
    };
    unsafe { crate::refcount::mux_rc_dec(read) };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return json_err("JSON reader input must be valid UTF-8");
    };
    parse_json_text(text.as_bytes(), duplicate_policy)
}

// Incremental token reader -------------------------------------------------

/// A lexical JSON token. `kind` is a closed `JsonTokenKind` category; `text`
/// always contains the exact source spelling for the token. Scalar tokens
/// keep their already-validated Mux value.
#[derive(Clone, Copy)]
#[repr(i32)]
enum JsonTokenKind {
    StartObject,
    EndObject,
    StartArray,
    EndArray,
    Colon,
    Comma,
    String,
    Number,
    Bool,
    Null,
}

impl JsonTokenKind {
    fn value(self) -> Value {
        Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
    }
}

#[derive(Clone)]
struct JsonTokenEntry {
    kind: JsonTokenKind,
    text: String,
    value: Option<JsonTokenValue>,
    names: usize,
}

#[derive(Clone)]
enum JsonTokenValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
}

struct JsonTokenReaderEntry {
    tokens: Vec<JsonTokenEntry>,
    next: usize,
    names: usize,
}

static JSON_TOKEN_READERS: LazyLock<Mutex<HashMap<i64, JsonTokenReaderEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static JSON_TOKENS: LazyLock<Mutex<HashMap<i64, JsonTokenEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_JSON_TOKEN_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static JSON_TOKEN_READER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "JsonTokenReader",
        std::mem::size_of::<i64>(),
        Some(drop_json_token_reader as extern "C" fn(*mut c_void)),
        Some(copy_json_token_reader as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static JSON_TOKEN_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "JsonToken",
        std::mem::size_of::<i64>(),
        Some(drop_json_token as extern "C" fn(*mut c_void)),
        Some(copy_json_token as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

const MAX_JSON_TOKENS: usize = 1_000_000;

fn next_json_token_handle() -> i64 {
    let mut next = NEXT_JSON_TOKEN_HANDLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}

fn json_token_object(type_id: TypeId, handle: i64) -> *mut Value {
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

fn json_token_reader_id(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *JSON_TOKEN_READER_TYPE_ID {
        return Err("expected JsonTokenReader value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("JsonTokenReader value is invalid".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if JSON_TOKEN_READERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&id)
    {
        Ok(id)
    } else {
        Err("JsonTokenReader is closed".to_string())
    }
}

fn json_token_id(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *JSON_TOKEN_TYPE_ID {
        return Err("expected JsonToken value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("JsonToken value is invalid".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if JSON_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&id)
    {
        Ok(id)
    } else {
        Err("JsonToken is invalid".to_string())
    }
}

extern "C" fn copy_json_token_reader(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = JSON_TOKEN_READERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&id)
    {
        entry.names = entry.names.saturating_add(1);
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn copy_json_token(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = JSON_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&id)
    {
        // Token values are immutable, so the token itself is the only name
        // that needs tracking; its optional payload is cloned by accessors.
        entry.names = entry.names.saturating_add(1);
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_json_token_reader(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut readers = JSON_TOKEN_READERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = readers.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        readers.remove(&id);
    }
}

extern "C" fn drop_json_token(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut tokens = JSON_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = tokens.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        tokens.remove(&id);
    }
}

fn scan_json_tokens(input: &str) -> Result<Vec<JsonTokenEntry>, String> {
    // Parse once up front so callers never observe a plausible token stream
    // for malformed structure or duplicate object keys.
    Json::parse(input)?;
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut tokens = Vec::new();
    let push = |tokens: &mut Vec<JsonTokenEntry>, token: JsonTokenEntry| {
        if tokens.len() >= MAX_JSON_TOKENS {
            return Err("JSON token stream exceeds the one-million-token limit".to_string());
        }
        tokens.push(token);
        Ok(())
    };
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        let start = index;
        let (kind, value, end) = match bytes[index] {
            b'{' => (JsonTokenKind::StartObject, None, index + 1),
            b'}' => (JsonTokenKind::EndObject, None, index + 1),
            b'[' => (JsonTokenKind::StartArray, None, index + 1),
            b']' => (JsonTokenKind::EndArray, None, index + 1),
            b':' => (JsonTokenKind::Colon, None, index + 1),
            b',' => (JsonTokenKind::Comma, None, index + 1),
            b'"' => {
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        break;
                    }
                }
                if index > bytes.len() || bytes.get(index - 1) != Some(&b'"') {
                    return Err("unterminated JSON string token".to_string());
                }
                let raw = &input[start..index];
                let parsed = serde_json::from_str::<String>(raw)
                    .map_err(|error| format!("invalid JSON string token: {error}"))?;
                (
                    JsonTokenKind::String,
                    Some(JsonTokenValue::String(parsed)),
                    index,
                )
            }
            b'-' | b'0'..=b'9' => {
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'e' | b'E' | b'+' | b'-' | b'.')
                {
                    index += 1;
                }
                let raw = &input[start..index];
                let parsed = Json::parse(raw)?;
                let (Json::Int(_) | Json::Float(_) | Json::Number(_)) = parsed else {
                    return Err("invalid JSON number token".to_string());
                };
                (
                    JsonTokenKind::Number,
                    Some(JsonTokenValue::Number(input[start..index].to_string())),
                    index,
                )
            }
            b't' if bytes.get(index..index + 4) == Some(b"true") => (
                JsonTokenKind::Bool,
                Some(JsonTokenValue::Bool(true)),
                index + 4,
            ),
            b'f' if bytes.get(index..index + 5) == Some(b"false") => (
                JsonTokenKind::Bool,
                Some(JsonTokenValue::Bool(false)),
                index + 5,
            ),
            b'n' if bytes.get(index..index + 4) == Some(b"null") => {
                (JsonTokenKind::Null, Some(JsonTokenValue::Null), index + 4)
            }
            _ => return Err(format!("invalid JSON token at byte {index}")),
        };
        index = end;
        push(
            &mut tokens,
            JsonTokenEntry {
                kind,
                text: input[start..end].to_string(),
                value,
                names: 0,
            },
        )?;
    }
    Ok(tokens)
}

#[unsafe(no_mangle)]
/// Create an incremental token reader from a bounded byte reader.
///
/// The source is consumed once and validated as a complete JSON document;
/// `next()` then yields one lexical token at a time without reparsing it.
///
/// # Safety
/// `reader` must be null or point to a live `std.io.Reader` value for the
/// duration of this call.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn mux_json_token_reader_from_reader(
    reader: *const Value,
    limit: i64,
) -> *mut Value {
    if !(0..=MAX_JSON_INPUT_BYTES as i64).contains(&limit) {
        return json_err("JSON token reader limit must be between 0 and 16 MiB");
    }
    let read = unsafe { crate::stream::mux_io_reader_read_to_end(reader, limit) };
    let bytes = match unsafe { read.as_ref() } {
        Some(Value::Result(Ok(value))) => {
            let Value::Bytes(bytes) = value.as_ref() else {
                unsafe { mux_rc_dec(read) };
                return json_err("reader returned a non-bytes value");
            };
            bytes.clone()
        }
        Some(Value::Result(Err(_))) => return json_error_from_io_result(read, "JSON token reader"),
        _ => {
            unsafe { mux_rc_dec(read) };
            return json_err("reader returned an invalid result");
        }
    };
    unsafe { mux_rc_dec(read) };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return json_err("JSON token reader input must be valid UTF-8");
    };
    let tokens = match scan_json_tokens(text) {
        Ok(tokens) => tokens,
        Err(error) => return json_err(error),
    };
    let id = next_json_token_handle();
    JSON_TOKEN_READERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            id,
            JsonTokenReaderEntry {
                tokens,
                next: 0,
                names: 1,
            },
        );
    let value = json_token_object(*JSON_TOKEN_READER_TYPE_ID, id);
    if value.is_null() {
        JSON_TOKEN_READERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        return json_err("could not allocate JsonTokenReader");
    }
    let owned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    json_value_ok(owned)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_json_token_reader_new() -> *mut Value {
    let id = next_json_token_handle();
    JSON_TOKEN_READERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            id,
            JsonTokenReaderEntry {
                tokens: Vec::new(),
                next: 0,
                names: 1,
            },
        );
    let value = json_token_object(*JSON_TOKEN_READER_TYPE_ID, id);
    if value.is_null() {
        JSON_TOKEN_READERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }
    value
}

#[unsafe(no_mangle)]
/// # Safety
/// `reader` must be null or point to a live `std.io.Reader` value for the
/// duration of this call.
pub unsafe extern "C" fn mux_json_token_reader_next(reader: *const Value) -> *mut Value {
    let id = match json_token_reader_id(reader) {
        Ok(id) => id,
        Err(error) => return json_err(error),
    };
    let token = {
        let mut readers = JSON_TOKEN_READERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = readers.get_mut(&id) else {
            // The handle can be closed concurrently through an alias between
            // validation and this lock acquisition. Report that race as an
            // ordinary Mux error instead of panicking inside the runtime.
            return json_err("JsonTokenReader is closed");
        };
        let Some(token) = entry.tokens.get(entry.next).cloned() else {
            return json_value_ok(Value::Optional(None));
        };
        entry.next += 1;
        token
    };
    let token_id = next_json_token_handle();
    JSON_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(token_id, JsonTokenEntry { names: 1, ..token });
    let value = json_token_object(*JSON_TOKEN_TYPE_ID, token_id);
    if value.is_null() {
        JSON_TOKENS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token_id);
        return json_err("could not allocate JsonToken");
    }
    let owned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    json_value_ok(Value::Optional(Some(Box::new(owned))))
}

#[unsafe(no_mangle)]
/// # Safety
/// `reader` must be null or point to a live `JsonTokenReader` value. The
/// handle is invalidated by this call, including all aliases.
pub unsafe extern "C" fn mux_json_token_reader_close(reader: *mut Value) {
    if let Ok(id) = json_token_reader_id(reader) {
        JSON_TOKEN_READERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if !reader.is_null() {
            let ptr = unsafe { get_object_ptr(reader) };
            if !ptr.is_null() {
                unsafe { *ptr.cast::<i64>() = 0 };
            }
        }
    }
}

fn json_token_entry(token: *const Value) -> Result<JsonTokenEntry, String> {
    let id = json_token_id(token)?;
    let tokens = JSON_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokens
        .get(&id)
        .cloned()
        .ok_or_else(|| "JsonToken is invalid".to_string())
}

#[unsafe(no_mangle)]
/// # Safety
/// `token` must be null or point to a live `JsonToken` value for the duration
/// of this call.
pub unsafe extern "C" fn mux_json_token_kind(token: *const Value) -> *mut Value {
    match json_token_entry(token) {
        Ok(entry) => json_value_ok(entry.kind.value()),
        Err(error) => json_err(error),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `token` must be null or point to a live `JsonToken` value for the duration
/// of this call.
pub unsafe extern "C" fn mux_json_token_text(token: *const Value) -> *mut Value {
    match json_token_entry(token) {
        Ok(entry) => json_value_ok(Value::String(entry.text)),
        Err(error) => json_err(error),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `token` must be null or point to a live `JsonToken` value for the duration
/// of this call.
pub unsafe extern "C" fn mux_json_token_value(token: *const Value) -> *mut Value {
    match json_token_entry(token) {
        Ok(entry) => match entry.value {
            Some(JsonTokenValue::Null) => json_value_ok(Value::Unit),
            Some(JsonTokenValue::Bool(value)) => json_value_ok(Value::Bool(value)),
            Some(JsonTokenValue::Number(raw)) => json_value_ok(json_number_value(&raw)),
            Some(JsonTokenValue::String(value)) => json_value_ok(Value::String(value)),
            None => json_err("JSON structural token has no value"),
        },
        Err(error) => json_err(error),
    }
}

const MAX_JSON_LINES: usize = 1_000_000;
const MAX_JSON_LINES_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_JSON_INDENT: i64 = 64;

#[unsafe(no_mangle)]
/// Parse newline-delimited JSON into a list of JSON values.
///
/// Blank lines are ignored. Each non-blank line must contain exactly one JSON
/// value; errors include the one-based line number.
///
/// # Safety
/// `input` must be null or a valid NUL-terminated C string readable for the
/// duration of this call.
pub unsafe extern "C" fn mux_json_parse_lines(input: *const c_char) -> *mut Value {
    if input.is_null() {
        return json_err("null input");
    }
    unsafe { mux_json_parse_lines_with_policy(input, std::ptr::null()) }
}

#[unsafe(no_mangle)]
/// Parse newline-delimited JSON with an explicit duplicate-object-key policy.
/// Blank lines are ignored and each non-blank line must contain one value.
///
/// # Safety
/// `input` must be null or a valid NUL-terminated C string. `policy` must be
/// null or point to a live `JsonDuplicatePolicy` value for this call.
pub unsafe extern "C" fn mux_json_parse_lines_with_policy(
    input: *const c_char,
    policy: *const Value,
) -> *mut Value {
    if input.is_null() {
        return json_err("null input");
    }
    let duplicate_policy = if policy.is_null() {
        JsonDuplicatePolicy::Reject
    } else {
        match json_duplicate_policy_from_value(policy) {
            Ok(policy) => policy,
            Err(error) => return json_err(error),
        }
    };
    let bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
    if bytes.len() > MAX_JSON_INPUT_BYTES {
        return json_err("JSON Lines input exceeds the 16 MiB limit");
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return json_err("JSON Lines input must be valid UTF-8");
    };
    let mut values = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if values.len() >= MAX_JSON_LINES {
            return json_err("JSON Lines input exceeds the one-million-line limit");
        }
        let parsed = match Json::parse_with_policy(line, duplicate_policy) {
            Ok(value) => value,
            Err(error) => return json_err(format!("JSON line {}: {error}", line_number + 1)),
        };
        values.push(json_to_value(&parsed));
    }
    json_value_ok(Value::List(values))
}

#[unsafe(no_mangle)]
/// Serialize a list of JSON values as newline-delimited JSON.
///
/// # Safety
/// `values` must be null or point to a live Mux list whose elements remain
/// readable for the duration of this call.
pub unsafe extern "C" fn mux_json_stringify_lines(values: *const Value) -> *mut Value {
    let Some(Value::List(values)) = values.as_ref() else {
        return json_err("JSON Lines values must be a list");
    };
    if values.len() > MAX_JSON_LINES {
        return json_err("JSON Lines input exceeds the one-million-line limit");
    }
    let mut output = String::new();
    for (index, value) in values.iter().enumerate() {
        let json = match value_to_json(value) {
            Ok(json) => json,
            Err(error) => return json_err(format!("JSON Lines value {}: {error}", index + 1)),
        };
        let line = json.stringify(None);
        if line.len() > MAX_JSON_LINE_BYTES {
            return json_err(format!("JSON line {} exceeds the 16 MiB limit", index + 1));
        }
        let next_size = output
            .len()
            .checked_add(line.len() + 1)
            .ok_or_else(|| "JSON Lines output is too large".to_string());
        let Ok(next_size) = next_size else {
            return json_err("JSON Lines output exceeds the 16 MiB limit");
        };
        if next_size > MAX_JSON_LINES_OUTPUT_BYTES {
            return json_err("JSON Lines output exceeds the 16 MiB limit");
        }
        output.push_str(&line);
        output.push('\n');
    }
    json_value_ok(Value::String(output))
}

#[unsafe(no_mangle)]
/// Serializes a value as JSON with an optional indentation value.
///
/// # Safety
///
/// `val` may be null, which returns a result error; otherwise it must point to
/// a live runtime `Value`. A non-null `indent_opt` must likewise point to a
/// live runtime `Value`. Both are borrowed for the duration of this call.
pub unsafe extern "C" fn mux_json_stringify(
    val: *const Value,
    indent_opt: *mut Value,
) -> *mut Value {
    if val.is_null() {
        return json_err("null input");
    }
    let v = unsafe { &*val };
    let indent = if indent_opt.is_null() {
        None
    } else {
        unsafe {
            match &*indent_opt {
                Value::Optional(None) => None,
                Value::Optional(Some(boxed)) => match boxed.as_ref() {
                    Value::Int(indent) if (0..=MAX_JSON_INDENT).contains(indent) => {
                        Some(*indent as usize)
                    }
                    Value::Int(_) => {
                        return json_err(format!(
                            "JSON indentation must be between 0 and {MAX_JSON_INDENT} spaces"
                        ));
                    }
                    _ => return json_err("JSON indentation must be an optional int"),
                },
                _ => return json_err("JSON indentation must be an optional int"),
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
        Err(e) => json_err(e),
    }
}

/// Serialize one JSON value and write it to a byte writer. The writer remains
/// open and must be flushed or closed by the caller.
///
/// # Safety
/// `val`, `writer`, and `indent_opt` must be null or point to live values for
/// the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_json_stringify_to(
    val: *const Value,
    writer: *const Value,
    indent_opt: *mut Value,
) -> *mut Value {
    let rendered = unsafe { mux_json_stringify(val, indent_opt) };
    let text = match unsafe { rendered.as_ref() } {
        Some(Value::Result(Ok(value))) => {
            if let Value::String(text) = value.as_ref() {
                text.clone()
            } else {
                unsafe { crate::refcount::mux_rc_dec(rendered) };
                return json_err("JSON serializer returned a non-string value");
            }
        }
        Some(Value::Result(Err(_))) => return rendered,
        _ => {
            unsafe { crate::refcount::mux_rc_dec(rendered) };
            return json_err("JSON serializer returned an invalid result");
        }
    };
    let bytes = Value::Bytes(text.into_bytes());
    let written = unsafe { crate::stream::mux_io_writer_write_all(writer, &bytes) };
    let write_failed = matches!(unsafe { written.as_ref() }, Some(Value::Result(Err(_))));
    unsafe { crate::refcount::mux_rc_dec(rendered) };
    if write_failed {
        return json_error_from_io_result(written, "JSON writer");
    }
    unsafe { crate::refcount::mux_rc_dec(written) };
    json_value_ok(Value::Unit)
}

#[unsafe(no_mangle)]
/// Converts a map value to a JSON object result.
///
/// # Safety
///
/// `val` may be null, which returns a result error; otherwise it must point to
/// a live runtime `Value` and remains borrowed for this call.
pub unsafe extern "C" fn mux_json_from_map(val: *const Value) -> *mut Value {
    if val.is_null() {
        return json_err("null input");
    }
    let v = unsafe { &*val };
    let Value::Map(map) = v else {
        return json_err("value is not a map");
    };
    // Convert each value to Json to validate
    let mut jmap = JsonMap::new();
    for (k, vv) in map {
        if let Value::String(key_str) = k {
            match value_to_json(vv) {
                Ok(jv) => {
                    jmap.insert(key_str.clone(), jv);
                }
                Err(e) => return json_err(e),
            }
        } else {
            return json_err("map contains non-string key, cannot convert to JSON");
        }
    }
    let j = Json::Object(jmap);
    let v = json_to_value(&j);
    // Wrap directly to avoid leaking the intermediate allocation (see
    // mux_json_parse).
    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(v))))
}

#[unsafe(no_mangle)]
/// Converts a JSON object value to a Mux map result.
///
/// # Safety
///
/// `val` may be null, which returns a result error; otherwise it must point to
/// a live runtime `Value` and remains borrowed for this call.
pub unsafe extern "C" fn mux_json_to_map(val: *const Value) -> *mut Value {
    if val.is_null() {
        return json_err("null input");
    }
    let v = unsafe { &*val };
    // A Mux `Json` response may carry the non-JSON `body_bytes` field so HTTP
    // callers can retain binary payloads. Converting an object to a map is a
    // structural operation, not serialization, so preserve those values
    // instead of rejecting the whole response while walking it.
    if let Value::Map(map) = v {
        if map.keys().any(|key| !matches!(key, Value::String(_))) {
            return json_err("map contains non-string key");
        }
        return crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(v.clone()))));
    }
    match value_to_json(v) {
        Ok(Json::Object(m)) => {
            let mv = json_to_value(&Json::Object(m));
            // Wrap directly to avoid leaking the intermediate allocation (see
            // mux_json_parse).
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(mv))))
        }
        Ok(_) => json_err("json value is not an object"),
        Err(e) => json_err(e),
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
/// Each accessor accepts a null pointer as an ordinary wrong-kind result. A
/// non-null `val` must point to a live runtime `Value` for the duration of the
/// call. The argument is borrowed, and each returned result is a new owned
/// value released with `mux_rc_dec`.
/// The kind of a JSON value, for an error that says what was actually there.
///
/// "expected an int" alone leaves the reader guessing whether the field was a
/// string, absent, or something else entirely - which is the information a
/// bare `none` used to throw away.
fn json_kind(value: &Value) -> &'static str {
    if json_number_raw(value).is_some() {
        return "a number";
    }
    match value {
        Value::Unit => "null",
        Value::Bool(_) => "a bool",
        Value::Int(_) => "an int",
        Value::Float(_) => "a float",
        Value::String(_) => "a string",
        Value::List(_) => "an array",
        Value::Map(_) => "an object",
        _ => "an unsupported value",
    }
}

/// Read a typed value out of a `Json`, reporting what was there when it is a
/// different kind.
///
/// A `result` rather than an `optional`: "not an int" is worth saying WHY, and
/// a caller that does not care can ignore the message. The wrong kind is still
/// ordinary - it is a failure of expectation, not of the runtime - so it is a
/// value to handle, never a panic.
fn json_accessor<F>(val: *const Value, expected: &str, f: F) -> *mut Value
where
    F: FnOnce(&Value) -> Option<Value>,
{
    if val.is_null() {
        return json_kind_error(expected, "nothing");
    }
    let value = unsafe { &*val };
    match f(value) {
        // Wrap directly rather than through mux_result_ok_value, which clones
        // its argument without consuming it and would leak the intermediate
        // allocation (see mux_json_parse).
        Some(inner) => crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(inner)))),
        None => json_kind_error(expected, json_kind(value)),
    }
}

fn json_kind_error(expected: &str, found: &str) -> *mut Value {
    json_err(format!("expected {expected}, found {found}"))
}

#[unsafe(no_mangle)]
/// Reads a JSON string as a result value.
///
/// # Safety
///
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_string(val: *const Value) -> *mut Value {
    json_accessor(val, "a string", |v| match v {
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
#[unsafe(no_mangle)]
/// Reads an integral JSON number as a result value.
///
/// # Safety
///
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_int(val: *const Value) -> *mut Value {
    json_accessor(val, "an int", |v| match v {
        Value::Int(i) => Some(Value::Int(*i)),
        Value::Object(_) => json_number_raw(v).and_then(|raw| {
            if let Ok(value) = raw.parse::<i64>() {
                return Some(Value::Int(value));
            }
            let value = raw.parse::<f64>().ok()?;
            let in_range = value >= (i64::MIN as f64) && value < (i64::MAX as f64);
            (value.fract() == 0.0 && value.is_finite() && in_range)
                .then_some(Value::Int(value as i64))
        }),
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
#[unsafe(no_mangle)]
/// Reads a JSON number as a floating-point result value.
///
/// # Safety
///
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_float(val: *const Value) -> *mut Value {
    json_accessor(val, "a float", |v| match v {
        Value::Float(f) => Some(Value::Float(*f)),
        Value::Int(i) => Some(Value::Float(ordered_float::OrderedFloat(*i as f64))),
        Value::Object(_) => json_number_raw(v)
            .and_then(|raw| raw.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .map(|value| Value::Float(ordered_float::OrderedFloat(value))),
        _ => None,
    })
}

#[unsafe(no_mangle)]
/// Returns a lossless number handle when `val` is any JSON number.
///
/// # Safety
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_number(val: *const Value) -> *mut Value {
    json_accessor(val, "a number", |value| {
        if let Some(raw) = json_number_raw(value) {
            return Some(json_number_value(&raw));
        }
        match value {
            Value::Int(value) => Some(json_number_value(&value.to_string())),
            Value::Float(value) if value.into_inner().is_finite() => {
                Some(json_number_value(&value.into_inner().to_string()))
            }
            _ => None,
        }
    })
}

#[unsafe(no_mangle)]
/// Returns the original JSON number token.
///
/// # Safety
/// `val` must be a live `JsonNumber` value for the duration of this call.
pub unsafe extern "C" fn mux_json_number_to_string(val: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(json_number_raw(val).unwrap_or_default()))
}

#[unsafe(no_mangle)]
/// Reads a lossless JSON number as an exact or integral Mux integer.
///
/// # Safety
/// `val` must be a live `JsonNumber` value for the duration of this call.
pub unsafe extern "C" fn mux_json_number_as_int(val: *const Value) -> *mut Value {
    let Some(raw) = json_number_raw(val) else {
        return json_err("expected JsonNumber value");
    };
    let parsed = raw.parse::<i64>().or_else(|_| {
        let value = raw.parse::<f64>().map_err(|_| ())?;
        let in_range = value >= (i64::MIN as f64) && value < (i64::MAX as f64);
        (value.fract() == 0.0 && value.is_finite() && in_range)
            .then_some(value as i64)
            .ok_or(())
    });
    match parsed {
        Ok(value) => json_value_ok(Value::Int(value)),
        Err(()) => json_err("JSON number is not an integer in the Mux int range"),
    }
}

#[unsafe(no_mangle)]
/// Reads a lossless JSON number as a finite Mux float.
///
/// # Safety
/// `val` must be a live `JsonNumber` value for the duration of this call.
pub unsafe extern "C" fn mux_json_number_as_float(val: *const Value) -> *mut Value {
    let Some(raw) = json_number_raw(val) else {
        return json_err("expected JsonNumber value");
    };
    match raw.parse::<f64>().ok().filter(|value| value.is_finite()) {
        Some(value) => json_value_ok(Value::Float(ordered_float::OrderedFloat(value))),
        None => json_err("JSON number cannot be represented as a finite float"),
    }
}

#[unsafe(no_mangle)]
/// Reads a JSON boolean as a result value.
///
/// # Safety
///
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_bool(val: *const Value) -> *mut Value {
    json_accessor(val, "a bool", |v| match v {
        Value::Bool(b) => Some(Value::Bool(*b)),
        _ => None,
    })
}

#[unsafe(no_mangle)]
/// Reads a JSON array as a result value.
///
/// # Safety
///
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_list(val: *const Value) -> *mut Value {
    json_accessor(val, "an array", |v| match v {
        Value::List(items) => Some(Value::List(items.clone())),
        _ => None,
    })
}

/// The object as a map. `json.to_map` does the same thing as a free function
/// returning a `result`; this is the method form, and `none` rather than an
/// error message for a non-object.
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
/// Reads a JSON object as a result value.
///
/// # Safety
///
/// `val` may be null; otherwise it must point to a live runtime `Value` for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_as_map(val: *const Value) -> *mut Value {
    json_accessor(val, "an object", |v| match v {
        Value::Map(m) => Some(Value::Map(m.clone())),
        _ => None,
    })
}

/// The document as JSON text, always.
///
/// `stringify` is the configurable form: it takes an indent and returns a
/// `result`, so it cannot be `Stringable`. This is the total one every other
/// type has, which is what `print` and string concatenation need.
///
/// It can be total because a `Json` that exists is already serializable. Both
/// constructors return a `result` and reject what JSON cannot represent -
/// `json.from_map` refuses NaN and infinity, and `json.parse` cannot produce
/// them because the format has no syntax for them. So the failing branch here
/// is unreachable rather than merely unlikely.
///
/// It is still given a total answer rather than a panic: the runtime does not
/// abort a program over its own invariant, and a value that somehow reached
/// this point renders as `null` - which is at least valid JSON - with a
/// `debug_assert` so a test build fails where someone can act on it.
#[unsafe(no_mangle)]
/// Returns a JSON value's serialized text.
///
/// # Safety
///
/// `val` may be null, which returns the JSON text `null`; otherwise it must
/// point to a live runtime `Value` for the duration of this call.
pub unsafe extern "C" fn mux_json_to_string(val: *const Value) -> *mut Value {
    if val.is_null() {
        return crate::refcount::mux_rc_alloc(Value::String("null".to_string()));
    }
    let text = if let Ok(json) = value_to_json(unsafe { &*val }) {
        json.stringify(None)
    } else {
        debug_assert!(false, "a Json value that cannot be serialized to JSON");
        "null".to_string()
    };
    crate::refcount::mux_rc_alloc(Value::String(text))
}

#[unsafe(no_mangle)]
/// Returns deterministic JSON text with recursively sorted object keys.
///
/// # Safety
/// `val` may be null; otherwise it must point to a live JSON-compatible value
/// for the duration of this call.
pub unsafe extern "C" fn mux_json_canonical(val: *const Value) -> *mut Value {
    if val.is_null() {
        return crate::refcount::mux_rc_alloc(Value::String("null".to_string()));
    }
    let text = value_to_json(unsafe { &*val })
        .map_or_else(|_| "null".to_string(), |json| json.canonical());
    crate::refcount::mux_rc_alloc(Value::String(text))
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
#[unsafe(no_mangle)]
/// Reads one JSON object field by C-string key.
///
/// # Safety
///
/// `val` may be null and `key` may be null, preserving the empty-optional
/// behavior. Non-null pointers must be valid live runtime values and a valid
/// NUL-terminated C string respectively for the duration of this call.
pub unsafe extern "C" fn mux_json_field(val: *const Value, key: *const c_char) -> *mut Value {
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
    // Optional, unlike the typed accessors: a key that is not there is not a
    // failure of expectation, it is the question being asked. That is what lets
    // an `optional<T>` field accept a missing key while a required one reports
    // it, and the two cases stay apart because a present `null` comes back as
    // `some(null)`.
    if val.is_null() {
        return crate::optional::mux_optional_none();
    }
    let found = match unsafe { &*val } {
        Value::Map(entries) => entries.get(&Value::String(name.clone())).cloned(),
        _ => None,
    };
    match found {
        Some(inner) => crate::refcount::mux_rc_alloc(Value::Optional(Some(Box::new(inner)))),
        None => crate::optional::mux_optional_none(),
    }
}

/// JSON null. `Value::Unit` is what `json_to_value` maps it to.
#[unsafe(no_mangle)]
/// Tests whether a value is JSON null.
///
/// # Safety
///
/// `val` may be null, which returns `false`; otherwise it must point to a live
/// runtime `Value` for the duration of this call.
pub unsafe extern "C" fn mux_json_is_null(val: *const Value) -> bool {
    !val.is_null() && matches!(unsafe { &*val }, Value::Unit)
}

fn json_ok() -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(Value::Unit))))
}

fn json_err(message: impl Into<String>) -> *mut Value {
    crate::std::json_result_err(message)
}

fn json_error_from_io_result(result: *mut Value, context: &str) -> *mut Value {
    let detail = unsafe { crate::result::mux_result_data(result) };
    let message = if detail.is_null() {
        context.to_string()
    } else {
        let text = unsafe { crate::std::mux_io_error_message(detail) };
        let value = unsafe { text.as_ref() };
        let message = match value {
            Some(Value::String(message)) => format!("{context}: {message}"),
            _ => context.to_string(),
        };
        unsafe { crate::refcount::mux_rc_dec(text) };
        message
    };
    unsafe { crate::refcount::mux_rc_dec(result) };
    json_err(message)
}

/// Insert or replace a JSON object field in place.
///
/// # Safety
/// `val`, `key`, and `value` must be null or valid live `Value` pointers for
/// the duration of the call. `val` must contain a JSON object and `key` a
/// string value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_json_set_field(
    val: *mut Value,
    key: *const Value,
    value: *const Value,
) -> *mut Value {
    if val.is_null() || key.is_null() || value.is_null() {
        return json_err("json.set_field received a null argument");
    }
    let Value::String(name) = (unsafe { &*key }) else {
        return json_err("json field name must be a string");
    };
    if value_to_json(unsafe { &*value }).is_err() {
        return json_err("value is not representable as JSON");
    }
    let Value::Map(entries) = (unsafe { &mut *val }) else {
        return json_err("json.set_field requires an object");
    };
    entries.insert(Value::String(name.clone()), unsafe { (*value).clone() });
    json_ok()
}

/// Append one value to a JSON array in place.
///
/// # Safety
/// `val` and `value` must be valid live `Value` pointers for the duration of
/// the call. `val` must contain an array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_json_push(val: *mut Value, value: *const Value) -> *mut Value {
    if val.is_null() || value.is_null() {
        return json_err("json.push received a null argument");
    }
    if value_to_json(unsafe { &*value }).is_err() {
        return json_err("value is not representable as JSON");
    }
    let Value::List(items) = (unsafe { &mut *val }) else {
        return json_err("json.push requires an array");
    };
    items.push(unsafe { (*value).clone() });
    json_ok()
}

fn decode_pointer_text(text: &str) -> Result<Vec<String>, String> {
    if text.len() > MAX_JSON_INPUT_BYTES {
        return Err("JSON Pointer exceeds the 16 MiB input limit".to_string());
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if !text.starts_with('/') {
        return Err("json pointer must be empty or start with '/'".to_string());
    }
    let mut tokens = Vec::new();
    for raw in text[1..].split('/') {
        if tokens.len() >= MAX_JSON_TOKENS {
            return Err("JSON Pointer exceeds the one-million-token limit".to_string());
        }
        let mut token = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(ch) = chars.next() {
            if ch != '~' {
                token.push(ch);
                continue;
            }
            match chars.next() {
                Some('0') => token.push('~'),
                Some('1') => token.push('/'),
                _ => return Err("json pointer contains an invalid '~' escape".to_string()),
            }
        }
        tokens.push(token);
    }
    Ok(tokens)
}

fn decode_pointer_tokens(pointer: *const c_char) -> Result<Vec<String>, String> {
    if pointer.is_null() {
        return Err("json pointer must not be null".to_string());
    }
    let text = unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|_| "json pointer must be valid UTF-8".to_string())?;
    decode_pointer_text(text)
}

fn pointer_index(token: &str, length: usize, allow_append: bool) -> Result<Option<usize>, String> {
    if allow_append && token == "-" {
        return Ok(None);
    }
    if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
        return Err("json pointer array index is not canonical".to_string());
    }
    let index = token
        .parse::<usize>()
        .map_err(|_| "json pointer array index is invalid".to_string())?;
    if index >= length {
        return Err("json pointer array index is out of bounds".to_string());
    }
    Ok(Some(index))
}

fn pointer_get(value: &Value, tokens: &[String]) -> Result<Value, String> {
    let Some((token, rest)) = tokens.split_first() else {
        return Ok(value.clone());
    };
    match value {
        Value::Map(entries) => entries
            .get(&Value::String(token.clone()))
            .ok_or_else(|| format!("json pointer object field '{token}' is missing"))
            .and_then(|child| pointer_get(child, rest)),
        Value::List(items) => {
            let Some(index) = pointer_index(token, items.len(), false)? else {
                return Err("JSON Pointer append is not valid for reads".to_string());
            };
            pointer_get(&items[index], rest)
        }
        _ => Err("json pointer traverses a non-container value".to_string()),
    }
}

fn pointer_set(value: &mut Value, tokens: &[String], replacement: &Value) -> Result<(), String> {
    let Some((token, rest)) = tokens.split_first() else {
        *value = replacement.clone();
        return Ok(());
    };
    match value {
        Value::Map(entries) => {
            if rest.is_empty() {
                entries.insert(Value::String(token.clone()), replacement.clone());
                Ok(())
            } else {
                let child = entries
                    .get_mut(&Value::String(token.clone()))
                    .ok_or_else(|| format!("json pointer object field '{token}' is missing"))?;
                pointer_set(child, rest, replacement)
            }
        }
        Value::List(items) => {
            let index = pointer_index(token, items.len(), rest.is_empty())?;
            match index {
                None => items.push(replacement.clone()),
                Some(index) => {
                    if rest.is_empty() {
                        items[index] = replacement.clone();
                    } else {
                        pointer_set(&mut items[index], rest, replacement)?;
                    }
                }
            }
            Ok(())
        }
        _ => Err("json pointer traverses a non-container value".to_string()),
    }
}

fn pointer_add(value: &mut Value, tokens: &[String], addition: &Value) -> Result<(), String> {
    let Some((token, rest)) = tokens.split_first() else {
        *value = addition.clone();
        return Ok(());
    };
    if rest.is_empty() {
        return match value {
            Value::Map(entries) => {
                entries.insert(Value::String(token.clone()), addition.clone());
                Ok(())
            }
            Value::List(items) => {
                if token == "-" {
                    items.push(addition.clone());
                    return Ok(());
                }
                let index = token
                    .parse::<usize>()
                    .map_err(|_| "json patch array index is invalid".to_string())?;
                if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
                    return Err("json patch array index is not canonical".to_string());
                }
                if index > items.len() {
                    return Err("json patch array index is out of bounds".to_string());
                }
                items.insert(index, addition.clone());
                Ok(())
            }
            _ => Err("json patch path traverses a non-container value".to_string()),
        };
    }
    match value {
        Value::Map(entries) => {
            let child = entries
                .get_mut(&Value::String(token.clone()))
                .ok_or_else(|| format!("json patch object field '{token}' is missing"))?;
            pointer_add(child, rest, addition)
        }
        Value::List(items) => {
            let Some(index) = pointer_index(token, items.len(), false)? else {
                return Err("JSON Patch append is not valid for nested paths".to_string());
            };
            pointer_add(&mut items[index], rest, addition)
        }
        _ => Err("json patch path traverses a non-container value".to_string()),
    }
}

fn apply_json_patch_operation(document: &mut Value, operation: &Value) -> Result<(), String> {
    let Value::Map(fields) = operation else {
        return Err("each JSON Patch operation must be an object".to_string());
    };
    let field = |name: &str| {
        fields
            .get(&Value::String(name.to_string()))
            .ok_or_else(|| format!("JSON Patch operation is missing '{name}'"))
    };
    let Value::String(op) = field("op")? else {
        return Err("JSON Patch 'op' must be a string".to_string());
    };
    let Value::String(path) = field("path")? else {
        return Err("JSON Patch 'path' must be a string".to_string());
    };
    let tokens = decode_pointer_text(path)?;
    match op.as_str() {
        "add" => pointer_add(document, &tokens, field("value")?),
        "remove" => pointer_remove(document, &tokens).map(|_| ()),
        "replace" => {
            pointer_get(document, &tokens)?;
            pointer_set(document, &tokens, field("value")?)
        }
        "copy" => {
            let Value::String(from) = field("from")? else {
                return Err("JSON Patch 'from' must be a string".to_string());
            };
            let source = pointer_get(document, &decode_pointer_text(from)?)?;
            pointer_add(document, &tokens, &source)
        }
        "move" => {
            let Value::String(from) = field("from")? else {
                return Err("JSON Patch 'from' must be a string".to_string());
            };
            let from_tokens = decode_pointer_text(from)?;
            if from_tokens.len() < tokens.len() && tokens.starts_with(&from_tokens) {
                return Err("JSON Patch cannot move a value into its own child".to_string());
            }
            let source = pointer_get(document, &from_tokens)?;
            pointer_remove(document, &from_tokens)?;
            pointer_add(document, &tokens, &source)
        }
        "test" => {
            if pointer_get(document, &tokens)? != *field("value")? {
                return Err("JSON Patch test operation failed".to_string());
            }
            Ok(())
        }
        _ => Err(format!("unsupported JSON Patch operation '{op}'")),
    }
}

fn pointer_remove(value: &mut Value, tokens: &[String]) -> Result<Value, String> {
    let Some((token, rest)) = tokens.split_first() else {
        return Err("removing the JSON document root is not supported".to_string());
    };
    match value {
        Value::Map(entries) => {
            if rest.is_empty() {
                entries
                    .remove(&Value::String(token.clone()))
                    .ok_or_else(|| format!("json pointer object field '{token}' is missing"))
            } else {
                let child = entries
                    .get_mut(&Value::String(token.clone()))
                    .ok_or_else(|| format!("json pointer object field '{token}' is missing"))?;
                pointer_remove(child, rest)
            }
        }
        Value::List(items) => {
            let Some(index) = pointer_index(token, items.len(), false)? else {
                return Err("JSON Pointer append is not valid for removals".to_string());
            };
            if rest.is_empty() {
                Ok(items.remove(index))
            } else {
                pointer_remove(&mut items[index], rest)
            }
        }
        _ => Err("json pointer traverses a non-container value".to_string()),
    }
}

fn apply_merge_patch(target: &mut Value, patch: &Value) {
    let Value::Map(patch_entries) = patch else {
        *target = patch.clone();
        return;
    };
    if !matches!(target, Value::Map(_)) {
        *target = Value::Map(crate::ordered::OrderedMap::new());
    }
    let Value::Map(target_entries) = target else {
        return;
    };
    for (key, patch_value) in patch_entries {
        let Value::String(_) = key else {
            continue;
        };
        if matches!(patch_value, Value::Unit) {
            target_entries.remove(key);
        } else if let Some(existing) = target_entries.get_mut(key) {
            apply_merge_patch(existing, patch_value);
        } else {
            target_entries.insert(key.clone(), patch_value.clone());
        }
    }
}

fn json_value_ok(value: Value) -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

#[unsafe(no_mangle)]
/// Resolve an RFC 6901 JSON Pointer and return a cloned value.
///
/// # Safety
/// `val` and `pointer` may be null; non-null arguments must remain valid for
/// the duration of this call.
pub unsafe extern "C" fn mux_json_at_pointer(
    val: *const Value,
    pointer: *const c_char,
) -> *mut Value {
    let Some(value) = val.as_ref() else {
        return json_err("json pointer received a null document");
    };
    match decode_pointer_tokens(pointer).and_then(|tokens| pointer_get(value, &tokens)) {
        Ok(found) => json_value_ok(found),
        Err(error) => json_err(error),
    }
}

#[unsafe(no_mangle)]
/// Replace or append through an RFC 6901 JSON Pointer in place.
///
/// # Safety
/// `val`, `pointer`, and `replacement` must be valid for the duration of this
/// call. The document is changed only after the replacement is JSON-valid.
pub unsafe extern "C" fn mux_json_set_pointer(
    val: *mut Value,
    pointer: *const c_char,
    replacement: *const Value,
) -> *mut Value {
    let Some(document) = val.as_mut() else {
        return json_err("json pointer received a null document");
    };
    let Some(replacement) = replacement.as_ref() else {
        return json_err("json pointer received a null replacement");
    };
    if value_to_json(replacement).is_err() {
        return json_err("value is not representable as JSON");
    }
    match decode_pointer_tokens(pointer)
        .and_then(|tokens| pointer_set(document, &tokens, replacement))
    {
        Ok(()) => json_ok(),
        Err(error) => json_err(error),
    }
}

#[unsafe(no_mangle)]
/// Remove and return the value selected by an RFC 6901 JSON Pointer.
///
/// # Safety
/// `val` and `pointer` must be valid for the duration of this call.
pub unsafe extern "C" fn mux_json_remove_pointer(
    val: *mut Value,
    pointer: *const c_char,
) -> *mut Value {
    let Some(document) = val.as_mut() else {
        return json_err("json pointer received a null document");
    };
    match decode_pointer_tokens(pointer).and_then(|tokens| pointer_remove(document, &tokens)) {
        Ok(removed) => json_value_ok(removed),
        Err(error) => json_err(error),
    }
}

#[unsafe(no_mangle)]
/// Apply an RFC 7396 JSON Merge Patch to a document in place.
///
/// # Safety
/// `val` and `patch` must be valid live values for the duration of this call.
pub unsafe extern "C" fn mux_json_merge_patch(val: *mut Value, patch: *const Value) -> *mut Value {
    let Some(document) = val.as_mut() else {
        return json_err("json merge patch received a null document");
    };
    let Some(patch) = patch.as_ref() else {
        return json_err("json merge patch received a null patch");
    };
    if value_to_json(patch).is_err() {
        return json_err("patch is not representable as JSON");
    }
    apply_merge_patch(document, patch);
    json_ok()
}

#[unsafe(no_mangle)]
/// Apply an RFC 6902 JSON Patch document atomically.
///
/// The patch is a list of operation objects with `op` and `path` fields and
/// the required `value`/`from` fields for the selected operation. The target
/// is only replaced after every operation succeeds.
///
/// # Safety
/// `val` and `patch` must be valid live values for the duration of this call.
pub unsafe extern "C" fn mux_json_apply_patch(val: *mut Value, patch: *const Value) -> *mut Value {
    let Some(document) = val.as_ref() else {
        return json_err("json patch received a null document");
    };
    let Some(Value::List(operations)) = patch.as_ref() else {
        return json_err("json patch must be a list of operations");
    };
    if operations
        .iter()
        .any(|operation| value_to_json(operation).is_err())
    {
        return json_err("patch contains a value that is not representable as JSON");
    }
    let mut candidate = document.clone();
    for operation in operations {
        if let Err(error) = apply_json_patch_operation(&mut candidate, operation) {
            return json_err(error);
        }
    }
    *unsafe { &mut *val } = candidate;
    json_ok()
}
