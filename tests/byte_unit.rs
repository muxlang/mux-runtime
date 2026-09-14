//! Unit coverage for checked and wrapping byte operations.

mod common;

use common::{assert_err, ok_int};
use mux_runtime::byte::*;

#[test]
fn checked_arithmetic_reports_domain_errors() {
    assert_eq!(ok_int(mux_byte_checked_add(40, 2)), 42);
    assert_err(mux_byte_checked_add(255, 1));
    assert_eq!(ok_int(mux_byte_checked_sub(40, 2)), 38);
    assert_err(mux_byte_checked_sub(0, 1));
    assert_eq!(ok_int(mux_byte_checked_mul(6, 7)), 42);
    assert_err(mux_byte_checked_mul(16, 16));
    assert_eq!(ok_int(mux_byte_checked_div(84, 2)), 42);
    assert_err(mux_byte_checked_div(1, 0));
    assert_eq!(ok_int(mux_byte_checked_rem(44, 2)), 0);
    assert_err(mux_byte_checked_rem(1, 0));
}

#[test]
fn wrapping_saturating_and_bit_operations() {
    assert_eq!(mux_byte_wrapping_add(255, 2), 1);
    assert_eq!(mux_byte_wrapping_sub(0, 1), 255);
    assert_eq!(mux_byte_wrapping_mul(17, 16), 16);
    assert_eq!(mux_byte_saturating_add(255, 2), 255);
    assert_eq!(mux_byte_saturating_sub(0, 2), 0);
    assert_eq!(mux_byte_saturating_mul(20, 20), 255);
    assert_eq!(mux_byte_bit_and(0b1010, 0b0110), 0b0010);
    assert_eq!(mux_byte_bit_or(0b1010, 0b0110), 0b1110);
    assert_eq!(mux_byte_bit_xor(0b1010, 0b0110), 0b1100);
    assert_eq!(mux_byte_bit_not(0), 255);
    assert_eq!(mux_byte_rotate_left(1, 1), 2);
    assert_eq!(mux_byte_rotate_right(2, 1), 1);
    assert_eq!(ok_int(mux_byte_shift_left(3, 2)), 12);
    assert_eq!(ok_int(mux_byte_shift_right(12, 2)), 3);
    assert_err(mux_byte_shift_left(1, 8));
}
