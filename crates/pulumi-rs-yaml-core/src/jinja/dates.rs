// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Python's `date`, `datetime` and `timedelta`, naive, in pure Rust.
//!
//! Sources: `date(y, m, d)`, `datetime(y, m, d[, h, mi, s, us])`,
//! `timedelta(days=, seconds=, microseconds=, milliseconds=, minutes=,
//! hours=, weeks=)`, and the filters `to_date`, `to_datetime` and
//! `strptime(fmt)`. Methods: `strftime`, `isoformat`, `date`, `weekday`,
//! `isoweekday`, `toordinal`, `replace`, `total_seconds`; attributes `year`
//! through `microsecond`, and `days`/`seconds`/`microseconds`. Dates compare,
//! and `date ± timedelta`, `date - date` and `timedelta ± timedelta` work
//! (the prepass routes `+` and `-` here when a template uses dates).
//!
//! The calendar is proleptic Gregorian over years 1-9999, as Python's.
//! There is no timezone database: every value is naive, `%z` and `%Z` are
//! empty, and an offset in an ISO string is refused rather than guessed.

use std::cmp::Ordering as CmpOrdering;
use std::fmt;
use std::sync::Arc;

use minijinja::value::{DynObject, Kwargs, Object, ObjectRepr, Value};
use minijinja::{Error, ErrorKind, State};

use super::arith::engine_op;
use super::methods::{value_error, Call};

const MICROS_PER_DAY: i64 = 86_400_000_000;
const MIN_ORDINAL: i64 = 1; // 0001-01-01
const MAX_ORDINAL: i64 = 3_652_059; // 9999-12-31

/// Days since 0001-01-01 (ordinal 1) for a civil date; Hinnant's algorithm.
fn to_ordinal(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    // 1970-01-01 is day 719_468 of this era count; ordinal of 1970-01-01 is 719_163.
    era * 146_097 + doe - 719_468 + 719_163
}

fn from_ordinal(n: i64) -> (i64, i64, i64) {
    let z = n - 719_163 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn check_date(y: i64, m: i64, d: i64) -> Result<(), Error> {
    if !(1..=9999).contains(&y) {
        return Err(value_error(format!("year {y} is out of range")));
    }
    if !(1..=12).contains(&m) {
        return Err(value_error("month must be in 1..12"));
    }
    if d < 1 || d > days_in_month(y, m) {
        return Err(value_error("day is out of range for month"));
    }
    Ok(())
}

/// A naive moment: an ordinal day and microseconds into it; `time` is
/// `None` for a `date`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Moment {
    day: i64,
    micros: Option<i64>,
}

impl Moment {
    fn ymd(&self) -> (i64, i64, i64) {
        from_ordinal(self.day)
    }

    fn hmsu(&self) -> (i64, i64, i64, i64) {
        let us = self.micros.unwrap_or(0);
        let s = us / 1_000_000;
        (s / 3600, s % 3600 / 60, s % 60, us % 1_000_000)
    }

    fn total_micros(&self) -> i128 {
        i128::from(self.day) * i128::from(MICROS_PER_DAY) + i128::from(self.micros.unwrap_or(0))
    }
}

/// A `date` or a `datetime`.
#[derive(Clone, Copy)]
pub(super) struct PyDate(Moment);

/// A `timedelta`, in microseconds.
#[derive(Clone, Copy, Debug)]
pub(super) struct PyDelta(i128);

