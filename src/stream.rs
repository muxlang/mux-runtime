//! In-memory byte readers and writers used by std.io and as the first concrete
//! implementations of the standard byte-stream contract.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_dec;
use crate::std::StdErrorKind;
use crate::{TypeId, Value};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

const MAX_STREAM_BYTES: i64 = 16 * 1024 * 1024;

#[cfg(feature = "net")]
type TcpStream = std::net::TcpStream;

struct ReaderEntry {
    data: Vec<u8>,
    position: usize,
    remaining_limit: Option<usize>,
    tee_writer: Option<TeeWriterLease>,
    stdio: Option<StandardStream>,
    file: Option<File>,
    #[cfg(feature = "net")]
    socket: Option<TcpStream>,
}

struct WriterEntry {
    data: Vec<u8>,
    position: usize,
    capture: bool,
    append_mode: bool,
    file: Option<File>,
    stdio: Option<StandardStream>,
    #[cfg(feature = "net")]
    socket: Option<TcpStream>,
}

struct ErasedStreamEntry {
    reader: Option<i64>,
    writer: Option<i64>,
    names: usize,
}

#[derive(Clone, Copy)]
enum StandardStream {
    Stdin,
    Stdout,
    Stderr,
}

struct ReaderSlot {
    state: Mutex<ReaderEntry>,
    refs: AtomicUsize,
}

struct WriterSlot {
    state: Mutex<WriterEntry>,
    refs: AtomicUsize,
}

#[derive(Clone)]
struct TeeWriterLease {
    handle: i64,
    slot: Arc<WriterSlot>,
}

// Registry locks protect membership and reference counts. Entry locks protect
// stream state and I/O. Operations clone a slot while holding the registry
// lock, release the registry lock, then lock the slot. Cleanup removes a slot
// before touching tee state, so it never nests registry and entry locks.
static READERS: LazyLock<Mutex<HashMap<i64, Arc<ReaderSlot>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static WRITERS: LazyLock<Mutex<HashMap<i64, Arc<WriterSlot>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static STREAMS: LazyLock<Mutex<HashMap<i64, ErasedStreamEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static READER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Reader",
        size_of::<i64>(),
        Some(drop_reader as extern "C" fn(*mut c_void)),
        Some(copy_reader as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static WRITER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Writer",
        size_of::<i64>(),
        Some(drop_writer as extern "C" fn(*mut c_void)),
        Some(copy_writer as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static STREAM_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Stream",
        size_of::<i64>(),
        Some(drop_erased_stream as extern "C" fn(*mut c_void)),
        Some(copy_erased_stream as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_handle() -> i64 {
    let mut next = lock(&NEXT_HANDLE);
    let value = *next;
    *next = next.checked_add(1).unwrap_or(1);
    value
}

fn reader_slot(handle: i64) -> Result<Arc<ReaderSlot>, String> {
    lock(&READERS)
        .get(&handle)
        .cloned()
        .ok_or_else(|| "stream is closed or invalid".to_string())
}

fn writer_slot(handle: i64) -> Result<Arc<WriterSlot>, String> {
    lock(&WRITERS)
        .get(&handle)
        .cloned()
        .ok_or_else(|| "stream is closed or invalid".to_string())
}

fn new_reader_slot(entry: ReaderEntry) -> Arc<ReaderSlot> {
    Arc::new(ReaderSlot {
        state: Mutex::new(entry),
        refs: AtomicUsize::new(1),
    })
}

fn new_writer_slot(entry: WriterEntry) -> Arc<WriterSlot> {
    Arc::new(WriterSlot {
        state: Mutex::new(entry),
        refs: AtomicUsize::new(1),
    })
}

fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => crate::std::io_result_ok(value),
        Err(error) => crate::std::io_result_err(error),
    }
}

fn stream_os_error(operation: &str, error: &std::io::Error) -> *mut Value {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => StdErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => StdErrorKind::Permission,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => StdErrorKind::Invalid,
        std::io::ErrorKind::Unsupported => StdErrorKind::Unsupported,
        _ => StdErrorKind::Io,
    };
    crate::std::io_result_err_kind(
        kind,
        format!("{operation} failed: {error}"),
        operation.to_string(),
    )
}

fn object(type_id: TypeId, handle: i64) -> *mut Value {
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

fn stream_path(value: *const Value) -> Result<String, String> {
    match unsafe { value.as_ref() } {
        Some(Value::String(path)) if !path.is_empty() => Ok(path.clone()),
        Some(Value::String(_)) => Err("stream path must not be empty".to_string()),
        _ => Err("stream path must be a string".to_string()),
    }
}

fn handle(value: *const Value, type_id: TypeId, readers: bool) -> Result<i64, String> {
    if value.is_null() {
        return Err("stream value is null".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("invalid stream value".to_string());
    }
    if unsafe { crate::object::get_object_type_id(value) } != type_id {
        return Err("stream type mismatch".to_string());
    }
    let value = unsafe { *ptr.cast::<i64>() };
    let valid = if readers {
        lock(&READERS).contains_key(&value)
    } else {
        lock(&WRITERS).contains_key(&value)
    };
    if valid {
        Ok(value)
    } else {
        Err("stream is closed or invalid".to_string())
    }
}

fn copy_handle(source: *mut c_void, dest: *mut c_void, readers: bool) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let value = unsafe { *source.cast::<i64>() };
    let valid = if readers {
        let entries = lock(&READERS);
        entries.get(&value).is_some_and(|entry| {
            entry
                .refs
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
                    refs.checked_add(1)
                })
                .is_ok()
        })
    } else {
        let entries = lock(&WRITERS);
        entries.get(&value).is_some_and(|entry| {
            entry
                .refs
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
                    refs.checked_add(1)
                })
                .is_ok()
        })
    };
    unsafe { *dest.cast::<i64>() = if valid { value } else { 0 } };
}

extern "C" fn copy_reader(source: *mut c_void, dest: *mut c_void) {
    copy_handle(source, dest, true);
}

extern "C" fn copy_writer(source: *mut c_void, dest: *mut c_void) {
    copy_handle(source, dest, false);
}

