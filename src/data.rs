use crate::Value;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

fn err_result(message: &str) -> *mut Value {
    let Ok(msg) = CString::new(message) else {
        return std::ptr::null_mut();
    };
    unsafe { crate::result::mux_result_err_str(msg.as_ptr()) }
}

fn csv_parse_error_result(error: impl std::fmt::Display) -> *mut Value {
    err_result(&format!("CSV parse error: {}", error))
}

fn read_input_string(input: *const c_char) -> Result<String, *mut Value> {
    if input.is_null() {
        return Err(err_result("null input"));
    }

    let s = unsafe { CStr::from_ptr(input) }
        .to_string_lossy()
        .into_owned();
    Ok(s)
}

fn record_to_value_list(record: &csv::StringRecord) -> Value {
    let row: Vec<Value> = record
        .iter()
        .map(|field| Value::String(field.to_string()))
        .collect();
    Value::List(row)
}

fn collect_rows(reader: &mut csv::Reader<&[u8]>) -> Result<Vec<Value>, *mut Value> {
    let mut rows = Vec::new();

    for result in reader.records() {
        match result {
            Ok(record) => rows.push(record_to_value_list(&record)),
            Err(error) => return Err(csv_parse_error_result(error)),
        }
    }

    Ok(rows)
}

#[allow(clippy::mutable_key_type)]
fn csv_value(headers: Value, rows: Vec<Value>) -> Value {
    let mut map = crate::ordered::OrderedMap::new();
    map.insert(Value::String("headers".to_string()), headers);
    map.insert(Value::String("rows".to_string()), Value::List(rows));
    Value::Map(map)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
#[allow(clippy::mutable_key_type)]
pub extern "C" fn mux_csv_parse(input: *const c_char) -> *mut Value {
    let s = match read_input_string(input) {
        Ok(input) => input,
        Err(error) => return error,
    };

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(s.as_bytes());
    let rows = match collect_rows(&mut reader) {
        Ok(rows) => rows,
        Err(error) => return error,
    };

    let csv_value = csv_value(Value::List(Vec::new()), rows);

    // Wrap directly to avoid leaking the intermediate allocation: the
    // mux_result_ok_value helper clones its argument without consuming it.
    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(csv_value))))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
#[allow(clippy::mutable_key_type)]
pub extern "C" fn mux_csv_parse_with_headers(input: *const c_char) -> *mut Value {
    let s = match read_input_string(input) {
        Ok(input) => input,
        Err(error) => return error,
    };

    let mut reader = csv::Reader::from_reader(s.as_bytes());

    let headers = match reader.headers() {
        Ok(hdr) => {
            let header_values: Vec<Value> = hdr
                .iter()
                .map(|field| Value::String(field.to_string()))
                .collect();
            Value::List(header_values)
        }
        Err(error) => return csv_parse_error_result(error),
    };

    let rows = match collect_rows(&mut reader) {
        Ok(rows) => rows,
        Err(error) => return error,
    };

    let csv_value = csv_value(headers, rows);

    // Wrap directly to avoid leaking the intermediate allocation (see
    // mux_csv_parse).
    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(csv_value))))
}

/// A parsed CSV as one map per row, keyed by header name.
///
/// The parsed form keeps headers and rows apart - headers are a list, rows are
/// a list of lists - so reading a named column means finding its index first.
/// Doing that per field, per row, in generated code would be a nested loop over
/// data the runtime already holds; this pairs them once.
///
/// Every cell stays a string, because CSV has no types. Deciding that a column
/// is a number is the reader's job, not this function's.
///
/// A repeated header is REJECTED, naming the column. Keying by name cannot
/// represent two columns called the same thing, so one of them would have to be
/// dropped - and dropping a whole source column without saying so is the one
/// answer a reader cannot recover from. Which one survived would also be an
/// arbitrary rule to remember.
///
/// A row with fewer cells than there are headers simply omits the missing keys,
/// which a typed reader then reports as a missing required field - the same
/// answer it gives for an absent JSON field, rather than a second vocabulary
/// for the same problem.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_csv_rows_as_maps(val: *const Value) -> *mut Value {
    if val.is_null() {
        return csv_rows_error("no CSV table to read");
    }
    let Value::Map(table) = (unsafe { &*val }) else {
        return csv_rows_error("expected a parsed CSV table");
    };
    let (Some(Value::List(headers)), Some(Value::List(rows))) = (
        table.get(&Value::String("headers".to_string())),
        table.get(&Value::String("rows".to_string())),
    ) else {
        return csv_rows_error("expected a parsed CSV table with headers and rows");
    };

    // Reject before pairing anything, so the answer does not depend on which
    // row the duplicate first shows up in.
    let mut seen: Vec<&String> = Vec::with_capacity(headers.len());
    for header in headers {
        if let Value::String(name) = header {
            if seen.contains(&name) {
                return csv_rows_error(&format!(
                    "duplicate column '{name}': rows cannot be keyed by name when a header repeats"
                ));
            }
            seen.push(name);
        }
    }

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::List(cells) = row else {
            continue;
        };
        let mut entry = crate::ordered::OrderedMap::new();
        for (header, cell) in headers.iter().zip(cells.iter()) {
            let Value::String(name) = header else {
                continue;
            };
            let key = Value::String(name.clone());
            // A repeated header keeps the FIRST column. Keying by name cannot
            // represent two columns called the same thing, and letting the
            // later cell overwrite the earlier one drops a whole column from
            // every row with nothing to say so.
            if entry.get(&key).is_none() {
                entry.insert(key, cell.clone());
            }
        }
        out.push(Value::Map(entry));
    }

    crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(Value::List(out)))))
}

