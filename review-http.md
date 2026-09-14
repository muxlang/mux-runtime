# HTTP review fixes

## Fixes

- `src/net.rs` now reaps finished HTTP worker-pool actor handles during the
  accept loop, keeping handle ownership bounded by the active-actor limit.
  Remaining actors are joined during shutdown. Actor startup failures release
  their permit, stop the pool, and use the same cleanup path.
- Worker receives use a timed poll that releases the receiver mutex between
  polls. Pool sends and response receives use the named poll interval, so
  cancellation and channel closure do not leave those operations blocked.
- Pool startup keeps closure snapshots under release guards and the listener
  under its mode guard. The pool listener binding is mutable where
  `restore(&mut self)` is called. Early startup errors therefore release copied
  handlers and restore the listener mode.
- Static files now open the configured root as a `cap_std::fs::Dir` and open
  only a validated relative path through that capability. Decoded segments
  reject empty, dot, dot-dot, slash, backslash, NUL, and colon components, so
  encoded separators cannot become extra path syntax. The opened file still
  gets metadata validation and a one-byte-over-limit bounded read. ETags are
  SHA-256 content validators, and `If-None-Match` handles wildcard, weak, and
  comma-separated validators.
- HTTP/3 records whether request dispatch succeeded. HTTP/2 or HTTP/1.1
  fallback is now limited to pre-dispatch transport connection failures.
  Post-dispatch errors are returned to the caller, so a POST is not replayed
  after a partial HTTP/3 send.
- HTTP/3 connection setup is the only part bounded by the connect timeout.
  The established actor now runs until its request sender is dropped. The
  sender is moved to the returned transport, and a test-only completion flag
  verifies that dropping the final transport stops the actor.
- HTTP server connections now apply the configured positive read timeout to
  writes as well. A zero read timeout leaves both directions blocking, which
  preserves the existing zero-value semantics while bounding graceful drains
  when a timeout is configured.
- The CORS/static-file regression now resolves its request handle before
  locking the request registry. The previous order attempted to reacquire
  that non-reentrant mutex and hung the test indefinitely.

## Regression coverage

The Rust regressions are in `src/net.rs` and cover conditional ETags, same-size
file changes, cancellation-aware pool channel operations, incremental actor
handle reaping, HTTP/3 fallback classification, the split HTTP/3 setup and
actor lifetimes, explicit HTTP/2 rustls provider selection without panic, and
static directory, symlink-escape, and decoded-separator rejection.

## Verification

- `rustfmt --edition 2021 --check src/net.rs tests/net_unit.rs` was run and
  reported formatting differences in the already-dirty HTTP files; it was not
  applied because that would rewrite unrelated changes. The stream files pass
  the same edition-2021 check.
- `git diff --check -- src/net.rs tests/net_unit.rs` was run; it does not
  inspect untracked files.
- A source scan found no remaining `lock_requests`/`request_handle` nesting in
  `src/net.rs`.
- Cargo build, check, test, and clippy commands were not run by request. The
  Rust regressions above remain unrun.

## Remaining issues

- The symlink escape regression is Unix-gated because creating symlinks on
  Windows is privilege-dependent. Windows CI should exercise the equivalent
  reparse-point cases; the production open remains through cap-std on Windows.
- The workspace contains many unrelated dirty and untracked files from earlier
  work. They were preserved and not reviewed or changed here.
