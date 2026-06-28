# mux-runtime

The runtime library for the [Mux programming language](https://github.com/muxlang),
published to crates.io as [`mux-runtime`](https://crates.io/crates/mux-runtime).

Compiled Mux programs link against this library at compile time. It is plain,
stable Rust with **no LLVM dependency** - so runtime and standard-library work
needs only a Rust toolchain, not the compiler's LLVM 22 + clang setup.

## What's here

- Memory allocation and reference counting
- String operations (UTF-8)
- Collections (list, map, set)
- Type conversions and standard-library runtime support
- Optional features: `json`, `csv`, `net`, `sql`, `sync` (see `[features]` in
  `Cargo.toml`; `full` enables everything and is the default)

## Development

```bash
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

No LLVM or clang required.

## Relationship to the compiler

The compiler does not import this crate as Rust code - it links the built library
when producing executables and fetches the published crate from crates.io (pinning
a compatible semver range). For coupled local development, check this repo out as a
sibling of `mux-compiler` (the compiler resolves `../mux-runtime` automatically) or
set `MUX_RUNTIME_SRC` to a local checkout.

## Versioning

Versioned independently of the compiler. The compiler pins a compatible semver
range and `mux --version` reports both, e.g. `mux 0.5.1 (runtime 0.5.0)`. A
coupled change ships as two steps: publish the runtime first, then bump the
compiler's `mux-runtime` pin.

## Related repositories

- [mux-compiler](https://github.com/muxlang/mux-compiler) - the compiler/CLI that links this runtime
- [mux-website](https://github.com/muxlang/mux-website) - documentation

## License

[MIT](LICENSE)