fn csv_rows_error(message: &str) -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Result(Err(Box::new(Value::String(
        message.to_string(),
    )))))
}

/// The table as CSV text, always.
///
/// The total counterpart to `mux_csv_to_string`, which returns a `result`
/// because it validates the shape. Same reasoning as `mux_json_to_string`: a
/// `Csv` that exists came from the parser and is well formed, so the failing
/// branch is unreachable - and is still given an answer rather than a panic.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_csv_render(val: *const Value) -> *mut Value {
    let text = if val.is_null() {
        String::new()
    } else if let Ok((headers, rows)) = validate_and_extract_csv(unsafe { &*val }) {
        build_csv_string(&headers, &rows, true)
    } else {
        debug_assert!(false, "a Csv value that is not a well formed table");
        String::new()
    };
    crate::refcount::mux_rc_alloc(Value::String(text))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_csv_to_string(val: *const Value) -> *mut Value {
    if val.is_null() {
        let msg = CString::new("null input").unwrap();
        unsafe {
            return crate::result::mux_result_err_str(msg.as_ptr());
        }
    }

    let v = unsafe { &*val };

    match validate_and_extract_csv(v) {
        Ok((headers, rows)) => {
            let csv_string = build_csv_string(&headers, &rows, true);
            // Wrap directly to avoid leaking the intermediate allocation (see
            // mux_csv_parse).
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(Value::String(csv_string)))))
        }
        Err(e) => {
            let msg = CString::new(e).unwrap();
            unsafe { crate::result::mux_result_err_str(msg.as_ptr()) }
        }
    }
}

fn validate_and_extract_csv(val: &Value) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    match val {
        Value::Map(map) => {
            let headers_val = map
                .get(&Value::String("headers".to_string()))
                .ok_or("missing 'headers' key")?;

            let rows_val = map
                .get(&Value::String("rows".to_string()))
                .ok_or("missing 'rows' key")?;

            let headers = extract_string_list(headers_val)?;
            let rows = extract_row_list(rows_val)?;

            Ok((headers, rows))
        }
        _ => Err("value is not a map".to_string()),
    }
}

fn extract_string_list(val: &Value) -> Result<Vec<String>, String> {
    match val {
        Value::List(list) => {
            let mut result = Vec::new();
            for item in list {
                match item {
                    Value::String(s) => result.push(s.clone()),
                    _ => return Err("headers contain non-string value".to_string()),
                }
            }
            Ok(result)
        }
        _ => Err("headers is not a list".to_string()),
    }
}

fn extract_row_list(val: &Value) -> Result<Vec<Vec<String>>, String> {
    match val {
        Value::List(rows) => {
            let mut result = Vec::new();
            for row_val in rows {
                match row_val {
                    Value::List(row) => {
                        let mut row_strings = Vec::new();
                        for field in row {
                            match field {
                                Value::String(s) => row_strings.push(s.clone()),
                                _ => return Err("row contains non-string field".to_string()),
                            }
                        }
                        result.push(row_strings);
                    }
                    _ => return Err("rows contain non-list item".to_string()),
                }
            }
            Ok(result)
        }
        _ => Err("rows is not a list".to_string()),
    }
}

fn build_csv_string(headers: &[String], rows: &[Vec<String>], include_headers: bool) -> String {
    let mut output = Vec::new();
    {
        let mut wtr = csv::Writer::from_writer(&mut output);

        if include_headers && !headers.is_empty() {
            wtr.write_record(headers)
                .unwrap_or_else(|e| eprintln!("CSV header write error: {e}"));
        }

        for row in rows {
            wtr.write_record(row)
                .unwrap_or_else(|e| eprintln!("CSV row write error: {e}"));
        }

        wtr.flush()
            .unwrap_or_else(|e| eprintln!("CSV flush error: {e}"));
    }
    String::from_utf8(output).unwrap_or_else(|_| "invalid UTF-8 in CSV output".to_string())
}
