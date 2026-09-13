//! Typed calendar and clock values for the datetime standard library.
//!
//! Timestamp functions remain in the datetime module. This module provides the
//! value-oriented API: ordinary calendar values are validated at
//! construction, while clock state is kept behind explicit handles.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::{TypeId, Value};
use chrono::{
    DateTime as ChronoDateTime, Datelike, Duration as ChronoDuration, NaiveDate, NaiveTime,
    SecondsFormat, Timelike, Utc,
};
#[cfg(feature = "chrono-tz")]
use chrono::{LocalResult, Offset, TimeZone};
#[cfg(feature = "chrono-tz")]
use chrono_tz::Tz;
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_MILLI: i128 = 1_000_000;
const NANOS_PER_MICRO: i128 = 1_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DateValue {
    Date(NaiveDate),
    Time(NaiveTime),
    DateTime(ChronoDateTime<Utc>),
    #[cfg(feature = "chrono-tz")]
    ZonedDateTime(ChronoDateTime<Tz>),
    #[cfg(feature = "chrono-tz")]
    LocalResolution {
        zone: Tz,
        earlier: Option<ChronoDateTime<Tz>>,
        later: Option<ChronoDateTime<Tz>>,
    },
    Instant(i128),
    Duration(i128),
    Period {
        years: i64,
        months: i64,
        days: i64,
    },
}

struct Entry {
    value: DateValue,
    names: usize,
}

static VALUES: LazyLock<Mutex<HashMap<i64, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));

macro_rules! type_id {
    ($name:literal, $slot:ident) => {
        static $slot: LazyLock<TypeId> = LazyLock::new(|| {
            let id = register_object_type_with_copy($name, 8, Some(drop_value), Some(copy_value));
            crate::object::mux_register_object_equals(id, date_equals);
            crate::object::mux_register_object_compare(id, date_compare);
            crate::object::mux_register_object_hash(id, date_hash);
            id
        });
    };
}

type_id!("Date", DATE_TYPE_ID);
type_id!("Time", TIME_TYPE_ID);
type_id!("DateTime", DATETIME_TYPE_ID);
#[cfg(feature = "chrono-tz")]
type_id!("ZonedDateTime", ZONED_DATETIME_TYPE_ID);
#[cfg(feature = "chrono-tz")]
type_id!("LocalResolution", LOCAL_RESOLUTION_TYPE_ID);
type_id!("Instant", INSTANT_TYPE_ID);
type_id!("Duration", DURATION_TYPE_ID);
type_id!("Period", PERIOD_TYPE_ID);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => mux_rc_alloc(Value::Result(Ok(Box::new(value)))),
        Err(error) => crate::std::datetime_result_err(error),
    }
}

fn err(message: impl Into<String>) -> *mut Value {
    result(Err(message.into()))
}

fn next_id() -> i64 {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}

fn object(type_id: TypeId, value: DateValue) -> *mut Value {
    let id = next_id();
    lock(&VALUES).insert(id, Entry { value, names: 1 });
    let object = alloc_object(type_id);
    if object.is_null() {
        lock(&VALUES).remove(&id);
        return object;
    }
    let data = unsafe { get_object_ptr(object) };
    if data.is_null() {
        unsafe { mux_rc_dec(object) };
        lock(&VALUES).remove(&id);
        return std::ptr::null_mut();
    }
    unsafe { *data.cast::<i64>() = id };
    object
}

fn object_result(object: *mut Value, name: &str) -> *mut Value {
    if object.is_null() {
        return err(format!("could not allocate {name}"));
    }
    let Value::Object(reference) = (unsafe { &*object }) else {
        unsafe { mux_rc_dec(object) };
        return err(format!("could not allocate {name}"));
    };
    let value = Value::Object(reference.clone());
    unsafe { mux_rc_dec(object) };
    result(Ok(value))
}

extern "C" fn copy_value(source: *mut std::ffi::c_void, dest: *mut std::ffi::c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let mut values = lock(&VALUES);
    if let Some(entry) = values.get_mut(&id) {
        entry.names += 1;
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_value(ptr: *mut std::ffi::c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut values = lock(&VALUES);
    let remove = values.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        values.remove(&id);
    }
}

unsafe fn id(value: *const Value, type_id: TypeId, name: &str) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return Err(format!("expected {name} value"));
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err(format!("invalid {name} value"));
    }
    let id = unsafe { *ptr.cast::<i64>() };
    if lock(&VALUES).contains_key(&id) {
        Ok(id)
    } else {
        Err(format!("{name} value is closed"))
    }
}

fn value(value: *const Value, type_id: TypeId, name: &str) -> Result<DateValue, String> {
    let id = unsafe { id(value, type_id, name) }?;
    lock(&VALUES)
        .get(&id)
        .map(|entry| entry.value.clone())
        .ok_or_else(|| format!("{name} value is closed"))
}

