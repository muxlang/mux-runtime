<div align="center">

<img src="https://mux-lang.dev/img/mux-logo.png" alt="Mux Logo" width="120">

# mux-runtime

**The runtime and standard library for [Mux](https://github.com/muxlang)**

[![License](https://img.shields.io/badge/license-MIT-green.svg?style=flat-square)](LICENSE)
[![crates.io](https://img.shields.io/crates/v/mux-runtime.svg?style=flat-square)](https://crates.io/crates/mux-runtime)
[![Documentation](https://img.shields.io/badge/docs-online-blue.svg?style=flat-square)](https://mux-lang.dev)
[![Sonar Quality Gate](https://sonarcloud.io/api/project_badges/measure?project=muxlang_mux-runtime&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=muxlang_mux-runtime)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=muxlang_mux-runtime&metric=coverage)](https://sonarcloud.io/summary/new_code?id=muxlang_mux-runtime)

</div>

Compiled Mux programs link against this library at compile time. It is plain,
stable Rust with **no LLVM dependency** - so runtime and standard-library work
needs only a Rust toolchain, not the compiler's LLVM 22 + clang setup. Published
to crates.io as [`mux-runtime`](https://crates.io/crates/mux-runtime).

---

## What's here

- Memory allocation and reference counting
- String operations (UTF-8)
- Collections (list, map, set)
- Type conversions and standard-library runtime support
- Optional features: `json`, `csv`, `net`, `sql`, `sync` (see `[features]` in
  `Cargo.toml`; `full` enables everything and is the default)

---

## Development

```bash
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

No LLVM or clang required.

Benchmarks for the hot paths (reference counting, list/map/set, string, JSON) live
under `benches/` and use criterion:

```bash
cargo bench                                  # run all hot-path benchmarks
cargo bench -- --save-baseline main          # save a baseline, then compare a change with
cargo bench -- --baseline main               # ... --baseline main
```

Benchmarks are a local/manual tool and a non-blocking CI report; they never gate a
merge (shared CI runners are too noisy for a wall-clock threshold).

---

## Relationship to the compiler

The compiler does not import this crate as Rust code - it links the built library
when producing executables and fetches the published crate from crates.io (pinning
a compatible semver range). For coupled local development, check this repo out as a
sibling of `mux-compiler` (the compiler resolves `../mux-runtime` automatically) or
set `MUX_RUNTIME_SRC` to a local checkout.

---

## Versioning

Versioned independently of the compiler. The compiler pins a compatible semver
range and `mux --version` reports both, e.g. `mux 0.5.1 (runtime 0.5.0)`. A
coupled change ships as two steps: publish the runtime first, then bump the
compiler's `mux-runtime` pin.

Full release steps:
[muxlang/mux-context](https://github.com/muxlang/mux-context/blob/main/docs/release-process.md#mux-runtime).

---

## Related repositories

| Repo | What it is |
|------|------------|
| [mux-compiler](https://github.com/muxlang/mux-compiler) | The language, compiler, and CLI that links this runtime |
| [mux-website](https://github.com/muxlang/mux-website) | Docs site (mux-lang.dev) and the language reference |
| [mux-website-api](https://github.com/muxlang/mux-website-api) | Compile/run API behind the playground |
| [tree-sitter-mux](https://github.com/muxlang/tree-sitter-mux) | Tree-sitter grammar + highlight queries |
| [mux-syntax-highlighting](https://github.com/muxlang/mux-syntax-highlighting) | TextMate grammar, VSCode extension, canonical syntax spec |
| [mux-context](https://github.com/muxlang/mux-context) | Cross-repo architecture, design rationale, glossary, releases |

---

## License

[MIT](LICENSE) - Maintained by [Derek Corniello](https://github.com/DerekCorniello)
