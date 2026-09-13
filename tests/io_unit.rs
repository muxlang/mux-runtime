//! Unit tests for the filesystem/path layer, using a unique temp directory.

mod common;

use std::ffi::CString;

use common::{assert_err, assert_ok, ok_bool, ok_int, ok_list_len, ok_string};
use mux_runtime::io::*;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_data, mux_result_is_err};
use mux_runtime::std::{mux_fs_error_kind, mux_fs_error_path};
use mux_runtime::Value;

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

fn assert_not_found(result: *mut Value, expected_path: &str) {
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_fs_error_kind(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if value.as_ref() == 2_i32.to_ne_bytes())
    );
    let path = unsafe { mux_fs_error_path(error) };
    assert!(matches!(unsafe { &*path }, Value::String(value) if value == expected_path));
    assert!(unsafe { mux_rc_dec(path) });
    assert!(unsafe { mux_rc_dec(kind) });
    assert!(unsafe { mux_rc_dec(error) });
    assert!(unsafe { mux_rc_dec(result) });
}

#[test]
fn file_lifecycle_and_dir_listing() {
    let dir = unique_dir();
    let dir_s = cstr(dir.to_str().unwrap());

    // mkdir + exists + is_dir
    assert_ok(unsafe { mux_io_mkdir(dir_s.as_ptr()) });
    assert!(ok_bool(unsafe { mux_io_exists(dir_s.as_ptr()) }));
    assert!(ok_bool(unsafe { mux_io_is_dir(dir_s.as_ptr()) }));

    // write + read + is_file
    let file = dir.join("hello.txt");
    let file_s = cstr(file.to_str().unwrap());
    assert_ok(unsafe { mux_io_write_file(file_s.as_ptr(), cstr("hi there").as_ptr()) });
    assert!(ok_bool(unsafe { mux_io_is_file(file_s.as_ptr()) }));
    assert_eq!(
        ok_string(unsafe { mux_io_read_file(file_s.as_ptr()) }),
        "hi there"
    );

    // listdir sees exactly the one file
    assert_eq!(ok_list_len(unsafe { mux_io_listdir(dir_s.as_ptr()) }), 1);

    // remove the file, then it no longer exists
    assert_ok(unsafe { mux_io_remove(file_s.as_ptr()) });
    assert!(!ok_bool(unsafe { mux_io_exists(file_s.as_ptr()) }));

    // cleanup
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn recursive_directory_removal_is_explicit() {
    let dir = unique_dir();
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("file.txt"), b"data").unwrap();
    let dir_s = cstr(dir.to_str().unwrap());

    assert_err(unsafe { mux_io_remove(dir_s.as_ptr()) });
    assert!(dir.exists());
    assert_ok(unsafe { mux_io_remove_dir_all(dir_s.as_ptr()) });
    assert!(!dir.exists());
}

#[test]
fn read_missing_file_is_error() {
    // Keep the missing-path fixture valid on Windows too; a POSIX root path
    // can resolve to a real location on another host or produce a different
    // error category than the intended "parent does not exist" case.
    let missing_path = unique_dir().join("xyz.txt");
    let missing = cstr(missing_path.to_str().unwrap());
    assert_err(unsafe { mux_io_read_file(missing.as_ptr()) });
}