/// Convert a DateTime value to its canonical RFC 3339 representation for
/// packages that need a portable textual interchange (for example SQL).
pub(crate) unsafe fn sql_datetime_string(value: *const Value) -> Result<String, String> {
    match self::value(value, *DATETIME_TYPE_ID, "DateTime")? {
        DateValue::DateTime(dt) => Ok(dt.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        _ => Err("expected DateTime value".to_string()),
    }
}

/// Parse a canonical RFC 3339 SQL value into a native DateTime value.
pub(crate) fn sql_datetime_value(text: &str) -> Result<Value, String> {
    let dt = ChronoDateTime::parse_from_rfc3339(text)
        .map_err(|error| format!("invalid RFC 3339 datetime: {error}"))?
        .with_timezone(&Utc);
    let ptr = object(*DATETIME_TYPE_ID, DateValue::DateTime(dt));
    if ptr.is_null() {
        return Err("could not allocate DateTime".to_string());
    }
    let value = unsafe { (&*ptr).clone() };
    unsafe { mux_rc_dec(ptr) };
    Ok(value)
}

/// Module-level parser used by `datetime.parse_datetime`. Module functions
/// receive primitive strings at the ABI boundary, unlike class methods.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_parse_datetime(input: *const c_char) -> *mut Value {
    if input.is_null() {
        return err("datetime input cannot be null");
    }
    let Ok(text) = std::str::from_utf8(unsafe { CStr::from_ptr(input) }.to_bytes()) else {
        return err("datetime input must be valid UTF-8");
    };
    result(sql_datetime_value(text))
}

fn callback_value(value: *mut Value) -> Option<DateValue> {
    if value.is_null() {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return None;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    lock(&VALUES).get(&id).map(|entry| entry.value.clone())
}

extern "C" fn date_equals(left: *mut Value, right: *mut Value) -> bool {
    callback_value(left) == callback_value(right)
}

extern "C" fn date_compare(left: *mut Value, right: *mut Value) -> i32 {
    match (callback_value(left), callback_value(right)) {
        (Some(left), Some(right)) => match date_value_cmp(&left, &right) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (None, None) => 0,
    }
}

fn date_value_cmp(left: &DateValue, right: &DateValue) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |value: &DateValue| match value {
        DateValue::Date(_) => 0,
        DateValue::Time(_) => 1,
        DateValue::DateTime(_) => 2,
        #[cfg(feature = "chrono-tz")]
        DateValue::ZonedDateTime(_) => 3,
        #[cfg(feature = "chrono-tz")]
        DateValue::LocalResolution { .. } => 4,
        DateValue::Instant(_) => 5,
        DateValue::Duration(_) => 6,
        DateValue::Period { .. } => 7,
    };
    if rank(left) != rank(right) {
        return rank(left).cmp(&rank(right));
    }
    match (left, right) {
        (DateValue::Date(a), DateValue::Date(b)) => a.cmp(b),
        (DateValue::Time(a), DateValue::Time(b)) => a.cmp(b),
        (DateValue::DateTime(a), DateValue::DateTime(b)) => a.cmp(b),
        #[cfg(feature = "chrono-tz")]
        (DateValue::ZonedDateTime(a), DateValue::ZonedDateTime(b)) => a.cmp(b),
        #[cfg(feature = "chrono-tz")]
        (
            DateValue::LocalResolution {
                zone: az,
                earlier: ae,
                later: al,
            },
            DateValue::LocalResolution {
                zone: bz,
                earlier: be,
                later: bl,
            },
        ) => (az.name(), ae, al).cmp(&(bz.name(), be, bl)),
        (DateValue::Instant(a), DateValue::Instant(b))
        | (DateValue::Duration(a), DateValue::Duration(b)) => a.cmp(b),
        (
            DateValue::Period {
                years: ay,
                months: am,
                days: ad,
            },
            DateValue::Period {
                years: by,
                months: bm,
                days: bd,
            },
        ) => (ay, am, ad).cmp(&(by, bm, bd)),
        _ => Ordering::Equal,
    }
}

extern "C" fn date_hash(value: *mut Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    callback_value(value).hash(&mut hasher);
    hasher.finish()
}

fn text(value: *const Value, label: &str) -> Result<String, String> {
    match unsafe { value.as_ref() } {
        Some(Value::String(text)) => Ok(text.clone()),
        _ => Err(format!("{label} must be a string")),
    }
}

fn date_parts(year: i64, month: i64, day: i64) -> Result<NaiveDate, String> {
    let year = i32::try_from(year).map_err(|_| "date year is out of range")?;
    let month = u32::try_from(month).map_err(|_| "date month must be between 1 and 12")?;
    let day = u32::try_from(day).map_err(|_| "date day must be positive")?;
    NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| "invalid calendar date".to_string())
}

