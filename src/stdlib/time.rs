//! The `time` module: calendar arithmetic (proleptic Gregorian, UTC) and clocks.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::platform::{Instant, SystemTime, UNIX_EPOCH};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, NResult, Vm};
use std::any::Any;
use std::fmt::Write;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;

const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// The opaque `time.Instant` type.
struct InstantObj(Instant);

impl NativeObject for InstantObj {
    fn type_name(&self) -> &'static str {
        "Instant"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `(year, month, day)` of a day number (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Broken-down UTC time.
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
    /// Days since the epoch.
    days: i64,
}

impl Civil {
    fn from_unix_nanos(n: i64) -> Civil {
        let secs = n.div_euclid(NANOS_PER_SEC);
        let days = secs.div_euclid(SECS_PER_DAY);
        let sod = secs.rem_euclid(SECS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Civil { year, month, day, hour: sod / 3600, minute: sod / 60 % 60, second: sod % 60, nanos: n.rem_euclid(NANOS_PER_SEC), days }
    }

    /// Monday = 1 ... Sunday = 7 (1970-01-01 was a Thursday).
    fn weekday(&self) -> i64 {
        (self.days + 3).rem_euclid(7) + 1
    }

    fn day_of_year(&self) -> i64 {
        self.days - days_from_civil(self.year, 1, 1) + 1
    }
}

/// Validates the components and converts them to Unix nanoseconds;
/// `offset` (seconds east of UTC) is subtracted.
fn to_unix_nanos((y, mo, d): (i64, i64, i64), (h, mi, s): (i64, i64, i64), nanos: i64, offset: i64) -> Result<i64, String> {
    if !(1..=12).contains(&mo) {
        return Err(format!("month out of range: {mo}"));
    }
    // Years far outside the representable range would overflow the day count.
    if !(-1_000_000..=1_000_000).contains(&y) || !(1..=days_in_month(y, mo)).contains(&d) {
        return Err(format!("invalid date: {y:04}-{mo:02}-{d:02}"));
    }
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..60).contains(&s) {
        return Err(format!("invalid time: {h:02}:{mi:02}:{s:02}"));
    }
    let secs = days_from_civil(y, mo, d) * SECS_PER_DAY + h * 3600 + mi * 60 + s - offset;
    i64::try_from(i128::from(secs) * i128::from(NANOS_PER_SEC) + i128::from(nanos))
        .map_err(|_| format!("date out of range (years 1677 to 2262 are supported): {y:04}-{mo:02}-{d:02}"))
}

/// Parses RFC 3339 / ISO 8601 extended format into Unix nanoseconds.
fn parse_iso(text: &str) -> Result<i64, String> {
    let err = || format!("invalid date-time: {:?}", super::truncate(text, 64));
    let mut p = Cursor { s: text.as_bytes(), i: 0 };
    let year = p.digits(4).ok_or_else(err)?;
    p.expect(b'-').ok_or_else(err)?;
    let month = p.digits(2).ok_or_else(err)?;
    p.expect(b'-').ok_or_else(err)?;
    let day = p.digits(2).ok_or_else(err)?;
    let (mut hour, mut minute, mut second, mut nanos, mut offset) = (0, 0, 0, 0, 0);
    if !p.at_end() {
        if !matches!(p.next(), Some(b'T' | b't' | b' ')) {
            return Err(err());
        }
        hour = p.digits(2).ok_or_else(err)?;
        p.expect(b':').ok_or_else(err)?;
        minute = p.digits(2).ok_or_else(err)?;
        if p.eat(b':') {
            second = p.digits(2).ok_or_else(err)?;
            if p.eat(b'.') || p.eat(b',') {
                let start = p.i;
                let mut scale = NANOS_PER_SEC;
                while let Some(d) = p.digit() {
                    // Digits beyond nanosecond precision are truncated.
                    scale /= 10;
                    nanos += d * scale;
                }
                if p.i == start {
                    return Err(err());
                }
            }
        }
        match p.next() {
            None => {}
            Some(b'Z' | b'z') => {}
            Some(sign @ (b'+' | b'-')) => {
                let oh = p.digits(2).ok_or_else(err)?;
                p.eat(b':');
                let om = p.digits(2).ok_or_else(err)?;
                if oh > 23 || om > 59 {
                    return Err(err());
                }
                offset = (oh * 3600 + om * 60) * if sign == b'-' { -1 } else { 1 };
            }
            Some(_) => return Err(err()),
        }
        if !p.at_end() {
            return Err(err());
        }
    }
    to_unix_nanos((year, month, day), (hour, minute, second), nanos, offset)
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }
    fn next(&mut self) -> Option<u8> {
        let b = *self.s.get(self.i)?;
        self.i += 1;
        Some(b)
    }
    fn eat(&mut self, b: u8) -> bool {
        let hit = self.s.get(self.i) == Some(&b);
        self.i += usize::from(hit);
        hit
    }
    fn expect(&mut self, b: u8) -> Option<()> {
        self.eat(b).then_some(())
    }
    fn digit(&mut self) -> Option<i64> {
        let d = self.s.get(self.i).filter(|b| b.is_ascii_digit())?;
        self.i += 1;
        Some(i64::from(d - b'0'))
    }
    /// Exactly `n` decimal digits.
    fn digits(&mut self, n: usize) -> Option<i64> {
        (0..n).try_fold(0, |acc, _| Some(acc * 10 + self.digit()?))
    }
}

