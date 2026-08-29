extern crate std as rust_std;

use crate::ordered::{OrderedMap, OrderedSet};
use rust_std::cmp;
use rust_std::ffi::c_void;
use rust_std::fmt;
use rust_std::hash;
use rust_std::mem;
use rust_std::rc::Rc;
use rust_std::sync::atomic::{AtomicUsize, Ordering};

pub type TypeId = u32;

struct ObjectData {
    ptr: *mut c_void,
    type_id: TypeId,
    size: usize,
    ref_count: AtomicUsize,
}

impl Drop for ObjectData {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.size > 0 {
            crate::object::call_object_destructor(self.type_id, self.ptr);
            if let Ok(layout) =
                ::std::alloc::Layout::from_size_align(self.size, ::std::mem::align_of::<u8>())
            {
                unsafe {
                    ::std::alloc::dealloc(self.ptr as *mut u8, layout);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ObjectRef {
    data: Rc<ObjectData>,
}

impl ::std::fmt::Debug for ObjectData {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        f.debug_struct("ObjectData")
            .field("ptr", &self.ptr)
            .field("type_id", &self.type_id)
            .field("size", &self.size)
            .field("ref_count", &self.ref_count.load(Ordering::Relaxed))
            .finish()
    }
}

impl ObjectRef {
    pub fn new(ptr: *mut c_void, type_id: TypeId, size: usize) -> Self {
        ObjectRef {
            data: Rc::new(ObjectData {
                ptr,
                type_id,
                size,
                ref_count: AtomicUsize::new(1),
            }),
        }
    }

    pub fn ptr(&self) -> *mut c_void {
        self.data.ptr
    }

    pub fn type_id(&self) -> TypeId {
        self.data.type_id
    }

    pub fn inc_ref(&self) {
        self.data.ref_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_ref(&self) -> usize {
        self.data.ref_count.fetch_sub(1, Ordering::Relaxed)
    }

    /// Box this object the way compiled code expects to receive `self`: a
    /// reference-counted `Value::Object` sharing this object's data.
    ///
    /// The registered callbacks are the class's own methods, which take `self`
    /// as a `*mut Value` and are free to retain it, so they cannot be handed a
    /// borrowed `&Value` that may live in a collection rather than in a
    /// reference-counted block. The caller releases the box with `mux_rc_dec`.
    fn boxed_for_callback(&self) -> *mut Value {
        refcount::mux_rc_alloc(Value::Object(self.clone()))
    }

    /// The callbacks this object is actually keyed by.
    ///
    /// Content-based keying needs two things beyond a hash, and a type missing
    /// either is keyed by identity instead - the same answer it had before it
    /// registered anything.
    ///
    /// It must be **copyable**. A key is stored at a position derived from its
    /// contents, and the only way to take a copy independent of the caller's
    /// handle is the registered copy callback. Without one the caller keeps a
    /// handle to the very object the table is keyed on, and mutating it moves
    /// where the key belongs without moving the entry.
    ///
    /// It must supply an **equality** (`equals`, or `compare` which answers
    /// equality too). A hash alone cannot key anything: two entries landing in
    /// one bucket need something to tell them apart, and without it equality
    /// stays pointer identity while the key is content-derived - so two
    /// distinct objects whose hashes collide would order equal while comparing
    /// unequal. Identity keys both consistently.
    ///
    /// The compiler registers copy for every class and requires `eq` of every
    /// `Hashable` one, so neither gap is reachable from Mux source; both are
    /// combinations only a direct FFI caller can build.
    fn keying(&self) -> object::ObjectCallbacks {
        let callbacks = object::object_callbacks(self.type_id());
        let has_equality = callbacks.equals.is_some() || callbacks.compare.is_some();
        if callbacks.can_snapshot && has_equality {
            callbacks
        } else {
            object::ObjectCallbacks::default()
        }
    }

    /// The value this object is keyed by, for both hashing and - absent a
    /// registered comparison - ordering. Using one key for both is what keeps
    /// them agreeing.
    ///
    /// A class that hashes itself is keyed by that hash. One that only compares
    /// itself is keyed by a constant: any two of its instances may turn out to
    /// be equal, so they all have to hash alike, and colliding is the only
    /// answer that cannot be wrong. Everything else is keyed by its address,
    /// which is exactly as precise as the identity equality it pairs with.
    fn structural_key(&self) -> u64 {
        let callbacks = self.keying();
        if let Some(hash) = callbacks.hash {
            return self.call_with_boxed(hash);
        }
        // Registered equality without a hash: any two instances may be equal,
        // so they all have to collide.
        if callbacks.equals.is_some() || callbacks.compare.is_some() {
            return 0;
        }
        self.ptr() as u64
    }

    /// Whether this object equals another of the same type by its contents,
    /// using the class's own `eq` (or `cmp`, which also answers equality).
    /// None when the two are different types, or when the class declared
    /// neither - the caller then falls back to identity.
    fn structural_eq(&self, other: &ObjectRef) -> Option<bool> {
        if self.type_id() != other.type_id() {
            return None;
        }
        let callbacks = self.keying();
        if let Some(equals) = callbacks.equals {
            return Some(self.call_with_boxed_pair(other, equals));
        }
        let compare = callbacks.compare?;
        Some(self.call_with_boxed_pair(other, compare) == 0)
    }

    /// Order this object against another of the same type by its contents,
    /// using the comparison the class registered for `Comparable`. None when
    /// the two are different types, or when the class declared no order.
    fn structural_cmp(&self, other: &ObjectRef) -> Option<cmp::Ordering> {
        if self.type_id() != other.type_id() {
            return None;
        }
        let compare = self.keying().compare?;
        Some(self.call_with_boxed_pair(other, compare).cmp(&0))
    }

    /// Run a one-argument callback on this object, boxed and then released.
    fn call_with_boxed<R>(&self, callback: extern "C" fn(*mut Value) -> R) -> R {
        let boxed = self.boxed_for_callback();
        let result = callback(boxed);
        unsafe { refcount::mux_rc_dec(boxed) };
        result
    }

    /// Run a two-argument callback on this object and another, each boxed and
    /// then released.
    fn call_with_boxed_pair<R>(
        &self,
        other: &ObjectRef,
        callback: extern "C" fn(*mut Value, *mut Value) -> R,
    ) -> R {
        let (a, b) = (self.boxed_for_callback(), other.boxed_for_callback());
        let result = callback(a, b);
        unsafe { refcount::mux_rc_dec(b) };
        unsafe { refcount::mux_rc_dec(a) };
        result
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tuple(pub Value, pub Value);

impl fmt::Display for Tuple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.0, self.1)
    }
}

/// Glue applied in place to the inline bytes of a boxed enum, deep-cloning
/// (`clone_glue`) or releasing (`drop_glue`) the active variant's
/// reference-counted payloads. Emitted by the compiler per enum (issue #309).
pub type EnumGlueFn = extern "C" fn(*mut u8);

/// Structural three-way comparison of two boxed enums of the same enum type,
/// returning a negative, zero, or positive `i32` like `Ord::cmp`. Emitted by the
/// compiler per enum: it compares discriminants, then each payload field by
/// value (recursing into nested enums and delegating pointer payloads to
/// `mux_value_compare`), so a payload-carrying enum orders and de-duplicates
/// correctly as a map key or set member (issue #309).
pub type EnumCmpFn = extern "C" fn(*mut u8, *mut u8) -> i32;

/// Structural hash of one boxed enum, emitted by the compiler per enum. It
/// combines the discriminant with the active variant's payload fields, so two
/// enums that `EnumCmpFn` reports as equal always hash the same - the contract
/// every hash table depends on.
///
/// Hashing the raw bytes instead would be wrong: the inline struct has padding
/// between the discriminant and the payload, and between fields, and those
/// bytes are not guaranteed equal for two otherwise equal values.
pub type EnumHashFn = extern "C" fn(*mut u8) -> u64;

/// An enum that carries reference-counted payloads, boxed as a first-class
/// Value with value semantics. Unlike a raw `Opaque` (whose bytes may hold
/// payload pointers the runtime cannot see), a `BoxedEnum` runs the compiler's
/// glue on `Clone` and `Drop`, so a payload-carrying enum copies and frees
/// correctly wherever the runtime manages it - crucially inside collections,
/// whose insert/read helpers `clone()` their elements (issue #309).
///
/// The inline struct bytes are backed by a `Box<[u64]>` so the pointer handed to
/// the compiler's typed glue and to unboxing is 8-byte aligned; an enum's
/// payloads (i32 discriminant, i64/pointer/f64 fields) need at most 8-byte
/// alignment, so a `u64` backing store satisfies every layout.
///
/// Equality, ordering, and hashing are structural, delegating to the compiler's
/// per-enum `cmp_glue`: it compares discriminants and payloads by value, so two
/// logically equal clones (whose payload pointers differ) compare equal and a
/// payload-carrying enum works as a map key or set member. Hashing goes through
/// `hash_glue` for the same reason comparison goes through `cmp_glue`: map and
/// set are hash tables, so hashing the discriminant alone would put every
/// `Code(1)`, `Code(2)`, `Code(3)` in one bucket and make lookup linear.
pub struct BoxedEnum {
    words: Box<[u64]>,
    len: usize,
    clone_glue: EnumGlueFn,
    drop_glue: EnumGlueFn,
    cmp_glue: EnumCmpFn,
    hash_glue: EnumHashFn,
}

impl BoxedEnum {
    /// Box `src` bytes into an 8-aligned backing store. Does not run the clone
    /// glue; callers that need an independent copy of the payloads (the boxing
    /// ABI) run `clone_glue` on `as_mut_ptr()` afterward.
    pub fn from_bytes(
        src: &[u8],
        clone_glue: EnumGlueFn,
        drop_glue: EnumGlueFn,
        cmp_glue: EnumCmpFn,
        hash_glue: EnumHashFn,
    ) -> Self {
        let mut words = vec![0u64; src.len().div_ceil(8).max(1)].into_boxed_slice();
        // SAFETY: `words` provides at least `src.len()` bytes of 8-aligned,
        // writable storage, and `src` is a distinct readable slice.
        unsafe {
            rust_std::ptr::copy_nonoverlapping(
                src.as_ptr(),
                words.as_mut_ptr() as *mut u8,
                src.len(),
            );
        }
        BoxedEnum {
            words,
            len: src.len(),
            clone_glue,
            drop_glue,
            cmp_glue,
            hash_glue,
        }
    }

    /// Read-only view of the inline enum struct bytes.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `len` bytes were initialized from the source in `from_bytes`.
        unsafe { rust_std::slice::from_raw_parts(self.words.as_ptr() as *const u8, self.len) }
    }

    /// 8-aligned pointer to the inline enum struct, for the compiler's glue.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.words.as_mut_ptr() as *mut u8
    }

