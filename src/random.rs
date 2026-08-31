use std::sync::{Mutex, MutexGuard, Once};
use std::time::{SystemTime, UNIX_EPOCH};

const RAND_MAX: i64 = 2_147_483_647;

/// Width of `mux_rand_int`'s output, in bits. `RAND_MAX` is `2^31 - 1`, so the
/// generator fills 31 bits. `mux_rand_range` scales by this; deriving it from
/// `RAND_MAX` keeps the two from drifting apart if the generator ever widens.
const RAND_BITS: u32 = RAND_MAX.count_ones();

static INIT: Once = Once::new();
static STATE: Mutex<u64> = Mutex::new(0);

fn lock_state() -> MutexGuard<'static, u64> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lcg_next(state: u64) -> u64 {
    state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_init(seed: i64) {
    INIT.call_once(|| {
        *lock_state() = seed.cast_unsigned();
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_int() -> i64 {
    INIT.call_once(|| {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        *lock_state() = seed;
    });
    let mut state = lock_state();
    *state = lcg_next(*state);
    ((*state >> 33).cast_signed()) & RAND_MAX
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
