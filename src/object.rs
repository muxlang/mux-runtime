use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::{ObjectRef, TypeId, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::sync::{LazyLock, Mutex};

static TYPE_REGISTRY: LazyLock<Mutex<HashMap<TypeId, ObjectType>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_TYPE_ID: LazyLock<Mutex<TypeId>> = LazyLock::new(|| Mutex::new(1));

#[derive(Clone, Debug)]
pub struct ObjectType {
    pub id: TypeId,
    pub name: String,
    pub size: usize,
    pub destructor: Option<extern "C" fn(*mut c_void)>,
    /// Called by `copy_object` to perform a deep copy: `copy(src, dst)`.
    /// If None, `copy_object` returns null and the caller must handle the
    /// "type does not support copying" case.
    pub copy: Option<extern "C" fn(*mut c_void, *mut c_void)>,
    /// Equality of two instances. Registered for a class that implements
    /// `Equatable`, and it is the class's own `eq` method, so a map or set
    /// matches instances the way the `==` operator does.
    ///
    /// Unlike `destructor` and `copy`, which take the object's data buffer,
    /// this and the two below take the boxed object - the same `*mut Value` a
    /// class method receives as `self`.
    pub equals: Option<extern "C" fn(*mut Value, *mut Value) -> bool>,
    /// Three-way comparison of two instances, like `Ord::cmp`. Registered for a
    /// class that implements `Comparable`.
    pub compare: Option<extern "C" fn(*mut Value, *mut Value) -> i32>,
    /// Hash of one instance. Registered for a class that implements `Hashable`.
    /// Must agree with `equals`: two instances it calls equal have to hash the
    /// same, or a lookup misses.
    pub hash: Option<extern "C" fn(*mut Value) -> u64>,
}

impl ObjectType {
    pub fn new(
        name: String,
        size: usize,
        destructor: Option<extern "C" fn(*mut c_void)>,
        copy: Option<extern "C" fn(*mut c_void, *mut c_void)>,
    ) -> Self {
        let id = {
            let mut next_id = NEXT_TYPE_ID
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = *next_id;
            *next_id += 1;
            id
        };

        ObjectType {
            id,
            name,
            size,
            destructor,
            copy,
            equals: None,
            compare: None,
            hash: None,
        }
    }
}

pub fn register_object_type(
    name: &str,
    size: usize,
    destructor: Option<extern "C" fn(*mut c_void)>,
) -> TypeId {
    register_object_type_with_copy(name, size, destructor, None)
}

pub fn register_object_type_with_copy(
    name: &str,
    size: usize,
    destructor: Option<extern "C" fn(*mut c_void)>,
    copy: Option<extern "C" fn(*mut c_void, *mut c_void)>,
) -> TypeId {
    let obj_type = ObjectType::new(name.to_string(), size, destructor, copy);
    let id = obj_type.id;
    TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, obj_type);
    id
}

pub fn call_object_destructor(type_id: TypeId, ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let destructor = {
        let registry = TYPE_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .get(&type_id)
            .and_then(|obj_type| obj_type.destructor)
    };
    if let Some(func) = destructor {
        func(ptr);
    }
}

pub fn alloc_object(type_id: TypeId) -> *mut Value {
    let registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(obj_type) = registry.get(&type_id) else {
        return std::ptr::null_mut();
    };
    let size = obj_type.size;

    // Allocate memory for the object
    let Ok(layout) = std::alloc::Layout::from_size_align(size, std::mem::align_of::<u8>()) else {
        return std::ptr::null_mut();
    };
    let ptr = unsafe { std::alloc::alloc(layout) };

    if ptr.is_null() {
        return std::ptr::null_mut();
    }

    // Create ObjectRef with size for proper cleanup
    let obj_ref = ObjectRef::new(ptr as *mut c_void, type_id, size);

    // Create Value::Object
    let value = Value::Object(obj_ref);

    // Return ref-counted value
    mux_rc_alloc(value)
}

/// # Safety
/// The `obj` pointer must be valid and obtained from `alloc_object` or similar.
/// After calling this function if the ref count reaches 0, the pointer becomes invalid.
///
/// This function decrements the reference count of the Value. When the count
/// reaches 0, the Value is dropped, which drops the ObjectRef, which (via Rc)
/// drops the ObjectData, which frees the underlying object memory.
pub unsafe fn free_object(obj: *mut Value) {
    // Simply decrement the RC - cleanup is automatic via Drop
    unsafe { mux_rc_dec(obj) };
}

/// # Safety
/// The `obj` pointer must be valid and point to a `Value::Object`.
pub unsafe fn get_object_ptr(obj: *const Value) -> *mut c_void {
    if obj.is_null() {
        return std::ptr::null_mut();
    }

    let value = unsafe { &*obj };
    if let Value::Object(obj_ref) = value {
        obj_ref.ptr()
    } else {
        std::ptr::null_mut()
    }
}

/// # Safety
/// The `obj` pointer must be valid and point to a `Value::Object`.
pub unsafe fn get_object_type_id(obj: *const Value) -> TypeId {
    if obj.is_null() {
        return 0;
    }

    let value = unsafe { &*obj };
    if let Value::Object(obj_ref) = value {
        obj_ref.type_id()
    } else {
        0
    }
}

