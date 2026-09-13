//! Synchronous process execution and process metadata.
//!
//! Commands and children are explicit shared-resource handles. Copying a
//! handle names the same command/child; dropping the final child handle
//! terminates and reaps the child so an abandoned process cannot outlive the
//! Mux program accidentally.

#![allow(clippy::missing_safety_doc)]

use crate::object::{alloc_object, get_object_ptr, register_object_type_with_copy};
use crate::refcount::mux_rc_alloc;
use crate::std::StdErrorKind;
use crate::{TypeId, Value};
use rust_std::collections::{HashMap, VecDeque};
use rust_std::ffi::{c_char, c_void, CStr};
use rust_std::io::Read;
#[cfg(unix)]
use rust_std::os::unix::process::CommandExt;
#[cfg(windows)]
use rust_std::os::windows::process::CommandExt;
use rust_std::process::{Child, Command as StdCommand, Output, Stdio};
use rust_std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Condvar, LazyLock, Mutex,
};
use rust_std::thread;
use rust_std::time::{Duration, Instant};

const MAX_PROCESS_IO_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
enum StdioMode {
    #[default]
    Inherit,
    Null,
    Piped,
}

#[derive(Clone, Debug, Default)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    stdin: StdioMode,
    stdout: StdioMode,
    stderr: StdioMode,
}

struct CommandEntry {
    spec: CommandSpec,
    names: usize,
}

struct ChildEntry {
    // Keep the registry lock independent from the child lock. Waiting for a
    // process can block, and holding the registry lock while doing so would
    // prevent another alias from draining a piped stdout/stderr stream.
    child: Arc<Mutex<Child>>,
    names: usize,
}

struct OutputEntry {
    output: Output,
    names: usize,
}

const DEFAULT_PROCESS_POOL_WORKERS: usize = 4;
const DEFAULT_PROCESS_POOL_QUEUE_CAPACITY: usize = 64;
const MAX_PROCESS_POOL_WORKERS: usize = 256;
const MAX_PROCESS_POOL_QUEUE_CAPACITY: usize = 65_536;
// Keep millisecond deadlines in the range supported consistently by the
// platform condition-variable and process wait implementations. In
// particular, constructing an `Instant` by unchecked addition can otherwise
// panic for an i64 millisecond value near the integer limit.
const MAX_PROCESS_TIMEOUT_MS: i64 = u32::MAX as i64;

struct ProcessPoolTask {
    spec: CommandSpec,
    result_channel: *mut Value,
}

// The task owns a cloned, Rust-native command specification and a reference to
// a runtime channel. Neither pointer is dereferenced by the worker without the
// corresponding registry operation, and the channel API owns its payload copy.
unsafe impl Send for ProcessPoolTask {}

struct ProcessPoolState {
    queue: Mutex<VecDeque<ProcessPoolTask>>,
    wake: Condvar,
    space: Condvar,
    queue_capacity: usize,
    closed: AtomicBool,
}

struct ProcessPoolEntry {
    state: Arc<ProcessPoolState>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    names: AtomicUsize,
}

static PROCESS_POOLS: LazyLock<Mutex<HashMap<i64, Arc<ProcessPoolEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static COMMANDS: LazyLock<Mutex<HashMap<i64, CommandEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHILDREN: LazyLock<Mutex<HashMap<i64, ChildEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OUTPUTS: LazyLock<Mutex<HashMap<i64, OutputEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));

