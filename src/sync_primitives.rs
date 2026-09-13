//! Synchronous atomics and coordination primitives for `std.sync`.
//!
//! Every value exposed here is an explicit shared handle. Atomics use
//! sequentially consistent operations; semaphores and barriers block the
//! calling thread and never create background tasks.

#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec, mux_rc_inc};
use crate::{Tuple, TypeId, Value};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Barrier, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_CHANNEL_CAPACITY: i64 = 16 * 1024 * 1024;
const MAX_SELECT_CHANNELS: usize = 1024;
const DEFAULT_POOL_WORKERS: i64 = 4;
const DEFAULT_POOL_QUEUE_CAPACITY: usize = 64;
const MAX_POOL_WORKERS: i64 = 256;
const MAX_POOL_QUEUE_CAPACITY: i64 = 65_536;
// Keep millisecond waits within the range supported consistently by the
// platform condition-variable and channel backends. Windows exposes a
// 32-bit millisecond timeout; using the same bound everywhere avoids a Unix
// caller accidentally requesting a deadline that can never be reached.
const MAX_SYNC_TIMEOUT_MS: i64 = u32::MAX as i64;

fn timeout_duration(timeout_ms: i64, operation: &str) -> Result<Duration, String> {
    if timeout_ms > MAX_SYNC_TIMEOUT_MS {
        return Err(format!("{operation} timeout is too large"));
    }
    Ok(Duration::from_millis(timeout_ms as u64))
}

fn timeout_deadline(timeout_ms: i64, operation: &str) -> Result<Instant, String> {
    let duration = timeout_duration(timeout_ms, operation)?;
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| format!("{operation} timeout is too large"))
}

/// Compiler closure ABI used by synchronous callback primitives. This is kept
/// local so `sync_primitives` remains usable in the `core` runtime feature,
/// where the broader `sync` module is intentionally not linked.
#[repr(C)]
struct ClosureRepr {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
    capture_count: i64,
    boxed_function_ptr: *mut c_void,
}

unsafe fn invoke_void_callback(callback: *mut c_void) -> Result<(), String> {
    if callback.is_null() {
        return Err("callback is null".to_string());
    }
    let repr = unsafe { &*(callback as *const ClosureRepr) };
    if repr.function_ptr.is_null() {
        return Err("callback function is null".to_string());
    }
    if repr.captures_ptr.is_null() {
        let function: extern "C" fn() = unsafe { std::mem::transmute(repr.function_ptr) };
        function();
    } else {
        let function: extern "C" fn(*mut c_void) =
            unsafe { std::mem::transmute(repr.function_ptr) };
        function(repr.captures_ptr);
    }
    Ok(())
}

unsafe fn invoke_value_callback(callback: *mut c_void) -> Result<*mut Value, String> {
    if callback.is_null() {
        return Err("callback is null".to_string());
    }
    let repr = unsafe { &*(callback as *const ClosureRepr) };
    if repr.function_ptr.is_null() {
        return Err("callback function is null".to_string());
    }
    if repr.boxed_function_ptr.is_null() {
        if repr.captures_ptr.is_null() {
            let function: extern "C" fn() = unsafe { std::mem::transmute(repr.function_ptr) };
            function();
        } else {
            let function: extern "C" fn(*mut c_void) =
                unsafe { std::mem::transmute(repr.function_ptr) };
            function(repr.captures_ptr);
        }
        return Ok(mux_rc_alloc(Value::Unit));
    }
    let value = if repr.captures_ptr.is_null() {
        let function: extern "C" fn() -> *mut Value =
            unsafe { std::mem::transmute(repr.boxed_function_ptr) };
        function()
    } else {
        let function: extern "C" fn(*mut c_void) -> *mut Value =
            unsafe { std::mem::transmute(repr.boxed_function_ptr) };
        function(repr.captures_ptr)
    };
    if value.is_null() {
        Err("callback returned a null value".to_string())
    } else {
        Ok(value)
    }
}

unsafe fn invoke_map_callback(
    callback: *mut c_void,
    argument: Value,
) -> Result<*mut Value, String> {
    let repr = unsafe { &*(callback as *const ClosureRepr) };
    if repr.boxed_function_ptr.is_null() {
        return Err("map callback has no boxed entry point".to_string());
    }
    let argument = mux_rc_alloc(argument);
    let result = if repr.captures_ptr.is_null() {
        let function: extern "C" fn(*mut Value) -> *mut Value =
            unsafe { std::mem::transmute(repr.boxed_function_ptr) };
        function(argument)
    } else {
        let function: extern "C" fn(*mut c_void, *mut Value) -> *mut Value =
            unsafe { std::mem::transmute(repr.boxed_function_ptr) };
        function(repr.captures_ptr, argument)
    };
    unsafe { mux_rc_dec(argument) };
    if result.is_null() {
        Err("map callback returned a null value".to_string())
    } else {
        Ok(result)
    }
}

struct AtomicIntEntry {
    value: AtomicI64,
    names: AtomicUsize,
}
struct AtomicBoolEntry {
    value: AtomicBool,
    names: AtomicUsize,
}
pub(crate) struct CancellationEntry {
    cancelled: AtomicBool,
    names: AtomicUsize,
}
struct SemaphoreState {
    permits: usize,
}
struct SemaphoreEntry {
    state: Mutex<SemaphoreState>,
    wake: Condvar,
    max: usize,
    names: AtomicUsize,
}
struct BarrierEntry {
    barrier: Barrier,
    names: AtomicUsize,
}

// Mux values are reference-counted at the language boundary and therefore
// cannot be marked Send wholesale. Channels validate payloads before wrapping
// them; the wrapper is only constructed for values whose representation is
// recursively thread-safe (primitive/collection data, never an Object handle).
struct SendValue(Value);
unsafe impl Send for SendValue {}

#[derive(Clone)]
enum ChannelSender {
    Unbounded(Sender<SendValue>),
    Bounded(SyncSender<SendValue>),
}

