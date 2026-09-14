use mux_runtime::datetime_types::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::Value;

fn string(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

unsafe fn ok_data(result: *mut Value) -> *mut Value {
    assert!(
        mux_result_is_ok(result),
        "expected Ok result: {:?}",
        &*result
    );
    let data = mux_result_data(result);
    assert!(mux_rc_dec(result));
    data
}

#[test]
fn date_and_period_preserve_calendar_rules() {
    unsafe {
        let date = ok_data(mux_datetime_date_from_parts(2024, 2, 29));
        assert_eq!(mux_datetime_date_year(date), 2024);
        assert_eq!(mux_datetime_date_month(date), 2);
        assert_eq!(mux_datetime_date_day(date), 29);
        let rendered = mux_datetime_date_to_string(date);
        assert!(matches!(&*rendered, Value::String(value) if value == "2024-02-29"));
        assert!(mux_rc_dec(rendered));

        let period = ok_data(mux_datetime_period_from_parts(0, 1, 0));
        let shifted = ok_data(mux_datetime_period_add_to_date(period, date));
        let rendered = mux_datetime_date_to_string(shifted);
        assert!(matches!(&*rendered, Value::String(value) if value == "2024-03-29"));
        assert!(mux_rc_dec(rendered));

        let input = string("2024-12-31");
        let parsed = ok_data(mux_datetime_date_parse(input));
        assert_eq!(mux_datetime_date_weekday(parsed), 2);
        assert!(mux_rc_dec(input));
        assert!(mux_rc_dec(parsed));
        assert!(mux_rc_dec(shifted));
        assert!(mux_rc_dec(period));
        assert!(mux_rc_dec(date));
    }
}

#[test]
fn time_datetime_and_duration_are_checked() {
    unsafe {
        let time = ok_data(mux_datetime_time_from_parts(12, 34, 56, 123_000_000));
        assert_eq!(mux_datetime_time_hour(time), 12);
        assert_eq!(mux_datetime_time_nanosecond(time), 123_000_000);
        let date = ok_data(mux_datetime_date_from_parts(2025, 1, 2));
        let datetime = ok_data(mux_datetime_datetime_from_date_time(date, time));
        let rendered = mux_datetime_datetime_to_string(datetime);
        assert!(matches!(&*rendered, Value::String(value) if value == "2025-01-02T12:34:56.123Z"));
        assert!(mux_rc_dec(rendered));
        let duration = ok_data(mux_datetime_duration_from_millis(1500));
        let total = ok_data(mux_datetime_duration_to_nanos(duration));
        assert!(matches!(&*total, Value::Int(1_500_000_000)));
        assert!(mux_rc_dec(total));
        let advanced = ok_data(mux_datetime_datetime_add_duration(datetime, duration));
        let seconds = ok_data(mux_datetime_datetime_unix_seconds(advanced));
        assert!(matches!(&*seconds, Value::Int(1_735_821_297)));
        assert!(mux_rc_dec(seconds));
        assert!(mux_rc_dec(advanced));
        assert!(mux_rc_dec(duration));
        assert!(mux_rc_dec(datetime));
        assert!(mux_rc_dec(date));
        assert!(mux_rc_dec(time));
    }
}

#[test]
fn instants_support_signed_differences() {
    unsafe {
        let start = ok_data(mux_datetime_instant_from_unix_nanos(1_000));
        let duration = ok_data(mux_datetime_duration_from_nanos(250));
        let end = ok_data(mux_datetime_instant_add_duration(start, duration));
        let difference = ok_data(mux_datetime_instant_duration_since(end, start));
        let nanos = ok_data(mux_datetime_duration_to_nanos(difference));
        assert!(matches!(&*nanos, Value::Int(250)));
        assert!(mux_rc_dec(nanos));
        assert!(mux_rc_dec(difference));
        assert!(mux_rc_dec(end));
        assert!(mux_rc_dec(duration));
        assert!(mux_rc_dec(start));
    }
}

#[cfg(feature = "chrono-tz")]
#[test]
fn zoned_datetimes_report_dst_resolution_without_silent_choices() {
    unsafe {
        let zone = string("America/New_York");

        let date = ok_data(mux_datetime_date_from_parts(2024, 1, 15));
        let time = ok_data(mux_datetime_time_from_parts(12, 0, 0, 0));
        let unique = ok_data(mux_datetime_zoned_datetime_resolve_local(date, time, zone));
        let kind = mux_datetime_local_resolution_kind(unique);
        assert!(matches!(&*kind, Value::String(value) if value == "unique"));
        assert!(mux_rc_dec(kind));
        let earlier = mux_datetime_local_resolution_earlier(unique);
        assert!(matches!(&*earlier, Value::Optional(Some(_))));
        assert!(mux_rc_dec(earlier));
        let later = mux_datetime_local_resolution_later(unique);
        assert!(matches!(&*later, Value::Optional(None)));
        assert!(mux_rc_dec(later));
        assert!(mux_rc_dec(unique));
        assert!(mux_rc_dec(date));
        assert!(mux_rc_dec(time));

        let date = ok_data(mux_datetime_date_from_parts(2024, 1, 15));
        let time = ok_data(mux_datetime_time_from_parts(12, 0, 0, 0));
        let zoned = ok_data(mux_datetime_zoned_datetime_from_local(date, time, zone));
        let offset = ok_data(mux_datetime_zoned_datetime_offset_seconds(zoned));
        assert!(matches!(&*offset, Value::Int(-18_000)));
        assert!(mux_rc_dec(offset));
        assert!(mux_rc_dec(zoned));
        assert!(mux_rc_dec(date));
        assert!(mux_rc_dec(time));

        let date = ok_data(mux_datetime_date_from_parts(2024, 11, 3));
        let time = ok_data(mux_datetime_time_from_parts(1, 30, 0, 0));
        let ambiguous = ok_data(mux_datetime_zoned_datetime_resolve_local(date, time, zone));
        let kind = mux_datetime_local_resolution_kind(ambiguous);
        assert!(matches!(&*kind, Value::String(value) if value == "ambiguous"));
        assert!(mux_rc_dec(kind));
        let earlier = mux_datetime_local_resolution_earlier(ambiguous);
        let later = mux_datetime_local_resolution_later(ambiguous);
        assert!(matches!(&*earlier, Value::Optional(Some(_))));
        assert!(matches!(&*later, Value::Optional(Some(_))));
        assert!(mux_rc_dec(earlier));
        assert!(mux_rc_dec(later));
        assert!(mux_rc_dec(ambiguous));
        let rejected = mux_datetime_zoned_datetime_from_local(date, time, zone);
        assert!(!mux_result_is_ok(rejected));
        assert!(mux_rc_dec(rejected));
        assert!(mux_rc_dec(date));
        assert!(mux_rc_dec(time));

        let date = ok_data(mux_datetime_date_from_parts(2024, 3, 10));
        let time = ok_data(mux_datetime_time_from_parts(2, 30, 0, 0));
        let nonexistent = ok_data(mux_datetime_zoned_datetime_resolve_local(date, time, zone));
        let kind = mux_datetime_local_resolution_kind(nonexistent);
        assert!(matches!(&*kind, Value::String(value) if value == "nonexistent"));
        assert!(mux_rc_dec(kind));
        let earlier = mux_datetime_local_resolution_earlier(nonexistent);
        let later = mux_datetime_local_resolution_later(nonexistent);
        assert!(matches!(&*earlier, Value::Optional(None)));
        assert!(matches!(&*later, Value::Optional(None)));
        assert!(mux_rc_dec(earlier));
        assert!(mux_rc_dec(later));
        assert!(mux_rc_dec(nonexistent));

        let instant = ok_data(mux_datetime_instant_from_unix_nanos(
            1_704_067_200_000_000_000,
        ));
        let zoned = ok_data(mux_datetime_zoned_datetime_from_instant(instant, zone));
        let rendered = mux_datetime_zoned_datetime_to_string(zoned);
        assert!(
            matches!(&*rendered, Value::String(value) if value.ends_with("[America/New_York]"))
        );
        let parsed = ok_data(mux_datetime_zoned_datetime_parse(rendered));
        let parsed_zone = mux_datetime_zoned_datetime_zone(parsed);
        assert!(matches!(&*parsed_zone, Value::String(value) if value == "America/New_York"));
        assert!(mux_rc_dec(parsed_zone));
        assert!(mux_rc_dec(parsed));
        assert!(mux_rc_dec(rendered));
        let zone_value = mux_datetime_zoned_datetime_zone(zoned);
        assert!(matches!(&*zone_value, Value::String(value) if value == "America/New_York"));
        assert!(mux_rc_dec(zone_value));
        assert!(mux_rc_dec(zoned));
        assert!(mux_rc_dec(instant));
        assert!(mux_rc_dec(date));
        assert!(mux_rc_dec(time));
        assert!(mux_rc_dec(zone));
    }
}
