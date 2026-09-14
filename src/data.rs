#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::{TypeId, Value};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ffi::CStr;
use std::io::{self, Cursor, Read, Write};
use std::os::raw::c_char;
use std::sync::{LazyLock, Mutex};

const MAX_CSV_INPUT_BYTES: usize = 16 * 1024 * 1024;

fn err_result(message: &str) -> *mut Value {
    crate::std::csv_result_err(message.to_owned())
}

fn csv_parse_error_result(error: impl std::fmt::Display) -> *mut Value {
    err_result(&format!("CSV parse error: {error}"))
}

/// Read a caller-owned NUL-terminated C string.
///
/// # Safety
/// A non-null `input` must point to a valid, NUL-terminated C string that
/// remains alive for the duration of this call.
unsafe fn read_input_string(input: *const c_char) -> Result<String, *mut Value> {
    if input.is_null() {
        return Err(err_result("null input"));
    }

    let bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
    if bytes.len() > MAX_CSV_INPUT_BYTES {
        return Err(err_result("CSV input exceeds the 16 MiB limit"));
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| err_result("CSV input must be valid UTF-8"))
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

fn parse_csv_reader(reader: &mut csv::Reader<&[u8]>) -> Result<Value, *mut Value> {
    let headers = if reader.has_headers() {
        match reader.headers() {
            Ok(record) => Value::List(
                record
                    .iter()
                    .map(|field| Value::String(field.to_string()))
                    .collect(),
            ),
            Err(error) => return Err(csv_parse_error_result(error)),
        }
    } else {
        Value::List(Vec::new())
    };
    let rows = collect_rows(reader)?;
    Ok(csv_value(headers, rows))
}

fn csv_option_byte(value: i64, name: &str) -> Result<u8, *mut Value> {
    let Ok(value) = u8::try_from(value) else {
        return Err(csv_parse_error_result(format!(
            "{name} must be an ASCII byte"
        )));
    };
    if value > 0x7f || value == 0 || value == b'\r' || value == b'\n' {
        return Err(csv_parse_error_result(format!(
            "{name} must be an ASCII byte other than NUL, CR, or LF"
        )));
    }
    Ok(value)
}

fn parse_csv_with_options(
    input: *const c_char,
    delimiter: i64,
    quote: i64,
    has_headers: i32,
    trim: i32,
    flexible: i32,
) -> *mut Value {
    let text = match unsafe { read_input_string(input) } {
        Ok(text) => text,
        Err(error) => return error,
    };
    let delimiter = match csv_option_byte(delimiter, "delimiter") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let quote = match csv_option_byte(quote, "quote") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let mut builder = csv::ReaderBuilder::new();
    builder
        .delimiter(delimiter)
        .quote(quote)
        .has_headers(has_headers != 0)
        .flexible(flexible != 0)
        .trim(if trim != 0 {
            csv::Trim::All
        } else {
            csv::Trim::None
        });
    let mut reader = builder.from_reader(text.as_bytes());
    match parse_csv_reader(&mut reader) {
        Ok(table) => crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(table)))),
        Err(error) => error,
    }
}

#[allow(clippy::mutable_key_type)]
fn csv_value(headers: Value, rows: Vec<Value>) -> Value {
    let mut map = crate::ordered::OrderedMap::new();
    map.insert(Value::String("headers".to_string()), headers);
    map.insert(Value::String("rows".to_string()), Value::List(rows));
    Value::Map(map)
}

