use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::raw::c_char;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::std::StdErrorKind;
use crate::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_FILE_READ_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDIN_LINE_BYTES: usize = 16 * 1024 * 1024;
const FILE_READ_CHUNK_BYTES: usize = 8192;
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_DIRECTORY_NAME_BYTES: usize = 16 * 1024 * 1024;

struct DirectoryEntry {
    entries: std::fs::ReadDir,
    refs: usize,
    entry_count: usize,
    entry_name_bytes: usize,
}

static DIRECTORIES: LazyLock<Mutex<HashMap<i64, DirectoryEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static DIRECTORY_TYPE_ID: LazyLock<crate::TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Directory",
        std::mem::size_of::<i64>(),
        Some(drop_directory),
        Some(copy_directory),
    )
});

fn directories() -> std::sync::MutexGuard<'static, HashMap<i64, DirectoryEntry>> {
    DIRECTORIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_directory_id() -> i64 {
    let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    i64::try_from(id).unwrap_or(1)
}

fn directory_handle(value: *const crate::Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *DIRECTORY_TYPE_ID {
        return Err("invalid Directory handle".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("invalid Directory handle".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if id <= 0 || !directories().contains_key(&id) {
        return Err("Directory is closed or invalid".to_string());
    }
    Ok(id)
}

fn directory_value(id: i64) -> *mut crate::Value {
    let value = alloc_object(*DIRECTORY_TYPE_ID);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { crate::refcount::mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = id };
    value
}

extern "C" fn copy_directory(source: *mut c_void, destination: *mut c_void) {
    if source.is_null() || destination.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let valid = directories().get_mut(&id).map(|entry| {
        entry.refs = entry.refs.saturating_add(1);
    });
    unsafe { *destination.cast::<i64>() = valid.map_or(0, |()| id) };
}

extern "C" fn drop_directory(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    let id = unsafe { *pointer.cast::<i64>() };
    let mut entries = directories();
    if entries.get_mut(&id).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    }) {
        entries.remove(&id);
    }
}

#[derive(Debug)]
pub struct MuxFile(pub std::fs::File);

pub fn print(s: &str) {
    print!("{s}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn read_bounded_line<R: BufRead>(reader: &mut R) -> io::Result<String> {
    let mut input = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(buffer.len(), |position| position + 1);
        let Some(next_size) = input.len().checked_add(count) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stdin line exceeds the supported read range",
            ));
        };
        if next_size > MAX_STDIN_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stdin line exceeds the 16 MiB read limit",
            ));
        }
        input.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if newline.is_some() {
            break;
        }
    }
    let input = String::from_utf8(input).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stdin is not valid UTF-8: {error}"),
        )
    })?;
    Ok(input.trim().to_string())
}

pub fn read_line() -> Result<String, String> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    read_bounded_line(&mut reader).map_err(|error| error.to_string())
}

pub fn open_file(path: &str) -> Result<File, String> {
    File::open(path).map_err(|e| e.to_string())
}

fn read_bounded<R: Read>(mut reader: R) -> io::Result<Vec<u8>> {
    let mut contents = Vec::new();
    let mut buffer = [0_u8; FILE_READ_CHUNK_BYTES];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let Some(new_size) = contents.len().checked_add(count) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file size exceeds the supported read range",
            ));
        };
        if new_size > MAX_FILE_READ_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file exceeds the 16 MiB read limit",
            ));
        }
        contents.extend_from_slice(&buffer[..count]);
    }
    Ok(contents)
}

fn read_bounded_path(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if let Ok(metadata) = file.metadata() {
        if metadata.is_file() && metadata.len() > MAX_FILE_READ_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file exceeds the 16 MiB read limit",
            ));
        }
    }
    read_bounded(file)
}

fn directory_list_budget(
    entry_count: usize,
    name_bytes: usize,
    name: &str,
) -> io::Result<(usize, usize)> {
    if entry_count >= MAX_DIRECTORY_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory listing exceeds the 65536-entry limit",
        ));
    }
    let next_name_bytes = name_bytes.checked_add(name.len()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory listing name data exceeds the supported range",
        )
    })?;
    if next_name_bytes > MAX_DIRECTORY_NAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory listing names exceed the 16 MiB limit",
        ));
    }
    Ok((entry_count + 1, next_name_bytes))
}

