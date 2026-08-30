use crate::object::{alloc_object, get_object_ptr, get_object_type_id, register_object_type};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::TypeId;
use crate::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
mod sync_backend {
    use std::mem::MaybeUninit;

    pub type MuxMutex = libc::pthread_mutex_t;
    pub type MuxRwLock = libc::pthread_rwlock_t;
    pub type MuxCondVar = libc::pthread_cond_t;

    pub fn init_mutex() -> Result<*mut MuxMutex, String> {
        let mut mutex = Box::new(MaybeUninit::<MuxMutex>::uninit());
        let rc = unsafe { libc::pthread_mutex_init(mutex.as_mut_ptr(), std::ptr::null()) };
        if rc != 0 {
            return Err(format!("pthread_mutex_init failed with error code {rc}"));
        }
        let initialized = unsafe { Box::<MaybeUninit<MuxMutex>>::assume_init(mutex) };
        Ok(Box::into_raw(initialized))
    }

    /// Destroys and deallocates an initialized POSIX mutex.
    ///
    /// # Safety
    /// `ptr` must be non-null, point to an initialized mutex allocated by
    /// [`init_mutex`], and not be locked or concurrently used.
    pub unsafe fn destroy_mutex(ptr: *mut MuxMutex) {
        unsafe {
            let _ = libc::pthread_mutex_destroy(ptr);
            drop(Box::from_raw(ptr));
        }
    }

    /// Locks an initialized POSIX mutex.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized mutex that remains
    /// alive for the duration of the call.
    pub unsafe fn lock_mutex(ptr: *mut MuxMutex) -> i32 {
        unsafe { libc::pthread_mutex_lock(ptr) }
    }

    /// Unlocks an initialized POSIX mutex held by the calling thread.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized mutex currently
    /// locked by the calling thread.
    pub unsafe fn unlock_mutex(ptr: *mut MuxMutex) -> i32 {
        unsafe { libc::pthread_mutex_unlock(ptr) }
    }

    pub fn init_rwlock() -> Result<*mut MuxRwLock, String> {
        let mut rwlock = Box::new(MaybeUninit::<MuxRwLock>::uninit());
        let rc = unsafe { libc::pthread_rwlock_init(rwlock.as_mut_ptr(), std::ptr::null()) };
        if rc != 0 {
            return Err(format!("pthread_rwlock_init failed with error code {rc}"));
        }
        let initialized = unsafe { Box::<MaybeUninit<MuxRwLock>>::assume_init(rwlock) };
        Ok(Box::into_raw(initialized))
    }

    /// Destroys and deallocates an initialized POSIX read-write lock.
    ///
    /// # Safety
    /// `ptr` must be non-null, point to an initialized lock allocated by
    /// [`init_rwlock`], and have no active readers or writer.
    pub unsafe fn destroy_rwlock(ptr: *mut MuxRwLock) {
        unsafe {
            let _ = libc::pthread_rwlock_destroy(ptr);
            drop(Box::from_raw(ptr));
        }
    }

    /// Acquires a shared POSIX read-write lock.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized lock that remains
    /// alive for the duration of the call.
    pub unsafe fn rwlock_read_lock(ptr: *mut MuxRwLock) -> i32 {
        unsafe { libc::pthread_rwlock_rdlock(ptr) }
    }

    /// Acquires an exclusive POSIX read-write lock.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized lock that remains
    /// alive for the duration of the call.
    pub unsafe fn rwlock_write_lock(ptr: *mut MuxRwLock) -> i32 {
        unsafe { libc::pthread_rwlock_wrlock(ptr) }
    }

    /// Releases a POSIX read-write lock held by the calling thread.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized lock currently held
    /// by the calling thread.
    pub unsafe fn rwlock_unlock(ptr: *mut MuxRwLock) -> i32 {
        unsafe { libc::pthread_rwlock_unlock(ptr) }
    }

    pub fn init_condvar() -> Result<*mut MuxCondVar, String> {
        let mut condvar = Box::new(MaybeUninit::<MuxCondVar>::uninit());
        let rc = unsafe { libc::pthread_cond_init(condvar.as_mut_ptr(), std::ptr::null()) };
        if rc != 0 {
            return Err(format!("pthread_cond_init failed with error code {rc}"));
        }
        let initialized = unsafe { Box::<MaybeUninit<MuxCondVar>>::assume_init(condvar) };
        Ok(Box::into_raw(initialized))
    }

    /// Destroys and deallocates an initialized POSIX condition variable.
    ///
    /// # Safety
    /// `ptr` must be non-null, point to an initialized condition variable
    /// allocated by [`init_condvar`], and have no waiters.
    pub unsafe fn destroy_condvar(ptr: *mut MuxCondVar) {
        unsafe {
            let _ = libc::pthread_cond_destroy(ptr);
            drop(Box::from_raw(ptr));
        }
    }

    /// Waits on a POSIX condition variable while atomically releasing and
    /// reacquiring its mutex.
    ///
    /// # Safety
    /// Both pointers must be non-null and point to initialized objects that
    /// remain alive for the duration of the call. `mutex_ptr` must be locked
    /// by the calling thread and associated with `cond_ptr`.
    pub unsafe fn condvar_wait(cond_ptr: *mut MuxCondVar, mutex_ptr: *mut MuxMutex) -> i32 {
        unsafe { libc::pthread_cond_wait(cond_ptr, mutex_ptr) }
    }