fn erased_stream_entry(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *STREAM_TYPE_ID {
        return Err("stream type mismatch".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("invalid stream value".to_string());
    }
    let handle = unsafe { *(ptr as *const i64) };
    if lock(&STREAMS).contains_key(&handle) {
        Ok(handle)
    } else {
        Err("stream is closed or invalid".to_string())
    }
}

fn release_erased_stream(handle: i64) {
    let entry = {
        let mut streams = lock(&STREAMS);
        let remove = streams.get_mut(&handle).is_some_and(|entry| {
            entry.names = entry.names.saturating_sub(1);
            entry.names == 0
        });
        remove.then(|| streams.remove(&handle)).flatten()
    };
    if let Some(entry) = entry {
        if let Some(reader) = entry.reader {
            release(reader, true);
        }
        if let Some(writer) = entry.writer {
            release(writer, false);
        }
    }
}

fn close_erased_stream(handle: i64) {
    let entry = lock(&STREAMS).remove(&handle);
    if let Some(entry) = entry {
        if let Some(reader) = entry.reader {
            release(reader, true);
        }
        if let Some(writer) = entry.writer {
            release(writer, false);
        }
    }
}

extern "C" fn drop_erased_stream(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_erased_stream(unsafe { *ptr.cast::<i64>() });
    }
}

extern "C" fn copy_erased_stream(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    let valid = lock(&STREAMS).get_mut(&handle).is_some_and(|entry| {
        entry.names = entry.names.saturating_add(1);
        true
    });
    unsafe { *dest.cast::<i64>() = if valid { handle } else { 0 } };
}

fn release(handle: i64, readers: bool) {
    if readers {
        // A tee retains its target writer independently of the reader's own
        // names.  Releasing the last reader name must release that retained
        // reference too; otherwise dropping a reader without an explicit
        // `close()` leaks the writer entry forever.
        let tee_writer = {
            let mut entries = lock(&READERS);
            let Some(entry) = entries.get(&handle).cloned() else {
                return;
            };
            let remove = entry
                .refs
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
                    refs.checked_sub(1)
                })
                .is_ok_and(|refs| refs == 1);
            if remove {
                entries.remove(&handle);
            }
            remove.then_some(entry)
        };
        if let Some(entry) = tee_writer {
            let tee_writer = lock(&entry.state).tee_writer.take();
            if let Some(tee_writer) = tee_writer {
                release_writer(tee_writer.handle);
            }
        }
    } else {
        let mut entries = lock(&WRITERS);
        let Some(entry) = entries.get(&handle).cloned() else {
            return;
        };
        let remove = entry
            .refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
                refs.checked_sub(1)
            })
            .is_ok_and(|refs| refs == 1);
        if remove {
            entries.remove(&handle);
        }
    }
}

fn close_stream(value: *mut Value, readers: bool) {
    if value.is_null() {
        return;
    }
    let expected = if readers {
        *READER_TYPE_ID
    } else {
        *WRITER_TYPE_ID
    };
    if unsafe { get_object_type_id(value) } != expected {
        return;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle <= 0 {
        return;
    }
    if readers {
        let entry = { lock(&READERS).remove(&handle) };
        let tee_writer = entry.and_then(|entry| lock(&entry.state).tee_writer.take());
        if let Some(tee_writer) = tee_writer {
            release_writer(tee_writer.handle);
        }
    } else {
        let _entry = { lock(&WRITERS).remove(&handle) };
    }
    unsafe { *ptr.cast::<i64>() = 0 };
}

pub(crate) fn writer_handle(value: *const Value) -> Result<i64, String> {
    handle(value, *WRITER_TYPE_ID, false)
}

/// Resolve a reader handle for another runtime component that needs to adapt
/// the shared `std.io.Reader` contract to a Rust `Read` implementation.
///
/// The handle is borrowed; callers that retain it beyond the current call
/// must pair it with `retain_reader` and `release_reader`.
pub(crate) fn reader_handle(value: *const Value) -> Result<i64, String> {
    handle(value, *READER_TYPE_ID, true)
}

pub(crate) fn retain_reader(handle: i64) -> Result<(), String> {
    let readers = lock(&READERS);
    let entry = readers
        .get(&handle)
        .ok_or_else(|| "stream is closed or invalid".to_string())?;
    entry
        .refs
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
            refs.checked_add(1)
        })
        .map_err(|_| "reader reference count overflow".to_string())?;
    Ok(())
}

pub(crate) fn release_reader(handle: i64) {
    release(handle, true);
}

/// Adapt a retained Mux `io.Reader` handle to Rust's synchronous `Read`
/// contract.  The adapter owns one reference to the underlying reader and
/// releases it when the Rust consumer is finished.  This is used by
/// protocol clients such as HTTP that can consume a body incrementally.
pub(crate) struct ReaderAdapter {
    handle: i64,
}

impl ReaderAdapter {
    pub(crate) fn from_value(value: *const Value) -> Result<Self, String> {
        let handle = reader_handle(value)?;
        retain_reader(handle)?;
        Ok(Self { handle })
    }
}

impl Read for ReaderAdapter {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let bytes = read_reader(self.handle, buffer.len()).map_err(std::io::Error::other)?;
        let count = bytes.len();
        buffer[..count].copy_from_slice(&bytes);
        Ok(count)
    }
}

impl Drop for ReaderAdapter {
    fn drop(&mut self) {
        release_reader(self.handle);
    }
}

/// Read bytes from a retained reader handle without constructing a temporary
/// Mux `Value`. This is the bridge used by incremental stdlib parsers.
pub(crate) fn read_reader(handle: i64, requested: usize) -> Result<Vec<u8>, String> {
    reader_read_up_to(handle, requested)
}

pub(crate) fn retain_writer(handle: i64) -> Result<(), String> {
    let writers = lock(&WRITERS);
    let entry = writers
        .get(&handle)
        .ok_or_else(|| "stream is closed or invalid".to_string())?;
    entry
        .refs
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
            refs.checked_add(1)
        })
        .map_err(|_| "writer reference count overflow".to_string())?;
    Ok(())
}

pub(crate) fn release_writer(handle: i64) {
    release(handle, false);
}

/// Flush a retained writer handle without constructing a temporary Mux value.
/// The operation deliberately mirrors `mux_io_writer_flush`, so adapters such
/// as CSV writers preserve the same file/socket/stdio semantics.
pub(crate) fn flush_writer(handle: i64) -> Result<(), String> {
    let slot = writer_slot(handle)?;
    let mut entry = lock(&slot.state);
    #[cfg(feature = "net")]
    let socket_result = entry.socket.as_mut().map(TcpStream::flush);
    #[cfg(not(feature = "net"))]
    let socket_result: Option<std::io::Result<()>> = None;
    let file_result = entry.file.as_mut().map(File::flush);
    let stdio_result = entry.stdio.map(|stream| match stream {
        StandardStream::Stdout => std::io::stdout().flush(),
        StandardStream::Stderr => std::io::stderr().flush(),
        StandardStream::Stdin => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "stdin is not writable",
        )),
    });
    match socket_result.or(file_result).or(stdio_result) {
        Some(Ok(())) | None => Ok(()),
        Some(Err(error)) => Err(format!("writer flush failed: {error}")),
    }
}

