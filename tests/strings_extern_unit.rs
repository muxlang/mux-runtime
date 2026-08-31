//! Unit tests for the C-ABI string/bool/boxing layer.

mod common;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use common::{assert_err, assert_ok, ok_int};
use mux_runtime::boxing::*;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_free_string, mux_string_value, mux_value_get_int};
use mux_runtime::string::*;
use mux_runtime::Value;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn read_cstr(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { mux_free_string(p) };
    s
}

#[test]
fn string_scalar_ops() {
    unsafe {
        assert_eq!(
            read_cstr(mux_string_concat(cs("foo").as_ptr(), cs("bar").as_ptr())),
            "foobar"
        );
        assert_eq!(mux_string_length(cs("hello").as_ptr()), 5);
        assert_eq!(
            mux_string_hash(cs("x").as_ptr()),
            mux_string_hash(cs("x").as_ptr())
        );
        assert_eq!(read_cstr(mux_string_to_string(cs("hi").as_ptr())), "hi");
    }
}

#[test]
fn string_parsing() {
    unsafe {
        assert_eq!(ok_int(mux_string_to_int(cs("42").as_ptr())), 42);
        assert_err(mux_string_to_int(cs("nope").as_ptr()));
        assert_ok(mux_string_to_float(cs("3.5").as_ptr()));
        assert_err(mux_string_to_float(cs("nope").as_ptr()));
    }
}

#[test]
fn string_equality() {
    unsafe {
        assert_eq!(mux_string_equal(cs("a").as_ptr(), cs("a").as_ptr()), 1);
        assert_eq!(mux_string_equal(cs("a").as_ptr(), cs("b").as_ptr()), 0);
        assert_eq!(mux_string_not_equal(cs("a").as_ptr(), cs("b").as_ptr()), 1);
    }
}

/// Lexicographic ordering, negative / zero / positive like strcmp.
///
/// The relational operators on `string` lower to this. Before it existed they
/// fell through to the numeric path, which unboxed the string POINTER as an
/// integer - so `<` and `>` compared addresses and answered nonsense in both
/// directions at once.
#[test]
fn string_comparison() {
    // Every pointer below is a live CString or an explicit null, which is the
    // contract the function documents.
    unsafe {
        assert!(mux_string_compare(cs("apple").as_ptr(), cs("pear").as_ptr()) < 0);
        assert!(mux_string_compare(cs("pear").as_ptr(), cs("apple").as_ptr()) > 0);
        assert_eq!(
            mux_string_compare(cs("same").as_ptr(), cs("same").as_ptr()),
            0
        );

        // A prefix sorts before the longer string that extends it.
        assert!(mux_string_compare(cs("ab").as_ptr(), cs("abc").as_ptr()) < 0);
        assert!(mux_string_compare(cs("").as_ptr(), cs("a").as_ptr()) < 0);

        // Null is handled rather than dereferenced: it sorts before any real
        // string and two nulls are equal, so the result stays a total order.
        assert_eq!(mux_string_compare(std::ptr::null(), std::ptr::null()), 0);
        assert!(mux_string_compare(std::ptr::null(), cs("a").as_ptr()) < 0);
        assert!(mux_string_compare(cs("a").as_ptr(), std::ptr::null()) > 0);

        // Antisymmetry: reversing the operands must reverse the sign. This is
        // what the pointer comparison could not do, since it reported "not
        // less" in both directions at once.
        let pairs = [("a", "b"), ("apple", "apricot"), ("Z", "a"), ("1", "2")];
        for (left, right) in pairs {
            let forward = mux_string_compare(cs(left).as_ptr(), cs(right).as_ptr());
            let backward = mux_string_compare(cs(right).as_ptr(), cs(left).as_ptr());
            assert_eq!(forward, -backward, "{left} vs {right} is not antisymmetric");
        }
    }
}

#[test]
fn string_containment() {
    let hay = unsafe { mux_string_value(cs("hello world").as_ptr()) };
    let needle = unsafe { mux_string_value(cs("world").as_ptr()) };
    unsafe {
        assert!(mux_string_contains(hay, needle));
        assert!(mux_string_contains_char(hay, 'o' as i64));
        assert!(!mux_string_contains_char(hay, 'z' as i64));
        assert!(!mux_string_contains_char(hay, -4_294_967_296));
    }
    assert!(unsafe { mux_rc_dec(hay) });
    assert!(unsafe { mux_rc_dec(needle) });
}

