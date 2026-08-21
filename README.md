<div align="center">

<img src="https://mux-lang.dev/img/mux-logo.png" alt="Mux Logo" width="120">

# mux-runtime

**The runtime and standard library for [Mux](https://github.com/muxlang)**

[![License](https://img.shields.io/badge/license-MIT-green.svg?style=flat-square)](LICENSE)
[![Documentation](https://img.shields.io/badge/docs-online-blue.svg?style=flat-square)](https://mux-lang.dev)
[![Sonar Quality Gate](https://sonarcloud.io/api/project_badges/measure?project=muxlang_mux-runtime&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=muxlang_mux-runtime)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=muxlang_mux-runtime&metric=coverage)](https://sonarcloud.io/summary/new_code?id=muxlang_mux-runtime)

</div>

Compiled Mux programs link against this library at compile time. It is plain,
stable Rust with **no LLVM dependency** - so runtime and standard-library work
needs only a Rust toolchain, not the compiler's LLVM 22 + clang setup.

> **crates.io is frozen.** Versions through 0.5.0 remain published and are not
> yanked, but no new ones will be. `mux-compiler` consumes this repo as a git
> dependency on `main`, pinned to an exact commit by its `Cargo.lock`, so
> merging here is what makes a change available. See
> [ADR 0004](https://github.com/muxlang/mux-context/blob/main/docs/decisions/0004-runtime-resolved-from-source.md).

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
when producing executables. It resolves this repo as a git dependency on `main`,
pinned to an exact commit by its `Cargo.lock`, and links the archive cargo builds
into its `target/` directory. Producing that archive takes `cargo build -p
mux-runtime`: cargo emits a dependency's rlib and never its staticlib.

For coupled local development, build this repo and point `MUX_RUNTIME_LIB` at the
resulting `target/debug/libmux_runtime.a`. That is the first thing the compiler
consults, so it overrides the library cargo built from the locked commit -
which also means a leftover `MUX_RUNTIME_LIB` will keep overriding it until you
unset it.

The compiler never builds this crate while compiling a Mux program, and always
links the `full` feature set. Static linking discards archive members nothing
references, so a program that does not touch `sql` carries no SQLite.

---

## Versioning

The compiler identifies this repo by commit, not by version: `mux version`
reports the locked commit as semver build metadata, e.g.
`runtime v0.5.0+g4e2dc14`. A coupled change is one PR here and one in
`mux-compiler` - no publish step between them.

Record changes under a numbered, dated changelog heading as part of the change
itself. A rolling `Unreleased` heading gives no way to say which changes a given
compiler pin contains, which is why it is no longer used. The `version` field is
not published while crates.io is frozen, but it moves with the heading so the
two do not drift.

Full release steps:
[muxlang/mux-context release process](https://github.com/muxlang/mux-context/blob/main/docs/release-process.md#mux-runtime).

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
