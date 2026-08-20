use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::c_char;

use ordered_float;

use crate::refcount::mux_rc_alloc;
use crate::Value;
#[derive(Clone, Debug)]
pub struct MuxString(pub String);

impl MuxString {
    pub fn to_int(&self) -> Result<i64, String> {
        self.0.parse().map_err(|_| "Invalid integer".to_string())
    }

    pub fn to_float(&self) -> Result<f64, String> {
        self.0.parse().map_err(|_| "Invalid float".to_string())
    }

    /// `true` or `false`, case-insensitively, and nothing else.
    ///
    /// Deliberately narrow. CSV has no types, so a bool column is whatever the
    /// writer spelled it - and accepting `1`, `yes` or `y` would mean guessing
    /// which convention a file follows, then being wrong for the file that uses
    /// `1` to mean the number one.
    pub fn to_bool(&self) -> Result<bool, String> {
        match self.0.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err("Invalid bool: expected true or false".to_string()),
        }
    }

    pub fn concat(&self, other: &MuxString) -> MuxString {
        MuxString(self.0.clone() + &other.0)
    }

    /// Length in CHARACTERS, not bytes.
    ///
    /// `String::len` is a byte count, so any non-ASCII character made this
    /// wrong - an accented letter counted 2, most CJK 3, an emoji 4. Mux treats
    /// a string as a sequence of characters, so every position-based operation
    /// has to agree on that, or `s[s.length() - 1]` is wrong for exactly the
    /// inputs nobody tests with.
    ///
    /// This is O(n) where the byte length was O(1). If that ever matters, cache
    /// a count alongside the string; do not go back to bytes.
    pub fn length(&self) -> i64 {
        self.0.chars().count() as i64
    }

    /// Lexicographic ordering, as negative / zero / positive like C's `strcmp`.
    ///
    /// Rust's `str` ordering is byte-wise, which for UTF-8 gives the same
    /// answer as comparing code points, so this is character ordering despite
    /// operating on bytes.
    pub fn compare(&self, other: &MuxString) -> i64 {
        match self.0.cmp(&other.0) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }

    pub fn hash(&self) -> i64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        self.0.hash(&mut hasher);
        hasher.finish() as i64
    }
}

impl fmt::Display for MuxString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Convert a Value to a C string (caller must free with mux_free_string).
///
/// # Safety
/// `v` must be a valid, non-null pointer to a `Value`. Does not take ownership of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_string_from_value(v: *mut Value) -> *mut c_char {
    if let Value::String(s) = unsafe { &*v } {
        match CString::new(s.clone()) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    } else {
        match CString::new("".to_string()) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }
}