fn write_writer_slot(slot: &WriterSlot, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_STREAM_BYTES as usize {
        return Err("write exceeds the 16 MiB limit".to_string());
    }
    let mut entry = lock(&slot.state);
    let Some(end) = entry.position.checked_add(bytes.len()) else {
        return Err("writer position exceeds the stream limit".to_string());
    };
    if entry.capture && end > MAX_STREAM_BYTES as usize {
        return Err("writer buffer exceeds the 16 MiB limit".to_string());
    }
    if let Some(stream) = entry.stdio {
        let result = match stream {
            StandardStream::Stdout => std::io::stdout().write_all(bytes),
            StandardStream::Stderr => std::io::stderr().write_all(bytes),
            StandardStream::Stdin => return Err("stdin is not writable".to_string()),
        };
        result.map_err(|error| format!("standard stream write failed: {error}"))?;
    }
    if let Some(file) = entry.file.as_mut() {
        file.write_all(bytes)
            .map_err(|error| format!("writer file write failed: {error}"))?;
    }
    #[cfg(feature = "net")]
    if let Some(socket) = entry.socket.as_mut() {
        socket
            .write_all(bytes)
            .map_err(|error| format!("socket writer write failed: {error}"))?;
    }
    if entry.capture {
        let position = entry.position;
        if position > entry.data.len() {
            entry.data.resize(position, 0);
        }
        if end > entry.data.len() {
            entry.data.resize(end, 0);
        }
        entry.data[position..end].copy_from_slice(bytes);
    }
    entry.position = end;
    Ok(())
}

pub(crate) fn write_writer(handle: i64, bytes: &[u8]) -> Result<(), String> {
    let slot = writer_slot(handle)?;
    write_writer_slot(&slot, bytes)
}

fn reader_read_up_to(handle: i64, requested: usize) -> Result<Vec<u8>, String> {
    // Internal Rust `Read` adapters can receive an arbitrarily large caller
    // buffer. Keep that bridge subject to the same per-operation bound as the
    // public `Reader.read` entry point instead of allocating an untrusted
    // buffer size for file or socket-backed readers.
    let requested = requested.min(MAX_STREAM_BYTES as usize);
    let (bytes, tee_writer) = {
        let slot = reader_slot(handle)?;
        let mut entry = lock(&slot.state);
        let size = entry
            .remaining_limit
            .map_or(requested, |remaining| remaining.min(requested));
        if size == 0 {
            return Ok(Vec::new());
        }
        let bytes = {
            if let Some(stream) = entry.stdio {
                let mut bytes = vec![0; size];
                let count = match stream {
                    StandardStream::Stdin => std::io::stdin().read(&mut bytes),
                    StandardStream::Stdout | StandardStream::Stderr => {
                        return Err("standard output streams are not readable".to_string())
                    }
                }
                .map_err(|error| format!("standard stream read failed: {error}"))?;
                bytes.truncate(count);
                bytes
            } else {
                #[cfg(feature = "net")]
                if let Some(socket) = entry.socket.as_mut() {
                    let mut bytes = vec![0; size];
                    let count = socket
                        .read(&mut bytes)
                        .map_err(|error| format!("socket reader read failed: {error}"))?;
                    bytes.truncate(count);
                    bytes
                } else if let Some(file) = entry.file.as_mut() {
                    let mut bytes = vec![0; size];
                    let count = file
                        .read(&mut bytes)
                        .map_err(|error| format!("file reader read failed: {error}"))?;
                    bytes.truncate(count);
                    bytes
                } else {
                    let position = entry.position;
                    let end = position.saturating_add(size).min(entry.data.len());
                    entry.data[position..end].to_vec()
                }
                #[cfg(not(feature = "net"))]
                {
                    if let Some(file) = entry.file.as_mut() {
                        let mut bytes = vec![0; size];
                        let count = file
                            .read(&mut bytes)
                            .map_err(|error| format!("file reader read failed: {error}"))?;
                        bytes.truncate(count);
                        bytes
                    } else {
                        let position = entry.position;
                        let end = position.saturating_add(size).min(entry.data.len());
                        entry.data[position..end].to_vec()
                    }
                }
            }
        };
        entry.position = entry.position.saturating_add(bytes.len());
        if let Some(remaining) = entry.remaining_limit.as_mut() {
            *remaining = remaining.saturating_sub(bytes.len());
        }
        (bytes, entry.tee_writer.clone())
    };
    if let Some(tee_writer) = tee_writer {
        if !bytes.is_empty() {
            write_writer_slot(&tee_writer.slot, &bytes)?;
        }
    }
    Ok(bytes)
}

extern "C" fn drop_reader(ptr: *mut c_void) {
    if !ptr.is_null() {
        release(unsafe { *ptr.cast::<i64>() }, true);
    }
}