/// `.123`-style fraction without trailing zeros, or `""` for zero.
fn fraction(nanos: i64) -> String {
    if nanos == 0 {
        return String::new();
    }
    let digits = format!("{nanos:09}");
    format!(".{}", digits.trim_end_matches('0'))
}

fn to_iso(n: i64) -> String {
    let c = Civil::from_unix_nanos(n);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}Z", c.year, c.month, c.day, c.hour, c.minute, c.second, fraction(c.nanos))
}

fn format(n: i64, fmt: &str) -> String {
    let c = Civil::from_unix_nanos(n);
    let mut out = String::with_capacity(fmt.len() + 16);
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(spec) = chars.next() else {
            out.push('%');
            break;
        };
        let _ = match spec {
            'Y' => write!(out, "{:04}", c.year),
            'm' => write!(out, "{:02}", c.month),
            'd' => write!(out, "{:02}", c.day),
            'e' => write!(out, "{:>2}", c.day),
            'H' => write!(out, "{:02}", c.hour),
            'M' => write!(out, "{:02}", c.minute),
            'S' => write!(out, "{:02}", c.second),
            'f' => write!(out, "{:09}", c.nanos),
            '3' | '6' | '9' if chars.peek() == Some(&'f') => {
                chars.next();
                let digits = spec.to_digit(10).unwrap_or(9);
                write!(out, "{:0width$}", c.nanos / 10i64.pow(9 - digits), width = digits as usize)
            }
            'j' => write!(out, "{:03}", c.day_of_year()),
            'a' => write!(out, "{}", &WEEKDAYS[c.weekday() as usize - 1][..3]),
            'A' => write!(out, "{}", WEEKDAYS[c.weekday() as usize - 1]),
            'b' => write!(out, "{}", &MONTHS[c.month as usize - 1][..3]),
            'B' => write!(out, "{}", MONTHS[c.month as usize - 1]),
            'y' => write!(out, "{:02}", c.year.rem_euclid(100)),
            'Z' => write!(out, "UTC"),
            'z' => write!(out, "+0000"),
            's' => write!(out, "{}", n.div_euclid(NANOS_PER_SEC)),
            '%' => write!(out, "%"),
            other => write!(out, "%{other}"),
        };
    }
    out
}

