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
- **No panics on internal invariants.** `unreachable!`, `panic!`, `expect`,
  `.unwrap()` and indexing that can go out of range are all the same thing: a
  compiled Mux program aborts, and the program has no way to handle it. That is
  a container bug becoming a crash in someone else's code. This is stricter
  than it is for the compiler, which is a tool a developer runs and where
  failing loudly is fine.

  A state you believe impossible still needs a total answer. Pick the one that
  is correct if it somehow happened - a lookup says "absent", an iterator ends,
  an allocator takes a fresh slot - and pair it with `debug_assert!`, so tests
  and debug builds still fail loudly where someone can act on it. `mux_panic_*`
  is for reporting a *program's* error (a missing map key, an out-of-bounds
  index) to its author, not for the runtime's own consistency.

  The one deliberate exception is the `rc-leak-check` feature, whose whole
  purpose is to assert at exit; it sits outside `full` so no shipped runtime
  carries it.
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
  the final release drops one reference to each capture cell and frees the
  closure. `mux_sync_spawn` retains the closure for the worker thread, which
  releases it when its body finishes (normal return or panic-unwind).
- **A capture cell is shared, not owned by one closure.** Each capture slot
  points at a `[refcount | *mut Value]` cell from `mux_cell_alloc`, managed by
  `mux_cell_retain` / `mux_cell_release`. The cell IS the captured variable's
  storage: the variable and every closure capturing it name the same cell, which
  is what makes a write through one visible to the others. It is reference
  counted because a variable can be captured by two closures, an ordinary local
  outlives the closures capturing it, and a returned closure outlives the
  function that declared it. A closure freeing its cells outright is what made a
  capture stop being shared at a block boundary (mux-compiler#384).

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
  ONE step per repo and no publish: merge here, then move the compiler's pin with
  `cargo update -p mux-runtime`. See
  [ADR 0004](https://github.com/muxlang/mux-context/blob/main/docs/decisions/0004-runtime-resolved-from-source.md).
- Local coupled dev: build here and point `MUX_RUNTIME_LIB` at the resulting
  `target/debug/libmux_runtime.a`. It is the first thing runtime resolution
  consults, so it overrides the archive built from the locked commit - and a
  leftover value keeps overriding it until unset.
- The compiler always links the `full` feature set; it no longer builds a
  feature-trimmed runtime, so there is no feature-parity test to keep in sync.

## Release

Not released on its own cadence. crates.io is frozen (versions through 0.5.0 stay
published, no new ones), and `mux-compiler` consumes this repo as a git dependency
pinned by its `Cargo.lock` - so merging to `main` is what makes a change
available. The `version` field is inert while the channel is frozen; record
changes under an `## [Unreleased]` changelog heading. Full steps:
[muxlang/mux-context release process](https://github.com/muxlang/mux-context/blob/main/docs/release-process.md#mux-runtime).

## Related repos

- `mux-compiler` - the compiler/CLI that links this runtime.
- `mux-website` - documentation.
- `muxlang/mux-context` - cross-repo architecture, design notes, glossary, releases.

**Add to this document as you learn vital information.**
