//! Cross-platform readiness polling for TCP, UDP, and listener sockets.

use crate::net::{clone_tcp_listener, clone_tcp_stream, clone_udp_socket};
use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_dec;
use crate::{TypeId, Value};
use mio::net::{TcpListener, TcpStream, UdpSocket};
use mio::{Events, Interest, Poll, Token};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

enum RegisteredSource {
    TcpStream(TcpStream),
    TcpListener(TcpListener),
    UdpSocket(UdpSocket),
}

struct PollerState {
    poll: Poll,
    events: Events,
    sources: HashMap<usize, RegisteredSource>,
    next_token: usize,
}

struct PollerEntry {
    state: Mutex<PollerState>,
    refs: AtomicUsize,
}

#[derive(Clone)]
struct PollEventEntry {
    token: i64,
    readable: bool,
    writable: bool,
    error: bool,
    closed: bool,
    refs: usize,
}

static POLLERS: LazyLock<Mutex<HashMap<i64, Arc<PollerEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static EVENTS: LazyLock<Mutex<HashMap<i64, PollEventEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static POLLER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Poller",
        size_of::<i64>(),
        Some(drop_poller as extern "C" fn(*mut c_void)),
        Some(copy_poller as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static EVENT_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "PollEvent",
        size_of::<i64>(),
        Some(drop_event as extern "C" fn(*mut c_void)),
        Some(copy_event as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_id() -> i64 {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}

fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => crate::std::net_result_ok(value),
        Err(error) => crate::std::net_result_err(error),
    }
}

fn object(type_id: TypeId, id: i64) -> Result<Value, String> {
    let value = alloc_object(type_id);
    if value.is_null() {
        return Err("could not allocate poller object".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        return Err("could not allocate poller object".to_string());
    }
    unsafe { *ptr.cast::<i64>() = id };
    let owned = unsafe { (*value).clone() };
    unsafe { mux_rc_dec(value) };
    Ok(owned)
}

fn handle(value: *const Value, type_id: TypeId, label: &str) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return Err(format!("expected {label} value"));
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err(format!("{label} value is invalid"));
    }
    let id = unsafe { *ptr.cast::<i64>() };
    (id > 0)
        .then_some(id)
        .ok_or_else(|| format!("{label} value is invalid"))
}

fn copy_resource<T>(
    map: &Mutex<HashMap<i64, T>>,
    source: *mut c_void,
    dest: *mut c_void,
    refs: impl Fn(&mut T),
) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = lock(map).get_mut(&id) {
        refs(entry);
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn copy_poller(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let entries = lock(&POLLERS);
    if let Some(entry) = entries.get(&id) {
        entry.refs.fetch_add(1, Ordering::Relaxed);
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn copy_event(source: *mut c_void, dest: *mut c_void) {
    copy_resource(&EVENTS, source, dest, |entry| entry.refs += 1);
}

extern "C" fn drop_poller(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut entries = lock(&POLLERS);
    let remove = entries.get(&id).is_some_and(|entry| {
        entry
            .refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |refs| {
                (refs > 0).then_some(refs - 1)
            })
            .is_ok_and(|refs| refs == 1)
    });
    if remove {
        entries.remove(&id);
    }
}

extern "C" fn drop_event(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut entries = lock(&EVENTS);
    if entries.get_mut(&id).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    }) {
        entries.remove(&id);
    }
}

fn interest(readable: i32, writable: i32) -> Result<Interest, String> {
    match (readable != 0, writable != 0) {
        (false, false) => {
            Err("poll registration requires readable or writable interest".to_string())
        }
        (true, false) => Ok(Interest::READABLE),
        (false, true) => Ok(Interest::WRITABLE),
        (true, true) => Ok(Interest::READABLE.add(Interest::WRITABLE)),
    }
}

fn with_poller<R>(
    value: *const Value,
    f: impl FnOnce(&mut PollerState) -> Result<R, String>,
) -> Result<R, String> {
    let id = handle(value, *POLLER_TYPE_ID, "Poller")?;
    with_poller_id(id, f)
}

fn with_poller_id<R>(
    id: i64,
    f: impl FnOnce(&mut PollerState) -> Result<R, String>,
) -> Result<R, String> {
    let entry = {
        let entries = lock(&POLLERS);
        entries
            .get(&id)
            .cloned()
            .ok_or_else(|| "Poller is closed".to_string())?
    };
    let mut state = lock(&entry.state);
    f(&mut state)
}

fn insert_event(event: &mio::event::Event) -> Result<Value, String> {
    let id = next_id();
    lock(&EVENTS).insert(
        id,
        PollEventEntry {
            token: event.token().0 as i64,
            readable: event.is_readable(),
            writable: event.is_writable(),
            error: event.is_error(),
            closed: event.is_read_closed() || event.is_write_closed(),
            refs: 1,
        },
    );
    match object(*EVENT_TYPE_ID, id) {
        Ok(value) => Ok(value),
        Err(error) => {
            lock(&EVENTS).remove(&id);
            Err(error)
        }
    }
}