pub fn read_file(file: File) -> Result<String, String> {
    let contents = read_bounded(file).map_err(|e| e.to_string())?;
    String::from_utf8(contents).map_err(|e| e.to_string())
}

pub fn write_file(mut file: File, content: &str) -> Result<(), String> {
    file.write_all(content.as_bytes())
        .map_err(|e| e.to_string())
}

pub fn close_file(_file: File) {
    // File closes on drop
}

/// # Safety
/// `s` must be null or a valid, NUL-terminated C string that remains alive for
/// the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_value_from_string(s: *const c_char) -> *mut crate::Value {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(s) };
    let rust_str = c_str.to_string_lossy().to_string();
    let value = crate::Value::String(rust_str);
    crate::refcount::mux_rc_alloc(value)
}

/// # Safety
/// `s` must be null or a valid, NUL-terminated C string that remains alive for
/// the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_print_cstr(s: *const c_char) {
    if s.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(s).to_string_lossy() };
    print!("{s}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// # Safety
/// `val` must be a valid, live `Value` pointer returned by `mux_rc_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_print(val: *mut Value) {
    let val = unsafe { &*val };
    println!("{val}");
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_read_line() -> *mut c_char {
    match read_line() {
        Ok(s) => match CString::new(s) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// This function is safe as it only flushes stdout.
#[unsafe(no_mangle)]
pub extern "C" fn mux_flush_stdout() {
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// # Safety
/// Returns a valid i64 from stdin, or 0 on error.
#[unsafe(no_mangle)]
pub extern "C" fn mux_read_int() -> i64 {
    match read_line() {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_open_file(path: *const c_char) -> *mut MuxFile {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = c_str.to_string_lossy();
    match std::fs::File::open(&*path_str) {
        Ok(f) => Box::into_raw(Box::new(MuxFile(f))),
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `file` must be a valid file handle returned by `mux_open_file` that has not
/// already been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_read_file(file: *mut MuxFile) -> *mut c_char {
    if file.is_null() {
        return std::ptr::null_mut();
    }
    let file = unsafe { &mut *file };
    match read_bounded(&mut file.0) {
        Ok(contents) => match String::from_utf8(contents)
            .ok()
            .and_then(|contents| CString::new(contents).ok())
        {
            Some(c) => c.into_raw(),
            None => std::ptr::null_mut(),
        },
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `file` must be a valid file handle returned by `mux_open_file` that has not
/// already been closed. `content` must be null or a valid, NUL-terminated C
/// string that remains alive for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_write_file(file: *mut MuxFile, content: *const c_char) -> bool {
    if file.is_null() || content.is_null() {
        return false;
    }
    let c_str = unsafe { CStr::from_ptr(content) };
    let content_str = c_str.to_string_lossy();
    unsafe { (*file).0.write_all(content_str.as_bytes()).is_ok() }
}

/// # Safety
/// `file` must be null or a valid file handle returned by `mux_open_file` that
/// has not already been closed. A non-null handle is consumed by this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_close_file(file: *mut MuxFile) {
    if !file.is_null() {
        unsafe {
            drop(Box::from_raw(file));
        }
    }
}

fn io_ok(val: Value) -> *mut Value {
    crate::std::fs_result_ok(val)
}

fn io_err(msg: String) -> *mut Value {
    crate::std::fs_result_err_kind(StdErrorKind::Invalid, msg)
}

fn io_err_kind(kind: StdErrorKind, msg: impl Into<String>) -> *mut Value {
    crate::std::fs_result_err_kind(kind, msg.into())
}

fn io_err_kind_path(
    kind: StdErrorKind,
    msg: impl Into<String>,
    path: impl Into<String>,
) -> *mut Value {
    crate::std::fs_result_err_kind_path(kind, msg.into(), path)
}

fn io_err_io_path(
    context: impl Into<String>,
    error: &std::io::Error,
    path: impl Into<String>,
) -> *mut Value {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => StdErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => StdErrorKind::Permission,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => StdErrorKind::Invalid,
        _ => StdErrorKind::Io,
    };
    io_err_kind_path(kind, format!("{}: {error}", context.into()), path)
}

fn create_temp_path(directory: bool) -> Result<String, String> {
    let root = std::env::temp_dir();
    for _ in 0..128 {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("mux-{}-{sequence}", std::process::id()));
        let result = if directory {
            std::fs::create_dir(&path)
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map(|_| ())
        };
        match result {
            Ok(()) => {
                return path
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "temporary path is not valid UTF-8".to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to create temporary resource: {error}")),
        }
    }
    Err("could not allocate a unique temporary path".to_string())
}

/// Create a unique empty temporary file and return its path.
#[unsafe(no_mangle)]
pub extern "C" fn mux_fs_temp_file() -> *mut Value {
    match create_temp_path(false) {
        Ok(path) => io_ok(Value::String(path)),
        Err(error) => io_err_kind(StdErrorKind::Io, error),
    }
}

/// Create a unique empty temporary directory and return its path.
#[unsafe(no_mangle)]
pub extern "C" fn mux_fs_temp_dir() -> *mut Value {
    match create_temp_path(true) {
        Ok(path) => io_ok(Value::String(path)),
        Err(error) => io_err_kind(StdErrorKind::Io, error),
    }
}

/// Check whether a path is marked read-only by the operating system.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_is_readonly(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match std::fs::metadata(&*path) {
        Ok(metadata) => io_ok(Value::Bool(metadata.permissions().readonly())),
        Err(error) => io_err_io_path(
            format!("Failed to inspect permissions for '{path}'"),
            &error,
            path,
        ),
    }
}

/// Set or clear the platform's read-only permission flag for a path.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_set_readonly(path: *const c_char, readonly: bool) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path = unsafe { CStr::from_ptr(path).to_string_lossy() };
    let permissions = match std::fs::metadata(&*path) {
        Ok(metadata) => metadata.permissions(),
        Err(error) => {
            return io_err_io_path(
                format!("Failed to inspect permissions for '{path}'"),
                &error,
                path,
            );
        }
    };
    let mut permissions = permissions;
    permissions.set_readonly(readonly);
    match std::fs::set_permissions(&*path, permissions) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to set permissions for '{path}'"),
            &error,
            path,
        ),
    }
}

/// Read a symbolic link's target without following the link.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_read_link(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match std::fs::read_link(&*path) {
        Ok(target) => match target.to_str() {
            Some(target) => io_ok(Value::String(target.to_string())),
            None => io_err_kind_path(
                StdErrorKind::NotUnicode,
                "symbolic-link target is not valid UTF-8",
                path,
            ),
        },
        Err(error) => io_err_io_path(
            format!("Failed to read symbolic link '{path}'"),
            &error,
            path,
        ),
    }
}

