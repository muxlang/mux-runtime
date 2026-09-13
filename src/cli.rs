//! Synchronous command-line parsing for `std.cli`.
//!
//! Parsers are shared mutable handles. Configuration is explicit and parsing
//! is deterministic: command-line values win over environment values, which
//! win over declared defaults.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::{TypeId, Value};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::io::Read;
use std::sync::{LazyLock, Mutex};

struct OptionSpec {
    name: String,
    short: Option<char>,
    takes_value: bool,
    required: bool,
    env: Option<String>,
    default: Option<String>,
    multiple: bool,
    conflicts: Vec<String>,
    requires: Vec<String>,
    aliases: Vec<String>,
    group: Option<String>,
    /// Address of a compiler-produced closure that normalizes one raw value.
    /// The parser owns one retained closure reference.
    parser: Option<usize>,
}

impl Clone for OptionSpec {
    fn clone(&self) -> Self {
        if let Some(parser) = self.parser {
            // A parse/help clone owns its own reference so a concurrent parser
            // mutation cannot release the callback while the clone is using it.
            unsafe { crate::closure::mux_closure_retain(parser as *mut c_void) };
        }
        Self {
            name: self.name.clone(),
            short: self.short,
            takes_value: self.takes_value,
            required: self.required,
            env: self.env.clone(),
            default: self.default.clone(),
            multiple: self.multiple,
            conflicts: self.conflicts.clone(),
            requires: self.requires.clone(),
            aliases: self.aliases.clone(),
            group: self.group.clone(),
            parser: self.parser,
        }
    }
}

impl Drop for OptionSpec {
    fn drop(&mut self) {
        if let Some(parser) = self.parser {
            unsafe { crate::closure::mux_closure_release(parser as *mut c_void) };
        }
    }
}

#[repr(C)]
struct ClosureRepr {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
    capture_count: i64,
    boxed_function_ptr: *mut c_void,
}

#[derive(Default)]
struct ParserEntry {
    program: String,
    about: String,
    version: Option<String>,
    options: Vec<OptionSpec>,
    positionals: Vec<(String, bool)>,
    children: Vec<(String, i64)>,
    response_files: bool,
    names: usize,
}

#[derive(Clone)]
struct MatchesEntry {
    values: HashMap<String, Vec<String>>,
    positionals: Vec<String>,
    help: String,
    subcommand: Option<String>,
    subcommand_matches: Option<Box<MatchesEntry>>,
    names: usize,
}

static PARSERS: LazyLock<Mutex<HashMap<i64, ParserEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MATCHES: LazyLock<Mutex<HashMap<i64, MatchesEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static PARSER_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "CliParser",
        size_of::<i64>(),
        Some(drop_parser),
        Some(copy_parser),
    )
});
static MATCHES_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "CliMatches",
        size_of::<i64>(),
        Some(drop_matches),
        Some(copy_matches),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn next_id() -> i64 {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}
fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => mux_rc_alloc(Value::Result(Ok(Box::new(value)))),
        Err(error) => crate::std::cli_result_err(error),
    }
}
fn ok(value: Value) -> *mut Value {
    result(Ok(value))
}
fn err(message: impl Into<String>) -> *mut Value {
    result(Err(message.into()))
}

fn object(type_id: TypeId, id: i64) -> *mut Value {
    let value = alloc_object(type_id);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        return std::ptr::null_mut();
    }
    unsafe {
        *ptr.cast::<i64>() = id;
    }
    value
}

fn handle(value: *const Value, expected: TypeId, name: &str) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != expected {
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

fn owned_object(type_id: TypeId, id: i64) -> Result<Value, String> {
    let value = object(type_id, id);
    if value.is_null() {
        return Err("could not allocate CLI value".to_string());
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return Err("could not allocate CLI value".to_string());
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    Ok(owned)
}

fn copy_handle<T>(
    map: &Mutex<HashMap<i64, T>>,
    source: *mut c_void,
    dest: *mut c_void,
    names: fn(&mut T) -> &mut usize,
) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = lock(map).get_mut(&id) {
        *names(entry) = names(entry).saturating_add(1);
        unsafe {
            *dest.cast::<i64>() = id;
        }
    } else {
        unsafe {
            *dest.cast::<i64>() = 0;
        }
    }
}
fn drop_handle<T>(map: &Mutex<HashMap<i64, T>>, ptr: *mut c_void, names: fn(&mut T) -> &mut usize) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut entries = lock(map);
    let remove = entries.get_mut(&id).is_some_and(|entry| {
        let count = names(entry);
        *count = count.saturating_sub(1);
        *count == 0
    });
    if remove {
        entries.remove(&id);
    }
}
fn parser_names(entry: &mut ParserEntry) -> &mut usize {
    &mut entry.names
}
fn matches_names(entry: &mut MatchesEntry) -> &mut usize {
    &mut entry.names
}
extern "C" fn copy_parser(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&PARSERS, source, dest, parser_names);
}
extern "C" fn copy_matches(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&MATCHES, source, dest, matches_names);
}

fn release_parser_reference(id: i64) {
    let children = {
        let mut parsers = lock(&PARSERS);
        let Some(entry) = parsers.get_mut(&id) else {
            return;
        };
        entry.names = entry.names.saturating_sub(1);
        if entry.names == 0 {
            parsers.remove(&id).map(|entry| entry.children)
        } else {
            None
        }
    };
    if let Some(children) = children {
        for (_, child_id) in children {
            release_parser_reference(child_id);
        }
    }
}

