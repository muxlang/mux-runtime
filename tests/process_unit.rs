//! Process metadata and explicit command/child/output handle coverage.

use std::ffi::CString;
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::{Duration, Instant};

use mux_runtime::process::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_process_error_detail, mux_process_error_kind};
use mux_runtime::sync_primitives::mux_channel_recv;
use mux_runtime::Value;

#[cfg(unix)]
struct SendValue(*mut Value);

#[cfg(unix)]
// The process runtime treats this as an opaque registry handle and protects
// the underlying child with its own synchronization.
unsafe impl Send for SendValue {}

#[cfg(unix)]
fn wait_with_timeout(handle: SendValue, sender: std::sync::mpsc::Sender<(bool, Duration)>) {
    let started = Instant::now();
    let result = unsafe { mux_process_child_wait_timeout(handle.0, 2_000) };
    let ok = unsafe { mux_result_is_ok(result) };
    assert!(unsafe { mux_rc_dec(result) });
    assert!(unsafe { mux_rc_dec(handle.0) });
    sender.send((ok, started.elapsed())).unwrap();
}

unsafe fn result_data(result: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(result), "expected Ok result: {result:?}");
    let data = mux_result_data(result);
    assert!(!data.is_null());
    assert!(mux_rc_dec(result));
    data
}

unsafe fn command_for(program: &CString) -> *mut Value {
    let command = result_data(mux_process_command_new());
    let configured = mux_process_command_set_program(command, program.as_ptr());
    assert!(mux_result_is_ok(configured));
    assert!(mux_rc_dec(configured));
    command
}

#[test]
fn metadata_is_available() {
    let args = mux_process_args();
    unsafe {
        assert!(matches!(&*args, Value::List(values) if !values.is_empty()));
        assert!(mux_rc_dec(args));
    }
    assert!(mux_process_id() > 0);
    let parent = mux_process_parent_id();
    unsafe {
        assert!(matches!(&*parent, Value::Optional(_)));
    }
    unsafe {
        assert!(mux_rc_dec(parent));
    }
    let executable = mux_process_executable();
    unsafe {
        let path = result_data(executable);
        assert!(matches!(&*path, Value::String(value) if !value.is_empty()));
        assert!(mux_rc_dec(path));
    }
}

#[test]
fn command_output_and_child_lifecycle_work() {
    let executable = std::env::current_exe().expect("test executable path");
    let executable = CString::new(executable.to_str().expect("UTF-8 test executable path"))
        .expect("test executable has no NUL");
    unsafe {
        let command = command_for(&executable);
        let help = CString::new("--help").unwrap();
        let arg_result = mux_process_command_arg(command, help.as_ptr());
        assert!(mux_result_is_ok(arg_result));
        assert!(mux_rc_dec(arg_result));

        let output_result = mux_process_command_output(command);
        let output = result_data(output_result);
        let status = result_data(mux_process_output_status(output));
        assert!(matches!(&*status, Value::Int(0)));
        assert!(mux_rc_dec(status));
        let stdout = result_data(mux_process_output_stdout(output));
        let stdout_nonempty = matches!(&*stdout, Value::Bytes(bytes) if !bytes.is_empty());
        assert!(mux_rc_dec(stdout));
        let stderr = result_data(mux_process_output_stderr(output));
        let stderr_nonempty = matches!(&*stderr, Value::Bytes(bytes) if !bytes.is_empty());
        assert!(stdout_nonempty || stderr_nonempty);
        assert!(mux_rc_dec(stderr));
        assert!(mux_rc_dec(output));
        assert!(mux_rc_dec(command));
    }
}

#[test]
fn command_environment_and_spawn_are_explicit() {
    let executable = std::env::current_exe().expect("test executable path");
    let executable = CString::new(executable.to_str().expect("UTF-8 test executable path"))
        .expect("test executable has no NUL");
    let key = CString::new("MUX_PROCESS_TEST_KEY").unwrap();
    let value = CString::new("value").unwrap();
    unsafe {
        let command = command_for(&executable);
        let env_result = mux_process_command_env(command, key.as_ptr(), value.as_ptr());
        assert!(mux_result_is_ok(env_result));
        assert!(mux_rc_dec(env_result));
        let cwd = std::env::current_dir().expect("current directory");
        let cwd = CString::new(cwd.to_str().expect("UTF-8 current directory")).unwrap();
        let cwd_result = mux_process_command_cwd(command, cwd.as_ptr());
        assert!(mux_result_is_ok(cwd_result));
        assert!(mux_rc_dec(cwd_result));
        let help = CString::new("--help").unwrap();
        let arg_result = mux_process_command_arg(command, help.as_ptr());
        assert!(mux_result_is_ok(arg_result));
        assert!(mux_rc_dec(arg_result));

        let child = result_data(mux_process_command_spawn(command));
        let status = result_data(mux_process_child_wait(child));
        assert!(matches!(&*status, Value::Int(0)));
        assert!(mux_rc_dec(status));
        assert!(mux_rc_dec(child));
        assert!(mux_rc_dec(command));
    }
}