// ============================================================================
// File Operations - All return Result<T, FsError> as *mut Value
// ============================================================================

/// Read file contents at path. Returns Result<string, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_read_file(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err_kind(StdErrorKind::Invalid, "path is null");
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = c_str.to_string_lossy();

    match read_bounded_path(Path::new(&*path_str)).and_then(|contents| {
        String::from_utf8(contents)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8"))
    }) {
        Ok(contents) => io_ok(Value::String(contents)),
        Err(e) => io_err_io_path(format!("Failed to read file '{path_str}'"), &e, path_str),
    }
}

/// Write content to file at path. Returns Result<void, FsError>
/// # Safety
/// `path` and `content` must each be null or valid, NUL-terminated C strings
/// that remain alive for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_write_file(
    path: *const c_char,
    content: *const c_char,
) -> *mut Value {
    if path.is_null() || content.is_null() {
        return io_err("path or content is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    let content_str = unsafe { CStr::from_ptr(content).to_string_lossy() };

    match std::fs::write(&*path_str, content_str.as_bytes()) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to write file '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

#[unsafe(no_mangle)]
/// Read arbitrary file bytes without a text decoding step.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated string for the duration of
/// the call.
pub unsafe extern "C" fn mux_io_read_bytes(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match read_bounded_path(Path::new(&*path_str)) {
        Ok(contents) => io_ok(Value::Bytes(contents)),
        Err(error) => io_err_io_path(
            format!("Failed to read file '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

#[unsafe(no_mangle)]
/// Write arbitrary bytes to a file, replacing any existing contents.
///
/// # Safety
/// `path` and `data` must be valid pointers for the duration of the call.
pub unsafe extern "C" fn mux_io_write_bytes(path: *const c_char, data: *const Value) -> *mut Value {
    if path.is_null() || data.is_null() {
        return io_err("path or data is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    let Value::Bytes(bytes) = (unsafe { &*data }) else {
        return io_err("data must be bytes".to_string());
    };
    match std::fs::write(&*path_str, bytes) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to write file '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

/// Check if path exists. Returns Result<bool, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_exists(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    io_ok(Value::Bool(Path::new(&*path_str).exists()))
}

/// Remove file at path. Returns Result<void, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_remove(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };

    match std::fs::remove_file(&*path_str) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(format!("Failed to remove '{path_str}'"), &error, path_str),
    }
}

/// Recursively remove a directory tree. This operation is deliberately named
/// separately from `mux_io_remove`, which only removes one file, so callers
/// cannot delete a tree accidentally.
///
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_remove_dir_all(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match std::fs::remove_dir_all(&*path_str) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to recursively remove '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

/// Check if path is a file. Returns Result<bool, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_is_file(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    io_ok(Value::Bool(Path::new(&*path_str).is_file()))
}

/// Check if path is a directory. Returns Result<bool, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_is_dir(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    io_ok(Value::Bool(Path::new(&*path_str).is_dir()))
}

// ============================================================================
// Directory Operations - All return Result<T, FsError> as *mut Value
// ============================================================================

/// Create directory at path. Returns Result<void, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_mkdir(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };

    match std::fs::create_dir_all(&*path_str) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to create directory '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

/// List directory contents. Returns `Result<list<string>, FsError>`.
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_listdir(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };

    match std::fs::read_dir(&*path_str) {
        Ok(entries) => {
            let mut list = Vec::new();
            let mut name_bytes = 0usize;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        return io_err_io_path(
                            format!("Failed to read an entry in directory '{path_str}'"),
                            &error,
                            path_str,
                        );
                    }
                };
                let Ok(name) = entry.file_name().into_string() else {
                    return io_err_kind_path(
                        StdErrorKind::NotUnicode,
                        format!(
                            "Failed to list directory '{path_str}': entry name is not valid UTF-8"
                        ),
                        path_str,
                    );
                };
                let (entry_count, next_name_bytes) =
                    match directory_list_budget(list.len(), name_bytes, &name) {
                        Ok(budget) => budget,
                        Err(error) => {
                            return io_err_io_path(
                                format!("Failed to list directory '{path_str}'"),
                                &error,
                                path_str,
                            );
                        }
                    };
                debug_assert_eq!(entry_count, list.len() + 1);
                name_bytes = next_name_bytes;
                list.push(Value::String(name));
            }
            io_ok(Value::List(list))
        }
        Err(error) => io_err_io_path(
            format!("Failed to list directory '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

/// Open a directory for incremental entry iteration. Entry names are returned
/// in the platform's filesystem order; callers that need ordering can sort the
/// values explicitly after collecting them.
///
/// # Safety
/// `path` must be null or point to a live string `Value` for the duration of
/// this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_directory_open(path: *const Value) -> *mut Value {
    let Some(Value::String(path)) = path.as_ref() else {
        return io_err("directory path must be a string".to_string());
    };
    if path.is_empty() {
        return io_err("directory path must not be empty".to_string());
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            return io_err_io_path(format!("failed to open directory '{path}'"), &error, path);
        }
    };
    let id = next_directory_id();
    directories().insert(
        id,
        DirectoryEntry {
            entries,
            refs: 1,
            entry_count: 0,
            entry_name_bytes: 0,
        },
    );
    let value = directory_value(id);
    if value.is_null() {
        directories().remove(&id);
        return io_err("could not allocate Directory handle".to_string());
    }
    let owned = unsafe { (*value).clone() };
    unsafe { crate::refcount::mux_rc_dec(value) };
    io_ok(owned)
}

/// Return the next entry name, or `none` after the directory is exhausted.
/// Filesystem iteration errors and non-Unicode entry names are reported as
/// errors instead of being silently dropped.
///
/// # Safety
/// `directory` must be null or point to a live Directory value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_directory_next(directory: *const Value) -> *mut Value {
    let id = match directory_handle(directory) {
        Ok(id) => id,
        Err(error) => return io_err(error),
    };
    let mut entries = directories();
    let Some(directory) = entries.get_mut(&id) else {
        return io_err("Directory is closed or invalid".to_string());
    };
    match directory.entries.next() {
        None => io_ok(Value::Optional(None)),
        Some(Ok(entry)) => match entry.file_name().into_string() {
            Ok(name) => {
                let (entry_count, entry_name_bytes) = match directory_list_budget(
                    directory.entry_count,
                    directory.entry_name_bytes,
                    &name,
                ) {
                    Ok(budget) => budget,
                    Err(error) => return io_err_kind(StdErrorKind::Invalid, error.to_string()),
                };
                directory.entry_count = entry_count;
                directory.entry_name_bytes = entry_name_bytes;
                io_ok(Value::Optional(Some(Box::new(Value::String(name)))))
            }
            Err(_) => io_err_kind(
                StdErrorKind::NotUnicode,
                "directory entry name is not valid UTF-8",
            ),
        },
        Some(Err(error)) => io_err_kind(
            StdErrorKind::Io,
            format!("directory iteration failed: {error}"),
        ),
    }
}

/// Close a directory handle and invalidate all aliases to it.
///
/// # Safety
/// `directory` must be null or point to a live Directory value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_directory_close(directory: *mut Value) {
    if directory.is_null() || unsafe { get_object_type_id(directory) } != *DIRECTORY_TYPE_ID {
        return;
    }
    let ptr = unsafe { get_object_ptr(directory) };
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if id > 0 {
        directories().remove(&id);
        unsafe { *ptr.cast::<i64>() = 0 };
    }
}

// ============================================================================
// Path Operations - All return Result<string, FsError> as *mut Value
// ============================================================================

/// Join two path components. Returns Result<string, FsError>
/// # Safety
/// `path1` and `path2` must each be null or valid, NUL-terminated C strings
/// that remain alive for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_join(path1: *const c_char, path2: *const c_char) -> *mut Value {
    if path1.is_null() || path2.is_null() {
        return io_err("path1 or path2 is null".to_string());
    }
    let path1_str = unsafe { CStr::from_ptr(path1).to_string_lossy() };
    let path2_str = unsafe { CStr::from_ptr(path2).to_string_lossy() };

    let joined = Path::new(&*path1_str).join(&*path2_str);
    match joined.to_str() {
        Some(s) => io_ok(Value::String(s.to_string())),
        None => io_err_kind_path(
            StdErrorKind::NotUnicode,
            "Failed to join paths: invalid UTF-8",
            format!("{path1_str}/{path2_str}"),
        ),
    }
}

