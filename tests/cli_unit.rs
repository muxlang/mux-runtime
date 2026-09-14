use mux_runtime::cli::{
    mux_cli_matches_get, mux_cli_matches_get_bool, mux_cli_matches_get_float,
    mux_cli_matches_get_int, mux_cli_matches_has, mux_cli_matches_positional,
    mux_cli_matches_subcommand, mux_cli_matches_subcommand_matches, mux_cli_parser_add_option,
    mux_cli_parser_add_positional, mux_cli_parser_add_subcommand, mux_cli_parser_completion,
    mux_cli_parser_help, mux_cli_parser_manpage, mux_cli_parser_new, mux_cli_parser_parse,
    mux_cli_parser_set_option_alias, mux_cli_parser_set_option_conflicts,
    mux_cli_parser_set_option_default, mux_cli_parser_set_option_group,
    mux_cli_parser_set_option_requires, mux_cli_parser_set_program,
    mux_cli_parser_set_response_files,
};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::std::{mux_cli_error_detail, mux_cli_error_kind};
use mux_runtime::Value;

fn text(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

#[test]
fn parser_returns_deterministic_shell_completion_text() {
    let parser = mux_cli_parser_new();
    let name = text("verbose");
    let short = text("v");
    let configured = unsafe { mux_cli_parser_add_option(parser, name, short, false, false) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(short);
    }
    let shell = text("bash");
    let completion = unsafe { mux_cli_parser_completion(parser, shell) };
    let value = take_ok(completion);
    assert!(
        matches!(unsafe { &*value }, Value::String(text) if text.contains("--verbose") && text.contains("-v"))
    );
    unsafe {
        mux_rc_dec(value);
        mux_rc_dec(shell);
        mux_rc_dec(parser);
    }
}
fn bool_value(value: bool) -> *mut Value {
    mux_rc_alloc(Value::Bool(value))
}
fn list(values: &[&str]) -> *mut Value {
    mux_rc_alloc(Value::List(
        values
            .iter()
            .map(|v| Value::String((*v).to_string()))
            .collect(),
    ))
}
fn take_ok(ptr: *mut Value) -> *mut Value {
    assert!(unsafe { mux_result_is_ok(ptr) });
    let data = unsafe { mux_result_data(ptr) };
    unsafe {
        assert!(mux_rc_dec(ptr));
    }
    data
}

#[test]
fn parser_applies_cli_values_and_defaults() {
    let parser = mux_cli_parser_new();
    let name = text("count");
    let short = text("c");
    let takes = bool_value(true);
    let required = bool_value(false);
    let configured = unsafe { mux_cli_parser_add_option(parser, name, short, true, false) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(short);
        mux_rc_dec(takes);
        mux_rc_dec(required);
    }
    let name = text("count");
    let default = text("1");
    let configured = unsafe { mux_cli_parser_set_option_default(parser, name, default) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(default);
    }
    let name = text("count");
    let alias = text("number");
    let configured = unsafe { mux_cli_parser_set_option_alias(parser, name, alias) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(alias);
    }
    let name = text("count");
    let group = text("numeric");
    let configured = unsafe { mux_cli_parser_set_option_group(parser, name, group) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(group);
    }
    let help = take_ok(unsafe { mux_cli_parser_help(parser) });
    assert!(
        matches!(unsafe { &*help }, Value::String(text) if text.contains("numeric:") && text.contains("--count"))
    );
    unsafe {
        mux_rc_dec(help);
    }
    let manpage = take_ok(unsafe { mux_cli_parser_manpage(parser) });
    assert!(
        matches!(unsafe { &*manpage }, Value::String(text) if text.contains(".SS numeric") && text.contains(".B --count"))
    );
    unsafe {
        mux_rc_dec(manpage);
    }
    let positional_name = text("input");
    let configured = unsafe { mux_cli_parser_add_positional(parser, positional_name, true) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(positional_name);
    }

    let args = list(&["--number", "3", "file.txt"]);
    let matches = take_ok(unsafe { mux_cli_parser_parse(parser, args) });
    unsafe {
        mux_rc_dec(args);
    }
    let count_name = text("count");
    let count = take_ok(unsafe { mux_cli_matches_get(matches, count_name) });
    assert!(
        matches!(unsafe { &*count }, Value::Optional(Some(value)) if matches!(value.as_ref(), Value::String(value) if value == "3"))
    );
    unsafe {
        mux_rc_dec(count);
        mux_rc_dec(count_name);
    }
    let has_name = text("count");
    let has = take_ok(unsafe { mux_cli_matches_has(matches, has_name) });
    assert!(matches!(unsafe { &*has }, Value::Bool(true)));
    unsafe {
        mux_rc_dec(has);
        mux_rc_dec(has_name);
    }
    let position = take_ok(unsafe { mux_cli_matches_positional(matches, 0) });
    assert!(
        matches!(unsafe { &*position }, Value::Optional(Some(value)) if matches!(value.as_ref(), Value::String(value) if value == "file.txt"))
    );
    unsafe {
        mux_rc_dec(position);
        mux_rc_dec(matches);
        mux_rc_dec(parser);
    }
}

#[test]
fn parser_enforces_conflicts_and_typed_accessors() {
    let parser = mux_cli_parser_new();
    for (name, short, takes_value) in [
        ("count", "c", true),
        ("verbose", "v", false),
        ("quiet", "q", false),
        ("ratio", "r", true),
    ] {
        let n = text(name);
        let s = text(short);
        let configured = unsafe { mux_cli_parser_add_option(parser, n, s, takes_value, false) };
        assert!(unsafe { mux_result_is_ok(configured) });
        unsafe {
            mux_rc_dec(configured);
            mux_rc_dec(n);
            mux_rc_dec(s);
        }
    }
    let name = text("verbose");
    let other = text("quiet");
    let configured = unsafe { mux_cli_parser_set_option_conflicts(parser, name, other) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(other);
    }
    let name = text("count");
    let required = text("verbose");
    let configured = unsafe { mux_cli_parser_set_option_requires(parser, name, required) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(required);
    }

    let args = list(&["--count", "7", "--ratio", "1.5", "--verbose"]);
    let matches = take_ok(unsafe { mux_cli_parser_parse(parser, args) });
    unsafe {
        mux_rc_dec(args);
    }
    let name = text("count");
    let value = take_ok(unsafe { mux_cli_matches_get_int(matches, name) });
    assert!(
        matches!(unsafe { &*value }, Value::Optional(Some(v)) if matches!(v.as_ref(), Value::Int(7)))
    );
    unsafe {
        mux_rc_dec(value);
        mux_rc_dec(name);
    }
    let name = text("ratio");
    let value = take_ok(unsafe { mux_cli_matches_get_float(matches, name) });
    assert!(
        matches!(unsafe { &*value }, Value::Optional(Some(v)) if matches!(v.as_ref(), Value::Float(v) if (v.into_inner() - 1.5).abs() < f64::EPSILON))
    );
    unsafe {
        mux_rc_dec(value);
        mux_rc_dec(name);
    }
    let name = text("verbose");
    let value = take_ok(unsafe { mux_cli_matches_get_bool(matches, name) });
    assert!(
        matches!(unsafe { &*value }, Value::Optional(Some(v)) if matches!(v.as_ref(), Value::Bool(true)))
    );
    unsafe {
        mux_rc_dec(value);
        mux_rc_dec(name);
    }
    unsafe {
        mux_rc_dec(matches);
    }

    let args = list(&["--count", "1"]);
    let missing = unsafe { mux_cli_parser_parse(parser, args) };
    assert!(unsafe { mux_result_is_err(missing) });
    let error = unsafe { mux_result_data(missing) };
    let kind = unsafe { mux_cli_error_kind(error) };
    let detail = unsafe { mux_cli_error_detail(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if i32::from_ne_bytes(value.as_ref().try_into().unwrap()) == 1)
    );
    assert!(matches!(unsafe { &*detail }, Value::String(value) if value.contains("requires")));
    unsafe {
        mux_rc_dec(kind);
        mux_rc_dec(detail);
        mux_rc_dec(error);
        mux_rc_dec(missing);
        mux_rc_dec(args);
    }

    let args = list(&["--verbose", "--quiet"]);
    let conflict = unsafe { mux_cli_parser_parse(parser, args) };
    assert!(unsafe { mux_result_is_err(conflict) });
    unsafe {
        mux_rc_dec(conflict);
        mux_rc_dec(args);
        mux_rc_dec(parser);
    }
}

#[test]
fn parser_expands_opt_in_response_files_with_quoting_and_nesting() {
    let parser = mux_cli_parser_new();
    let name = text("output");
    let short = text("o");
    let configured = unsafe { mux_cli_parser_add_option(parser, name, short, true, false) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
        mux_rc_dec(short);
    }
    let enabled = unsafe { mux_cli_parser_set_response_files(parser, true) };
    assert!(unsafe { mux_result_is_ok(enabled) });
    unsafe { mux_rc_dec(enabled) };

    let root = std::env::temp_dir().join(format!("mux-cli-response-{}", std::process::id()));
    let nested = root.with_extension("nested");
    std::fs::write(&nested, "--output 'nested value'").expect("write nested response file");
    std::fs::write(
        &root,
        format!("@{} # include nested\n@@literal", nested.display()),
    )
    .expect("write response file");

    let argument = text(&format!("@{}", root.display()));
    let args = mux_rc_alloc(Value::List(vec![unsafe { (&*argument).clone() }]));
    let matches = take_ok(unsafe { mux_cli_parser_parse(parser, args) });
    let option = text("output");
    let value = take_ok(unsafe { mux_cli_matches_get(matches, option) });
    assert!(
        matches!(unsafe { &*value }, Value::Optional(Some(value)) if **value == Value::String("nested value".to_string()))
    );

    unsafe {
        mux_rc_dec(value);
        mux_rc_dec(option);
        mux_rc_dec(matches);
        mux_rc_dec(args);
        mux_rc_dec(argument);
        mux_rc_dec(parser);
    }
    std::fs::remove_file(root).expect("remove response file");
    std::fs::remove_file(nested).expect("remove nested response file");
}

#[test]
fn response_files_reject_invalid_utf8_content() {
    let parser = mux_cli_parser_new();
    let enabled = unsafe { mux_cli_parser_set_response_files(parser, true) };
    assert!(unsafe { mux_result_is_ok(enabled) });
    unsafe { mux_rc_dec(enabled) };

    let root = std::env::temp_dir().join(format!("mux-cli-response-limit-{}", std::process::id()));
    std::fs::write(&root, [0xff, 0]).expect("write invalid response file");
    let argument = text(&format!("@{}", root.display()));
    let args = mux_rc_alloc(Value::List(vec![unsafe { (&*argument).clone() }]));
    let result = unsafe { mux_cli_parser_parse(parser, args) };
    assert!(unsafe { mux_result_is_err(result) });
    unsafe {
        mux_rc_dec(result);
        mux_rc_dec(args);
        mux_rc_dec(argument);
        mux_rc_dec(parser);
    }
    std::fs::remove_file(root).expect("remove response file");
}

#[test]
fn nested_subcommand_parser_returns_nested_matches() {
    let parser = mux_cli_parser_new();
    let name = text("serve");
    let about = text("run the server");
    let child_result = unsafe { mux_cli_parser_add_subcommand(parser, name, about) };
    let child = take_ok(child_result);
    unsafe {
        mux_rc_dec(name);
        mux_rc_dec(about);
    }

    let help = take_ok(unsafe { mux_cli_parser_help(parser) });
    assert!(
        matches!(unsafe { &*help }, Value::String(text) if text.contains("serve") && text.contains("run the server"))
    );
    unsafe {
        mux_rc_dec(help);
    }

    let option = text("port");
    let short = text("p");
    let configured = unsafe { mux_cli_parser_add_option(child, option, short, true, true) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(option);
        mux_rc_dec(short);
    }

    let args = list(&["serve", "--port", "8080"]);
    let matches = take_ok(unsafe { mux_cli_parser_parse(parser, args) });
    let command = take_ok(unsafe { mux_cli_matches_subcommand(matches) });
    assert!(
        matches!(unsafe { &*command }, Value::Optional(Some(value)) if **value == Value::String("serve".to_string()))
    );
    let nested = take_ok(unsafe { mux_cli_matches_subcommand_matches(matches) });
    let nested_matches = match unsafe { &*nested } {
        Value::Optional(Some(value)) => mux_rc_alloc((**value).clone()),
        other => panic!("expected nested matches, got {other:?}"),
    };
    let port = text("port");
    let port_value = take_ok(unsafe { mux_cli_matches_get_int(nested_matches, port) });
    assert!(
        matches!(unsafe { &*port_value }, Value::Optional(Some(value)) if **value == Value::Int(8080))
    );

    unsafe {
        mux_rc_dec(port_value);
        mux_rc_dec(port);
        mux_rc_dec(nested_matches);
        mux_rc_dec(nested);
        mux_rc_dec(command);
        mux_rc_dec(matches);
        mux_rc_dec(args);
        mux_rc_dec(child);
        mux_rc_dec(parser);
    }
}

#[test]
fn parser_returns_deterministic_manpage_text() {
    let parser = mux_cli_parser_new();
    let name = text("backup");
    let configured = unsafe { mux_cli_parser_set_program(parser, name) };
    assert!(unsafe { mux_result_is_ok(configured) });
    unsafe {
        mux_rc_dec(configured);
        mux_rc_dec(name);
    }
    let manpage = unsafe { mux_cli_parser_manpage(parser) };
    let value = take_ok(manpage);
    assert!(
        matches!(unsafe { &*value }, Value::String(text) if text.contains(".TH \"backup\" 1") && text.contains(".SH OPTIONS"))
    );
    unsafe {
        mux_rc_dec(value);
        mux_rc_dec(parser);
    }
}