#[test]
fn malformed_commands_return_errors() {
    let empty = CString::new("").unwrap();
    unsafe {
        let command = result_data(mux_process_command_new());
        let result = mux_process_command_set_program(command, empty.as_ptr());
        assert!(!mux_result_is_ok(result));
        let error = mux_result_data(result);
        let kind = mux_process_error_kind(error);
        let detail = mux_process_error_detail(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes()));
        assert!(matches!(&*detail, Value::String(value) if value.contains("empty")));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(command));
    }
    let missing = CString::new("mux-command-that-does-not-exist").unwrap();
    unsafe {
        let command = command_for(&missing);
        let status = mux_process_command_status(command);
        assert!(!mux_result_is_ok(status));
        let error = mux_result_data(status);
        let kind = mux_process_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 2_i32.to_ne_bytes()));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(status));

        // Resource handles are type-specific. Passing a Command where an
        // Output is required must fail cleanly instead of interpreting the
        // shared handle id through the wrong registry.
        let wrong_type = mux_process_output_status(command);
        assert!(!mux_result_is_ok(wrong_type));
        assert!(mux_rc_dec(wrong_type));
        assert!(mux_rc_dec(command));
    }
}

#[test]
fn child_wait_timeout_reports_completion() {
    let executable = std::env::current_exe().expect("test executable path");
    let executable = CString::new(executable.to_str().expect("UTF-8 test executable path"))
        .expect("test executable has no NUL");
    unsafe {
        let command = command_for(&executable);
        let help = CString::new("--help").unwrap();
        let arg_result = mux_process_command_arg(command, help.as_ptr());
        assert!(mux_result_is_ok(arg_result));
        assert!(mux_rc_dec(arg_result));
        let child = result_data(mux_process_command_spawn(command));
        let status = result_data(mux_process_child_wait_timeout(child, 1_000));
        assert!(
            matches!(&*status, Value::Optional(Some(value)) if matches!(value.as_ref(), Value::Int(0)))
        );
        assert!(mux_rc_dec(status));
        assert!(mux_rc_dec(child));
        assert!(mux_rc_dec(command));
    }
}

#[test]
fn child_pipe_configuration_is_explicit() {
    let executable = std::env::current_exe().expect("test executable path");
    let executable = CString::new(executable.to_str().expect("UTF-8 test executable path"))
        .expect("test executable has no NUL");
    unsafe {
        let command = command_for(&executable);
        let help = CString::new("--help").unwrap();
        let arg_result = mux_process_command_arg(command, help.as_ptr());
        assert!(mux_result_is_ok(arg_result));
        assert!(mux_rc_dec(arg_result));
        for configure in [
            mux_process_command_stdin_piped,
            mux_process_command_stdout_piped,
            mux_process_command_stderr_piped,
        ] {
            let result = configure(command);
            assert!(mux_result_is_ok(result));
            assert!(mux_rc_dec(result));
        }
        let child = result_data(mux_process_command_spawn(command));
        let close = mux_process_child_close_stdin(child);
        assert!(mux_result_is_ok(close));
        assert!(mux_rc_dec(close));
        let stdout = result_data(mux_process_child_read_stdout(child, 16 * 1024));
        assert!(matches!(&*stdout, Value::Bytes(_)));
        assert!(mux_rc_dec(stdout));
        let stderr = result_data(mux_process_child_read_stderr(child, 16 * 1024));
        assert!(matches!(&*stderr, Value::Bytes(_)));
        assert!(mux_rc_dec(stderr));
        let status = result_data(mux_process_child_wait(child));
        assert!(matches!(&*status, Value::Int(0)));
        assert!(mux_rc_dec(status));
        assert!(mux_rc_dec(child));
        assert!(mux_rc_dec(command));
    }
}