/// Get base name of path. Returns Result<string, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_basename(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };

    let Some(basename) = Path::new(&*path_str).file_name() else {
        return io_ok(Value::String(String::new()));
    };
    match basename.to_str() {
        Some(basename) => io_ok(Value::String(basename.to_string())),
        None => io_err_kind_path(
            StdErrorKind::NotUnicode,
            "path basename is not valid UTF-8",
            path_str,
        ),
    }
}

/// Get directory name of path. Returns Result<string, FsError>
/// # Safety
/// `path` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_dirname(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };

    let Some(dirname) = Path::new(&*path_str).parent() else {
        return io_ok(Value::String(".".to_string()));
    };
    match dirname.to_str() {
        Some(dirname) => io_ok(Value::String(dirname.to_string())),
        None => io_err_kind_path(
            StdErrorKind::NotUnicode,
            "path parent is not valid UTF-8",
            path_str,
        ),
    }
}

#[unsafe(no_mangle)]
/// Return the process working directory.
pub extern "C" fn mux_io_cwd() -> *mut Value {
    match std::env::current_dir() {
        Ok(path) => match path.to_str() {
            Some(path) => io_ok(Value::String(path.to_string())),
            None => io_err_kind(
                StdErrorKind::NotUnicode,
                "working directory is not valid UTF-8",
            ),
        },
        Err(error) => io_err_kind(
            StdErrorKind::Os,
            format!("Failed to read working directory: {error}"),
        ),
    }
}