    /// Wakes one waiter on a POSIX condition variable.
    ///
    /// # Safety
    /// `cond_ptr` must be non-null and point to an initialized condition
    /// variable that remains alive for the duration of the call.
    pub unsafe fn condvar_signal(cond_ptr: *mut MuxCondVar) -> i32 {
        unsafe { libc::pthread_cond_signal(cond_ptr) }
    }

    /// Wakes all waiters on a POSIX condition variable.
    ///
    /// # Safety
    /// `cond_ptr` must be non-null and point to an initialized condition
    /// variable that remains alive for the duration of the call.
    pub unsafe fn condvar_broadcast(cond_ptr: *mut MuxCondVar) -> i32 {
        unsafe { libc::pthread_cond_broadcast(cond_ptr) }
    }
}

#[cfg(windows)]
mod sync_backend {
    use std::collections::HashMap;
    use std::mem::MaybeUninit;
    use std::sync::{LazyLock, Mutex, MutexGuard};
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::{
        AcquireSRWLockExclusive, AcquireSRWLockShared, DeleteCriticalSection, EnterCriticalSection,
        GetCurrentThreadId, InitializeConditionVariable, InitializeCriticalSection,
        InitializeSRWLock, LeaveCriticalSection, ReleaseSRWLockExclusive, ReleaseSRWLockShared,
        SleepConditionVariableCS, WakeAllConditionVariable, WakeConditionVariable,
        CONDITION_VARIABLE, CRITICAL_SECTION, INFINITE, SRWLOCK,
    };

    static RWLOCK_HOLD_MODES: LazyLock<Mutex<HashMap<(u32, usize), Vec<bool>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn rwlock_modes_lock() -> MutexGuard<'static, HashMap<(u32, usize), Vec<bool>>> {
        match RWLOCK_HOLD_MODES.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub type MuxMutex = CRITICAL_SECTION;
    pub type MuxRwLock = SRWLOCK;
    pub type MuxCondVar = CONDITION_VARIABLE;

    pub fn init_mutex() -> Result<*mut MuxMutex, String> {
        let mut mutex = Box::new(MaybeUninit::<MuxMutex>::uninit());
        unsafe { InitializeCriticalSection(mutex.as_mut_ptr()) };
        let initialized = unsafe { Box::<MaybeUninit<MuxMutex>>::assume_init(mutex) };
        Ok(Box::into_raw(initialized))
    }

    /// Destroys and deallocates an initialized Windows critical section.
    ///
    /// # Safety
    /// `ptr` must be non-null, point to an initialized critical section
    /// allocated by [`init_mutex`], and not be locked or concurrently used.
    pub unsafe fn destroy_mutex(ptr: *mut MuxMutex) {
        unsafe {
            DeleteCriticalSection(ptr);
            drop(Box::from_raw(ptr));
        }
    }

    /// Locks an initialized Windows critical section.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized critical section
    /// that remains alive for the duration of the call.
    pub unsafe fn lock_mutex(ptr: *mut MuxMutex) -> i32 {
        unsafe { EnterCriticalSection(ptr) };
        0
    }

    /// Unlocks an initialized Windows critical section held by the caller.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized critical section
    /// currently locked by the calling thread.
    pub unsafe fn unlock_mutex(ptr: *mut MuxMutex) -> i32 {
        unsafe { LeaveCriticalSection(ptr) };
        0
    }

    pub fn init_rwlock() -> Result<*mut MuxRwLock, String> {
        let mut rwlock = Box::new(MaybeUninit::<MuxRwLock>::uninit());
        unsafe { InitializeSRWLock(rwlock.as_mut_ptr()) };
        let initialized = unsafe { Box::<MaybeUninit<MuxRwLock>>::assume_init(rwlock) };
        Ok(Box::into_raw(initialized))
    }

    /// Destroys and deallocates an initialized Windows SRW lock.
    ///
    /// # Safety
    /// `ptr` must be non-null, point to an initialized lock allocated by
    /// [`init_rwlock`], and have no active readers or writer.
    pub unsafe fn destroy_rwlock(ptr: *mut MuxRwLock) {
        let lock_addr = ptr as usize;
        let mut hold_modes = rwlock_modes_lock();
        hold_modes.retain(|(_, addr), _| *addr != lock_addr);
        unsafe {
            drop(Box::from_raw(ptr));
        }
    }

    /// Acquires a shared Windows SRW lock.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized lock that remains
    /// alive for the duration of the call.
    pub unsafe fn rwlock_read_lock(ptr: *mut MuxRwLock) -> i32 {
        let thread_id = unsafe { GetCurrentThreadId() };
        let key = (thread_id, ptr as usize);
        unsafe { AcquireSRWLockShared(ptr) };
        {
            let mut hold_modes = rwlock_modes_lock();
            hold_modes.entry(key).or_default().push(false);
        }
        0
    }

    /// Acquires an exclusive Windows SRW lock.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized lock that remains
    /// alive for the duration of the call.
    pub unsafe fn rwlock_write_lock(ptr: *mut MuxRwLock) -> i32 {
        let thread_id = unsafe { GetCurrentThreadId() };
        let key = (thread_id, ptr as usize);
        unsafe { AcquireSRWLockExclusive(ptr) };
        {
            let mut hold_modes = rwlock_modes_lock();
            hold_modes.entry(key).or_default().push(true);
        }
        0
    }