static COMMAND_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Command",
        size_of::<i64>(),
        Some(drop_command as extern "C" fn(*mut c_void)),
        Some(copy_command as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static CHILD_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Child",
        size_of::<i64>(),
        Some(drop_child as extern "C" fn(*mut c_void)),
        Some(copy_child as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static OUTPUT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Output",
        size_of::<i64>(),
        Some(drop_output as extern "C" fn(*mut c_void)),
        Some(copy_output as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static PROCESS_POOL_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "ProcessPool",
        size_of::<i64>(),
        Some(drop_process_pool as extern "C" fn(*mut c_void)),
        Some(copy_process_pool as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> rust_std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(rust_std::sync::PoisonError::into_inner)
}

fn next_handle() -> i64 {
    let mut next = lock(&NEXT_HANDLE);
    let handle = *next;
    *next = next.checked_add(1).unwrap_or(1);
    handle
}

fn error(message: impl Into<String>) -> *mut Value {
    crate::std::process_result_err(message)
}

fn error_kind(kind: StdErrorKind, message: impl Into<String>) -> *mut Value {
    crate::std::process_result_err_kind(kind, message)
}

fn ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn unit_ok() -> *mut Value {
    ok(Value::Unit)
}

unsafe fn c_string(value: *const c_char, name: &str) -> Result<String, *mut Value> {
    if value.is_null() {
        return Err(error(format!("{name} must not be null")));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| error(format!("{name} must be valid UTF-8")))
}

fn object(type_id: TypeId, handle: i64) -> *mut Value {
    let value = alloc_object(type_id);
    if value.is_null() {
        return std::ptr::null_mut();
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { crate::refcount::mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = handle };
    value
}

unsafe fn handle(value: *const Value, type_id: TypeId) -> Option<i64> {
    if value.is_null() || unsafe { crate::object::get_object_type_id(value) } != type_id {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        None
    } else {
        let id = *ptr.cast::<i64>();
        (id > 0).then_some(id)
    }
}

unsafe fn command_handle(value: *const Value) -> Option<i64> {
    unsafe { handle(value, *COMMAND_TYPE_ID) }
}

unsafe fn child_handle(value: *const Value) -> Option<i64> {
    unsafe { handle(value, *CHILD_TYPE_ID) }
}

fn child_mutex(handle: i64) -> Result<Arc<Mutex<Child>>, String> {
    lock(&CHILDREN)
        .get(&handle)
        .map(|entry| Arc::clone(&entry.child))
        .ok_or_else(|| "invalid child handle".to_string())
}

unsafe fn output_handle(value: *const Value) -> Option<i64> {
    unsafe { handle(value, *OUTPUT_TYPE_ID) }
}

/// Output handles are immutable snapshots and may safely cross a channel or
/// process-pool result boundary. Commands and children remain non-sendable
/// because they own mutable process state.
pub(crate) fn is_sendable_output(value: &Value) -> bool {
    matches!(value, Value::Object(reference) if reference.type_id() == *OUTPUT_TYPE_ID)
}

fn process_pool_handle(value: *const Value) -> Option<i64> {
    unsafe { handle(value, *PROCESS_POOL_TYPE_ID) }
}

fn status_code(status: rust_std::process::ExitStatus) -> i64 {
    status.code().map_or(-1, i64::from)
}

fn terminate_process_group(child: &mut Child) -> Result<(), String> {
    let pid = child.id();
    #[cfg(unix)]
    {
        let pid = i32::try_from(pid)
            .map_err(|_| "child process id does not fit the platform process type".to_string())?;
        let rc = unsafe { libc::kill(-pid, libc::SIGKILL) };
        if rc != 0 {
            let error_value = rust_std::io::Error::last_os_error();
            if error_value.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!(
                    "terminating child process group failed: {error_value}"
                ));
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let status = StdCommand::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .map_err(|error_value| {
                format!("terminating child process tree failed: {error_value}")
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("taskkill failed with status {status}"))
        }
    }
}

fn make_command(spec: &CommandSpec) -> StdCommand {
    let mut command = StdCommand::new(&spec.program);
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command.stdin(match spec.stdin {
        StdioMode::Inherit => Stdio::inherit(),
        StdioMode::Null => Stdio::null(),
        StdioMode::Piped => Stdio::piped(),
    });
    command.stdout(match spec.stdout {
        StdioMode::Inherit => Stdio::inherit(),
        StdioMode::Null => Stdio::null(),
        StdioMode::Piped => Stdio::piped(),
    });
    command.stderr(match spec.stderr {
        StdioMode::Inherit => Stdio::inherit(),
        StdioMode::Null => Stdio::null(),
        StdioMode::Piped => Stdio::piped(),
    });
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(rust_std::io::Error::last_os_error())
            }
        });
    }
    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP keeps tree termination scoped to this
        // command. Windows has no setpgid equivalent; taskkill /T below walks
        // the child process tree for us.
        command.creation_flags(0x0000_0200);
    }
    command
}

fn insert_command(spec: CommandSpec) -> *mut Value {
    let handle = next_handle();
    lock(&COMMANDS).insert(handle, CommandEntry { spec, names: 1 });
    let value = object(*COMMAND_TYPE_ID, handle);
    if value.is_null() {
        lock(&COMMANDS).remove(&handle);
    }
    value
}

fn copy_handle<T>(
    map: &Mutex<HashMap<i64, T>>,
    source: *mut c_void,
    dest: *mut c_void,
    add: impl Fn(&mut T),
) where
    T: 'static,
{
    if source.is_null() || dest.is_null() {
        return;
    }
    let source_handle = unsafe { *source.cast::<i64>() };
    let mut entries = lock(map);
    if let Some(entry) = entries.get_mut(&source_handle) {
        add(entry);
        unsafe { *dest.cast::<i64>() = source_handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn copy_command(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&COMMANDS, source, dest, |entry: &mut CommandEntry| {
        entry.names += 1;
    });
}

extern "C" fn copy_child(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&CHILDREN, source, dest, |entry: &mut ChildEntry| {
        entry.names += 1;
    });
}

extern "C" fn copy_output(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&OUTPUTS, source, dest, |entry: &mut OutputEntry| {
        entry.names += 1;
    });
}

fn release_command(handle: i64) {
    let mut entries = lock(&COMMANDS);
    if let Some(entry) = entries.get_mut(&handle) {
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            entries.remove(&handle);
        }
    }
}

fn release_child(handle: i64) {
    let entry = {
        let mut entries = lock(&CHILDREN);
        let remove = entries.get_mut(&handle).is_some_and(|entry| {
            entry.names = entry.names.saturating_sub(1);
            entry.names == 0
        });
        remove.then(|| entries.remove(&handle)).flatten()
    };
    if let Some(entry) = entry {
        let mut child = lock(&entry.child);
        let _ = terminate_process_group(&mut child);
        let _ = child.wait();
    }
}

fn release_output(handle: i64) {
    let mut entries = lock(&OUTPUTS);
    if let Some(entry) = entries.get_mut(&handle) {
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            entries.remove(&handle);
        }
    }
}

extern "C" fn drop_command(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_command(unsafe { *ptr.cast::<i64>() });
    }
}

extern "C" fn drop_child(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_child(unsafe { *ptr.cast::<i64>() });
    }
}

extern "C" fn drop_output(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_output(unsafe { *ptr.cast::<i64>() });
    }
}

