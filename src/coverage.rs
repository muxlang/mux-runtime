//! Opt-in coverage counters for isolated `mux test` processes.
//!
//! Storage grows with source sites, not loop iterations. The exit handler
//! writes a report with a final marker so the runner can reject partial writes.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::raw::c_char;
use std::sync::{LazyLock, Mutex};

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct CoverageSite {
    file: Vec<u8>,
    line: i64,
    branch: Option<(i64, bool)>,
}

#[derive(Default)]
struct CoverageCounts {
    sites: BTreeMap<CoverageSite, u64>,
    overflowed: bool,
}

impl CoverageCounts {
    fn record(&mut self, site: CoverageSite) {
        let count = self.sites.entry(site).or_default();
        if let Some(next) = count.checked_add(1) {
            *count = next;
        } else {
            self.overflowed = true;
        }
    }

    fn write_report(&self, output: &mut impl Write) -> io::Result<()> {
        if self.overflowed {
            return Err(io::Error::other("coverage counter overflow"));
        }
        writeln!(output, "MUXCOV2")?;
        for (site, count) in &self.sites {
            let file = hex_encode(&site.file);
            match site.branch {
                None => writeln!(output, "S\t{file}\t{}\t{count}", site.line)?,
                Some((id, taken)) => writeln!(
                    output,
                    "B\t{file}\t{}\t{id}\t{}\t{count}",
                    site.line,
                    i32::from(taken)
                )?,
            }
        }
        writeln!(output, "END")
    }
}

struct CoverageState {
    output: File,
    counts: CoverageCounts,
}

static COVERAGE: LazyLock<Mutex<Option<CoverageState>>> = LazyLock::new(|| {
    let state = (|| {
        let path = std::env::var_os("MUX_COVERAGE_FILE")?;
        let output = File::create(path).ok()?;
        if unsafe { libc::atexit(finish_coverage) } != 0 {
            return None;
        }
        Some(CoverageState {
            output,
            counts: CoverageCounts::default(),
        })
    })();
    Mutex::new(state)
});

extern "C" fn finish_coverage() {
    let state = COVERAGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(state) = state {
        let mut output = BufWriter::new(state.output);
        // Failed writes lack the final marker and are rejected by the runner.
        if state.counts.write_report(&mut output).is_ok() {
            let _ = output.flush();
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        output.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    output
}

/// Count one executed statement or branch outcome. Ordinary builds omit calls.
///
/// # Safety
/// `file` must be null or point to a valid NUL-terminated C string for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_coverage_record(
    file: *const c_char,
    line: i64,
    kind: i32,
    branch_id: i64,
    taken: i32,
) {
    if file.is_null() || line <= 0 || !matches!(kind, 0 | 1) {
        return;
    }
    let mut state = COVERAGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = state.as_mut() {
        state.counts.record(CoverageSite {
            file: unsafe { CStr::from_ptr(file) }.to_bytes().to_vec(),
            line,
            branch: (kind == 1).then_some((branch_id, taken != 0)),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{CoverageCounts, CoverageSite};

    #[test]
    fn repeated_hits_produce_one_counted_record() {
        let mut counts = CoverageCounts::default();
        for _ in 0..100_000 {
            counts.record(CoverageSite {
                file: b"a.mux".to_vec(),
                line: 7,
                branch: None,
            });
        }
        let mut report = Vec::new();
        counts.write_report(&mut report).unwrap();
        assert_eq!(report, b"MUXCOV2\nS\t612e6d7578\t7\t100000\nEND\n");
        assert_eq!(counts.sites.len(), 1);
    }

    #[test]
    fn counter_overflow_does_not_publish_a_complete_report() {
        let site = || CoverageSite {
            file: b"a.mux".to_vec(),
            line: 1,
            branch: Some((0, true)),
        };
        let mut counts = CoverageCounts::default();
        counts.sites.insert(site(), u64::MAX);
        counts.record(site());
        let mut report = Vec::new();
        assert!(counts.write_report(&mut report).is_err());
        assert!(report.is_empty());
    }
}
