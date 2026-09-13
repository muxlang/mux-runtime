use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::std::{
    mux_sync_error_detail, mux_sync_error_from_message, mux_sync_error_kind, mux_sync_error_message,
};
use mux_runtime::sync_primitives::*;
use mux_runtime::Value;

#[repr(C)]
struct MapCallback {
    function: *mut std::ffi::c_void,
    captures: *mut std::ffi::c_void,
    count: i64,
    boxed: *mut std::ffi::c_void,
}

#[test]
fn sync_errors_are_structured() {
    unsafe {
        let detail = mux_rc_alloc(Value::String("closed channel".to_string()));
        let error = mux_sync_error_from_message(detail);
        assert!(matches!(&*error, Value::Object(_)));
        let kind = mux_sync_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes()));
        let detail_value = mux_sync_error_detail(error);
        assert!(matches!(&*detail_value, Value::String(value) if value == "closed channel"));
        let message = mux_sync_error_message(error);
        assert!(matches!(&*message, Value::String(value) if value == "closed channel"));
        assert!(mux_rc_dec(message));
        assert!(mux_rc_dec(detail_value));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(detail));
    }
}

extern "C" fn mapped_string(argument: *mut Value) -> *mut Value {
    let Value::Int(number) = (unsafe { &*argument }) else {
        return std::ptr::null_mut();
    };
    if *number == 0 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    mux_rc_alloc(Value::String(number.to_string()))
}

#[test]
fn worker_pool_map_preserves_order_with_a_bounded_queue() {
    let pool = take_handle(mux_pool_with_config(3, 1));
    let mut callback = MapCallback {
        function: mapped_string as *mut std::ffi::c_void,
        captures: std::ptr::null_mut(),
        count: 0,
        boxed: mapped_string as *mut std::ffi::c_void,
    };
    let values = mux_rc_alloc(Value::List((0..30).map(Value::Int).collect()));
    let result =
        take_ok(unsafe { mux_pool_map(pool, values, (&mut callback as *mut MapCallback).cast()) });
    assert_eq!(
        result,
        Value::List((0..30).map(|n| Value::String(n.to_string())).collect())
    );
    let empty = mux_rc_alloc(Value::List(Vec::new()));
    assert_eq!(
        take_ok(unsafe { mux_pool_map(pool, empty, (&mut callback as *mut MapCallback).cast()) }),
        Value::List(Vec::new())
    );
    take_ok(unsafe { mux_pool_close(pool) });
    assert_eq!(
        take_err(unsafe { mux_pool_map(pool, empty, (&mut callback as *mut MapCallback).cast()) }),
        "pool is closed"
    );
    unsafe {
        mux_rc_dec(empty);
        mux_rc_dec(values);
        mux_rc_dec(pool);
    }
}

#[test]
fn worker_pool_rejects_null_capture_cells_and_values() {
    let pool = take_handle(mux_pool_with_config(1, 1));
    let mut null_value: *mut Value = std::ptr::null_mut();
    for (cell, expected) in [
        (std::ptr::null_mut(), "callback capture cell is null"),
        (
            &mut null_value as *mut *mut Value,
            "callback capture is null",
        ),
    ] {
        let mut captures = [cell];
        let mut callback = MapCallback {
            function: mapped_string as *mut std::ffi::c_void,
            captures: captures.as_mut_ptr().cast(),
            count: 1,
            boxed: mapped_string as *mut std::ffi::c_void,
        };
        assert_eq!(
            take_err(unsafe { mux_pool_submit(pool, (&mut callback as *mut MapCallback).cast()) }),
            expected
        );
    }
    take_ok(unsafe { mux_pool_close(pool) });
    unsafe { mux_rc_dec(pool) };
}

fn take_ok(ptr: *mut Value) -> Value {
    unsafe {
        let value = (&*ptr).clone();
        assert!(mux_rc_dec(ptr));
        match value {
            Value::Result(Ok(value)) => *value,
            Value::Result(Err(error)) => panic!("unexpected error: {error}"),
            other => panic!("expected result, got {other:?}"),
        }
    }
}