    /// Releases a Windows SRW lock held by the caller.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized lock currently held
    /// by the calling thread.
    pub unsafe fn rwlock_unlock(ptr: *mut MuxRwLock) -> i32 {
        let thread_id = unsafe { GetCurrentThreadId() };
        let key = (thread_id, ptr as usize);
        let mode = {
            let mut hold_modes = rwlock_modes_lock();
            if let Some(modes) = hold_modes.get_mut(&key) {
                let mode = modes.pop();
                if modes.is_empty() {
                    hold_modes.remove(&key);
                }
                mode
            } else {
                None
            }
        };

        match mode {
            Some(true) => unsafe { ReleaseSRWLockExclusive(ptr) },
            Some(false) => unsafe { ReleaseSRWLockShared(ptr) },
            None => {
                // No tracked hold mode for this thread; releasing a lock that
                // was never acquired is UB on Windows SRW locks.
                return -1;
            }
        }
        0
    }

    pub fn init_condvar() -> Result<*mut MuxCondVar, String> {
        let mut condvar = Box::new(MaybeUninit::<MuxCondVar>::uninit());
        unsafe { InitializeConditionVariable(condvar.as_mut_ptr()) };
        let initialized = unsafe { Box::<MaybeUninit<MuxCondVar>>::assume_init(condvar) };
        Ok(Box::into_raw(initialized))
    }

    /// Releases an initialized Windows condition variable allocation.
    ///
    /// # Safety
    /// `ptr` must be non-null and point to an initialized condition variable
    /// allocated by [`init_condvar`], with no active waiters.
    pub unsafe fn destroy_condvar(ptr: *mut MuxCondVar) {
        unsafe {
            drop(Box::from_raw(ptr));
        }
    }

    /// Waits on a Windows condition variable while releasing its mutex.
    ///
    /// # Safety
    /// Both pointers must be non-null and point to initialized objects that
    /// remain alive for the duration of the call. `mutex_ptr` must be locked
    /// by the calling thread and associated with `cond_ptr`.
    pub unsafe fn condvar_wait(cond_ptr: *mut MuxCondVar, mutex_ptr: *mut MuxMutex) -> i32 {
        let ok = unsafe { SleepConditionVariableCS(cond_ptr, mutex_ptr, INFINITE) };
        if ok == 0 {
            return unsafe { GetLastError() as i32 };
        }
        0
    }

    /// Wakes one waiter on a Windows condition variable.
    ///
    /// # Safety
    /// `cond_ptr` must be non-null and point to an initialized condition
    /// variable that remains alive for the duration of the call.
    pub unsafe fn condvar_signal(cond_ptr: *mut MuxCondVar) -> i32 {
        unsafe { WakeConditionVariable(cond_ptr) };
        0
    }

    /// Wakes all waiters on a Windows condition variable.
    ///
    /// # Safety
    /// `cond_ptr` must be non-null and point to an initialized condition
    /// variable that remains alive for the duration of the call.
    pub unsafe fn condvar_broadcast(cond_ptr: *mut MuxCondVar) -> i32 {
        unsafe { WakeAllConditionVariable(cond_ptr) };
        0
    }
}

/// Closure representation as produced by the Mux compiler.
///
/// INVARIANTS (must match codegen exactly):
/// - Field order: `function_ptr` MUST be first, `captures_ptr` MUST be second
/// - `captures_ptr` == null if and only if the closure has no captures
/// - `function_ptr` always points to a valid function with signature:
///   - `extern "C" fn()` if `captures_ptr` is null
///   - `extern "C" fn(*mut c_void)` if `captures_ptr` is non-null
///
/// These invariants are critical for safe transmutation in `mux_sync_spawn`.
/// If the compiler's closure representation changes, this must be updated.
#[repr(C)]
struct ClosureRepr {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
}

// Compile-time assertion: ClosureRepr layout assumptions
const _: () = {
    const fn assert_closure_layout() {
        let _ = std::mem::transmute::<ClosureRepr, [*mut c_void; 2]>;
    }
    assert_closure_layout();
};

struct ThreadEntry {
    handle: Option<thread::JoinHandle<()>>,
}

struct ClosureReleaseGuard(usize);

impl Drop for ClosureReleaseGuard {
    fn drop(&mut self) {
        // SAFETY: This address is a valid retained closure reference transferred
        // to the guard, and Drop releases that ownership exactly once.
        unsafe { crate::closure::mux_closure_release(self.0 as *mut c_void) };
    }
}

struct MutexEntry {
    ptr: *mut sync_backend::MuxMutex,
    active_holds: AtomicUsize,
}

struct RwLockEntry {
    ptr: *mut sync_backend::MuxRwLock,
    active_holds: AtomicUsize,
}

struct CondVarEntry {
    ptr: *mut sync_backend::MuxCondVar,
}