    /// 8-aligned read pointer to the inline enum struct, for unboxing.
    pub fn as_ptr(&self) -> *const u8 {
        self.words.as_ptr() as *const u8
    }

    /// Structural hash, through the compiler-emitted glue, so two enums that
    /// compare equal hash equally.
    fn hash_value(&self) -> u64 {
        // The glue only reads its operand; the *mut is C-ABI convention.
        (self.hash_glue)(self.as_ptr() as *mut u8)
    }

    /// Structural ordering. Two enums of the same type share a `cmp_glue`, so it
    /// does the comparison; enums of different types (never mixed in a
    /// well-typed collection) fall back to a stable order on the glue address.
    fn compare(&self, other: &BoxedEnum) -> cmp::Ordering {
        if (self.cmp_glue as usize) == (other.cmp_glue as usize) {
            // The glue only reads its operands; the *mut is C-ABI convention.
            let ordering = (self.cmp_glue)(self.as_ptr() as *mut u8, other.as_ptr() as *mut u8);
            ordering.cmp(&0)
        } else {
            (self.cmp_glue as usize).cmp(&(other.cmp_glue as usize))
        }
    }
}

impl Clone for BoxedEnum {
    fn clone(&self) -> Self {
        // Copy the inline struct, then deep-clone its payloads in place so the
        // clone shares nothing with the source.
        let mut cloned = BoxedEnum {
            words: self.words.clone(),
            len: self.len,
            clone_glue: self.clone_glue,
            drop_glue: self.drop_glue,
            cmp_glue: self.cmp_glue,
            hash_glue: self.hash_glue,
        };
        (cloned.clone_glue)(cloned.as_mut_ptr());
        cloned
    }
}

impl Drop for BoxedEnum {
    fn drop(&mut self) {
        let drop_glue = self.drop_glue;
        drop_glue(self.as_mut_ptr());
    }
}

impl fmt::Debug for BoxedEnum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BoxedEnum({} bytes)", self.len)
    }
}