extern "C" fn copy_process_pool(source: *mut c_void, dest: *mut c_void) {
    copy_handle(
        &PROCESS_POOLS,
        source,
        dest,
        |entry: &mut Arc<ProcessPoolEntry>| {
            entry.names.fetch_add(1, Ordering::Relaxed);
        },
    );
}

fn close_process_pool(entry: &ProcessPoolEntry, cancel_pending: bool) {
    entry.state.closed.store(true, Ordering::Release);
    if cancel_pending {
        let mut queue = lock(&entry.state.queue);
        let tasks = queue.drain(..).collect::<Vec<_>>();
        entry.state.space.notify_all();
        drop(queue);
        for task in tasks {
            unsafe {
                let _ = crate::sync_primitives::mux_channel_close(task.result_channel);
                crate::refcount::mux_rc_dec(task.result_channel);
            }
        }
    }
    entry.state.wake.notify_all();
    let mut workers = lock(&entry.workers);
    let handles = workers.drain(..).collect::<Vec<_>>();
    drop(workers);
    for worker in handles {
        let _ = worker.join();
    }
}

fn release_process_pool(handle: i64) {
    let entry = {
        let mut pools = lock(&PROCESS_POOLS);
        let Some(entry) = pools.get(&handle).cloned() else {
            return;
        };
        if entry.names.fetch_sub(1, Ordering::Relaxed) != 1 {
            return;
        }
        pools.remove(&handle);
        entry
    };
    close_process_pool(&entry, true);
}

extern "C" fn drop_process_pool(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_process_pool(unsafe { *ptr.cast::<i64>() });
    }
}

#[unsafe(no_mangle)]
/// Create a default command builder. Set its program before execution.
pub unsafe extern "C" fn mux_process_command_new() -> *mut Value {
    let ptr = insert_command(CommandSpec::default());
    if ptr.is_null() {
        return error("could not allocate command");
    }
    let Value::Object(reference) = (unsafe { &*ptr }) else {
        unsafe { crate::refcount::mux_rc_dec(ptr) };
        return error("could not allocate command");
    };
    let value = Value::Object(reference.clone());
    unsafe { crate::refcount::mux_rc_dec(ptr) };
    ok(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_set_program(
    command: *mut Value,
    program: *const c_char,
) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let program = match c_string(program, "program") {
        Ok(program) if !program.is_empty() => program,
        Ok(_) => return error_kind(StdErrorKind::Invalid, "program must not be empty"),
        Err(result) => return result,
    };
    let mut entries = lock(&COMMANDS);
    let Some(entry) = entries.get_mut(&handle) else {
        return error("invalid command handle");
    };
    entry.spec.program = program;
    unit_ok()
}

#[unsafe(no_mangle)]
/// Create a command that is explicitly interpreted by the platform shell.
/// Direct commands configured with `set_program` remain shell-free.
pub unsafe extern "C" fn mux_process_command_shell(commandline: *const c_char) -> *mut Value {
    let commandline = match c_string(commandline, "command line") {
        Ok(commandline) if !commandline.is_empty() => commandline,
        Ok(_) => return error("command line must not be empty"),
        Err(result) => return result,
    };
    #[cfg(windows)]
    let spec = CommandSpec {
        program: "cmd.exe".to_string(),
        args: vec!["/C".to_string(), commandline],
        ..CommandSpec::default()
    };
    #[cfg(not(windows))]
    let spec = CommandSpec {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), commandline],
        ..CommandSpec::default()
    };
    let ptr = insert_command(spec);
    if ptr.is_null() {
        return error("could not allocate command");
    }
    let Value::Object(reference) = (unsafe { &*ptr }) else {
        unsafe { crate::refcount::mux_rc_dec(ptr) };
        return error("could not allocate command");
    };
    let value = Value::Object(reference.clone());
    unsafe { crate::refcount::mux_rc_dec(ptr) };
    ok(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_arg(
    command: *mut Value,
    argument: *const c_char,
) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let argument = match c_string(argument, "argument") {
        Ok(argument) => argument,
        Err(result) => return result,
    };
    let mut entries = lock(&COMMANDS);
    let Some(entry) = entries.get_mut(&handle) else {
        return error("invalid command handle");
    };
    entry.spec.args.push(argument);
    unit_ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_env(
    command: *mut Value,
    key: *const c_char,
    value: *const c_char,
) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let key = match c_string(key, "environment key") {
        Ok(key) if !key.is_empty() => key,
        Ok(_) => return error("environment key must not be empty"),
        Err(result) => return result,
    };
    let value = match c_string(value, "environment value") {
        Ok(value) => value,
        Err(result) => return result,
    };
    let mut entries = lock(&COMMANDS);
    let Some(entry) = entries.get_mut(&handle) else {
        return error("invalid command handle");
    };
    if let Some((_, existing)) = entry.spec.env.iter_mut().find(|(name, _)| name == &key) {
        *existing = value;
    } else {
        entry.spec.env.push((key, value));
    }
    unit_ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_cwd(
    command: *mut Value,
    path: *const c_char,
) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let path = match c_string(path, "working directory") {
        Ok(path) if !path.is_empty() => path,
        Ok(_) => return error("working directory must not be empty"),
        Err(result) => return result,
    };
    let mut entries = lock(&COMMANDS);
    let Some(entry) = entries.get_mut(&handle) else {
        return error("invalid command handle");
    };
    entry.spec.cwd = Some(path);
    unit_ok()
}

