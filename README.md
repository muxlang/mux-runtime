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
stable Rust with **no LLVM dependency**, so runtime and standard-library work
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

## Runtime failure codes

Terminating runtime failures use stable `E06xx` codes. The registry is exposed
as `mux_runtime::panic::RuntimeErrorCode` and is part of the C-compatible panic
ABI:

| Code | Failure |
| --- | --- |
| E0600 | List index out of bounds |
| E0601 | Missing map key |
| E0602 | Division or modulo by zero |
| E0603 | Assertion failure |
| E0604 | Where-constraint violation |
| E0605 | Integer overflow |
| E0699 | Internal runtime failure |

The compiler and runtime registries are intentionally separate because they
are independently built. Unknown codes received over the FFI are rendered as
`E0699` for compatibility with older runtimes.
- Optional features mirror the runtime-backed stdlib packages:
  `json`, `csv`, `net`, `tls`, `http2`, `http3`, `url`, `uuid`, `crypto`, `sql`, `sync`, and
  `regex` (see `[features]` in `Cargo.toml`; `full` enables every package and
  is the default). `core` contains the feature-independent runtime, while
  `chrono-tz` adds timezone database support to `datetime`.

The opt-in `http3` feature provides synchronous typed client and server
transports built on Quinn and h3. HTTPS requests try HTTP/3 first, then use the
existing HTTP/2 and HTTP/1.1 paths when QUIC is unavailable. The request queue
and in-flight h3 stream are cancelled when the caller's deadline expires. The
server listener accepts DER certificates and serves one bounded request at a
time through a blocking handler callback. Request and response bodies are
bounded, and connection, protocol, timeout, and size failures retain typed
error categories. Long-lived SSE and WebSocket server actors emit bounded
heartbeats at the configured interval; setting that interval to zero disables
the server heartbeat. `HttpServer.serve_until_cancelled` accepts a
`CancellationToken`, stops admitting connections after cancellation, and
drains queued worker jobs before returning. With multiple workers, connection
actors retain sockets and exchange bounded owned request and response snapshots;
each worker receives its own snapshot of sendable captures, while resource
handles in captures are rejected.

The HTTP package also exposes a synchronous `OAuthClient` for public OAuth 2.0
and OIDC clients. It validates HTTPS discovery metadata, builds S256 PKCE and
nonce authorization URLs, and performs bounded token exchange, refresh,
introspection, and revocation requests. `OAuthSession` owns token-response
values and exposes refresh, introspection, revocation, expiry, and explicit
close. These APIs do not store browser sessions or client secrets.

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

The compiler identifies this repo by commit, not by a runtime release number.
`mux version` reports the locked commit as build metadata, for example
`runtime v0.6.1+g8806dc7`. A coupled change is one PR here and one in
`mux-compiler`; there is no publish step between them.

Record user-visible changes under a dated changelog heading as part of the
change itself. Do not create an empty runtime release. Cargo still requires the
`version` field in this manifest, but it is technical metadata for the private
git dependency, not a public compatibility promise.

Full release steps:
[muxlang/mux-context release process](https://github.com/muxlang/mux-context/blob/main/docs/release-process.md#mux-runtime).

### Live SQL checks

The integration workflow starts PostgreSQL and MySQL service containers and
runs `scripts/ci/run-live-sql.sh`. That gate requires both
`MUX_TEST_POSTGRES_URL` and `MUX_TEST_MYSQL_URL`; missing variables fail before
Cargo starts instead of producing skipped driver tests. SQL Server uses the
native TDS provider with verified TLS by default. Set the optional
`MUX_TEST_SQLSERVER_URL`, for example
`sqlserver://user:pass@host:1433/db?trustServerCertificate=true`, to run the SQL
Server acceptance test. It checks TDS connect, `SELECT`, leased result-set EOF
and close, transaction commit/rollback, prepared-statement reuse, pool lease
return, and connection reuse. The variable is unset in ordinary local runs, so
they do not need SQL Server.

The runtime exposes `mux_sql_connection_capabilities` and
`mux_sql_pool_capabilities` as typed result values. Their maps contain
`provider`, `query_timeout`, and `query_cancellation`. MySQL supports both
query-interruption flags by using a second authenticated session to issue
`KILL QUERY`; the primary connection remains reusable after an interruption.

To run only that fixture in a hosted environment:

```bash
MUX_TEST_SQLSERVER_URL='sqlserver://user:pass@host:1433/db?trustServerCertificate=true' \
  ./scripts/ci/run-live-sqlserver.sh
```

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
