// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Resource limits on one render: time, depth, output, and `range()`.
//!
//! A template is a program, and a program can loop for days
//! (`range(100000)` nested three deep) or write gigabytes (`'x' * 10**8`
//! emitted in a loop). Each limit below turns that into a typed error that
//! names the file and line, before the deploy hangs or the host runs out of
//! memory. The defaults are an order of magnitude above the heaviest real
//! stack measured, so no legitimate template comes near them.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// The environment variable that overrides the defaults:
/// `PULUMI_YAML_JINJA_LIMITS=fuel=50000000,output=33554432`.
pub const LIMITS_ENV: &str = "PULUMI_YAML_JINJA_LIMITS";

/// minijinja's own ceiling on the recursion limit without its `stacker`
/// feature; asking for more is silently capped there, so it is refused here.
pub const MAX_RECURSION: usize = 500;

/// Limits applied to every render of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JinjaLimits {
    /// Instructions the engine may execute (minijinja fuel).
    pub fuel: u64,
    /// Nesting depth the engine may reach. A macro call costs about 4, an
    /// include 10, so the default allows ~50 nested includes and ~100 nested
    /// macro calls.
    pub recursion: usize,
    /// Bytes one rendered file may hold, `readFile()` contents included.
    pub output_bytes: usize,
    /// Elements one `range()` call may produce.
    pub range: usize,
}

impl JinjaLimits {
    /// Over 10x the most any real stack measured: 6,575 instructions, the
    /// heaviest of 4,583 production project files (see JINJA.md).
    pub const DEFAULT_FUEL: u64 = 100_000;
    pub const DEFAULT_RECURSION: usize = MAX_RECURSION;
    /// 16 MiB, against a measured maximum of 61 KB of rendered output.
    pub const DEFAULT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
    /// Jinja2's own `MAX_RANGE`.
    pub const DEFAULT_RANGE: usize = 100_000;

    /// Parses `key=value[,key=value...]`, starting from the defaults.
    ///
    /// Keys are `fuel`, `recursion`, `output` and `range`; values are
    /// positive integers (underscores allowed). An unknown key, a repeated
    /// key, a zero, or a recursion above [`MAX_RECURSION`] is an error that
    /// names it -- a typo must never silently keep a default.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut limits = Self::default();
        let mut seen: [bool; 4] = [false; 4];
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| format!("{LIMITS_ENV}: {item:?} is not key=value"))?;
            let (key, value) = (key.trim(), value.trim());
            let n: u64 = value
                .replace('_', "")
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    format!("{LIMITS_ENV}: {key}={value:?} is not a positive integer")
                })?;
            let slot = match key {
                "fuel" => 0,
                "recursion" => 1,
                "output" => 2,
                "range" => 3,
                other => {
                    return Err(format!(
                        "{LIMITS_ENV}: unknown limit {other:?} (expected fuel, recursion, output or range)"
                    ))
                }
            };
            if std::mem::replace(&mut seen[slot], true) {
                return Err(format!("{LIMITS_ENV}: {key} is given twice"));
            }
            let as_usize = |n: u64| {
                usize::try_from(n).map_err(|_| format!("{LIMITS_ENV}: {key}={n} is too large"))
            };
            match slot {
                0 => limits.fuel = n,
                1 => {
                    let r = as_usize(n)?;
                    if r > MAX_RECURSION {
                        return Err(format!(
                            "{LIMITS_ENV}: recursion={r} is above the engine's maximum of {MAX_RECURSION}"
                        ));
                    }
                    limits.recursion = r;
                }
                2 => limits.output_bytes = as_usize(n)?,
                _ => limits.range = as_usize(n)?,
            }
        }
        Ok(limits)
    }

    /// The limits for this process: the defaults, overridden by
    /// [`LIMITS_ENV`] when it is set. A malformed value is an error.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(LIMITS_ENV) {
            Ok(spec) => Self::parse(&spec),
            Err(std::env::VarError::NotPresent) => Ok(Self::default()),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{LIMITS_ENV} is not valid UTF-8"))
            }
        }
    }
}

impl Default for JinjaLimits {
    fn default() -> Self {
        Self {
            fuel: Self::DEFAULT_FUEL,
            recursion: Self::DEFAULT_RECURSION,
            output_bytes: Self::DEFAULT_OUTPUT_BYTES,
            range: Self::DEFAULT_RANGE,
        }
    }
}

impl fmt::Display for JinjaLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "fuel={},recursion={},output={},range={}",
            self.fuel, self.recursion, self.output_bytes, self.range
        )
    }
}

/// What one render cost: what the counted performance tests assert on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderStats {
    /// Engine instructions executed (0 on the zero-copy fast path).
    pub fuel: u64,
    /// Bytes of the final output.
    pub output_bytes: usize,
}

/// The tag a `range()` refusal carries in its detail, read back by the
/// classifier.
pub(super) const RANGE_TAG: &str = "range() would produce";

/// Jinja2's `range(stop)` / `range(start, stop[, step])`, refusing more
/// than `limit` elements before allocating any.
pub(super) fn range(
    limit: usize,
    lower: i64,
    upper: Option<i64>,
    step: Option<i64>,
) -> Result<minijinja::Value, minijinja::Error> {
    let (start, stop) = match upper {
        Some(stop) => (lower, stop),
        None => (0, lower),
    };
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "range() arg 3 must not be zero",
        ));
    }
    let span = i128::from(stop) - i128::from(start);
    let len = if (step > 0 && span <= 0) || (step < 0 && span >= 0) {
        0u128
    } else {
        let step = i128::from(step).unsigned_abs();
        span.unsigned_abs().div_ceil(step)
    };
    if len > limit as u128 {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("{RANGE_TAG} {len} elements, more than the limit of {limit}"),
        ));
    }
    let values: Vec<i64> = (0..len as i64).map(|i| start + i * step).collect();
    Ok(minijinja::Value::from(values))
}