#[derive(Clone, Debug)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(ordered_float::OrderedFloat<f64>),
    String(String),
    List(Vec<Value>),
    Map(OrderedMap<Value, Value>),
    Set(OrderedSet<Value>),
    Tuple(Box<Tuple>),
    Optional(Option<Box<Value>>),
    Result(Result<Box<Value>, Box<Value>>),
    Object(ObjectRef),
    Opaque(Box<[u8]>),
    BoxedEnum(BoxedEnum),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Set(a), Value::Set(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Optional(a), Value::Optional(b)) => a == b,
            (Value::Result(a), Value::Result(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => match a.structural_eq(b) {
                Some(equal) => equal,
                // A class that declares no `Equatable`/`Comparable` is still
                // equal to itself, by identity.
                None => a.ptr() == b.ptr() && a.type_id() == b.type_id(),
            },
            (Value::Opaque(a), Value::Opaque(b)) => a == b,
            (Value::BoxedEnum(a), Value::BoxedEnum(b)) => a.compare(b) == cmp::Ordering::Equal,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl hash::Hash for Value {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        mem::discriminant(self).hash(state);
        match self {
            Value::Unit => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => i.hash(state),
            Value::Float(f) => f.hash(state),
            Value::String(s) => s.hash(state),
            Value::List(l) => l.hash(state),
            // Delegated, not hand-rolled: these combine entry hashes
            // commutatively, matching an equality that ignores insertion order.
            // Hashing in iteration order gave two maps that compare equal
            // different hashes, so a map used as a key could not be found.
            Value::Map(m) => m.hash(state),
            Value::Set(s) => s.hash(state),
            Value::Tuple(t) => t.hash(state),
            Value::Optional(o) => o.hash(state),
            Value::Result(r) => r.hash(state),
            Value::Object(obj) => {
                obj.type_id().hash(state);
                obj.structural_key().hash(state);
            }
            Value::Opaque(bytes) => bytes.hash(state),
            Value::BoxedEnum(be) => be.hash_value().hash(state),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Value {
    pub fn type_tag(&self) -> i32 {
        // A BoxedEnum shares the Opaque tag (12): both wrap inline enum bytes and
        // are indistinguishable to the language's type reflection.
        const TAG_BY_ORDER: [i32; 14] = [11, 0, 1, 2, 3, 4, 5, 6, 10, 7, 8, 9, 12, 12];
        TAG_BY_ORDER[self.variant_order() as usize]
    }

    fn variant_order(&self) -> u8 {
        match self {
            Value::Unit => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::String(_) => 4,
            Value::List(_) => 5,
            Value::Map(_) => 6,
            Value::Set(_) => 7,
            Value::Tuple(_) => 8,
            Value::Optional(_) => 9,
            Value::Result(_) => 10,
            Value::Object(_) => 11,
            Value::Opaque(_) => 12,
            Value::BoxedEnum(_) => 13,
        }
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        match (self, other) {
            (Value::Unit, Value::Unit) => cmp::Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(cmp::Ordering::Equal),
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::List(a), Value::List(b)) => a.cmp(b),
            (Value::Map(a), Value::Map(b)) => a.cmp(b),
            (Value::Set(a), Value::Set(b)) => a.cmp(b),
            (Value::Tuple(a), Value::Tuple(b)) => a.cmp(b),
            (Value::Optional(a), Value::Optional(b)) => a.cmp(b),
            (Value::Result(a), Value::Result(b)) => a.cmp(b),
            (Value::Object(a), Value::Object(b)) => match a.structural_cmp(b) {
                Some(ordering) => ordering,
                // A class that declares no order has none to give, and the
                // language never orders one either - `<` requires `Comparable`.
                // Ordering by the same key that hashes it keeps equal instances
                // ordering equal without inventing an order between unequal
                // ones: mixing content equality with address ordering was not
                // transitive, and `sort_by` may panic on a comparison that is
                // not a total order.
                //
                // One disagreement with `==` survives here and cannot be
                // removed: a class that declares equality but no order shares
                // one key across instances its `eq` separates, so two unequal
                // instances order equal. An arbitrary equivalence relation
                // admits no consistent total order without a comparator, and
                // every alternative either reintroduces the non-transitivity
                // above or invents an order the class never defined. Declaring
                // `Comparable` is how a class supplies the real one, and that
                // takes the branch above.
                None => (a.type_id(), a.structural_key()).cmp(&(b.type_id(), b.structural_key())),
            },
            (Value::Opaque(a), Value::Opaque(b)) => a.cmp(b),
            (Value::BoxedEnum(a), Value::BoxedEnum(b)) => a.compare(b),
            _ => self.variant_order().cmp(&other.variant_order()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_delimited<T>(
            f: &mut fmt::Formatter<'_>,
            open: &str,
            close: &str,
            items: impl IntoIterator<Item = T>,
            mut write_item: impl FnMut(&mut fmt::Formatter<'_>, T) -> fmt::Result,
        ) -> fmt::Result {
            write!(f, "{}", open)?;
            for (i, item) in items.into_iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write_item(f, item)?;
            }
            write!(f, "{}", close)
        }

        match self {
            Value::Unit => write!(f, "()"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(fl) => write!(f, "{}", crate::float::format_float(fl.into_inner())),
            Value::String(s) => write!(f, "{}", s),
            Value::List(list) => {
                write_delimited(f, "[", "]", list.iter(), |f, item| write!(f, "{}", item))
            }
            Value::Map(map) => write_delimited(f, "{", "}", map.iter(), |f, (key, val)| {
                write!(f, "{}: {}", key, val)
            }),
            Value::Set(set) => {
                write_delimited(f, "{", "}", set.iter(), |f, item| write!(f, "{}", item))
            }
            Value::Tuple(tuple) => write!(f, "{}", tuple),
            Value::Optional(opt) => match opt {
                Some(val) => write!(f, "Some({})", val),
                None => write!(f, "None"),
            },
            Value::Result(res) => match res {
                Ok(val) => write!(f, "Ok({})", val),
                Err(val) => write!(f, "Err({})", val),
            },
            Value::Object(obj) => {
                write!(f, "<Object at {:p} type_id={}>", obj.ptr(), obj.type_id())
            }
            Value::Opaque(bytes) => {
                write!(f, "<Opaque {} bytes>", bytes.len())
            }
            Value::BoxedEnum(be) => {
                write!(f, "<enum {} bytes>", be.len)
            }
        }
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

pub mod assert;
pub mod bool;
pub mod boxing;
pub mod closure;
#[cfg(feature = "csv")]
pub mod data;
pub mod datetime;
pub mod float;
pub mod int;
pub mod io;
#[cfg(feature = "json")]
pub mod json;
pub mod list;
pub mod map;
pub mod math;
#[cfg(feature = "net")]
pub mod net;
pub mod object;
pub mod optional;
pub mod ordered;
pub mod panic;
pub mod random;
pub mod refcount;
pub mod result;
pub mod set;
#[cfg(feature = "sql")]
pub mod sql;
pub mod std;
pub mod string;
#[cfg(feature = "sync")]
pub mod sync;
pub mod tuple;

pub use std::{mux_value_list_get_value, mux_value_list_length, mux_value_list_slice};

#[unsafe(no_mangle)]
pub extern "C" fn mux_float_value(f: f64) -> *mut Value {
    refcount::mux_rc_alloc(Value::Float(ordered_float::OrderedFloat(f)))
}
