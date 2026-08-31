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
//! reference-counted cell (`mux_cell_alloc`) holding a `*mut Value`.
//!
//! A cell is the storage of the captured variable itself, shared rather than
//! copied: the variable and every closure capturing it name the same cell, so a
//! write through one is visible to the others. The closure holds a reference to
//! each cell and drops it on release; the cell frees itself and the value it
//! holds when the last holder goes away.
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

/// The refcount header of a capture cell, one i64 before the cell pointer -
/// the same convention the closure struct uses.
#[inline]
unsafe fn cell_header(cell: *mut c_void) -> *const AtomicI64 {
    unsafe { (cell as *const AtomicI64).sub(1) }
}

/// Allocate a capture cell holding `initial`, with a reference count of 1.
///
/// A cell is the storage of a variable that some closure captures. It is
/// shared, not copied: the variable and every closure capturing it name the
/// same cell, which is what makes a write through one visible to the others.
/// That is why it is reference counted rather than owned outright by a single
/// closure - a variable may be captured by two closures, and it outlives them
/// when it is an ordinary local, while a returned closure outlives the function
/// that declared it.
///
/// Layout mirrors the closure's: one allocation, refcount first, and the
/// pointer handed back points at the payload, so reading a cell is still just
/// dereferencing it as a `*mut Value`.
///
/// ```text
/// [ i64 refcount | *mut Value ]
///   ^base          ^--- cell pointer returned to codegen
/// ```
#[unsafe(no_mangle)]
/// Allocates a capture cell and transfers one value reference into it.
///
/// # Safety
///
/// `initial` must be null or a live pointer returned by `mux_rc_alloc` (or
/// another runtime function that returns an owned reference). For a non-null
/// pointer, ownership of that reference transfers to the cell; the caller
/// must not release or otherwise use that reference after this call. The cell
/// releases it when its final holder calls [`mux_cell_release`].
pub unsafe extern "C" fn mux_cell_alloc(initial: *mut Value) -> *mut c_void {
    unsafe {
        let base = libc::malloc(std::mem::size_of::<i64>() + std::mem::size_of::<*mut Value>());
        if base.is_null() {
            return std::ptr::null_mut();
        }
        *base.cast::<i64>() = 1;
        let cell = base.cast::<i64>().add(1).cast::<c_void>();
        *cell.cast::<*mut Value>() = initial;
        cell
    }
}

/// Increment a capture cell's reference count, for each additional holder: a
/// closure capturing a variable that already has one.
///
/// # Safety
/// `cell` must be null or a live cell returned by [`mux_cell_alloc`]. The
/// caller must hold a live ownership reference while retaining it.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cell_retain(cell: *mut c_void) {
    if cell.is_null() {
        return;
    }
    unsafe {
        (*cell_header(cell)).fetch_add(1, Ordering::Relaxed);
    }
}

/// Decrement a capture cell's reference count, releasing the value it holds and
/// freeing the cell when the last holder drops it. Null-safe.
///
/// # Safety
/// `cell` must be null or a live cell returned by [`mux_cell_alloc`]. The
/// caller must release exactly one ownership reference and must not call this
/// function again after the final reference is released.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cell_release(cell: *mut c_void) {
    if cell.is_null() {
        return;
    }
    unsafe {
        if (*cell_header(cell)).fetch_sub(1, Ordering::AcqRel) > 1 {
            return;
        }
        crate::refcount::mux_rc_dec(*(cell as *const *mut Value));
        libc::free(cell.cast::<i64>().sub(1).cast::<c_void>());
    }
}

/// Increment a closure's reference count. Used when ownership is shared (e.g. a
/// closure returned to a caller that must outlive the producing scope, or handed
/// to a spawned thread).
///
/// # Safety
/// `closure` must be null or a live closure allocation produced by the
/// compiler. The caller must hold a live ownership reference while retaining it.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_closure_retain(closure: *mut c_void) {
    if closure.is_null() {
        return;
    }
    unsafe {
        (*header(closure)).fetch_add(1, Ordering::Relaxed);
    }
}

/// Decrement a closure's reference count, freeing the closure, its capture
/// array, every heap-storage cell, and one reference to every captured value
/// when the count reaches zero. Null-safe.
///
/// # Safety
/// `closure` must be null or a live closure allocation produced by the
/// compiler. The caller must release exactly one ownership reference and must
/// not call this function again after the final reference is released.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_closure_release(closure: *mut c_void) {
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
                // A cell is shared with the variable it is the storage of, and
                // with any other closure capturing that variable, so the
                // closure drops its reference rather than freeing the cell.
                mux_cell_release(*slots.add(i));
            }
            libc::free(captures_ptr);
        }

        // The refcount header is the base of the single closure allocation.
        libc::free(header(closure) as *mut c_void);
    }
}
