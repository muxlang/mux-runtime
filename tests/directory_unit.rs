//! Focused tests for the streaming `std.fs.Directory` handle.

use mux_runtime::io::{mux_fs_directory_close, mux_fs_directory_next, mux_fs_directory_open};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::Value;

fn string_value(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

fn ok_data(result: *mut Value) -> *mut Value {
    assert!(
        unsafe { mux_result_is_ok(result) },
        "expected successful result"
    );
    let data = unsafe { mux_result_data(result) };
    assert!(unsafe { mux_rc_dec(result) });
    data
}

#[test]
fn directory_next_streams_entries_and_reports_eof() {
    let root = std::env::temp_dir().join(format!(
        "mux-directory-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir(&root).expect("create test directory");
    std::fs::write(root.join("first.txt"), b"first").expect("write first file");
    std::fs::write(root.join("second.txt"), b"second").expect("write second file");
    let path = string_value(root.to_str().expect("temporary path is UTF-8"));

    let directory_result = unsafe { mux_fs_directory_open(path) };
    let directory = ok_data(directory_result);
    assert!(unsafe { mux_rc_dec(path) });

    let mut names = Vec::new();
    for _ in 0..2 {
        let result = unsafe { mux_fs_directory_next(directory) };
        let entry = ok_data(result);
        match unsafe { &*entry } {
            Value::Optional(Some(value)) => match value.as_ref() {
                Value::String(name) => names.push(name.clone()),
                other => panic!("expected a string entry, got {other:?}"),
            },
            other => panic!("expected some entry, got {other:?}"),
        }
        assert!(unsafe { mux_rc_dec(entry) });
    }
    names.sort();
    assert_eq!(names, vec!["first.txt", "second.txt"]);

    let eof = unsafe { mux_fs_directory_next(directory) };
    let eof_data = ok_data(eof);
    assert!(matches!(unsafe { &*eof_data }, Value::Optional(None)));
    assert!(unsafe { mux_rc_dec(eof_data) });

    unsafe { mux_fs_directory_close(directory) };
    let closed = unsafe { mux_fs_directory_next(directory) };
    assert!(!unsafe { mux_result_is_ok(closed) });
    assert!(unsafe { mux_rc_dec(closed) });
    assert!(unsafe { mux_rc_dec(directory) });
    std::fs::remove_dir_all(root).expect("remove test directory");
}
