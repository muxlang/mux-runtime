# mux-runtime: AI Agent Guidelines

The runtime library for the Mux language, published to crates.io as `mux-runtime`.
Compiled Mux programs link against it. Part of the multi-repo
[muxlang](https://github.com/muxlang) ecosystem.

> Cross-repo architecture, design rationale, the feature map, and the release
> process live in [muxlang/mux-context](https://github.com/muxlang/mux-context).

## Critical Rules

- **No special characters** - avoid em-dashes, emojis, or other non-ASCII in code,
  comments, or commit messages.
- **Plain, stable Rust - NO LLVM.** Do not add an LLVM/clang dependency. The whole
  point of this repo is that runtime/stdlib work needs only a Rust toolchain. Any
  recent stable Rust builds it (CI pins 1.93.1 for reproducibility).
- **No clippy warnings**: `cargo clippy --all-targets --all-features -- -D warnings`.
- **Idiomatic Rust**: `Result<T, E>`, the `?` operator, no `.unwrap()` outside
  tests, document public APIs with `///`.
- **Understand existing code first**; follow existing patterns.
- **Remove outdated comments.**

## What this is

The link-time runtime for compiled Mux programs: reference counting, UTF-8 string
ops, collections (list/map/set), type conversions, and standard-library support.
It exposes a C-ABI FFI surface consumed by compiler-generated code.

## Memory & ownership ABI

Every heap `Value` is `[RefHeader | Value]`, where `RefHeader` is an atomic
counter (`AtomicUsize` - `u64`-sized on 64-bit targets). `mux_rc_inc` /
`mux_rc_dec` adjust the count and `mux_rc_dec` (null-safe) frees at zero. The
compiler emits the inc/dec calls; the runtime just implements them. The full
ownership model (borrowed vs owned values, statement-temporary cleanup,
value-semantics copies, program-exit global teardown) lives in
`mux-context/docs/design/memory.md` - keep this ABI aligned with it.

Conventions that matter when adding or changing FFI functions:

- **Collections, object fields, and value wrappers take independent copies.**
  Insert/push helpers and wrappers `clone()` their argument *without consuming it*
  (`mux_list_push_back`, `mux_map_get`, `mux_result_ok_value`,
  `mux_optional_some_value`, `mux_new_tuple`, ...), so the caller keeps ownership
  of what it passed and releases it itself - including any intermediate value it
  allocated only to wrap. Do not store a caller pointer without cloning.
- **C strings are explicitly owned or borrowed.** Helpers returning `*mut c_char`
  (`*_to_string`, `mux_string_concat`, `mux_value_get_string`) return an **owned**
  string the caller frees with `mux_free_string`, and only **borrow** any
  `*const c_char` inputs - e.g. `mux_string_concat` reads its two operand strings
  and frees neither, so the caller frees all three. For wrapping an owned C string
  back into a Mux value use `mux_new_string_from_owned_cstr` (takes ownership,
  frees the input after copying); `mux_new_string_from_cstr` only **borrows** its
  input and is for compiler-owned static string data. Mixing these up double-frees
  or leaks - see `src/string.rs`.
- **Closures are reference-counted separately from `Value`s.** A closure is
  `[refcount | fn_ptr | captures_ptr | capture_count]` managed by
  `mux_closure_retain` / `mux_closure_release` (atomic header, `src/closure.rs`);
  the final release walks the `capture_count` heap cells, drops one reference per
  captured value, and frees the closure. `mux_sync_spawn` retains the closure for
  the worker thread, which releases it when its body finishes (normal return or
  panic-unwind).

## Features

`default = ["full"]`. Optional: `json`, `csv`, `net`, `sql`, `sync`. Keep the
feature gating intact - the compiler enables only the features a program imports.

## Development

```bash
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

No LLVM/clang needed. CI runs fmt + clippy + tests + a SonarQube scan.

## Compiler coupling (important)

- The compiler links the BUILT library; it does NOT import this crate's Rust code.
- Changing exported FFI symbols/signatures is a coupled change with the compiler.
- A coupled change (a new language feature needing a new runtime function) ships in
  TWO steps: publish the runtime (new version) first, then bump the compiler's
  `mux-runtime = "X.Y"` pin.
- Local coupled dev: check this out as a sibling of `mux-compiler` (resolved as
  `../mux-runtime` automatically) or set `MUX_RUNTIME_SRC`.
- The compiler's `full_runtime_features()` parity test reads this `Cargo.toml`'s
  `full` feature list - keep them in sync.

## Release

Versioned independently of the compiler. Published manually from a local checkout
(MAINTAINER-ONLY, no token in CI). Full steps:
[muxlang/mux-context release process](https://github.com/muxlang/mux-context/blob/main/docs/release-process.md#mux-runtime).
Publish the runtime before bumping the compiler's `mux-runtime` pin.

## Related repos

- `mux-compiler` - the compiler/CLI that links this runtime.
- `mux-website` - documentation.
- `muxlang/mux-context` - cross-repo architecture, design notes, glossary, releases.

**Add to this document as you learn vital information.**
