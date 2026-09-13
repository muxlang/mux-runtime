use crate::Value;

#[must_use]
pub fn pow(base: f64, exp: f64) -> f64 {
    base.powf(exp)
}

#[must_use]
pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

#[must_use]
pub fn sin(x: f64) -> f64 {
    x.sin()
}

#[must_use]
pub fn cos(x: f64) -> f64 {
    x.cos()
}

#[must_use]
pub fn tan(x: f64) -> f64 {
    x.tan()
}

#[must_use]
pub fn asin(x: f64) -> f64 {
    x.asin()
}

#[must_use]
pub fn acos(x: f64) -> f64 {
    x.acos()
}

#[must_use]
pub fn atan(x: f64) -> f64 {
    x.atan()
}

#[must_use]
pub fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

#[must_use]
pub fn ln(x: f64) -> f64 {
    x.ln()
}

#[must_use]
pub fn log(x: f64, base: f64) -> f64 {
    x.log(base)
}

#[must_use]
pub fn log2(x: f64) -> f64 {
    x.log2()
}

#[must_use]
pub fn log10(x: f64) -> f64 {
    x.log10()
}

#[must_use]
pub fn exp(x: f64) -> f64 {
    x.exp()
}

#[must_use]
pub fn abs(x: f64) -> f64 {
    x.abs()
}

#[must_use]
pub fn floor(x: f64) -> f64 {
    x.floor()
}

#[must_use]
pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

#[must_use]
pub fn round(x: f64) -> f64 {
    x.round()
}

#[must_use]
pub fn trunc(x: f64) -> f64 {
    x.trunc()
}

#[must_use]
pub fn fract(x: f64) -> f64 {
    x.fract()
}

#[must_use]
pub fn sinh(x: f64) -> f64 {
    x.sinh()
}

#[must_use]
pub fn cosh(x: f64) -> f64 {
    x.cosh()
}

#[must_use]
pub fn tanh(x: f64) -> f64 {
    x.tanh()
}

#[must_use]
pub fn asinh(x: f64) -> f64 {
    x.asinh()
}

#[must_use]
pub fn acosh(x: f64) -> f64 {
    x.acosh()
}

#[must_use]
pub fn atanh(x: f64) -> f64 {
    x.atanh()
}

#[must_use]
pub fn to_radians(x: f64) -> f64 {
    x.to_radians()
}

#[must_use]
pub fn to_degrees(x: f64) -> f64 {
    x.to_degrees()
}

#[must_use]
pub fn exp2(x: f64) -> f64 {
    x.exp2()
}

#[must_use]
pub fn exp_m1(x: f64) -> f64 {
    x.exp_m1()
}

#[must_use]
pub fn ln_1p(x: f64) -> f64 {
    x.ln_1p()
}

#[must_use]
pub fn cbrt(x: f64) -> f64 {
    x.cbrt()
}

#[must_use]
pub fn signum(x: f64) -> f64 {
    x.signum()
}

/// Sum a sequence with Neumaier compensation so small terms are not lost
/// when they are added to a much larger running total.
#[must_use]
pub fn sum(values: &[f64]) -> f64 {
    let mut total = 0.0;
    let mut compensation = 0.0;
    for value in values {
        let next = total + value;
        if total.abs() >= value.abs() {
            compensation += (total - next) + value;
        } else {
            compensation += (value - next) + total;
        }
        total = next;
    }
    total + compensation
}

/// Multiply a sequence while retaining the first rounding error of each
/// product. Non-finite intermediates fall back to ordinary IEEE multiplication
/// so NaN and infinity behavior stays exactly as users expect.
#[must_use]
pub fn product(values: &[f64]) -> f64 {
    let mut total = 1.0_f64;
    let mut compensation = 0.0_f64;
    for value in values {
        let next = total * value;
        if !next.is_finite() {
            return values.iter().copied().product();
        }
        let error = total.mul_add(*value, -next);
        compensation = compensation.mul_add(*value, error);
        total = next;
    }
    total + compensation
}

