pub fn pow(base: f64, exp: f64) -> f64 {
    base.powf(exp)
}

pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

pub fn sin(x: f64) -> f64 {
    x.sin()
}

pub fn cos(x: f64) -> f64 {
    x.cos()
}

pub fn tan(x: f64) -> f64 {
    x.tan()
}

pub fn asin(x: f64) -> f64 {
    x.asin()
}

pub fn acos(x: f64) -> f64 {
    x.acos()
}

pub fn atan(x: f64) -> f64 {
    x.atan()
}

pub fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

pub fn ln(x: f64) -> f64 {
    x.ln()
}

pub fn log(x: f64, base: f64) -> f64 {
    x.log(base)
}

pub fn log2(x: f64) -> f64 {
    x.log2()
}

pub fn log10(x: f64) -> f64 {
    x.log10()
}

pub fn exp(x: f64) -> f64 {
    x.exp()
}

pub fn abs(x: f64) -> f64 {
    x.abs()
}

pub fn floor(x: f64) -> f64 {
    x.floor()
}

pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

pub fn round(x: f64) -> f64 {
    x.round()
}

pub fn min(a: f64, b: f64) -> f64 {
    a.min(b)
}

pub fn max(a: f64, b: f64) -> f64 {
    a.max(b)
}

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