// Native synchronization objects are allocated once and are destroyed only
// after the registry and every active operation/lock owner release their Arc.
// The raw pointer is immutable, its allocation is owned solely by this Arc,
// and every backend call is made only while an Arc pin is held. The backend
// itself supplies the synchronization semantics for concurrent callers.
unsafe impl Send for MutexEntry {}
unsafe impl Sync for MutexEntry {}
unsafe impl Send for RwLockEntry {}
unsafe impl Sync for RwLockEntry {}
unsafe impl Send for CondVarEntry {}
unsafe impl Sync for CondVarEntry {}

impl Drop for MutexEntry {
    fn drop(&mut self) {
        if self.active_holds.load(Ordering::Acquire) != 0 {
            // A backend refused the owner-thread cleanup. Destroying an
            // active native primitive is unsafe, so deliberately leak it and
            // keep the process safe; the failed unlock is reported below.
            eprintln!(
                "mux-runtime: leaking a still-held native mutex after thread cleanup failure"
            );
            return;
        }
        // SAFETY: the entry owns this initialized native mutex and is dropped
        // only after all operation and lock-owner pins have been released.
        unsafe { sync_backend::destroy_mutex(self.ptr) };
    }
}

impl Drop for RwLockEntry {
    fn drop(&mut self) {
        if self.active_holds.load(Ordering::Acquire) != 0 {
            eprintln!(
                "mux-runtime: leaking a still-held native rwlock after thread cleanup failure"
            );
            return;
        }
        // SAFETY: the entry owns this initialized native lock and is dropped
        // only after all operation and lock-owner pins have been released.
        unsafe { sync_backend::destroy_rwlock(self.ptr) };
    }
}

impl Drop for CondVarEntry {
    fn drop(&mut self) {
        // SAFETY: the entry owns this initialized condition variable and is
        // dropped only after all operation and waiter pins have been released.
        unsafe { sync_backend::destroy_condvar(self.ptr) };
    }
}

struct HeldMutex {
    entry: Arc<MutexEntry>,
    active: bool,
}

impl Drop for HeldMutex {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Thread-local teardown runs on the lock owner thread, so it is valid
        // to release the native mutex before its final Arc pin is dropped.
        let rc = unsafe { sync_backend::unlock_mutex(self.entry.ptr) };
        if rc == 0 {
            self.entry.active_holds.fetch_sub(1, Ordering::AcqRel);
        } else {
            eprintln!(
                "mux-runtime: failed to unlock native mutex during thread cleanup (error code {rc})"
            );
        }
    }
}

struct HeldRwLock {
    entry: Arc<RwLockEntry>,
    active: bool,
}

impl Drop for HeldRwLock {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let rc = unsafe { sync_backend::rwlock_unlock(self.entry.ptr) };
        if rc == 0 {
            self.entry.active_holds.fetch_sub(1, Ordering::AcqRel);
        } else {
            eprintln!(
                "mux-runtime: failed to unlock native rwlock during thread cleanup (error code {rc})"
            );
        }
    }
}

thread_local! {
    // Keep one pin for every successful lock acquisition. Some native
    // backends permit recursive read-lock acquisition, so a single Arc per
    // handle would release the lifetime pin too early on the first unlock.
    static HELD_MUTEXES: RefCell<HashMap<i64, Vec<HeldMutex>>> = RefCell::new(HashMap::new());
    static HELD_RWLOCKS: RefCell<HashMap<i64, Vec<HeldRwLock>>> = RefCell::new(HashMap::new());
}

static NEXT_THREAD_ID: LazyLock<AtomicI64> = LazyLock::new(|| AtomicI64::new(1));
static THREADS: LazyLock<Mutex<HashMap<i64, ThreadEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_MUTEX_ID: LazyLock<AtomicI64> = LazyLock::new(|| AtomicI64::new(1));
static MUTEXES: LazyLock<Mutex<HashMap<i64, Arc<MutexEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_RWLOCK_ID: LazyLock<AtomicI64> = LazyLock::new(|| AtomicI64::new(1));
static RWLOCKS: LazyLock<Mutex<HashMap<i64, Arc<RwLockEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CONDVAR_ID: LazyLock<AtomicI64> = LazyLock::new(|| AtomicI64::new(1));
static CONDVARS: LazyLock<Mutex<HashMap<i64, Arc<CondVarEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MUTEX_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type(
        "Mutex",
        8,
        Some(destroy_mutex_object as extern "C" fn(*mut c_void)),
    )
});
static RWLOCK_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type(
        "RwLock",
        8,
        Some(destroy_rwlock_object as extern "C" fn(*mut c_void)),
    )
});
static CONDVAR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type(
        "CondVar",
        8,
        Some(destroy_condvar_object as extern "C" fn(*mut c_void)),
    )
});
static THREAD_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type(
        "Thread",
        8,
        Some(destroy_thread_object as extern "C" fn(*mut c_void)),
    )
});

fn ok_unit() -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(Value::Unit))))
}

fn err_string(message: impl Into<String>) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(Value::String(message.into())))))
}

extern "C" fn destroy_mutex_object(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let entry = {
        let mut mutexes = MUTEXES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mutexes.remove(&id)
    };
    drop(entry);
}

extern "C" fn destroy_rwlock_object(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let entry = {
        let mut rwlocks = RWLOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rwlocks.remove(&id)
    };
    drop(entry);
}

extern "C" fn destroy_condvar_object(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let entry = {
        let mut condvars = CONDVARS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        condvars.remove(&id)
    };
    drop(entry);
}