#[test]
fn char_conversions() {
    unsafe {
        assert_eq!(ok_int(mux_string_to_char(cs("a").as_ptr())), 'a' as i64);
        assert_err(mux_string_to_char(cs("ab").as_ptr()));
    }
    assert_eq!(ok_int(mux_char_to_int('5' as i64)), 5);
    assert_err(mux_char_to_int('a' as i64));
    assert_err(mux_char_to_int(-4_294_967_296));
    assert_eq!(read_cstr(mux_char_to_string('A' as i64)), "A");
    assert_eq!(read_cstr(mux_char_to_string(-4_294_967_296)), "");
}

#[test]
fn string_from_value_roundtrip() {
    let v = unsafe { mux_new_string_from_cstr(cs("data").as_ptr()) };
    assert!(!v.is_null());
    assert_eq!(read_cstr(unsafe { mux_string_from_value(v) }), "data");
    assert!(unsafe { mux_rc_dec(v) });
}

#[test]
fn string_from_owned_cstr_frees_input() {
    // mux_new_string_from_owned_cstr takes ownership and frees the input CString.
    // We pass into_raw() to give it an owned pointer.
    let v = unsafe { mux_new_string_from_owned_cstr(cs("owned").into_raw()) };
    assert!(!v.is_null());
    assert_eq!(read_cstr(unsafe { mux_string_from_value(v) }), "owned");
    assert!(unsafe { mux_rc_dec(v) });
}

#[test]
fn bool_extern() {
    use mux_runtime::bool::*;
    use mux_runtime::std::mux_bool_value;

    assert_eq!(read_cstr(mux_bool_to_string(1)), "true");
    assert_eq!(read_cstr(mux_bool_to_string(0)), "false");

    let bv = mux_bool_value(1);
    assert_eq!(unsafe { mux_bool_from_value(bv) }, 1);
    let as_int = unsafe { mux_bool_to_int(bv) };
    assert_eq!(unsafe { mux_value_get_int(as_int) }, 1);
    assert!(unsafe { mux_rc_dec(as_int) });
    let as_float = unsafe { mux_bool_to_float(bv) };
    assert!(!as_float.is_null());
    assert!(unsafe { mux_rc_dec(as_float) });
    assert!(unsafe { mux_rc_dec(bv) });
}

#[test]
fn boxing_roundtrips() {
    let i = mux_box_int(5);
    assert_eq!(unsafe { mux_value_get_int(i) }, 5);
    assert!(unsafe { mux_rc_dec(i) });

    let f = mux_box_float(1.5);
    assert!(!f.is_null());
    assert!(unsafe { mux_rc_dec(f) });

    let b = mux_box_bool(1);
    assert!(!b.is_null());
    assert!(unsafe { mux_rc_dec(b) });

    let s = unsafe { mux_box_str(cs("hi").as_ptr()) };
    assert!(!s.is_null());
    assert!(unsafe { mux_rc_dec(s) });
}

/// Splitting, which is the operation whose absence meant a program could read
/// a file with `io.read_file` and then do nothing with the text.
#[test]
fn string_split() {
    use mux_runtime::Value;

    let got = unsafe { mux_string_split(cs("a,b,c").as_ptr(), cs(",").as_ptr()) };
    assert_eq!(
        unsafe { &*got },
        &Value::List(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ])
    );
    assert!(unsafe { mux_rc_dec(got) });

    // A separator that is not present yields the whole string, not nothing.
    let got = unsafe { mux_string_split(cs("abc").as_ptr(), cs(",").as_ptr()) };
    assert_eq!(
        unsafe { &*got },
        &Value::List(vec![Value::String("abc".into())])
    );
    assert!(unsafe { mux_rc_dec(got) });

    // An empty separator splits into characters, which is what makes this the
    // inverse of joining a character list.
    let got = unsafe { mux_string_split(cs("hi").as_ptr(), cs("").as_ptr()) };
    assert_eq!(
        unsafe { &*got },
        &Value::List(vec![Value::String("h".into()), Value::String("i".into())])
    );
    assert!(unsafe { mux_rc_dec(got) });
}

/// Positions are CHARACTERS, not bytes. Every assertion here passes either way
/// for ASCII, so the non-ASCII cases are the ones doing the work.
#[test]
fn string_positions_are_characters() {
    use mux_runtime::Value;

    // Accented e is two bytes; the last character is 'o' at index 4, not 5.
    let accented = cs("h\u{e9}llo");
    let got = unsafe { mux_string_char_at(accented.as_ptr(), 4) };
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Int('o' as i64))))
    );
    assert!(unsafe { mux_rc_dec(got) });

    // Negative indices count from the end, as they do for lists.
    let got = unsafe { mux_string_char_at(accented.as_ptr(), -1) };
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Int('o' as i64))))
    );
    assert!(unsafe { mux_rc_dec(got) });

    // Out of range is none rather than a panic.
    let got = unsafe { mux_string_char_at(accented.as_ptr(), 99) };
    assert_eq!(unsafe { &*got }, &Value::Optional(None));
    assert!(unsafe { mux_rc_dec(got) });

    // index_of reports a character offset; a byte offset would say 2 here.
    assert_eq!(
        unsafe { mux_string_index_of(accented.as_ptr(), cs("llo").as_ptr()) },
        2
    );
    assert_eq!(
        unsafe { mux_string_index_of(accented.as_ptr(), cs("zz").as_ptr()) },
        -1
    );

    // Slicing counts characters too.
    assert_eq!(
        read_cstr(unsafe { mux_string_slice(accented.as_ptr(), 0, 2) }),
        "h\u{e9}"
    );
}