extern "C" fn drop_writer(ptr: *mut c_void) {
    if !ptr.is_null() {
        release(unsafe { *ptr.cast::<i64>() }, false);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_io_reader_new() -> *mut Value {
    let handle = next_handle();
    lock(&READERS).insert(
        handle,
        new_reader_slot(ReaderEntry {
            data: Vec::new(),
            position: 0,
            remaining_limit: None,
            tee_writer: None,
            stdio: None,
            file: None,
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*READER_TYPE_ID, handle);
    if value.is_null() {
        lock(&READERS).remove(&handle);
    }
    value
}

fn reader_from_standard(stream: StandardStream) -> *mut Value {
    let handle = next_handle();
    lock(&READERS).insert(
        handle,
        new_reader_slot(ReaderEntry {
            data: Vec::new(),
            position: 0,
            remaining_limit: None,
            tee_writer: None,
            stdio: Some(stream),
            file: None,
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*READER_TYPE_ID, handle);
    if value.is_null() {
        lock(&READERS).remove(&handle);
    }
    value
}

/// Returns a reader for the process's standard input.
#[unsafe(no_mangle)]
pub extern "C" fn mux_io_stdin() -> *mut Value {
    reader_from_standard(StandardStream::Stdin)
}

#[unsafe(no_mangle)]
/// # Safety
/// `reader` must be null or a valid stream `Value` pointer.
pub unsafe extern "C" fn mux_io_reader_close(reader: *mut Value) {
    close_stream(reader, true);
}

#[unsafe(no_mangle)]
/// # Safety
/// `writer` must be null or a valid stream `Value` pointer.
pub unsafe extern "C" fn mux_io_writer_close(writer: *mut Value) {
    close_stream(writer, false);
}

fn erased_from_handle(handle: i64, reader: bool) -> *mut Value {
    let retained = if reader {
        retain_reader(handle).is_ok()
    } else {
        retain_writer(handle).is_ok()
    };
    if !retained {
        return result(Err("stream is closed or invalid".to_string()));
    }
    let erased_handle = next_handle();
    lock(&STREAMS).insert(
        erased_handle,
        ErasedStreamEntry {
            reader: reader.then_some(handle),
            writer: (!reader).then_some(handle),
            names: 1,
        },
    );
    let value = object(*STREAM_TYPE_ID, erased_handle);
    if value.is_null() {
        lock(&STREAMS).remove(&erased_handle);
        release(handle, reader);
        return result(Err("failed to allocate stream handle".to_string()));
    }
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    result(Ok(owned))
}

/// Wrap a reader capability in an owned erased stream.
///
/// # Safety
/// `reader` must be null or point to a live Mux value for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_from_reader(reader: *const Value) -> *mut Value {
    match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => erased_from_handle(handle, true),
        Err(error) => result(Err(error)),
    }
}

/// Wrap a writer capability in an owned erased stream.
///
/// # Safety
/// `writer` must be null or point to a live Mux value for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_from_writer(writer: *const Value) -> *mut Value {
    match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => erased_from_handle(handle, false),
        Err(error) => result(Err(error)),
    }
}

/// Explicitly close an erased stream.
///
/// # Safety
/// `stream` must be null or point to a live erased-stream value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_close(stream: *mut Value) {
    let Ok(handle) = erased_stream_entry(stream) else {
        return;
    };
    close_erased_stream(handle);
    let ptr = unsafe { get_object_ptr(stream) };
    if !ptr.is_null() {
        unsafe { *(ptr as *mut i64) = 0 };
    }
}

fn erased_reader_value(handle: i64) -> Result<*mut Value, String> {
    retain_reader(handle)?;
    let value = object(*READER_TYPE_ID, handle);
    if value.is_null() {
        release(handle, true);
        Err("could not allocate reader capability".to_string())
    } else {
        Ok(value)
    }
}

fn erased_writer_value(handle: i64) -> Result<*mut Value, String> {
    retain_writer(handle)?;
    let value = object(*WRITER_TYPE_ID, handle);
    if value.is_null() {
        release(handle, false);
        Err("could not allocate writer capability".to_string())
    } else {
        Ok(value)
    }
}

/// Read from an erased stream.
///
/// # Safety
/// `stream` must be null or point to a live erased-stream value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_read(stream: *const Value, size: i64) -> *mut Value {
    let operation = erased_stream_entry(stream).and_then(|handle| {
        let reader = lock(&STREAMS)
            .get(&handle)
            .and_then(|entry| entry.reader)
            .ok_or_else(|| "stream does not support read".to_string())?;
        let value = erased_reader_value(reader)?;
        let output = unsafe { mux_io_reader_read(value, size) };
        unsafe { mux_rc_dec(value) };
        Ok(output)
    });
    match operation {
        Ok(value) => value,
        Err(error) => result(Err(error)),
    }
}

/// Write to an erased stream.
///
/// # Safety
/// `stream` and `bytes` must be null or point to live Mux values for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_write(
    stream: *const Value,
    bytes: *const Value,
) -> *mut Value {
    let operation = erased_stream_entry(stream).and_then(|handle| {
        let writer = lock(&STREAMS)
            .get(&handle)
            .and_then(|entry| entry.writer)
            .ok_or_else(|| "stream does not support write".to_string())?;
        let value = erased_writer_value(writer)?;
        let output = unsafe { mux_io_writer_write(value, bytes) };
        unsafe { mux_rc_dec(value) };
        Ok(output)
    });
    match operation {
        Ok(value) => value,
        Err(error) => result(Err(error)),
    }
}

/// Flush an erased stream.
///
/// # Safety
/// `stream` must be null or point to a live erased-stream value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_flush(stream: *const Value) -> *mut Value {
    let operation = erased_stream_entry(stream).and_then(|handle| {
        let writer = lock(&STREAMS)
            .get(&handle)
            .and_then(|entry| entry.writer)
            .ok_or_else(|| "stream does not support flush".to_string())?;
        let value = erased_writer_value(writer)?;
        let output = unsafe { mux_io_writer_flush(value) };
        unsafe { mux_rc_dec(value) };
        Ok(output)
    });
    match operation {
        Ok(value) => value,
        Err(error) => result(Err(error)),
    }
}