#[unsafe(no_mangle)]
#[allow(clippy::mutable_key_type)]
/// Parse CSV text without treating the first row as headers.
///
/// # Safety
/// `input` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
pub unsafe extern "C" fn mux_csv_parse(input: *const c_char) -> *mut Value {
    let s = match unsafe { read_input_string(input) } {
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

#[unsafe(no_mangle)]
#[allow(clippy::mutable_key_type)]
/// Parse CSV text, treating its first row as headers.
///
/// # Safety
/// `input` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
pub unsafe extern "C" fn mux_csv_parse_with_headers(input: *const c_char) -> *mut Value {
    let s = match unsafe { read_input_string(input) } {
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

#[unsafe(no_mangle)]
#[allow(clippy::mutable_key_type)]
/// Parse CSV text with explicit delimiter, quote, header, trimming, and
/// flexible-row settings. Delimiter and quote are passed as ASCII byte values.
///
/// # Safety
/// `input` must be null or a valid, NUL-terminated C string that remains alive
/// for the duration of this call.
pub unsafe extern "C" fn mux_csv_parse_with_options(
    input: *const c_char,
    delimiter: i64,
    quote: i64,
    has_headers: i32,
    trim: i32,
    flexible: i32,
) -> *mut Value {
    parse_csv_with_options(input, delimiter, quote, has_headers, trim, flexible)
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
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
///
/// # Safety
/// `val` must be null or a valid, live `Value` pointer returned by
/// `mux_rc_alloc`.
pub unsafe extern "C" fn mux_csv_rows_as_maps(val: *const Value) -> *mut Value {
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
    crate::std::csv_result_err(message.to_owned())
}

/// The table as CSV text, always.
///
/// The total counterpart to `mux_csv_to_string`, which returns a `result`
/// because it validates the shape. Same reasoning as `mux_json_to_string`: a
/// `Csv` that exists came from the parser and is well formed, so the failing
/// branch is unreachable - and is still given an answer rather than a panic.
#[unsafe(no_mangle)]
///
/// # Safety
/// `val` must be null or a valid, live `Value` pointer returned by
/// `mux_rc_alloc`.
pub unsafe extern "C" fn mux_csv_render(val: *const Value) -> *mut Value {
    let text = if val.is_null() {
        String::new()
    } else if let Ok((headers, rows)) = validate_and_extract_csv(unsafe { &*val }) {
        build_csv_string(&headers, &rows, true, b',', b'"').unwrap_or_default()
    } else {
        debug_assert!(false, "a Csv value that is not a well formed table");
        String::new()
    };
    crate::refcount::mux_rc_alloc(Value::String(text))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `val` must be null or a valid, live `Value` pointer returned by
/// `mux_rc_alloc`.
pub unsafe extern "C" fn mux_csv_to_string(val: *const Value) -> *mut Value {
    if val.is_null() {
        return err_result("null input");
    }

    let v = unsafe { &*val };

    match validate_and_extract_csv(v) {
        Ok((headers, rows)) => {
            let csv_string = match build_csv_string(&headers, &rows, true, b',', b'"') {
                Ok(csv_string) => csv_string,
                Err(error) => return err_result(&error),
            };
            // Wrap directly to avoid leaking the intermediate allocation (see
            // mux_csv_parse).
            crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(Value::String(csv_string)))))
        }
        Err(e) => err_result(&e),
    }
}

#[unsafe(no_mangle)]
/// Render a parsed CSV table with explicit delimiter and quote bytes.
///
/// # Safety
/// `val` must be null or a valid, live `Value` pointer returned by
/// `mux_rc_alloc`.
pub unsafe extern "C" fn mux_csv_to_string_with(
    val: *const Value,
    delimiter: i64,
    quote: i64,
) -> *mut Value {
    if val.is_null() {
        return err_result("null input");
    }
    let delimiter = match csv_option_byte(delimiter, "delimiter") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let quote = match csv_option_byte(quote, "quote") {
        Ok(value) => value,
        Err(error) => return error,
    };
    match validate_and_extract_csv(unsafe { &*val })
        .and_then(|(headers, rows)| build_csv_string(&headers, &rows, true, delimiter, quote))
    {
        Ok(text) => crate::refcount::mux_rc_alloc(Value::Result(Ok(Box::new(Value::String(text))))),
        Err(error) => err_result(&error),
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

fn build_csv_string(
    headers: &[String],
    rows: &[Vec<String>],
    include_headers: bool,
    delimiter: u8,
    quote: u8,
) -> Result<String, String> {
    let mut output = Vec::new();
    {
        let mut wtr = csv::WriterBuilder::new()
            .delimiter(delimiter)
            .quote(quote)
            .from_writer(&mut output);

        if include_headers && !headers.is_empty() {
            wtr.write_record(headers)
                .map_err(|error| format!("CSV header write error: {error}"))?;
        }

        for row in rows {
            wtr.write_record(row)
                .map_err(|error| format!("CSV row write error: {error}"))?;
        }

        wtr.flush()
            .map_err(|error| format!("CSV flush error: {error}"))?;
    }
    String::from_utf8(output).map_err(|_| "invalid UTF-8 in CSV output".to_string())
}

// Streaming CSV -----------------------------------------------------------

/// The source behind an incremental CSV reader.  A `CsvInput::Reader` keeps
/// only a retained handle; csv's own buffered reader asks it for chunks as
/// records are consumed, so a CSV document need not be materialized first.
enum CsvInput {
    Memory(Cursor<Vec<u8>>),
    Reader(MuxReader),
}

struct MuxReader {
    handle: i64,
    remaining: usize,
    checked_limit: bool,
}

impl Read for MuxReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if self.checked_limit {
                return Ok(0);
            }
            // Read one sentinel byte so an input that is exactly at the
            // package limit is accepted while one byte beyond it is rejected.
            let extra = crate::stream::read_reader(self.handle, 1).map_err(io::Error::other)?;
            self.checked_limit = true;
            if !extra.is_empty() {
                return Err(io::Error::other("CSV input exceeds the 16 MiB limit"));
            }
            return Ok(0);
        }
        let requested = output.len().min(self.remaining);
        let bytes = crate::stream::read_reader(self.handle, requested).map_err(io::Error::other)?;
        if bytes.is_empty() {
            self.remaining = 0;
            return Ok(0);
        }
        output[..bytes.len()].copy_from_slice(&bytes);
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }
}

impl Drop for MuxReader {
    fn drop(&mut self) {
        crate::stream::release_reader(self.handle);
    }
}

impl Read for CsvInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Memory(source) => source.read(output),
            Self::Reader(source) => source.read(output),
        }
    }
}

