#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::{Cell, RefCell};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_alloc;
use crate::refcount::mux_rc_dec;
use crate::{TypeId, Value};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::LazyLock;

const RAND_MAX: i64 = 2_147_483_647;
const MAX_RANDOM_BYTES: i64 = 16 * 1024 * 1024;

struct RandomEntry {
    state: u64,
    refs: usize,
}

static RANDOMS: LazyLock<Mutex<HashMap<i64, RandomEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_RANDOM_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static RANDOM_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Random",
        size_of::<i64>(),
        Some(drop_random as extern "C" fn(*mut c_void)),
        Some(copy_random as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

/// Width of `mux_rand_int`'s output, in bits. `RAND_MAX` is `2^31 - 1`, so the
/// generator fills 31 bits. `mux_rand_range` scales by this; deriving it from
/// `RAND_MAX` keeps the two from drifting apart if the generator ever widens.
const RAND_BITS: u32 = RAND_MAX.count_ones();

thread_local! {
    static MODULE_INITIALIZED: Cell<bool> = const { Cell::new(false) };
    static MODULE_STATE: RefCell<u64> = const { RefCell::new(0) };
}

fn lcg_next(state: u64) -> u64 {
    state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

// SplitMix64 is deliberately specified here instead of delegating to a
// platform RNG: seeded Random values must produce the same sequence on every
// supported target and in every release.
fn splitmix_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn next_random_handle() -> i64 {
    let mut next = NEXT_RANDOM_HANDLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let handle = *next;
    *next = next.checked_add(1).unwrap_or(1);
    handle
}

fn random_object(handle: i64) -> *mut Value {
    let value = alloc_object(*RANDOM_TYPE_ID);
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

fn make_random(seed: u64) -> *mut Value {
    let handle = next_random_handle();
    RANDOMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            RandomEntry {
                state: seed,
                refs: 1,
            },
        );
    let value = random_object(handle);
    if value.is_null() {
        RANDOMS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
    value
}

unsafe fn random_handle(value: *const Value) -> Option<i64> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *RANDOM_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return None;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    RANDOMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&handle)
        .then_some(handle)
}

fn release_random(handle: i64) {
    let mut entries = RANDOMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = entries.get_mut(&handle).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    });
    if remove {
        entries.remove(&handle);
    }
}