#[unsafe(no_mangle)]
/// Resolve a path against the current directory without following symlinks.
/// Absolute paths are returned unchanged.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string for this call.
pub unsafe extern "C" fn mux_io_absolute(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    let path = Path::new(&*path_str);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(error) => {
                return io_err_kind(
                    StdErrorKind::Os,
                    format!("Failed to read working directory: {error}"),
                );
            }
        }
    };
    match absolute.to_str() {
        Some(path) => io_ok(Value::String(path.to_string())),
        None => io_err_kind_path(
            StdErrorKind::NotUnicode,
            "absolute path is not valid UTF-8",
            path_str,
        ),
    }
}

#[unsafe(no_mangle)]
/// Canonicalize a path, resolving symlinks and requiring it to exist.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string for this call.
pub unsafe extern "C" fn mux_io_canonical(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match std::fs::canonicalize(&*path_str) {
        Ok(path) => match path.to_str() {
            Some(path) => io_ok(Value::String(path.to_string())),
            None => io_err_kind_path(
                StdErrorKind::NotUnicode,
                "canonical path is not valid UTF-8",
                path_str,
            ),
        },
        Err(error) => io_err_io_path(
            format!("Failed to canonicalize '{path_str}'"),
            &error,
            path_str,
        ),
    }
}

