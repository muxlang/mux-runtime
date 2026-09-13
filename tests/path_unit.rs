use mux_runtime::path::{
    mux_fs_path_display, mux_fs_path_extension, mux_fs_path_file_name, mux_fs_path_from_string,
    mux_fs_path_is_absolute, mux_fs_path_join, mux_fs_path_to_string,
};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::Value;

fn string(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

fn ok_string(value: *mut Value) -> String {
    let Value::Result(Ok(inner)) = (unsafe { &*value }) else {
        panic!("expected ok")
    };
    let Value::String(text) = inner.as_ref() else {
        panic!("expected string")
    };
    let out = text.clone();
    unsafe { mux_rc_dec(value) };
    out
}

fn ok_optional_string(value: *mut Value) -> String {
    let Value::Result(Ok(inner)) = (unsafe { &*value }) else {
        panic!("expected ok")
    };
    let Value::Optional(Some(text)) = inner.as_ref() else {
        panic!("expected some string")
    };
    let Value::String(text) = text.as_ref() else {
        panic!("expected string")
    };
    let out = text.clone();
    unsafe { mux_rc_dec(value) };
    out
}

#[test]
fn path_is_lossless_for_utf8_and_composes_components() {
    let source_path = std::path::Path::new("logs").join("today.txt");
    let source_text = source_path.to_str().unwrap();
    let joined_path = source_path.join("archive.txt");
    let joined_text = joined_path.to_str().unwrap();
    let source = string(source_text);
    let path_result = unsafe { mux_fs_path_from_string(source) };
    unsafe { mux_rc_dec(source) };
    let Value::Result(Ok(path)) = (unsafe { &*path_result }) else {
        panic!("expected path")
    };
    let path = mux_rc_alloc(path.as_ref().clone());
    unsafe { mux_rc_dec(path_result) };

    let child = string("archive.txt");
    let joined_result = unsafe { mux_fs_path_join(path, child) };
    unsafe { mux_rc_dec(child) };
    let Value::Result(Ok(joined)) = (unsafe { &*joined_result }) else {
        panic!("expected join")
    };
    let joined = mux_rc_alloc(joined.as_ref().clone());
    unsafe { mux_rc_dec(joined_result) };

    assert_eq!(
        ok_string(unsafe { mux_fs_path_to_string(joined) }),
        joined_text
    );
    assert_eq!(
        ok_string(unsafe { mux_fs_path_display(joined) }),
        joined_text
    );
    assert_eq!(
        ok_optional_string(unsafe { mux_fs_path_file_name(joined) }),
        "archive.txt"
    );
    assert_eq!(
        ok_optional_string(unsafe { mux_fs_path_extension(joined) }),
        "txt"
    );
    let absolute = unsafe { mux_fs_path_is_absolute(joined) };
    assert!(
        matches!(unsafe { &*absolute }, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Bool(false)))
    );

    unsafe {
        mux_rc_dec(absolute);
        mux_rc_dec(joined);
        mux_rc_dec(path);
    }
}