fn duration_between(later: Instant, earlier: Instant) -> Value {
    let nanos = match later.checked_duration_since(earlier) {
        Some(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        None => -i64::try_from(earlier.duration_since(later).as_nanos()).unwrap_or(i64::MAX),
    };
    Value::Dur(nanos)
}

fn instant(vm: &Vm, v: Value) -> Result<Instant, Ctl> {
    vm.heap.native::<InstantObj>(v).map(|i| i.0).ok_or_else(|| Ctl::Panic("not an Instant".into()))
}

/// The `unix_nanos` field of a `DateTime`.
fn unix_nanos(vm: &Vm, v: Value) -> i64 {
    vm.heap.fields(v)[0].as_int()
}

fn nanos_result(vm: &mut Vm, r: Result<i64, String>, kind: &str) -> NResult {
    Ok(match r {
        Ok(n) => vm.ok(Value::Int(n)),
        Err(msg) => vm.err_value(kind, &msg),
    })
}

pub fn register(r: &mut Registry) {
    r.source("time", ModKind::Std, include_str!("time.sand"));

    macro_rules! component {
        ($($name:literal => $f:expr),* $(,)?) => {
            $(r.add_builtin(concat!("time.DateTime.", $name), |vm, a| {
                let c = Civil::from_unix_nanos(unix_nanos(vm, a[0]));
                Ok(Value::Int($f(&c)))
            });)*
        };
    }
    component! {
        "year" => |c: &Civil| c.year,
        "month" => |c: &Civil| c.month,
        "day" => |c: &Civil| c.day,
        "hour" => |c: &Civil| c.hour,
        "minute" => |c: &Civil| c.minute,
        "second" => |c: &Civil| c.second,
        "nanosecond" => |c: &Civil| c.nanos,
        "weekday" => Civil::weekday,
        "day_of_year" => Civil::day_of_year,
    }
    r.add_builtin("time.DateTime.to_iso", |vm, a| {
        let s = to_iso(unix_nanos(vm, a[0]));
        Ok(vm.new_str(s))
    });
    r.add_builtin("time.DateTime.format", |vm, a| {
        let fmt = vm.str(a[1]);
        // Every directive expands to at most 16 bytes.
        vm.reserve(fmt.len().saturating_mul(16))?;
        vm.charge_slow_bytes(fmt.len())?;
        let s = format(unix_nanos(vm, a[0]), fmt);
        Ok(vm.new_str(s))
    });

    r.add_builtin("time.now_nanos", |_, _| {
        let now = SystemTime::now();
        let nanos = match now.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
        };
        Ok(Value::Int(nanos))
    });
    r.add_builtin("time.monotonic", |vm, _| Ok(vm.heap.alloc(Obj::Native(Box::new(InstantObj(Instant::now()))))));
    r.add_builtin("time.Instant.elapsed", |vm, a| Ok(duration_between(Instant::now(), instant(vm, a[0])?)));
    r.add_builtin("time.Instant.since", |vm, a| Ok(duration_between(instant(vm, a[0])?, instant(vm, a[1])?)));

    r.add_builtin("time.parse_nanos", |vm, a| {
        vm.charge_bytes(vm.str(a[0]).len())?;
        let n = parse_iso(vm.str(a[0]));
        nanos_result(vm, n, "parse")
    });
    r.add_builtin("time.date_nanos", |vm, a| {
        let [y, mo, d, h, mi, s] = [0, 1, 2, 3, 4, 5].map(|i| a[i].as_int());
        nanos_result(vm, to_unix_nanos((y, mo, d), (h, mi, s), 0, 0), "invalid")
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in -800_000..800_000 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
            assert!((1..=days_in_month(y, m)).contains(&d));
        }
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_iso("1969-12-31T23:59:59.999999999Z"), Ok(-1));
        assert_eq!(parse_iso("1970-01-01T01:00:00+01:00"), Ok(0));
        assert_eq!(parse_iso("1970-01-01T00:00:00-0130"), Ok(5_400 * NANOS_PER_SEC));
        assert_eq!(parse_iso("1970-01-01T00:00"), Ok(0));
        assert!(parse_iso("1970-01-01T").is_err());
        assert!(parse_iso("1970-01-01T00:00:00.Z").is_err());
        assert!(parse_iso("1970-1-01").is_err());
        assert!(parse_iso("2300-01-01").is_err());
        assert_eq!(to_iso(-1), "1969-12-31T23:59:59.999999999Z");
        assert_eq!(format(0, "%e|%y|%q|%"), " 1|70|%q|%");
    }
}
