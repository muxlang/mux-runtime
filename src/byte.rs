//! Checked operations for Mux's byte scalar.
//!
//! The compiler stores a byte in the same i64 ABI slot as other integer
//! scalars, but every operation in this module validates the 0..=255 domain.

use crate::refcount::mux_rc_alloc;
use crate::std::byte_result_err;
use crate::Value;

fn result_ok(value: i64) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(Value::Int(value)))))
}

fn checked(value: i64) -> Result<u8, String> {
    u8::try_from(value).map_err(|_| format!("Byte value {value} is outside the range 0..255"))
}

fn checked_pair(left: i64, right: i64) -> Result<(u8, u8), String> {
    Ok((checked(left)?, checked(right)?))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_checked_add(left: i64, right: i64) -> *mut Value {
    match checked_pair(left, right) {
        Ok((a, b)) => {
            let sum = i64::from(a) + i64::from(b);
            if sum <= 255 {
                result_ok(sum)
            } else {
                byte_result_err("Byte addition overflowed")
            }
        }
        Err(error) => byte_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_checked_sub(left: i64, right: i64) -> *mut Value {
    match checked_pair(left, right) {
        Ok((a, b)) => i64::from(a)
            .checked_sub(i64::from(b))
            .filter(|value| *value >= 0)
            .map_or_else(
                || byte_result_err("Byte subtraction underflowed"),
                result_ok,
            ),
        Err(error) => byte_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_checked_mul(left: i64, right: i64) -> *mut Value {
    match checked_pair(left, right) {
        Ok((a, b)) => i64::from(a)
            .checked_mul(i64::from(b))
            .filter(|value| *value <= 255)
            .map_or_else(
                || byte_result_err("Byte multiplication overflowed"),
                result_ok,
            ),
        Err(error) => byte_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_checked_div(left: i64, right: i64) -> *mut Value {
    match checked_pair(left, right) {
        Ok((_, 0)) => byte_result_err("Byte division by zero"),
        Ok((a, b)) => result_ok(i64::from(a / b)),
        Err(error) => byte_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_checked_rem(left: i64, right: i64) -> *mut Value {
    match checked_pair(left, right) {
        Ok((_, 0)) => byte_result_err("Byte modulo by zero"),
        Ok((a, b)) => result_ok(i64::from(a % b)),
        Err(error) => byte_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_wrapping_add(left: i64, right: i64) -> i64 {
    (left.rem_euclid(256) + right.rem_euclid(256)).rem_euclid(256)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_wrapping_sub(left: i64, right: i64) -> i64 {
    (left.rem_euclid(256) - right.rem_euclid(256)).rem_euclid(256)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_wrapping_mul(left: i64, right: i64) -> i64 {
    (left.rem_euclid(256) * right.rem_euclid(256)).rem_euclid(256)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_saturating_add(left: i64, right: i64) -> i64 {
    left.clamp(0, 255)
        .saturating_add(right.clamp(0, 255))
        .min(255)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_saturating_sub(left: i64, right: i64) -> i64 {
    left.clamp(0, 255)
        .saturating_sub(right.clamp(0, 255))
        .max(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_saturating_mul(left: i64, right: i64) -> i64 {
    left.clamp(0, 255)
        .saturating_mul(right.clamp(0, 255))
        .min(255)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_bit_and(left: i64, right: i64) -> i64 {
    (left as u8 & right as u8) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_bit_or(left: i64, right: i64) -> i64 {
    (left as u8 | right as u8) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_bit_xor(left: i64, right: i64) -> i64 {
    (left as u8 ^ right as u8) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_bit_not(value: i64) -> i64 {
    (!value as u8) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_rotate_left(value: i64, shift: i64) -> i64 {
    (value as u8).rotate_left(shift.rem_euclid(8) as u32) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_rotate_right(value: i64, shift: i64) -> i64 {
    (value as u8).rotate_right(shift.rem_euclid(8) as u32) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_shift_left(value: i64, shift: i64) -> *mut Value {
    if !(0..=7).contains(&shift) {
        return byte_result_err("Byte shift amount must be in the range 0..7");
    }
    let shifted = (value as u8).checked_shl(shift as u32).unwrap_or(0);
    result_ok(i64::from(shifted))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_byte_shift_right(value: i64, shift: i64) -> *mut Value {
    if !(0..=7).contains(&shift) {
        return byte_result_err("Byte shift amount must be in the range 0..7");
    }
    result_ok(i64::from((value as u8) >> shift))
}
