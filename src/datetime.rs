use crate::refcount::mux_rc_alloc;
use crate::Value;
use chrono::{DateTime, Datelike, Local, NaiveDateTime, SecondsFormat, Timelike, Utc};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn dt_ok(val: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(val))))
}

fn dt_err(msg: String) -> *mut Value {
    crate::std::datetime_result_err(msg)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_now() -> *mut Value {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let Ok(seconds) = i64::try_from(duration.as_secs()) else {
                return dt_err("Current time exceeds the supported timestamp range".to_string());
            };
            dt_ok(Value::Int(seconds))
        }
        Err(e) => dt_err(format!("Failed to get current time: {e}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_now_millis() -> *mut Value {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let Ok(millis) = i64::try_from(duration.as_millis()) else {
                return dt_err("Current time exceeds the supported timestamp range".to_string());
            };
            dt_ok(Value::Int(millis))
        }
        Err(e) => dt_err(format!("Failed to get current time: {e}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_now_micros() -> *mut Value {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => match i64::try_from(duration.as_micros()) {
            Ok(micros) => dt_ok(Value::Int(micros)),
            Err(_) => dt_err("Current time exceeds the supported timestamp range".to_string()),
        },
        Err(e) => dt_err(format!("Failed to get current time: {e}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_now_nanos() -> *mut Value {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => match i64::try_from(duration.as_nanos()) {
            Ok(nanos) => dt_ok(Value::Int(nanos)),
            Err(_) => dt_err("Current time exceeds the supported timestamp range".to_string()),
        },
        Err(e) => dt_err(format!("Failed to get current time: {e}")),
    }
}

fn timestamp_to_datetime(timestamp: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(timestamp, 0)
}

fn read_pattern(pattern: *const c_char) -> Result<String, String> {
    if pattern.is_null() {
        return Err("Format pattern cannot be null".to_string());
    }

    let bytes = unsafe { CStr::from_ptr(pattern) }.to_bytes();
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| "Format pattern must be valid UTF-8".to_string())
}

