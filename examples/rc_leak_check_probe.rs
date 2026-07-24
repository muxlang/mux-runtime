//! Exercises the `rc-leak-check` feature end to end so a regression fails
//! mux-runtime's own CI, not only downstream in the compiler (issue #22, part 1).
//!
//! Built with `--features rc-leak-check`, allocating a reference-counted block
//! arms an `atexit` handler that asserts the live-block count is zero at exit.
//! This probe takes one argument:
//!
//! - `clean` allocates one block and releases it, so the count returns to zero
//!   and the process exits 0.
//! - `leak` allocates one block and never releases it, so the handler reports a
//!   live block and exits 101.
//!
//! The CI leg runs both and asserts those exit codes, so a never-released RC
//! allocation in runtime code is caught here directly. Without the feature the
//! counter is inert and both modes exit 0; the leg always passes the feature.

use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::Value;

fn main() {
    // Validate the mode before allocating anything, so an unknown argument exits
    // cleanly rather than arming the checker and reporting a false leak.
    let mode = std::env::args().nth(1).unwrap_or_default();
    let release = match mode.as_str() {
        "clean" => true,
        "leak" => false,
        other => {
            eprintln!("usage: rc_leak_check_probe <clean|leak> (got {other:?})");
            std::process::exit(2);
        }
    };

    // Allocate one reference-counted block. Under `rc-leak-check` this arms the
    // atexit handler and increments the live-block counter.
    let value = mux_rc_alloc(Value::Int(42));
    if value.is_null() {
        eprintln!("rc_leak_check_probe: allocation failed");
        std::process::exit(2);
    }

    if release {
        // `clean`: release the block so the counter returns to zero and the
        // atexit check passes (exit 0). `leak`: leave it live so the handler
        // reports it and exits 101.
        let _freed = mux_rc_dec(value);
    }
}