fn time_parts(hour: i64, minute: i64, second: i64, nanos: i64) -> Result<NaiveTime, String> {
    let hour = u32::try_from(hour).map_err(|_| "time hour is out of range")?;
    let minute = u32::try_from(minute).map_err(|_| "time minute is out of range")?;
    let second = u32::try_from(second).map_err(|_| "time second is out of range")?;
    let nanos = u32::try_from(nanos).map_err(|_| "time nanosecond is out of range")?;
    NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)
        .ok_or_else(|| "invalid clock time".to_string())
}

fn unix_nanos(dt: ChronoDateTime<Utc>) -> Result<i128, String> {
    i128::from(dt.timestamp())
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| value.checked_add(i128::from(dt.timestamp_subsec_nanos())))
        .ok_or_else(|| "timestamp exceeds the supported range".to_string())
}

fn datetime_from_nanos(nanos: i128) -> Result<ChronoDateTime<Utc>, String> {
    let seconds = nanos.div_euclid(NANOS_PER_SECOND);
    let fraction = nanos.rem_euclid(NANOS_PER_SECOND);
    let seconds = i64::try_from(seconds).map_err(|_| "timestamp exceeds the supported range")?;
    ChronoDateTime::from_timestamp(seconds, fraction as u32)
        .ok_or_else(|| "timestamp exceeds the supported range".to_string())
}

fn scalar(result_value: Result<i64, String>) -> *mut Value {
    result(result_value.map(Value::Int))
}

