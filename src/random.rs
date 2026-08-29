use std::sync::{Mutex, MutexGuard, Once};
use std::time::{SystemTime, UNIX_EPOCH};

const RAND_MAX: i64 = 2147483647;

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
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_init(seed: i64) {
    INIT.call_once(|| {
        *lock_state() = seed as u64;
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
    ((*state >> 33) as i64) & RAND_MAX
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_rand_range(min: i64, max: i64) -> i64 {
    if min >= max {
        return min;
    }
    let range_size = max - min;
    // Fixed-point multiply: scale a random fraction by the range and take the
    // integer part. The shift must match the generator's WIDTH, not a machine
    // word - `mux_rand_int` masks with RAND_MAX and so yields 31 bits, and
    // shifting by 32 capped every result at half the requested range.
    let scaled = ((mux_rand_int() as u128) * (range_size as u128)) >> RAND_BITS;
    min + (scaled as i64)
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