struct ChannelEntry {
    sender: Mutex<Option<ChannelSender>>,
    receiver: Mutex<Receiver<SendValue>>,
    capacity: Option<usize>,
    closed: AtomicBool,
    names: AtomicUsize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OnceStatus {
    Pending,
    Running,
    Complete,
}

struct OnceEntry {
    state: Mutex<OnceStatus>,
    wake: Condvar,
    names: AtomicUsize,
}

struct PoolTask {
    closure: usize,
    result_channel: *mut Value,
    argument: Option<SendValue>,
}

unsafe impl Send for PoolTask {}

struct PoolState {
    queue: Mutex<VecDeque<PoolTask>>,
    wake: Condvar,
    space: Condvar,
    queue_capacity: usize,
    closed: AtomicBool,
}

struct PoolEntry {
    state: Arc<PoolState>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    names: AtomicUsize,
}

static ATOMIC_INTS: LazyLock<Mutex<HashMap<i64, Arc<AtomicIntEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ATOMIC_BOOLS: LazyLock<Mutex<HashMap<i64, Arc<AtomicBoolEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CANCELLATIONS: LazyLock<Mutex<HashMap<i64, Arc<CancellationEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SEMAPHORES: LazyLock<Mutex<HashMap<i64, Arc<SemaphoreEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static BARRIERS: LazyLock<Mutex<HashMap<i64, Arc<BarrierEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHANNELS: LazyLock<Mutex<HashMap<i64, Arc<ChannelEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ONCES: LazyLock<Mutex<HashMap<i64, Arc<OnceEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static POOLS: LazyLock<Mutex<HashMap<i64, Arc<PoolEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

static ATOMIC_INT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "AtomicInt",
        size_of::<i64>(),
        Some(drop_atomic_int),
        Some(copy_atomic_int),
    )
});
static ATOMIC_BOOL_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "AtomicBool",
        size_of::<i64>(),
        Some(drop_atomic_bool),
        Some(copy_atomic_bool),
    )
});
static CANCELLATION_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "CancellationToken",
        size_of::<i64>(),
        Some(drop_cancellation),
        Some(copy_cancellation),
    )
});
static SEMAPHORE_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Semaphore",
        size_of::<i64>(),
        Some(drop_semaphore),
        Some(copy_semaphore),
    )
});
static BARRIER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Barrier",
        size_of::<i64>(),
        Some(drop_barrier),
        Some(copy_barrier),
    )
});
static CHANNEL_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Channel",
        size_of::<i64>(),
        Some(drop_channel),
        Some(copy_channel),
    )
});
static ONCE_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy("Once", size_of::<i64>(), Some(drop_once), Some(copy_once))
});
static POOL_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "WorkerPool",
        size_of::<i64>(),
        Some(drop_pool),
        Some(copy_pool),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => crate::std::sync_result_ok(value),
        Err(error) => crate::std::sync_result_err(error),
    }
}

fn ok(value: Value) -> *mut Value {
    result(Ok(value))
}
fn err(message: impl Into<String>) -> *mut Value {
    crate::std::sync_result_err(message)
}

fn object(type_id: TypeId, id: i64) -> *mut Value {
    let value = alloc_object(type_id);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe {
            mux_rc_dec(value);
        }
        return std::ptr::null_mut();
    }
    unsafe {
        *ptr.cast::<i64>() = id;
    }
    value
}

unsafe fn id(value: *const Value, expected: TypeId, name: &str) -> Result<i64, String> {
    if value.is_null() {
        return Err(format!("{name} handle is null"));
    }
    if unsafe { get_object_type_id(value) } != expected {
        return Err(format!("expected {name} handle"));
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err(format!("{name} handle data is null"));
    }
    let id = unsafe { *ptr.cast::<i64>() };
    (id > 0)
        .then_some(id)
        .ok_or_else(|| format!("invalid {name} handle"))
}

trait Counted {
    fn names(&self) -> &AtomicUsize;
}
impl Counted for AtomicIntEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}
impl Counted for AtomicBoolEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}
impl Counted for CancellationEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}
impl Counted for SemaphoreEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}
impl Counted for BarrierEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}
impl Counted for ChannelEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}
impl Counted for OnceEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}

fn copy_map<T: Counted>(map: &Mutex<HashMap<i64, Arc<T>>>, source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let entry = lock(map).get(&id).cloned();
    if let Some(entry) = entry {
        entry.names().fetch_add(1, Ordering::Relaxed);
        unsafe {
            *dest.cast::<i64>() = id;
        }
    } else {
        unsafe {
            *dest.cast::<i64>() = 0;
        }
    }
}

// Concrete callbacks intentionally spell out the count mutation; this avoids
// unsafe trait tricks around Arc and keeps object-drop behavior auditable.
extern "C" fn copy_atomic_int(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&ATOMIC_INTS, source, dest);
}
extern "C" fn copy_atomic_bool(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&ATOMIC_BOOLS, source, dest);
}
extern "C" fn copy_cancellation(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&CANCELLATIONS, source, dest);
}
extern "C" fn copy_semaphore(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&SEMAPHORES, source, dest);
}
extern "C" fn copy_barrier(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&BARRIERS, source, dest);
}
extern "C" fn copy_channel(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&CHANNELS, source, dest);
}
extern "C" fn copy_once(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&ONCES, source, dest);
}

fn copy_counted<T: Counted>(
    map: &Mutex<HashMap<i64, Arc<T>>>,
    source: *mut c_void,
    dest: *mut c_void,
) {
    copy_map(map, source, dest);
}

extern "C" fn drop_atomic_int(ptr: *mut c_void) {
    drop_counted(&ATOMIC_INTS, ptr);
}
extern "C" fn drop_atomic_bool(ptr: *mut c_void) {
    drop_counted(&ATOMIC_BOOLS, ptr);
}
extern "C" fn drop_cancellation(ptr: *mut c_void) {
    drop_counted(&CANCELLATIONS, ptr);
}
extern "C" fn drop_semaphore(ptr: *mut c_void) {
    drop_counted(&SEMAPHORES, ptr);
}
extern "C" fn drop_barrier(ptr: *mut c_void) {
    drop_counted(&BARRIERS, ptr);
}
extern "C" fn drop_channel(ptr: *mut c_void) {
    drop_counted(&CHANNELS, ptr);
}
extern "C" fn drop_once(ptr: *mut c_void) {
    drop_counted(&ONCES, ptr);
}

fn drop_counted<T: Counted>(map: &Mutex<HashMap<i64, Arc<T>>>, ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut values = lock(map);
    let remove = values
        .get(&id)
        .is_some_and(|entry| entry.names().fetch_sub(1, Ordering::Relaxed) <= 1);
    if remove {
        values.remove(&id);
    }
}

fn pool_id(handle: *const Value) -> Result<i64, String> {
    unsafe { id(handle, *POOL_TYPE_ID, "WorkerPool") }
}

fn pool_entry(handle: *const Value) -> Result<Arc<PoolEntry>, String> {
    let id = pool_id(handle)?;
    lock(&POOLS)
        .get(&id)
        .cloned()
        .ok_or_else(|| "WorkerPool handle is closed".to_string())
}