/// A CSV writer can either retain output in memory (the default) or forward
/// encoded bytes to a caller-supplied `std.io.Writer` as they are produced.
enum CsvOutput {
    Memory(Vec<u8>),
    Writer(MuxWriter),
}

struct MuxWriter {
    handle: i64,
}

impl Write for MuxWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        crate::stream::write_writer(self.handle, bytes)
            .map(|()| bytes.len())
            .map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        crate::stream::flush_writer(self.handle).map_err(io::Error::other)
    }
}

impl Drop for MuxWriter {
    fn drop(&mut self) {
        crate::stream::release_writer(self.handle);
    }
}

impl Write for CsvOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Memory(output) => {
                let end = output
                    .len()
                    .checked_add(bytes.len())
                    .ok_or_else(|| io::Error::other("CSV output is too large"))?;
                if end > MAX_CSV_INPUT_BYTES {
                    return Err(io::Error::other("CSV output exceeds the 16 MiB limit"));
                }
                output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            Self::Writer(output) => output.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Memory(_) => Ok(()),
            Self::Writer(output) => output.flush(),
        }
    }
}

impl CsvOutput {
    fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Memory(bytes) => Some(bytes.clone()),
            Self::Writer(_) => None,
        }
    }
}

struct CsvReaderEntry {
    reader: csv::Reader<CsvInput>,
    has_headers: bool,
    names: usize,
}

struct CsvWriterEntry {
    writer: csv::Writer<CsvOutput>,
    names: usize,
}

static CSV_READERS: LazyLock<Mutex<HashMap<i64, CsvReaderEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CSV_WRITERS: LazyLock<Mutex<HashMap<i64, CsvWriterEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CSV_NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static CSV_READER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "CsvReader",
        size_of::<i64>(),
        Some(drop_csv_reader),
        Some(copy_csv_reader),
    )
});
static CSV_WRITER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "CsvWriter",
        size_of::<i64>(),
        Some(drop_csv_writer),
        Some(copy_csv_writer),
    )
});

fn csv_stream_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn csv_stream_id() -> i64 {
    let mut next = csv_stream_lock(&CSV_NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}

fn csv_stream_object(type_id: TypeId, id: i64) -> *mut Value {
    let value = alloc_object(type_id);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = id };
    value
}

fn csv_stream_result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => mux_rc_alloc(Value::Result(Ok(Box::new(value)))),
        Err(error) => crate::std::csv_result_err(error),
    }
}

fn csv_stream_id_for(value: *const Value, type_id: TypeId, name: &str) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return Err(format!("expected {name} value"));
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err(format!("{name} value is invalid"));
    }
    let id = unsafe { *ptr.cast::<i64>() };
    (id > 0)
        .then_some(id)
        .ok_or_else(|| format!("{name} value is invalid"))
}

