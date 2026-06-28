//! Unit tests for the filesystem/path layer, using a unique temp directory.

mod common;

use std::ffi::CString;

use common::{assert_err, assert_ok, ok_bool, ok_list_len, ok_string};
use mux_runtime::io::*;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn unique_dir() -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("mux_io_test_{}_{}", std::process::id(), nanos));
    dir
}

#[test]
fn file_lifecycle_and_dir_listing() {
    let dir = unique_dir();
    let dir_s = cstr(dir.to_str().unwrap());

    // mkdir + exists + is_dir
    assert_ok(mux_io_mkdir(dir_s.as_ptr()));
    assert!(ok_bool(mux_io_exists(dir_s.as_ptr())));
    assert!(ok_bool(mux_io_is_dir(dir_s.as_ptr())));

    // write + read + is_file
    let file = dir.join("hello.txt");
    let file_s = cstr(file.to_str().unwrap());
    assert_ok(mux_io_write_file(file_s.as_ptr(), cstr("hi there").as_ptr()));
    assert!(ok_bool(mux_io_is_file(file_s.as_ptr())));
    assert_eq!(ok_string(mux_io_read_file(file_s.as_ptr())), "hi there");

    // listdir sees exactly the one file
    assert_eq!(ok_list_len(mux_io_listdir(dir_s.as_ptr())), 1);

    // remove the file, then it no longer exists
    assert_ok(mux_io_remove(file_s.as_ptr()));
    assert!(!ok_bool(mux_io_exists(file_s.as_ptr())));

    // cleanup
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_missing_file_is_error() {
    let missing = cstr("/definitely/not/a/real/path/xyz.txt");
    assert_err(mux_io_read_file(missing.as_ptr()));
}

#[test]
fn path_helpers() {
    assert_eq!(
        ok_string(mux_io_join(cstr("/a/b").as_ptr(), cstr("c.txt").as_ptr())),
        "/a/b/c.txt"
    );
    assert_eq!(ok_string(mux_io_basename(cstr("/a/b/c.txt").as_ptr())), "c.txt");
    assert_eq!(ok_string(mux_io_dirname(cstr("/a/b/c.txt").as_ptr())), "/a/b");
}

#[test]
fn null_paths_are_errors() {
    assert_err(mux_io_read_file(std::ptr::null()));
    assert_err(mux_io_mkdir(std::ptr::null()));
    assert_err(mux_io_join(std::ptr::null(), std::ptr::null()));
}