extern "C" fn copy_random(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    let mut entries = RANDOMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = entries.get_mut(&handle) {
        entry.refs += 1;
        unsafe { *dest.cast::<i64>() = handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_random(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_random(unsafe { *ptr.cast::<i64>() });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_seeded(seed: i64) -> *mut Value {
    make_random(seed.cast_unsigned())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_system() -> *mut Value {
    let mut seed = [0u8; 8];
    if getrandom::fill(&mut seed).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        seed = ((nanos as u64) ^ (std::process::id() as u64).rotate_left(17)).to_le_bytes();
    }
    make_random(u64::from_le_bytes(seed))
}

fn random_next(handle: i64) -> u64 {
    let mut entries = RANDOMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries
        .get_mut(&handle)
        .map_or(0, |entry| splitmix_next(&mut entry.state))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_next_int(random: *const Value) -> i64 {
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return 0;
    };
    random_next(handle).cast_signed()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_next_range(random: *const Value, min: i64, max: i64) -> i64 {
    if min >= max {
        return min;
    }
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return min;
    };
    let span = (i128::from(max) - i128::from(min)) as u128;
    // Draw from a precisely known 2^64-sized domain. Using `u128::MAX` here
    // would make the rejection bound unrelated to the generator's output and
    // reintroduce modulo bias for most spans.
    let domain = 1u128 << 64;
    let limit = domain - (domain % span);
    let value = loop {
        let candidate = u128::from(random_next(handle));
        if candidate < limit {
            break candidate % span;
        }
    };
    (i128::from(min) + i128::try_from(value).unwrap_or(0)) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_next_float(random: *const Value) -> f64 {
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return 0.0;
    };
    (random_next(handle) >> 11) as f64 / ((1u64 << 53) as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_next_bool(random: *const Value) -> bool {
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return false;
    };
    random_next(handle) & 1 == 0
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_bytes(random: *const Value, length: i64) -> *mut Value {
    if !(0..=MAX_RANDOM_BYTES).contains(&length) {
        return crate::std::random_result_err("length must be between 0 and 16 MiB".to_string());
    }
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return crate::std::random_result_err("invalid Random value".to_string());
    };
    let length = usize::try_from(length).unwrap_or(0);
    let mut bytes = Vec::with_capacity(length);
    while bytes.len() < length {
        bytes.extend_from_slice(&random_next(handle).to_le_bytes());
    }
    bytes.truncate(length);
    mux_rc_alloc(Value::Result(Ok(Box::new(Value::Bytes(bytes)))))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_init(seed: i64) {
    MODULE_STATE.with(|state| *state.borrow_mut() = seed.cast_unsigned());
    MODULE_INITIALIZED.with(|initialized| initialized.set(true));
}

fn ensure_initialized() {
    MODULE_INITIALIZED.with(|initialized| {
        if initialized.get() {
            return;
        }
        let mut seed_bytes = [0u8; 8];
        let seed = if getrandom::fill(&mut seed_bytes).is_ok() {
            u64::from_le_bytes(seed_bytes)
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        };
        MODULE_STATE.with(|state| *state.borrow_mut() = seed);
        initialized.set(true);
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_int() -> i64 {
    ensure_initialized();
    MODULE_STATE.with(|state| {
        let mut state = state.borrow_mut();
        *state = lcg_next(*state);
        ((*state >> 33).cast_signed()) & RAND_MAX
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_range(min: i64, max: i64) -> i64 {
    if min >= max {
        return min;
    }
    // Widen before subtracting: the valid i64 domain is wider than the
    // positive half of i64, so subtracting in i64 would overflow for ranges
    // that cross the sign boundary (including the full integer domain).
    let Ok(range_size) = u128::try_from(i128::from(max) - i128::from(min)) else {
        return min;
    };
    // Fixed-point multiply: scale a random fraction by the range and take the
    // integer part. The shift must match the generator's WIDTH, not a machine
    // word - `mux_rand_int` masks with RAND_MAX and so yields 31 bits, and
    // shifting by 32 capped every result at half the requested range.
    let scaled = (u128::from(mux_rand_int().cast_unsigned()) * range_size) >> RAND_BITS;
    // `scaled < range_size <= 2^64 - 1`, so this addition is in the i64
    // interval by construction. Keep the conversion checked at the ABI
    // boundary rather than relying on a potentially wrapping cast.
    let Ok(scaled) = i128::try_from(scaled) else {
        return min;
    };
    match i64::try_from(i128::from(min) + scaled) {
        Ok(value) => value,
        Err(_) => min,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_float() -> f64 {
    let r = mux_rand_int() as f64;
    r / ((RAND_MAX as f64) + 1.0)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_bool() -> bool {
    mux_rand_int() % 2 == 0
}

/// Generate a deterministic sequence of pseudo-random bytes from the current
/// generator. The result is wrapped in the standard Mux result shape so an
/// invalid (negative) length is reported without allocating an unbounded
/// buffer.
#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_bytes(length: i64) -> *mut Value {
    if length < 0 {
        return crate::std::random_result_err("length must not be negative".to_string());
    }
    if length > MAX_RANDOM_BYTES {
        return crate::std::random_result_err("length exceeds the 16 MiB limit".to_string());
    }
    let Ok(length) = usize::try_from(length) else {
        return crate::std::random_result_err("length is too large".to_string());
    };
    ensure_initialized();
    MODULE_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let mut bytes = Vec::with_capacity(length);
        while bytes.len() < length {
            *state = lcg_next(*state);
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(length);
        mux_rc_alloc(Value::Result(Ok(Box::new(Value::Bytes(bytes)))))
    })
}

fn result_float(result: Result<f64, String>) -> *mut Value {
    match result {
        Ok(value) => mux_rc_alloc(Value::Result(Ok(Box::new(Value::Float(value.into()))))),
        Err(error) => crate::std::random_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_normal(
    random: *const Value,
    mean: f64,
    standard_deviation: f64,
) -> *mut Value {
    if !mean.is_finite() {
        return result_float(Err("normal mean must be finite".to_string()));
    }
    if !standard_deviation.is_finite() || standard_deviation < 0.0 {
        return result_float(Err(
            "standard deviation must be finite and non-negative".to_string()
        ));
    }
    if standard_deviation == 0.0 {
        return result_float(Ok(mean));
    }
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return result_float(Err("invalid Random value".to_string()));
    };
    let mut u1 = (random_next(handle) >> 11) as f64 / ((1u64 << 53) as f64);
    while u1 == 0.0 {
        u1 = (random_next(handle) >> 11) as f64 / ((1u64 << 53) as f64);
    }
    let u2 = (random_next(handle) >> 11) as f64 / ((1u64 << 53) as f64);
    let radius = (-2.0 * u1.ln()).sqrt();
    let angle = 2.0 * std::f64::consts::PI * u2;
    result_float(Ok(mean + standard_deviation * radius * angle.cos()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_random_exponential(random: *const Value, rate: f64) -> *mut Value {
    if !rate.is_finite() || rate <= 0.0 {
        return result_float(Err(
            "exponential rate must be finite and positive".to_string()
        ));
    }
    let Some(handle) = (unsafe { random_handle(random) }) else {
        return result_float(Err("invalid Random value".to_string()));
    };
    let mut sample = (random_next(handle) >> 11) as f64 / ((1u64 << 53) as f64);
    while sample == 0.0 {
        sample = (random_next(handle) >> 11) as f64 / ((1u64 << 53) as f64);
    }
    result_float(Ok(-sample.ln() / rate))
}

/// Draw a normally distributed value using the Box-Muller transform.
#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_normal(mean: f64, standard_deviation: f64) -> *mut Value {
    if !mean.is_finite() {
        return result_float(Err("normal mean must be finite".to_string()));
    }
    if !standard_deviation.is_finite() || standard_deviation < 0.0 {
        return result_float(Err(
            "standard deviation must be finite and non-negative".to_string()
        ));
    }
    if standard_deviation == 0.0 {
        return result_float(Ok(mean));
    }
    let mut u1 = mux_rand_float();
    while u1 == 0.0 {
        u1 = mux_rand_float();
    }
    let u2 = mux_rand_float();
    let radius = (-2.0 * u1.ln()).sqrt();
    let angle = 2.0 * std::f64::consts::PI * u2;
    result_float(Ok(mean + standard_deviation * radius * angle.cos()))
}

/// Draw an exponentially distributed value for a positive rate parameter.
#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_exponential(rate: f64) -> *mut Value {
    if !rate.is_finite() || rate <= 0.0 {
        return result_float(Err(
            "exponential rate must be finite and positive".to_string()
        ));
    }
    let mut sample = mux_rand_float();
    while sample == 0.0 {
        sample = mux_rand_float();
    }
    result_float(Ok(-sample.ln() / rate))
}

unsafe fn list_items<'a>(value: *const Value) -> Option<&'a Vec<Value>> {
    if value.is_null() {
        return None;
    }
    match unsafe { &*value } {
        Value::List(items) => Some(items),
        _ => None,
    }
}

fn random_optional(value: Option<Value>) -> *mut Value {
    mux_rc_alloc(Value::Optional(value.map(Box::new)))
}

/// Choose one item from a non-empty list using the module's thread-local
/// generator. The original item is cloned because Mux collections are values.
///
/// # Safety
/// `items` must be null or a valid pointer to a live Mux value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_random_choose(items: *const Value) -> *mut Value {
    let Some(items) = list_items(items) else {
        return random_optional(None);
    };
    if items.is_empty() {
        return random_optional(None);
    }
    let index = mux_rand_range(0, items.len() as i64) as usize;
    random_optional(items.get(index).cloned())
}

/// Shuffle a list in place using Fisher-Yates and the module generator.
///
/// # Safety
/// `items` must be null or a valid, uniquely mutable pointer to a live Mux
/// value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_random_shuffle(items: *mut Value) {
    let Some(Value::List(items)) = (unsafe { items.as_mut() }) else {
        return;
    };
    let mut index = items.len();
    while index > 1 {
        index -= 1;
        let other = mux_rand_range(0, (index + 1) as i64) as usize;
        items.swap(index, other);
    }
}

/// Draw a sample without replacement. The returned list is a value copy and
/// the source list is left unchanged.
///
/// # Safety
/// `items` must be null or a valid pointer to a live Mux value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_random_sample(items: *const Value, count: i64) -> *mut Value {
    let Some(items) = list_items(items) else {
        return crate::std::random_result_err("items must be a list".to_string());
    };
    if count < 0 || count > items.len() as i64 {
        return crate::std::random_result_err(
            "sample count must be between 0 and the list length".to_string(),
        );
    }
    let mut indexes: Vec<usize> = (0..items.len()).collect();
    let mut remaining = indexes.len();
    let target = count as usize;
    while remaining > target {
        let choice = mux_rand_range(0, remaining as i64) as usize;
        indexes.swap(choice, remaining - 1);
        remaining -= 1;
    }
    let sampled = indexes[..target]
        .iter()
        .filter_map(|index| items.get(*index).cloned())
        .collect();
    mux_rc_alloc(Value::Result(Ok(Box::new(Value::List(sampled)))))
}

/// Choose an item according to non-negative finite weights. A zero total
/// weight is rejected rather than silently selecting an arbitrary item.
///
/// # Safety
/// `items` and `weights` must be null or valid pointers to live Mux values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_random_weighted_choice(
    items: *const Value,
    weights: *const Value,
) -> *mut Value {
    let Some(items) = list_items(items) else {
        return crate::std::random_result_err("items must be a list".to_string());
    };
    let Some(weights) = list_items(weights) else {
        return crate::std::random_result_err("weights must be a list".to_string());
    };
    if items.is_empty() || items.len() != weights.len() {
        return crate::std::random_result_err(
            "items and weights must have the same non-zero length".to_string(),
        );
    }
    let mut total = 0.0;
    let mut numeric = Vec::with_capacity(weights.len());
    for weight in weights {
        let Value::Float(weight) = weight else {
            return crate::std::random_result_err("weights must contain floats".to_string());
        };
        let weight = weight.into_inner();
        if !weight.is_finite() || weight < 0.0 {
            return crate::std::random_result_err(
                "weights must be finite and non-negative".to_string(),
            );
        }
        total += weight;
        numeric.push(weight);
    }
    if !total.is_finite() || total <= 0.0 {
        return crate::std::random_result_err(
            "weights must have a finite, positive total".to_string(),
        );
    }
    let mut threshold = mux_rand_float() * total;
    for (index, weight) in numeric.iter().enumerate() {
        if threshold < *weight {
            return mux_rc_alloc(Value::Result(Ok(Box::new(items[index].clone()))));
        }
        threshold -= *weight;
    }
    mux_rc_alloc(Value::Result(Ok(Box::new(items[items.len() - 1].clone()))))
}
