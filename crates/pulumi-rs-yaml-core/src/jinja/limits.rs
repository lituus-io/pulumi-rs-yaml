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
/// The same refusal for a list built from a count other than `range()`'s:
/// `slice`'s one list per count, and a repeated list.
pub(super) const LIST_TAG: &str = "a list past the range limit:";

/// Jinja2's `range(stop)` / `range(start, stop[, step])`, refusing more
/// than `limit` elements before allocating any.
/// The global a render publishes its range limit under, for the filters
/// that produce one list per count (`slice`).
pub(super) const RANGE_LIMIT_GLOBAL: &str = "__rsyaml_range_limit";

/// The render's range limit, as published in its globals.
pub(super) fn range_cap(state: &minijinja::State<'_, '_>) -> usize {
    state
        .lookup(RANGE_LIMIT_GLOBAL)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(JinjaLimits::DEFAULT_RANGE)
}

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
/// The formatter runs at each `{{ ... }}` -- also when the text goes into a
/// `{% set %}` block or a macro's return value rather than the output -- and
/// an error it returns carries that expression's line. So every expression
/// is charged here before it is written, against both what expressions have
/// produced so far and what the output already holds, literal text
/// included: whichever crosses the limit first, the expression that would
/// carry it over is the one refused, at its own line.
///
/// Literal text alone can still cross it (a loop of plain text): the writer
/// refuses that write and records where in the template the text lies, for
/// the caller to place the refusal.
#[derive(Debug)]
pub(super) struct OutputBudget {
    cap: usize,
    /// Bytes every `{{ ... }}` has produced, wherever they went.
    emitted: AtomicUsize,
    /// Bytes the output holds.
    written: AtomicUsize,
    /// Set once the output itself would pass the cap.
    over: AtomicBool,
}

impl OutputBudget {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            emitted: AtomicUsize::new(0),
            written: AtomicUsize::new(0),
            over: AtomicBool::new(false),
        }
    }

    pub(super) fn cap(&self) -> usize {
        self.cap
    }

    pub(super) fn is_over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }

    /// Charges one expression's text; `false` once the budget is spent.
    pub(super) fn charge(&self, bytes: usize) -> bool {
        let before = self.emitted.fetch_add(bytes, Ordering::Relaxed);
        let held = self.written.load(Ordering::Relaxed);
        if before.saturating_add(bytes) > self.cap || held.saturating_add(bytes) > self.cap {
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

/// A buffer that stops storing past `cap` and says so, for text a filter
/// builds before it reaches the output.
pub(super) struct LimitedBuf {
    pub(super) bytes: Vec<u8>,
    pub(super) over: bool,
    cap: usize,
}

impl LimitedBuf {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            over: false,
            cap,
        }
    }
}

impl io::Write for LimitedBuf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(data.len()) > self.cap {
            self.over = true;
            return Err(io::Error::other("output limit"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The render's output buffer. A write that would pass the cap is refused
/// and nothing past the cap is stored.
///
/// Expressions are refused earlier, by the formatter, so a write refused
/// here is literal template text, and the engine reports a refused write
/// with no location at all. The writer places it instead, by address: the
/// compiled template borrows `template`, so the rendered file's own text
/// arrives as slices of it, and the refusal is kept as the offset where the
/// cap was crossed. Text from another file -- an include, a parent template
/// -- is placed at the statement that brought it in: the first one after
/// the last text of this file written before it.
pub(super) struct CappedWriter<'t> {
    pub(super) buf: Vec<u8>,
    budget: Arc<OutputBudget>,
    template: &'t str,
    /// Where the last write of the template's own text ended.
    own_end: usize,
    crossed: Option<usize>,
}

impl<'t> CappedWriter<'t> {
    pub(super) fn new(budget: Arc<OutputBudget>, hint: usize, template: &'t str) -> Self {
        Self {
            buf: Vec::with_capacity(hint.min(budget.cap)),
            budget,
            template,
            own_end: 0,
            crossed: None,
        }
    }

    /// Where in the template text the cap was crossed.
    pub(super) fn crossed(&self) -> Option<usize> {
        self.crossed
    }

    /// The offset of `data` in the template text, when it is a slice of it.
    /// Addresses are compared, never dereferenced.
    fn offset_in_template(&self, data: &[u8]) -> Option<usize> {
        let base = self.template.as_ptr() as usize;
        let at = (data.as_ptr() as usize).checked_sub(base)?;
        (at.checked_add(data.len())? <= self.template.len()).then_some(at)
    }

    /// The statement after the template's last text written.
    fn next_statement(&self) -> Option<usize> {
        let rest = self.template.get(self.own_end..)?;
        rest.find("{%").map(|i| self.own_end + i)
    }
}

impl io::Write for CappedWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = self.budget.cap.saturating_sub(self.buf.len());
        let own = self.offset_in_template(data);
        if data.len() > room || self.budget.is_over() {
            self.budget.over.store(true, Ordering::Relaxed);
            if self.crossed.is_none() {
                self.crossed = match own {
                    Some(at) => Some(at + room),
                    None => self.next_statement(),
                };
            }
            return Err(io::Error::other("output limit"));
        }
        if let Some(at) = own {
            self.own_end = at + data.len();
        }
        self.buf.extend_from_slice(data);
        self.budget.written.store(self.buf.len(), Ordering::Relaxed);
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
    fn capped_writer_refuses_past_the_cap_and_says_where() {
        let template = "abcdefgh";
        let budget = Arc::new(OutputBudget::new(4));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0, template);
        w.write_all(&template.as_bytes()[..3]).unwrap();
        assert!(!budget.is_over());
        // Bytes 3..6 of the template: the cap is crossed at its byte 4.
        assert!(w.write_all(&template.as_bytes()[3..6]).is_err());
        assert!(budget.is_over());
        assert_eq!(w.crossed(), Some(4));
        assert!(
            w.write_all(b"z").is_err(),
            "once over, every write is refused"
        );
        assert_eq!(w.buf, b"abc", "nothing is stored past the cap");
        assert_eq!(w.crossed(), Some(4), "the first crossing is the one kept");
    }

    #[test]
    fn text_from_another_file_is_placed_at_the_statement_that_brought_it() {
        let template = "a: {% include 'x' %}\nb: {% include 'y' %}";
        let other = String::from("0123456789");
        let budget = Arc::new(OutputBudget::new(8));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0, template);
        w.write_all(&template.as_bytes()[..3]).unwrap();
        assert!(w.write_all(other.as_bytes()).is_err());
        assert_eq!(w.crossed(), Some(3), "the include after `a: `");
        // Nothing of the file written yet: its first statement.
        let budget = Arc::new(OutputBudget::new(2));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0, template);
        assert!(w.write_all(other.as_bytes()).is_err());
        assert_eq!(w.crossed(), Some(3));
        // With no statement left to name, unplaced.
        let budget = Arc::new(OutputBudget::new(2));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0, "plain");
        assert!(w.write_all(other.as_bytes()).is_err());
        assert_eq!(w.crossed(), None);
    }

    #[test]
    fn an_expression_is_charged_against_the_text_already_written() {
        let budget = Arc::new(OutputBudget::new(10));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0, "");
        w.write_all(b"12345678").unwrap();
        assert!(budget.charge(2), "8 written + 2 fits");
        let budget = Arc::new(OutputBudget::new(10));
        let mut w = CappedWriter::new(Arc::clone(&budget), 0, "");
        w.write_all(b"12345678").unwrap();
        assert!(!budget.charge(3), "8 written + 3 does not");
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