struct PoolClosureRelease(usize);

impl Drop for PoolClosureRelease {
    fn drop(&mut self) {
        unsafe { crate::closure::mux_closure_release(self.0 as *mut c_void) };
    }
}

fn finish_pool_task(task: PoolTask) {
    let _release = PoolClosureRelease(task.closure);
    let callback_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        match task.argument {
            Some(argument) => invoke_map_callback(task.closure as *mut c_void, argument.0),
            None => invoke_value_callback(task.closure as *mut c_void),
        }
    }));
    let Ok(callback_result) = callback_result else {
        std::process::abort();
    };
    if let Ok(value) = callback_result {
        let sent = unsafe { mux_channel_send(task.result_channel, value) };
        unsafe {
            mux_rc_dec(sent);
            mux_rc_dec(value);
        }
    }
    unsafe {
        let closed = mux_channel_close(task.result_channel);
        mux_rc_dec(closed);
        mux_rc_dec(task.result_channel);
    }
}

fn pool_worker(state: Arc<PoolState>) {
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
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let Some(task) = task else {
            return;
        };
        finish_pool_task(task);
    }
}

fn close_pool(entry: &PoolEntry, cancel_pending: bool) -> usize {
    entry.state.closed.store(true, Ordering::Release);
    let cancelled = if cancel_pending {
        let mut queue = lock(&entry.state.queue);
        let tasks = queue.drain(..).collect::<Vec<_>>();
        entry.state.space.notify_all();
        drop(queue);
        for task in &tasks {
            unsafe {
                let closed = mux_channel_close(task.result_channel);
                mux_rc_dec(closed);
                mux_rc_dec(task.result_channel);
                crate::closure::mux_closure_release(task.closure as *mut c_void);
            }
        }
        tasks.len()
    } else {
        0
    };
    entry.state.wake.notify_all();
    let mut workers = lock(&entry.workers);
    let handles = workers.drain(..).collect::<Vec<_>>();
    drop(workers);
    for worker in handles {
        let _ = worker.join();
    }
    cancelled
}

impl Counted for PoolEntry {
    fn names(&self) -> &AtomicUsize {
        &self.names
    }
}

extern "C" fn copy_pool(source: *mut c_void, dest: *mut c_void) {
    copy_counted(&POOLS, source, dest);
}

extern "C" fn drop_pool(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let entry = {
        let mut pools = lock(&POOLS);
        let Some(entry) = pools.get(&id).cloned() else {
            return;
        };
        if entry.names.fetch_sub(1, Ordering::Relaxed) > 1 {
            return;
        }
        pools.remove(&id);
        entry
    };
    close_pool(&entry, true);
}

fn new_counted<T: Counted>(
    map: &Mutex<HashMap<i64, Arc<T>>>,
    type_id: TypeId,
    entry: T,
) -> *mut Value {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock(map).insert(id, Arc::new(entry));
    let value = object(type_id, id);
    if value.is_null() {
        lock(map).remove(&id);
        return err("could not allocate synchronization handle");
    }
    let copy = if let Value::Object(r) = unsafe { &*value } {
        Value::Object(r.clone())
    } else {
        unsafe {
            mux_rc_dec(value);
        }
        lock(map).remove(&id);
        return err("could not allocate synchronization handle");
    };
    unsafe {
        mux_rc_dec(value);
    }
    result(Ok(copy))
}

fn new_raw_counted<T: Counted>(
    map: &Mutex<HashMap<i64, Arc<T>>>,
    type_id: TypeId,
    entry: T,
) -> *mut Value {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock(map).insert(id, Arc::new(entry));
    let value = object(type_id, id);
    if value.is_null() {
        lock(map).remove(&id);
    }
    value
}