/// Slice bounds follow Python: half-open, negatives from the end, clamping
/// rather than failing, and no reversal when the bounds cross.
#[test]
fn string_slice_bounds() {
    let s = cs("abcdef");
    unsafe {
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), 1, 3)), "bc");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), 0, 6)), "abcdef");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), -2, 6)), "ef");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), 2, 99)), "cdef");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), 4, 2)), "");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), 99, 100)), "");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), -99, 2)), "ab");
        assert_eq!(read_cstr(mux_string_slice(s.as_ptr(), i64::MIN, 2)), "ab");
    }
}

#[test]
fn string_transforms_and_predicates() {
    unsafe {
        assert_eq!(read_cstr(mux_string_trim(cs("  hi \n").as_ptr())), "hi");
        assert_eq!(read_cstr(mux_string_to_upper(cs("hi").as_ptr())), "HI");
        assert_eq!(read_cstr(mux_string_to_lower(cs("Hi").as_ptr())), "hi");

        assert!(mux_string_starts_with(
            cs("hello").as_ptr(),
            cs("he").as_ptr()
        ));
        assert!(!mux_string_starts_with(
            cs("hello").as_ptr(),
            cs("lo").as_ptr()
        ));
        assert!(mux_string_ends_with(
            cs("hello").as_ptr(),
            cs("lo").as_ptr()
        ));
        assert!(!mux_string_ends_with(
            cs("hello").as_ptr(),
            cs("he").as_ptr()
        ));

        assert_eq!(
            read_cstr(mux_string_replace(
                cs("a-b-c").as_ptr(),
                cs("-").as_ptr(),
                cs("+").as_ptr()
            )),
            "a+b+c"
        );
        // An empty pattern would otherwise insert between every character.
        assert_eq!(
            read_cstr(mux_string_replace(
                cs("abc").as_ptr(),
                cs("").as_ptr(),
                cs("X").as_ptr()
            )),
            "abc"
        );
    }
}

/// `to_list` is what lets `for char c in s` work through the existing list loop
/// rather than needing a string case of its own.
#[test]
fn string_to_list_yields_characters() {
    use mux_runtime::Value;

    let got = unsafe { mux_string_to_list(cs("h\u{e9}i").as_ptr()) };
    assert_eq!(
        unsafe { &*got },
        &Value::List(vec![
            Value::Int('h' as i64),
            Value::Int('\u{e9}' as i64),
            Value::Int('i' as i64),
        ])
    );
    assert!(unsafe { mux_rc_dec(got) });
}

/// `to_bool` accepts only `true` and `false`, case-insensitively.
///
/// CSV columns are the reason it exists, and the reason it is narrow: accepting
/// `1` or `yes` means guessing which convention a file follows, then being
/// wrong for the file where `1` is the number one.
#[test]
fn string_to_bool_is_deliberately_narrow() {
    use mux_runtime::string::mux_string_to_bool;

    let parse = |text: &str| {
        let c = CString::new(text).expect("no interior nul");
        let result = unsafe { mux_string_to_bool(c.as_ptr()) };
        let ok = mux_result_is_ok(result);
        let data = mux_result_data(result);
        let value = unsafe { &*data }.clone();
        assert!(unsafe { mux_rc_dec(data) });
        assert!(unsafe { mux_rc_dec(result) });
        (ok, value)
    };

    for text in ["true", "TRUE", " True "] {
        assert_eq!(parse(text), (true, Value::Bool(true)), "{text}");
    }
    for text in ["false", "False"] {
        assert_eq!(parse(text), (true, Value::Bool(false)), "{text}");
    }
    for text in ["1", "0", "yes", "no", "y", ""] {
        let (ok, _) = parse(text);
        assert!(!ok, "{text} must not parse as a bool");
    }

    let result = unsafe { mux_string_to_bool(std::ptr::null()) };
    assert!(mux_runtime::result::mux_result_is_err(result));
    assert!(unsafe { mux_rc_dec(result) });
}