fn take_handle(ptr: *mut Value) -> *mut Value {
    let value = take_ok(ptr);
    mux_rc_alloc(value)
}

fn take_err(ptr: *mut Value) -> String {
    unsafe {
        let value = (&*ptr).clone();
        assert!(mux_rc_dec(ptr));
        match value {
            Value::Result(Err(error)) => {
                let message = mux_sync_error_message((&*error) as *const Value);
                let value = (&*message).clone();
                assert!(mux_rc_dec(message));
                match value {
                    Value::String(message) => message,
                    other => panic!("expected SyncError message, got {other:?}"),
                }
            }
            other => panic!("expected error result, got {other:?}"),
        }
    }
}

#[test]
fn atomics_are_sequentially_consistent() {
    let integer = mux_atomic_int_with_value(4);
    assert_eq!(
        take_ok(unsafe { mux_atomic_int_load(integer) }),
        Value::Int(4)
    );
    assert_eq!(
        take_ok(unsafe { mux_atomic_int_add(integer, 3) }),
        Value::Int(4)
    );
    assert_eq!(
        take_ok(unsafe { mux_atomic_int_load(integer) }),
        Value::Int(7)
    );
    assert_eq!(
        take_ok(unsafe { mux_atomic_int_store(integer, -2) }),
        Value::Unit
    );
    assert_eq!(
        take_ok(unsafe { mux_atomic_int_load(integer) }),
        Value::Int(-2)
    );

    let flag = mux_atomic_bool_new();
    assert_eq!(
        take_ok(unsafe { mux_atomic_bool_load(flag) }),
        Value::Bool(false)
    );
    assert_eq!(
        take_ok(unsafe { mux_atomic_bool_swap(flag, true) }),
        Value::Bool(false)
    );
    assert_eq!(
        take_ok(unsafe { mux_atomic_bool_load(flag) }),
        Value::Bool(true)
    );
    unsafe {
        assert!(mux_rc_dec(integer));
        assert!(mux_rc_dec(flag));
    }
}

#[test]
fn semaphore_try_acquire_and_release_are_bounded() {
    let semaphore = take_handle(mux_semaphore_with_permits(1));
    assert_eq!(
        take_ok(unsafe { mux_semaphore_try_acquire(semaphore) }),
        Value::Bool(true)
    );
    assert_eq!(
        take_ok(unsafe { mux_semaphore_try_acquire(semaphore) }),
        Value::Bool(false)
    );
    assert_eq!(
        take_ok(unsafe { mux_semaphore_release(semaphore) }),
        Value::Unit
    );
    assert_eq!(
        take_ok(unsafe { mux_semaphore_try_acquire(semaphore) }),
        Value::Bool(true)
    );
    assert_eq!(
        take_ok(unsafe { mux_semaphore_release(semaphore) }),
        Value::Unit
    );
    let overflow = unsafe { mux_semaphore_release(semaphore) };
    assert!(matches!(unsafe { &*overflow }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(semaphore));
    }
    unsafe {
        assert!(mux_rc_dec(overflow));
    }
}

#[test]
fn semaphore_timed_acquire_reports_timeout_and_success() {
    let semaphore = take_handle(mux_semaphore_with_permits(1));
    assert_eq!(
        take_ok(unsafe { mux_semaphore_acquire_timeout(semaphore, 0) }),
        Value::Bool(true)
    );
    assert_eq!(
        take_ok(unsafe { mux_semaphore_acquire_timeout(semaphore, 0) }),
        Value::Bool(false)
    );
    assert_eq!(
        take_ok(unsafe { mux_semaphore_release(semaphore) }),
        Value::Unit
    );
    unsafe {
        assert!(mux_rc_dec(semaphore));
    }
}

#[test]
fn constructors_reject_invalid_coordination_sizes() {
    let semaphore = mux_semaphore_with_permits(-1);
    let barrier = mux_barrier_with_size(0);
    let channel = mux_channel_new_bounded(i64::MAX);
    assert!(matches!(unsafe { &*semaphore }, Value::Result(Err(_))));
    assert!(matches!(unsafe { &*barrier }, Value::Result(Err(_))));
    assert!(matches!(unsafe { &*channel }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(semaphore));
        assert!(mux_rc_dec(barrier));
        assert!(mux_rc_dec(channel));
    }
}