impl fmt::Debug for PyDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for PyDate {
    /// Python's `str()`: `2026-10-18`, or `2026-10-18 12:30:00[.ffffff]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.0.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")?;
        if self.0.micros.is_some() {
            let (h, mi, s, us) = self.0.hmsu();
            write!(f, " {h:02}:{mi:02}:{s:02}")?;
            if us != 0 {
                write!(f, ".{us:06}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for PyDelta {
    /// Python's `str(timedelta)`: `[-]D day[s], H:MM:SS[.ffffff]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.0;
        let day = i128::from(MICROS_PER_DAY);
        let days = total.div_euclid(day);
        let rest = total.rem_euclid(day);
        if days != 0 {
            write!(f, "{days} day{}, ", if days.abs() == 1 { "" } else { "s" })?;
        }
        let secs = rest / 1_000_000;
        let us = rest % 1_000_000;
        write!(
            f,
            "{}:{:02}:{:02}",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )?;
        if us != 0 {
            write!(f, ".{us:06}")?;
        }
        Ok(())
    }
}

fn int(v: &Value, what: &str) -> Result<i64, Error> {
    i64::try_from(v.clone()).map_err(|_| value_error(format!("{what} must be an integer")))
}

fn make(y: i64, m: i64, d: i64, time: Option<(i64, i64, i64, i64)>) -> Result<Value, Error> {
    check_date(y, m, d)?;
    let micros = match time {
        Some((h, mi, s, us)) => {
            if !(0..24).contains(&h)
                || !(0..60).contains(&mi)
                || !(0..60).contains(&s)
                || !(0..1_000_000).contains(&us)
            {
                return Err(value_error("time component out of range"));
            }
            Some(((h * 60 + mi) * 60 + s) * 1_000_000 + us)
        }
        None => None,
    };
    Ok(Value::from_object(PyDate(Moment {
        day: to_ordinal(y, m, d),
        micros,
    })))
}

/// `date(year, month, day)`.
pub(super) fn date(y: Value, m: Value, d: Value) -> Result<Value, Error> {
    make(int(&y, "year")?, int(&m, "month")?, int(&d, "day")?, None)
}

/// `datetime(year, month, day, hour=0, minute=0, second=0, microsecond=0)`.
pub(super) fn datetime(args: minijinja::value::Rest<Value>) -> Result<Value, Error> {
    let a = &args.0;
    if a.len() < 3 || a.len() > 7 {
        return Err(Error::new(
            ErrorKind::MissingArgument,
            "datetime() takes 3 to 7 integers",
        ));
    }
    let n = |i: usize| a.get(i).map_or(Ok(0), |v| int(v, "datetime field"));
    make(n(0)?, n(1)?, n(2)?, Some((n(3)?, n(4)?, n(5)?, n(6)?)))
}

/// `timedelta(days=0, seconds=0, microseconds=0, milliseconds=0, minutes=0,
/// hours=0, weeks=0)`, positional in that order as well.
pub(super) fn timedelta(
    args: minijinja::value::Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    const UNITS: [(&str, i128); 7] = [
        ("days", 86_400_000_000),
        ("seconds", 1_000_000),
        ("microseconds", 1),
        ("milliseconds", 1_000),
        ("minutes", 60_000_000),
        ("hours", 3_600_000_000),
        ("weeks", 604_800_000_000),
    ];
    if args.0.len() > UNITS.len() {
        return Err(Error::new(
            ErrorKind::TooManyArguments,
            "timedelta() takes at most 7 arguments",
        ));
    }
    let mut total: f64 = 0.0;
    let mut exact: i128 = 0;
    for (i, (name, scale)) in UNITS.iter().enumerate() {
        let v = match args.0.get(i) {
            Some(v) => Some(v.clone()),
            None => kwargs.get::<Option<Value>>(name)?,
        };
        if let Some(v) = v {
            if minijinja::tests::is_integer(&v) {
                exact += i128::try_from(v)? * scale;
            } else {
                total += f64::try_from(v)? * *scale as f64;
            }
        }
    }
    kwargs.assert_all_used()?;
    let micros = exact + total.round_ties_even() as i128;
    // Python's bound: |days| <= 999999999.
    if micros.abs() > 999_999_999i128 * i128::from(MICROS_PER_DAY) {
        return Err(value_error("timedelta out of range"));
    }
    Ok(Value::from_object(PyDelta(micros)))
}

/// Parses `YYYY-MM-DD[(T| )HH:MM[:SS[.ffffff]]]`.
/// A parsed ISO string: year, month, day, and the time when there is one.
type IsoParts = (i64, i64, i64, Option<(i64, i64, i64, i64)>);

fn parse_iso(s: &str) -> Result<IsoParts, Error> {
    let bad = || value_error(format!("Invalid isoformat string: '{s}'"));
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64, Error> {
        let t = s.get(r).ok_or_else(bad)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        t.parse().map_err(|_| bad())
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b.len() == 10 {
        return Ok((y, m, d, None));
    }
    if !matches!(b[10], b'T' | b' ') || b.len() < 16 || b[13] != b':' {
        return Err(bad());
    }
    let (h, mi) = (num(11..13)?, num(14..16)?);
    let (mut sec, mut us) = (0, 0);
    let mut i = 16;
    if b.len() > i {
        if b[i] != b':' || b.len() < i + 3 {
            return Err(bad());
        }
        sec = num(i + 1..i + 3)?;
        i += 3;
        if b.len() > i {
            if b[i] != b'.' {
                return Err(value_error(format!(
                    "'{s}': a UTC offset or zone is not supported; dates here are naive"
                )));
            }
            let frac = s.get(i + 1..).ok_or_else(bad)?;
            if frac.is_empty() || frac.len() > 6 || !frac.bytes().all(|c| c.is_ascii_digit()) {
                return Err(bad());
            }
            us = format!("{frac:0<6}").parse().map_err(|_| bad())?;
        }
    }
    Ok((y, m, d, Some((h, mi, sec, us))))
}

/// `to_date`: a date from an ISO string, a date or a datetime.
pub(super) fn to_date(v: Value) -> Result<Value, Error> {
    if let Some(d) = v.downcast_object_ref::<PyDate>() {
        return Ok(Value::from_object(PyDate(Moment {
            day: d.0.day,
            micros: None,
        })));
    }
    let s = v
        .as_str()
        .ok_or_else(|| value_error("to_date expects an ISO date string"))?;
    let (y, m, d, _) = parse_iso(s)?;
    make(y, m, d, None)
}

/// `to_datetime`: a datetime from an ISO string or a date.
pub(super) fn to_datetime(v: Value) -> Result<Value, Error> {
    if let Some(d) = v.downcast_object_ref::<PyDate>() {
        return Ok(Value::from_object(PyDate(Moment {
            day: d.0.day,
            micros: Some(d.0.micros.unwrap_or(0)),
        })));
    }
    let s = v
        .as_str()
        .ok_or_else(|| value_error("to_datetime expects an ISO date-time string"))?;
    let (y, m, d, t) = parse_iso(s)?;
    make(y, m, d, Some(t.unwrap_or((0, 0, 0, 0))))
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// `strptime(s, fmt)`: the directives `strftime` writes, read back.
pub(super) fn strptime(s: String, fmt: String) -> Result<Value, Error> {
    let bad = || value_error(format!("time data '{s}' does not match format '{fmt}'"));
    let (mut y, mut mo, mut d, mut h, mut mi, mut sec, mut us) = (1900, 1, 1, 0, 0, 0, 0);
    let mut pm: Option<bool> = None;
    let mut rest = s.as_str();
    let mut f = fmt.chars().peekable();
    let digits = |rest: &mut &str, max: usize| -> Result<i64, Error> {
        let n = rest
            .bytes()
            .take(max)
            .take_while(u8::is_ascii_digit)
            .count();
        if n == 0 {
            return Err(bad());
        }
        let v = rest[..n].parse().map_err(|_| bad())?;
        *rest = &rest[n..];
        Ok(v)
    };
    let name = |rest: &mut &str, names: &[&str]| -> Result<i64, Error> {
        for (i, full) in names.iter().enumerate() {
            for cand in [*full, &full[..3]] {
                if rest.len() >= cand.len() && rest[..cand.len()].eq_ignore_ascii_case(cand) {
                    *rest = &rest[cand.len()..];
                    return Ok(i as i64);
                }
            }
        }
        Err(bad())
    };
    while let Some(c) = f.next() {
        if c != '%' {
            rest = rest.strip_prefix(c).ok_or_else(bad)?;
            continue;
        }
        match f.next().ok_or_else(bad)? {
            'Y' => y = digits(&mut rest, 4)?,
            'y' => {
                // Python: 69-99 are the 1900s, 00-68 the 2000s.
                let n = digits(&mut rest, 2)?;
                y = if n >= 69 { 1900 + n } else { 2000 + n };
            }
            'm' => mo = digits(&mut rest, 2)?,
            'd' => d = digits(&mut rest, 2)?,
            'H' => h = digits(&mut rest, 2)?,
            'I' => h = digits(&mut rest, 2)?,
            'M' => mi = digits(&mut rest, 2)?,
            'S' => sec = digits(&mut rest, 2)?,
            'f' => {
                let n = rest.bytes().take(6).take_while(u8::is_ascii_digit).count();
                if n == 0 {
                    return Err(bad());
                }
                us = format!("{:0<6}", &rest[..n]).parse().map_err(|_| bad())?;
                rest = &rest[n..];
            }
            'p' => {
                let up = rest.get(..2).map(str::to_ascii_uppercase);
                pm = match up.as_deref() {
                    Some("AM") => Some(false),
                    Some("PM") => Some(true),
                    _ => return Err(bad()),
                };
                rest = &rest[2..];
            }
            'b' | 'B' => mo = name(&mut rest, &MONTHS)? + 1,
            'a' | 'A' => {
                name(&mut rest, &DAYS)?;
            }
            'j' => {
                let doy = digits(&mut rest, 3)?;
                let (yy, mm, dd) = from_ordinal(to_ordinal(y, 1, 1) + doy - 1);
                (y, mo, d) = (yy, mm, dd);
            }
            '%' => rest = rest.strip_prefix('%').ok_or_else(bad)?,
            other => {
                return Err(value_error(format!(
                    "'{other}' is a bad directive in format '%{other}'"
                )))
            }
        }
    }
    if !rest.is_empty() {
        return Err(value_error(format!("unconverted data remains: {rest}")));
    }
    if let Some(pm) = pm {
        h = h % 12 + if pm { 12 } else { 0 };
    }
    make(y, mo, d, Some((h, mi, sec, us)))
}

impl PyDate {
    fn strftime(&self, fmt: &str) -> Result<String, Error> {
        let (y, m, d) = self.0.ymd();
        let (h, mi, s, us) = self.0.hmsu();
        let wd = (self.0.day + 6).rem_euclid(7); // Monday = 0
        let mut out = String::with_capacity(fmt.len() + 16);
        let mut it = fmt.chars();
        while let Some(c) = it.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            use std::fmt::Write as _;
            let _ = match it.next() {
                Some('Y') => write!(out, "{y:04}"),
                Some('y') => write!(out, "{:02}", y % 100),
                Some('m') => write!(out, "{m:02}"),
                Some('d') => write!(out, "{d:02}"),
                Some('H') => write!(out, "{h:02}"),
                Some('I') => write!(out, "{:02}", if h % 12 == 0 { 12 } else { h % 12 }),
                Some('M') => write!(out, "{mi:02}"),
                Some('S') => write!(out, "{s:02}"),
                Some('f') => write!(out, "{us:06}"),
                Some('p') => write!(out, "{}", if h < 12 { "AM" } else { "PM" }),
                Some('b') => write!(out, "{}", &MONTHS[(m - 1) as usize][..3]),
                Some('B') => write!(out, "{}", MONTHS[(m - 1) as usize]),
                Some('a') => write!(out, "{}", &DAYS[wd as usize][..3]),
                Some('A') => write!(out, "{}", DAYS[wd as usize]),
                Some('w') => write!(out, "{}", (wd + 1) % 7),
                Some('u') => write!(out, "{}", wd + 1),
                Some('j') => write!(out, "{:03}", self.0.day - to_ordinal(y, 1, 1) + 1),
                Some('z') | Some('Z') => Ok(()),
                Some('%') => write!(out, "%"),
                Some(other) => {
                    return Err(value_error(format!(
                        "unsupported strftime directive '%{other}'"
                    )))
                }
                None => return Err(value_error("strftime format ends with a lone '%'")),
            };
        }
        Ok(out)
    }

    fn isoformat(&self) -> String {
        let s = self.to_string();
        if self.0.micros.is_some() {
            s.replacen(' ', "T", 1)
        } else {
            s
        }
    }
}

impl Object for PyDate {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result
    where
        Self: Sized + 'static,
    {
        fmt::Display::fmt(&**self, f)
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let (y, m, d) = self.0.ymd();
        let (h, mi, s, us) = self.0.hmsu();
        let timed = self.0.micros.is_some();
        Some(Value::from(match key.as_str()? {
            "year" => y,
            "month" => m,
            "day" => d,
            "hour" if timed => h,
            "minute" if timed => mi,
            "second" if timed => s,
            "microsecond" if timed => us,
            _ => return None,
        }))
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<CmpOrdering> {
        let o = other.downcast_ref::<PyDate>()?;
        (self.0.micros.is_some() == o.0.micros.is_some()).then(|| self.0.cmp(&o.0))
    }

    fn call_method(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let c = Call::new(state, args)?;
        match method {
            "strftime" => {
                let f = c.get(0, "format")?.ok_or_else(|| {
                    Error::new(ErrorKind::MissingArgument, "strftime() needs a format")
                })?;
                c.done("strftime", 1)?;
                Ok(Value::from(self.strftime(f.as_str().unwrap_or_default())?))
            }
            "isoformat" => {
                c.done("isoformat", 0)?;
                Ok(Value::from(self.isoformat()))
            }
            "date" => {
                c.done("date", 0)?;
                Ok(Value::from_object(PyDate(Moment {
                    day: self.0.day,
                    micros: None,
                })))
            }
            "weekday" | "isoweekday" => {
                c.done(method, 0)?;
                let wd = (self.0.day + 6).rem_euclid(7);
                Ok(Value::from(if method == "weekday" { wd } else { wd + 1 }))
            }
            "toordinal" => {
                c.done("toordinal", 0)?;
                Ok(Value::from(self.0.day))
            }
            "replace" => {
                let (y, m, d) = self.0.ymd();
                let (h, mi, s, us) = self.0.hmsu();
                let mut f = [y, m, d, h, mi, s, us];
                let names = [
                    "year",
                    "month",
                    "day",
                    "hour",
                    "minute",
                    "second",
                    "microsecond",
                ];
                let fields = if self.0.micros.is_some() { 7 } else { 3 };
                for (i, n) in names.iter().enumerate().take(fields) {
                    if let Some(v) = c.get(i, n)? {
                        f[i] = int(&v, n)?;
                    }
                }
                c.done("replace", fields)?;
                make(
                    f[0],
                    f[1],
                    f[2],
                    self.0.micros.map(|_| (f[3], f[4], f[5], f[6])),
                )
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

impl Object for PyDelta {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result
    where
        Self: Sized + 'static,
    {
        fmt::Display::fmt(&**self, f)
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let day = i128::from(MICROS_PER_DAY);
        let rest = self.0.rem_euclid(day);
        Some(Value::from(match key.as_str()? {
            "days" => self.0.div_euclid(day),
            "seconds" => rest / 1_000_000,
            "microseconds" => rest % 1_000_000,
            _ => return None,
        }))
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<CmpOrdering> {
        Some(self.0.cmp(&other.downcast_ref::<PyDelta>()?.0))
    }

    fn call_method(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let c = Call::new(state, args)?;
        match method {
            "total_seconds" => {
                c.done("total_seconds", 0)?;
                Ok(Value::from(self.0 as f64 / 1e6))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

fn moment_plus(m: Moment, delta: i128) -> Result<Value, Error> {
    let total = m.total_micros()
        + if m.micros.is_some() {
            delta
        } else {
            // A date moves by whole days; the time part of the delta is dropped.
            delta.div_euclid(i128::from(MICROS_PER_DAY)) * i128::from(MICROS_PER_DAY)
        };
    let day = total.div_euclid(i128::from(MICROS_PER_DAY));
    let micros = total.rem_euclid(i128::from(MICROS_PER_DAY));
    if !(i128::from(MIN_ORDINAL)..=i128::from(MAX_ORDINAL)).contains(&day) {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "date value out of range",
        ));
    }
    Ok(Value::from_object(PyDate(Moment {
        day: day as i64,
        micros: m.micros.map(|_| micros as i64),
    })))
}

/// The date/timedelta arithmetic, or `None` when neither side is a date
/// or a timedelta (the caller then uses the engine's own operator).
fn arith(a: &Value, b: &Value, sub: bool) -> Option<Result<Value, Error>> {
    let (da, db) = (
        a.downcast_object_ref::<PyDate>(),
        b.downcast_object_ref::<PyDate>(),
    );
    let (ta, tb) = (
        a.downcast_object_ref::<PyDelta>(),
        b.downcast_object_ref::<PyDelta>(),
    );
    if da.is_none() && db.is_none() && ta.is_none() && tb.is_none() {
        return None;
    }
    let unsupported = || {
        Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "unsupported operand types for {}",
                if sub { "-" } else { "+" }
            ),
        )
    };
    Some(match (da, db, ta, tb) {
        (Some(d), None, None, Some(t)) => moment_plus(d.0, if sub { -t.0 } else { t.0 }),
        (None, Some(d), Some(t), None) if !sub => moment_plus(d.0, t.0),
        (Some(x), Some(y), None, None) if sub && x.0.micros.is_some() == y.0.micros.is_some() => {
            Ok(Value::from_object(PyDelta(
                x.0.total_micros() - y.0.total_micros(),
            )))
        }
        (None, None, Some(x), Some(y)) => Ok(Value::from_object(PyDelta(if sub {
            x.0 - y.0
        } else {
            x.0 + y.0
        }))),
        _ => Err(unsupported()),
    })
}

/// `__rsyaml_add(a, b)`: `a + b`, dates included.
pub(super) fn add(_state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    match arith(&a, &b, false) {
        Some(r) => r,
        None => engine_op(a, b, "+"),
    }
}

/// `__rsyaml_sub(a, b)`: `a - b`, dates included.
pub(super) fn sub(_state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    match arith(&a, &b, true) {
        Some(r) => r,
        None => engine_op(a, b, "-"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinals_round_trip_and_match_python() {
        assert_eq!(to_ordinal(1, 1, 1), 1);
        assert_eq!(to_ordinal(1970, 1, 1), 719_163);
        assert_eq!(to_ordinal(2026, 10, 18), 739_907);
        assert_eq!(to_ordinal(9999, 12, 31), MAX_ORDINAL);
        for n in [1, 59, 60, 365, 366, 719_163, 739_907, MAX_ORDINAL] {
            let (y, m, d) = from_ordinal(n);
            assert_eq!(to_ordinal(y, m, d), n);
        }
        assert_eq!(from_ordinal(739_907), (2026, 10, 18));
    }

    #[test]
    fn iso_parsing() {
        assert_eq!(parse_iso("2026-10-18").unwrap(), (2026, 10, 18, None));
        assert_eq!(
            parse_iso("2026-10-18T12:30").unwrap().3,
            Some((12, 30, 0, 0))
        );
        assert_eq!(
            parse_iso("2026-10-18 12:30:05.25").unwrap().3,
            Some((12, 30, 5, 250_000))
        );
        assert!(parse_iso("2026-10-18T12:30:05+02:00").is_err());
        assert!(parse_iso("2026/10/18").is_err());
        assert!(parse_iso("").is_err());
    }

    #[test]
    fn delta_display_matches_python() {
        assert_eq!(PyDelta(0).to_string(), "0:00:00");
        assert_eq!(
            PyDelta(86_400_000_000 + 3_723_000_000).to_string(),
            "1 day, 1:02:03"
        );
        assert_eq!(PyDelta(-1_000_000).to_string(), "-1 day, 23:59:59");
        assert_eq!(
            PyDelta(2 * 86_400_000_000 + 5).to_string(),
            "2 days, 0:00:00.000005"
        );
    }
}