/// # Safety
/// Borrows `v` and clones the string data. Does NOT take ownership of `v`.
/// Returns a new C string that caller must free with `mux_free_string`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_value_get_string(v: *mut Value) -> *mut c_char {
    unsafe { mux_string_from_value(v) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_bool(s: *const c_char) -> *mut Value {
    if s.is_null() {
        return mux_rc_alloc(Value::Result(Err(Box::new(Value::String(
            "null input".to_string(),
        )))));
    }
    let text = unsafe { CStr::from_ptr(s) }.to_string_lossy();
    match MuxString(text.to_string()).to_bool() {
        Ok(b) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Bool(b))))),
        Err(e) => mux_rc_alloc(Value::Result(Err(Box::new(Value::String(e))))),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_int(s: *const c_char) -> *mut Value {
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy();
    match MuxString(rust_str.to_string()).to_int() {
        Ok(i) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Int(i))))),
        Err(e) => mux_rc_alloc(Value::Result(Err(Box::new(Value::String(e))))),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_float(s: *const c_char) -> *mut Value {
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy();
    match MuxString(rust_str.to_string()).to_float() {
        Ok(f) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Float(
            ordered_float::OrderedFloat(f),
        ))))),
        Err(e) => mux_rc_alloc(Value::Result(Err(Box::new(Value::String(e))))),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_concat(a: *const c_char, b: *const c_char) -> *mut c_char {
    let a_str = unsafe { CStr::from_ptr(a).to_string_lossy() };
    let b_str = unsafe { CStr::from_ptr(b).to_string_lossy() };
    let result = MuxString(a_str.to_string()).concat(&MuxString(b_str.to_string()));
    match CString::new(result.0) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_length(s: *const c_char) -> i64 {
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy();
    MuxString(rust_str.to_string()).length()
}

/// Compare two strings lexicographically, returning negative / zero / positive.
///
/// The compiler's relational operators on `string` lower to this. Without it
/// they fell through to the numeric path, which unboxed the string POINTER as
/// an integer - so `<` and `>` compared addresses and every ordering answer was
/// meaningless, silently.
///
/// # Safety
///
/// `a` and `b` must each be either null or a valid pointer to a NUL-terminated
/// C string that stays alive for the call. Null is handled; any other invalid
/// pointer is undefined behaviour, which is why this is `unsafe` rather than a
/// safe function that happens to dereference raw pointers - the same reason
/// `mux_string_from_value` is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_string_compare(a: *const c_char, b: *const c_char) -> i64 {
    // Guarded rather than dereferenced blind, matching `mux_string_equal`. A
    // null sorts before any real string, and two nulls are equal, so the result
    // is still a total order and a caller cannot crash the program by passing
    // one.
    if a.is_null() || b.is_null() {
        return match (a.is_null(), b.is_null()) {
            (true, true) => 0,
            (true, false) => -1,
            _ => 1,
        };
    }
    let left = unsafe { CStr::from_ptr(a) }.to_string_lossy();
    let right = unsafe { CStr::from_ptr(b) }.to_string_lossy();
    MuxString(left.to_string()).compare(&MuxString(right.to_string()))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_hash(s: *const c_char) -> i64 {
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy();
    MuxString(rust_str.to_string()).hash()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_contains(haystack: *const Value, needle: *const Value) -> bool {
    unsafe {
        if let (Value::String(haystack_str), Value::String(needle_str)) = (&*haystack, &*needle) {
            haystack_str.contains(needle_str)
        } else {
            false
        }
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_contains_char(haystack: *const Value, needle: i64) -> bool {
    let haystack_str = unsafe {
        match &*haystack {
            Value::String(s) => s,
            _ => return false,
        }
    };
    let Some(ch) = char::from_u32(needle as u32) else {
        return false;
    };
    haystack_str.contains(ch)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_string(s: *const c_char) -> *mut c_char {
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy();
    match std::ffi::CString::new(rust_str.as_ref()) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Create a new reference-counted Value::String from a C string.
/// Borrows the input pointer (does not free it). Caller must manage the input's lifetime.
///
/// # Safety
/// `s` must be a valid pointer or null. Does not take ownership of `s`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_new_string_from_cstr(s: *const c_char) -> *mut Value {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy().to_string();
    let value = Value::String(rust_str);
    mux_rc_alloc(value)
}

/// Create a new reference-counted Value::String from an owned C string.
/// Takes ownership of the input pointer and frees it after cloning the string.
/// This is used by codegen for primitive-to-string conversions like `int.to_string()`.
///
/// # Safety
/// `s` must be a valid pointer returned by a runtime function's `CString::into_raw()` call,
/// or null. This function takes ownership and will free the memory.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_new_string_from_owned_cstr(s: *mut c_char) -> *mut Value {
    if s.is_null() {
        return std::ptr::null_mut();
    }

    let rust_str = {
        let c_str = unsafe { CStr::from_ptr(s) };
        c_str.to_string_lossy().to_string()
    };

    // Free the input C string after copying its contents.
    // The c_str borrow is out of scope, so this deallocation is safe.
    unsafe {
        let _ = CString::from_raw(s);
    }

    let value = Value::String(rust_str);
    mux_rc_alloc(value)
}

/// Compare two C strings for equality
/// Returns 1 if equal, 0 if not equal
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_equal(a: *const c_char, b: *const c_char) -> i32 {
    if a.is_null() || b.is_null() {
        return if a == b { 1 } else { 0 };
    }
    unsafe {
        let a_str = CStr::from_ptr(a);
        let b_str = CStr::from_ptr(b);
        if a_str == b_str {
            1
        } else {
            0
        }
    }
}

/// Compare two C strings for inequality
/// Returns 1 if not equal, 0 if equal
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_not_equal(a: *const c_char, b: *const c_char) -> i32 {
    if mux_string_equal(a, b) == 1 {
        0
    } else {
        1
    }
}

/// Convert a string to a single character.
/// Returns Result<char, str>. Fails if the string is not exactly one Unicode character.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_char(s: *const c_char) -> *mut Value {
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy();
    let mut chars = rust_str.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Int(c as i64))))),
        _ => mux_rc_alloc(Value::Result(Err(Box::new(Value::String(
            "String must be exactly one character".to_string(),
        ))))),
    }
}