/// The output limit, shared by the writer and the formatter of one render.
///
/// The writer sees every byte that reaches the output, literal template text
/// included, but an error it returns reaches the caller stripped of where in
/// the template it arose. The formatter runs at each `{{ ... }}` -- also when
/// the text goes into a `{% set %}` block or a macro's return value rather
/// than the output -- and an error it returns carries that expression's line.
/// So the writer keeps memory bounded and records the overflow, and the
/// formatter is where the render is stopped, with its location.
#[derive(Debug)]
pub(super) struct OutputBudget {
    cap: usize,
    /// Bytes every `{{ ... }}` has produced, wherever they went.
    emitted: AtomicUsize,
    /// Set once the output itself would pass the cap.
    over: AtomicBool,
}

impl OutputBudget {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            emitted: AtomicUsize::new(0),
            over: AtomicBool::new(false),
        }
    }

    pub(super) fn is_over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }

    /// Charges one expression's text; `false` once the budget is spent.
    pub(super) fn charge(&self, bytes: usize) -> bool {
        let before = self.emitted.fetch_add(bytes, Ordering::Relaxed);
        if before.saturating_add(bytes) > self.cap {
            self.over.store(true, Ordering::Relaxed);
        }
        !self.is_over()
    }
}

/// Counts the bytes a value would format to, without allocating them.
pub(super) struct CountingSink(pub(super) usize);

impl fmt::Write for CountingSink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// The render's output buffer. Past the cap it stops storing and records the
/// overflow instead of failing the write, so the formatter -- which knows
/// the line -- is the one that stops the render.
pub(super) struct CappedWriter {
    pub(super) buf: Vec<u8>,
    budget: Arc<OutputBudget>,
}

impl CappedWriter {
    pub(super) fn new(budget: Arc<OutputBudget>, hint: usize) -> Self {
        Self {
            buf: Vec::with_capacity(hint.min(budget.cap)),
            budget,
        }
    }
}

impl io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.budget.cap {
            self.budget.over.store(true, Ordering::Relaxed);
        } else if !self.budget.is_over() {
            self.buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_and_round_trip() {
        let d = JinjaLimits::default();
        assert_eq!(JinjaLimits::parse("").unwrap(), d);
        assert_eq!(JinjaLimits::parse(&d.to_string()).unwrap(), d);
        let l = JinjaLimits::parse(" fuel=1_000 , range=5,output=64,recursion=20 ").unwrap();
        assert_eq!(
            l,
            JinjaLimits {
                fuel: 1000,
                recursion: 20,
                output_bytes: 64,
                range: 5
            }
        );
    }

    #[test]
    fn malformed_specs_are_refused_by_name() {
        for (spec, needle) in [
            ("fuel", "not key=value"),
            ("fuel=0", "positive integer"),
            ("fuel=-1", "positive integer"),
            ("fuel=ten", "positive integer"),
            ("fule=10", "unknown limit \"fule\""),
            ("fuel=1,fuel=2", "given twice"),
            ("recursion=501", "maximum of 500"),
        ] {
            let err = JinjaLimits::parse(spec).unwrap_err();
            assert!(err.contains(needle), "{spec}: {err}");
            assert!(err.starts_with(LIMITS_ENV), "{spec}: {err}");
        }
        assert!(JinjaLimits::parse("recursion=500").is_ok());
    }

    #[test]
    fn range_matches_python_and_refuses_before_allocating() {
        let r = |a, b, c| range(100, a, b, c).map(|v| v.to_string());
        assert_eq!(r(3, None, None).unwrap(), "[0, 1, 2]");
        assert_eq!(r(1, Some(4), None).unwrap(), "[1, 2, 3]");
        assert_eq!(r(10, Some(0), Some(-3)).unwrap(), "[10, 7, 4, 1]");
        assert_eq!(r(0, None, None).unwrap(), "[]");
        assert_eq!(r(5, Some(1), None).unwrap(), "[]");
        assert!(r(0, Some(1), Some(0)).is_err());
        assert!(range(100, 0, Some(101), None).is_err());
        assert!(range(100, 0, Some(100), None).is_ok());
        // A span near i64's extremes is computed without overflow.
        let err = range(100, i64::MIN, Some(i64::MAX), None).unwrap_err();
        assert!(err.to_string().contains(RANGE_TAG));
    }

    #[test]
    fn capped_writer_stops_storing_at_the_cap_and_says_so() {
        let budget = Arc::new(OutputBudget::new(4));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0);
        w.write_all(b"abcd").unwrap();
        assert!(!budget.is_over());
        w.write_all(b"e").unwrap();
        assert!(budget.is_over());
        w.write_all(b"f").unwrap();
        assert_eq!(w.buf, b"abcd", "nothing is stored past the cap");
    }

    #[test]
    fn the_budget_counts_every_expression() {
        let budget = OutputBudget::new(10);
        assert!(budget.charge(6));
        assert!(budget.charge(4));
        assert!(!budget.charge(1));
        assert!(!budget.charge(0), "once spent it stays spent");
        let mut sink = CountingSink(0);
        std::fmt::Write::write_fmt(&mut sink, format_args!("{}", 12345)).unwrap();
        assert_eq!(sink.0, 5);
    }
}