#[test]
fn whole_file_reads_reject_unbounded_input() {
    let directory = unique_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("oversized.bin");
    std::fs::write(&path, vec![b'x'; 16 * 1024 * 1024 + 1]).unwrap();
    let path = cstr(path.to_str().unwrap());

    assert_err(unsafe { mux_io_read_file(path.as_ptr()) });
    assert_err(unsafe { mux_io_read_bytes(path.as_ptr()) });

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn filesystem_os_errors_preserve_category_and_path() {
    let missing = unique_dir().join("missing.txt");
    let missing_s = cstr(missing.to_str().unwrap());

    assert_not_found(
        unsafe { mux_io_read_bytes(missing_s.as_ptr()) },
        missing.to_str().unwrap(),
    );
    assert_not_found(
        unsafe { mux_io_write_file(missing_s.as_ptr(), cstr("data").as_ptr()) },
        missing.to_str().unwrap(),
    );
    assert_not_found(
        unsafe { mux_io_remove(missing_s.as_ptr()) },
        missing.to_str().unwrap(),
    );
    assert_not_found(
        unsafe { mux_io_listdir(missing_s.as_ptr()) },
        missing.to_str().unwrap(),
    );
    assert_not_found(
        unsafe { mux_io_file_size(missing_s.as_ptr()) },
        missing.to_str().unwrap(),
    );
}

#[test]
fn path_helpers() {
    // Build the fixture with the host path implementation rather than spelling
    // a POSIX separator. These tests run in the native-host acceptance matrix,
    // where Windows uses `\\` and would otherwise fail despite the path API
    // returning the correct platform-native value.
    let parent = std::path::Path::new("a").join("b");
    let child = parent.join("c.txt");
    let parent_str = parent.to_str().unwrap();
    let child_str = child.to_str().unwrap();
    assert_eq!(
        ok_string(unsafe { mux_io_join(cstr(parent_str).as_ptr(), cstr("c.txt").as_ptr()) }),
        child_str
    );
    assert_eq!(
        ok_string(unsafe { mux_io_basename(cstr(child_str).as_ptr()) }),
        "c.txt"
    );
    assert_eq!(
        ok_string(unsafe { mux_io_dirname(cstr(child_str).as_ptr()) }),
        parent_str
    );
}

#[test]
fn path_resolution_and_file_operations_report_results() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.txt");
    let copied = dir.join("copied.txt");
    let renamed = dir.join("renamed.txt");
    std::fs::write(&source, b"hello").unwrap();
    let source_s = cstr(source.to_str().unwrap());
    let copied_s = cstr(copied.to_str().unwrap());
    let renamed_s = cstr(renamed.to_str().unwrap());

    assert!(!ok_bool(unsafe { mux_io_is_symlink(source_s.as_ptr()) }));
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(ok_string(mux_io_cwd()), cwd.to_str().unwrap());
    assert_eq!(
        ok_string(unsafe { mux_io_absolute(cstr(".").as_ptr()) }),
        cwd.join(".").to_str().unwrap()
    );
    assert!(ok_string(unsafe { mux_io_canonical(source_s.as_ptr()) }).ends_with("source.txt"));
    assert_eq!(ok_int(unsafe { mux_io_file_size(source_s.as_ptr()) }), 5);
    assert_ok(unsafe { mux_io_copy(source_s.as_ptr(), copied_s.as_ptr()) });
    assert_eq!(
        ok_string(unsafe { mux_io_read_file(copied_s.as_ptr()) }),
        "hello"
    );
    assert_ok(unsafe { mux_io_rename(copied_s.as_ptr(), renamed_s.as_ptr()) });
    assert!(!ok_bool(unsafe { mux_io_exists(copied_s.as_ptr()) }));
    assert!(ok_bool(unsafe { mux_io_exists(renamed_s.as_ptr()) }));

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn atomic_replacement_replaces_existing_destination() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.tmp");
    let destination = dir.join("destination.txt");
    std::fs::write(&source, b"new contents").unwrap();
    std::fs::write(&destination, b"old contents").unwrap();
    let source_s = cstr(source.to_str().unwrap());
    let destination_s = cstr(destination.to_str().unwrap());

    assert_ok(unsafe { mux_io_replace_atomic(source_s.as_ptr(), destination_s.as_ptr()) });
    assert!(!source.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), b"new contents");

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn temporary_resources_are_unique_and_created() {
    let file = ok_string(mux_fs_temp_file());
    let dir = ok_string(mux_fs_temp_dir());
    assert!(std::path::Path::new(&file).is_file());
    assert!(std::path::Path::new(&dir).is_dir());
    assert_ne!(file, dir);
    std::fs::remove_file(file).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn readonly_permission_round_trip() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("permissions.txt");
    std::fs::write(&file, b"data").unwrap();
    let file_s = cstr(file.to_str().unwrap());

    assert!(!ok_bool(unsafe { mux_fs_is_readonly(file_s.as_ptr()) }));
    assert_ok(unsafe { mux_fs_set_readonly(file_s.as_ptr(), true) });
    assert!(ok_bool(unsafe { mux_fs_is_readonly(file_s.as_ptr()) }));
    assert_ok(unsafe { mux_fs_set_readonly(file_s.as_ptr(), false) });
    assert!(!ok_bool(unsafe { mux_fs_is_readonly(file_s.as_ptr()) }));

    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn symbolic_link_target_is_read_without_following() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("target.txt");
    let link = dir.join("link.txt");
    std::fs::write(&target, b"target").unwrap();
    std::os::unix::fs::symlink("target.txt", &link).unwrap();
    let link_s = cstr(link.to_str().unwrap());

    assert_eq!(
        ok_string(unsafe { mux_fs_read_link(link_s.as_ptr()) }),
        "target.txt"
    );
    assert!(ok_bool(unsafe { mux_io_is_symlink(link_s.as_ptr()) }));

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn null_paths_are_errors() {
    assert_err(unsafe { mux_io_read_file(std::ptr::null()) });
    assert_err(unsafe { mux_io_mkdir(std::ptr::null()) });
    assert_err(unsafe { mux_io_join(std::ptr::null(), std::ptr::null()) });
}

#[cfg(unix)]
#[test]
fn directory_listing_reports_non_utf8_names() {
    use std::os::unix::ffi::OsStringExt;

    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let invalid_name = std::ffi::OsString::from_vec(vec![b'b', b'a', b'd', 0xff]);
    std::fs::write(dir.join(invalid_name), b"content").unwrap();
    let dir_s = cstr(dir.to_str().unwrap());

    assert_err(unsafe { mux_io_listdir(dir_s.as_ptr()) });
    std::fs::remove_dir_all(&dir).unwrap();
}