/// Convert a character to its integer value
/// Only works for digit characters '0'-'9'
/// Returns Result<int, str>
#[unsafe(no_mangle)]
pub extern "C" fn mux_char_to_int(c: i64) -> *mut Value {
    if let Some(ch) = char::from_u32(c as u32) {
        if ch.is_ascii_digit() {
            let digit = (ch as u8 - b'0') as i64;
            mux_rc_alloc(Value::Result(Ok(Box::new(Value::Int(digit)))))
        } else {
            mux_rc_alloc(Value::Result(Err(Box::new(Value::String(
                "Character is not a digit (0-9)".to_string(),
            )))))
        }
    } else {
        mux_rc_alloc(Value::Result(Err(Box::new(Value::String(
            "Invalid character".to_string(),
        )))))
    }
}

/// Convert a character (i64) to a string
#[unsafe(no_mangle)]
pub extern "C" fn mux_char_to_string(c: i64) -> *mut c_char {
    if let Some(ch) = char::from_u32(c as u32) {
        let s = ch.to_string();
        match CString::new(s) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    } else {
        match CString::new("") {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }
}

// String decomposition.
//
// A string could previously only be measured, parsed whole, compared and
// concatenated - there was no split, no indexing, no iteration. So a program
// could receive text (io.read_file hands back a whole file as one string) and
// had no way to take it apart (mux-compiler#389).
//
// Every position here is a CHARACTER position, matching `length`. Indexing by
// byte would make `s[s.length() - 1]` wrong for any non-ASCII string, which is
// a bug only someone else would find.

/// Character at `index`, as a `Value::Optional`, or `none` when out of range.
///
/// Negative indices count from the end, the same rule lists use. Borrows `s`
/// and returns an OWNED optional the caller releases with `mux_rc_dec`; a null
/// input yields `none` rather than being dereferenced.
fn char_at_index(s: &str, index: i64) -> Option<char> {
    let count = s.chars().count() as i64;
    let wrapped = if index < 0 { count + index } else { index };
    if wrapped < 0 || wrapped >= count {
        return None;
    }
    s.chars().nth(wrapped as usize)
}

/// Resolve a half-open slice range to character offsets.
///
/// Python's rules, which is what `xs[1:3]`, `xs[:3]`, `xs[2:]` and `xs[-2:]`
/// lead people to expect: negative counts from the end, out-of-range clamps
/// rather than failing, and a start past the end yields empty rather than
/// reversing.
fn slice_bounds(count: i64, start: i64, end: i64) -> (usize, usize) {
    let resolve = |i: i64| -> i64 {
        let wrapped = if i < 0 { count + i } else { i };
        wrapped.clamp(0, count)
    };
    let from = resolve(start);
    let to = resolve(end);
    if to <= from {
        (from as usize, from as usize)
    } else {
        (from as usize, to as usize)
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_char_at(s: *const c_char, index: i64) -> *mut Value {
    if s.is_null() {
        return crate::optional::mux_optional_none();
    }
    let text = unsafe { CStr::from_ptr(s) }.to_string_lossy();
    match char_at_index(&text, index) {
        Some(c) => {
            crate::refcount::mux_rc_alloc(Value::Optional(Some(Box::new(Value::Int(c as i64)))))
        }
        None => crate::optional::mux_optional_none(),
    }
}

/// Half-open character slice, `[start, end)`.
///
/// Borrows `s` and returns an OWNED C string the caller frees with
/// `mux_free_string`. A null input is treated as empty. Bounds are CHARACTER
/// positions: negative counts from the end, out of range clamps, and a start at
/// or past the end yields empty rather than reversing.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_slice(s: *const c_char, start: i64, end: i64) -> *mut c_char {
    let text = if s.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    };
    let count = text.chars().count() as i64;
    let (from, to) = slice_bounds(count, start, end);
    let out: String = text.chars().skip(from).take(to - from).collect();
    owned_cstr(out)
}

/// Split on a separator, as a `Value::List` of strings.
///
/// Borrows both and returns an OWNED list the caller releases with
/// `mux_rc_dec`. A null argument is treated as empty. An empty separator splits
/// into single characters, which is what makes this the inverse of joining a
/// character list.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_split(s: *const c_char, sep: *const c_char) -> *mut Value {
    let text = borrowed_str(s);
    let separator = borrowed_str(sep);

    let parts: Vec<Value> = if separator.is_empty() {
        text.chars().map(|c| Value::String(c.to_string())).collect()
    } else {
        text.split(separator.as_str())
            .map(|p| Value::String(p.to_string()))
            .collect()
    };
    crate::refcount::mux_rc_alloc(Value::List(parts))
}