/// Seek an erased stream.
///
/// # Safety
/// `stream` must be null or point to a live erased-stream value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_stream_seek(stream: *const Value, position: i64) -> *mut Value {
    let operation = erased_stream_entry(stream).and_then(|handle| {
        let entry = lock(&STREAMS);
        if let Some(reader) = entry.get(&handle).and_then(|entry| entry.reader) {
            drop(entry);
            let value = erased_reader_value(reader)?;
            let output = unsafe { mux_io_reader_seek(value, position) };
            unsafe { mux_rc_dec(value) };
            return Ok(output);
        }
        if let Some(writer) = entry.get(&handle).and_then(|entry| entry.writer) {
            drop(entry);
            let value = erased_writer_value(writer)?;
            let output = unsafe { mux_io_writer_seek(value, position) };
            unsafe { mux_rc_dec(value) };
            return Ok(output);
        }
        Err("stream does not support seek".to_string())
    });
    match operation {
        Ok(value) => value,
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
/// Creates a reader over a copied byte buffer.
///
/// # Safety
/// `bytes` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_reader_from_bytes(bytes: *const Value) -> *mut Value {
    let data = match bytes.as_ref() {
        Some(Value::Bytes(data)) => data.clone(),
        _ => return std::ptr::null_mut(),
    };
    let handle = next_handle();
    lock(&READERS).insert(
        handle,
        new_reader_slot(ReaderEntry {
            data,
            position: 0,
            remaining_limit: None,
            tee_writer: None,
            stdio: None,
            file: None,
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*READER_TYPE_ID, handle);
    if value.is_null() {
        lock(&READERS).remove(&handle);
    }
    value
}

#[unsafe(no_mangle)]
/// Opens a streaming file-backed reader. Individual reads are bounded to the
/// reader's 16 MiB operation limit; the file is not copied into memory.
///
/// # Safety
/// `path` must be null or point to a live string `Value` for the duration of
/// this call.
pub unsafe extern "C" fn mux_io_reader_from_file(path: *const Value) -> *mut Value {
    let path = match stream_path(path) {
        Ok(path) => path,
        Err(error) => return result(Err(error)),
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => return stream_os_error("reader.from_file", &error),
    };
    let handle = next_handle();
    lock(&READERS).insert(
        handle,
        new_reader_slot(ReaderEntry {
            data: Vec::new(),
            position: 0,
            remaining_limit: None,
            tee_writer: None,
            stdio: None,
            file: Some(file),
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*READER_TYPE_ID, handle);
    if value.is_null() {
        lock(&READERS).remove(&handle);
        return result(Err("failed to allocate reader handle".to_string()));
    }
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    result(Ok(owned))
}

#[unsafe(no_mangle)]
/// Reads up to `size` bytes from a reader.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_reader_read(reader: *const Value, size: i64) -> *mut Value {
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    if !(0..=MAX_STREAM_BYTES).contains(&size) {
        return result(Err("read size must be between 0 and 16 MiB".to_string()));
    }
    match reader_read_up_to(handle, size as usize) {
        Ok(bytes) => result(Ok(Value::Bytes(bytes))),
        Err(error) => result(Err(error)),
    }
}

/// Reads one UTF-8 line, excluding its trailing LF or CRLF. EOF before any
/// bytes returns `none`; EOF after a final unterminated line returns that line.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_reader_read_line(reader: *const Value) -> *mut Value {
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let mut line = Vec::new();
    loop {
        let byte = match reader_read_up_to(handle, 1) {
            Ok(bytes) => bytes,
            Err(error) => return result(Err(error)),
        };
        if byte.is_empty() {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_STREAM_BYTES as usize {
            return result(Err("line exceeds the 16 MiB stream limit".to_string()));
        }
        if byte[0] == b'\n' {
            break;
        }
    }
    if line.is_empty() {
        return result(Ok(Value::Optional(None)));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    let Ok(line) = String::from_utf8(line) else {
        return result(Err("reader line is not valid UTF-8".to_string()));
    };
    result(Ok(Value::Optional(Some(Box::new(Value::String(line))))))
}

/// Limit the total number of bytes this reader may return from now on.
///
/// # Safety
/// reader must be null or point to a live Reader value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_reader_limit(reader: *const Value, limit: i64) -> *mut Value {
    if !(0..=MAX_STREAM_BYTES).contains(&limit) {
        return result(Err("reader limit must be between 0 and 16 MiB".to_string()));
    }
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let slot = match reader_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let mut entry = lock(&slot.state);
    entry.remaining_limit = Some(limit as usize);
    result(Ok(Value::Unit))
}

/// Copy every subsequent read into writer as well as returning it to the
/// caller. The writer is retained until untee or reader close.
///
/// # Safety
/// reader and writer must be null or point to live stream values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_reader_tee(
    reader: *const Value,
    writer: *const Value,
) -> *mut Value {
    let reader_handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let writer_handle = match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    if let Err(error) = retain_writer(writer_handle) {
        return result(Err(error));
    }
    let slot = match reader_slot(reader_handle) {
        Ok(slot) => slot,
        Err(error) => {
            release_writer(writer_handle);
            return result(Err(error));
        }
    };
    let writer_slot = match writer_slot(writer_handle) {
        Ok(slot) => slot,
        Err(error) => {
            release_writer(writer_handle);
            return result(Err(error));
        }
    };
    let old_writer = {
        let mut entry = lock(&slot.state);
        entry.tee_writer.replace(TeeWriterLease {
            handle: writer_handle,
            slot: writer_slot,
        })
    };
    if let Some(old_writer) = old_writer {
        release_writer(old_writer.handle);
    }
    result(Ok(Value::Unit))
}

/// Stop teeing future reads from this reader.
///
/// # Safety
/// reader must be null or point to a live Reader value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_reader_untee(reader: *const Value) -> *mut Value {
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let slot = match reader_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let old_writer = {
        let mut entry = lock(&slot.state);
        entry.tee_writer.take()
    };
    if let Some(old_writer) = old_writer {
        release_writer(old_writer.handle);
    }
    result(Ok(Value::Unit))
}

/// Returns the number of unread bytes in a reader.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_reader_remaining(reader: *const Value) -> *mut Value {
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let slot = match reader_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let mut entry = lock(&slot.state);
    #[cfg(feature = "net")]
    if entry.socket.is_some() {
        return result(Err(
            "remaining is not available for a socket-backed reader".to_string()
        ));
    }
    if entry.stdio.is_some() {
        return result(Err(
            "remaining is not available for a standard input reader".to_string(),
        ));
    }
    if let Some(file) = entry.file.as_mut() {
        let end = match file.metadata() {
            Ok(metadata) => metadata.len().min(usize::MAX as u64) as usize,
            Err(error) => return result(Err(format!("file reader metadata failed: {error}"))),
        };
        let remaining = end.saturating_sub(entry.position);
        let remaining = entry
            .remaining_limit
            .map_or(remaining, |limit| limit.min(remaining));
        return result(Ok(Value::Int(remaining as i64)));
    }
    let remaining = entry.data.len().saturating_sub(entry.position);
    let remaining = entry
        .remaining_limit
        .map_or(remaining, |limit| limit.min(remaining)) as i64;
    result(Ok(Value::Int(remaining)))
}

/// Reads all remaining bytes, bounded by `limit`.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_reader_read_to_end(reader: *const Value, limit: i64) -> *mut Value {
    if !(0..=MAX_STREAM_BYTES).contains(&limit) {
        return result(Err("read limit must be between 0 and 16 MiB".to_string()));
    }
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let mut output = Vec::new();
    while output.len() < limit as usize {
        let chunk_size = (limit as usize - output.len()).min(8192);
        let chunk = match reader_read_up_to(handle, chunk_size) {
            Ok(chunk) => chunk,
            Err(error) => return result(Err(error)),
        };
        if chunk.is_empty() {
            break;
        }
        output.extend_from_slice(&chunk);
    }
    result(Ok(Value::Bytes(output)))
}