/// Error function using the classic Abramowitz-Stegun approximation. Its
/// maximum absolute error is below 1.5e-7, which is appropriate for a scalar
/// standard-library helper without pulling in a platform-specific libm.
#[must_use]
pub fn erf(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x.is_infinite() {
        return x.signum();
    }
    let sign = x.signum();
    let ax = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * ax);
    let polynomial =
        (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t;
    sign * (1.0 - polynomial * (-ax * ax).exp())
}

/// Gamma function using a Lanczos approximation for positive values and the
/// reflection formula for negative non-integral values.
#[must_use]
pub fn gamma(x: f64) -> f64 {
    const COEFFICIENTS: [f64; 8] = [
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x.is_nan() {
        return f64::NAN;
    }
    if x < 0.5 {
        return std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * gamma(1.0 - x));
    }
    let z = x - 1.0;
    let mut sum = 0.999_999_999_999_809_9;
    for (index, coefficient) in COEFFICIENTS.into_iter().enumerate() {
        sum += coefficient / (z + index as f64 + 1.0);
    }
    let t = z + 7.5;
    (2.0 * std::f64::consts::PI).sqrt() * t.powf(z + 0.5) * (-t).exp() * sum
}

#[must_use]
pub fn min(a: f64, b: f64) -> f64 {
    a.min(b)
}

#[must_use]
pub fn max(a: f64, b: f64) -> f64 {
    a.max(b)
}

#[must_use]
pub fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

pub const PI: f64 = std::f64::consts::PI;

pub const E: f64 = std::f64::consts::E;

// --- extern "C" wrappers ---

macro_rules! mux_math_extern {
    // single-arg: fn(f64) -> f64
    ($export:ident, $name:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $export(x: f64) -> f64 {
            $name(x)
        }
    };
    // two-arg: fn(f64, f64) -> f64
    ($export:ident, $name:ident, $a:ident, $b:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $export($a: f64, $b: f64) -> f64 {
            $name($a, $b)
        }
    };
}

mux_math_extern!(mux_math_sqrt, sqrt);
mux_math_extern!(mux_math_sin, sin);
mux_math_extern!(mux_math_cos, cos);
mux_math_extern!(mux_math_tan, tan);
mux_math_extern!(mux_math_asin, asin);
mux_math_extern!(mux_math_acos, acos);
mux_math_extern!(mux_math_atan, atan);
mux_math_extern!(mux_math_ln, ln);
mux_math_extern!(mux_math_log2, log2);
mux_math_extern!(mux_math_log10, log10);
mux_math_extern!(mux_math_exp, exp);
mux_math_extern!(mux_math_abs, abs);
mux_math_extern!(mux_math_floor, floor);
mux_math_extern!(mux_math_ceil, ceil);
mux_math_extern!(mux_math_round, round);
mux_math_extern!(mux_math_trunc, trunc);
mux_math_extern!(mux_math_fract, fract);
mux_math_extern!(mux_math_sinh, sinh);
mux_math_extern!(mux_math_cosh, cosh);
mux_math_extern!(mux_math_tanh, tanh);
mux_math_extern!(mux_math_asinh, asinh);
mux_math_extern!(mux_math_acosh, acosh);
mux_math_extern!(mux_math_atanh, atanh);
mux_math_extern!(mux_math_to_radians, to_radians);
mux_math_extern!(mux_math_to_degrees, to_degrees);
mux_math_extern!(mux_math_exp2, exp2);
mux_math_extern!(mux_math_exp_m1, exp_m1);
mux_math_extern!(mux_math_ln_1p, ln_1p);
mux_math_extern!(mux_math_cbrt, cbrt);
mux_math_extern!(mux_math_signum, signum);
mux_math_extern!(mux_math_erf, erf);
mux_math_extern!(mux_math_gamma, gamma);

fn float_list(value: *const Value) -> Result<Vec<f64>, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err("values must be a list<float>".to_string());
    };
    values
        .iter()
        .map(|value| match value {
            Value::Float(value) => Ok(value.into_inner()),
            _ => Err("values must contain only floats".to_string()),
        })
        .collect()
}