fn register_source(
    poller: *const Value,
    source: RegisteredSource,
    readable: i32,
    writable: i32,
) -> Result<i64, String> {
    let interest = interest(readable, writable)?;
    with_poller(poller, |state| {
        let token = state.next_token;
        state.next_token = state.next_token.checked_add(1).unwrap_or(1);
        let mut source = source;
        match &mut source {
            RegisteredSource::TcpStream(source) => {
                state
                    .poll
                    .registry()
                    .register(source, Token(token), interest)
            }
            RegisteredSource::TcpListener(source) => {
                state
                    .poll
                    .registry()
                    .register(source, Token(token), interest)
            }
            RegisteredSource::UdpSocket(source) => {
                state
                    .poll
                    .registry()
                    .register(source, Token(token), interest)
            }
        }
        .map_err(|error| format!("poller register failed: {error}"))?;
        state.sources.insert(token, source);
        Ok(token as i64)
    })
}

#[unsafe(no_mangle)]
/// Creates an empty readiness poller.
pub extern "C" fn mux_poller_new() -> *mut Value {
    let poll = match Poll::new() {
        Ok(poll) => poll,
        Err(error) => return result(Err(format!("poller creation failed: {error}"))),
    };
    let id = next_id();
    lock(&POLLERS).insert(
        id,
        Arc::new(PollerEntry {
            state: Mutex::new(PollerState {
                poll,
                events: Events::with_capacity(128),
                sources: HashMap::new(),
                next_token: 1,
            }),
            refs: AtomicUsize::new(1),
        }),
    );
    match object(*POLLER_TYPE_ID, id) {
        Ok(value) => result(Ok(value)),
        Err(error) => {
            lock(&POLLERS).remove(&id);
            result(Err(error))
        }
    }
}