#[unsafe(no_mangle)]
/// Copies up to `limit` remaining bytes into a writer and returns the count.
/// Both streams remain open and the reader advances by the bytes copied.
///
/// # Safety
/// `reader` and `writer` must be null or point to live stream `Value`s for the
/// duration of this call.
pub unsafe extern "C" fn mux_io_reader_copy_to(
    reader: *const Value,
    writer: *const Value,
    limit: i64,
) -> *mut Value {
    if !(0..=MAX_STREAM_BYTES).contains(&limit) {
        return result(Err("copy limit must be between 0 and 16 MiB".to_string()));
    }
    let reader_handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let writer_handle = match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let mut total = 0usize;
    while total < limit as usize {
        let chunk = match reader_read_up_to(reader_handle, (limit as usize - total).min(8192)) {
            Ok(chunk) => chunk,
            Err(error) => return result(Err(error)),
        };
        if chunk.is_empty() {
            break;
        }
        if let Err(error) = write_writer(writer_handle, &chunk) {
            return result(Err(error));
        }
        total += chunk.len();
    }
    result(Ok(Value::Int(total as i64)))
}

#[unsafe(no_mangle)]
/// Reads exactly `size` bytes or returns an EOF error.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_reader_read_exact(reader: *const Value, size: i64) -> *mut Value {
    if !(0..=MAX_STREAM_BYTES).contains(&size) {
        return result(Err(
            "read-exact size must be between 0 and 16 MiB".to_string()
        ));
    }
    let target = size as usize;
    let mut output = Vec::with_capacity(target);
    while output.len() < target {
        let chunk = mux_io_reader_read(reader, (target - output.len()) as i64);
        let bytes = match unsafe { &*chunk } {
            Value::Result(Ok(value)) => {
                if let Value::Bytes(bytes) = value.as_ref() {
                    bytes.clone()
                } else {
                    mux_rc_dec(chunk);
                    return result(Err("reader returned a non-bytes value".to_string()));
                }
            }
            Value::Result(Err(_)) => return chunk,
            _ => {
                mux_rc_dec(chunk);
                return result(Err("reader returned an invalid result".to_string()));
            }
        };
        mux_rc_dec(chunk);
        if bytes.is_empty() {
            return result(Err(
                "reader reached EOF before the requested size".to_string()
            ));
        }
        output.extend_from_slice(&bytes);
    }
    result(Ok(Value::Bytes(output)))
}

#[unsafe(no_mangle)]
/// Returns the current reader position.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_reader_position(reader: *const Value) -> *mut Value {
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let position = reader_slot(handle)
        .map(|slot| lock(&slot.state).position as i64)
        .unwrap_or(0);
    result(Ok(Value::Int(position)))
}

#[unsafe(no_mangle)]
/// Moves a reader cursor to an existing position.
///
/// # Safety
/// `reader` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_reader_seek(reader: *const Value, position: i64) -> *mut Value {
    let handle = match handle(reader, *READER_TYPE_ID, true) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    if position < 0 {
        return result(Err("reader position must not be negative".to_string()));
    }
    let slot = match reader_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let mut entry = lock(&slot.state);
    if entry.stdio.is_some() {
        return result(Err(
            "seek is not available for a standard input reader".to_string()
        ));
    }
    #[cfg(feature = "net")]
    if entry.socket.is_some() {
        return result(Err(
            "seek is not available for a socket-backed reader".to_string()
        ));
    }
    if let Some(file) = entry.file.as_mut() {
        let end = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => return result(Err(format!("file reader metadata failed: {error}"))),
        };
        if position as u64 > end {
            return result(Err("reader position exceeds the file".to_string()));
        }
        if let Err(error) = file.seek(SeekFrom::Start(position as u64)) {
            return result(Err(format!("file reader seek failed: {error}")));
        }
        entry.position = position as usize;
        return result(Ok(Value::Int(position)));
    }
    if position as usize > entry.data.len() {
        return result(Err("reader position exceeds the buffer".to_string()));
    }
    entry.position = position as usize;
    result(Ok(Value::Int(position)))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_io_writer_new() -> *mut Value {
    let handle = next_handle();
    lock(&WRITERS).insert(
        handle,
        new_writer_slot(WriterEntry {
            data: Vec::new(),
            position: 0,
            capture: true,
            append_mode: false,
            file: None,
            stdio: None,
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*WRITER_TYPE_ID, handle);
    if value.is_null() {
        lock(&WRITERS).remove(&handle);
    }
    value
}

fn writer_from_standard(stream: StandardStream) -> *mut Value {
    let handle = next_handle();
    lock(&WRITERS).insert(
        handle,
        new_writer_slot(WriterEntry {
            data: Vec::new(),
            position: 0,
            capture: false,
            append_mode: false,
            file: None,
            stdio: Some(stream),
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*WRITER_TYPE_ID, handle);
    if value.is_null() {
        lock(&WRITERS).remove(&handle);
    }
    value
}

/// Returns a writer for the process's standard output.
#[unsafe(no_mangle)]
pub extern "C" fn mux_io_stdout() -> *mut Value {
    writer_from_standard(StandardStream::Stdout)
}

/// Returns a writer for the process's standard error.
#[unsafe(no_mangle)]
pub extern "C" fn mux_io_stderr() -> *mut Value {
    writer_from_standard(StandardStream::Stderr)
}

#[unsafe(no_mangle)]
/// Opens a file-backed writer, truncating the destination at construction.
/// Writes are bounded to 16 MiB and are flushed explicitly with `flush`.
///
/// # Safety
/// `path` must be null or point to a live string `Value` for the duration of
/// this call.
pub unsafe extern "C" fn mux_io_writer_to_file(path: *const Value) -> *mut Value {
    let path = match stream_path(path) {
        Ok(path) => path,
        Err(error) => return result(Err(error)),
    };
    let file = match OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => return stream_os_error("writer.to_file", &error),
    };
    let handle = next_handle();
    lock(&WRITERS).insert(
        handle,
        new_writer_slot(WriterEntry {
            data: Vec::new(),
            position: 0,
            capture: true,
            append_mode: false,
            file: Some(file),
            stdio: None,
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*WRITER_TYPE_ID, handle);
    if value.is_null() {
        lock(&WRITERS).remove(&handle);
        return result(Err("failed to allocate writer handle".to_string()));
    }
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    result(Ok(owned))
}

/// Opens a file-backed writer in append mode. Existing contents are preserved;
/// each write is added at the end of the file. Writes are bounded to 16 MiB
/// and are flushed explicitly with `flush`.
///
/// # Safety
/// `path` must be null or point to a live string `Value` for the duration of
/// this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_writer_append_file(path: *const Value) -> *mut Value {
    let path = match stream_path(path) {
        Ok(path) => path,
        Err(error) => return result(Err(error)),
    };
    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(error) => return stream_os_error("writer.append_file", &error),
    };
    let handle = next_handle();
    lock(&WRITERS).insert(
        handle,
        new_writer_slot(WriterEntry {
            data: Vec::new(),
            position: 0,
            capture: true,
            append_mode: true,
            file: Some(file),
            stdio: None,
            #[cfg(feature = "net")]
            socket: None,
        }),
    );
    let value = object(*WRITER_TYPE_ID, handle);
    if value.is_null() {
        lock(&WRITERS).remove(&handle);
        return result(Err("failed to allocate writer handle".to_string()));
    }
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    result(Ok(owned))
}

#[unsafe(no_mangle)]
/// Appends bytes to a writer and returns the number written.
///
/// # Safety
/// `writer` and `bytes` must be null or point to live `Value`s for the duration of this call.
pub unsafe extern "C" fn mux_io_writer_write(
    writer: *const Value,
    bytes: *const Value,
) -> *mut Value {
    let handle = match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let Some(Value::Bytes(bytes)) = (unsafe { bytes.as_ref() }) else {
        return result(Err("writer input must be bytes".to_string()));
    };
    match write_writer(handle, bytes) {
        Ok(()) => result(Ok(Value::Int(bytes.len() as i64))),
        Err(error) => result(Err(error)),
    }
}

/// Appends all bytes to an in-memory writer.
///
/// # Safety
/// `writer` and `bytes` must be null or point to live `Value`s for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_writer_write_all(
    writer: *const Value,
    bytes: *const Value,
) -> *mut Value {
    let written = mux_io_writer_write(writer, bytes);
    let is_error = matches!(unsafe { &*written }, Value::Result(Err(_)));
    if is_error {
        return written;
    }
    unsafe { mux_rc_dec(written) };
    result(Ok(Value::Unit))
}

#[unsafe(no_mangle)]
/// Returns a copy of all bytes accumulated by a writer.
///
/// # Safety
/// `writer` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_writer_bytes(writer: *const Value) -> *mut Value {
    let handle = match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let slot = match writer_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let entry = lock(&slot.state);
    if !entry.capture {
        return result(Err(
            "bytes is not available for a non-buffered standard stream".to_string(),
        ));
    }
    let bytes = entry.data.clone();
    result(Ok(Value::Bytes(bytes)))
}

/// Returns the current cursor position for a seekable writer.
///
/// # Safety
/// `writer` must be null or point to a live `Writer` value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_writer_position(writer: *const Value) -> *mut Value {
    let handle = match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let slot = match writer_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let entry = lock(&slot.state);
    if entry.stdio.is_some() {
        return result(Err(
            "position is not available for a standard stream".to_string()
        ));
    }
    if entry.append_mode {
        return result(Err(
            "position is not available for an append-mode writer".to_string()
        ));
    }
    #[cfg(feature = "net")]
    if entry.socket.is_some() {
        return result(Err(
            "position is not available for a socket-backed writer".to_string()
        ));
    }
    result(Ok(Value::Int(entry.position as i64)))
}