/// # Safety
/// The `src` pointer must be valid and point to a `Value::Object`.
/// Returns a new object that is a copy of the source, or null if the type
/// does not support copying (no copy callback registered).
pub unsafe fn copy_object(src: *const Value) -> *mut Value {
    if src.is_null() {
        return std::ptr::null_mut();
    }

    let type_id = unsafe { get_object_type_id(src) };
    let copy_fn = {
        let registry = TYPE_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(obj_type) = registry.get(&type_id) else {
            return std::ptr::null_mut();
        };
        obj_type.copy
    };
    let Some(copy_fn) = copy_fn else {
        return std::ptr::null_mut();
    };

    let dest = alloc_object(type_id);
    if dest.is_null() {
        return std::ptr::null_mut();
    }

    unsafe {
        let src_ptr = get_object_ptr(src);
        let dest_ptr = get_object_ptr(dest);
        copy_fn(src_ptr, dest_ptr);
    }

    dest
}

// C API functions
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_register_object_type(name: *const c_char, size: usize) -> TypeId {
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = c_str.to_string_lossy().into_owned();
    register_object_type(&name_str, size, None)
}

/// Register a deep-copy callback for an object type.
///
/// `copy_fn(src, dst)` must copy all heap-allocated fields from `src` into the
/// already-allocated `dst` buffer of the same size.  Without this, `mux_copy_object`
/// returns null and the caller must handle the "type does not support copying"
/// case.
#[unsafe(no_mangle)]
pub extern "C" fn mux_register_object_copy(
    type_id: TypeId,
    copy_fn: extern "C" fn(*mut c_void, *mut c_void),
) {
    let mut registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(obj_type) = registry.get_mut(&type_id) {
        obj_type.copy = Some(copy_fn);
    }
}

/// Register a destructor for an object type.
///
/// `destructor(ptr)` is called when the object's storage is being released
/// so the type can release any heap-allocated fields it owns.  It is invoked
/// from `Drop` for the underlying `ObjectData`.  Registering is idempotent;
/// the most recent registration wins.
#[unsafe(no_mangle)]
pub extern "C" fn mux_register_object_destructor(
    type_id: TypeId,
    destructor: extern "C" fn(*mut c_void),
) {
    let mut registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(obj_type) = registry.get_mut(&type_id) {
        obj_type.destructor = Some(destructor);
    }
}

/// Register an equality for an object type, so instances match by their
/// contents rather than by address.
///
/// `equals(a, b)` takes two boxed objects - the `*mut Value` a class method
/// receives as `self` - and is the class's own `eq`. Registering is idempotent;
/// the most recent registration wins.
#[unsafe(no_mangle)]
pub extern "C" fn mux_register_object_equals(
    type_id: TypeId,
    equals_fn: extern "C" fn(*mut Value, *mut Value) -> bool,
) {
    let mut registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(obj_type) = registry.get_mut(&type_id) {
        obj_type.equals = Some(equals_fn);
    }
}

/// Register a three-way comparison for an object type, so instances order by
/// their contents rather than by address.
///
/// `compare(a, b)` returns negative, zero or positive like `Ord::cmp`. Emitted
/// for a class that implements `Comparable`. Registering is idempotent; the
/// most recent registration wins.
#[unsafe(no_mangle)]
pub extern "C" fn mux_register_object_compare(
    type_id: TypeId,
    compare_fn: extern "C" fn(*mut Value, *mut Value) -> i32,
) {
    let mut registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(obj_type) = registry.get_mut(&type_id) {
        obj_type.compare = Some(compare_fn);
    }
}

/// Register a hash for an object type, so instances can key a map or join a
/// set by their contents.
///
/// `hash(ptr)` must agree with whatever `mux_register_object_compare` registered
/// for the same type: two instances that compare equal have to hash equally, or
/// a lookup misses. Registering is idempotent; the most recent wins.
#[unsafe(no_mangle)]
pub extern "C" fn mux_register_object_hash(
    type_id: TypeId,
    hash_fn: extern "C" fn(*mut Value) -> u64,
) {
    let mut registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(obj_type) = registry.get_mut(&type_id) {
        obj_type.hash = Some(hash_fn);
    }
}

/// What a class registered for matching, ordering and hashing its instances.
#[derive(Clone, Copy, Default)]
pub struct ObjectCallbacks {
    pub equals: Option<extern "C" fn(*mut Value, *mut Value) -> bool>,
    pub compare: Option<extern "C" fn(*mut Value, *mut Value) -> i32>,
    pub hash: Option<extern "C" fn(*mut Value) -> u64>,
    /// Whether the type can be copied, which is what lets a key be snapshotted
    /// independently of the caller's handle. Content-based keying is only sound
    /// for a type that has this.
    pub can_snapshot: bool,
}

/// The three callbacks registered for `type_id`, read under one lock.
///
/// Fetched together because every caller needs more than one of them, and the
/// registry is a single global mutex sitting on the hot path of a map probe.
/// The lock is released before returning, so the caller may then run compiled
/// code that registers or allocates another object type.
pub fn object_callbacks(type_id: TypeId) -> ObjectCallbacks {
    let registry = TYPE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match registry.get(&type_id) {
        Some(obj_type) => ObjectCallbacks {
            equals: obj_type.equals,
            compare: obj_type.compare,
            hash: obj_type.hash,
            can_snapshot: obj_type.copy.is_some(),
        },
        None => ObjectCallbacks::default(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_alloc_object(type_id: TypeId) -> *mut Value {
    alloc_object(type_id)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_free_object(obj: *mut Value) {
    unsafe { free_object(obj) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_get_object_ptr(obj: *const Value) -> *mut c_void {
    unsafe { get_object_ptr(obj) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_get_object_type_id(obj: *const Value) -> TypeId {
    unsafe { get_object_type_id(obj) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_copy_object(src: *const Value) -> *mut Value {
    unsafe { copy_object(src) }
}