#[cfg(unix)]
#[test]
fn child_wait_timeout_does_not_block_concurrent_pipe_reads() {
    // `wait_timeout` must not hold the process registry lock while polling a
    // live child. Otherwise a child that is writing to a piped stream can
    // never be drained by another alias until the timeout expires.
    unsafe {
        let commandline = CString::new("yes mux-process").unwrap();
        let command = result_data(mux_process_command_shell(commandline.as_ptr()));
        let configured = mux_process_command_stdout_piped(command);
        assert!(mux_result_is_ok(configured));
        assert!(mux_rc_dec(configured));
        let child = result_data(mux_process_command_spawn(command));
        let wait_alias = SendValue(mux_rc_alloc((*child).clone()));
        let (sender, receiver) = mpsc::channel();
        let wait_thread = std::thread::spawn(move || wait_with_timeout(wait_alias, sender));

        std::thread::sleep(Duration::from_millis(20));
        let read_started = Instant::now();
        let read = mux_process_child_read_stdout(child, 1024);
        let read_elapsed = read_started.elapsed();
        assert!(mux_result_is_ok(read));
        assert!(mux_rc_dec(read));
        assert!(read_elapsed < Duration::from_millis(500));

        let killed = mux_process_child_kill_group(child);
        assert!(mux_result_is_ok(killed));
        assert!(mux_rc_dec(killed));
        let (wait_ok, _) = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(wait_ok);
        wait_thread.join().unwrap();
        assert!(mux_rc_dec(child));
        assert!(mux_rc_dec(command));
    }
}

#[test]
fn shell_execution_is_explicitly_named() {
    let commandline = CString::new("echo mux-process-shell").unwrap();
    unsafe {
        let command = result_data(mux_process_command_shell(commandline.as_ptr()));
        let output = result_data(mux_process_command_output(command));
        let status = result_data(mux_process_output_status(output));
        assert!(matches!(&*status, Value::Int(0)));
        assert!(mux_rc_dec(status));
        let stdout = result_data(mux_process_output_stdout(output));
        assert!(matches!(&*stdout, Value::Bytes(bytes) if !bytes.is_empty()));
        assert!(mux_rc_dec(stdout));
        assert!(mux_rc_dec(output));
        assert!(mux_rc_dec(command));
    }
}

#[test]
fn process_pool_submits_and_delivers_output() {
    let executable = std::env::current_exe().expect("test executable path");
    let executable = CString::new(executable.to_str().expect("UTF-8 test executable path"))
        .expect("test executable has no NUL");
    unsafe {
        let pool = result_data(mux_process_pool_new());
        let command = command_for(&executable);
        let help = CString::new("--help").unwrap();
        let arg_result = mux_process_command_arg(command, help.as_ptr());
        assert!(mux_result_is_ok(arg_result));
        assert!(mux_rc_dec(arg_result));

        // Submission returns a Result containing a result channel. Receiving
        // from that channel yields an Optional containing the worker's
        // Result<Output, string> value.
        let channel = result_data(mux_process_pool_submit(pool, command));
        let received = result_data(mux_channel_recv(channel));
        assert!(matches!(
            &*received,
            Value::Optional(Some(value))
                if matches!(value.as_ref(), Value::Result(Ok(output))
                    if matches!(output.as_ref(), Value::Object(_)))
        ));
        assert!(mux_rc_dec(received));
        assert!(mux_rc_dec(channel));

        let close = mux_process_pool_close(pool);
        assert!(mux_result_is_ok(close));
        assert!(mux_rc_dec(close));
        assert!(mux_rc_dec(command));
        assert!(mux_rc_dec(pool));
    }
}

#[cfg(unix)]
#[test]
fn captured_output_is_bounded() {
    let commandline = CString::new("head -c 17000000 /dev/zero").unwrap();
    unsafe {
        let command = result_data(mux_process_command_shell(commandline.as_ptr()));
        let output = mux_process_command_output(command);
        assert!(!mux_result_is_ok(output));
        assert!(mux_rc_dec(output));
        assert!(mux_rc_dec(command));
    }
}

#[cfg(unix)]
#[test]
fn child_group_kill_terminates_the_private_process_group() {
    let commandline = CString::new("sleep 30").unwrap();
    unsafe {
        let command = result_data(mux_process_command_shell(commandline.as_ptr()));
        let child = result_data(mux_process_command_spawn(command));
        let killed = mux_process_child_kill_group(child);
        assert!(mux_result_is_ok(killed));
        assert!(mux_rc_dec(killed));
        let status = result_data(mux_process_child_wait(child));
        assert!(matches!(&*status, Value::Int(code) if *code != 0));
        assert!(mux_rc_dec(status));
        assert!(mux_rc_dec(child));
        assert!(mux_rc_dec(command));
    }
}