#[test]
fn channel_send_timeout_rejects_deadline_overflow() {
    let channel = take_handle(mux_channel_new_bounded(1));
    let first = mux_rc_alloc(Value::Int(1));
    assert_eq!(
        take_ok(unsafe { mux_channel_send(channel, first) }),
        Value::Unit
    );

    let second = mux_rc_alloc(Value::Int(2));
    let result = unsafe { mux_channel_send_timeout(channel, second, i64::MAX) };
    assert_eq!(take_err(result), "channel send timeout is too large");

    take_ok(unsafe { mux_channel_close(channel) });
    unsafe {
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(second));
        assert!(mux_rc_dec(channel));
    }
}

#[test]
fn channel_receive_and_semaphore_timeouts_reject_unrepresentable_deadlines() {
    let channel = take_handle(mux_channel_new_bounded(1));
    let receive = unsafe { mux_channel_recv_timeout(channel, i64::MAX) };
    assert_eq!(take_err(receive), "channel receive timeout is too large");
    take_ok(unsafe { mux_channel_close(channel) });
    unsafe {
        assert!(mux_rc_dec(channel));
    }

    let semaphore = take_handle(mux_semaphore_with_permits(1));
    assert_eq!(
        take_ok(unsafe { mux_semaphore_acquire(semaphore) }),
        Value::Unit
    );
    let acquire = unsafe { mux_semaphore_acquire_timeout(semaphore, i64::MAX) };
    assert_eq!(take_err(acquire), "semaphore timeout is too large");
    take_ok(unsafe { mux_semaphore_release(semaphore) });
    unsafe {
        assert!(mux_rc_dec(semaphore));
    }
}

#[test]
fn worker_pool_constructors_and_submission_outcomes_are_typed() {
    let pool = mux_pool_new();
    assert!(!pool.is_null());

    let null_callback = unsafe { mux_pool_try_submit(pool, std::ptr::null_mut()) };
    assert_eq!(take_err(null_callback), "WorkerPool callback is null");

    assert_eq!(
        take_ok(unsafe { mux_pool_cancel_pending(pool) }),
        Value::Int(0)
    );
    assert_eq!(take_ok(unsafe { mux_pool_close(pool) }), Value::Unit);
    unsafe {
        assert!(mux_rc_dec(pool));
    }

    let configured = mux_pool_with_config(2, 3);
    let configured = take_handle(configured);
    assert_eq!(take_ok(unsafe { mux_pool_close(configured) }), Value::Unit);
    unsafe {
        assert!(mux_rc_dec(configured));
    }

    let invalid = mux_pool_with_config(0, 3);
    assert!(matches!(unsafe { &*invalid }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(invalid));
    }
}

#[test]
fn cancellation_tokens_are_shared_and_one_way() {
    let token = mux_cancellation_new();
    assert_eq!(
        take_ok(unsafe { mux_cancellation_is_cancelled(token) }),
        Value::Bool(false)
    );
    assert_eq!(
        take_ok(unsafe { mux_cancellation_cancel(token) }),
        Value::Unit
    );
    assert_eq!(
        take_ok(unsafe { mux_cancellation_is_cancelled(token) }),
        Value::Bool(true)
    );
    unsafe {
        assert!(mux_rc_dec(token));
    }
}