fn datetime_field(timestamp: i64, get_field: impl FnOnce(&DateTime<Utc>) -> i64) -> *mut Value {
    match timestamp_to_datetime(timestamp) {
        Some(dt) => dt_ok(Value::Int(get_field(&dt))),
        None => dt_err(format!("Invalid timestamp: {timestamp}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_year(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| i64::from(dt.year()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_month(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| i64::from(dt.month()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_day(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| i64::from(dt.day()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_hour(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| i64::from(dt.hour()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_minute(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| i64::from(dt.minute()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_second(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| i64::from(dt.second()))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_weekday(timestamp: i64) -> *mut Value {
    datetime_field(timestamp, |dt| {
        i64::from(dt.weekday().num_days_from_sunday())
    })
}

#[unsafe(no_mangle)]
/// Formats a UTC timestamp with a C-string pattern.
///
/// # Safety
///
/// `pattern` may be null, which returns a result error; otherwise it must be a
/// valid NUL-terminated C string readable for the duration of this call.
pub unsafe extern "C" fn mux_datetime_format(timestamp: i64, pattern: *const c_char) -> *mut Value {
    let pattern = match read_pattern(pattern) {
        Ok(p) => p,
        Err(msg) => return dt_err(msg),
    };

    match timestamp_to_datetime(timestamp) {
        Some(dt) => {
            let formatted = dt.format(&pattern).to_string();
            dt_ok(Value::String(formatted))
        }
        None => dt_err(format!("Invalid timestamp: {timestamp}")),
    }
}

#[unsafe(no_mangle)]
/// Formats a timestamp in the local timezone with a C-string pattern.
///
/// # Safety
///
/// `pattern` may be null, which returns a result error; otherwise it must be a
/// valid NUL-terminated C string readable for the duration of this call.
pub unsafe extern "C" fn mux_datetime_format_local(
    timestamp: i64,
    pattern: *const c_char,
) -> *mut Value {
    let pattern = match read_pattern(pattern) {
        Ok(p) => p,
        Err(msg) => return dt_err(msg),
    };

    match timestamp_to_datetime(timestamp) {
        Some(dt) => {
            let local_dt = dt.with_timezone(&Local);
            let formatted = local_dt.format(&pattern).to_string();
            dt_ok(Value::String(formatted))
        }
        None => dt_err(format!("Invalid timestamp: {timestamp}")),
    }
}

/// Parse an RFC 3339 timestamp and return Unix seconds.
///
/// # Safety
/// `input` must be null or a valid NUL-terminated string for the duration of
/// the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_parse_timestamp(input: *const c_char) -> *mut Value {
    if input.is_null() {
        return dt_err("RFC 3339 input cannot be null".to_string());
    }
    let Ok(text) = std::str::from_utf8(unsafe { CStr::from_ptr(input) }.to_bytes()) else {
        return dt_err("RFC 3339 input must be valid UTF-8".to_string());
    };
    match DateTime::parse_from_rfc3339(text) {
        Ok(parsed) => dt_ok(Value::Int(parsed.timestamp())),
        Err(error) => dt_err(format!("Invalid RFC 3339 timestamp: {error}")),
    }
}

/// Parse an HTTP-date in IMF-fixdate, RFC 850, or asctime form and return Unix
/// seconds. HTTP dates are always interpreted as UTC.
///
/// # Safety
/// `input` must be null or point to a valid NUL-terminated string for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_parse_http_date(input: *const c_char) -> *mut Value {
    if input.is_null() {
        return dt_err("HTTP date cannot be null".to_string());
    }
    let Ok(text) = std::str::from_utf8(unsafe { CStr::from_ptr(input) }.to_bytes()) else {
        return dt_err("HTTP date must be valid UTF-8".to_string());
    };
    let parsed = DateTime::parse_from_rfc2822(text)
        .map(|value| value.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            text.split_once(", ").and_then(|(_, rest)| {
                NaiveDateTime::parse_from_str(rest, "%d-%b-%y %H:%M:%S GMT")
                    .ok()
                    .map(|value| DateTime::from_naive_utc_and_offset(value, Utc))
            })
        })
        .or_else(|| {
            let rest = ["Sun ", "Mon ", "Tue ", "Wed ", "Thu ", "Fri ", "Sat "]
                .iter()
                .find_map(|prefix| text.strip_prefix(prefix));
            rest.and_then(|rest| {
                NaiveDateTime::parse_from_str(rest, "%b %e %H:%M:%S %Y GMT")
                    .ok()
                    .map(|value| DateTime::from_naive_utc_and_offset(value, Utc))
            })
        });
    match parsed {
        Some(value) => dt_ok(Value::Int(value.timestamp())),
        None => dt_err("Invalid HTTP date".to_string()),
    }
}

/// Format Unix seconds as an RFC 3339 UTC timestamp.
#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_format_timestamp(timestamp: i64) -> *mut Value {
    match timestamp_to_datetime(timestamp) {
        Some(dt) => dt_ok(Value::String(
            dt.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )),
        None => dt_err(format!("Invalid timestamp: {timestamp}")),
    }
}

/// Format Unix seconds using the canonical IMF-fixdate HTTP representation.
#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_format_http_date(timestamp: i64) -> *mut Value {
    match timestamp_to_datetime(timestamp) {
        Some(dt) => dt_ok(Value::String(
            dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        )),
        None => dt_err(format!("Invalid timestamp: {timestamp}")),
    }
}

/// Sleep for the specified number of seconds.
/// Blocks the executing thread. For async/parallel use cases, consider using the `sync` module.
/// Returns error if seconds is negative.
#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_sleep(seconds: i64) -> *mut Value {
    if seconds < 0 {
        return dt_err("Sleep duration cannot be negative".to_string());
    }
    thread::sleep(Duration::from_secs(seconds.unsigned_abs()));
    dt_ok(Value::Unit)
}

/// Sleep for the specified number of milliseconds.
/// Blocks the executing thread. For async/parallel use cases, consider using the `sync` module.
/// Returns error if milliseconds is negative.
#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_sleep_millis(milliseconds: i64) -> *mut Value {
    if milliseconds < 0 {
        return dt_err("Sleep duration cannot be negative".to_string());
    }
    thread::sleep(Duration::from_millis(milliseconds.unsigned_abs()));
    dt_ok(Value::Unit)
}
