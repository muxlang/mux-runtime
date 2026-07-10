//! Reference counting and teardown for compiler-produced closures.
//!
//! A closure is C-`malloc`'d by codegen with this layout (see
//! `allocate_closure` / `create_closure_with_captures` in the compiler):
//!
//! ```text
//! full allocation (one malloc):
//!   [ i64 refcount | fn_ptr : *fn | captures_ptr : *cap | i64 capture_count ]
//!     ^header                ^--- closure struct returned to codegen ---^
//! ```
//!
//! The pointer that flows through generated code (`closure`) points at the
//! closure struct, i.e. 8 bytes past the allocation base, so the refcount
//! header sits at `closure - 8`.
//!
//! `captures_ptr` is null for a capture-free closure. Otherwise it is a
//! C-`malloc`'d array of `capture_count` pointers; each element points at a
//! C-`malloc`'d one-pointer cell ("heap storage") holding an owned (`+1`)
//! `*mut Value` capture. The closure owns one reference to each captured value.
//!
//! These functions manage that ownership so closures - and everything they
//! capture - are released exactly once when the last reference is dropped.

use crate::Value;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicI64, Ordering};

/// Offset (in pointer-sized words) from the closure struct pointer to each
/// field, and to the refcount header that precedes it.
const CAPTURES_FIELD_WORD: usize = 1; // closure + 8  -> captures_ptr
const CAPTURE_COUNT_FIELD_WORD: usize = 2; // closure + 16 -> capture_count

/// The refcount header lives 8 bytes (one i64) before the closure struct.
/// Codegen initializes it to 1 with a plain store before the closure is shared,
/// so treating it as an `AtomicI64` here is sound and makes retain/release safe
/// across threads (a spawned closure is retained on one thread and released on
/// another).
#[inline]
unsafe fn header(closure: *mut c_void) -> *const AtomicI64 {
    unsafe { (closure as *const AtomicI64).sub(1) }
}

/// Increment a closure's reference count. Used when ownership is shared (e.g. a
/// closure returned to a caller that must outlive the producing scope, or handed
/// to a spawned thread).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_closure_retain(closure: *mut c_void) {
    if closure.is_null() {
        return;
    }
    unsafe {
        (*header(closure)).fetch_add(1, Ordering::Relaxed);
    }
}

/// Decrement a closure's reference count, freeing the closure, its capture
/// array, every heap-storage cell, and one reference to every captured value
/// when the count reaches zero. Null-safe and idempotent against a zero count.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_closure_release(closure: *mut c_void) {
    if closure.is_null() {
        return;
    }
    unsafe {
        // Acquire/Release so the thread that frees the closure sees all writes
        // (captured values) made through other references.
        if (*header(closure)).fetch_sub(1, Ordering::AcqRel) > 1 {
            return;
        }

        // Last reference: tear the closure down.
        let captures_ptr = *(closure as *const *mut c_void).add(CAPTURES_FIELD_WORD);
        let capture_count = *(closure as *const i64).add(CAPTURE_COUNT_FIELD_WORD);

        if !captures_ptr.is_null() && capture_count > 0 {
            let slots = captures_ptr as *const *mut c_void;
            for i in 0..capture_count as usize {
                let heap_storage = *slots.add(i);
                if !heap_storage.is_null() {
                    // Each heap-storage cell holds one owned reference to a Value.
                    let captured = *(heap_storage as *const *mut Value);
                    crate::refcount::mux_rc_dec(captured);
                    libc::free(heap_storage);
                }
            }
            libc::free(captures_ptr);
        }

        // The refcount header is the base of the single closure allocation.
        libc::free(header(closure) as *mut c_void);
    }
}
