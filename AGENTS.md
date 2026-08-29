# mux-runtime

`mux-runtime` is the Rust library linked into compiled Mux programs. It owns
the C ABI, reference counting, collections, strings, and standard-library
support used by generated code.

Cross-repository architecture and release facts live in
[`mux-context`](https://github.com/muxlang/mux-context). Read its canonical
[`SKILL.md`](https://github.com/muxlang/mux-context/blob/main/SKILL.md) before
changing an interface shared with the compiler.

## Invariants

- Keep the exported ABI and ownership rules aligned with
  `mux-context/docs/design/memory.md`.
- Do not add an LLVM dependency; this library must build with stable Rust alone.
- Runtime internals must return a defined result instead of panicking. `unwrap`,
  `expect`, indexing, and `unreachable!` are not valid error handling here.
- Preserve null-safety and thread-safety of every exported FFI function.

## Quality gate

Run `cargo fmt --all -- --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, and
`cargo test --all-features` before committing. Run strict rustdoc and the
security/coverage jobs in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
for cross-cutting work.

## Documentation

See [`README.md`](README.md), [the CI workflow](.github/workflows/ci.yml), and
the linked design documents in `mux-context` for public API and ownership
details.
