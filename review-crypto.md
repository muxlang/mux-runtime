# Crypto framing review

## Review result

The version 2 file framing follows the requested binding rules. The 26-byte
header carries a random 16-byte file identity. Each record authenticates the
header, record index, record length, and caller data. An authenticated empty
record marks the end of every file, including an empty input. The reader
requires that record to be the end of the file before publishing output.

The temporary output is created beside the destination and is published only
after all records authenticate and the file is flushed. On Unix, the temporary
file mode is now set exactly to `0600`.

## Changes in this review

- `src/crypto.rs`: enforce exact Unix `0600` permissions on the temporary file
  descriptor before writing.
- `tests/crypto_unit.rs`: verify the complete record sequence ends in a zero-
  length final record, assert that a wrong key leaves the destination alone,
  and require the published Unix mode to be exactly `0600`.

The existing 1.5 MiB fixture now exercises multiple data records plus the
authenticated final record. Its truncation, record-splicing, changed-header,
trailing-byte, wrong-AAD, and wrong-key cases all keep the old destination
untouched.

## Verification

Parent verification outside the sandbox passed the runtime `crypto4` tests
with a limited build. No Cargo command was run by this review agent. This
review makes no claim of cryptographic security certification.

## Status

- No remaining scoped crypto issues.
- Existing dirty and untracked work outside the assigned crypto paths was left
  untouched.