#[unsafe(no_mangle)]
/// Copy one file to another path.
///
/// # Safety
/// `source` and `destination` must be null or valid NUL-terminated C strings.
pub unsafe extern "C" fn mux_io_copy(
    source: *const c_char,
    destination: *const c_char,
) -> *mut Value {
    if source.is_null() || destination.is_null() {
        return io_err("source or destination is null".to_string());
    }
    let source = unsafe { CStr::from_ptr(source).to_string_lossy() };
    let destination = unsafe { CStr::from_ptr(destination).to_string_lossy() };
    match std::fs::copy(&*source, &*destination) {
        Ok(_) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to copy '{source}' to '{destination}'"),
            &error,
            source,
        ),
    }
}

#[unsafe(no_mangle)]
/// Rename a file or directory.
///
/// # Safety
/// `source` and `destination` must be null or valid NUL-terminated C strings.
pub unsafe extern "C" fn mux_io_rename(
    source: *const c_char,
    destination: *const c_char,
) -> *mut Value {
    if source.is_null() || destination.is_null() {
        return io_err("source or destination is null".to_string());
    }
    let source = unsafe { CStr::from_ptr(source).to_string_lossy() };
    let destination = unsafe { CStr::from_ptr(destination).to_string_lossy() };
    match std::fs::rename(&*source, &*destination) {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to rename '{source}' to '{destination}'"),
            &error,
            source,
        ),
    }
}

/// Atomically replace the destination with the source on the same filesystem.
/// The source is moved and the destination is replaced when it already exists.
///
/// # Safety
/// `source` and `destination` must be null or valid NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_replace_atomic(
    source: *const c_char,
    destination: *const c_char,
) -> *mut Value {
    if source.is_null() || destination.is_null() {
        return io_err("source or destination is null".to_string());
    }
    let source = unsafe { CStr::from_ptr(source).to_string_lossy() };
    let destination = unsafe { CStr::from_ptr(destination).to_string_lossy() };

    #[cfg(windows)]
    let result = {
        use std::os::windows::ffi::OsStrExt;
        let source_wide: Vec<u16> = std::ffi::OsStr::new(&*source)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let destination_wide: Vec<u16> = std::ffi::OsStr::new(&*destination)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let succeeded = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING,
            )
        } != 0;
        if succeeded {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };

    #[cfg(not(windows))]
    let result = std::fs::rename(&*source, &*destination);

    match result {
        Ok(()) => io_ok(Value::Unit),
        Err(error) => io_err_io_path(
            format!("Failed to atomically replace '{destination}' with '{source}'"),
            &error,
            source,
        ),
    }
}

