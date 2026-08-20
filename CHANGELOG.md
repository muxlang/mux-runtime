# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions through 0.5.0 were published to crates.io. That channel is now frozen:
`mux-compiler` consumes this repo as a git dependency pinned to a commit by its
`Cargo.lock`, so merging to `main` is what makes a change available and there is
no version to bump. Record changes under `Unreleased` as part of the change
itself - see
[ADR 0004](https://github.com/muxlang/mux-context/blob/main/docs/decisions/0004-runtime-resolved-from-source.md).

## [Unreleased]

### Added
- **`mux_csv_rows_as_maps(csv)`** - a parsed CSV as one map per row, keyed by
  header name, returning `optional<list<map<string, string>>>`. The parsed form
  keeps headers and rows apart, so reading a named column means finding its
  index first; doing that per field per row in generated code would be a nested
  loop over data the runtime already holds. Every cell stays a string, because
  CSV has no types - deciding a column is a number is the reader's job. Needed
  by typed deserialization (muxlang/mux-compiler#404).
- **`mux_string_to_bool(text)`** - `result<bool, string>`, accepting `true` and
  `false` case-insensitively and nothing else. Deliberately narrow: a CSV bool
  column is whatever the writer spelled, and accepting `1` or `yes` means
  guessing which convention a file follows, then being wrong for the file where
  `1` is the number one.

### Added
- **`mux_json_field(value, key)`** - one field of a JSON object by name,
  returning `optional<Json>`. `none` covers both "not an object" and "no such
  key"; a field explicitly set to `null` comes back as `some(null)`, so an
  ABSENT field stays distinguishable from a present null. Typed deserialization
  (muxlang/mux-compiler#404) depends on that difference: a missing required
  field is an error, while `optional<T>` accepts either spelling.

  The compiler emits one call per declared field rather than converting the
  whole object to a Mux map first, which would clone every value including the
  ones the class never declares.

### Fixed
- **Copying a socket produced an unusable one.** `TcpStream`, `TcpListener` and
  `UdpSocket` registered a destructor but no copy callback, so `copy_object`
  returned null and any value-semantics copy - `auto keep = listener`, passing a
  listener to a function, assigning one out of a `match` arm - yielded a value
  whose handle was zero. Every later call on it answered "invalid tcp listener",
  including on the original spelling of the flat `result` style
  (muxlang/mux-compiler#393).

  A socket is a resource, not a value, so a copy cannot mean a second socket:
  both names now mean the same one, and it closes when the last name goes away,
  which is the rule every other heap value in the language already follows.
  `close()` stays a hard close for every name - it is an explicit act by the
  program, and the remaining names get "invalid handle" rather than silently
  keeping a socket alive.
- **`random.next_range` returned only the lower half of its range.** The
  fixed-point scale shifted by 32 while `mux_rand_int` yields 31 bits, so
  `next_range(1, 7)` - the dice roll in the stdlib docs - could never return 4,
  5 or 6. The shift is now derived from `RAND_MAX` so the two cannot drift
  apart. The previous test asserted only that results were *within* the range,
  which a too-narrow range satisfies; the new one asserts the range is fully
  covered (mux-runtime#50).
- **JSON integers became floats, and large ones changed value.** `Json` had a
  single `Number(f64)` case, so `{"n":42}` re-serialized as `{"n":42.0}` and
  `9007199254740993` came back as `9007199254740992`. `Json` now has separate
  `Int(i64)` and `Float(f64)` cases and asks serde_json which one the literal
  was. An HTTP response `status` is consequently an integer, so a caller reads
  `201` rather than `201.0` (mux-runtime#52).
- **JSON object keys were re-ordered alphabetically.** `Json::Object` was a
  `BTreeMap`, so `{"zebra":1,"apple":2}` round-tripped sorted and a program
  could not read a document and write it back unchanged. It is now an
  insertion-ordered `JsonMap`, with serde_json's `preserve_order` feature so the
  parse order survives to reach it - matching the reasoning `ordered.rs` already
  gives for Mux's own `map` (mux-runtime#53).
- **`string.length()` counted bytes rather than characters.** Any non-ASCII
  character made it wrong - an accented letter counted 2, an emoji 4. It now
  counts characters, which is what every position-based string operation has to
  agree on. This makes `length` O(n) rather than O(1); if that matters later,
  cache a count rather than returning to byte semantics (mux-runtime#51).

### Added
- **Typed JSON accessors**: `mux_json_as_string`, `mux_json_as_int`,
  `mux_json_as_float`, `mux_json_as_bool`, `mux_json_as_list`,
  `mux_json_as_map` and `mux_json_is_null`. Each returns an `optional`, `none`
  when the value is a different kind - ordinary control flow when reading a
  document, not an error worth reporting.

  `stringify` was previously the only way to inspect a value, so a string field
  came back JSON-encoded with its quotes and there was no way to strip them; a
  string could not be read out of a document at all. An integral float reads as
  an int so `{"n": 42.0}` still works, while a fractional one is `none` rather
  than silently truncated. The compiler side lands separately
  (mux-compiler#392).
- **`mux_string_compare`**, lexicographic ordering returning negative / zero /
  positive. The compiler's relational operators on `string` had no runtime
  function to call and fell through to the numeric path, which unboxed the
  string pointer as an integer - so `<` and `>` compared addresses and answered
  `false` in both directions (mux-compiler#390). The compiler side of that fix
  lands separately.

### Changed
- **A closure capture cell is reference counted and shared.** Each capture slot
  used to point at a plain `malloc`'d cell that the closure freed outright, so a
  cell could belong to exactly one closure. That forced the compiler to copy a
  captured variable into a fresh cell and rebind the variable to it, which is
  why a capture made inside a block stopped being shared once the block ended
  (mux-compiler#384). A cell is now `[refcount | *mut Value]` from
  `mux_cell_alloc`, retained and released via `mux_cell_retain` /
  `mux_cell_release`, so the captured variable and every closure capturing it
  can name the same cell. `mux_closure_release` drops a reference instead of
  freeing.

  Coupled change: the compiler must allocate capture cells with `mux_cell_alloc`
  rather than `malloc`.

### Added
- **A class can key a map or join a set.** An object type may now register the
  class's own equality, ordering and hash
  (`mux_register_object_equals` / `_compare` / `_hash`) alongside its copy and
  destructor, so a map, a set or `contains` matches instances the way the
  operators do. Unlike the copy and destructor callbacks, these take the boxed
  object - the same `*mut Value` a class method receives as `self`. A class that
  registers none of them keeps identity semantics.

### Changed
- **`map` and `set` are hash tables that preserve insertion order**, replacing
  the `BTreeMap`/`BTreeSet` behind them. Lookup, insert and remove are now O(1)
  rather than O(log n), which is what a user of a hash-based collection expects
  in any other language, and iteration yields insertion order rather than sorted
  order. Re-assigning an existing key keeps its original position, matching
  Python and JavaScript. Equality and hashing stay order-insensitive: two maps
  with the same pairs are equal however they were built. This is also what makes
  `Hashable` implementable at all, since the runtime now hashes.
- **A whole-number float keeps its `.0` wherever it is printed.** One inside a
  list, map, set, tuple or optional printed through a plain format and rendered
  `6.0` as `6`, so a list of floats was indistinguishable from a list of ints
  and the same value disagreed with itself depending on where it appeared.
- **`mux_box_enum_managed` takes a hash callback.** An exported signature
  change, so it lands with the matching `mux-compiler` update per ADR 0004.

### Fixed
- **A map or set used as a key hashed by insertion order** while its equality
  ignored order, so two equal maps had different hashes and one used as a key
  could not be found.
- **A boxed enum hashed only its discriminant**, putting every value of one
  variant in a single bucket - correct, but no longer acceptable once map and
  set became hash tables.
- **A map key or set member is now an independent snapshot.** Objects share
  their data through a handle, so mutating one after it became a key moved
  where the key belonged without moving the entry, stranding it where no lookup
  would go.
- **Content-based keying now requires an equality as well as a hash.** A hash
  alone cannot key anything: two entries landing in one bucket need something
  to tell them apart, and without it equality stayed pointer identity while the
  key came from the contents - so two distinct objects whose hashes collided
  ordered equal while comparing unequal. Such a type is keyed by identity,
  which is consistent for both. The compiler requires `eq` of every `Hashable`
  class, so this is only reachable through the FFI directly.
- **Content-based keying now requires the type to be copyable.** A key is
  stored at a position derived from its contents, and the only way to snapshot
  one away from the caller's handle is the registered copy callback. A type
  that registered equality or a hash without one is keyed by identity instead,
  which is stable under mutation. Every class the compiler emits registers copy,
  so this only reaches an object type registered through the FFI directly.
- **An object with registered equality but no registered hash** compared by
  contents and hashed by address, so equal instances hashed differently.
  Hashing and ordering now share one key, which also keeps the ordering a total
  order - mixing content equality with address ordering was not transitive, and
  `sort_by` may panic on that.

## [0.5.0] - 2026-07-13

First release of `mux-runtime` as a standalone, independently versioned repo,
extracted from the former monorepo. From here the runtime is published to
crates.io on its own cadence; the compiler pins a compatible semver range and
`mux --version` reports both.

### Added
- **`mux_value_unbox_enum`**: New FFI entry point to unbox enum payloads, consumed
  by compiler codegen for enum handling (#6).
- **Criterion hot-path benchmarks**: A `hot_paths` bench target covering the runtime
  functions on the critical path; local/manual, non-gating (#14).
- **Runtime test suite + coverage**: Added the test suite and wired LCOV coverage
  reporting into SonarCloud.

### Changed
- **Copy-on-write collection mutation + O(1) map reads**: Collection mutators now
  copy-on-write and map reads use an O(1) accessor, removing a read-path quadratic
  in loop-heavy programs (#15, #16).
- **Documented the memory & ownership ABI**: `AGENTS.md` and the design notes now
  record the borrowed-vs-owned conventions, C-string ownership rules, and closure
  reference-counting that FFI changes must honor (#11).
- **Standalone repo setup**: Established as an independent crate published manually
  from a local checkout (no registry token in CI).

### Fixed
- **Closure lifetime management + Result/Optional wrappers**: Corrected closure
  capture reference counting and a wrapper bug in `result`/`optional` values (#12).
- **C-string leaks in primitive-to-string conversions**: `*_to_string` conversions
  now free the C strings they allocate, fixing leaks flagged by Valgrind (#10,
  closes #251).
- **Panic-path correctness**: Fixes to runtime panic handling and messaging (#3).

---

> **Independent multi-repo versioning begins at 0.5.0.** Entries below are inherited
> from the pre-split (monorepo-era) compiler changelog and are shared history, not
> specific to `mux-runtime`.

---

## [0.4.1] - 2026-06-27

### Fixed
- **Windows CI linker failure (`xml2.lib`)**: The conda-forge `libxml2` packages do not install any `.lib` import library into `Library/lib/`, causing `LNK1181: cannot open input file 'xml2.lib'` on `windows-latest` runners. Fixed by adding a dedicated step after MSVC toolchain setup that generates `xml2.lib` from the installed `libxml2*.dll` using `dumpbin /exports` and `lib.exe`.

## [0.4.0] - 2026-06-26

### Added
- **Mux AI documentation assistant**: In-docs chat widget powered by a Cloudflare Worker (RAG over `mux-website/docs/` via Vectorize + Llama 3.3 70B). Answers Mux questions with citations, explains compiler errors, and rejects off-topic queries. Includes `tools/docs-indexer/` for re-indexing and `tools/retrieval-test/` eval harness (8/8 retrieval, 19/19 error-explainer). Full runbook in `workers/mux-ai/README.md`.
- **DSA stdlib expanded**: Added `algorithm.mux` (generic graph algorithms: topological sort, cycle detection, DFS, BFS), `graph.mux` (adjacency-list directed graph), `bintree.mux` (binary tree with inorder/preorder/postorder traversals), `heap.mux` (min/max heap), `queue.mux` (FIFO), `stack.mux` (LIFO), and `collection.mux` (base Collection interface). Closes #203.
- **`to_char()` conversion method**: Implemented `string.to_char() -> result<char, string>` and `int.to_char() -> char` (Unicode code-point to char). Closes #207.
- **`to_list()` on set and map**: `set<T>.to_list()` and `map<K,V>.to_list()` now registered and callable. Closes #209.

### Changed
- **LLVM upgraded from 17 to 22**: Migrated inkwell dependency and all CI/build tooling to LLVM 22, which is more broadly available and actively maintained. Closes #215.
- **Dead code elimination**: Unused symbols (variables, classes, enums, functions, generics) are no longer emitted to LLVM IR, reducing binary size and intermediate output. Closes #200.
- **Minimal end-user installation**: End-user installs now ship only the compiler binary and runtime; development tooling (LLVM, clang, analysis tools) is separated into the dev setup path. Closes #193.
- **God Object refactor**: Broke down oversized structs/impls in the compiler (semantic analyzer, codegen context) into smaller focused components. Closes #194.
- **Improved error messages for collection types**: Set and map type errors now display `set<T>` and `map<K,V>` instead of raw brace-syntax (`{char}`, `{string: int}`). Closes #210.
- **Improved error for `.new()` on built-in collections**: Calling `list.new()`, `map.new()`, or `set.new()` now emits a helpful diagnostic suggesting `[]` or `{}` literal syntax instead of a generic undefined-type error. Closes #204.
- **SonarQube and Greptile cleanups**: Addressed code quality findings across multiple passes: god-object decomposition, vulnerability dependency updates, and ESLint/security-hotspot fixes in the website.

### Fixed
- **`void` functions require explicit `return`**: Functions declared `returns void` without a `return` statement now produce a compile-time error instead of silently compiling. Closes #211.
- **Map `{}` literal compiled as Set**: `map<K,V> m = {}` previously produced a `Value::Set` at runtime, causing segfaults on map operations. Fixed by resolving `{}` type contextually during semantic analysis (`SetOrMapLiteral`) so codegen emits `mux_new_map` vs `mux_new_set` correctly.
- **Struct layout corruption in interface-implementing classes**: Inline constructor initialization used positional field indices instead of the interface-aware field map, causing the first real field's data to overwrite the vtable slot. Affected all classes implementing interfaces.
- **Non-primitive field initialization in class constructors**: Non-generic class constructors (e.g., `Graph.new()`) zero-initialized `list`/`map`/`set` fields as null instead of real empty collections.
- **Generic class vtable generation crash**: `generate_class_vtables()` attempted to build vtables using unspecialized method names (e.g., `Graph.len`) which do not exist; generic classes only have monomorphized instances. Vtable generation is now skipped for generic classes (interfaces use static dispatch).
- **Cross-module import ordering**: `collect_hoistable_declarations()` ran before imports were resolved, so classes in a file could not see imported interfaces during the hoisting pass. Imports are now processed during hoisting; `expression_type_overrides` from submodules are also merged so empty `{}` literals are correctly disambiguated. Closes #203.
- **`Type::Module` panic**: `resolve_type_with_seen()` and `llvm_type_from_resolved_type()` panicked on `Type::Module` instead of returning `Err`, breaking `module.CONST.method()` call patterns.
- **Website frontend examples**: Audited and corrected all code examples and interactive demos on the documentation site; removed a stale debug log from the compiler.
- **Dependency vulnerabilities**: Updated website and tooling dependencies to resolve known CVEs.

## [0.3.2] - 2026-06-13

### Changed
- **SonarQube quality issues resolved**: Replaced `unreachable!()` in `deep_clone_value` for `Value::Object` inside containers, fixed UB in sync unlock arm, replaced 7 `.expect()` calls with proper error propagation, and extracted duplicate constructor helpers.
- **Code duplication reduced**: Overall project duplication dropped from 4.5% to 3.9%. Extracted module-level expression helpers in `methods.rs`, merged duplicate equality and return value arms in `statements.rs`, and added signature macros to compact ~40 runtime function declarations.
- **Version metadata updated**: All configuration files bumped from 0.3.1 to 0.3.2.

### Fixed
- **Segfault when running `cargo test`**: `LD_LIBRARY_PATH` was checked before `DT_RUNPATH`, so the workspace `.so` was loaded instead of the cached release `.so`. Added `-Wl,--disable-new-dtags` to force `DT_RPATH`, which is checked before `LD_LIBRARY_PATH`.
- **LLD linker flags**: Removed `-no-pie` flag to fix LLD compatibility on modern Linux distributions.

## [0.3.0] - 2026-05-07

### Added
- **Syntax highlighting support**: Added TextMate and Tree-sitter grammar support with setup guidance for VSCode, Sublime Text, JetBrains, Neovim, and Helix.
- **Setup documentation**: New `mux-website/docs/setup.md` with language installation and editor configuration guides.

### Changed
- **Profiling decoupled**: Removed built-in profiling infrastructure (`mux-profiling` crate) from compiler and runtime. Profiling now uses external tools (perf, Instruments, WPA) only.
- **Code quality improvements**: Pinned GitHub Actions versions, added `--locked` to cargo commands, added Cargo.lock files, refactored Python and JavaScript generators to fix SonarQube findings.

### Fixed
- **Code review cleanup**: Removed orphaned profiling scripts, cleaned up empty scope blocks in compiler, and fixed numbered list in CONTRIBUTING.md.

## [0.2.1] - 2026-04-22

### Changed
- **Compiler maintainability work**: Reduced complexity across compiler modules with a broad cleanup and refactor pass.
- **Standard library internals**: Refactored and optimized stdlib implementations for better consistency and maintainability.
- **Developer workflow and project metadata**: Updated AI agent guidance, OpenCode configuration, and supporting repository automation files.
- **Documentation and website updates**: Improved README content and landing page structure, examples, and installation guidance.

### Fixed
- **Codegen regressions**: Fixed recent LLVM IR generation regressions and related import handling issues.
- **Website behavior**: Corrected landing page rendering details, including list key usage and stack example behavior.
- **Build and CI support scripts**: Fixed tooling and script issues affecting local and CI workflows.
- **Versioning release prep**: Synced release metadata and version-related files for `0.2.1`.

### Security
- **Dependency and vulnerability updates**: Applied dependency maintenance and vulnerability fixes, including Dependabot-driven updates.
- **Static analysis cleanup**: Addressed SonarCloud findings and code quality issues across the codebase.

## [0.2.0] - 2026-03-24

### Added
- **Standard library**: Full implementation of standard library modules (`math`, `io`, `net`, `sql`, `random`, `datetime`, `dsa`).
- **Data structures library**: New `dsa` module with binary tree, graph, and other data structures.
- **SQL support**: SQL client functionality for database interactions.
- **HTTP client**: Built-in HTTP client for making web requests.
- **Network server architecture**: Foundation for building network servers.
- **JSON, CSV, and environment utilities**: Tools for handling JSON, CSV, and environment variables.
- **Networking primitives**: Low-level networking building blocks.
- **IO stdlib library**: Standard I/O operations.
- **Error message improvements**: More helpful and context-aware error messages.
- **Refactored codebase to Rust idioms**: Improved readability and maintainability.
- **CI improvements**: Fixed continuous integration pipelines.
- **Project tooling & hooks**: Updated pre-commit hooks and development tooling.

### Changed
- **Upgraded to LLVM 17** (already present, but now formally documented).
- **Improved installation process**: Better installer scripts and platform detection.
- **Simplified project structure**: Cleanup of repository layout.

### Fixed
- **Numerous bug fixes** across the compiler and runtime.
- **Reference counting issues**: Fixed memory management bugs.
- **Type checking edge cases**: Corrected handling of complex type scenarios.
- **Code generation correctness**: Fixed issues with LLVM IR generation.
- **Exhaustiveness checking in match statements**: Guards and wildcards now work correctly.
- **Class and interface resolution**: Fixed bugs in type hierarchy.

### Security
- **Resolved dependabot alerts** (see PR #140).

## [0.1.2] - 2026-02-08

### Added
- **Match as switch statement**: Extended `match` to work as a switch statement for any type (not just enums).
- **Improved pattern matching**: Enhanced exhaustiveness checking and guard support.

### Fixed
- **Reference and chaining fixes**: Resolved issues with reference handling and method chaining.
- **Function return handling**: Corrected return value processing.
- **Class-related bugs**: Fixed errors in class instantiation and inheritance.
- **Frontend cleanup**: Removed erroneous information from error messages.

## [0.1.1] - 2026-02-07

### Fixed
- **Crates.io publishing**: Fixed configuration and metadata for publishing to crates.io.
- **Build updates**: Adjusted build scripts for proper release artifacts.

## [0.1.0] - 2026-02-07

### Added
- **Initial public release** of the Mux compiler and runtime.
- **Core language features**: Static typing, generics, pattern matching, error handling (`result<T,E>`, `optional<T>`).
- **LLVM-based code generation**: Produces native executables.
- **Reference-counted memory management**: Automatic memory safety.
- **Basic standard library**: Collections, string operations, I/O.
- **Installer scripts** for Linux, macOS, and Windows.
- **Documentation website** (mux-lang.dev) with language specification.

### Known Issues
- No LSP or code formatter yet.
- Standard library is minimal.
- Breaking changes expected.