fn set_stdio_mode(
    command: *mut Value,
    mode: StdioMode,
    field: fn(&mut CommandSpec) -> &mut StdioMode,
) -> *mut Value {
    let Some(handle) = (unsafe { command_handle(command) }) else {
        return error("invalid command handle");
    };
    let mut entries = lock(&COMMANDS);
    let Some(entry) = entries.get_mut(&handle) else {
        return error("invalid command handle");
    };
    *field(&mut entry.spec) = mode;
    unit_ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_stdin_piped(command: *mut Value) -> *mut Value {
    set_stdio_mode(command, StdioMode::Piped, |spec| &mut spec.stdin)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_stdout_piped(command: *mut Value) -> *mut Value {
    set_stdio_mode(command, StdioMode::Piped, |spec| &mut spec.stdout)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_stderr_piped(command: *mut Value) -> *mut Value {
    set_stdio_mode(command, StdioMode::Piped, |spec| &mut spec.stderr)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_stdin_null(command: *mut Value) -> *mut Value {
    set_stdio_mode(command, StdioMode::Null, |spec| &mut spec.stdin)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_stdout_null(command: *mut Value) -> *mut Value {
    set_stdio_mode(command, StdioMode::Null, |spec| &mut spec.stdout)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_stderr_null(command: *mut Value) -> *mut Value {
    set_stdio_mode(command, StdioMode::Null, |spec| &mut spec.stderr)
}

fn command_spec(handle: i64) -> Result<CommandSpec, String> {
    lock(&COMMANDS)
        .get(&handle)
        .map(|entry| entry.spec.clone())
        .ok_or_else(|| "invalid command handle".to_string())
}

fn insert_child(child: Child) -> *mut Value {
    let handle = next_handle();
    lock(&CHILDREN).insert(
        handle,
        ChildEntry {
            child: Arc::new(Mutex::new(child)),
            names: 1,
        },
    );
    let value = object(*CHILD_TYPE_ID, handle);
    if value.is_null() {
        if let Some(entry) = lock(&CHILDREN).remove(&handle) {
            let mut child = lock(&entry.child);
            let _ = terminate_process_group(&mut child);
            let _ = child.wait();
        }
    }
    value
}

fn insert_output(output: Output) -> *mut Value {
    let handle = next_handle();
    lock(&OUTPUTS).insert(handle, OutputEntry { output, names: 1 });
    let value = object(*OUTPUT_TYPE_ID, handle);
    if value.is_null() {
        lock(&OUTPUTS).remove(&handle);
    }
    value
}

/// Read one captured process stream without allowing an unbounded allocation.
/// The reader returns as soon as the limit is crossed; the parent process is
/// responsible for terminating the child so a producer cannot remain blocked
/// on a full pipe after the limit has been reported.
fn read_captured_pipe<R: Read>(
    mut reader: R,
    overflow: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error_value) => {
                abort.store(true, Ordering::Release);
                return Err(format!("reading process output failed: {error_value}"));
            }
        };
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_PROCESS_IO_BYTES {
            overflow.store(true, Ordering::Release);
            // Stop the producer as soon as one captured pipe crosses the
            // bound.  Leaving `abort` unset makes the parent wait for a
            // process that may be blocked forever writing to the pipe whose
            // reader has already exited.
            abort.store(true, Ordering::Release);
            return Err(format!(
                "process output exceeded {MAX_PROCESS_IO_BYTES} bytes"
            ));
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

fn run_command_output(spec: &CommandSpec) -> Result<Output, String> {
    let mut command = make_command(spec);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error_value| format!("process output failed: {error_value}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = terminate_process_group(&mut child);
        let _ = child.wait();
        return Err("process output stdout pipe was not created".to_string());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = terminate_process_group(&mut child);
        let _ = child.wait();
        return Err("process output stderr pipe was not created".to_string());
    };
    let overflow = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let stdout_thread = {
        let overflow = Arc::clone(&overflow);
        let abort = Arc::clone(&abort);
        thread::spawn(move || read_captured_pipe(stdout, overflow, abort))
    };
    let stderr_thread = {
        let overflow = Arc::clone(&overflow);
        let abort = Arc::clone(&abort);
        thread::spawn(move || read_captured_pipe(stderr, overflow, abort))
    };

    let status = loop {
        if abort.load(Ordering::Acquire) {
            let _ = terminate_process_group(&mut child);
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error_value) => {
                let _ = terminate_process_group(&mut child);
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("waiting for process output failed: {error_value}"));
            }
        }
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| "process stdout reader panicked".to_string())??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| "process stderr reader panicked".to_string())??;
    if overflow.load(Ordering::Acquire) {
        return Err(format!(
            "process output exceeded {MAX_PROCESS_IO_BYTES} bytes"
        ));
    }
    let Some(status) = status else {
        return Err("process output did not produce a complete result".to_string());
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_output(command: *mut Value) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let spec = match command_spec(handle) {
        Ok(spec) => spec,
        Err(message) => return error(message),
    };
    let output = match run_command_output(&spec) {
        Ok(output) => output,
        Err(message) => return error(message),
    };
    match insert_output(Output {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    }) {
        ptr if !ptr.is_null() => unsafe {
            let value = match &*ptr {
                Value::Object(reference) => Value::Object(reference.clone()),
                _ => return error("could not allocate process output"),
            };
            crate::refcount::mux_rc_dec(ptr);
            ok(value)
        },
        _ => error("could not allocate process output"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_status(command: *mut Value) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let spec = match command_spec(handle) {
        Ok(spec) => spec,
        Err(message) => return error(message),
    };
    match make_command(&spec).status() {
        Ok(status) => ok(Value::Int(status_code(status))),
        Err(error_value) => error_kind(
            StdErrorKind::Spawn,
            format!("process status failed: {error_value}"),
        ),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_command_spawn(command: *mut Value) -> *mut Value {
    let Some(handle) = command_handle(command) else {
        return error("invalid command handle");
    };
    let spec = match command_spec(handle) {
        Ok(spec) => spec,
        Err(message) => return error(message),
    };
    match make_command(&spec).spawn() {
        Ok(child) => match insert_child(child) {
            ptr if !ptr.is_null() => unsafe {
                let value = match &*ptr {
                    Value::Object(reference) => Value::Object(reference.clone()),
                    _ => return error("could not allocate child"),
                };
                crate::refcount::mux_rc_dec(ptr);
                ok(value)
            },
            _ => error("could not allocate child"),
        },
        Err(error_value) => error_kind(
            StdErrorKind::Spawn,
            format!("process spawn failed: {error_value}"),
        ),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_wait(child: *mut Value) -> *mut Value {
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    // Match `std::process::Child::wait`: close a still-open piped stdin so a
    // child waiting for EOF can make progress, but poll without holding the
    // registry or child lock so callers can drain stdout/stderr concurrently.
    lock(&child).stdin.take();
    let status = loop {
        let status = match lock(&child).try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error_value) => return error(format!("waiting for child failed: {error_value}")),
        };
        break status;
    };
    ok(Value::Int(status_code(status)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_try_wait(child: *mut Value) -> *mut Value {
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    let status = {
        let mut child = lock(&child);
        child.try_wait()
    };
    match status {
        Ok(Some(status)) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Optional(Some(
            Box::new(Value::Int(status_code(status))),
        )))))),
        Ok(None) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Optional(None))))),
        Err(error_value) => error(format!("checking child status failed: {error_value}")),
    }
}

#[unsafe(no_mangle)]
/// Wait up to `timeout_ms` milliseconds. `none` means the child is still running.
pub unsafe extern "C" fn mux_process_child_wait_timeout(
    child: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    if timeout_ms < 0 {
        return error("timeout must not be negative");
    }
    if timeout_ms > MAX_PROCESS_TIMEOUT_MS {
        return error("timeout is too large");
    }
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let timeout = Duration::from_millis(timeout_ms as u64);
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return error("timeout is too large");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    loop {
        let status = {
            let mut child = lock(&child);
            child.try_wait()
        };
        match status {
            Ok(Some(status)) => {
                return mux_rc_alloc(Value::Result(Ok(Box::new(Value::Optional(Some(
                    Box::new(Value::Int(status_code(status))),
                ))))));
            }
            Ok(None) if Instant::now() >= deadline => {
                return mux_rc_alloc(Value::Result(Ok(Box::new(Value::Optional(None)))));
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(2)));
            }
            Err(error_value) => {
                return error(format!("checking child status failed: {error_value}"));
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_kill(child: *mut Value) -> *mut Value {
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    let status = {
        let mut child = lock(&child);
        child.kill()
    };
    match status {
        Ok(()) => unit_ok(),
        Err(error_value) => error(format!("killing child failed: {error_value}")),
    }
}

/// Terminate the child and every descendant in its private process group.
/// Unix children are started in a fresh process group; Windows uses the
/// platform's process-tree termination command.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_kill_group(child: *mut Value) -> *mut Value {
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    let result = terminate_process_group(&mut lock(&child));
    match result {
        Ok(()) => unit_ok(),
        Err(message) => error(message),
    }
}

fn process_io_size(size: i64) -> Result<usize, String> {
    let size = usize::try_from(size).map_err(|_| "invalid buffer size".to_string())?;
    if size == 0 || size > MAX_PROCESS_IO_BYTES {
        return Err(format!(
            "buffer size must be between 1 and {MAX_PROCESS_IO_BYTES} bytes"
        ));
    }
    Ok(size)
}

fn process_input_bytes(value: *mut Value) -> Result<Vec<u8>, String> {
    if value.is_null() {
        return Err("input is null".to_string());
    }
    match unsafe { &*value } {
        Value::Bytes(bytes) => Ok(bytes.clone()),
        _ => Err("input must be bytes".to_string()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_write_stdin(
    child: *mut Value,
    input: *mut Value,
) -> *mut Value {
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let input = match process_input_bytes(input) {
        Ok(input) => input,
        Err(message) => return error(message),
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    let mut child = lock(&child);
    let Some(stdin) = child.stdin.as_mut() else {
        return error("child stdin is not piped");
    };
    match std::io::Write::write(stdin, &input) {
        Ok(written) => match i64::try_from(written) {
            Ok(written) => ok(Value::Int(written)),
            Err(_) => error("written byte count exceeds the Mux integer range"),
        },
        Err(error_value) => error(format!("writing child stdin failed: {error_value}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_close_stdin(child: *mut Value) -> *mut Value {
    let Some(handle) = child_handle(child) else {
        return error("invalid child handle");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    if lock(&child).stdin.take().is_none() {
        return error("child stdin is not piped or is already closed");
    }
    unit_ok()
}

fn read_child_pipe(child: *mut Value, size: i64, stderr: bool) -> *mut Value {
    let size = match process_io_size(size) {
        Ok(size) => size,
        Err(message) => return error(message),
    };
    let Some(handle) = (unsafe { child_handle(child) }) else {
        return error("invalid child handle");
    };
    let child = match child_mutex(handle) {
        Ok(child) => child,
        Err(message) => return error(message),
    };
    let mut child = lock(&child);
    let mut bytes = vec![0u8; size];
    let read_result = if stderr {
        match child.stderr.as_mut() {
            Some(pipe) => pipe.read(&mut bytes),
            None => return error("child stderr is not piped"),
        }
    } else {
        match child.stdout.as_mut() {
            Some(pipe) => pipe.read(&mut bytes),
            None => return error("child stdout is not piped"),
        }
    };
    match read_result {
        Ok(count) => {
            bytes.truncate(count);
            ok(Value::Bytes(bytes))
        }
        Err(error_value) => error(format!("reading child pipe failed: {error_value}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_read_stdout(child: *mut Value, size: i64) -> *mut Value {
    read_child_pipe(child, size, false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_child_read_stderr(child: *mut Value, size: i64) -> *mut Value {
    read_child_pipe(child, size, true)
}

fn output_bytes(handle: i64, stderr: bool) -> Result<Vec<u8>, String> {
    lock(&OUTPUTS)
        .get(&handle)
        .map(|entry| {
            if stderr {
                entry.output.stderr.clone()
            } else {
                entry.output.stdout.clone()
            }
        })
        .ok_or_else(|| "invalid process output handle".to_string())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_output_status(output: *mut Value) -> *mut Value {
    let Some(handle) = output_handle(output) else {
        return error("invalid process output handle");
    };
    match lock(&OUTPUTS).get(&handle) {
        Some(entry) => ok(Value::Int(status_code(entry.output.status))),
        None => error("invalid process output handle"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_output_stdout(output: *mut Value) -> *mut Value {
    let Some(handle) = output_handle(output) else {
        return error("invalid process output handle");
    };
    match output_bytes(handle, false) {
        Ok(bytes) => ok(Value::Bytes(bytes)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_output_stderr(output: *mut Value) -> *mut Value {
    let Some(handle) = output_handle(output) else {
        return error("invalid process output handle");
    };
    match output_bytes(handle, true) {
        Ok(bytes) => ok(Value::Bytes(bytes)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_process_args() -> *mut Value {
    mux_rc_alloc(Value::List(
        rust_std::env::args().map(Value::String).collect(),
    ))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_process_id() -> i64 {
    i64::from(rust_std::process::id())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_process_parent_id() -> *mut Value {
    #[cfg(unix)]
    {
        mux_rc_alloc(Value::Optional(Some(Box::new(Value::Int(i64::from(
            unsafe { libc::getppid() as u32 },
        ))))))
    }
    #[cfg(not(unix))]
    {
        mux_rc_alloc(Value::Optional(None))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_process_executable() -> *mut Value {
    match rust_std::env::current_exe() {
        Ok(path) => match path.to_str() {
            Some(path) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::String(path.to_owned()))))),
            None => error("current executable path is not valid UTF-8"),
        },
        Err(error_value) => error(format!("reading current executable failed: {error_value}")),
    }
}

// ProcessPool --------------------------------------------------------------

fn process_pool_object(entry: ProcessPoolEntry) -> Result<*mut Value, String> {
    let id = next_handle();
    let entry = Arc::new(entry);
    lock(&PROCESS_POOLS).insert(id, entry);
    let value = object(*PROCESS_POOL_TYPE_ID, id);
    if value.is_null() {
        if let Some(entry) = lock(&PROCESS_POOLS).remove(&id) {
            close_process_pool(&entry, true);
        }
        return Err("could not allocate ProcessPool handle".to_string());
    }
    Ok(value)
}

fn process_pool_result(value: *mut Value) -> *mut Value {
    if value.is_null() {
        return error("could not allocate ProcessPool handle");
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { crate::refcount::mux_rc_dec(value) };
        return error("could not allocate ProcessPool handle");
    };
    let converted = Value::Object(reference.clone());
    unsafe { crate::refcount::mux_rc_dec(value) };
    ok(converted)
}

// A small helper avoids duplicating the object-to-Value conversion while
// preserving the single owned reference returned by insert_output.
fn output_value(output: Output) -> Result<Value, String> {
    let raw_value = insert_output(output);
    if raw_value.is_null() {
        return Err("could not allocate process output".to_string());
    }
    let Value::Object(reference) = (unsafe { &*raw_value }) else {
        unsafe { crate::refcount::mux_rc_dec(raw_value) };
        return Err("could not allocate process output".to_string());
    };
    let value = Value::Object(reference.clone());
    unsafe { crate::refcount::mux_rc_dec(raw_value) };
    Ok(value)
}

fn send_process_pool_result(channel: *mut Value, result: Value) {
    let result = mux_rc_alloc(result);
    let sent = unsafe { crate::sync_primitives::mux_channel_send(channel, result) };
    unsafe {
        crate::refcount::mux_rc_dec(sent);
        crate::refcount::mux_rc_dec(result);
    }
}

fn process_pool_worker(state: Arc<ProcessPoolState>) {
    loop {
        let task = {
            let mut queue = lock(&state.queue);
            loop {
                if let Some(task) = queue.pop_front() {
                    state.space.notify_one();
                    break Some(task);
                }
                if state.closed.load(Ordering::Acquire) {
                    break None;
                }
                queue = state
                    .wake
                    .wait(queue)
                    .unwrap_or_else(rust_std::sync::PoisonError::into_inner);
            }
        };
        let Some(task) = task else {
            return;
        };
        let result = match run_command_output(&task.spec).and_then(output_value) {
            Ok(output) => Value::Result(Ok(Box::new(output))),
            Err(message) => Value::Result(Err(Box::new(Value::String(message)))),
        };
        send_process_pool_result(task.result_channel, result);
        unsafe {
            let closed = crate::sync_primitives::mux_channel_close(task.result_channel);
            crate::refcount::mux_rc_dec(closed);
            crate::refcount::mux_rc_dec(task.result_channel);
        }
    }
}

fn create_process_pool(workers: usize, queue_capacity: usize) -> Result<*mut Value, String> {
    if !(1..=MAX_PROCESS_POOL_WORKERS).contains(&workers) {
        return Err(format!(
            "ProcessPool worker count must be between 1 and {MAX_PROCESS_POOL_WORKERS}"
        ));
    }
    if !(1..=MAX_PROCESS_POOL_QUEUE_CAPACITY).contains(&queue_capacity) {
        return Err(format!(
            "ProcessPool queue capacity must be between 1 and {MAX_PROCESS_POOL_QUEUE_CAPACITY}"
        ));
    }
    let state = Arc::new(ProcessPoolState {
        queue: Mutex::new(VecDeque::with_capacity(queue_capacity)),
        wake: Condvar::new(),
        space: Condvar::new(),
        queue_capacity,
        closed: AtomicBool::new(false),
    });
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let worker_state = Arc::clone(&state);
        match thread::Builder::new().spawn(move || process_pool_worker(worker_state)) {
            Ok(handle) => handles.push(handle),
            Err(error_value) => {
                state.closed.store(true, Ordering::Release);
                state.wake.notify_all();
                for handle in handles {
                    let _ = handle.join();
                }
                return Err(format!("failed to start ProcessPool worker: {error_value}"));
            }
        }
    }
    process_pool_object(ProcessPoolEntry {
        state,
        workers: Mutex::new(handles),
        names: AtomicUsize::new(1),
    })
}

fn process_pool_entry(handle: *const Value) -> Result<Arc<ProcessPoolEntry>, String> {
    let id = process_pool_handle(handle).ok_or_else(|| "invalid ProcessPool handle".to_string())?;
    lock(&PROCESS_POOLS)
        .get(&id)
        .cloned()
        .ok_or_else(|| "ProcessPool handle is closed".to_string())
}

fn process_pool_enqueue(
    entry: &ProcessPoolEntry,
    task: ProcessPoolTask,
    timeout_ms: Option<i64>,
) -> Result<bool, String> {
    if timeout_ms.is_some_and(|timeout| timeout < 0) {
        return Err("ProcessPool timeout must not be negative".to_string());
    }
    if timeout_ms.is_some_and(|timeout| timeout > MAX_PROCESS_TIMEOUT_MS) {
        return Err("ProcessPool timeout is too large".to_string());
    }
    let deadline = timeout_ms
        .map(|timeout| {
            Instant::now()
                .checked_add(Duration::from_millis(timeout as u64))
                .ok_or_else(|| "ProcessPool timeout is too large".to_string())
        })
        .transpose()?;
    let mut queue = lock(&entry.state.queue);
    loop {
        if entry.state.closed.load(Ordering::Acquire) {
            return Err("ProcessPool is closed".to_string());
        }
        if queue.len() < entry.state.queue_capacity {
            queue.push_back(task);
            entry.state.wake.notify_one();
            return Ok(true);
        }
        if timeout_ms == Some(0) {
            return Ok(false);
        }
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let (next_queue, wait_result) = entry
                .state
                .space
                .wait_timeout(queue, remaining)
                .unwrap_or_else(rust_std::sync::PoisonError::into_inner);
            queue = next_queue;
            if wait_result.timed_out() && queue.len() >= entry.state.queue_capacity {
                return Ok(false);
            }
        } else {
            queue = entry
                .state
                .space
                .wait(queue)
                .unwrap_or_else(rust_std::sync::PoisonError::into_inner);
        }
    }
}

unsafe fn enqueue_process_pool_task(
    pool: *const Value,
    command: *const Value,
    timeout_ms: Option<i64>,
) -> Result<Option<*mut Value>, String> {
    let entry = process_pool_entry(pool)?;
    let command_id =
        unsafe { command_handle(command) }.ok_or_else(|| "invalid Command handle".to_string())?;
    let spec = command_spec(command_id)?;
    let result_channel = crate::sync_primitives::mux_channel_new_unbounded();
    if result_channel.is_null() {
        return Err("could not allocate ProcessPool result channel".to_string());
    }
    unsafe { crate::refcount::mux_rc_inc(result_channel) };
    let task = ProcessPoolTask {
        spec,
        result_channel,
    };
    match process_pool_enqueue(&entry, task, timeout_ms) {
        Ok(true) => Ok(Some(result_channel)),
        Ok(false) => {
            unsafe {
                let closed = crate::sync_primitives::mux_channel_close(result_channel);
                crate::refcount::mux_rc_dec(closed);
                crate::refcount::mux_rc_dec(result_channel);
                crate::refcount::mux_rc_dec(result_channel);
            }
            Ok(None)
        }
        Err(error_value) => {
            unsafe {
                let closed = crate::sync_primitives::mux_channel_close(result_channel);
                crate::refcount::mux_rc_dec(closed);
                crate::refcount::mux_rc_dec(result_channel);
                crate::refcount::mux_rc_dec(result_channel);
            }
            Err(error_value)
        }
    }
}

fn channel_result(channel: *mut Value) -> *mut Value {
    if channel.is_null() {
        return error("ProcessPool result channel is null");
    }
    let Value::Object(reference) = (unsafe { &*channel }) else {
        unsafe { crate::refcount::mux_rc_dec(channel) };
        return error("ProcessPool result channel has an invalid type");
    };
    let value = Value::Object(reference.clone());
    unsafe { crate::refcount::mux_rc_dec(channel) };
    ok(value)
}

fn optional_channel_result(channel: Option<*mut Value>) -> *mut Value {
    let value = match channel {
        Some(channel) => {
            if channel.is_null() {
                return error("ProcessPool result channel is null");
            }
            let Value::Object(reference) = (unsafe { &*channel }) else {
                unsafe { crate::refcount::mux_rc_dec(channel) };
                return error("ProcessPool result channel has an invalid type");
            };
            let value = Value::Object(reference.clone());
            unsafe { crate::refcount::mux_rc_dec(channel) };
            Value::Optional(Some(Box::new(value)))
        }
        None => Value::Optional(None),
    };
    ok(value)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_process_pool_new() -> *mut Value {
    match create_process_pool(
        DEFAULT_PROCESS_POOL_WORKERS,
        DEFAULT_PROCESS_POOL_QUEUE_CAPACITY,
    ) {
        Ok(value) => process_pool_result(value),
        Err(error_value) => error(error_value),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_process_pool_with_config(workers: i64, queue_capacity: i64) -> *mut Value {
    let Ok(workers) = usize::try_from(workers) else {
        return error("ProcessPool worker count is invalid");
    };
    let Ok(queue_capacity) = usize::try_from(queue_capacity) else {
        return error("ProcessPool queue capacity is invalid");
    };
    match create_process_pool(workers, queue_capacity) {
        Ok(value) => process_pool_result(value),
        Err(error_value) => error(error_value),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_pool_submit(
    pool: *const Value,
    command: *const Value,
) -> *mut Value {
    match unsafe { enqueue_process_pool_task(pool, command, None) } {
        Ok(Some(channel)) => channel_result(channel),
        Ok(None) => error("ProcessPool submission was unexpectedly not accepted"),
        Err(error_value) => error(error_value),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_pool_try_submit(
    pool: *const Value,
    command: *const Value,
) -> *mut Value {
    match unsafe { enqueue_process_pool_task(pool, command, Some(0)) } {
        Ok(channel) => optional_channel_result(channel),
        Err(error_value) => error(error_value),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_pool_submit_timeout(
    pool: *const Value,
    command: *const Value,
    timeout_ms: i64,
) -> *mut Value {
    match unsafe { enqueue_process_pool_task(pool, command, Some(timeout_ms)) } {
        Ok(channel) => optional_channel_result(channel),
        Err(error_value) => error(error_value),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_pool_cancel_pending(pool: *const Value) -> *mut Value {
    let entry = match process_pool_entry(pool) {
        Ok(entry) => entry,
        Err(error_value) => return error(error_value),
    };
    let mut queue = lock(&entry.state.queue);
    let tasks = queue.drain(..).collect::<Vec<_>>();
    entry.state.space.notify_all();
    drop(queue);
    for task in &tasks {
        unsafe {
            let closed = crate::sync_primitives::mux_channel_close(task.result_channel);
            crate::refcount::mux_rc_dec(closed);
            crate::refcount::mux_rc_dec(task.result_channel);
        }
    }
    ok(Value::Int(tasks.len() as i64))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_process_pool_close(pool: *const Value) -> *mut Value {
    let entry = match process_pool_entry(pool) {
        Ok(entry) => entry,
        Err(error_value) => return error(error_value),
    };
    close_process_pool(&entry, false);
    ok(Value::Unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_pipe_overflow_requests_producer_abort() {
        let overflow = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let input = vec![0_u8; MAX_PROCESS_IO_BYTES + 1];

        let result = read_captured_pipe(
            rust_std::io::Cursor::new(input),
            overflow.clone(),
            abort.clone(),
        );

        assert!(result.is_err());
        assert!(overflow.load(Ordering::Acquire));
        assert!(abort.load(Ordering::Acquire));
    }

    #[test]
    fn process_pool_rejects_unrepresentable_timeout() {
        let state = Arc::new(ProcessPoolState {
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            space: Condvar::new(),
            queue_capacity: 1,
            closed: AtomicBool::new(false),
        });
        let entry = ProcessPoolEntry {
            state,
            workers: Mutex::new(Vec::new()),
            names: AtomicUsize::new(1),
        };
        let task = ProcessPoolTask {
            spec: CommandSpec::default(),
            result_channel: std::ptr::null_mut(),
        };

        let error = process_pool_enqueue(&entry, task, Some(i64::MAX))
            .expect_err("an oversized timeout must be rejected");
        assert_eq!(error, "ProcessPool timeout is too large");
    }
}
