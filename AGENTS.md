# mux-runtime: AI Agent Guidelines

The runtime library for the Mux language, published to crates.io as `mux-runtime`.
Compiled Mux programs link against it. Part of the multi-repo
[muxlang](https://github.com/muxlang) ecosystem.

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

Versioned independently of the compiler. Update the version in `Cargo.toml`, then
push a `vX.Y.Z` tag to trigger the crates.io publish workflow (needs the
`CARGO_REGISTRY_TOKEN` org secret).

## Related repos

- `mux-compiler` - the compiler/CLI that links this runtime.
- `mux-website` - documentation.

**Add to this document as you learn vital information.**
