//! Unit tests for datetime helpers, driven by fixed UTC timestamps so the
//! assertions are deterministic regardless of the machine clock or timezone.

mod common;

use std::ffi::CString;

use common::{assert_err, assert_ok, ok_int, ok_string};
use mux_runtime::datetime::*;

// Unix epoch: 1970-01-01 00:00:00 UTC, a Thursday.
const EPOCH: i64 = 0;

#[test]
fn fields_at_epoch() {
    assert_eq!(ok_int(mux_datetime_year(EPOCH)), 1970);
    assert_eq!(ok_int(mux_datetime_month(EPOCH)), 1);
    assert_eq!(ok_int(mux_datetime_day(EPOCH)), 1);
    assert_eq!(ok_int(mux_datetime_hour(EPOCH)), 0);
    assert_eq!(ok_int(mux_datetime_minute(EPOCH)), 0);
    assert_eq!(ok_int(mux_datetime_second(EPOCH)), 0);
    // num_days_from_sunday: Thursday == 4
    assert_eq!(ok_int(mux_datetime_weekday(EPOCH)), 4);
}

#[test]
fn format_utc() {
    let date = CString::new("%Y-%m-%d").unwrap();
    assert_eq!(ok_string(mux_datetime_format(EPOCH, date.as_ptr())), "1970-01-01");
    let time = CString::new("%H:%M:%S").unwrap();
    assert_eq!(ok_string(mux_datetime_format(EPOCH, time.as_ptr())), "00:00:00");
}

#[test]
fn now_and_sleep_validation() {
    assert_ok(mux_datetime_now());
    assert_ok(mux_datetime_now_millis());
    assert_ok(mux_datetime_sleep_millis(0)); // no real wait
    assert_err(mux_datetime_sleep(-1));
    assert_err(mux_datetime_sleep_millis(-1));
}

#[test]
fn invalid_inputs_are_errors() {
    assert_err(mux_datetime_year(i64::MAX)); // out-of-range timestamp
    assert_err(mux_datetime_format(EPOCH, std::ptr::null())); // null pattern
}