/// The characters, as a `Value::List` of char codes.
///
/// Borrows `s` and returns an OWNED list the caller releases with
/// `mux_rc_dec`; a null input yields an empty list. This is what makes
/// `for char c in s` work without the loop needing a string case of its own.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_list(s: *const c_char) -> *mut Value {
    let text = borrowed_str(s);
    let chars: Vec<Value> = text.chars().map(|c| Value::Int(c as i64)).collect();
    crate::refcount::mux_rc_alloc(Value::List(chars))
}

/// The text with leading and trailing whitespace removed.
///
/// Borrows `s` and returns an OWNED C string the caller frees with
/// `mux_free_string`. A null input is treated as empty rather than
/// dereferenced. Trims Unicode whitespace, not only ASCII.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_trim(s: *const c_char) -> *mut c_char {
    owned_cstr(borrowed_str(s).trim().to_string())
}

/// The text uppercased.
///
/// Borrows `s` and returns an OWNED C string the caller frees with
/// `mux_free_string`. A null input is treated as empty. Uses full Unicode
/// case mapping, so the result may be LONGER than the input in characters -
/// the German sharp s uppercases to two letters.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_upper(s: *const c_char) -> *mut c_char {
    owned_cstr(borrowed_str(s).to_uppercase())
}

/// The text lowercased.
///
/// Borrows `s` and returns an OWNED C string the caller frees with
/// `mux_free_string`. A null input is treated as empty. Uses full Unicode case
/// mapping, so the result may differ in length from the input.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_to_lower(s: *const c_char) -> *mut c_char {
    owned_cstr(borrowed_str(s).to_lowercase())
}

/// Whether `s` begins with `prefix`.
///
/// Borrows both and allocates nothing. Either being null is treated as empty,
/// so an empty prefix is always a match. Compares by content, not by
/// normalization form: two strings that render alike but differ in code points
/// do not match.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_starts_with(s: *const c_char, prefix: *const c_char) -> bool {
    borrowed_str(s).starts_with(borrowed_str(prefix).as_str())
}

/// Whether `s` ends with `suffix`.
///
/// Borrows both and allocates nothing. Either being null is treated as empty,
/// so an empty suffix is always a match. Compares by content, not by
/// normalization form.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_ends_with(s: *const c_char, suffix: *const c_char) -> bool {
    borrowed_str(s).ends_with(borrowed_str(suffix).as_str())
}

/// First CHARACTER index of `needle`, or -1. Rust's `find` returns a byte
/// offset, which would disagree with every other position in this module.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_index_of(s: *const c_char, needle: *const c_char) -> i64 {
    let text = borrowed_str(s);
    let pat = borrowed_str(needle);
    match text.find(pat.as_str()) {
        Some(byte_offset) => text[..byte_offset].chars().count() as i64,
        None => -1,
    }
}

/// Every occurrence of `from` replaced with `to`.
///
/// Borrows all three and returns an OWNED C string the caller frees with
/// `mux_free_string`. A null argument is treated as empty. An empty `from`
/// returns the text unchanged rather than inserting `to` between every
/// character, which is never what a caller means.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_string_replace(
    s: *const c_char,
    from: *const c_char,
    to: *const c_char,
) -> *mut c_char {
    let text = borrowed_str(s);
    let needle = borrowed_str(from);
    // Replacing an empty pattern inserts between every character, which is
    // never what a caller means; return the text unchanged instead.
    if needle.is_empty() {
        return owned_cstr(text);
    }
    owned_cstr(text.replace(needle.as_str(), borrowed_str(to).as_str()))
}

/// Read a borrowed C string, treating null as empty.
fn borrowed_str(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

/// Hand back an owned C string the caller frees with `mux_free_string`.
fn owned_cstr(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        // An interior NUL cannot survive a C string; an empty result is the
        // total answer, and the runtime must not panic into a compiled program.
        Err(_) => CString::default().into_raw(),
    }
}