#[unsafe(no_mangle)]
/// Return a regular file's size in bytes.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string for this call.
pub unsafe extern "C" fn mux_io_file_size(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match std::fs::metadata(&*path_str) {
        Ok(metadata) => match i64::try_from(metadata.len()) {
            Ok(size) => io_ok(Value::Int(size)),
            Err(_) => {
                io_err_kind_path(StdErrorKind::Range, "file size exceeds int range", path_str)
            }
        },
        Err(error) => io_err_io_path(format!("Failed to inspect '{path_str}'"), &error, path_str),
    }
}

#[unsafe(no_mangle)]
/// Check whether a path itself is a symbolic link without following it.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated C string for this call.
pub unsafe extern "C" fn mux_io_is_symlink(path: *const c_char) -> *mut Value {
    if path.is_null() {
        return io_err("path is null".to_string());
    }
    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
    match std::fs::symlink_metadata(&*path_str) {
        Ok(metadata) => io_ok(Value::Bool(metadata.file_type().is_symlink())),
        Err(error) => io_err_io_path(format!("Failed to inspect '{path_str}'"), &error, path_str),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::refcount::mux_rc_dec;
    use crate::result::mux_result_is_err;

    use super::{
        directories, directory_list_budget, directory_value, mux_fs_directory_close,
        mux_fs_directory_next, next_directory_id, read_bounded_line, DirectoryEntry,
        MAX_DIRECTORY_ENTRIES, MAX_DIRECTORY_NAME_BYTES, MAX_STDIN_LINE_BYTES,
    };

    #[test]
    fn directory_list_budget_accepts_boundaries_and_rejects_overflow() {
        assert_eq!(
            directory_list_budget(0, MAX_DIRECTORY_NAME_BYTES - 3, "abc")
                .expect("exact name-byte limit should be accepted"),
            (1, MAX_DIRECTORY_NAME_BYTES)
        );
        assert!(directory_list_budget(0, MAX_DIRECTORY_NAME_BYTES, "a").is_err());

        assert!(directory_list_budget(MAX_DIRECTORY_ENTRIES - 1, 0, "a").is_ok());
        assert!(directory_list_budget(MAX_DIRECTORY_ENTRIES, 0, "a").is_err());
    }

    #[test]
    fn directory_iteration_enforces_the_entry_budget() {
        let root = std::env::temp_dir().join(format!(
            "mux-directory-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir(&root).expect("create test directory");
        std::fs::write(root.join("entry"), b"entry").expect("create test entry");
        let id = next_directory_id();
        let entries = std::fs::read_dir(&root).expect("open test directory");
        directories().insert(
            id,
            DirectoryEntry {
                entries,
                refs: 1,
                entry_count: MAX_DIRECTORY_ENTRIES,
                entry_name_bytes: 0,
            },
        );
        let directory = directory_value(id);

        let result = unsafe { mux_fs_directory_next(directory) };
        assert!(unsafe { mux_result_is_err(result) });

        unsafe {
            mux_rc_dec(result);
            mux_fs_directory_close(directory);
            mux_rc_dec(directory);
        }
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn stdin_line_reader_rejects_oversized_lines_before_unbounded_growth() {
        let mut reader = Cursor::new(vec![b'x'; MAX_STDIN_LINE_BYTES + 1]);
        let error = read_bounded_line(&mut reader).expect_err("oversized stdin line");
        assert!(error.to_string().contains("16 MiB"));
    }

    #[test]
    fn stdin_line_reader_preserves_trimmed_line_behavior() {
        let mut reader = Cursor::new(b"  hello  \nnext".to_vec());
        assert_eq!(read_bounded_line(&mut reader).expect("first line"), "hello");
        assert_eq!(read_bounded_line(&mut reader).expect("second line"), "next");
    }
}
