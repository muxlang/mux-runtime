# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions through 0.5.0 were published to crates.io. That channel is now frozen:
`mux-compiler` consumes this repo as a git dependency pinned to a commit by its
`Cargo.lock`, so merging to `main` is what makes a change available - see
[ADR 0004](https://github.com/muxlang/mux-context/blob/main/docs/decisions/0004-runtime-resolved-from-source.md).

Changes are still recorded under a numbered heading rather than a rolling
`Unreleased` one. A heading that never closes gives no way to say which set of
changes a given compiler pin actually contains, and it is what let three
compiler PRs land with no release notes at all.

## 2026-09-12

### Changed

- Coverage counters are aggregated per source site and written at process
  exit. Reports include a completion marker so partial writes are rejected;
  loop iteration counts no longer determine artifact size.
- SQL result-set reads now return typed results instead of hiding streaming
  failures as EOF or empty lists. Closing a result set is explicitly fallible.
- **MySQL query interruption is supported.** Timeout and cancellation queries
  use a second authenticated session to issue `KILL QUERY`, preserve typed
  timeout/cancellation errors, and keep the primary connection reusable after
  an interrupted statement. Server-side interrupt failures remain typed
  database errors.
- **HTTP/3 has bounded synchronous client and server paths.** The opt-in
  `http3` feature uses h3 framing and QPACK over Quinn, maps transport,
  protocol, timeout, and body-limit failures to typed errors, falls back to
  HTTP/2 or HTTP/1.1 when QUIC is unavailable, and exposes a blocking server
  listener with DER certificate configuration. Caller deadlines now cancel
  queued requests and drop in-flight h3 streams.
- **OAuth/OIDC middleware verifies RS256 bearer tokens.** It validates issuer,
  audience, expiry, optional not-before time, key id, and signatures using a
  bounded five-minute JWKS cache. Invalid credentials return 401 and JWKS
  failures return typed 503 errors.
- **OAuth/OIDC public clients are typed and bounded.** `OAuthClient` validates
  secure configuration, performs HTTPS discovery, builds S256 PKCE authorization
  URLs with a nonce, and supports token exchange, refresh, introspection, and
  revocation without storing client secrets or browser sessions.
- **Long-lived HTTP streams have server-owned heartbeats.** SSE connections
  receive bounded comment heartbeats and WebSocket connections receive bounded
  ping frames at the configured `heartbeat_interval_ms`; zero disables them.
- **HTTP servers accept cooperative shutdown tokens.**
  `HttpServer.serve_until_cancelled` stops accepting new connections when its
  `CancellationToken` is cancelled, then drains work already queued for
  workers before returning.
- **HTTP worker jobs use owned snapshots.** Connection actors retain sockets,
  workers receive bounded request data, and response data returns through a
  bounded channel for actor-owned serialization. Multi-worker mode snapshots
  sendable captures per worker and rejects resource handles.
- **Native-host smoke checks use one portable runner.** The platform matrix
  now builds subprocess argument lists in Python, enforces a per-command
  timeout, and terminates descendants on timeout on Unix and Windows.
- **Live SQL wiring cannot silently skip.** The integration job runs the
  PostgreSQL and MySQL driver fixture through a gate that requires both service
  URLs before Cargo starts.
- **Coverage checks enforce both metrics.** The runtime gate now applies line
  and branch floors to every validated LCOV record.
- **Eligible HTTPS requests share a bounded HTTP/2 actor.** TLS ALPN still
  falls back to HTTP/1.1, while negotiated HTTP/2 sockets are owned by a
  runtime actor with bounded queueing, independent stream tasks, per-request
  deadlines, flow-control release, and cancellation on timeout. The public
  request API remains synchronous and buffered.
- **SQLite memory construction is explicit.** `sql.connect` and
  `Pool.from_config` now reject the legacy `sqlite::memory:` and
  `sqlite://:memory:` spellings. Use `sql.sqlite_memory()` for an isolated
  in-memory connection, or a `sqlite:///path/to/file.db` URI for a pool.

## 2026-09-11

### Added

- **WebSocket fragmented-message reassembly.** The bounded
  `mux_net_websocket_frame_reassemble` runtime operation combines decoded
  continuation frames, permits validated interleaved control frames, checks
  message-level UTF-8, and preserves the existing explicit frame surface.
- Live PostgreSQL and MySQL SQL driver coverage now exercises repeated named
  parameters, batch and bulk execution, savepoints, prepared statement reuse,
  pools, and typed constraint diagnostics.
- **SQL interruption capabilities are explicit.** Connections and pools now
  report provider-specific timeout and cancellation support. The synchronous
  MySQL driver interrupts queries through a second authenticated control
  session and returns typed timeout or cancellation errors; the SQL Server live
  fixture now covers transaction, prepared-statement, and pool lease paths.

## 2026-09-09

### Changed
- **Legacy file writes reject null handles safely.** The low-level
  `mux_write_file` bridge now returns `false` when either the file handle or
  content pointer is null instead of dereferencing a null file pointer.
- **JSON Pointer paths are bounded.** RFC 6901 paths, including JSON Patch
  `path` and `from` values, now share the JSON 16 MiB input and one-million
  token limits before allocating the decoded token vector.
- **Global stdin line reads are bounded.** The built-in `read_line()` now
  limits each input line to 16 MiB before allocating beyond the standard
  library's buffered-read bound, matching `io.Reader.read_line()`.
- **Stream line limits accept the exact boundary.** `io.Reader.read_line()`
  now accepts a final unterminated line of exactly 16 MiB and rejects only
  lines that exceed the documented limit, matching the built-in `read_line()`.
- **Mutex acquisition is consistently non-reentrant.** A same-thread recursive
  acquisition now returns a synchronization error before reaching the native
  backend, matching the Unix and Windows behavior required by `std.sync`.
- **Logger handles validate their runtime type.** Logger operations now reject
  values from another opaque-handle registry before reading the embedded handle
  id, preventing a `Writer` or other resource from being mistaken for a
  `Logger` at the runtime boundary.
- **Filesystem and URL fixtures are host-portable.** Cross-platform tests now
  build temporary and missing paths with the native path implementation rather
  than assuming POSIX separators or roots.
- **Filesystem directory listings are bounded.** `fs.listdir` now rejects
  listings over 65,536 entries or 16 MiB of aggregate entry-name bytes before
  materializing an unbounded list; use `fs.Directory` for incremental traversal.
- **URL origins serialize IPv6 hosts correctly.** Origin values now retain
  required brackets around IPv6 literals and use the URL library's standard
  default-port normalization.
- **Varint decoding rejects overflowing tenth bytes.** The bytes cursor no
  longer accepts malformed 64-bit varints that could wrap in release builds;
  varint writer offset arithmetic is checked before indexing the destination.
- **HTTP `HEAD` responses omit the wire body.** The synchronous server now
  preserves the representation length in `Content-Length` while suppressing
  response bytes for `HEAD` requests.
- **HTTP response construction enforces the body bound.** `HttpResponse`
  constructors and direct body assignment now reject buffered bodies over the
  documented 16 MiB limit instead of deferring failure until a later write.
- **HTTP request body field assignment enforces the body bound.** Assigning an
  oversized buffered value through the mutable `request.body` field now fails
  before changing the request, matching the constructor and explicit setter.
- **HTTP header field assignment enforces the header budget.** Replacing a
  request or response's mutable `headers` field now rejects oversized
  collections before changing the object, matching transport-boundary checks.
- **HTTP header collections enforce the header budget during mutation.**
  `Headers.set` and `Headers.append` now reject an oversized resulting
  collection before changing it, so a shared header handle cannot accumulate
  an invalid block before a later request or response boundary.
- **HTTP header budgets are enforced at every transport boundary.** Outgoing
  request headers, client response headers, and constructed response headers
  now share the 64 KiB/128-field limits; oversized collections fail before
  request setup, response storage, or response output.
- **Internal reader adapters enforce the stream read bound.** Protocol bridges
  that request bytes directly from a `Reader` now clamp untrusted buffer sizes
  to the same 16 MiB per-operation limit before allocating.
- **HTTP response writes enforce the body bound.** The common close-after-write
  writer now applies the same 16 MiB limit to every response, including the
  plain-text response synthesized when a handler returns an error.
- **HTTP servers reject invalid handlers before accepting connections.** A null
  or malformed callback now fails immediately instead of waiting for an
  incoming peer or a read timeout before reporting the programming error.
- **Process output overflow aborts promptly.** A captured stdout or stderr
  reader now signals the parent to terminate the producer as soon as the
  16 MiB bound is crossed, preventing a child blocked on a full pipe from
  making `Command.output()` wait indefinitely.
- **Default process pools return typed construction errors.** `ProcessPool.new()`
  now returns `result<ProcessPool, ProcessError>` like the configurable
  constructor, so worker-start and handle-allocation failures cannot become a
  null opaque value.
- **Child waits do not block pipe aliases.** `Child.wait()` and
  `wait_timeout()` now poll without holding the global child registry lock, so
  another alias can drain a piped stdout or stderr stream while the wait is in
  progress; `wait()` still closes an open stdin pipe before waiting.
- **SQL placeholder rewriting rejects unterminated block comments.** Positional
  and named parameter paths now reject malformed comments before provider
  execution, keeping behavior consistent across drivers.
- **SQL statement text is bounded consistently.** Execute, query, named
  parameter, timeout, cancellation, and prepared-statement paths now reject
  SQL text over 16 MiB before scanning or sending it to a provider; batch and
  migration limits use the same bound.
- **In-memory migration sets enforce the aggregate SQL bound.** Explicit
  migration lists now share the directory loader's 64 MiB aggregate `up`/`down`
  SQL limit, preventing a large list of individually valid migrations from
  bypassing the total-size budget.
- **SQL query deadlines preserve large valid durations.** SQLite and
  PostgreSQL timeout paths now compare elapsed monotonic time instead of
  overflowing an absolute `Instant` deadline and incorrectly timing out
  immediately for very large nonnegative values.
- **SQL batch parsing preserves PostgreSQL dollar-quoted bodies.** Semicolons
  inside `$$...$$` and tagged `$name$...$name$` bodies no longer make a valid
  one-statement function definition look like multiple statements; unterminated
  dollar-quoted regions are rejected before execution.
- **MySQL binary columns preserve byte values.** Result decoding now uses the
  provider's binary-column flag, so a BLOB containing valid UTF-8 remains a
  `bytes` value instead of being silently converted to a `string`.
- **Closing a connection invalidates its prepared statements.** Prepared
  handles bound to a connection are removed from the runtime registry when
  that connection closes, so retained aliases fail cleanly instead of keeping
  stale statement entries alive.
- **Chunked request trailers are validated and bounded.** Trailer fields now
  count against the configured header budget, reject malformed names/values,
  and cannot override `Content-Length` or `Transfer-Encoding` framing.
- **JSON class deserialization preserves optional list entries.** A declared
  `list<optional<T>>` now maps JSON `null` entries to `none` and wraps other
  entries in `some`.
- **Coverage gates reject incomplete LCOV records.** The shared validator now
  checks per-file line and branch counters, so a malformed later record cannot
  be hidden by aggregate totals; the runtime path filter and CI script test
  exercise this contract.
- **CSV writer dialect validation.** In-memory `CsvWriter.from_config` now
  rejects the same invalid NUL, CR, LF, and non-ASCII delimiter/quote bytes as
  the parser and writer-backed APIs.
- **Coverage reports now include a checked summary.** The coverage job emits
  aggregate line and branch percentages in the GitHub job summary and uploads
  the Markdown alongside LCOV. Missing or inconsistent records fail closed.
- **Platform smoke checks compile every target.** The Linux/macOS/Windows
  matrix now runs an all-targets check before tests, covering examples and
  benchmarks without executing benchmark workloads in the acceptance job.
- **Examples leave no Windows build debris.** The shared examples runner and
  its post-run check now clean and reject the compiler's native `main.exe`
  output as well as the Unix `main` spelling.
- **Assertions are a single built-in ABI entry point.** The runtime now exports
  only `mux_assert(bool, message)` for Mux's global `assert(condition, message)`;
  the specialized assertion symbols are removed.
- **Removed the unused untyped panic ABI.** Generated code uses the typed
  `mux_panic_cstr_code` entry point exclusively; the legacy untyped
  `mux_panic_cstr` symbol and its compatibility tests are gone.
- **Condition variables expose bounded waits.** The existing checked runtime
  wait path is now available to Mux as `CondVar.wait_timeout`, returning whether
  notification arrived before the millisecond deadline.
- **Synchronization timeouts are cross-platform bounded.** Channel, semaphore,
  condition-variable, selection, and worker-pool waits reject millisecond values
  above the platform-neutral `u32` limit instead of creating unreachable Unix
  deadlines.
- **Crypto file sealing rejects in-place paths safely.** `seal_file` and
  `open_file` resolve existing input/output paths (including symlink aliases)
  and compare native file identities (including hard-link aliases) before
  opening the destination, so an in-place request fails without truncating the
  source. They now also write to a private destination-directory temporary
  file and atomically publish only after all records authenticate and flush, so
  late failures leave an existing destination unchanged.
- Atomic encrypted-file lifecycle invariants now fail as ordinary operation
  errors instead of aborting through an internal `expect`, preserving the
  caller's typed error path if a temporary output is unexpectedly unavailable.
- Encrypted-file format version 2 authenticates the header, a random file
  identity, record positions, and a required final record. Truncation and
  cross-file record splicing fail without replacing the destination. Version 1
  files are rejected. Unix temporary outputs are created with mode 0600.
- **CSV stream adapters accept shared I/O handles.** `CsvReader` can now pull
  records incrementally from an `io.Reader`, and `CsvWriter` can forward
  encoded records to an `io.Writer`, retaining neither complete document in
  memory.
- **File-backed I/O constructors preserve native open failures.** `Reader.from_file`,
  `Writer.to_file`, and `Writer.append_file` now classify missing, permission,
  invalid, and other operating-system failures in `IoError.kind` and identify
  the failed operation through `IoError.operation`.
- **HTTP server limits are configurable.** `HttpServerConfig` now carries
  bounded header/body/count limits and a read timeout that the typed
  `serve_once` runtime path validates and applies.
- **HTTP request correlation is built in.** Server requests preserve a safe
  incoming `X-Request-ID` or receive a bounded generated ID; responses echo it,
  client requests can set it, and `HttpServerConfig.access_log` enables a
  compact method/path/status line on stderr.
- **HTTP header limits count headers, not coalesced body bytes.** The server
  now accepts a bounded header block when the same socket read also contains
  body bytes, while still rejecting an actually oversized or incomplete
  header block.
- **Removed the obsolete JSON-shaped HTTP bridge.** HTTP requests and
  responses now cross the runtime boundary only through typed handles; the
  former map-field helpers and verb-specific entry points are gone.
- **SQL failures preserve provider and operation context.** Connection,
  prepared-statement, transaction, and pool boundaries now record those fields
  from the typed handle and selected operation rather than leaving callers to
  infer them from diagnostic text.
- SQL connection and pool setup failures now carry the selected provider and
  setup operation as structured context, including unsupported SQL Server
  URIs and invalid pool configuration.
- Malformed MySQL/MariaDB connection URLs are now classified as `Invalid`
  configuration errors before the driver attempts a network connection.
- Nested transaction handles retain their provider context while a child
  savepoint is active; begin, savepoint, commit, and rollback failures now
  identify their SQL operation.
- Materialized SQL rows retain their provider, so invalid `Row.at` and
  ambiguous or invalid `Row.get` failures carry `row_at`/`row_get` context.
- SQL migration construction and lifecycle failures preserve typed provider
  and operation context, including invalid migration targets.
- **Fixed-field accessors use typed dispatch.** Runtime accessors no longer
  route statically known fields through string-name matches. Dynamic keys still
  use strings.
- **Structured error categories no longer inspect rendered messages.** Native
  operations now select their `kind` at the failure boundary, while typed
  registries retain the detail text only for display. This keeps categories
  stable when dependency wording changes and preserves explicit authentication,
  spawn, filesystem, and validation distinctions.
- **Encoding error categories are typed.** `std.encoding.EncodingError.kind`
  now crosses the language boundary as `EncodingErrorKind`; codec, detail, and
  offset remain contextual display fields.
- **JSON token categories are typed.** `JsonToken.kind()` now returns the
  payloadless `JsonTokenKind` enum; exact source spelling remains available
  through `text()`.
- **JSON duplicate-key handling is explicit.** Parsing rejects duplicate
  object keys by default; callers can opt into `JsonDuplicatePolicy.First` or
  `Last` on string, bounded-reader, and JSON Lines entry points. Discarded
  values are still fully validated and retained numbers keep their exact
  spelling.
- **SQL constraint diagnostics are classified.** SQLite constraint result
  codes, PostgreSQL SQLSTATE class 23, and MySQL constraint states/vendor
  codes now produce `SqlErrorKind.Constraint` through connection, statement,
  transaction, pool, and migration boundaries.
- **PostgreSQL query cancellation uses the wire-level cancel request.**
  Cooperative cancellation now reports `SqlErrorKind.Cancelled` for
  PostgreSQL queries and leaves the connection reusable; MySQL remains an
  explicit unsupported path because its synchronous driver has no safe
  per-statement interrupt primitive.
- **PostgreSQL query timeouts use the wire-level cancel request.**
  Deadline-triggered SQLSTATE `57014` is classified as
  `SqlErrorKind.Timeout`; the cancellation helper is joined before direct,
  prepared, transaction, or pooled clients are reused. MySQL remains an
  explicit unsupported path.
- Named SQL calls now reject unused map entries, in addition to rejecting
  missing names, before sending a statement to the provider. This makes
  misspelled named parameters deterministic validation errors across direct,
  prepared, transaction, and pooled operations.
- SQL `execute_many` now sends its validated, provider-normalized placeholder
  spelling to the driver for every backend. Portable `?` and `$n` forms are
  no longer validated and then accidentally sent unchanged.
- SQL float-to-int accessors now reject finite integral values outside the
  `int` range instead of silently saturating at an endpoint.
- **Filesystem OS failures preserve their categories.** File, directory,
  metadata, link, copy, rename, and permission operations now classify native
  `NotFound`, `PermissionDenied`, and invalid-data errors as `FsError.kind`
  values and retain the operation path instead of collapsing them into generic
  I/O errors.
- **Filesystem and migration metadata context no longer comes from diagnostic
  text.** Filesystem paths are passed as structured fields, and migration
  status checks use provider catalog queries to detect an absent metadata table.
- **Network and URL context no longer comes from diagnostic text.** Address and
  URL fields are supplied explicitly when an operation has them; generic error
  constructors leave those fields empty.
- **HTTP transfer codings are fail-closed.** The HTTP/1.x server now accepts
  exactly one `chunked` transfer coding and rejects unsupported, repeated, or
  ambiguous `Transfer-Encoding` values instead of decoding only the portion it
  recognizes.
- **HTTP response framing is fail-closed.** The HTTP/1.x response writer now
  rejects caller-supplied `Transfer-Encoding`, forbids bodies for `304`, omits
  `Content-Length` for bodyless `1xx`/`204`/`304` responses, and enforces the
  zero-length framing required by `205` responses.
- **HTTP response connection semantics are explicit.** The HTTP/1.x response
  writer always closes its stream; caller-supplied `Connection` headers are
  rejected unless they contain exactly the compatible `close` directive. This
  rejects keep-alive, contradictory (`close, keep-alive`), and duplicate
  directives rather than advertising an ambiguous lifecycle.
- **HTTP/1.1 request authority is validated.** The server now requires one
  non-empty `Host` header for HTTP/1.1, rejects duplicate `Host` fields for all
  supported HTTP/1.x requests, and validates parsed header names and values
  before exposing them to handlers.
- **HTTP request readers stream incrementally.** `HttpRequest.set_body_reader`
  now retains a single-use Reader and sends it without first materializing the
  complete body; retries are rejected for non-replayable reader bodies.
- **Buffered HTTP request bodies are bounded.** Sends reject byte payloads
  larger than 16 MiB (and `from_config`/the explicit setter report the error
  immediately); direct field assignment reports the failure at `send()`. Use a
  bounded `Reader` when an explicit streaming limit is needed.
- **SQL placeholder rewriting preserves Unicode.** Positional and named
  parameter scanning now copies non-ASCII SQL code points without splitting
  their UTF-8 bytes, including inside quoted text and comments.
- **Byte-stream failures now return `IoError`.** Reader, Writer, and the
  Readable/Writable/Seek stream operations expose stable `kind`, `detail`, and
  `operation` fields instead of bare string errors.
- **Tee reader cleanup is complete.** Dropping a reader now releases the
  writer reference retained by `reader.tee(writer)`, matching explicit
  `untee`/`close` cleanup and preventing leaked writer handles.
- **Malformed WebSocket frames fail closed.** Length decoding no longer relies
  on panic paths after bounds checks; invalid framing returns the normal typed
  protocol error.
- **Socket-operation failures now return `NetError`.** The runtime now matches
  the compiler's typed network signatures for TCP, UDP, local sockets, and
  poller operations; callers use the structured `kind`, `detail`, and
  `address` fields for programmatic handling and reserve display text for
  humans.
- **HTTP error categories are typed.** `HttpError.kind` crosses the ABI as a
  package-specific `net.HttpErrorKind` enum; category checks do not depend on
  rendered diagnostic text.
- **SQL error categories are typed.** `SqlError.kind` crosses the ABI as a
  package-specific `sql.SqlErrorKind` enum; provider diagnostics remain in
  textual context fields.
- **Provider SQL diagnostics now retain native codes on direct operations.**
  SQLite extended codes, PostgreSQL SQLSTATE/constraint metadata, and MySQL
  state/vendor codes are normalized into `SqlError.code` where available;
  wrapper operations still share the follow-up carrier work.
- **Environment error categories are typed.** `EnvError.kind` crosses the ABI
  as `env.EnvErrorKind`; key and detail context remain textual.
- **Filesystem error categories are typed.** `FsError.kind` crosses the ABI as
  `fs.FsErrorKind`; path and detail context remain textual.
- **Network, URL, and UUID error categories are typed.** Their `kind` fields
  cross the ABI as `net.NetErrorKind`, `url.UrlErrorKind`, and
  `uuid.UuidErrorKind`; address, URL, and detail context remain textual.
- **Data and byte error categories are typed.** JSON, CSV, byte, and bytes
  errors now cross the ABI as package-specific enum values; diagnostic detail
  remains textual.
- **Synchronization and process error categories are typed.** Their `kind`
  fields now cross the ABI as `SyncErrorKind` and `ProcessErrorKind` values;
  diagnostic detail remains textual.
- **TLS error categories are typed.** `TlsError.kind` now crosses the ABI as a
  package-specific enum; handshake, certificate, protocol, and I/O detail
  remains textual context.
- **CLI, crypto, regex, and log error categories are typed.** Their public
  `kind` fields now cross the ABI as package-specific enums; rendered messages
  remain text for humans.
- **Synchronization failures now return `SyncError`.** Threads, locks,
  atomics, channels, worker pools, and coordination primitives expose stable
  `kind` and `detail` fields instead of bare string errors.
- **Data parsing failures now return typed errors.** JSON and CSV parsing,
  conversion, mutation, accessors, and streaming operations return `JsonError`
  or `CsvError` objects with stable `kind`/`detail` fields.
- **Class deserializers use typed data errors.** Generated `from_json`,
  `list_from_json`, and `list_from_csv` entry points wrap generated validation
  failures as `JsonError`/`CsvError` results and preserve parser failures.
- **Byte scalar failures now return `ByteError`.** Checked conversions,
  arithmetic, and shifts expose stable `kind`/`detail` fields instead of raw
  string errors.
- **Byte-sequence failures now return `BytesError`.** Built-in `bytes` values
  and `BytesCursor` binary/UTF-8 operations expose the same structured error
  contract.

## 2026-08-29

### Changed
- **The supported Rust toolchain is now declared in the repository.** Local
  builds and CI use Rust 1.93.1, and Cargo records that minimum version for
  dependency resolution and lint checks.
- **Math FFI wrappers no longer use the unmaintained `paste` crate.** The
  exported symbol names stay unchanged while the wrapper macro receives each
  name explicitly.
- **Synchronization primitives now defer native destruction until every active
  operation and lock owner releases its lifetime pin.** Owner-thread cleanup
  unlocks held primitives before teardown, and the Windows rwlock mode tracker
  preserves repeated shared acquisitions. The closure and capture retain/
  release entry points document their unsafe exactly-one-reference contract;
  the C ABI symbols and balanced compiler-generated ownership flow are
  unchanged.
- **SQLite execution now rejects multi-statement SQL explicitly.** The
  rusqlite 0.40 upgrade makes `execute` and `prepare` fail closed when a query
  contains more than one statement; callers should submit statements
  individually or use a transaction.

## [0.6.1] - 2026-08-27

### Added
- **Stable runtime diagnostic codes.** Terminating runtime failures now use the
  `E06xx` registry, and the typed panic ABI preserves those codes for compiler
  generated programs. Unknown codes intentionally map to `E0699`.

## [0.6.0] - 2026-08-20

### Changed
- **Typed accessors return `result` rather than `optional`**, on both `Json` and
  `SqlValue`. The error names what was actually there - `expected an int, found
  a string` - instead of a bare "no", which left a reader unable to tell a
  string from a null from something else while debugging a document.

  On the SQL side the message already existed and was being discarded: every
  `sql_value_to_*` returns a `Result` with a reason, and the accessors called
  `.ok()` on it. Those declarations were also out of step with the compiler,
  which said `result` while the runtime returned `optional` - a program matching
  `ok`/`err` worked only by coincidence of representation
  (muxlang/mux-compiler#404).

  `mux_json_field` stays `optional`: a key that is not there is the question
  being asked, not a failure of expectation, and that is what lets an
  `optional<T>` field accept a missing key.

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
- **Improved pattern matching**: Better exhaustiveness checking and guard support.

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