extern "C" fn drop_parser(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    release_parser_reference(id);
}
extern "C" fn drop_matches(ptr: *mut c_void) {
    drop_handle(&MATCHES, ptr, matches_names);
}

fn text(value: *const Value, label: &str) -> Result<String, String> {
    match unsafe { value.as_ref() } {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(format!("{label} must be a string")),
    }
}
fn list_strings(value: *const Value) -> Result<Vec<String>, String> {
    let Some(Value::List(values)) = (unsafe { value.as_ref() }) else {
        return Err("CLI arguments must be list<string>".to_string());
    };
    values
        .iter()
        .map(|value| match value {
            Value::String(value) => Ok(value.clone()),
            _ => Err("CLI arguments must be list<string>".to_string()),
        })
        .collect()
}
fn parser_value(entry: ParserEntry) -> *mut Value {
    let id = next_id();
    lock(&PARSERS).insert(id, entry);
    if let Ok(value) = owned_object(*PARSER_TYPE_ID, id) {
        mux_rc_alloc(value)
    } else {
        lock(&PARSERS).remove(&id);
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_cli_parser_new() -> *mut Value {
    parser_value(ParserEntry::default())
}

fn mutate_parser(
    parser: *const Value,
    f: impl FnOnce(&mut ParserEntry) -> Result<(), String>,
) -> *mut Value {
    let id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let mut parsers = lock(&PARSERS);
    match parsers.get_mut(&id) {
        Some(entry) => match f(entry) {
            Ok(()) => ok(Value::Unit),
            Err(e) => err(e),
        },
        None => err("CliParser is closed"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_program(
    parser: *const Value,
    name: *const Value,
) -> *mut Value {
    let name = match text(name, "program name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    if name.is_empty() {
        return err("program name must not be empty");
    }
    mutate_parser(parser, |entry| {
        entry.program = name;
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_about(
    parser: *const Value,
    about: *const Value,
) -> *mut Value {
    let about = match text(about, "about text") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    mutate_parser(parser, |entry| {
        entry.about = about;
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_version(
    parser: *const Value,
    version: *const Value,
) -> *mut Value {
    let version = match text(version, "version") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    mutate_parser(parser, |entry| {
        entry.version = Some(version);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_response_files(
    parser: *const Value,
    enabled: bool,
) -> *mut Value {
    mutate_parser(parser, |entry| {
        entry.response_files = enabled;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_add_option(
    parser: *const Value,
    name: *const Value,
    short: *const Value,
    takes_value: bool,
    required: bool,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let short = match text(short, "short option") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return err(
            "option name must contain only letters, digits, '-' or '_' and must not be empty",
        );
    }
    let short = if short.is_empty() {
        None
    } else {
        let mut chars = short.chars();
        let Some(ch) = chars.next() else {
            return err("short option must not be empty");
        };
        if !ch.is_ascii_alphanumeric() || chars.next().is_some() {
            return err("short option must be one ASCII letter or digit");
        }
        Some(ch)
    };
    mutate_parser(parser, |entry| {
        if entry
            .options
            .iter()
            .any(|option| option.name == name || option.short == short && short.is_some())
        {
            return Err("duplicate CLI option".to_string());
        }
        entry.options.push(OptionSpec {
            name,
            short,
            takes_value,
            required,
            env: None,
            default: None,
            multiple: false,
            conflicts: Vec::new(),
            requires: Vec::new(),
            aliases: Vec::new(),
            group: None,
            parser: None,
        });
        Ok(())
    })
}

/// Set a synchronous typed parser for an option's raw value.
///
/// The callback receives the raw command-line, environment, or default text
/// and must return `result<string, string>`. A successful string replaces the
/// stored value; an error aborts parsing with the callback's message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_parser(
    parser: *const Value,
    name: *const Value,
    callback: *mut c_void,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(value) => value,
        Err(error) => return err(error),
    };
    if callback.is_null() {
        return err("option parser callback must not be null");
    }
    mutate_parser(parser, |entry| {
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        // The parser retains a reference independent of the expression
        // temporary that supplied the callback.
        unsafe { crate::closure::mux_closure_retain(callback) };
        let previous = option.parser.replace(callback as usize);
        if let Some(previous) = previous {
            unsafe { crate::closure::mux_closure_release(previous as *mut c_void) };
        }
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_env(
    parser: *const Value,
    name: *const Value,
    env: *const Value,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let env = match text(env, "environment name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    mutate_parser(parser, |entry| {
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        option.env = (!env.is_empty()).then_some(env);
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_default(
    parser: *const Value,
    name: *const Value,
    default: *const Value,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let default = match text(default, "default value") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    mutate_parser(parser, |entry| {
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        option.default = (!default.is_empty()).then_some(default);
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_multiple(
    parser: *const Value,
    name: *const Value,
    multiple: bool,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    mutate_parser(parser, |entry| {
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        option.multiple = multiple;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_conflicts(
    parser: *const Value,
    name: *const Value,
    other: *const Value,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let other = match text(other, "conflicting option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    if name == other {
        return err("an option cannot conflict with itself");
    }
    mutate_parser(parser, |entry| {
        if !entry.options.iter().any(|option| option.name == name) {
            return Err("unknown CLI option".to_string());
        }
        if !entry.options.iter().any(|option| option.name == other) {
            return Err("unknown conflicting CLI option".to_string());
        }
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        if !option.conflicts.contains(&other) {
            option.conflicts.push(other);
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_requires(
    parser: *const Value,
    name: *const Value,
    required: *const Value,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let required = match text(required, "required option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    if name == required {
        return err("an option cannot require itself");
    }
    mutate_parser(parser, |entry| {
        if !entry.options.iter().any(|option| option.name == name) {
            return Err("unknown CLI option".to_string());
        }
        if !entry.options.iter().any(|option| option.name == required) {
            return Err("unknown required CLI option".to_string());
        }
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        if !option.requires.contains(&required) {
            option.requires.push(required);
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_alias(
    parser: *const Value,
    name: *const Value,
    alias: *const Value,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let alias = match text(alias, "option alias") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    if alias.is_empty()
        || !alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return err(
            "option alias must contain only letters, digits, '-' or '_' and must not be empty",
        );
    }
    mutate_parser(parser, |entry| {
        if !entry.options.iter().any(|option| option.name == name) {
            return Err("unknown CLI option".to_string());
        }
        if entry.options.iter().any(|option| {
            option.name == alias || option.aliases.iter().any(|known| known == &alias)
        }) {
            return Err("duplicate CLI option alias".to_string());
        }
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        option.aliases.push(alias);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_set_option_group(
    parser: *const Value,
    name: *const Value,
    group: *const Value,
) -> *mut Value {
    let name = match text(name, "option name") {
        Ok(value) => value,
        Err(e) => return err(e),
    };
    let group = match text(group, "option group") {
        Ok(value) => value,
        Err(e) => return err(e),
    };
    if group.is_empty() {
        return err("option group must not be empty");
    }
    mutate_parser(parser, |entry| {
        let option = entry
            .options
            .iter_mut()
            .find(|option| option.name == name)
            .ok_or_else(|| "unknown CLI option".to_string())?;
        option.group = Some(group);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_add_positional(
    parser: *const Value,
    name: *const Value,
    required: bool,
) -> *mut Value {
    let name = match text(name, "positional name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    mutate_parser(parser, |entry| {
        if entry.positionals.iter().any(|(known, _)| known == &name) {
            return Err("duplicate positional argument".to_string());
        }
        entry.positionals.push((name, required));
        Ok(())
    })
}

/// Add a nested command and return a parser handle for configuring it. The
/// parent keeps the child alive, so the returned handle may be discarded after
/// its options and positionals have been declared.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_add_subcommand(
    parser: *const Value,
    name: *const Value,
    about: *const Value,
) -> *mut Value {
    let parent_id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let name = match text(name, "subcommand name") {
        Ok(value) => value,
        Err(e) => return err(e),
    };
    let about = match text(about, "subcommand about") {
        Ok(value) => value,
        Err(e) => return err(e),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return err(
            "subcommand name must contain only letters, digits, '-' or '_' and must not be empty",
        );
    }

    let child_id = next_id();
    let mut parsers = lock(&PARSERS);
    let Some(parent) = parsers.get_mut(&parent_id) else {
        return err("CliParser is closed");
    };
    if parent.children.iter().any(|(known, _)| known == &name) {
        return err("duplicate CLI subcommand");
    }
    parent.children.push((name.clone(), child_id));
    parsers.insert(
        child_id,
        ParserEntry {
            program: name,
            about,
            version: None,
            options: Vec::new(),
            positionals: Vec::new(),
            children: Vec::new(),
            response_files: false,
            // One reference belongs to the parent and one to the returned
            // child handle.
            names: 2,
        },
    );
    drop(parsers);

    match owned_object(*PARSER_TYPE_ID, child_id) {
        Ok(value) => result(Ok(value)),
        Err(error) => {
            let mut parsers = lock(&PARSERS);
            if let Some(parent) = parsers.get_mut(&parent_id) {
                parent.children.retain(|(_, id)| *id != child_id);
            }
            parsers.remove(&child_id);
            err(error)
        }
    }
}

fn help_text(entry: &ParserEntry) -> String {
    let mut text = String::new();
    if !entry.program.is_empty() {
        text.push_str(&entry.program);
    }
    if !entry.about.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&entry.about);
    }
    let mut groups: BTreeMap<String, Vec<&OptionSpec>> = BTreeMap::new();
    for option in &entry.options {
        groups
            .entry(
                option
                    .group
                    .clone()
                    .unwrap_or_else(|| "Options".to_string()),
            )
            .or_default()
            .push(option);
    }
    for (group, options) in groups {
        text.push_str("\n\n");
        text.push_str(&group);
        text.push_str(":\n");
        for option in options {
            text.push_str("  --");
            text.push_str(&option.name);
            if let Some(short) = option.short {
                text.push_str(", -");
                text.push(short);
            }
            for alias in &option.aliases {
                text.push_str(", --");
                text.push_str(alias);
            }
            if option.takes_value {
                text.push_str(" <value>");
            }
            if option.required {
                text.push_str(" (required)");
            }
            text.push('\n');
        }
    }
    if !entry.positionals.is_empty() {
        text.push_str("\nPositionals:\n");
        for (name, required) in &entry.positionals {
            text.push_str("  ");
            text.push_str(name);
            if *required {
                text.push_str(" (required)");
            }
            text.push('\n');
        }
    }
    if !entry.children.is_empty() {
        text.push_str("\nCommands:\n");
        let mut children = entry.children.iter().collect::<Vec<_>>();
        children.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        for (name, child_id) in children {
            let about = lock(&PARSERS)
                .get(child_id)
                .map(|child| child.about.clone())
                .unwrap_or_default();
            text.push_str("  ");
            text.push_str(name);
            if !about.is_empty() {
                text.push_str("  ");
                text.push_str(&about);
            }
            text.push('\n');
        }
    }
    text
}

fn completion_text(entry: &ParserEntry, shell: &str) -> Result<String, String> {
    let program = if entry.program.is_empty() {
        "mux_program"
    } else {
        entry.program.as_str()
    };
    let mut options: Vec<String> = entry
        .options
        .iter()
        .flat_map(|option| {
            let mut names = vec![format!("--{}", option.name)];
            if let Some(short) = option.short {
                names.push(format!("-{short}"));
            }
            names.extend(option.aliases.iter().map(|alias| format!("--{alias}")));
            names
        })
        .collect();
    let mut commands = entry
        .children
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    commands.sort_unstable();
    options.extend(commands);
    match shell {
        "bash" => Ok(format!(
            "_{program}_complete() {{\n    local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n    COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n}}\ncomplete -F _{program}_complete {program}\n",
            options.join(" ")
        )),
        "zsh" => {
            let specs = options
                .iter()
                .map(|option| format!("'{}[{}]'", option, option))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!("#compdef {program}\n_arguments {specs}\n"))
        }
        "fish" => Ok(options
            .iter()
            .map(|option| format!("complete -c {program} -l {}", option.trim_start_matches("--")))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"),
        _ => Err("completion shell must be one of bash, zsh, or fish".to_string()),
    }
}

fn roff_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn manpage_text(entry: &ParserEntry) -> String {
    let program = if entry.program.is_empty() {
        "mux_program"
    } else {
        entry.program.as_str()
    };
    let mut output = format!(
        ".TH \"{}\" 1\n.SH NAME\n{}",
        roff_escape(program),
        roff_escape(program)
    );
    if !entry.about.is_empty() {
        output.push_str(" - ");
        output.push_str(&roff_escape(&entry.about));
    }
    output.push_str("\n.SH SYNOPSIS\n.B ");
    output.push_str(&roff_escape(program));
    for option in &entry.options {
        output.push_str(" [--");
        output.push_str(&roff_escape(&option.name));
        if option.takes_value {
            output.push_str(" value");
        }
        output.push(']');
    }
    output.push_str("\n.SH OPTIONS\n");
    let mut groups: BTreeMap<String, Vec<&OptionSpec>> = BTreeMap::new();
    for option in &entry.options {
        groups
            .entry(
                option
                    .group
                    .clone()
                    .unwrap_or_else(|| "Options".to_string()),
            )
            .or_default()
            .push(option);
    }
    for (group, options) in groups {
        if group != "Options" {
            output.push_str(".SS ");
            output.push_str(&roff_escape(&group));
            output.push('\n');
        }
        for option in options {
            output.push_str(".TP\n.B --");
            output.push_str(&roff_escape(&option.name));
            if let Some(short) = option.short {
                output.push_str(", -");
                output.push(short);
            }
            output.push('\n');
            if option.required {
                output.push_str("Required option.\n");
            } else if option.takes_value {
                output.push_str("Takes a value.\n");
            } else {
                output.push_str("Boolean flag.\n");
            }
        }
    }
    if !entry.children.is_empty() {
        output.push_str(".SH COMMANDS\n");
        let mut children = entry.children.iter().collect::<Vec<_>>();
        children.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        for (name, child_id) in children {
            output.push_str(".TP\n.B ");
            output.push_str(&roff_escape(name));
            output.push('\n');
            let about = lock(&PARSERS)
                .get(child_id)
                .map(|child| child.about.clone())
                .unwrap_or_default();
            if about.is_empty() {
                output.push_str("Subcommand.\n");
            } else {
                output.push_str(&roff_escape(&about));
                output.push('\n');
            }
        }
    }
    output
}

fn tokenize_response_file(source: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut comment = false;
    let mut characters = source.chars().peekable();
    while let Some(character) = characters.next() {
        if comment {
            if character == '\n' {
                comment = false;
            }
            continue;
        }
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            match character {
                '\\' if characters.peek().is_none_or(|next| {
                    next.is_whitespace() || matches!(*next, '\\' | '\'' | '"' | '#')
                }) =>
                {
                    escaped = true;
                }
                '\\' => current.push('\\'),
                value if value == active_quote => quote = None,
                value => current.push(value),
            }
            continue;
        }
        match character {
            '\\' if characters.peek().is_none_or(|next| {
                next.is_whitespace() || matches!(*next, '\\' | '\'' | '"' | '#')
            }) =>
            {
                escaped = true;
            }
            '\\' => current.push('\\'),
            '\'' | '"' => quote = Some(character),
            '#' if current.is_empty() => comment = true,
            value if value.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            value => current.push(value),
        }
    }
    if escaped {
        return Err("response file ends with an escape".to_string());
    }
    if quote.is_some() {
        return Err("response file has an unterminated quote".to_string());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

const MAX_RESPONSE_DEPTH: usize = 8;
const MAX_RESPONSE_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_ARGUMENTS: usize = 1_000_000;

struct ResponseBudget {
    remaining_bytes: usize,
    remaining_arguments: usize,
    argument_limit: usize,
}

impl ResponseBudget {
    fn new(max_bytes: usize, max_arguments: usize) -> Self {
        Self {
            remaining_bytes: max_bytes,
            remaining_arguments: max_arguments,
            argument_limit: max_arguments,
        }
    }

    fn consume_bytes(&mut self, path: &str, bytes: usize) -> Result<(), String> {
        if bytes > self.remaining_bytes {
            return Err(format!(
                "response files exceed the aggregate byte limit while reading '{path}'"
            ));
        }
        self.remaining_bytes -= bytes;
        Ok(())
    }

    fn consume_argument(&mut self) -> Result<(), String> {
        if self.remaining_arguments == 0 {
            let limit = if self.argument_limit == MAX_RESPONSE_ARGUMENTS {
                "one million".to_string()
            } else {
                self.argument_limit.to_string()
            };
            return Err(format!(
                "expanded response arguments exceed {limit} entries"
            ));
        }
        self.remaining_arguments -= 1;
        Ok(())
    }
}

fn response_file_limit_error(path: &str, max_bytes: usize) -> String {
    if max_bytes == MAX_RESPONSE_FILE_BYTES {
        format!("response file '{path}' exceeds the 16 MiB limit")
    } else {
        format!("response file '{path}' exceeds the {max_bytes}-byte limit")
    }
}

fn read_response_file(
    path: &str,
    max_file_bytes: usize,
    budget: &mut ResponseBudget,
) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("failed to read response file '{path}': {error}"))?;
    let read_limit = max_file_bytes
        .min(budget.remaining_bytes)
        .checked_add(1)
        .ok_or_else(|| "response file byte limit is too large".to_string())?;
    let mut source = Vec::new();
    file.take(read_limit as u64)
        .read_to_end(&mut source)
        .map_err(|error| format!("failed to read response file '{path}': {error}"))?;
    if source.len() > max_file_bytes {
        return Err(response_file_limit_error(path, max_file_bytes));
    }
    budget.consume_bytes(path, source.len())?;
    String::from_utf8(source).map_err(|_| format!("response file '{path}' must be valid UTF-8"))
}

fn expand_response_files(args: Vec<String>, depth: usize) -> Result<Vec<String>, String> {
    let mut budget = ResponseBudget::new(MAX_RESPONSE_FILE_BYTES, MAX_RESPONSE_ARGUMENTS);
    expand_response_files_with_budget(args, depth, MAX_RESPONSE_FILE_BYTES, &mut budget)
}

fn expand_response_files_with_budget(
    args: Vec<String>,
    depth: usize,
    max_file_bytes: usize,
    budget: &mut ResponseBudget,
) -> Result<Vec<String>, String> {
    if depth > MAX_RESPONSE_DEPTH {
        return Err(format!(
            "response file nesting exceeds {MAX_RESPONSE_DEPTH} levels"
        ));
    }
    let mut expanded = Vec::new();
    for argument in args {
        if let Some(path) = argument.strip_prefix("@@") {
            budget.consume_argument()?;
            expanded.push(format!("@{path}"));
            continue;
        }
        let Some(path) = argument.strip_prefix('@').filter(|path| !path.is_empty()) else {
            budget.consume_argument()?;
            expanded.push(argument);
            continue;
        };
        let source = read_response_file(path, max_file_bytes, budget)?;
        let nested = tokenize_response_file(&source)
            .map_err(|error| format!("invalid response file '{path}': {error}"))?;
        let nested = expand_response_files_with_budget(nested, depth + 1, max_file_bytes, budget)?;
        expanded.extend(nested);
    }
    Ok(expanded)
}

/// Invoke an option's compiler-produced `func(string) returns
/// result<string, string>` callback. The callback is synchronous and its
/// result is consumed before the raw argument is released.
unsafe fn apply_option_parser(callback: usize, raw: &str) -> Result<String, String> {
    let input = mux_rc_alloc(Value::String(raw.to_string()));
    if input.is_null() {
        return Err("could not allocate CLI option parser input".to_string());
    }
    let repr = unsafe { &*(callback as *const ClosureRepr) };
    let boxed_function = repr.boxed_function_ptr;
    if boxed_function.is_null() {
        unsafe { mux_rc_dec(input) };
        return Err("option parser callback has no return wrapper".to_string());
    }
    let parsed = if repr.captures_ptr.is_null() {
        let function: extern "C" fn(*mut Value) -> *mut Value =
            unsafe { std::mem::transmute(boxed_function) };
        function(input)
    } else {
        let function: extern "C" fn(*mut c_void, *mut Value) -> *mut Value =
            unsafe { std::mem::transmute(boxed_function) };
        function(repr.captures_ptr, input)
    };
    unsafe { mux_rc_dec(input) };
    if parsed.is_null() {
        return Err("option parser callback returned null".to_string());
    }
    let outcome = match unsafe { parsed.as_ref() } {
        Some(Value::Result(Ok(value))) => match value.as_ref() {
            Value::String(value) => Ok(value.clone()),
            _ => Err("option parser must return result<string, string>".to_string()),
        },
        Some(Value::Result(Err(error))) => match error.as_ref() {
            Value::String(error) => Err(error.clone()),
            _ => Err("option parser must return result<string, string>".to_string()),
        },
        _ => Err("option parser must return result<string, string>".to_string()),
    };
    unsafe { mux_rc_dec(parsed) };
    outcome
}

fn normalize_option_values(
    option: &OptionSpec,
    values: &mut HashMap<String, Vec<String>>,
) -> Result<(), String> {
    let Some(callback) = option.parser else {
        return Ok(());
    };
    let Some(raw_values) = values.get_mut(&option.name) else {
        return Ok(());
    };
    for raw in raw_values {
        let normalized = unsafe { apply_option_parser(callback, raw) }
            .map_err(|error| format!("invalid value for '--{}': {error}", option.name))?;
        *raw = normalized;
    }
    Ok(())
}

fn parse_args(entry: &ParserEntry, args: Vec<String>) -> Result<MatchesEntry, String> {
    let args = if entry.response_files {
        expand_response_files(args, 0)?
    } else {
        args
    };
    let mut values: HashMap<String, Vec<String>> = HashMap::new();
    let mut positionals = Vec::new();
    let mut subcommand = None;
    let mut subcommand_matches = None;
    let mut end_options = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !end_options && arg == "--" {
            end_options = true;
            index += 1;
            continue;
        }
        if !end_options && arg.starts_with("--") && arg.len() > 2 {
            let raw = &arg[2..];
            let (name, inline) = raw
                .split_once('=')
                .map_or((raw, None), |(name, value)| (name, Some(value.to_string())));
            let option = entry
                .options
                .iter()
                .find(|option| {
                    option.name == name || option.aliases.iter().any(|alias| alias == name)
                })
                .ok_or_else(|| format!("unknown option '--{name}'"))?;
            let value = if option.takes_value {
                if let Some(value) = inline {
                    value
                } else {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("option '--{name}' requires a value"))?
                }
            } else {
                if inline.is_some() {
                    return Err(format!("option '--{name}' does not take a value"));
                }
                "true".to_string()
            };
            if !option.multiple && values.contains_key(&option.name) {
                return Err(format!("option '--{name}' was provided more than once"));
            }
            values.entry(option.name.clone()).or_default().push(value);
        } else if !end_options && arg.starts_with('-') && arg.len() > 1 {
            let mut chars = arg[1..].chars().peekable();
            while let Some(short) = chars.next() {
                let option = entry
                    .options
                    .iter()
                    .find(|option| option.short == Some(short))
                    .ok_or_else(|| format!("unknown option '-{short}'"))?;
                let value = if option.takes_value {
                    let rest: String = chars.by_ref().collect();
                    if rest.is_empty() {
                        index += 1;
                        args.get(index)
                            .cloned()
                            .ok_or_else(|| format!("option '-{short}' requires a value"))?
                    } else {
                        rest
                    }
                } else {
                    "true".to_string()
                };
                if !option.takes_value && chars.peek().is_some() {
                    values.entry(option.name.clone()).or_default().push(value);
                    continue;
                }
                if !option.multiple && values.contains_key(&option.name) {
                    return Err(format!("option '-{short}' was provided more than once"));
                }
                values.entry(option.name.clone()).or_default().push(value);
            }
        } else {
            if subcommand.is_none() && positionals.is_empty() {
                if let Some((name, child_id)) = entry.children.iter().find(|(name, _)| name == arg)
                {
                    let child = lock(&PARSERS)
                        .get(child_id)
                        .map(CloneForParse::clone_for_parse)
                        .ok_or_else(|| format!("CLI subcommand '{name}' is unavailable"))?;
                    let child_matches = parse_args(&child, args[index + 1..].to_vec())?;
                    subcommand = Some(name.clone());
                    subcommand_matches = Some(Box::new(child_matches));
                    break;
                }
            }
            positionals.push(arg.clone());
        }
        index += 1;
    }
    for option in &entry.options {
        if !values.contains_key(&option.name) {
            if let Some(env) = &option.env {
                if let Ok(value) = std::env::var(env) {
                    values.insert(option.name.clone(), vec![value]);
                    continue;
                }
            }
            if let Some(default) = &option.default {
                values.insert(option.name.clone(), vec![default.clone()]);
                continue;
            }
            if option.required {
                return Err(format!("missing required option '--{}'", option.name));
            }
        }
    }
    for option in &entry.options {
        normalize_option_values(option, &mut values)?;
    }
    for option in &entry.options {
        if !values.contains_key(&option.name) {
            continue;
        }
        for conflicting in &option.conflicts {
            if values.contains_key(conflicting) {
                return Err(format!(
                    "option '--{}' conflicts with '--{}'",
                    option.name, conflicting
                ));
            }
        }
        for required in &option.requires {
            if !values.contains_key(required) {
                return Err(format!(
                    "option '--{}' requires '--{}'",
                    option.name, required
                ));
            }
        }
    }
    for (index, (name, required)) in entry.positionals.iter().enumerate() {
        if *required && positionals.get(index).is_none() {
            return Err(format!("missing required positional '{name}'"));
        }
    }
    Ok(MatchesEntry {
        values,
        positionals,
        help: help_text(entry),
        subcommand,
        subcommand_matches,
        names: 1,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_parse(
    parser: *const Value,
    args: *const Value,
) -> *mut Value {
    let id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let args = match list_strings(args) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let entry = match lock(&PARSERS).get(&id) {
        Some(entry) => entry.clone_for_parse(),
        None => return err("CliParser is closed"),
    };
    match parse_args(&entry, args) {
        Ok(matches) => {
            let id = next_id();
            lock(&MATCHES).insert(id, matches);
            match owned_object(*MATCHES_TYPE_ID, id) {
                Ok(value) => result(Ok(value)),
                Err(e) => err(e),
            }
        }
        Err(e) => err(e),
    }
}

trait CloneForParse {
    fn clone_for_parse(&self) -> ParserEntry;
}
impl CloneForParse for ParserEntry {
    fn clone_for_parse(&self) -> ParserEntry {
        ParserEntry {
            program: self.program.clone(),
            about: self.about.clone(),
            version: self.version.clone(),
            options: self.options.clone(),
            positionals: self.positionals.clone(),
            children: self.children.clone(),
            response_files: self.response_files,
            names: 1,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_parse_process(parser: *const Value) -> *mut Value {
    let args: Vec<Value> = std::env::args().skip(1).map(Value::String).collect();
    let value = mux_rc_alloc(Value::List(args));
    let result = mux_cli_parser_parse(parser, value);
    unsafe { mux_rc_dec(value) };
    result
}

/// Parse the process arguments using conventional `--help`/`--version`
/// handling. Help and version print to stdout and exit successfully; parse
/// errors print to stderr and exit with status 2. This is deliberately opt-in
/// so the normal `parse_process` API can always return outcomes to Mux code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_parse_or_exit(parser: *const Value) -> *mut Value {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let entry = if let Some(entry) = lock(&PARSERS).get(&id) {
        entry.clone_for_parse()
    } else {
        eprintln!("CliParser is closed");
        std::process::exit(2);
    };
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", help_text(&entry));
        std::process::exit(0);
    }
    if args.iter().any(|arg| arg == "--version") {
        if let Some(version) = &entry.version {
            println!("{version}");
            std::process::exit(0);
        }
    }
    let values = args.into_iter().map(Value::String).collect::<Vec<_>>();
    let input = mux_rc_alloc(Value::List(values));
    let parsed = unsafe { mux_cli_parser_parse(parser, input) };
    unsafe { mux_rc_dec(input) };
    let Some(Value::Result(result)) = (unsafe { parsed.as_ref() }) else {
        eprintln!("CLI parser returned an invalid result");
        unsafe { mux_rc_dec(parsed) };
        std::process::exit(2);
    };
    match result.as_ref() {
        Ok(value) => {
            let owned = (**value).clone();
            unsafe { mux_rc_dec(parsed) };
            mux_rc_alloc(owned)
        }
        Err(error) => {
            eprintln!("{}", error);
            unsafe { mux_rc_dec(parsed) };
            std::process::exit(2);
        }
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_help(parser: *const Value) -> *mut Value {
    let id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let entry = lock(&PARSERS).get(&id).map(CloneForParse::clone_for_parse);
    match entry {
        Some(entry) => ok(Value::String(help_text(&entry))),
        None => err("CliParser is closed"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_completion(
    parser: *const Value,
    shell: *const Value,
) -> *mut Value {
    let id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let shell = match text(shell, "completion shell") {
        Ok(value) => value,
        Err(e) => return err(e),
    };
    match lock(&PARSERS).get(&id) {
        Some(entry) => {
            completion_text(entry, &shell).map_or_else(err, |value| ok(Value::String(value)))
        }
        None => err("CliParser is closed"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_parser_manpage(parser: *const Value) -> *mut Value {
    let id = match handle(parser, *PARSER_TYPE_ID, "CliParser") {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    match lock(&PARSERS).get(&id) {
        Some(entry) => ok(Value::String(manpage_text(entry))),
        None => err("CliParser is closed"),
    }
}

fn match_entry(value: *const Value) -> Result<i64, String> {
    handle(value, *MATCHES_TYPE_ID, "CliMatches")
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_has(
    matches: *const Value,
    name: *const Value,
) -> *mut Value {
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    ok(Value::Bool(
        lock(&MATCHES)
            .get(&id)
            .is_some_and(|entry| entry.values.contains_key(&name)),
    ))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_get(
    matches: *const Value,
    name: *const Value,
) -> *mut Value {
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let value = lock(&MATCHES)
        .get(&id)
        .and_then(|entry| entry.values.get(&name))
        .and_then(|values| values.last())
        .cloned()
        .map(|value| Box::new(Value::String(value)));
    ok(Value::Optional(value))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_values(
    matches: *const Value,
    name: *const Value,
) -> *mut Value {
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let name = match text(name, "option name") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let values = lock(&MATCHES)
        .get(&id)
        .and_then(|entry| entry.values.get(&name))
        .cloned()
        .unwrap_or_default();
    ok(Value::List(values.into_iter().map(Value::String).collect()))
}

fn match_text_value(matches: *const Value, name: *const Value) -> Result<Option<String>, String> {
    let id = match_entry(matches)?;
    let name = text(name, "option name")?;
    Ok(lock(&MATCHES)
        .get(&id)
        .and_then(|entry| entry.values.get(&name))
        .and_then(|values| values.last())
        .cloned())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_get_int(
    matches: *const Value,
    name: *const Value,
) -> *mut Value {
    match match_text_value(matches, name).and_then(|value| match value {
        Some(value) => value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| "CLI option value is not a valid integer".to_string()),
        None => Ok(None),
    }) {
        Ok(value) => ok(Value::Optional(
            value.map(|value| Box::new(Value::Int(value))),
        )),
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_get_float(
    matches: *const Value,
    name: *const Value,
) -> *mut Value {
    match match_text_value(matches, name).and_then(|value| match value {
        Some(value) => value
            .parse::<f64>()
            .map(Some)
            .map_err(|_| "CLI option value is not a valid float".to_string()),
        None => Ok(None),
    }) {
        Ok(value) => ok(Value::Optional(value.map(|value| {
            Box::new(Value::Float(ordered_float::OrderedFloat(value)))
        }))),
        Err(e) => err(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_get_bool(
    matches: *const Value,
    name: *const Value,
) -> *mut Value {
    match match_text_value(matches, name).and_then(|value| match value {
        Some(value) => match value.as_str() {
            "true" | "1" | "yes" | "on" => Ok(Some(true)),
            "false" | "0" | "no" | "off" => Ok(Some(false)),
            _ => Err("CLI option value is not a valid boolean".to_string()),
        },
        None => Ok(None),
    }) {
        Ok(value) => ok(Value::Optional(
            value.map(|value| Box::new(Value::Bool(value))),
        )),
        Err(e) => err(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_positional(
    matches: *const Value,
    index: i64,
) -> *mut Value {
    if index < 0 {
        return err("positional index must not be negative");
    }
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let value = lock(&MATCHES)
        .get(&id)
        .and_then(|entry| entry.positionals.get(index as usize))
        .cloned()
        .map(|value| Box::new(Value::String(value)));
    ok(Value::Optional(value))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_subcommand(matches: *const Value) -> *mut Value {
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let value = lock(&MATCHES)
        .get(&id)
        .and_then(|entry| entry.subcommand.clone())
        .map(|value| Box::new(Value::String(value)));
    ok(Value::Optional(value))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_subcommand_matches(matches: *const Value) -> *mut Value {
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let Some(child) = lock(&MATCHES)
        .get(&id)
        .and_then(|entry| entry.subcommand_matches.clone())
    else {
        return ok(Value::Optional(None));
    };
    let child_id = next_id();
    lock(&MATCHES).insert(child_id, *child);
    match owned_object(*MATCHES_TYPE_ID, child_id) {
        Ok(value) => ok(Value::Optional(Some(Box::new(value)))),
        Err(error) => {
            lock(&MATCHES).remove(&child_id);
            err(error)
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_cli_matches_help(matches: *const Value) -> *mut Value {
    let id = match match_entry(matches) {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    match lock(&MATCHES).get(&id) {
        Some(entry) => ok(Value::String(entry.help.clone())),
        None => err("CliMatches is closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_response_files_with_budget, ResponseBudget};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mux-cli-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn response_file_read_stops_at_configured_limit_plus_one() {
        let path = temporary_path("bounded");
        fs::write(&path, b"12345").expect("write response file");
        let mut budget = ResponseBudget::new(32, 8);
        let error = expand_response_files_with_budget(
            vec![format!("@{}", path.display())],
            0,
            4,
            &mut budget,
        )
        .expect_err("oversized response file should fail");
        assert!(error.contains("exceeds the 4-byte limit"));
        fs::remove_file(path).expect("remove response file");
    }

    #[test]
    fn response_file_tokens_preserve_windows_path_separators() {
        assert_eq!(
            super::tokenize_response_file(r"@C:\\Users\\mux\\child.rsp"),
            Ok(vec![r"@C:\Users\mux\child.rsp".to_string()])
        );
    }

    #[test]
    fn nested_response_files_share_byte_and_argument_budgets() {
        let child = temporary_path("child");
        let root = temporary_path("root");
        let child_text = "value";
        fs::write(&child, child_text).expect("write child response file");
        let root_text = format!("@{}\n@{}", child.display(), child.display());
        fs::write(&root, &root_text).expect("write root response file");

        let mut byte_budget = ResponseBudget::new(root_text.len() + child_text.len(), 8);
        let byte_error = expand_response_files_with_budget(
            vec![format!("@{}", root.display())],
            0,
            root_text.len() + 1,
            &mut byte_budget,
        )
        .expect_err("nested response files should share the byte budget");
        assert!(byte_error.contains("aggregate byte limit"));

        let mut argument_budget = ResponseBudget::new(1024, 1);
        let argument_error = expand_response_files_with_budget(
            vec![format!("@{}", root.display())],
            0,
            1024,
            &mut argument_budget,
        )
        .expect_err("nested response files should share the argument budget");
        assert!(argument_error.contains("expanded response arguments"));

        fs::remove_file(root).expect("remove root response file");
        fs::remove_file(child).expect("remove child response file");
    }
}