#[unsafe(no_mangle)]
/// # Safety
/// `values` must be null or point to a live Mux `Value` containing a list.
pub unsafe extern "C" fn mux_math_sum(values: *const Value) -> *mut Value {
    match float_list(values) {
        Ok(values) => result_ok(Value::Float(ordered_float::OrderedFloat(sum(&values)))),
        Err(message) => result_err(message),
    }
}

#[unsafe(no_mangle)]
/// # Safety
/// `values` must be null or point to a live Mux `Value` containing a list.
pub unsafe extern "C" fn mux_math_product(values: *const Value) -> *mut Value {
    match float_list(values) {
        Ok(values) => result_ok(Value::Float(ordered_float::OrderedFloat(product(&values)))),
        Err(message) => result_err(message),
    }
}

mux_math_extern!(mux_math_pow, pow, base, exp);
mux_math_extern!(mux_math_atan2, atan2, y, x);
mux_math_extern!(mux_math_log, log, x, base);
mux_math_extern!(mux_math_min, min, a, b);
mux_math_extern!(mux_math_max, max, a, b);
mux_math_extern!(mux_math_hypot, hypot, x, y);

/// Integer exponentiation using exponentiation by squaring.
/// Handles negative exponents: 1^(-n)=1, (-1)^(-n)=1/-1, other^(-n)=0 (truncates).
/// Uses wrapping multiplication on overflow.
#[unsafe(no_mangle)]
pub extern "C" fn mux_int_pow(base: i64, exp: i64) -> i64 {
    if exp < 0 {
        // Handle special cases: 1^(-n) = 1, (-1)^(-n) = 1/-1
        if base == 1 {
            return 1;
        }
        if base == -1 {
            // (-1)^(-n) = 1/((-1)^n)
            // If n is odd: 1/(-1) = -1; if n is even: 1/1 = 1
            return if (-exp) % 2 == 0 { 1 } else { -1 };
        }
        // 1/(other^n) truncates to 0 for integers
        return 0;
    }
    let mut result = 1i64;
    let mut b = base;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            result = result.wrapping_mul(b);
        }
        b = b.wrapping_mul(b);
        e >>= 1;
    }
    result
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_pi() -> f64 {
    PI
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_e() -> f64 {
    E
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_is_nan(value: f64) -> bool {
    value.is_nan()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_is_infinite(value: f64) -> bool {
    value.is_infinite()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_is_finite(value: f64) -> bool {
    value.is_finite()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_gcd(a: i64, b: i64) -> i64 {
    // The absolute value of i64::MIN cannot be represented as an i64. Keep
    // Euclid's algorithm in unsigned space so that edge case is defined.
    let ua = gcd_unsigned(a.unsigned_abs(), b.unsigned_abs());
    if ua > i64::MAX as u64 {
        return i64::MAX;
    }
    ua as i64
}

fn gcd_unsigned(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_lcm(a: i64, b: i64) -> i64 {
    if a == 0 || b == 0 {
        return 0;
    }
    // Keep the calculation unsigned: gcd(MIN, 0) is 2^63, which is
    // intentionally saturated when returned by `gcd` but must not be reused
    // as a signed divisor here.
    let gcd = gcd_unsigned(a.unsigned_abs(), b.unsigned_abs());
    let product = u128::from(a.unsigned_abs() / gcd) * u128::from(b.unsigned_abs());
    i64::try_from(product).unwrap_or(i64::MAX)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_isqrt(value: i64) -> i64 {
    if value <= 0 {
        return 0;
    }
    let mut low = 1i64;
    let mut high = value.min(3_037_000_499); // floor(sqrt(i64::MAX))
    let mut answer = 0i64;
    while low <= high {
        let middle = low + (high - low) / 2;
        if middle <= value / middle {
            answer = middle;
            low = middle + 1;
        } else {
            high = middle - 1;
        }
    }
    answer
}

fn result_ok(value: Value) -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn result_err(message: impl Into<String>) -> *mut Value {
    crate::std::math_result_err(message.into())
}

/// Clamp a value to an inclusive range. Reversed bounds are normalized so the
/// function remains total and never panics.
#[must_use]
pub fn clamp(value: f64, lower: f64, upper: f64) -> f64 {
    let (low, high) = if lower <= upper {
        (lower, upper)
    } else {
        (upper, lower)
    };
    value.max(low).min(high)
}

#[must_use = "check the returned error before using the clamped value"]
pub fn clamp_checked(value: f64, lower: f64, upper: f64) -> Result<f64, String> {
    if lower > upper {
        return Err("clamp lower bound must not exceed upper bound".to_string());
    }
    Ok(value.max(lower).min(upper))
}

#[must_use]
pub fn lerp(start: f64, end: f64, amount: f64) -> f64 {
    start + (end - start) * amount
}

#[must_use = "check the returned error before using the interpolation value"]
#[allow(clippy::float_cmp)]
pub fn inverse_lerp(start: f64, end: f64, value: f64) -> Result<f64, String> {
    if start == end {
        return Err("inverse_lerp endpoints must differ".to_string());
    }
    Ok((value - start) / (end - start))
}

#[must_use = "check the returned error before using the interpolation value"]
pub fn smoothstep(edge0: f64, edge1: f64, value: f64) -> Result<f64, String> {
    if edge0 >= edge1 {
        return Err("smoothstep edges must be strictly increasing".to_string());
    }
    let t = clamp((value - edge0) / (edge1 - edge0), 0.0, 1.0);
    Ok(t * t * (3.0 - 2.0 * t))
}

fn checked_combinatoric(value: u128, operation: &str) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{operation} result overflowed int"))
}

pub fn factorial(value: i64) -> Result<i64, String> {
    if value < 0 {
        return Err("factorial is undefined for negative integers".to_string());
    }
    let mut result = 1u128;
    for factor in 2..=value as u128 {
        result = result.saturating_mul(factor);
    }
    checked_combinatoric(result, "factorial")
}

pub fn combinations(n: i64, k: i64) -> Result<i64, String> {
    if n < 0 || k < 0 || k > n {
        return Err("combinations require 0 <= k <= n".to_string());
    }
    let k = k.min(n - k) as u128;
    let mut result = 1u128;
    for index in 1..=k {
        let numerator = result
            .checked_mul(n as u128 - k + index)
            .ok_or_else(|| "combinations result overflowed internal range".to_string())?;
        result = numerator / index;
    }
    checked_combinatoric(result, "combinations")
}

pub fn permutations(n: i64, k: i64) -> Result<i64, String> {
    if n < 0 || k < 0 || k > n {
        return Err("permutations require 0 <= k <= n".to_string());
    }
    let mut result = 1u128;
    for value in (n - k + 1) as u128..=n as u128 {
        result = result
            .checked_mul(value)
            .ok_or_else(|| "permutations result overflowed internal range".to_string())?;
    }
    checked_combinatoric(result, "permutations")
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_clamp(value: f64, lower: f64, upper: f64) -> f64 {
    clamp(value, lower, upper)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_clamp_checked(value: f64, lower: f64, upper: f64) -> *mut Value {
    match clamp_checked(value, lower, upper) {
        Ok(value) => result_ok(Value::Float(value.into())),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_lerp(start: f64, end: f64, amount: f64) -> f64 {
    lerp(start, end, amount)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_inverse_lerp(start: f64, end: f64, value: f64) -> *mut Value {
    match inverse_lerp(start, end, value) {
        Ok(value) => result_ok(Value::Float(value.into())),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_smoothstep(edge0: f64, edge1: f64, value: f64) -> *mut Value {
    match smoothstep(edge0, edge1, value) {
        Ok(value) => result_ok(Value::Float(value.into())),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_factorial(value: i64) -> *mut Value {
    match factorial(value) {
        Ok(value) => result_ok(Value::Int(value)),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_combinations(n: i64, k: i64) -> *mut Value {
    match combinations(n, k) {
        Ok(value) => result_ok(Value::Int(value)),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_math_permutations(n: i64, k: i64) -> *mut Value {
    match permutations(n, k) {
        Ok(value) => result_ok(Value::Int(value)),
        Err(error) => result_err(error),
    }
}