#[test]
fn channels_support_bounded_try_and_drain_after_close() {
    let channel = take_handle(mux_channel_new_bounded(1));
    let first = mux_rc_alloc(Value::Int(7));
    assert_eq!(
        take_ok(unsafe { mux_channel_try_send(channel, first) }),
        Value::Bool(true)
    );
    unsafe {
        assert!(mux_rc_dec(first));
    }

    let second = mux_rc_alloc(Value::Int(8));
    assert_eq!(
        take_ok(unsafe { mux_channel_try_send(channel, second) }),
        Value::Bool(false)
    );
    unsafe {
        assert!(mux_rc_dec(second));
    }

    assert_eq!(
        take_ok(unsafe { mux_channel_try_recv(channel) }),
        Value::Optional(Some(Box::new(Value::Int(7))))
    );
    assert_eq!(take_ok(unsafe { mux_channel_close(channel) }), Value::Unit);
    let closed_value = mux_rc_alloc(Value::Int(9));
    assert_eq!(
        take_err(unsafe { mux_channel_try_send(channel, closed_value) }),
        "channel is closed"
    );
    unsafe {
        assert!(mux_rc_dec(closed_value));
    }
    assert_eq!(
        take_ok(unsafe { mux_channel_try_recv(channel) }),
        Value::Optional(None)
    );
    unsafe {
        assert!(mux_rc_dec(channel));
    }
}

#[test]
fn channels_timeout_without_busy_waiting_forever() {
    let channel = take_handle(mux_channel_new_bounded(1));
    assert_eq!(
        take_ok(unsafe { mux_channel_recv_timeout(channel, 0) }),
        Value::Optional(None)
    );
    let first = mux_rc_alloc(Value::Int(1));
    assert_eq!(
        take_ok(unsafe { mux_channel_send_timeout(channel, first, 0) }),
        Value::Bool(true)
    );
    unsafe {
        assert!(mux_rc_dec(first));
    }
    let second = mux_rc_alloc(Value::Int(2));
    assert_eq!(
        take_ok(unsafe { mux_channel_send_timeout(channel, second, 0) }),
        Value::Bool(false)
    );
    unsafe {
        assert!(mux_rc_dec(second));
    }
    unsafe {
        assert!(mux_rc_dec(channel));
    }
}

#[test]
fn channel_select_is_ordered_and_timeout_bounded() {
    let first = mux_channel_new_unbounded();
    let second = mux_channel_new_unbounded();
    let payload = mux_rc_alloc(Value::Int(42));
    assert_eq!(
        take_ok(unsafe { mux_channel_send(second, payload) }),
        Value::Unit
    );
    unsafe {
        assert!(mux_rc_dec(payload));
    }
    let channels = mux_rc_alloc(Value::List(vec![unsafe { (&*first).clone() }, unsafe {
        (&*second).clone()
    }]));
    assert_eq!(
        take_ok(unsafe { mux_channel_select(channels, 100) }),
        Value::Optional(Some(Box::new(Value::Tuple(Box::new(mux_runtime::Tuple(
            Value::Int(1),
            Value::Int(42),
        ))))))
    );
    unsafe {
        assert!(mux_rc_dec(channels));
    }
    let timeout_channels = mux_rc_alloc(Value::List(vec![unsafe { (&*first).clone() }]));
    assert_eq!(
        take_ok(unsafe { mux_channel_select(timeout_channels, 0) }),
        Value::Optional(None)
    );
    unsafe {
        assert!(mux_rc_dec(timeout_channels));
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(second));
    }
}

#[test]
fn channels_honor_cooperative_cancellation() {
    let channel = take_handle(mux_channel_new_bounded(1));
    let token = mux_cancellation_new();
    let first = mux_rc_alloc(Value::Int(1));
    assert_eq!(
        take_ok(unsafe { mux_channel_send(channel, first) }),
        Value::Unit
    );
    unsafe {
        assert!(mux_rc_dec(first));
    }
    take_ok(unsafe { mux_cancellation_cancel(token) });
    let blocked = mux_rc_alloc(Value::Int(2));
    assert_eq!(
        take_err(unsafe { mux_channel_send_cancelled(channel, blocked, token) }),
        "channel send cancelled"
    );
    unsafe {
        assert!(mux_rc_dec(blocked));
    }
    assert_eq!(
        take_err(unsafe { mux_channel_recv_cancelled(channel, token) }),
        "channel receive cancelled"
    );
    unsafe {
        assert!(mux_rc_dec(channel));
        assert!(mux_rc_dec(token));
    }
}