extern "C" fn copy_csv_reader(source: *mut c_void, dest: *mut c_void) {
    copy_csv_stream(&CSV_READERS, source, dest);
}
extern "C" fn copy_csv_writer(source: *mut c_void, dest: *mut c_void) {
    copy_csv_stream(&CSV_WRITERS, source, dest);
}
fn copy_csv_stream<T>(map: &Mutex<HashMap<i64, T>>, source: *mut c_void, dest: *mut c_void)
where
    T: CsvNames,
{
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = csv_stream_lock(map).get_mut(&id) {
        entry.names_mut().saturating_add_assign(1);
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

trait CsvNames {
    fn names_mut(&mut self) -> &mut usize;
}
impl CsvNames for CsvReaderEntry {
    fn names_mut(&mut self) -> &mut usize {
        &mut self.names
    }
}
impl CsvNames for CsvWriterEntry {
    fn names_mut(&mut self) -> &mut usize {
        &mut self.names
    }
}

trait SaturatingAddAssign {
    fn saturating_add_assign(&mut self, value: usize);
}
impl SaturatingAddAssign for usize {
    fn saturating_add_assign(&mut self, value: usize) {
        *self = self.saturating_add(value);
    }
}

extern "C" fn drop_csv_reader(ptr: *mut c_void) {
    drop_csv_stream(&CSV_READERS, ptr);
}
extern "C" fn drop_csv_writer(ptr: *mut c_void) {
    drop_csv_stream(&CSV_WRITERS, ptr);
}
fn drop_csv_stream<T>(map: &Mutex<HashMap<i64, T>>, ptr: *mut c_void)
where
    T: CsvNames,
{
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut values = csv_stream_lock(map);
    let remove = values.get_mut(&id).is_some_and(|entry| {
        let names = entry.names_mut();
        *names = names.saturating_sub(1);
        *names == 0
    });
    if remove {
        values.remove(&id);
    }
}

fn record_value(record: &csv::StringRecord) -> Value {
    Value::List(
        record
            .iter()
            .map(|field| Value::String(field.to_string()))
            .collect(),
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_csv_reader_new() -> *mut Value {
    let id = csv_stream_id();
    csv_stream_lock(&CSV_READERS).insert(
        id,
        CsvReaderEntry {
            reader: csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(CsvInput::Memory(Cursor::new(Vec::new()))),
            has_headers: false,
            names: 1,
        },
    );
    let value = csv_stream_object(*CSV_READER_TYPE_ID, id);
    if value.is_null() {
        csv_stream_lock(&CSV_READERS).remove(&id);
    }
    value
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_reader_from_bytes(
    bytes: *const Value,
    has_headers: i32,
) -> *mut Value {
    let data = match unsafe { bytes.as_ref() } {
        Some(Value::Bytes(data)) if data.len() <= MAX_CSV_INPUT_BYTES => data.clone(),
        Some(Value::Bytes(_)) => {
            return csv_stream_result(Err("CSV input exceeds the 16 MiB limit".to_string()))
        }
        _ => return csv_stream_result(Err("CSV reader input must be bytes".to_string())),
    };
    let id = csv_stream_id();
    let has_headers = has_headers != 0;
    let reader = csv::ReaderBuilder::new()
        .has_headers(has_headers)
        .from_reader(CsvInput::Memory(Cursor::new(data)));
    csv_stream_lock(&CSV_READERS).insert(
        id,
        CsvReaderEntry {
            reader,
            has_headers,
            names: 1,
        },
    );
    let value = csv_stream_object(*CSV_READER_TYPE_ID, id);
    if value.is_null() {
        csv_stream_lock(&CSV_READERS).remove(&id);
        return csv_stream_result(Err("could not allocate CsvReader".to_string()));
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return csv_stream_result(Err("could not allocate CsvReader".to_string()));
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    csv_stream_result(Ok(owned))
}

/// Create an incremental CSV reader over an existing `std.io.Reader`.
/// Records are pulled from the source as `read()` is called; the source stays
/// open and is retained until this CSV reader is closed or dropped.
///
/// # Safety
/// `reader` must be null or point to a live `std.io.Reader` value for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_reader_from_reader(
    reader: *const Value,
    has_headers: i32,
) -> *mut Value {
    let handle = match crate::stream::reader_handle(reader) {
        Ok(handle) => handle,
        Err(error) => return csv_stream_result(Err(error)),
    };
    if let Err(error) = crate::stream::retain_reader(handle) {
        return csv_stream_result(Err(error));
    }
    let has_headers = has_headers != 0;
    let source = CsvInput::Reader(MuxReader {
        handle,
        remaining: MAX_CSV_INPUT_BYTES,
        checked_limit: false,
    });
    let reader = csv::ReaderBuilder::new()
        .has_headers(has_headers)
        .from_reader(source);
    let id = csv_stream_id();
    csv_stream_lock(&CSV_READERS).insert(
        id,
        CsvReaderEntry {
            reader,
            has_headers,
            names: 1,
        },
    );
    let value = csv_stream_object(*CSV_READER_TYPE_ID, id);
    if value.is_null() {
        csv_stream_lock(&CSV_READERS).remove(&id);
        return csv_stream_result(Err("could not allocate CsvReader".to_string()));
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return csv_stream_result(Err("could not allocate CsvReader".to_string()));
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    csv_stream_result(Ok(owned))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_reader_headers(reader: *const Value) -> *mut Value {
    let id = match csv_stream_id_for(reader, *CSV_READER_TYPE_ID, "CsvReader") {
        Ok(id) => id,
        Err(e) => return csv_stream_result(Err(e)),
    };
    let mut readers = csv_stream_lock(&CSV_READERS);
    let Some(reader) = readers.get_mut(&id) else {
        return csv_stream_result(Err("CsvReader is closed".to_string()));
    };
    if !reader.has_headers {
        return csv_stream_result(Ok(Value::List(Vec::new())));
    }
    match reader.reader.headers() {
        Ok(record) => csv_stream_result(Ok(Value::List(
            record
                .iter()
                .map(|field| Value::String(field.to_string()))
                .collect(),
        ))),
        Err(error) => csv_stream_result(Err(format!("CSV header error: {error}"))),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_reader_read(reader: *const Value) -> *mut Value {
    let id = match csv_stream_id_for(reader, *CSV_READER_TYPE_ID, "CsvReader") {
        Ok(id) => id,
        Err(e) => return csv_stream_result(Err(e)),
    };
    let mut readers = csv_stream_lock(&CSV_READERS);
    let Some(reader) = readers.get_mut(&id) else {
        return csv_stream_result(Err("CsvReader is closed".to_string()));
    };
    let mut record = csv::StringRecord::new();
    match reader.reader.read_record(&mut record) {
        Ok(true) => csv_stream_result(Ok(Value::Optional(Some(Box::new(record_value(&record)))))),
        Ok(false) => csv_stream_result(Ok(Value::Optional(None))),
        Err(error) => csv_stream_result(Err(format!("CSV record error: {error}"))),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_csv_writer_new() -> *mut Value {
    let id = csv_stream_id();
    csv_stream_lock(&CSV_WRITERS).insert(
        id,
        CsvWriterEntry {
            writer: csv::Writer::from_writer(CsvOutput::Memory(Vec::new())),
            names: 1,
        },
    );
    let value = csv_stream_object(*CSV_WRITER_TYPE_ID, id);
    if value.is_null() {
        csv_stream_lock(&CSV_WRITERS).remove(&id);
    }
    value
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_writer_from_config(delimiter: i64, quote: i64) -> *mut Value {
    let delimiter = match csv_option_byte(delimiter, "delimiter") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let quote = match csv_option_byte(quote, "quote") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let mut builder = csv::WriterBuilder::new();
    builder.delimiter(delimiter).quote(quote);
    let id = csv_stream_id();
    csv_stream_lock(&CSV_WRITERS).insert(
        id,
        CsvWriterEntry {
            writer: builder.from_writer(CsvOutput::Memory(Vec::new())),
            names: 1,
        },
    );
    let value = csv_stream_object(*CSV_WRITER_TYPE_ID, id);
    if value.is_null() {
        csv_stream_lock(&CSV_WRITERS).remove(&id);
        return csv_stream_result(Err("could not allocate CsvWriter".to_string()));
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return csv_stream_result(Err("could not allocate CsvWriter".to_string()));
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    csv_stream_result(Ok(owned))
}

/// Create an incremental CSV writer over an existing `std.io.Writer`.
/// Encoded records are forwarded as they are written; the destination remains
/// open and is retained until this CSV writer is closed or dropped.
///
/// # Safety
/// `writer` must be null or point to a live `std.io.Writer` value for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_writer_from_writer(
    writer: *const Value,
    delimiter: i64,
    quote: i64,
) -> *mut Value {
    let handle = match crate::stream::writer_handle(writer) {
        Ok(handle) => handle,
        Err(error) => return csv_stream_result(Err(error)),
    };
    let delimiter = match csv_option_byte(delimiter, "delimiter") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let quote = match csv_option_byte(quote, "quote") {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = crate::stream::retain_writer(handle) {
        return csv_stream_result(Err(error));
    }
    let mut builder = csv::WriterBuilder::new();
    builder.delimiter(delimiter).quote(quote);
    let id = csv_stream_id();
    csv_stream_lock(&CSV_WRITERS).insert(
        id,
        CsvWriterEntry {
            writer: builder.from_writer(CsvOutput::Writer(MuxWriter { handle })),
            names: 1,
        },
    );
    let value = csv_stream_object(*CSV_WRITER_TYPE_ID, id);
    if value.is_null() {
        csv_stream_lock(&CSV_WRITERS).remove(&id);
        return csv_stream_result(Err("could not allocate CsvWriter".to_string()));
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return csv_stream_result(Err("could not allocate CsvWriter".to_string()));
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    csv_stream_result(Ok(owned))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_writer_write(
    writer: *const Value,
    row: *const Value,
) -> *mut Value {
    let id = match csv_stream_id_for(writer, *CSV_WRITER_TYPE_ID, "CsvWriter") {
        Ok(id) => id,
        Err(e) => return csv_stream_result(Err(e)),
    };
    let Value::List(fields) = (unsafe { row.as_ref() })
        .cloned()
        .unwrap_or(Value::List(Vec::new()))
    else {
        return csv_stream_result(Err("CSV row must be list<string>".to_string()));
    };
    let mut values = Vec::with_capacity(fields.len());
    for field in fields {
        let Value::String(value) = field else {
            return csv_stream_result(Err("CSV row must contain only strings".to_string()));
        };
        values.push(value);
    }
    let mut writers = csv_stream_lock(&CSV_WRITERS);
    let Some(writer) = writers.get_mut(&id) else {
        return csv_stream_result(Err("CsvWriter is closed".to_string()));
    };
    match writer.writer.write_record(values) {
        Ok(()) => csv_stream_result(Ok(Value::Unit)),
        Err(error) => csv_stream_result(Err(format!("CSV write error: {error}"))),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_writer_flush(writer: *const Value) -> *mut Value {
    let id = match csv_stream_id_for(writer, *CSV_WRITER_TYPE_ID, "CsvWriter") {
        Ok(id) => id,
        Err(e) => return csv_stream_result(Err(e)),
    };
    let mut writers = csv_stream_lock(&CSV_WRITERS);
    let Some(writer) = writers.get_mut(&id) else {
        return csv_stream_result(Err("CsvWriter is closed".to_string()));
    };
    match writer.writer.flush() {
        Ok(()) => csv_stream_result(Ok(Value::Unit)),
        Err(error) => csv_stream_result(Err(format!("CSV flush error: {error}"))),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_csv_writer_bytes(writer: *const Value) -> *mut Value {
    let id = match csv_stream_id_for(writer, *CSV_WRITER_TYPE_ID, "CsvWriter") {
        Ok(id) => id,
        Err(e) => return csv_stream_result(Err(e)),
    };
    let mut writers = csv_stream_lock(&CSV_WRITERS);
    let Some(writer) = writers.get_mut(&id) else {
        return csv_stream_result(Err("CsvWriter is closed".to_string()));
    };
    if let Err(error) = writer.writer.flush() {
        return csv_stream_result(Err(format!("CSV flush error: {error}")));
    }
    let Some(bytes) = writer.writer.get_ref().bytes() else {
        return csv_stream_result(Err(
            "bytes is not available for a writer-backed CsvWriter".to_string()
        ));
    };
    csv_stream_result(Ok(Value::Bytes(bytes)))
}