/// Moves a seekable writer cursor to an existing position.
///
/// # Safety
/// `writer` must be null or point to a live `Writer` value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_writer_seek(writer: *const Value, position: i64) -> *mut Value {
    if !(0..=MAX_STREAM_BYTES).contains(&position) {
        return result(Err(format!(
            "writer position must be between 0 and {MAX_STREAM_BYTES}"
        )));
    }
    let handle = match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => handle,
        Err(error) => return result(Err(error)),
    };
    let slot = match writer_slot(handle) {
        Ok(slot) => slot,
        Err(error) => return result(Err(error)),
    };
    let mut entry = lock(&slot.state);
    if entry.stdio.is_some() {
        return result(Err(
            "seek is not available for a standard stream".to_string()
        ));
    }
    #[cfg(feature = "net")]
    if entry.socket.is_some() {
        return result(Err(
            "seek is not available for a socket-backed writer".to_string()
        ));
    }
    if entry.append_mode {
        return result(Err(
            "seek is not available for an append-mode writer".to_string()
        ));
    }
    let position = position as usize;
    if let Some(file) = entry.file.as_mut() {
        let end = match file.metadata() {
            Ok(metadata) => metadata.len().min(usize::MAX as u64) as usize,
            Err(error) => return result(Err(format!("writer metadata failed: {error}"))),
        };
        if position > end {
            return result(Err("writer position exceeds the file".to_string()));
        }
        if let Err(error) = file.seek(SeekFrom::Start(position as u64)) {
            return result(Err(format!("writer seek failed: {error}")));
        }
    } else if position > entry.data.len() {
        return result(Err("writer position exceeds the buffer".to_string()));
    }
    entry.position = position;
    result(Ok(Value::Int(position as i64)))
}

#[unsafe(no_mangle)]
/// Flushes a writer. In-memory writers have nothing to flush, but the handle is validated.
///
/// # Safety
/// `writer` must be null or point to a live `Value` for the duration of this call.
pub unsafe extern "C" fn mux_io_writer_flush(writer: *const Value) -> *mut Value {
    match handle(writer, *WRITER_TYPE_ID, false) {
        Ok(handle) => {
            let slot = match writer_slot(handle) {
                Ok(slot) => slot,
                Err(error) => return result(Err(error)),
            };
            let mut entry = lock(&slot.state);
            #[cfg(feature = "net")]
            let socket_result = entry.socket.as_mut().map(TcpStream::flush);
            #[cfg(not(feature = "net"))]
            let socket_result: Option<std::io::Result<()>> = None;
            let file_result = entry.file.as_mut().map(File::flush);
            let stdio_result = entry.stdio.map(|stream| match stream {
                StandardStream::Stdout => std::io::stdout().flush(),
                StandardStream::Stderr => std::io::stderr().flush(),
                StandardStream::Stdin => Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "stdin is not writable",
                )),
            });
            match socket_result.or(file_result).or(stdio_result) {
                Some(Ok(())) | None => result(Ok(Value::Unit)),
                Some(Err(error)) => result(Err(format!("writer flush failed: {error}"))),
            }
        }
        Err(error) => result(Err(error)),
    }
}