extern "C" fn destroy_thread_object(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let _entry = {
        let mut threads = THREADS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        threads.remove(&id)
    };
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
fn extract_handle_id(
    handle: *mut Value,
    expected_type_id: TypeId,
    type_name: &str,
) -> Result<i64, *mut Value> {
    if handle.is_null() {
        return Err(err_string("handle is null"));
    }
    let actual_type_id = unsafe { get_object_type_id(handle) };
    if actual_type_id != expected_type_id {
        return Err(err_string(format!("expected {type_name} handle")));
    }
    let ptr = unsafe { get_object_ptr(handle) };
    if ptr.is_null() {
        return Err(err_string("handle data is null"));
    }
    Ok(unsafe { *(ptr as *const i64) })
}

fn mutex_entry(id: i64) -> Result<Arc<MutexEntry>, *mut Value> {
    MUTEXES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| err_string(format!("Mutex handle {id} not found")))
}

fn rwlock_entry(id: i64) -> Result<Arc<RwLockEntry>, *mut Value> {
    RWLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| err_string(format!("RwLock handle {id} not found")))
}

fn condvar_entry(id: i64) -> Result<Arc<CondVarEntry>, *mut Value> {
    CONDVARS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| err_string(format!("CondVar handle {id} not found")))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sync_spawn(closure: *mut c_void) -> *mut Value {
    // SAFETY: `closure` must be a pointer to a ClosureRepr as produced by the Mux compiler.
    // The ClosureRepr struct documents the invariants that make this safe.
    if closure.is_null() {
        return err_string("sync.spawn received null function value");
    }

    {
        // The spawned thread uses the closure's captures for its whole lifetime,
        // which outlives this call. Retain the closure so the caller's scope
        // cleanup does not free it out from under the thread; the thread releases
        // it (freeing captures when it is the last owner) once the body returns.
        unsafe { crate::closure::mux_closure_retain(closure) };
        let closure_addr = closure as usize;

        // Read ClosureRepr fields before spawning so the thread does not hold a
        // raw pointer into memory that the caller may free after this call returns.
        let (function_addr, captures_addr) = unsafe {
            let r = &*(closure as *const ClosureRepr);
            (r.function_ptr as usize, r.captures_ptr as usize)
        };
        // Releases the retained closure reference when dropped. Owning it in a
        // guard (rather than releasing after the body) means the reference is
        // released on EVERY thread exit path - normal return AND unwinding if the
        // closure body panics - so the closure and its captures never leak.
        let handle = thread::Builder::new().spawn(move || {
            let _release = ClosureReleaseGuard(closure_addr);
            if captures_addr == 0 {
                let func: extern "C" fn() = unsafe { std::mem::transmute(function_addr) };
                func();
            } else {
                let func: extern "C" fn(*mut c_void) =
                    unsafe { std::mem::transmute(function_addr) };
                func(captures_addr as *mut c_void);
            }
        });

        let join_handle = match handle {
            Ok(h) => h,
            Err(e) => {
                // The thread never started, so it will never run the guard that
                // releases the retain above; release it here before returning.
                unsafe { crate::closure::mux_closure_release(closure_addr as *mut c_void) };
                return err_string(format!("Failed to spawn thread: {e}"));
            }
        };

        let id = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
        let mut threads = THREADS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        threads.insert(
            id,
            ThreadEntry {
                handle: Some(join_handle),
            },
        );
        drop(threads);

        let obj_ptr = alloc_object(*THREAD_TYPE_ID);
        let data_ptr = unsafe { get_object_ptr(obj_ptr) };
        if !data_ptr.is_null() {
            unsafe { *data_ptr.cast::<i64>() = id };
        }
        let value = unsafe { (*obj_ptr).clone() };
        unsafe { mux_rc_dec(obj_ptr) };
        mux_rc_alloc(Value::Result(Ok(Box::new(value))))
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_thread_join(thread_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(thread_handle, *THREAD_TYPE_ID, "Thread") {
        Ok(id) => id,
        Err(e) => return e,
    };

    let join_handle = {
        let mut threads = THREADS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match threads.remove(&id) {
            Some(entry) => entry.handle,
            None => return err_string(format!("Thread handle {id} not found")),
        }
    };

    let Some(handle) = join_handle else {
        return err_string("Thread already joined or detached");
    };

    match handle.join() {
        Ok(()) => ok_unit(),
        Err(_) => err_string("Thread panicked during execution"),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_thread_detach(thread_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(thread_handle, *THREAD_TYPE_ID, "Thread") {
        Ok(id) => id,
        Err(e) => return e,
    };

    let mut threads = THREADS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = threads.remove(&id) else {
        return err_string(format!("Thread handle {id} not found"));
    };
    if entry.handle.is_none() {
        return err_string("Thread already joined or detached");
    }
    ok_unit()
}

fn init_pthread_mutex() -> Result<*mut sync_backend::MuxMutex, String> {
    sync_backend::init_mutex()
}

fn init_pthread_rwlock() -> Result<*mut sync_backend::MuxRwLock, String> {
    sync_backend::init_rwlock()
}

fn init_pthread_condvar() -> Result<*mut sync_backend::MuxCondVar, String> {
    sync_backend::init_condvar()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_mutex_new() -> *mut Value {
    match init_pthread_mutex() {
        Ok(ptr) => {
            let id = NEXT_MUTEX_ID.fetch_add(1, Ordering::Relaxed);
            let mut mutexes = MUTEXES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mutexes.insert(
                id,
                Arc::new(MutexEntry {
                    ptr,
                    active_holds: AtomicUsize::new(0),
                }),
            );

            let obj_ptr = alloc_object(*MUTEX_TYPE_ID);
            let data_ptr = unsafe { get_object_ptr(obj_ptr) };
            if !data_ptr.is_null() {
                unsafe { *data_ptr.cast::<i64>() = id };
            }
            let value = unsafe { (*obj_ptr).clone() };
            unsafe { mux_rc_dec(obj_ptr) };
            mux_rc_alloc(value)
        }
        Err(e) => err_string(format!("Failed to initialize Mutex: {e}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rwlock_new() -> *mut Value {
    match init_pthread_rwlock() {
        Ok(ptr) => {
            let id = NEXT_RWLOCK_ID.fetch_add(1, Ordering::Relaxed);
            let mut rwlocks = RWLOCKS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            rwlocks.insert(
                id,
                Arc::new(RwLockEntry {
                    ptr,
                    active_holds: AtomicUsize::new(0),
                }),
            );

            let obj_ptr = alloc_object(*RWLOCK_TYPE_ID);
            let data_ptr = unsafe { get_object_ptr(obj_ptr) };
            if !data_ptr.is_null() {
                unsafe { *data_ptr.cast::<i64>() = id };
            }
            let value = unsafe { (*obj_ptr).clone() };
            unsafe { mux_rc_dec(obj_ptr) };
            mux_rc_alloc(value)
        }
        Err(e) => err_string(format!("Failed to initialize RwLock: {e}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_condvar_new() -> *mut Value {
    match init_pthread_condvar() {
        Ok(ptr) => {
            let id = NEXT_CONDVAR_ID.fetch_add(1, Ordering::Relaxed);
            let mut condvars = CONDVARS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            condvars.insert(id, Arc::new(CondVarEntry { ptr }));

            let obj_ptr = alloc_object(*CONDVAR_TYPE_ID);
            let data_ptr = unsafe { get_object_ptr(obj_ptr) };
            if !data_ptr.is_null() {
                unsafe { *data_ptr.cast::<i64>() = id };
            }
            let value = unsafe { (*obj_ptr).clone() };
            unsafe { mux_rc_dec(obj_ptr) };
            mux_rc_alloc(value)
        }
        Err(e) => err_string(format!("Failed to initialize CondVar: {e}")),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_mutex_lock(mutex_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(mutex_handle, *MUTEX_TYPE_ID, "Mutex") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = match mutex_entry(id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::lock_mutex(entry.ptr) };
    if rc != 0 {
        return err_string(format!("mux_mutex_lock failed with error code {rc}"));
    }
    entry.active_holds.fetch_add(1, Ordering::AcqRel);
    HELD_MUTEXES.with(|held| {
        held.borrow_mut().entry(id).or_default().push(HeldMutex {
            entry,
            active: true,
        });
    });
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_mutex_unlock(mutex_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(mutex_handle, *MUTEX_TYPE_ID, "Mutex") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = HELD_MUTEXES.with(|held| {
        let mut held = held.borrow_mut();
        let entries = held.get_mut(&id)?;
        let entry = entries.pop();
        if entries.is_empty() {
            held.remove(&id);
        }
        entry
    });
    let Some(mut held_entry) = entry else {
        return err_string(format!("Mutex handle {id} is not locked by this thread"));
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::unlock_mutex(held_entry.entry.ptr) };
    if rc != 0 {
        HELD_MUTEXES.with(|held| {
            held.borrow_mut().entry(id).or_default().push(held_entry);
        });
        return err_string(format!("mux_mutex_unlock failed with error code {rc}"));
    }
    held_entry.active = false;
    held_entry.entry.active_holds.fetch_sub(1, Ordering::AcqRel);
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_rwlock_read_lock(rwlock_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(rwlock_handle, *RWLOCK_TYPE_ID, "RwLock") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = match rwlock_entry(id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::rwlock_read_lock(entry.ptr) };
    if rc != 0 {
        return err_string(format!("mux_rwlock_read_lock failed with error code {rc}"));
    }
    entry.active_holds.fetch_add(1, Ordering::AcqRel);
    HELD_RWLOCKS.with(|held| {
        held.borrow_mut().entry(id).or_default().push(HeldRwLock {
            entry,
            active: true,
        });
    });
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_rwlock_write_lock(rwlock_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(rwlock_handle, *RWLOCK_TYPE_ID, "RwLock") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = match rwlock_entry(id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::rwlock_write_lock(entry.ptr) };
    if rc != 0 {
        return err_string(format!("mux_rwlock_write_lock failed with error code {rc}"));
    }
    entry.active_holds.fetch_add(1, Ordering::AcqRel);
    HELD_RWLOCKS.with(|held| {
        held.borrow_mut().entry(id).or_default().push(HeldRwLock {
            entry,
            active: true,
        });
    });
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_rwlock_unlock(rwlock_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(rwlock_handle, *RWLOCK_TYPE_ID, "RwLock") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = HELD_RWLOCKS.with(|held| {
        let mut held = held.borrow_mut();
        let entries = held.get_mut(&id)?;
        let entry = entries.pop();
        if entries.is_empty() {
            held.remove(&id);
        }
        entry
    });
    let Some(mut held_entry) = entry else {
        return err_string(format!("RwLock handle {id} is not locked by this thread"));
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::rwlock_unlock(held_entry.entry.ptr) };
    if rc != 0 {
        HELD_RWLOCKS.with(|held| {
            held.borrow_mut().entry(id).or_default().push(held_entry);
        });
        return err_string(format!("mux_rwlock_unlock failed with error code {rc}"));
    }
    held_entry.active = false;
    held_entry.entry.active_holds.fetch_sub(1, Ordering::AcqRel);
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_condvar_wait(
    condvar_handle: *mut Value,
    mutex_handle: *mut Value,
) -> *mut Value {
    let cond_id = match extract_handle_id(condvar_handle, *CONDVAR_TYPE_ID, "CondVar") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let mutex_id = match extract_handle_id(mutex_handle, *MUTEX_TYPE_ID, "Mutex") {
        Ok(id) => id,
        Err(e) => return e,
    };

    let mutex_is_held = HELD_MUTEXES.with(|held| {
        held.borrow()
            .get(&mutex_id)
            .is_some_and(|entries| !entries.is_empty())
    });
    if !mutex_is_held {
        return err_string(format!(
            "CondVar wait requires Mutex handle {mutex_id} to be locked by this thread"
        ));
    }

    let cond_entry = match condvar_entry(cond_id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };
    let mutex_entry = match mutex_entry(mutex_id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };

    // SAFETY: both pointers were read from their live-handle registries and
    // remain initialized; the Mux contract requires the mutex to be held.
    let rc = unsafe { sync_backend::condvar_wait(cond_entry.ptr, mutex_entry.ptr) };
    if rc != 0 {
        return err_string(format!("mux_condvar_wait failed with error code {rc}"));
    }
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_condvar_signal(condvar_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(condvar_handle, *CONDVAR_TYPE_ID, "CondVar") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = match condvar_entry(id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::condvar_signal(entry.ptr) };
    if rc != 0 {
        return err_string(format!("mux_condvar_signal failed with error code {rc}"));
    }
    ok_unit()
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_condvar_broadcast(condvar_handle: *mut Value) -> *mut Value {
    let id = match extract_handle_id(condvar_handle, *CONDVAR_TYPE_ID, "CondVar") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let entry = match condvar_entry(id) {
        Ok(entry) => entry,
        Err(error) => return error,
    };

    // SAFETY: the pointer was read from the live-handle registry and remains
    // initialized for this backend operation.
    let rc = unsafe { sync_backend::condvar_broadcast(entry.ptr) };
    if rc != 0 {
        return err_string(format!("mux_condvar_broadcast failed with error code {rc}"));
    }
    ok_unit()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_sync_sleep(ms: i64) {
    if ms <= 0 {
        return;
    }
    thread::sleep(Duration::from_millis(ms as u64));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refcount::{mux_rc_alloc, mux_rc_dec};

    fn release_result(result: *mut Value) {
        assert!(!result.is_null());
        assert!(unsafe { mux_rc_dec(result) });
    }

    unsafe fn clone_handle(handle: *mut Value) -> *mut Value {
        mux_rc_alloc((*handle).clone())
    }

    #[test]
    fn mutex_destroy_keeps_native_lock_alive_until_unlock() {
        let handle = mux_mutex_new();
        assert!(!handle.is_null());
        let id = extract_handle_id(handle, *MUTEX_TYPE_ID, "Mutex").unwrap();
        release_result(mux_mutex_lock(handle));

        // Dropping the last language handle removes the registry entry while
        // the native mutex is still locked. The per-thread pin must keep the
        // native object alive so the owner can finish unlocking it.
        assert!(unsafe { mux_rc_dec(handle) });
        assert!(!MUTEXES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id));
        let entry =
            HELD_MUTEXES.with(|held| held.borrow_mut().get_mut(&id).and_then(std::vec::Vec::pop));
        let mut entry = entry.expect("successful lock must retain a lifetime pin");
        assert_eq!(unsafe { sync_backend::unlock_mutex(entry.entry.ptr) }, 0);
        entry.active = false;
        entry.entry.active_holds.fetch_sub(1, Ordering::AcqRel);
        drop(entry);
    }

    #[test]
    fn rwlock_destroy_keeps_native_lock_alive_until_unlock() {
        let handle = mux_rwlock_new();
        assert!(!handle.is_null());
        let id = extract_handle_id(handle, *RWLOCK_TYPE_ID, "RwLock").unwrap();
        release_result(mux_rwlock_read_lock(handle));
        assert!(unsafe { mux_rc_dec(handle) });
        assert!(!RWLOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id));
        let entry =
            HELD_RWLOCKS.with(|held| held.borrow_mut().get_mut(&id).and_then(std::vec::Vec::pop));
        let mut entry = entry.expect("successful lock must retain a lifetime pin");
        assert_eq!(unsafe { sync_backend::rwlock_unlock(entry.entry.ptr) }, 0);
        entry.active = false;
        entry.entry.active_holds.fetch_sub(1, Ordering::AcqRel);
        drop(entry);
    }

    #[test]
    fn condvar_operation_pins_entry_after_handle_drop() {
        let handle = mux_condvar_new();
        assert!(!handle.is_null());
        let id = extract_handle_id(handle, *CONDVAR_TYPE_ID, "CondVar").unwrap();
        let entry = condvar_entry(id).unwrap();
        assert!(unsafe { mux_rc_dec(handle) });
        assert!(!CONDVARS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id));
        assert_eq!(unsafe { sync_backend::condvar_signal(entry.ptr) }, 0);
        drop(entry);
    }

    #[test]
    fn condvar_wait_and_signal_use_exported_api() {
        let condvar = mux_condvar_new();
        let mutex = mux_mutex_new();
        assert!(!condvar.is_null());
        assert!(!mutex.is_null());
        release_result(mux_mutex_lock(mutex));

        let signal_handle = condvar as usize;
        let signaler = thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            // The owning test thread keeps the Value alive until this call
            // completes; only immutable handle metadata is read here.
            let result = mux_condvar_signal(signal_handle as *mut Value);
            assert!(!result.is_null());
            assert!(unsafe { mux_rc_dec(result) });
        });
        release_result(mux_condvar_wait(condvar, mutex));
        signaler.join().unwrap();
        release_result(mux_mutex_unlock(mutex));
        assert!(unsafe { mux_rc_dec(condvar) });
        assert!(unsafe { mux_rc_dec(mutex) });
    }

    #[test]
    fn condvar_wait_rejects_unowned_mutex() {
        let condvar = mux_condvar_new();
        let mutex = mux_mutex_new();
        assert!(!condvar.is_null());
        assert!(!mutex.is_null());
        let result = mux_condvar_wait(condvar, mutex);
        assert!(!result.is_null());
        assert!(!matches!(unsafe { &*result }, Value::Result(Ok(_))));
        assert!(unsafe { mux_rc_dec(result) });
        assert!(unsafe { mux_rc_dec(condvar) });
        assert!(unsafe { mux_rc_dec(mutex) });
    }

    #[test]
    fn condvar_wait_survives_owner_handle_cleanup() {
        use std::sync::mpsc::channel;

        let condvar = mux_condvar_new();
        let mutex = mux_mutex_new();
        assert!(!condvar.is_null());
        assert!(!mutex.is_null());
        let waiter_mutex = unsafe { clone_handle(mutex) };
        let signal_condvar = unsafe { clone_handle(condvar) };
        assert!(!waiter_mutex.is_null());
        assert!(!signal_condvar.is_null());

        let (ready_tx, ready_rx) = channel();
        let waiter_mutex_addr = waiter_mutex as usize;
        let waiter_condvar_addr = condvar as usize;
        let waiter = thread::spawn(move || {
            release_result(mux_mutex_lock(waiter_mutex_addr as *mut Value));
            ready_tx.send(()).unwrap();
            release_result(mux_condvar_wait(
                waiter_condvar_addr as *mut Value,
                waiter_mutex_addr as *mut Value,
            ));
            release_result(mux_mutex_unlock(waiter_mutex_addr as *mut Value));
            assert!(unsafe { mux_rc_dec(waiter_mutex_addr as *mut Value) });
        });
        ready_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(5));

        // The waiter has already entered the exported wait path. Dropping the
        // owner's handles must not invalidate the native objects needed by the
        // in-flight wait; the signaler keeps only its own condvar handle.
        assert!(unsafe { mux_rc_dec(condvar) });
        assert!(unsafe { mux_rc_dec(mutex) });
        let signal_condvar_addr = signal_condvar as usize;
        let signaler = thread::spawn(move || {
            release_result(mux_condvar_signal(signal_condvar_addr as *mut Value));
            assert!(unsafe { mux_rc_dec(signal_condvar_addr as *mut Value) });
        });
        waiter.join().unwrap();
        signaler.join().unwrap();
    }

    #[test]
    fn owner_thread_cleanup_unlocks_before_entry_drop() {
        let owner = thread::spawn(|| {
            let mutex = mux_mutex_new();
            assert!(!mutex.is_null());
            let result = mux_mutex_lock(mutex);
            assert!(!result.is_null());
            assert!(unsafe { mux_rc_dec(result) });
            // The thread-local HeldMutex must unlock this native mutex before
            // its final Arc pin is dropped during thread teardown.
            assert!(unsafe { mux_rc_dec(mutex) });
        });
        owner.join().unwrap();
    }

    #[test]
    fn owner_thread_cleanup_makes_mutex_reacquirable() {
        let mutex = mux_mutex_new();
        assert!(!mutex.is_null());
        let handle_addr = mutex as usize;
        let owner = thread::spawn(move || {
            // The main test thread keeps the handle alive; this thread only
            // reads its immutable type/id metadata through the exported API.
            let result = mux_mutex_lock(handle_addr as *mut Value);
            assert!(!result.is_null());
            assert!(unsafe { mux_rc_dec(result) });
            // Deliberately omit unlock: thread-local cleanup must release it.
        });
        owner.join().unwrap();

        release_result(mux_mutex_lock(mutex));
        release_result(mux_mutex_unlock(mutex));
        assert!(unsafe { mux_rc_dec(mutex) });
    }
}
