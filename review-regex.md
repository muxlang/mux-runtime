# Regex review fixes

## Causes and fixes

- `replace_with` kept the `REGEXES` mutex guard alive while it called the
  replacement callback. A callback that entered another regex API then waited
  forever on the same non-reentrant mutex. The code now clones the configured
  matcher and drops the guard before invoking user code.
- `full_match` checked the span of the first unanchored find. For `a|ab` and
  lazy quantifiers, that find can be a shorter prefix even when the pattern
  can match the whole string. Regex construction now parses the pattern into
  `regex-syntax` HIR, adds absolute start/end assertions structurally, and
  prints that HIR once for the full-match matcher.
- Wrapping the raw pattern in concrete anchors broke verbose patterns ending
  in a comment because the wrapper text became part of that comment. The
  canonical HIR path preserves valid verbose comments and scoped flags without
  duplicating the regex grammar.

## Changed paths

- `src/regex.rs`
- `tests/regex_unit.rs`
- `Cargo.toml`
- `Cargo.lock`

The CI files were not edited.

## Verification

- Added regressions for `a|ab`, a lazy quantifier, dot-matches-newline flags,
  verbose trailing comments, a literal non-verbose `#`, and callback reentry
  through `mux_regex_is_match`.
- Added `regex-syntax` as a direct optional dependency because the runtime
  needs its public HIR API to add anchors without rewriting user syntax.
- Parent verification outside the sandbox passed the runtime `regex5` tests
  with a limited build. No Cargo command was run by this review agent.
- The worktree contains no new generated files from this task.

## Status

- No remaining scoped regex issues.
- Existing dirty and untracked work outside the assigned regex paths was left
  untouched.