#[cfg(feature = "net")]
/// Creates a reader over a cloned TCP stream. The socket remains owned by the
/// caller; the returned reader owns its own OS-level stream clone.
///
/// # Safety
/// `stream` must be null or point to a live `TcpStream` value for the duration
/// of this call.
pub unsafe extern "C" fn mux_io_reader_from_tcp(stream: *const Value) -> *mut Value {
    let socket = match crate::net::clone_tcp_stream(stream) {
        Ok(socket) => socket,
        Err(error) => return result(Err(error)),
    };
    let handle = next_handle();
    lock(&READERS).insert(
        handle,
        new_reader_slot(ReaderEntry {
            data: Vec::new(),
            position: 0,
            remaining_limit: None,
            tee_writer: None,
            stdio: None,
            file: None,
            socket: Some(socket),
        }),
    );
    let value = object(*READER_TYPE_ID, handle);
    if value.is_null() {
        lock(&READERS).remove(&handle);
        return result(Err("failed to allocate reader handle".to_string()));
    }
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    result(Ok(owned))
}

#[cfg(feature = "net")]
/// Creates a writer over a cloned TCP stream. The socket remains owned by the
/// caller; the returned writer owns its own OS-level stream clone.
///
/// # Safety
/// `stream` must be null or point to a live `TcpStream` value for the duration
/// of this call.
pub unsafe extern "C" fn mux_io_writer_from_tcp(stream: *const Value) -> *mut Value {
    let socket = match crate::net::clone_tcp_stream(stream) {
        Ok(socket) => socket,
        Err(error) => return result(Err(error)),
    };
    let handle = next_handle();
    lock(&WRITERS).insert(
        handle,
        new_writer_slot(WriterEntry {
            data: Vec::new(),
            position: 0,
            capture: true,
            append_mode: false,
            file: None,
            stdio: None,
            socket: Some(socket),
        }),
    );
    let value = object(*WRITER_TYPE_ID, handle);
    if value.is_null() {
        lock(&WRITERS).remove(&handle);
        return result(Err("failed to allocate writer handle".to_string()));
    }
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    result(Ok(owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refcount::mux_rc_alloc;

    #[test]
    fn internal_reader_bridge_caps_untrusted_request_sizes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mux-stream-reader-cap-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::write(&path, b"safe").expect("seed reader file");
        let path_value = mux_rc_alloc(Value::String(path.to_string_lossy().into_owned()));
        let result = unsafe { mux_io_reader_from_file(path_value) };
        let reader = match unsafe { &*result } {
            Value::Result(Ok(value)) => value.as_ref(),
            other => panic!("expected reader result, got {other:?}"),
        };
        let handle = reader_handle(reader).expect("reader handle");
        let bytes = read_reader(handle, usize::MAX).expect("bounded read");
        assert_eq!(bytes, b"safe");

        unsafe {
            assert!(mux_rc_dec(result));
            assert!(mux_rc_dec(path_value));
        }
        std::fs::remove_file(path).expect("remove reader file");
    }

    #[test]
    fn a_blocked_writer_does_not_block_an_independent_writer() {
        const PROGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

        let writer_a = mux_io_writer_new();
        let writer_b = mux_io_writer_new();
        assert!(!writer_a.is_null());
        assert!(!writer_b.is_null());
        let handle_a = writer_handle(unsafe { &*writer_a }).expect("writer A handle");
        let handle_b = writer_handle(unsafe { &*writer_b }).expect("writer B handle");
        let slot_a = writer_slot(handle_a).expect("writer A slot");
        let state_a = lock(&slot_a.state);

        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let stalled = std::thread::spawn(move || {
            started_sender.send(()).expect("signal stalled writer");
            write_writer(handle_a, b"blocked")
        });
        started_receiver
            .recv_timeout(PROGRESS_TIMEOUT)
            .expect("stalled writer did not start");

        let (progress_sender, progress_receiver) = std::sync::mpsc::channel();
        let progress = std::thread::spawn(move || {
            progress_sender
                .send(write_writer(handle_b, b"progress"))
                .expect("signal independent writer");
        });
        let independent_result = progress_receiver.recv_timeout(PROGRESS_TIMEOUT);

        drop(state_a);
        assert_eq!(stalled.join().expect("stalled writer panicked"), Ok(()));
        progress.join().expect("independent writer panicked");
        assert_eq!(
            independent_result.expect("independent writer blocked"),
            Ok(())
        );

        unsafe {
            assert!(mux_rc_dec(writer_a));
            assert!(mux_rc_dec(writer_b));
        }
    }

    #[test]
    fn tee_writer_lease_keeps_state_alive_after_registry_removal() {
        let writer_handle = next_handle();
        let writer_slot = new_writer_slot(WriterEntry {
            data: Vec::new(),
            position: 0,
            capture: true,
            append_mode: false,
            file: None,
            stdio: None,
            #[cfg(feature = "net")]
            socket: None,
        });
        lock(&WRITERS).insert(writer_handle, Arc::clone(&writer_slot));
        let lease = TeeWriterLease {
            handle: writer_handle,
            slot: Arc::clone(&writer_slot),
        };
        let state = lock(&writer_slot.state);
        let in_flight_lease = lease.clone();
        let write = std::thread::spawn(move || write_writer_slot(&in_flight_lease.slot, b"tee"));

        assert!(lock(&WRITERS).remove(&writer_handle).is_some());
        drop(state);
        assert_eq!(write.join().expect("tee writer panicked"), Ok(()));
        assert_eq!(lease.handle, writer_handle);
        assert_eq!(lock(&writer_slot.state).data, b"tee");
    }

    #[test]
    fn dropping_a_tee_reader_releases_its_retained_writer() {
        let input = mux_rc_alloc(Value::Bytes(b"tee".to_vec()));
        let reader = unsafe { mux_io_reader_from_bytes(input) };
        let writer = mux_io_writer_new();
        assert!(!reader.is_null());
        assert!(!writer.is_null());
        let reader_id = reader_handle(unsafe { &*reader }).expect("reader should be valid");

        let tee = unsafe { mux_io_reader_tee(reader, writer) };
        assert!(!tee.is_null());
        unsafe { mux_rc_dec(tee) };

        let writer_id = writer_handle(writer).expect("writer should be valid");
        unsafe {
            // Dropping the reader must release the extra reference held by
            // `tee`, while the caller's writer name is still alive.
            assert!(mux_rc_dec(reader));
            assert!(!READERS.lock().unwrap().contains_key(&reader_id));
            assert!(WRITERS.lock().unwrap().contains_key(&writer_id));

            assert!(mux_rc_dec(writer));
            assert!(!WRITERS.lock().unwrap().contains_key(&writer_id));
            assert!(mux_rc_dec(input));
        }
    }
}