// Atomic integers ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn mux_atomic_int_new() -> *mut Value {
    new_raw_counted(
        &ATOMIC_INTS,
        *ATOMIC_INT_TYPE_ID,
        AtomicIntEntry {
            value: AtomicI64::new(0),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_atomic_int_with_value(value: i64) -> *mut Value {
    new_raw_counted(
        &ATOMIC_INTS,
        *ATOMIC_INT_TYPE_ID,
        AtomicIntEntry {
            value: AtomicI64::new(value),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_int_load(handle: *const Value) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_INT_TYPE_ID, "AtomicInt") }.and_then(|id| {
        lock(&ATOMIC_INTS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicInt handle is closed".to_string())
    }) {
        Ok(e) => ok(Value::Int(e.value.load(Ordering::SeqCst))),
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_int_store(handle: *const Value, value: i64) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_INT_TYPE_ID, "AtomicInt") }.and_then(|id| {
        lock(&ATOMIC_INTS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicInt handle is closed".to_string())
    }) {
        Ok(e) => {
            e.value.store(value, Ordering::SeqCst);
            ok(Value::Unit)
        }
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_int_add(handle: *const Value, delta: i64) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_INT_TYPE_ID, "AtomicInt") }.and_then(|id| {
        lock(&ATOMIC_INTS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicInt handle is closed".to_string())
    }) {
        Ok(e) => ok(Value::Int(e.value.fetch_add(delta, Ordering::SeqCst))),
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_int_swap(handle: *const Value, value: i64) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_INT_TYPE_ID, "AtomicInt") }.and_then(|id| {
        lock(&ATOMIC_INTS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicInt handle is closed".to_string())
    }) {
        Ok(entry) => ok(Value::Int(entry.value.swap(value, Ordering::SeqCst))),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_int_compare_exchange(
    handle: *const Value,
    expected: i64,
    replacement: i64,
) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_INT_TYPE_ID, "AtomicInt") }.and_then(|id| {
        lock(&ATOMIC_INTS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicInt handle is closed".to_string())
    }) {
        Ok(entry) => {
            let previous = entry
                .value
                .compare_exchange(expected, replacement, Ordering::SeqCst, Ordering::SeqCst)
                .unwrap_or_else(|actual| actual);
            ok(Value::Int(previous))
        }
        Err(error) => err(error),
    }
}

// Atomic booleans ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn mux_atomic_bool_new() -> *mut Value {
    new_raw_counted(
        &ATOMIC_BOOLS,
        *ATOMIC_BOOL_TYPE_ID,
        AtomicBoolEntry {
            value: AtomicBool::new(false),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_atomic_bool_with_value(value: bool) -> *mut Value {
    new_raw_counted(
        &ATOMIC_BOOLS,
        *ATOMIC_BOOL_TYPE_ID,
        AtomicBoolEntry {
            value: AtomicBool::new(value),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_bool_load(handle: *const Value) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_BOOL_TYPE_ID, "AtomicBool") }.and_then(|id| {
        lock(&ATOMIC_BOOLS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicBool handle is closed".to_string())
    }) {
        Ok(e) => ok(Value::Bool(e.value.load(Ordering::SeqCst))),
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_bool_store(handle: *const Value, value: bool) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_BOOL_TYPE_ID, "AtomicBool") }.and_then(|id| {
        lock(&ATOMIC_BOOLS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicBool handle is closed".to_string())
    }) {
        Ok(e) => {
            e.value.store(value, Ordering::SeqCst);
            ok(Value::Unit)
        }
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_bool_swap(handle: *const Value, value: bool) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_BOOL_TYPE_ID, "AtomicBool") }.and_then(|id| {
        lock(&ATOMIC_BOOLS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicBool handle is closed".to_string())
    }) {
        Ok(e) => ok(Value::Bool(e.value.swap(value, Ordering::SeqCst))),
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_atomic_bool_compare_exchange(
    handle: *const Value,
    expected: bool,
    replacement: bool,
) -> *mut Value {
    match unsafe { id(handle, *ATOMIC_BOOL_TYPE_ID, "AtomicBool") }.and_then(|id| {
        lock(&ATOMIC_BOOLS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "AtomicBool handle is closed".to_string())
    }) {
        Ok(entry) => {
            let previous = entry
                .value
                .compare_exchange(expected, replacement, Ordering::SeqCst, Ordering::SeqCst)
                .unwrap_or_else(|actual| actual);
            ok(Value::Bool(previous))
        }
        Err(error) => err(error),
    }
}

// Cooperative cancellation ------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn mux_cancellation_new() -> *mut Value {
    new_raw_counted(
        &CANCELLATIONS,
        *CANCELLATION_TYPE_ID,
        CancellationEntry {
            cancelled: AtomicBool::new(false),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cancellation_cancel(handle: *const Value) -> *mut Value {
    match unsafe { id(handle, *CANCELLATION_TYPE_ID, "CancellationToken") }.and_then(|id| {
        lock(&CANCELLATIONS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "CancellationToken handle is closed".to_string())
    }) {
        Ok(entry) => {
            entry.cancelled.store(true, Ordering::SeqCst);
            ok(Value::Unit)
        }
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cancellation_is_cancelled(handle: *const Value) -> *mut Value {
    match unsafe { id(handle, *CANCELLATION_TYPE_ID, "CancellationToken") }.and_then(|id| {
        lock(&CANCELLATIONS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "CancellationToken handle is closed".to_string())
    }) {
        Ok(entry) => ok(Value::Bool(entry.cancelled.load(Ordering::SeqCst))),
        Err(error) => err(error),
    }
}

// Semaphore ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn mux_semaphore_with_permits(permits: i64) -> *mut Value {
    if permits < 0 {
        return err("semaphore permits must not be negative");
    }
    let permits = permits as usize;
    new_counted(
        &SEMAPHORES,
        *SEMAPHORE_TYPE_ID,
        SemaphoreEntry {
            state: Mutex::new(SemaphoreState { permits }),
            wake: Condvar::new(),
            max: permits,
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_semaphore_acquire(handle: *const Value) -> *mut Value {
    let entry = match unsafe { id(handle, *SEMAPHORE_TYPE_ID, "Semaphore") }.and_then(|id| {
        lock(&SEMAPHORES)
            .get(&id)
            .cloned()
            .ok_or_else(|| "Semaphore handle is closed".to_string())
    }) {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    let mut state = entry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while state.permits == 0 {
        state = entry
            .wake
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    state.permits -= 1;
    ok(Value::Unit)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_semaphore_try_acquire(handle: *const Value) -> *mut Value {
    let entry = match unsafe { id(handle, *SEMAPHORE_TYPE_ID, "Semaphore") }.and_then(|id| {
        lock(&SEMAPHORES)
            .get(&id)
            .cloned()
            .ok_or_else(|| "Semaphore handle is closed".to_string())
    }) {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    let mut state = entry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let acquired = state.permits > 0;
    if acquired {
        state.permits -= 1;
    }
    ok(Value::Bool(acquired))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_semaphore_acquire_timeout(
    handle: *const Value,
    timeout_ms: i64,
) -> *mut Value {
    if timeout_ms < 0 {
        return err("semaphore timeout must not be negative");
    }
    let entry = match unsafe { id(handle, *SEMAPHORE_TYPE_ID, "Semaphore") }.and_then(|id| {
        lock(&SEMAPHORES)
            .get(&id)
            .cloned()
            .ok_or_else(|| "Semaphore handle is closed".to_string())
    }) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let state = entry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let timeout = match timeout_duration(timeout_ms, "semaphore") {
        Ok(timeout) => timeout,
        Err(error) => return err(error),
    };
    let mut state = match entry
        .wake
        .wait_timeout_while(state, timeout, |state| state.permits == 0)
    {
        Ok((state, _)) => state,
        Err(poisoned) => {
            let (state, _) = poisoned.into_inner();
            state
        }
    };
    if state.permits == 0 {
        return ok(Value::Bool(false));
    }
    state.permits -= 1;
    ok(Value::Bool(true))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_semaphore_release(handle: *const Value) -> *mut Value {
    let entry = match unsafe { id(handle, *SEMAPHORE_TYPE_ID, "Semaphore") }.and_then(|id| {
        lock(&SEMAPHORES)
            .get(&id)
            .cloned()
            .ok_or_else(|| "Semaphore handle is closed".to_string())
    }) {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    let mut state = entry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.permits == entry.max {
        return err("semaphore release would exceed its permit count");
    }
    state.permits += 1;
    entry.wake.notify_one();
    ok(Value::Unit)
}

// Barrier -----------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn mux_barrier_with_size(size: i64) -> *mut Value {
    if size <= 0 {
        return err("barrier size must be positive");
    }
    new_counted(
        &BARRIERS,
        *BARRIER_TYPE_ID,
        BarrierEntry {
            barrier: Barrier::new(size as usize),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_barrier_wait(handle: *const Value) -> *mut Value {
    let entry = match unsafe { id(handle, *BARRIER_TYPE_ID, "Barrier") }.and_then(|id| {
        lock(&BARRIERS)
            .get(&id)
            .cloned()
            .ok_or_else(|| "Barrier handle is closed".to_string())
    }) {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    let result = entry.barrier.wait();
    ok(Value::Bool(result.is_leader()))
}

// Channels -----------------------------------------------------------------

fn channel_entry(handle: *const Value) -> Result<Arc<ChannelEntry>, String> {
    let id = unsafe { id(handle, *CHANNEL_TYPE_ID, "Channel") }?;
    lock(&CHANNELS)
        .get(&id)
        .cloned()
        .ok_or_else(|| "Channel handle is closed".to_string())
}

/// Return the shared state behind a cancellation token.
///
/// SQL and other synchronous runtime operations use this to install a
/// backend-native cooperative cancellation callback without retaining a raw
/// Mux value pointer. The `Arc` keeps the state alive for the duration of the
/// operation even when the caller drops its token name afterward.
pub(crate) fn cancellation_entry(handle: *const Value) -> Result<Arc<CancellationEntry>, String> {
    let id = unsafe { id(handle, *CANCELLATION_TYPE_ID, "CancellationToken") }?;
    lock(&CANCELLATIONS)
        .get(&id)
        .cloned()
        .ok_or_else(|| "CancellationToken handle is closed".to_string())
}

pub(crate) fn cancellation_entry_is_cancelled(entry: &Arc<CancellationEntry>) -> bool {
    entry.cancelled.load(Ordering::Acquire)
}

pub(crate) fn cancellation_requested(handle: *const Value) -> Result<bool, String> {
    let entry = cancellation_entry(handle)?;
    Ok(cancellation_entry_is_cancelled(&entry))
}

fn channel_payload_is_send(value: &Value) -> bool {
    match value {
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_) => true,
        Value::List(values) => values.iter().all(channel_payload_is_send),
        Value::Map(values) => values
            .iter()
            .all(|(key, value)| channel_payload_is_send(key) && channel_payload_is_send(value)),
        Value::Set(values) => values.iter().all(channel_payload_is_send),
        Value::Tuple(tuple) => {
            channel_payload_is_send(&tuple.0) && channel_payload_is_send(&tuple.1)
        }
        Value::Optional(value) => value.as_deref().is_none_or(channel_payload_is_send),
        Value::Result(result) => match result {
            Ok(value) | Err(value) => channel_payload_is_send(value),
        },
        // Resource handles and boxed compiler values may contain Rc-backed
        // state. They must be moved through an explicit owned abstraction,
        // not copied into a channel implicitly.
        Value::Object(_) if crate::process::is_sendable_output(value) => true,
        Value::Object(_) | Value::Opaque(_) | Value::BoxedEnum(_) => false,
    }
}

fn channel_value(value: *const Value) -> Result<SendValue, String> {
    if value.is_null() {
        return Err("channel value is null".to_string());
    }
    let value = unsafe { &*value };
    if !channel_payload_is_send(value) {
        return Err("channel payload contains a non-sendable resource handle".to_string());
    }
    Ok(SendValue(value.clone()))
}

fn channel_sender(entry: &ChannelEntry) -> Result<ChannelSender, String> {
    lock(&entry.sender)
        .as_ref()
        .cloned()
        .ok_or_else(|| "channel is closed".to_string())
}

fn channel_object(entry: ChannelEntry) -> *mut Value {
    new_raw_counted(&CHANNELS, *CHANNEL_TYPE_ID, entry)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_channel_new_unbounded() -> *mut Value {
    let (sender, receiver) = mpsc::channel();
    channel_object(ChannelEntry {
        sender: Mutex::new(Some(ChannelSender::Unbounded(sender))),
        receiver: Mutex::new(receiver),
        capacity: None,
        closed: AtomicBool::new(false),
        names: AtomicUsize::new(1),
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_channel_new() -> *mut Value {
    mux_channel_new_unbounded()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_channel_new_bounded(capacity: i64) -> *mut Value {
    if !(1..=MAX_CHANNEL_CAPACITY).contains(&capacity) {
        return err(format!(
            "channel capacity must be between 1 and {MAX_CHANNEL_CAPACITY}"
        ));
    }
    let Ok(capacity_usize) = usize::try_from(capacity) else {
        return err("channel capacity is too large for this platform");
    };
    let (sender, receiver) = mpsc::sync_channel(capacity_usize);
    new_counted(
        &CHANNELS,
        *CHANNEL_TYPE_ID,
        ChannelEntry {
            sender: Mutex::new(Some(ChannelSender::Bounded(sender))),
            receiver: Mutex::new(receiver),
            capacity: Some(capacity_usize),
            closed: AtomicBool::new(false),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_send(handle: *const Value, value: *const Value) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let value = match channel_value(value) {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let sender = match channel_sender(&entry) {
        Ok(sender) => sender,
        Err(error) => return err(error),
    };
    let result = match sender {
        ChannelSender::Unbounded(sender) => sender.send(value).map_err(|_| "channel is closed"),
        ChannelSender::Bounded(sender) => sender.send(value).map_err(|_| "channel is closed"),
    };
    match result {
        Ok(()) => ok(Value::Unit),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_try_send(
    handle: *const Value,
    value: *const Value,
) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let value = match channel_value(value) {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let sender = match channel_sender(&entry) {
        Ok(sender) => sender,
        Err(error) => return err(error),
    };
    let result = match sender {
        ChannelSender::Unbounded(sender) => sender
            .send(value)
            .map(|()| true)
            .map_err(|_| "channel is closed"),
        ChannelSender::Bounded(sender) => match sender.try_send(value) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err("channel is closed"),
        },
    };
    match result {
        Ok(sent) => ok(Value::Bool(sent)),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_send_timeout(
    handle: *const Value,
    value: *const Value,
    timeout_ms: i64,
) -> *mut Value {
    if timeout_ms < 0 {
        return err("channel timeout must not be negative");
    }
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let value = match channel_value(value) {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let sender = match channel_sender(&entry) {
        Ok(sender) => sender,
        Err(error) => return err(error),
    };
    let result = match sender {
        ChannelSender::Unbounded(sender) => sender
            .send(value)
            .map(|()| true)
            .map_err(|_| "channel is closed"),
        ChannelSender::Bounded(sender) => {
            let deadline = match timeout_deadline(timeout_ms, "channel send") {
                Ok(deadline) => deadline,
                Err(error) => return err(error),
            };
            let mut value = value;
            loop {
                match sender.try_send(value) {
                    Ok(()) => break Ok(true),
                    Err(TrySendError::Disconnected(_)) => break Err("channel is closed"),
                    Err(TrySendError::Full(returned)) => {
                        if Instant::now() >= deadline {
                            break Ok(false);
                        }
                        value = returned;
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        std::thread::sleep(remaining.min(Duration::from_millis(1)));
                    }
                }
            }
        }
    };
    match result {
        Ok(sent) => ok(Value::Bool(sent)),
        Err(error) => err(error),
    }
}

fn channel_received(value: Option<SendValue>) -> *mut Value {
    ok(Value::Optional(value.map(|value| Box::new(value.0))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_recv(handle: *const Value) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let receiver = lock(&entry.receiver);
    if let Ok(value) = receiver.recv() {
        channel_received(Some(value))
    } else {
        entry.closed.store(true, Ordering::Release);
        channel_received(None)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_try_recv(handle: *const Value) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let receiver = lock(&entry.receiver);
    match receiver.try_recv() {
        Ok(value) => channel_received(Some(value)),
        Err(TryRecvError::Empty) => channel_received(None),
        Err(TryRecvError::Disconnected) => {
            entry.closed.store(true, Ordering::Release);
            channel_received(None)
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_recv_timeout(
    handle: *const Value,
    timeout_ms: i64,
) -> *mut Value {
    if timeout_ms < 0 {
        return err("channel timeout must not be negative");
    }
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let receiver = lock(&entry.receiver);
    let timeout = match timeout_duration(timeout_ms, "channel receive") {
        Ok(timeout) => timeout,
        Err(error) => return err(error),
    };
    match receiver.recv_timeout(timeout) {
        Ok(value) => channel_received(Some(value)),
        Err(RecvTimeoutError::Timeout) => channel_received(None),
        Err(RecvTimeoutError::Disconnected) => {
            entry.closed.store(true, Ordering::Release);
            channel_received(None)
        }
    }
}

/// Wait for the first value available from any channel. Selection is
/// deterministic for simultaneously-ready channels: the input order wins.
/// A non-negative timeout in milliseconds bounds the polling loop; `none` is
/// returned when no channel receives before the deadline. Closed channels are
/// skipped, and an empty list is rejected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_select(channels: *const Value, timeout_ms: i64) -> *mut Value {
    if timeout_ms < 0 {
        return err("channel select timeout must not be negative");
    }
    let Some(Value::List(values)) = (unsafe { channels.as_ref() }) else {
        return err("channel select expects list<Channel>");
    };
    if values.is_empty() {
        return err("channel select requires at least one channel");
    }
    if values.len() > MAX_SELECT_CHANNELS {
        return err(format!(
            "channel select accepts at most {MAX_SELECT_CHANNELS} channels"
        ));
    }
    let entries = match values
        .iter()
        .map(|value| channel_entry(value as *const Value))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(entries) => entries,
        Err(error) => return err(error),
    };
    let deadline = match timeout_deadline(timeout_ms, "channel select") {
        Ok(deadline) => deadline,
        Err(error) => return err(error),
    };
    loop {
        let mut open = false;
        for (index, entry) in entries.iter().enumerate() {
            if entry.closed.load(Ordering::Acquire) {
                continue;
            }
            open = true;
            let receiver = lock(&entry.receiver);
            match receiver.try_recv() {
                Ok(value) => {
                    return ok(Value::Optional(Some(Box::new(Value::Tuple(Box::new(
                        Tuple(Value::Int(index as i64), value.0),
                    ))))));
                }
                Err(TryRecvError::Disconnected) => {
                    entry.closed.store(true, Ordering::Release);
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if !open || Instant::now() >= deadline {
            return channel_received(None);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_send_cancelled(
    handle: *const Value,
    value: *const Value,
    cancellation: *const Value,
) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let value = match channel_value(value) {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let mut value = value;
    loop {
        match cancellation_requested(cancellation) {
            Ok(true) => return err("channel send cancelled"),
            Ok(false) => {}
            Err(error) => return err(error),
        }
        let sender = match channel_sender(&entry) {
            Ok(sender) => sender,
            Err(error) => return err(error),
        };
        match sender {
            ChannelSender::Unbounded(sender) => {
                return match sender.send(value) {
                    Ok(()) => ok(Value::Bool(true)),
                    Err(_) => err("channel is closed"),
                };
            }
            ChannelSender::Bounded(sender) => match sender.try_send(value) {
                Ok(()) => return ok(Value::Bool(true)),
                Err(TrySendError::Disconnected(_)) => return err("channel is closed"),
                Err(TrySendError::Full(returned)) => {
                    value = returned;
                    std::thread::sleep(Duration::from_millis(1));
                }
            },
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_recv_cancelled(
    handle: *const Value,
    cancellation: *const Value,
) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let receiver = lock(&entry.receiver);
    loop {
        match cancellation_requested(cancellation) {
            Ok(true) => return err("channel receive cancelled"),
            Ok(false) => {}
            Err(error) => return err(error),
        }
        match receiver.recv_timeout(Duration::from_millis(1)) {
            Ok(value) => return channel_received(Some(value)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                entry.closed.store(true, Ordering::Release);
                return channel_received(None);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_close(handle: *const Value) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    entry.closed.store(true, Ordering::Release);
    lock(&entry.sender).take();
    ok(Value::Unit)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_is_closed(handle: *const Value) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    ok(Value::Bool(entry.closed.load(Ordering::Acquire)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_channel_capacity(handle: *const Value) -> *mut Value {
    let entry = match channel_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    ok(entry.capacity.map_or(Value::Optional(None), |capacity| {
        Value::Optional(Some(Box::new(Value::Int(capacity as i64))))
    }))
}

// WorkerPool ----------------------------------------------------------------

fn pool_object(entry: PoolEntry) -> Result<*mut Value, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock(&POOLS).insert(id, Arc::new(entry));
    let value = object(*POOL_TYPE_ID, id);
    if value.is_null() {
        if let Some(entry) = lock(&POOLS).remove(&id) {
            close_pool(&entry, true);
        }
        return Err("could not allocate WorkerPool handle".to_string());
    }
    Ok(value)
}

fn create_pool(workers: i64, queue_capacity: i64) -> Result<*mut Value, String> {
    if !(1..=MAX_POOL_WORKERS).contains(&workers) {
        return Err(format!(
            "WorkerPool worker count must be between 1 and {MAX_POOL_WORKERS}"
        ));
    }
    if !(1..=MAX_POOL_QUEUE_CAPACITY).contains(&queue_capacity) {
        return Err(format!(
            "WorkerPool queue capacity must be between 1 and {MAX_POOL_QUEUE_CAPACITY}"
        ));
    }
    let worker_count = usize::try_from(workers).map_or(0, |value| value);
    let queue_capacity = usize::try_from(queue_capacity).map_or(0, |value| value);
    let state = Arc::new(PoolState {
        queue: Mutex::new(VecDeque::with_capacity(queue_capacity)),
        wake: Condvar::new(),
        space: Condvar::new(),
        queue_capacity,
        closed: AtomicBool::new(false),
    });
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let worker_state = Arc::clone(&state);
        match std::thread::Builder::new().spawn(move || pool_worker(worker_state)) {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                state.closed.store(true, Ordering::Release);
                state.wake.notify_all();
                for handle in handles {
                    let _ = handle.join();
                }
                return Err(format!("failed to start WorkerPool worker: {error}"));
            }
        }
    }
    pool_object(PoolEntry {
        state,
        workers: Mutex::new(handles),
        names: AtomicUsize::new(1),
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_pool_new() -> *mut Value {
    match create_pool(DEFAULT_POOL_WORKERS, DEFAULT_POOL_QUEUE_CAPACITY as i64) {
        Ok(value) => value,
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_pool_with_size(workers: i64) -> *mut Value {
    match create_pool(
        workers,
        workers.saturating_mul(DEFAULT_POOL_QUEUE_CAPACITY as i64),
    ) {
        Ok(value) => unsafe { object_result(value) },
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_pool_with_config(workers: i64, queue_capacity: i64) -> *mut Value {
    match create_pool(workers, queue_capacity) {
        Ok(value) => unsafe { object_result(value) },
        Err(error) => err(error),
    }
}

unsafe fn object_result(value: *mut Value) -> *mut Value {
    if value.is_null() {
        return err("could not allocate WorkerPool handle");
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return err("could not allocate WorkerPool handle");
    };
    let copied = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    ok(copied)
}

fn pool_enqueue(
    entry: &PoolEntry,
    task: PoolTask,
    timeout_ms: Option<i64>,
) -> Result<bool, String> {
    if timeout_ms.is_some_and(|timeout| timeout < 0) {
        return Err("pool timeout must not be negative".to_string());
    }
    let deadline = match timeout_ms {
        Some(timeout) => match timeout_deadline(timeout, "pool") {
            Ok(deadline) => Some(deadline),
            Err(error) => return Err(error),
        },
        None => None,
    };
    let mut queue = lock(&entry.state.queue);
    loop {
        if entry.state.closed.load(Ordering::Acquire) {
            return Err("pool is closed".to_string());
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
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue = next_queue;
            if wait_result.timed_out() && queue.len() >= entry.state.queue_capacity {
                return Ok(false);
            }
        } else {
            queue = entry
                .state
                .space
                .wait(queue)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

unsafe fn release_pool_task(task: &PoolTask) {
    unsafe {
        let closed = mux_channel_close(task.result_channel);
        mux_rc_dec(closed);
        mux_rc_dec(task.result_channel);
        crate::closure::mux_closure_release(task.closure as *mut c_void);
        mux_rc_dec(task.result_channel);
    }
}

unsafe fn snapshot_pool_callback(closure: *mut c_void) -> Result<*mut c_void, String> {
    if closure.is_null() {
        return Err("WorkerPool callback is null".to_string());
    }
    let source = unsafe { &*closure.cast::<ClosureRepr>() };
    let count = usize::try_from(source.capture_count)
        .map_err(|_| "invalid callback capture count".to_string())?;
    if count != 0 && source.captures_ptr.is_null() {
        return Err("callback captures are missing".to_string());
    }
    let captures =
        unsafe { libc::calloc(count, std::mem::size_of::<*mut c_void>()) }.cast::<*mut c_void>();
    if count != 0 && captures.is_null() {
        return Err("could not allocate callback captures".to_string());
    }
    let base =
        unsafe { libc::malloc(std::mem::size_of::<i64>() + std::mem::size_of::<ClosureRepr>()) };
    if base.is_null() {
        unsafe { libc::free(captures.cast()) };
        return Err("could not allocate callback".to_string());
    }
    let copied = unsafe { base.cast::<i64>().add(1).cast::<ClosureRepr>() };
    unsafe {
        *base.cast::<i64>() = 1;
        copied.write(ClosureRepr {
            function_ptr: source.function_ptr,
            captures_ptr: if count == 0 {
                std::ptr::null_mut()
            } else {
                captures.cast()
            },
            capture_count: 0,
            boxed_function_ptr: source.boxed_function_ptr,
        });
    }
    if count == 0 {
        unsafe { libc::free(captures.cast()) };
    }
    for index in 0..count {
        let cell = unsafe { *source.captures_ptr.cast::<*mut *mut Value>().add(index) };
        if cell.is_null() {
            unsafe { crate::closure::mux_closure_release(copied.cast()) };
            return Err("callback capture cell is null".to_string());
        }
        let value = unsafe { *cell };
        let Some(value) = (unsafe { value.as_ref() }) else {
            unsafe { crate::closure::mux_closure_release(copied.cast()) };
            return Err("callback capture is null".to_string());
        };
        if !channel_payload_is_send(value) {
            unsafe { crate::closure::mux_closure_release(copied.cast()) };
            return Err("WorkerPool captures must be safe to send between threads".to_string());
        }
        let value = mux_rc_alloc(value.clone());
        let cell = unsafe { crate::closure::mux_cell_alloc(value) };
        if cell.is_null() {
            unsafe {
                mux_rc_dec(value);
                crate::closure::mux_closure_release(copied.cast());
            }
            return Err("could not allocate callback capture".to_string());
        }
        unsafe {
            captures.add(index).write(cell);
            (*copied).capture_count += 1;
        }
    }
    Ok(copied.cast())
}

/// Clone a callback and its captures for an independent worker.
///
/// Each captured value is checked with the same sendability rule used by
/// `WorkerPool`. The returned closure owns its capture cells and must be
/// released with `mux_closure_release` by its worker.
pub(crate) unsafe fn snapshot_sendable_callback(
    closure: *mut c_void,
) -> Result<*mut c_void, String> {
    unsafe { snapshot_pool_callback(closure) }
}

unsafe fn enqueue_pool_task(
    pool: *const Value,
    closure: *mut c_void,
    timeout_ms: Option<i64>,
    argument: Option<SendValue>,
) -> Result<Option<*mut Value>, String> {
    let entry = pool_entry(pool)?;
    if closure.is_null() {
        return Err("WorkerPool callback is null".to_string());
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let result_channel = channel_object(ChannelEntry {
        sender: Mutex::new(Some(ChannelSender::Bounded(sender))),
        receiver: Mutex::new(receiver),
        capacity: Some(1),
        closed: AtomicBool::new(false),
        names: AtomicUsize::new(1),
    });
    if result_channel.is_null() {
        return Err("could not allocate WorkerPool result channel".to_string());
    }
    let closure = match unsafe { snapshot_pool_callback(closure) } {
        Ok(closure) => closure,
        Err(error) => {
            unsafe { mux_rc_dec(result_channel) };
            return Err(error);
        }
    };
    unsafe {
        mux_rc_inc(result_channel);
    }
    let task = PoolTask {
        closure: closure as usize,
        result_channel,
        argument,
    };
    match pool_enqueue(&entry, task, timeout_ms) {
        Ok(true) => Ok(Some(result_channel)),
        Ok(false) => {
            unsafe {
                release_pool_task(&PoolTask {
                    closure: closure as usize,
                    result_channel,
                    argument: None,
                });
            }
            Ok(None)
        }
        Err(error) => {
            unsafe {
                release_pool_task(&PoolTask {
                    closure: closure as usize,
                    result_channel,
                    argument: None,
                });
            }
            Err(error)
        }
    }
}

unsafe fn channel_handle_result(channel: *mut Value) -> *mut Value {
    if channel.is_null() {
        return err("WorkerPool result channel is null");
    }
    let Value::Object(reference) = (unsafe { &*channel }) else {
        unsafe { mux_rc_dec(channel) };
        return err("WorkerPool result channel has an invalid type");
    };
    let value = Value::Object(reference.clone());
    unsafe { mux_rc_dec(channel) };
    ok(value)
}

unsafe fn optional_channel_result(channel: Option<*mut Value>) -> *mut Value {
    let value = match channel {
        Some(channel) => {
            if channel.is_null() {
                return err("WorkerPool result channel is null");
            }
            let Value::Object(reference) = (unsafe { &*channel }) else {
                unsafe { mux_rc_dec(channel) };
                return err("WorkerPool result channel has an invalid type");
            };
            let value = Value::Object(reference.clone());
            unsafe { mux_rc_dec(channel) };
            Value::Optional(Some(Box::new(value)))
        }
        None => Value::Optional(None),
    };
    ok(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_pool_submit(pool: *const Value, closure: *mut c_void) -> *mut Value {
    match unsafe { enqueue_pool_task(pool, closure, None, None) } {
        Ok(Some(channel)) => unsafe { channel_handle_result(channel) },
        Ok(None) => err("WorkerPool submission was unexpectedly not accepted"),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_pool_try_submit(
    pool: *const Value,
    closure: *mut c_void,
) -> *mut Value {
    match unsafe { enqueue_pool_task(pool, closure, Some(0), None) } {
        Ok(channel) => unsafe { optional_channel_result(channel) },
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_pool_submit_timeout(
    pool: *const Value,
    closure: *mut c_void,
    timeout_ms: i64,
) -> *mut Value {
    match unsafe { enqueue_pool_task(pool, closure, Some(timeout_ms), None) } {
        Ok(channel) => unsafe { optional_channel_result(channel) },
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_pool_cancel_pending(pool: *const Value) -> *mut Value {
    let entry = match pool_entry(pool) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let mut queue = lock(&entry.state.queue);
    let tasks = queue.drain(..).collect::<Vec<_>>();
    entry.state.space.notify_all();
    drop(queue);
    for task in &tasks {
        unsafe {
            let closed = mux_channel_close(task.result_channel);
            mux_rc_dec(closed);
            mux_rc_dec(task.result_channel);
            crate::closure::mux_closure_release(task.closure as *mut c_void);
        }
    }
    ok(Value::Int(tasks.len() as i64))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_pool_close(pool: *const Value) -> *mut Value {
    let entry = match pool_entry(pool) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    close_pool(&entry, false);
    ok(Value::Unit)
}

/// Runs callbacks on pool workers and collects results in input order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_pool_map(
    pool: *const Value,
    values: *const Value,
    closure: *mut c_void,
) -> *mut Value {
    let entry = match pool_entry(pool) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    if entry.state.closed.load(Ordering::Acquire) {
        return err("pool is closed");
    }
    let Some(Value::List(values)) = (unsafe { values.as_ref() }) else {
        return err("WorkerPool.map requires a list");
    };
    if closure.is_null() {
        return err("WorkerPool callback is null");
    }
    if values.iter().any(|value| !channel_payload_is_send(value)) {
        return err("WorkerPool.map inputs must be safe to send between threads");
    }
    let mut channels = Vec::with_capacity(values.len());
    let mut failure = None;
    for value in values {
        match unsafe { enqueue_pool_task(pool, closure, None, Some(SendValue(value.clone()))) } {
            Ok(Some(channel)) => channels.push(channel),
            Ok(None) => {
                failure = Some("WorkerPool map submission was not accepted".to_string());
                break;
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let mut output = Vec::with_capacity(channels.len());
    for channel in channels {
        match channel_entry(channel) {
            Ok(entry) => match lock(&entry.receiver).recv() {
                Ok(value) => output.push(value.0),
                Err(_) => {
                    failure.get_or_insert_with(|| {
                        "WorkerPool map task was cancelled or failed".to_string()
                    });
                }
            },
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
        unsafe { mux_rc_dec(channel) };
    }
    match failure {
        Some(error) => err(error),
        None => ok(Value::List(output)),
    }
}

// Once ---------------------------------------------------------------------

fn once_entry(handle: *const Value) -> Result<Arc<OnceEntry>, String> {
    let id = unsafe { id(handle, *ONCE_TYPE_ID, "Once") }?;
    lock(&ONCES)
        .get(&id)
        .cloned()
        .ok_or_else(|| "Once handle is closed".to_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_once_new() -> *mut Value {
    new_raw_counted(
        &ONCES,
        *ONCE_TYPE_ID,
        OnceEntry {
            state: Mutex::new(OnceStatus::Pending),
            wake: Condvar::new(),
            names: AtomicUsize::new(1),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_once_call(handle: *const Value, callback: *mut c_void) -> *mut Value {
    let entry = match once_entry(handle) {
        Ok(entry) => entry,
        Err(error) => return err(error),
    };
    let mut state = lock(&entry.state);
    loop {
        match *state {
            OnceStatus::Complete => return ok(Value::Unit),
            OnceStatus::Pending => {
                *state = OnceStatus::Running;
                break;
            }
            OnceStatus::Running => {
                state = entry
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }
    drop(state);

    let callback_result = unsafe { invoke_void_callback(callback) };
    let mut state = lock(&entry.state);
    if callback_result.is_ok() {
        *state = OnceStatus::Complete;
    } else {
        *state = OnceStatus::Pending;
    }
    entry.wake.notify_all();
    drop(state);
    match callback_result {
        Ok(()) => ok(Value::Unit),
        Err(error) => err(error),
    }
}