#[unsafe(no_mangle)]
/// Registers a cloned TCP stream and returns its poll token.
///
/// # Safety
/// `poller` and `stream` must point to live Mux values for the duration.
pub unsafe extern "C" fn mux_poller_register_tcp(
    poller: *const Value,
    stream: *const Value,
    readable: i32,
    writable: i32,
) -> *mut Value {
    let source = match clone_tcp_stream(stream).and_then(|stream| {
        stream
            .set_nonblocking(true)
            .map_err(|error| format!("poller tcp nonblocking failed: {error}"))?;
        Ok(RegisteredSource::TcpStream(TcpStream::from_std(stream)))
    }) {
        Ok(source) => source,
        Err(error) => return result(Err(error)),
    };
    match register_source(poller, source, readable, writable) {
        Ok(token) => result(Ok(Value::Int(token))),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
/// Registers a cloned TCP listener and returns its poll token.
///
/// # Safety
/// `poller` and `listener` must point to live Mux values for the duration.
pub unsafe extern "C" fn mux_poller_register_listener(
    poller: *const Value,
    listener: *const Value,
) -> *mut Value {
    let source = match clone_tcp_listener(listener).and_then(|listener| {
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("poller listener nonblocking failed: {error}"))?;
        Ok(RegisteredSource::TcpListener(TcpListener::from_std(
            listener,
        )))
    }) {
        Ok(source) => source,
        Err(error) => return result(Err(error)),
    };
    match register_source(poller, source, 1, 0) {
        Ok(token) => result(Ok(Value::Int(token))),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
/// Registers a cloned UDP socket and returns its poll token.
///
/// # Safety
/// `poller` and `socket` must point to live Mux values for the duration.
pub unsafe extern "C" fn mux_poller_register_udp(
    poller: *const Value,
    socket: *const Value,
    readable: i32,
    writable: i32,
) -> *mut Value {
    let source = match clone_udp_socket(socket).and_then(|socket| {
        socket
            .set_nonblocking(true)
            .map_err(|error| format!("poller udp nonblocking failed: {error}"))?;
        Ok(RegisteredSource::UdpSocket(UdpSocket::from_std(socket)))
    }) {
        Ok(source) => source,
        Err(error) => return result(Err(error)),
    };
    match register_source(poller, source, readable, writable) {
        Ok(token) => result(Ok(Value::Int(token))),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
/// Removes a registration by token.
///
/// # Safety
/// `poller` must point to a live Mux value for the duration.
pub unsafe extern "C" fn mux_poller_deregister(poller: *const Value, token: i64) -> *mut Value {
    if token <= 0 {
        return result(Err("poll token must be positive".to_string()));
    }
    let result_value = with_poller(poller, |state| {
        let Some(mut source) = state.sources.remove(&(token as usize)) else {
            return Err("poll token is not registered".to_string());
        };
        match &mut source {
            RegisteredSource::TcpStream(source) => state.poll.registry().deregister(source),
            RegisteredSource::TcpListener(source) => state.poll.registry().deregister(source),
            RegisteredSource::UdpSocket(source) => state.poll.registry().deregister(source),
        }
        .map_err(|error| format!("poller deregister failed: {error}"))
    });
    result(result_value.map(|()| Value::Unit))
}

#[unsafe(no_mangle)]
/// Waits for readiness events. A timeout of `-1` waits indefinitely; `0` polls
/// without blocking.
///
/// # Safety
/// `poller` must point to a live Mux value for the duration.
pub unsafe extern "C" fn mux_poller_poll(poller: *const Value, timeout_ms: i64) -> *mut Value {
    if timeout_ms < -1 {
        return result(Err(
            "poll timeout must be -1 or non-negative milliseconds".to_string()
        ));
    }
    let timeout = (timeout_ms >= 0).then(|| Duration::from_millis(timeout_ms as u64));
    match with_poller(poller, |state| {
        state.events.clear();
        state
            .poll
            .poll(&mut state.events, timeout)
            .map_err(|error| format!("poller wait failed: {error}"))?;
        state
            .events
            .iter()
            .map(insert_event)
            .collect::<Result<Vec<_>, _>>()
    }) {
        Ok(events) => result(Ok(Value::List(events))),
        Err(error) => result(Err(error)),
    }
}

#[derive(Clone, Copy)]
enum PollEventField {
    Token,
    Readable,
    Writable,
    Error,
    Closed,
}

fn event_field(event: *const Value, field: PollEventField) -> Result<Value, String> {
    let id = handle(event, *EVENT_TYPE_ID, "PollEvent")?;
    let entries = lock(&EVENTS);
    let entry = entries
        .get(&id)
        .ok_or_else(|| "PollEvent is closed".to_string())?;
    match field {
        PollEventField::Token => Ok(Value::Int(entry.token)),
        PollEventField::Readable => Ok(Value::Bool(entry.readable)),
        PollEventField::Writable => Ok(Value::Bool(entry.writable)),
        PollEventField::Error => Ok(Value::Bool(entry.error)),
        PollEventField::Closed => Ok(Value::Bool(entry.closed)),
    }
}

macro_rules! event_accessor {
    ($name:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        /// # Safety
        /// `event` must point to a live `PollEvent` value for the duration.
        pub unsafe extern "C" fn $name(event: *const Value) -> *mut Value {
            result(event_field(event, $field))
        }
    };
}

event_accessor!(mux_poll_event_token, PollEventField::Token);
event_accessor!(mux_poll_event_readable, PollEventField::Readable);
event_accessor!(mux_poll_event_writable, PollEventField::Writable);
event_accessor!(mux_poll_event_error, PollEventField::Error);
event_accessor!(mux_poll_event_closed, PollEventField::Closed);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn poller_entry_lock_does_not_block_registry_progress_for_a_copied_handle() -> Result<(), String>
    {
        let poll = Poll::new().map_err(|error| error.to_string())?;
        let id = next_id();
        lock(&POLLERS).insert(
            id,
            Arc::new(PollerEntry {
                state: Mutex::new(PollerState {
                    poll,
                    events: Events::with_capacity(1),
                    sources: HashMap::new(),
                    next_token: 1,
                }),
                refs: AtomicUsize::new(1),
            }),
        );
        let mut source = id;
        let mut copied = 0;
        copy_poller(
            (&mut source as *mut i64).cast(),
            (&mut copied as *mut i64).cast(),
        );
        if copied != id {
            lock(&POLLERS).remove(&id);
            return Err("copying the poller handle did not retain the poller".to_string());
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let operation = thread::spawn(move || {
            with_poller_id(copied, |state| {
                let _ = state;
                entered_tx
                    .send(())
                    .map_err(|error| format!("poller test synchronization failed: {error}"))?;
                thread::sleep(Duration::from_millis(250));
                Ok(())
            })
        });
        entered_rx
            .recv_timeout(Duration::from_millis(100))
            .map_err(|error| format!("poller entry lock was not acquired: {error}"))?;

        let (progress_tx, progress_rx) = mpsc::channel();
        let registry_probe = thread::spawn(move || {
            let _pollers = lock(&POLLERS);
            progress_tx
                .send(())
                .map_err(|error| format!("poller registry synchronization failed: {error}"))
        });
        let progressed = progress_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        operation
            .join()
            .map_err(|_| "poller operation thread panicked".to_string())??;
        registry_probe
            .join()
            .map_err(|_| "poller registry probe panicked".to_string())??;
        lock(&POLLERS).remove(&id);
        if !progressed {
            return Err("poller registry remained locked during entry work".to_string());
        }
        Ok(())
    }
}
