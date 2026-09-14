//! Unit tests for datetime helpers, driven by fixed UTC timestamps so the
//! assertions are deterministic regardless of the machine clock or timezone.

mod common;

use std::ffi::CString;

use common::{assert_err, assert_ok, ok_int, ok_string};
use mux_runtime::datetime::*;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::mux_result_is_err;
use mux_runtime::std::{mux_datetime_error_detail, mux_datetime_error_kind};
use mux_runtime::Value;

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
    assert_eq!(
        ok_string(unsafe { mux_datetime_format(EPOCH, date.as_ptr()) }),
        "1970-01-01"
    );
    let time = CString::new("%H:%M:%S").unwrap();
    assert_eq!(
        ok_string(unsafe { mux_datetime_format(EPOCH, time.as_ptr()) }),
        "00:00:00"
    );
}

#[test]
fn now_and_sleep_validation() {
    assert_ok(mux_datetime_now());
    assert_ok(mux_datetime_now_millis());
    assert_ok(mux_datetime_now_micros());
    assert_ok(mux_datetime_now_nanos());
    assert_ok(mux_datetime_sleep_millis(0)); // no real wait
    assert_err(mux_datetime_sleep(-1));
    assert_err(mux_datetime_sleep_millis(-1));
}

#[test]
fn invalid_inputs_are_errors() {
    assert_err(mux_datetime_year(i64::MAX)); // out-of-range timestamp
    assert_err(unsafe { mux_datetime_format(EPOCH, std::ptr::null()) }); // null pattern
}

#[test]
fn invalid_utf8_inputs_are_errors() {
    let invalid = [0xff_u8, 0];
    let timestamp = unsafe { mux_datetime_parse_timestamp(invalid.as_ptr().cast()) };
    assert!(unsafe { mux_result_is_err(timestamp) });
    assert!(unsafe { mux_rc_dec(timestamp) });
    let http_date = unsafe { mux_datetime_parse_http_date(invalid.as_ptr().cast()) };
    assert!(unsafe { mux_result_is_err(http_date) });
    assert!(unsafe { mux_rc_dec(http_date) });
    let formatted = unsafe { mux_datetime_format(0, invalid.as_ptr().cast()) };
    assert!(unsafe { mux_result_is_err(formatted) });
    assert!(unsafe { mux_rc_dec(formatted) });
}

#[test]
fn rfc3339_roundtrip_and_validation() {
    let input = CString::new("2024-02-29T12:34:56+02:00").unwrap();
    let timestamp = ok_int(unsafe { mux_datetime_parse_timestamp(input.as_ptr()) });
    assert_eq!(timestamp, 1_709_202_896);
    assert_eq!(
        ok_string(mux_datetime_format_timestamp(timestamp)),
        "2024-02-29T10:34:56Z"
    );
    let invalid = CString::new("not-a-date").unwrap();
    let result = unsafe { mux_datetime_parse_timestamp(invalid.as_ptr()) };
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_runtime::result::mux_result_data(result) };
    let kind = unsafe { mux_datetime_error_kind(error) };
    let detail = unsafe { mux_datetime_error_detail(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes())
    );
    assert!(matches!(unsafe { &*detail }, Value::String(value) if value.contains("RFC 3339")));
    unsafe {
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

#[test]
fn http_date_roundtrip_accepts_standard_forms() {
    let input = CString::new("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
    let timestamp = ok_int(unsafe { mux_datetime_parse_http_date(input.as_ptr()) });
    assert_eq!(timestamp, 784_111_777);
    assert_eq!(
        ok_string(mux_datetime_format_http_date(timestamp)),
        "Sun, 06 Nov 1994 08:49:37 GMT"
    );

    let rfc850 = CString::new("Sunday, 06-Nov-94 08:49:37 GMT").unwrap();
    assert_eq!(
        ok_int(unsafe { mux_datetime_parse_http_date(rfc850.as_ptr()) }),
        timestamp
    );
    let asctime = CString::new("Sun Nov  6 08:49:37 1994 GMT").unwrap();
    assert_eq!(
        ok_int(unsafe { mux_datetime_parse_http_date(asctime.as_ptr()) }),
        timestamp
    );
    let invalid = CString::new("not-a-date").unwrap();
    assert_err(unsafe { mux_datetime_parse_http_date(invalid.as_ptr()) });
}
