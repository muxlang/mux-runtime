//! Coverage for the MuxFile handle API, value_from_string, and the print/flush
//! helpers. (stdin-based helpers like read_line are not exercised.)

use std::ffi::{CStr, CString};

use mux_runtime::io::*;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::std::mux_free_string;

fn unique_file(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mux_iofile_{}_{}_{}", std::process::id(), nanos, name))
}

#[test]
fn open_read_close_file() {
    let path = unique_file("read.txt");
    std::fs::write(&path, "file contents").unwrap();
    let path_c = CString::new(path.to_str().unwrap()).unwrap();

    let file = mux_open_file(path_c.as_ptr());
    assert!(!file.is_null());
    let contents_ptr = mux_read_file(file);
    assert!(!contents_ptr.is_null());
    let contents = unsafe { CStr::from_ptr(contents_ptr) }.to_string_lossy().into_owned();
    mux_free_string(contents_ptr);
    assert_eq!(contents, "file contents");
    mux_close_file(file);

    // write to a read-only handle fails (returns false)
    let ro = mux_open_file(path_c.as_ptr());
    assert!(!mux_write_file(ro, CString::new("x").unwrap().as_ptr()));
    mux_close_file(ro);

    std::fs::remove_file(&path).ok();
}

#[test]
fn open_missing_and_null() {
    let missing = CString::new("/no/such/file/at/all.xyz").unwrap();
    assert!(mux_open_file(missing.as_ptr()).is_null());
    assert!(mux_open_file(std::ptr::null()).is_null());
    // close on null is a no-op
    mux_close_file(std::ptr::null_mut());
}

#[test]
fn value_from_string_and_prints() {
    let s = CString::new("hello").unwrap();
    let v = mux_value_from_string(s.as_ptr());
    assert!(!v.is_null());
    // print helpers just need to run without crashing
    mux_print(v);
    mux_print_cstr(s.as_ptr());
    mux_flush_stdout();
    assert!(mux_rc_dec(v));

    assert!(mux_value_from_string(std::ptr::null()).is_null());
    mux_print_cstr(std::ptr::null());
}