fn date_last_day(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        year.checked_add(1)
            .and_then(|next_year| NaiveDate::from_ymd_opt(next_year, 1, 1))
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next.and_then(|date| date.pred_opt())
        .map_or(28, |date| date.day())
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_date_new() -> *mut Value {
    let Some(date) = NaiveDate::from_ymd_opt(1970, 1, 1) else {
        return std::ptr::null_mut();
    };
    object(*DATE_TYPE_ID, DateValue::Date(date))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_date_from_parts(
    year: i64,
    month: i64,
    day: i64,
) -> *mut Value {
    match date_parts(year, month, day) {
        Ok(date) => object_result(object(*DATE_TYPE_ID, DateValue::Date(date)), "Date"),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_date_parse(value: *const Value) -> *mut Value {
    let text = match text(value, "date") {
        Ok(text) => text,
        Err(error) => return err(error),
    };
    match NaiveDate::parse_from_str(&text, "%Y-%m-%d") {
        Ok(date) => object_result(object(*DATE_TYPE_ID, DateValue::Date(date)), "Date"),
        Err(error) => err(format!("invalid ISO date: {error}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_date_to_string(value: *const Value) -> *mut Value {
    match self::value(value, *DATE_TYPE_ID, "Date") {
        Ok(DateValue::Date(date)) => {
            mux_rc_alloc(Value::String(date.format("%Y-%m-%d").to_string()))
        }
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_date_format(
    value: *const Value,
    pattern: *const Value,
) -> *mut Value {
    let pattern = match text(pattern, "date format pattern") {
        Ok(pattern) => pattern,
        Err(error) => return err(error),
    };
    match self::value(value, *DATE_TYPE_ID, "Date") {
        Ok(DateValue::Date(date)) => result(Ok(Value::String(date.format(&pattern).to_string()))),
        Ok(_) => err("expected Date value"),
        Err(error) => err(error),
    }
}

macro_rules! date_field {
    ($name:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(value: *const Value) -> i64 {
            match self::value(value, *DATE_TYPE_ID, "Date") {
                Ok(DateValue::Date(date)) => $field(&date),
                _ => -1,
            }
        }
    };
}
date_field!(mux_datetime_date_year, |date: &NaiveDate| i64::from(
    date.year()
));
date_field!(mux_datetime_date_month, |date: &NaiveDate| i64::from(
    date.month()
));
date_field!(mux_datetime_date_day, |date: &NaiveDate| i64::from(
    date.day()
));
date_field!(mux_datetime_date_weekday, |date: &NaiveDate| i64::from(
    date.weekday().number_from_monday()
));

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_date_add_days(value: *const Value, days: i64) -> *mut Value {
    match self::value(value, *DATE_TYPE_ID, "Date") {
        Ok(DateValue::Date(date)) => match date.checked_add_signed(ChronoDuration::days(days)) {
            Some(next) => object_result(object(*DATE_TYPE_ID, DateValue::Date(next)), "Date"),
            None => err("date addition exceeds the supported range"),
        },
        Err(error) => err(error),
        _ => err("expected Date value"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_time_new() -> *mut Value {
    let Some(time) = NaiveTime::from_hms_opt(0, 0, 0) else {
        return std::ptr::null_mut();
    };
    object(*TIME_TYPE_ID, DateValue::Time(time))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_time_from_parts(
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
) -> *mut Value {
    match time_parts(hour, minute, second, nanos) {
        Ok(time) => object_result(object(*TIME_TYPE_ID, DateValue::Time(time)), "Time"),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_time_parse(value: *const Value) -> *mut Value {
    let text = match text(value, "time") {
        Ok(text) => text,
        Err(error) => return err(error),
    };
    let parsed = NaiveTime::parse_from_str(&text, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(&text, "%H:%M:%S"));
    match parsed {
        Ok(time) => object_result(object(*TIME_TYPE_ID, DateValue::Time(time)), "Time"),
        Err(error) => err(format!("invalid ISO time: {error}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_time_to_string(value: *const Value) -> *mut Value {
    match self::value(value, *TIME_TYPE_ID, "Time") {
        Ok(DateValue::Time(time)) => {
            mux_rc_alloc(Value::String(time.format("%H:%M:%S%.f").to_string()))
        }
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_time_format(
    value: *const Value,
    pattern: *const Value,
) -> *mut Value {
    let pattern = match text(pattern, "time format pattern") {
        Ok(pattern) => pattern,
        Err(error) => return err(error),
    };
    match self::value(value, *TIME_TYPE_ID, "Time") {
        Ok(DateValue::Time(time)) => result(Ok(Value::String(time.format(&pattern).to_string()))),
        Ok(_) => err("expected Time value"),
        Err(error) => err(error),
    }
}

macro_rules! time_field {
    ($name:ident, $field:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(value: *const Value) -> i64 {
            match self::value(value, *TIME_TYPE_ID, "Time") {
                Ok(DateValue::Time(time)) => $field(&time),
                _ => -1,
            }
        }
    };
}
time_field!(mux_datetime_time_hour, |time: &NaiveTime| i64::from(
    time.hour()
));
time_field!(mux_datetime_time_minute, |time: &NaiveTime| i64::from(
    time.minute()
));
time_field!(mux_datetime_time_second, |time: &NaiveTime| i64::from(
    time.second()
));
time_field!(mux_datetime_time_nanosecond, |time: &NaiveTime| i64::from(
    time.nanosecond()
));

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_datetime_new() -> *mut Value {
    let Some(datetime) = ChronoDateTime::from_timestamp(0, 0) else {
        return std::ptr::null_mut();
    };
    object(*DATETIME_TYPE_ID, DateValue::DateTime(datetime))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_datetime_now() -> *mut Value {
    object(*DATETIME_TYPE_ID, DateValue::DateTime(Utc::now()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_from_timestamp(
    seconds: i64,
    nanos: i64,
) -> *mut Value {
    if !(0..1_000_000_000).contains(&nanos) {
        return err("nanoseconds must be between 0 and 999999999");
    }
    match ChronoDateTime::from_timestamp(seconds, nanos as u32) {
        Some(dt) => object_result(
            object(*DATETIME_TYPE_ID, DateValue::DateTime(dt)),
            "DateTime",
        ),
        None => err("timestamp exceeds the supported range"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_from_date_time(
    date: *const Value,
    time: *const Value,
) -> *mut Value {
    let date = match value(date, *DATE_TYPE_ID, "Date") {
        Ok(DateValue::Date(date)) => date,
        Ok(_) => return err("expected Date value"),
        Err(error) => return err(error),
    };
    let time = match value(time, *TIME_TYPE_ID, "Time") {
        Ok(DateValue::Time(time)) => time,
        Ok(_) => return err("expected Time value"),
        Err(error) => return err(error),
    };
    let naive = date.and_time(time);
    let dt = ChronoDateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
    object_result(
        object(*DATETIME_TYPE_ID, DateValue::DateTime(dt)),
        "DateTime",
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_parse(value: *const Value) -> *mut Value {
    let text = match text(value, "datetime") {
        Ok(text) => text,
        Err(error) => return err(error),
    };
    match ChronoDateTime::parse_from_rfc3339(&text) {
        Ok(dt) => object_result(
            object(
                *DATETIME_TYPE_ID,
                DateValue::DateTime(dt.with_timezone(&Utc)),
            ),
            "DateTime",
        ),
        Err(error) => err(format!("invalid RFC 3339 datetime: {error}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_parse_pattern(
    value: *const Value,
    pattern: *const Value,
) -> *mut Value {
    let input = match text(value, "datetime") {
        Ok(input) => input,
        Err(error) => return err(error),
    };
    let pattern = match text(pattern, "datetime parse pattern") {
        Ok(pattern) => pattern,
        Err(error) => return err(error),
    };
    if let Ok(parsed) = ChronoDateTime::parse_from_str(&input, &pattern) {
        return object_result(
            object(
                *DATETIME_TYPE_ID,
                DateValue::DateTime(parsed.with_timezone(&Utc)),
            ),
            "DateTime",
        );
    }
    match chrono::NaiveDateTime::parse_from_str(&input, &pattern) {
        Ok(parsed) => object_result(
            object(
                *DATETIME_TYPE_ID,
                DateValue::DateTime(ChronoDateTime::from_naive_utc_and_offset(parsed, Utc)),
            ),
            "DateTime",
        ),
        Err(error) => err(format!("invalid datetime for pattern: {error}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_to_string(value: *const Value) -> *mut Value {
    match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(dt)) => mux_rc_alloc(Value::String(
            dt.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )),
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_format(
    value: *const Value,
    pattern: *const Value,
) -> *mut Value {
    let pattern = match text(pattern, "datetime format pattern") {
        Ok(pattern) => pattern,
        Err(error) => return err(error),
    };
    match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(datetime)) => {
            result(Ok(Value::String(datetime.format(&pattern).to_string())))
        }
        Ok(_) => err("expected DateTime value"),
        Err(error) => err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_unix_seconds(value: *const Value) -> *mut Value {
    match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(dt)) => scalar(Ok(dt.timestamp())),
        Err(error) => err(error),
        _ => err("expected DateTime value"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_unix_nanos(value: *const Value) -> *mut Value {
    match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(dt)) => scalar(unix_nanos(dt).and_then(|nanos| {
            i64::try_from(nanos).map_err(|_| "nanosecond timestamp exceeds int range".to_string())
        })),
        Err(error) => err(error),
        _ => err("expected DateTime value"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_date(value: *const Value) -> *mut Value {
    match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(dt)) => object(*DATE_TYPE_ID, DateValue::Date(dt.date_naive())),
        _ => mux_rc_alloc(Value::Unit),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_time(value: *const Value) -> *mut Value {
    match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(dt)) => object(*TIME_TYPE_ID, DateValue::Time(dt.time())),
        _ => mux_rc_alloc(Value::Unit),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_datetime_add_duration(
    value: *const Value,
    duration: *const Value,
) -> *mut Value {
    let dt = match self::value(value, *DATETIME_TYPE_ID, "DateTime") {
        Ok(DateValue::DateTime(dt)) => dt,
        Ok(_) => return err("expected DateTime value"),
        Err(error) => return err(error),
    };
    let nanos = match self::value(duration, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(nanos)) => nanos,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    let current = match unix_nanos(dt) {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    match current
        .checked_add(nanos)
        .and_then(|nanos| datetime_from_nanos(nanos).ok())
    {
        Some(next) => object_result(
            object(*DATETIME_TYPE_ID, DateValue::DateTime(next)),
            "DateTime",
        ),
        None => err("datetime addition exceeds the supported range"),
    }
}

#[cfg(feature = "chrono-tz")]
fn timezone(value: *const Value) -> Result<Tz, String> {
    let name = text(value, "timezone")?;
    name.parse::<Tz>()
        .map_err(|_| format!("unknown IANA timezone '{name}'"))
}

#[cfg(feature = "chrono-tz")]
fn zoned_value(datetime: ChronoDateTime<Tz>) -> Result<Value, String> {
    let object = object(*ZONED_DATETIME_TYPE_ID, DateValue::ZonedDateTime(datetime));
    if object.is_null() {
        return Err("could not allocate ZonedDateTime".to_string());
    }
    let Value::Object(reference) = (unsafe { &*object }) else {
        unsafe { mux_rc_dec(object) };
        return Err("could not allocate ZonedDateTime".to_string());
    };
    let value = Value::Object(reference.clone());
    unsafe { mux_rc_dec(object) };
    Ok(value)
}

#[cfg(feature = "chrono-tz")]
fn optional_zoned(datetime: Option<ChronoDateTime<Tz>>) -> Result<Value, String> {
    datetime
        .map(zoned_value)
        .transpose()
        .map(|value| Value::Optional(value.map(Box::new)))
}

#[cfg(feature = "chrono-tz")]
fn local_resolution(
    date: *const Value,
    time: *const Value,
    zone: *const Value,
) -> Result<DateValue, String> {
    let DateValue::Date(date) = self::value(date, *DATE_TYPE_ID, "Date")? else {
        return Err("expected Date value".to_string());
    };
    let DateValue::Time(time) = self::value(time, *TIME_TYPE_ID, "Time")? else {
        return Err("expected Time value".to_string());
    };
    let zone = timezone(zone)?;
    let local = date.and_time(time);
    let (earlier, later) = match zone.from_local_datetime(&local) {
        LocalResult::Single(value) => (Some(value), None),
        LocalResult::Ambiguous(earlier, later) => (Some(earlier), Some(later)),
        LocalResult::None => (None, None),
    };
    Ok(DateValue::LocalResolution {
        zone,
        earlier,
        later,
    })
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_zoned_datetime_new() -> *mut Value {
    let Some(utc) = ChronoDateTime::<Utc>::from_timestamp(0, 0) else {
        return err("failed to construct Unix epoch");
    };
    object(
        *ZONED_DATETIME_TYPE_ID,
        DateValue::ZonedDateTime(chrono_tz::UTC.from_utc_datetime(&utc.naive_utc())),
    )
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_from_local(
    date: *const Value,
    time: *const Value,
    zone: *const Value,
) -> *mut Value {
    match local_resolution(date, time, zone) {
        Ok(DateValue::LocalResolution {
            earlier: Some(value),
            later: None,
            ..
        }) => result(zoned_value(value)),
        Ok(DateValue::LocalResolution {
            earlier: Some(_),
            later: Some(_),
            zone,
        }) => err(format!("ambiguous local time in {}", zone.name())),
        Ok(DateValue::LocalResolution { zone, .. }) => {
            err(format!("nonexistent local time in {}", zone.name()))
        }
        Ok(_) => err("could not resolve local time"),
        Err(error) => err(error),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_resolve_local(
    date: *const Value,
    time: *const Value,
    zone: *const Value,
) -> *mut Value {
    match local_resolution(date, time, zone) {
        Ok(value) => result(value_from_date_value(*LOCAL_RESOLUTION_TYPE_ID, value)),
        Err(error) => err(error),
    }
}

#[cfg(feature = "chrono-tz")]
fn value_from_date_value(type_id: TypeId, value: DateValue) -> Result<Value, String> {
    let object = object(type_id, value);
    if object.is_null() {
        return Err("could not allocate datetime resolution".to_string());
    }
    let Value::Object(reference) = (unsafe { &*object }) else {
        unsafe { mux_rc_dec(object) };
        return Err("could not allocate datetime resolution".to_string());
    };
    let value = Value::Object(reference.clone());
    unsafe { mux_rc_dec(object) };
    Ok(value)
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_from_instant(
    instant: *const Value,
    zone: *const Value,
) -> *mut Value {
    let nanos = match self::value(instant, *INSTANT_TYPE_ID, "Instant") {
        Ok(DateValue::Instant(value)) => value,
        Ok(_) => return err("expected Instant value"),
        Err(error) => return err(error),
    };
    let zone = match timezone(zone) {
        Ok(zone) => zone,
        Err(error) => return err(error),
    };
    match datetime_from_nanos(nanos) {
        Ok(utc) => result(zoned_value(zone.from_utc_datetime(&utc.naive_utc()))),
        Err(error) => err(error),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_parse(value: *const Value) -> *mut Value {
    let input = match text(value, "zoned datetime") {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    let Some((rfc3339, zone_name)) = input
        .strip_suffix(']')
        .and_then(|value| value.rsplit_once('['))
    else {
        return err("zoned datetime must end with an IANA zone in brackets");
    };
    let Ok(zone) = zone_name.parse::<Tz>() else {
        return err(format!("unknown IANA timezone '{zone_name}'"));
    };
    match ChronoDateTime::parse_from_rfc3339(rfc3339) {
        Ok(parsed) => {
            let utc = parsed.with_timezone(&Utc);
            result(zoned_value(zone.from_utc_datetime(&utc.naive_utc())))
        }
        Err(error) => err(format!("invalid RFC 3339 zoned datetime: {error}")),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_to_string(value: *const Value) -> *mut Value {
    match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => mux_rc_alloc(Value::String(format!(
            "{}[{}]",
            value.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            value.timezone().name()
        ))),
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_zone(value: *const Value) -> *mut Value {
    match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => {
            mux_rc_alloc(Value::String(value.timezone().name().to_string()))
        }
        _ => mux_rc_alloc(Value::String(String::new())),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_offset_seconds(
    value: *const Value,
) -> *mut Value {
    match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => {
            scalar(Ok(i64::from(value.offset().fix().local_minus_utc())))
        }
        Err(error) => err(error),
        _ => err("expected ZonedDateTime value"),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_date(value: *const Value) -> *mut Value {
    match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => {
            object(*DATE_TYPE_ID, DateValue::Date(value.date_naive()))
        }
        _ => mux_rc_alloc(Value::Unit),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_time(value: *const Value) -> *mut Value {
    match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => object(*TIME_TYPE_ID, DateValue::Time(value.time())),
        _ => mux_rc_alloc(Value::Unit),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_instant(value: *const Value) -> *mut Value {
    match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => match unix_nanos(value.with_timezone(&Utc)) {
            Ok(nanos) => object_result(
                object(*INSTANT_TYPE_ID, DateValue::Instant(nanos)),
                "Instant",
            ),
            Err(error) => err(error),
        },
        Err(error) => err(error),
        _ => err("expected ZonedDateTime value"),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_zoned_datetime_add_duration(
    value: *const Value,
    duration: *const Value,
) -> *mut Value {
    let value = match self::value(value, *ZONED_DATETIME_TYPE_ID, "ZonedDateTime") {
        Ok(DateValue::ZonedDateTime(value)) => value,
        Ok(_) => return err("expected ZonedDateTime value"),
        Err(error) => return err(error),
    };
    let nanos = match self::value(duration, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => value,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    let seconds = nanos.div_euclid(NANOS_PER_SECOND);
    let fraction = nanos.rem_euclid(NANOS_PER_SECOND);
    let Ok(seconds) = i64::try_from(seconds) else {
        return err("datetime addition exceeds the supported range");
    };
    let next = value
        .checked_add_signed(ChronoDuration::seconds(seconds))
        .and_then(|value| value.checked_add_signed(ChronoDuration::nanoseconds(fraction as i64)));
    match next {
        Some(next) => result(zoned_value(next)),
        None => err("datetime addition exceeds the supported range"),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_local_resolution_kind(value: *const Value) -> *mut Value {
    let kind = match self::value(value, *LOCAL_RESOLUTION_TYPE_ID, "LocalResolution") {
        Ok(DateValue::LocalResolution {
            earlier: Some(_),
            later: None,
            ..
        }) => "unique",
        Ok(DateValue::LocalResolution {
            earlier: Some(_),
            later: Some(_),
            ..
        }) => "ambiguous",
        Ok(DateValue::LocalResolution { .. }) => "nonexistent",
        _ => "",
    };
    mux_rc_alloc(Value::String(kind.to_string()))
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_local_resolution_earlier(value: *const Value) -> *mut Value {
    match self::value(value, *LOCAL_RESOLUTION_TYPE_ID, "LocalResolution") {
        Ok(DateValue::LocalResolution {
            earlier: Some(value),
            ..
        }) => mux_rc_alloc(optional_zoned(Some(value)).unwrap_or(Value::Optional(None))),
        _ => mux_rc_alloc(Value::Optional(None)),
    }
}

#[cfg(feature = "chrono-tz")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_local_resolution_later(value: *const Value) -> *mut Value {
    match self::value(value, *LOCAL_RESOLUTION_TYPE_ID, "LocalResolution") {
        Ok(DateValue::LocalResolution {
            later: Some(value), ..
        }) => mux_rc_alloc(optional_zoned(Some(value)).unwrap_or(Value::Optional(None))),
        _ => mux_rc_alloc(Value::Optional(None)),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_instant_new() -> *mut Value {
    object(*INSTANT_TYPE_ID, DateValue::Instant(0))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_instant_now() -> *mut Value {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |error| -(error.duration().as_nanos() as i128),
        |duration| duration.as_nanos() as i128,
    );
    object(*INSTANT_TYPE_ID, DateValue::Instant(nanos))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_instant_from_unix_nanos(nanos: i64) -> *mut Value {
    object_result(
        object(*INSTANT_TYPE_ID, DateValue::Instant(i128::from(nanos))),
        "Instant",
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_instant_unix_nanos(value: *const Value) -> *mut Value {
    match self::value(value, *INSTANT_TYPE_ID, "Instant") {
        Ok(DateValue::Instant(nanos)) => {
            scalar(i64::try_from(nanos).map_err(|_| "instant exceeds int range".to_string()))
        }
        Err(error) => err(error),
        _ => err("expected Instant value"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_instant_add_duration(
    value: *const Value,
    duration: *const Value,
) -> *mut Value {
    let instant = match self::value(value, *INSTANT_TYPE_ID, "Instant") {
        Ok(DateValue::Instant(value)) => value,
        Ok(_) => return err("expected Instant value"),
        Err(error) => return err(error),
    };
    let duration = match self::value(duration, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => value,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    match instant.checked_add(duration) {
        Some(value) => object_result(
            object(*INSTANT_TYPE_ID, DateValue::Instant(value)),
            "Instant",
        ),
        None => err("instant addition exceeds the supported range"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_instant_duration_since(
    value: *const Value,
    other: *const Value,
) -> *mut Value {
    let value = match self::value(value, *INSTANT_TYPE_ID, "Instant") {
        Ok(DateValue::Instant(value)) => value,
        Ok(_) => return err("expected Instant value"),
        Err(error) => return err(error),
    };
    let other = match self::value(other, *INSTANT_TYPE_ID, "Instant") {
        Ok(DateValue::Instant(value)) => value,
        Ok(_) => return err("expected Instant value"),
        Err(error) => return err(error),
    };
    match value.checked_sub(other) {
        Some(value) => object_result(
            object(*DURATION_TYPE_ID, DateValue::Duration(value)),
            "Duration",
        ),
        None => err("instant difference exceeds the supported range"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_duration_new() -> *mut Value {
    object(*DURATION_TYPE_ID, DateValue::Duration(0))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_duration_from_parts(seconds: i64, nanos: i64) -> *mut Value {
    let value = i128::from(seconds)
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| value.checked_add(i128::from(nanos)));
    match value {
        Some(value) => object_result(
            object(*DURATION_TYPE_ID, DateValue::Duration(value)),
            "Duration",
        ),
        None => err("duration exceeds the supported range"),
    }
}

macro_rules! duration_from_unit {
    ($name:ident, $unit:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(value: i64) -> *mut Value {
            match i128::from(value).checked_mul($unit) {
                Some(value) => object_result(
                    object(*DURATION_TYPE_ID, DateValue::Duration(value)),
                    "Duration",
                ),
                None => err("duration exceeds the supported range"),
            }
        }
    };
}
duration_from_unit!(mux_datetime_duration_from_seconds, NANOS_PER_SECOND);
duration_from_unit!(mux_datetime_duration_from_millis, NANOS_PER_MILLI);
duration_from_unit!(mux_datetime_duration_from_micros, NANOS_PER_MICRO);
duration_from_unit!(mux_datetime_duration_from_nanos, 1);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_duration_to_nanos(value: *const Value) -> *mut Value {
    match self::value(value, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => {
            scalar(i64::try_from(value).map_err(|_| "duration exceeds int range".to_string()))
        }
        Err(error) => err(error),
        _ => err("expected Duration value"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_duration_add(
    value: *const Value,
    other: *const Value,
) -> *mut Value {
    let value = match self::value(value, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => value,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    let other = match self::value(other, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => value,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    match value.checked_add(other) {
        Some(value) => object_result(
            object(*DURATION_TYPE_ID, DateValue::Duration(value)),
            "Duration",
        ),
        None => err("duration addition exceeds the supported range"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_duration_sub(
    value: *const Value,
    other: *const Value,
) -> *mut Value {
    let value = match self::value(value, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => value,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    let other = match self::value(other, *DURATION_TYPE_ID, "Duration") {
        Ok(DateValue::Duration(value)) => value,
        Ok(_) => return err("expected Duration value"),
        Err(error) => return err(error),
    };
    match value.checked_sub(other) {
        Some(value) => object_result(
            object(*DURATION_TYPE_ID, DateValue::Duration(value)),
            "Duration",
        ),
        None => err("duration subtraction exceeds the supported range"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_datetime_period_new() -> *mut Value {
    object(
        *PERIOD_TYPE_ID,
        DateValue::Period {
            years: 0,
            months: 0,
            days: 0,
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_period_from_parts(
    years: i64,
    months: i64,
    days: i64,
) -> *mut Value {
    object_result(
        object(
            *PERIOD_TYPE_ID,
            DateValue::Period {
                years,
                months,
                days,
            },
        ),
        "Period",
    )
}

macro_rules! period_field {
    ($name:ident, $field:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(value: *const Value) -> i64 {
            match self::value(value, *PERIOD_TYPE_ID, "Period") {
                Ok(DateValue::Period { $field, .. }) => $field,
                _ => 0,
            }
        }
    };
}
period_field!(mux_datetime_period_years, years);
period_field!(mux_datetime_period_months, months);
period_field!(mux_datetime_period_days, days);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_datetime_period_add_to_date(
    value: *const Value,
    date: *const Value,
) -> *mut Value {
    let period = match self::value(value, *PERIOD_TYPE_ID, "Period") {
        Ok(DateValue::Period {
            years,
            months,
            days,
        }) => (years, months, days),
        Ok(_) => return err("expected Period value"),
        Err(error) => return err(error),
    };
    let date = match self::value(date, *DATE_TYPE_ID, "Date") {
        Ok(DateValue::Date(date)) => date,
        Ok(_) => return err("expected Date value"),
        Err(error) => return err(error),
    };
    let (years, months, days) = period;
    let Some(total_months) = i64::from(date.year())
        .checked_mul(12)
        .and_then(|value| value.checked_add(i64::from(date.month0())))
        .and_then(|value| value.checked_add(years.checked_mul(12)?))
        .and_then(|value| value.checked_add(months))
    else {
        return err("period month arithmetic exceeds the supported range");
    };
    let target_year = total_months.div_euclid(12);
    let target_month = (total_months.rem_euclid(12) + 1) as u32;
    let Ok(target_year) = i32::try_from(target_year) else {
        return err("period result year is out of range");
    };
    let target_day = date.day().min(date_last_day(target_year, target_month));
    let Some(shifted) = NaiveDate::from_ymd_opt(target_year, target_month, target_day)
        .and_then(|date| date.checked_add_signed(ChronoDuration::days(days)))
    else {
        return err("period result exceeds the supported range");
    };
    object_result(object(*DATE_TYPE_ID, DateValue::Date(shifted)), "Date")
}
