//! The `time` module: the types of JavaScript's Temporal API (see
//! `time.sand`), implemented with `jiff` as native value types.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::platform::{Instant as Monotonic, UNIX_EPOCH};
use crate::policy::Capability;
use crate::vm::value::{NativeObject, Obj, Packing, Value, ValueType};
use crate::vm::{Ctl, NResult, NativeFn, Vm};
use jiff::civil::{Date, DateDifference, DateTime, DateTimeDifference, DateTimeRound, Time, TimeRound};
use jiff::fmt::strtime::{self, BrokenDownTime};
use jiff::fmt::temporal::{DateTimeParser, DateTimePrinter};
use jiff::tz::{self, Disambiguation, TimeZoneDatabase, TimeZoneTransition};
use jiff::{
    RoundMode, SignedDuration, SpanArithmetic, SpanRelativeTo, SpanRound, Timestamp, TimestampRound, Unit, Zoned, ZonedDifference,
    ZonedRound,
};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

static PARSER: DateTimeParser = DateTimeParser::new();
/// The compiled-in time zone database. Lookups use it explicitly: jiff's
/// global database reads the host's zone files if any crate in the build
/// enables that.
static DB: LazyLock<TimeZoneDatabase> = LazyLock::new(TimeZoneDatabase::bundled);
static PRINTER: DateTimePrinter = DateTimePrinter::new();

// ----------------------------------------------------------------------
// The value types

/// The error for text that is not in the strftime format `format`.
fn unformatted(text: &str, name: &str, format: &str) -> String {
    format!("{} in the format {:?}", unreadable(text, name), super::truncate(format, 64))
}

/// The error for text that is not the text form of type `name`.
fn unreadable(text: &str, name: &str) -> String {
    format!("cannot read {:?} as a {name}", super::truncate(text, 64))
}

/// The year, offset to be positive, then the month and the day: the bits
/// order as the dates do.
fn pack_date(d: &Date) -> u64 {
    ((i64::from(d.year()) + 0x8000) as u64) << 16 | (u64::from(d.month() as u8) << 8) | u64::from(d.day() as u8)
}

fn unpack_date(bits: u64) -> Date {
    Date::new(((bits >> 16) as i64 - 0x8000) as i16, (bits >> 8) as u8 as i8, bits as u8 as i8).expect("packed by pack_date")
}

fn pack_time(t: &Time) -> u64 {
    (u64::from(t.hour() as u8) << 48)
        | (u64::from(t.minute() as u8) << 40)
        | (u64::from(t.second() as u8) << 32)
        | u64::from(t.subsec_nanosecond() as u32)
}

fn unpack_time(bits: u64) -> Time {
    Time::new((bits >> 48) as u8 as i8, (bits >> 40) as u8 as i8, (bits >> 32) as u8 as i8, bits as u32 as i32)
        .expect("packed by pack_time")
}

impl ValueType for Timestamp {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "Instant";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    // The nanoseconds since the epoch, offset to be positive.
    const PACKING: Option<Packing<Self>> = Some(Packing {
        pack: |t| {
            let n = (t.as_nanosecond() as u128) ^ (1 << 127);
            [(n >> 64) as u64, n as u64]
        },
        unpack: |[hi, lo]| {
            let n = ((u128::from(hi) << 64 | u128::from(lo)) ^ (1 << 127)) as i128;
            Timestamp::from_nanosecond(n).expect("packed from a timestamp")
        },
        ordered: true,
    });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| unreadable(text, Self::NAME))
    }
}

impl ValueType for DateTime {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "PlainDateTime";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> = Some(Packing {
        pack: |dt| [pack_date(&dt.date()), pack_time(&dt.time())],
        unpack: |[d, t]| unpack_date(d).to_datetime(unpack_time(t)),
        ordered: true,
    });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| unreadable(text, Self::NAME))
    }
}

impl ValueType for Date {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "PlainDate";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> = Some(Packing { pack: |d| [pack_date(d), 0], unpack: |[d, _]| unpack_date(d), ordered: true });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| unreadable(text, Self::NAME))
    }
}

impl ValueType for Time {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "PlainTime";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> = Some(Packing { pack: |t| [pack_time(t), 0], unpack: |[t, _]| unpack_time(t), ordered: true });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| unreadable(text, Self::NAME))
    }
}

/// What identifies a time zone: its IANA name, else its offset.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ZoneKey<'a> {
    Iana(&'a str),
    Fixed(i32),
}

fn zone_key(tz: &tz::TimeZone) -> ZoneKey<'_> {
    match tz.iana_name() {
        Some(name) => ZoneKey::Iana(name),
        None => ZoneKey::Fixed(tz.to_fixed_offset().map_or(0, |o| o.seconds())),
    }
}

/// `time.ZonedDateTime`: an instant in a supported zone (see
/// [`TimeZone`]). Unlike [`Zoned`] values, values in different zones differ.
#[derive(Clone, Debug)]
pub struct ZonedDateTime(Zoned);

impl ZonedDateTime {
    /// `z`, if Sand supports its zone (see [`TimeZone::get`]).
    pub fn new(z: Zoned) -> Option<ZonedDateTime> {
        supported(z.time_zone()).ok()?;
        Some(ZonedDateTime(z))
    }

    pub fn zoned(&self) -> &Zoned {
        &self.0
    }
}

impl PartialEq for ZonedDateTime {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for ZonedDateTime {}
impl PartialOrd for ZonedDateTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ZonedDateTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.timestamp().cmp(&other.0.timestamp()).then_with(|| zone_key(self.0.time_zone()).cmp(&zone_key(other.0.time_zone())))
    }
}
impl Hash for ZonedDateTime {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.timestamp().hash(h);
        zone_key(self.0.time_zone()).hash(h);
    }
}
impl fmt::Display for ZonedDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl ValueType for ZonedDateTime {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "ZonedDateTime";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    fn parse(text: &str) -> Result<Self, String> {
        let z = PARSER.parse_zoned_with(&DB, text).map_err(|_| unreadable(text, Self::NAME))?;
        supported(z.time_zone())?;
        Ok(ZonedDateTime(z))
    }
}

/// Writes a year as ISO 8601 does: 4 digits, or a sign and 6 digits.
fn write_year(f: &mut fmt::Formatter<'_>, year: i16) -> fmt::Result {
    if (0..=9999).contains(&year) { write!(f, "{year:04}") } else { write!(f, "{year:+07}") }
}

/// `time.PlainYearMonth`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlainYearMonth(Date);

impl PlainYearMonth {
    /// The month `year`-`month`, if there is one.
    pub fn new(year: i16, month: i8) -> Option<PlainYearMonth> {
        Date::new(year, month, 1).ok().map(PlainYearMonth)
    }

    /// The month's first day.
    pub fn first_day(self) -> Date {
        self.0
    }
}

impl fmt::Display for PlainYearMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_year(f, self.0.year())?;
        write!(f, "-{:02}", self.0.month())
    }
}
impl ValueType for PlainYearMonth {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "PlainYearMonth";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> =
        Some(Packing { pack: |ym| [pack_date(&ym.0), 0], unpack: |[d, _]| PlainYearMonth(unpack_date(d)), ordered: true });
    /// `2024-03`, or a date in the month.
    fn parse(text: &str) -> Result<Self, String> {
        format!("{text}-01")
            .parse::<Date>()
            .or_else(|_| text.parse::<Date>())
            .map(|d| PlainYearMonth(d.first_of_month()))
            .map_err(|_| unreadable(text, Self::NAME))
    }
}

/// The year of the dates that `PlainMonthDay` values are stored as, a leap year.
const MONTH_DAY_YEAR: i16 = 1972;

/// `time.PlainMonthDay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlainMonthDay(Date);

impl PlainMonthDay {
    /// The day `month`-`day`, if there is one (February 29 included).
    pub fn new(month: i8, day: i8) -> Option<PlainMonthDay> {
        Date::new(MONTH_DAY_YEAR, month, day).ok().map(PlainMonthDay)
    }

    pub fn month(self) -> i8 {
        self.0.month()
    }

    pub fn day(self) -> i8 {
        self.0.day()
    }
}

impl fmt::Display for PlainMonthDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}-{:02}", self.0.month(), self.0.day())
    }
}
impl ValueType for PlainMonthDay {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "PlainMonthDay";
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> =
        Some(Packing { pack: |md| [pack_date(&md.0), 0], unpack: |[d, _]| PlainMonthDay(unpack_date(d)), ordered: true });
    /// `03-10`, `--03-10`, or a date on the day.
    fn parse(text: &str) -> Result<Self, String> {
        let short = text.strip_prefix("--").unwrap_or(text);
        let two_digits = |s: &str| (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i8>().ok()).flatten();
        let (month, day) = match short.split_once('-').and_then(|(m, d)| Some((two_digits(m)?, two_digits(d)?))) {
            Some(md) => md,
            None => text.parse::<Date>().map(|d| (d.month(), d.day())).map_err(|_| unreadable(text, Self::NAME))?,
        };
        PlainMonthDay::new(month, day).ok_or_else(|| format!("no such day: {month:02}-{day:02}"))
    }
}

/// `time.Span`. Its units below a second are balanced into seconds, as its
/// text form writes them, so equal spans have the same units.
#[derive(Clone, Copy, Debug)]
pub struct Span(jiff::Span);

impl Span {
    /// `s` with its milliseconds, microseconds and nanoseconds balanced into
    /// seconds; an error if the seconds are then out of range.
    pub fn new(s: jiff::Span) -> Result<Span, jiff::Error> {
        let [y, mo, w, d, h, mi, sec, ms, us, ns] = span_units(&s);
        let sub = i128::from(sec) * 1_000_000_000 + i128::from(ms) * 1_000_000 + i128::from(us) * 1000 + i128::from(ns);
        let part = |div: i128, modulo: i128| (sub / div % modulo) as i64;
        build_span([y, mo, w, d, h, mi, (sub / 1_000_000_000) as i64, part(1_000_000, 1000), part(1000, 1000), part(1, 1000)]).map(Span)
    }

    pub fn span(&self) -> jiff::Span {
        self.0
    }
}

/// Sets a unit of a span (`Span::try_years`, ...).
type SetUnit = fn(jiff::Span, i64) -> Result<jiff::Span, jiff::Error>;

/// The span with the given units, from years to nanoseconds, which must not
/// have different signs.
fn build_span(units: [i64; 10]) -> Result<jiff::Span, jiff::Error> {
    let setters: [SetUnit; 10] = [
        jiff::Span::try_years,
        jiff::Span::try_months,
        jiff::Span::try_weeks,
        jiff::Span::try_days,
        jiff::Span::try_hours,
        jiff::Span::try_minutes,
        jiff::Span::try_seconds,
        jiff::Span::try_milliseconds,
        jiff::Span::try_microseconds,
        jiff::Span::try_nanoseconds,
    ];
    units.iter().zip(setters).filter(|(u, _)| **u != 0).try_fold(jiff::Span::new(), |s, (&u, set)| set(s, u))
}

fn span_units(s: &jiff::Span) -> [i64; 10] {
    [
        s.get_years().into(),
        s.get_months().into(),
        s.get_weeks().into(),
        s.get_days().into(),
        s.get_hours().into(),
        s.get_minutes(),
        s.get_seconds(),
        s.get_milliseconds(),
        s.get_microseconds(),
        s.get_nanoseconds(),
    ]
}

impl PartialEq for Span {
    fn eq(&self, other: &Self) -> bool {
        span_units(&self.0) == span_units(&other.0)
    }
}
impl Eq for Span {}
impl Hash for Span {
    fn hash<H: Hasher>(&self, h: &mut H) {
        span_units(&self.0).hash(h);
    }
}
impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl ValueType for Span {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "Span";
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().and_then(Span::new).map_err(|_| unreadable(text, Self::NAME))
    }
}

/// `time.TimeZone`: an IANA zone or an offset in whole minutes, whose text
/// forms read back as the same zone.
#[derive(Clone, Debug)]
pub struct TimeZone(tz::TimeZone);

impl TimeZone {
    /// The zone named `name`: an IANA name (`Europe/Paris`, matched ignoring
    /// case), `UTC`, or an offset in whole minutes (`+05:30`); `None` if
    /// there is none.
    pub fn get(name: &str) -> Option<TimeZone> {
        parse_zone(name).ok().map(TimeZone)
    }

    /// The operating system's time zone, if it has an IANA name.
    #[cfg(feature = "os")]
    pub fn system() -> Option<TimeZone> {
        TimeZone::get(tz::TimeZone::try_system().ok()?.iana_name()?)
    }

    pub fn zone(&self) -> &tz::TimeZone {
        &self.0
    }
}

impl PartialEq for TimeZone {
    fn eq(&self, other: &Self) -> bool {
        zone_key(&self.0) == zone_key(&other.0)
    }
}
impl Eq for TimeZone {}
impl Hash for TimeZone {
    fn hash<H: Hasher>(&self, h: &mut H) {
        zone_key(&self.0).hash(h);
    }
}
impl fmt::Display for TimeZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match zone_key(&self.0) {
            ZoneKey::Iana(name) => f.write_str(name),
            ZoneKey::Fixed(_) => f.write_str(&PRINTER.time_zone_to_string(&self.0).unwrap_or_default()),
        }
    }
}
impl ValueType for TimeZone {
    const MODULE: &'static str = "time";
    const NAME: &'static str = "TimeZone";
    fn parse(text: &str) -> Result<Self, String> {
        parse_zone(text).map(TimeZone)
    }
}

/// The zone named `name`: an IANA name, `UTC`, or an offset in whole minutes.
fn parse_zone(name: &str) -> Result<tz::TimeZone, String> {
    let unknown = || format!("unknown time zone {:?}", super::truncate(name, 64));
    let tz = PARSER.parse_time_zone_with(&DB, name).map_err(|_| unknown())?;
    supported(&tz).map_err(|_| unknown())?;
    Ok(tz)
}

/// Accepts IANA zones and offsets in whole minutes up to 23:59 (as Temporal
/// does), whose text forms read back as the same zone, unlike POSIX rules
/// and offsets with seconds.
fn supported(tz: &tz::TimeZone) -> Result<(), String> {
    match (tz.iana_name(), tz.to_fixed_offset()) {
        (Some(_), _) => Ok(()),
        (None, Ok(offset)) if offset.seconds() % 60 == 0 && offset.seconds().abs() < 24 * 3600 => Ok(()),
        _ => Err("the time zone must be an IANA zone or an offset in whole minutes up to 23:59".into()),
    }
}

// ----------------------------------------------------------------------
// Arguments and results

fn wrong<T>(what: &str) -> Result<T, Ctl> {
    Err(Ctl::internal(format!("expected a time.{what}")))
}

fn get<T: ValueType>(vm: &Vm, v: Value) -> Result<T, Ctl> {
    match vm.heap.value::<T>(v) {
        Some(x) => Ok(x),
        None => wrong(T::NAME),
    }
}

/// The instant of an `Instant` or `ZonedDateTime`.
fn instant(vm: &Vm, v: Value) -> Result<Timestamp, Ctl> {
    if let Some(t) = vm.heap.value::<Timestamp>(v) {
        Ok(t)
    } else if let Some(z) = vm.heap.value::<ZonedDateTime>(v) {
        Ok(z.0.timestamp())
    } else {
        wrong("Instant")
    }
}

fn zoned(vm: &Vm, v: Value) -> Result<Zoned, Ctl> {
    get::<ZonedDateTime>(vm, v).map(|z| z.0)
}

fn opt_zoned(vm: &Vm, v: Value) -> Result<Option<Zoned>, Ctl> {
    vm.as_option(v).map(|z| zoned(vm, z)).transpose()
}

/// The date of a `PlainDate`, `PlainDateTime`, `ZonedDateTime`,
/// `PlainYearMonth` (its first day) or `PlainMonthDay` (in 1972).
fn date(vm: &Vm, v: Value) -> Result<Date, Ctl> {
    let heap = &vm.heap;
    if let Some(d) = heap.value::<Date>(v) {
        Ok(d)
    } else if let Some(dt) = heap.value::<DateTime>(v) {
        Ok(dt.date())
    } else if let Some(z) = heap.value::<ZonedDateTime>(v) {
        Ok(z.0.date())
    } else if let Some(ym) = heap.value::<PlainYearMonth>(v) {
        Ok(ym.0)
    } else if let Some(md) = heap.value::<PlainMonthDay>(v) {
        Ok(md.0)
    } else {
        wrong("PlainDate")
    }
}

/// The wall-clock time of a `PlainTime`, `PlainDateTime` or `ZonedDateTime`.
fn time_of(vm: &Vm, v: Value) -> Result<Time, Ctl> {
    let heap = &vm.heap;
    if let Some(t) = heap.value::<Time>(v) {
        Ok(t)
    } else if let Some(dt) = heap.value::<DateTime>(v) {
        Ok(dt.time())
    } else if let Some(z) = heap.value::<ZonedDateTime>(v) {
        Ok(z.0.time())
    } else {
        wrong("PlainTime")
    }
}

/// The date-time of a `PlainDateTime` or `ZonedDateTime`.
fn datetime(vm: &Vm, v: Value) -> Result<DateTime, Ctl> {
    if let Some(dt) = vm.heap.value::<DateTime>(v) { Ok(dt) } else { Ok(zoned(vm, v)?.datetime()) }
}

fn span(vm: &Vm, v: Value) -> Result<jiff::Span, Ctl> {
    get::<Span>(vm, v).map(|s| s.0)
}

fn zone(vm: &Vm, v: Value) -> Result<tz::TimeZone, Ctl> {
    get::<TimeZone>(vm, v).map(|z| z.0)
}

/// Declaration order of the variant `v` of an enum without payloads.
fn variant(vm: &Vm, v: Value) -> usize {
    match v {
        Value::Tag(id) => vm.prog.variants.get(id as usize).map_or(usize::MAX, |r| r.index as usize),
        _ => usize::MAX,
    }
}

/// The `time.Unit` variants, in order.
const UNITS: [Unit; 10] = [
    Unit::Year,
    Unit::Month,
    Unit::Week,
    Unit::Day,
    Unit::Hour,
    Unit::Minute,
    Unit::Second,
    Unit::Millisecond,
    Unit::Microsecond,
    Unit::Nanosecond,
];

/// The `time.RoundingMode` variants, in order.
const MODES: [RoundMode; 9] = [
    RoundMode::Ceil,
    RoundMode::Floor,
    RoundMode::Expand,
    RoundMode::Trunc,
    RoundMode::HalfCeil,
    RoundMode::HalfFloor,
    RoundMode::HalfExpand,
    RoundMode::HalfTrunc,
    RoundMode::HalfEven,
];

/// The `time.Disambiguation` variants, in order.
const DISAMBIGUATIONS: [Disambiguation; 3] = [Disambiguation::Compatible, Disambiguation::Earlier, Disambiguation::Later];

fn unit(vm: &Vm, v: Value) -> Result<Unit, Ctl> {
    UNITS.get(variant(vm, v)).copied().ok_or_else(|| Ctl::internal("expected a time.Unit"))
}

fn mode(vm: &Vm, v: Value) -> Result<RoundMode, Ctl> {
    MODES.get(variant(vm, v)).copied().ok_or_else(|| Ctl::internal("expected a time.RoundingMode"))
}

fn disambiguation(vm: &Vm, v: Value) -> Result<Disambiguation, Ctl> {
    DISAMBIGUATIONS.get(variant(vm, v)).copied().ok_or_else(|| Ctl::internal("expected a time.Disambiguation"))
}

/// The rounding options `smallest`, `increment` and `mode` at `a[i..]`.
fn rounding(vm: &Vm, a: &[Value], i: usize) -> Result<(Unit, i64, RoundMode), Ctl> {
    Ok((unit(vm, a[i])?, increment(a[i + 1])?, mode(vm, a[i + 2])?))
}

/// A rounding increment, 1 to 10⁹.
fn increment(v: Value) -> Result<i64, Ctl> {
    match v.as_int() {
        i @ 1..=1_000_000_000 => Ok(i),
        i => Err(Ctl::fail("invalid", format!("the rounding increment must be from 1 to 1000000000, not {i}"))),
    }
}

/// Fails unless `smallest` is at most hours: instants and wall-clock times
/// have no calendar units.
fn clock_unit(smallest: Unit, what: &str) -> Result<(), Ctl> {
    if smallest > Unit::Hour {
        return Err(Ctl::fail("invalid", format!("{what} can only be rounded to hours or smaller units")));
    }
    Ok(())
}

/// The difference options `largest`, `smallest`, `increment` and `mode` at
/// `a[2..]`. A `largest` smaller than `smallest` is raised to it.
fn difference(vm: &Vm, a: &[Value]) -> Result<(Unit, Unit, i64, RoundMode), Ctl> {
    let (smallest, increment, mode) = rounding(vm, a, 3)?;
    Ok((unit(vm, a[2])?.max(smallest), smallest, increment, mode))
}

/// Fails with an error of jiff: kind `"overflow"` for a result outside
/// the supported range, else `"invalid"`.
fn failure(e: jiff::Error) -> Ctl {
    if e.is_range() {
        Ctl::fail("overflow", "the result is outside the supported range (years -9999 to 9999)")
    } else {
        Ctl::fail("invalid", e)
    }
}

/// Allocates `x`, or fails (see [`failure`]).
fn made<T: ValueType>(vm: &mut Vm, x: Result<T, jiff::Error>) -> NResult {
    match x {
        Ok(x) => Ok(vm.new_value(x)),
        Err(e) => Err(failure(e)),
    }
}

/// `Ok(x)`, or `Err(Error { kind, message })`.
fn result<T: ValueType>(vm: &mut Vm, x: Result<T, String>, kind: &str) -> NResult {
    Ok(match x {
        Ok(x) => {
            let v = vm.new_value(x);
            vm.ok(v)
        }
        Err(msg) => vm.err_value(kind, &msg),
    })
}

fn int(n: impl Into<i64>) -> Value {
    Value::Int(n.into())
}

/// `d` as a `duration`, failing outside its range.
fn dur(d: SignedDuration) -> NResult {
    match i64::try_from(d.as_nanos()) {
        Ok(n) => Ok(Value::Dur(n)),
        Err(_) => Err(Ctl::fail("overflow", "the time is too long for a duration")),
    }
}

/// `x` if it fits in `T` and is in `range`.
fn small<T: TryFrom<i64> + PartialOrd>(x: i64, range: std::ops::RangeInclusive<T>) -> Option<T> {
    T::try_from(x).ok().filter(|x| range.contains(x))
}

/// The date with the given components, or `None` if there is no such date.
fn exact_date(year: i64, month: i64, day: i64) -> Option<Date> {
    Date::new(small(year, -9999..=9999)?, small(month, 1..=12)?, small(day, 1..=31)?).ok()
}

/// The time with the given components, or `None` if one is out of range.
fn exact_time([h, m, s, ms, us, ns]: [i64; 6]) -> Option<Time> {
    let subsec = |x: i64| small::<i32>(x, 0..=999);
    let nanos = subsec(ms)? * 1_000_000 + subsec(us)? * 1000 + subsec(ns)?;
    Time::new(small(h, 0..=23)?, small(m, 0..=59)?, small(s, 0..=59)?, nanos).ok()
}

/// The date with the given components, the month and day clamped to their
/// ranges; they must be positive.
fn constrain_date(year: i64, month: i64, day: i64) -> Result<Date, Ctl> {
    if month < 1 || day < 1 {
        return Err(Ctl::fail("invalid", format!("month and day must be positive: {month}, {day}")));
    }
    let Some(first) = exact_date(year, month.min(12), 1) else {
        return Err(Ctl::fail("invalid", format!("year out of range: {year}")));
    };
    let day = day.min(first.days_in_month().into());
    exact_date(year, month.min(12), day).ok_or_else(|| Ctl::internal("a clamped date is invalid"))
}

/// The time with the given components, each clamped to its range.
fn constrain_time([h, m, s, ms, us, ns]: [i64; 6]) -> Result<Time, Ctl> {
    let parts = [h.clamp(0, 23), m.clamp(0, 59), s.clamp(0, 59), ms.clamp(0, 999), us.clamp(0, 999), ns.clamp(0, 999)];
    exact_time(parts).ok_or_else(|| Ctl::internal("a clamped time is invalid"))
}

fn ints<const N: usize>(a: &[Value], from: usize) -> [i64; N] {
    std::array::from_fn(|i| a[from + i].as_int())
}

/// The start that span arithmetic measures from: `rel`, or without one, a
/// day of 24 hours (and a week of 7 days).
fn relative(rel: &Option<Zoned>) -> SpanRelativeTo<'_> {
    match rel {
        Some(z) => z.into(),
        None => SpanRelativeTo::days_are_24_hours(),
    }
}

/// Parses `a[0]` as the text form of `T`, or if the format `a[1]` is not
/// empty, with strptime and `formatted`.
fn parse_with<T: ValueType>(vm: &mut Vm, a: &[Value], formatted: fn(&BrokenDownTime) -> Result<T, jiff::Error>) -> NResult {
    let (text, format) = (vm.str(a[0]), vm.str(a[1]));
    vm.charge_bytes(text.len() + format.len())?;
    let x = if format.is_empty() {
        T::parse(text)
    } else {
        strtime::parse(format, text).and_then(|b| formatted(&b)).map_err(|_| unformatted(text, T::NAME, format))
    };
    result(vm, x, "parse")
}

/// Parses `a[0]` as the text form of `T`.
fn parse<T: ValueType>(vm: &mut Vm, a: &[Value]) -> NResult {
    let text = vm.str(a[0]);
    vm.charge_bytes(text.len())?;
    let x = T::parse(text);
    result(vm, x, "parse")
}

/// Formats `t` with the strftime format `a[1]`.
fn format(vm: &mut Vm, a: &[Value], t: impl Into<BrokenDownTime>) -> NResult {
    let format = vm.str(a[1]);
    // A directive of at least 5 bytes (`%255Y`) expands to at most 255
    // bytes, and a shorter one to at most a zone name.
    vm.reserve(format.len().saturating_mul(64))?;
    vm.charge_slow_bytes(format.len())?;
    let s = strtime::format(format, t).map_err(|e| Ctl::fail("invalid", e))?;
    vm.charge_bytes(s.len())?;
    Ok(vm.new_str(s))
}

/// The first of `transitions` that changes the offset (the database also
/// records changes of only a zone's abbreviation or daylight saving flag).
fn offset_change<'t>(
    vm: &Vm,
    tz: &tz::TimeZone,
    transitions: impl Iterator<Item = TimeZoneTransition<'t>>,
) -> Result<Option<Timestamp>, Ctl> {
    for t in transitions {
        vm.charge(1)?;
        // Transitions fall on whole seconds, so a second before is still the previous period.
        let before = t.timestamp().checked_sub(SignedDuration::from_secs(1)).map(|b| tz.to_offset(b));
        if before.is_ok_and(|o| o != t.offset()) {
            return Ok(Some(t.timestamp()));
        }
    }
    Ok(None)
}

/// Temporal rounds an instant as if the time since the epoch were positive.
fn as_if_positive(mode: RoundMode) -> RoundMode {
    match mode {
        RoundMode::Trunc => RoundMode::Floor,
        RoundMode::Expand => RoundMode::Ceil,
        RoundMode::HalfTrunc => RoundMode::HalfFloor,
        RoundMode::HalfExpand => RoundMode::HalfCeil,
        mode => mode,
    }
}

/// Fails unless `spans` can be measured: years and months need a start.
fn measurable(spans: &[jiff::Span], largest: Option<Unit>, rel: &Option<Zoned>) -> Result<(), Ctl> {
    let calendar = spans.iter().any(|s| s.get_years() != 0 || s.get_months() != 0) || matches!(largest, Some(Unit::Year | Unit::Month));
    if calendar && rel.is_none() {
        return Err(Ctl::fail("invalid", "a span with years or months needs `relative_to`"));
    }
    Ok(())
}

/// `time.Stopwatch`: when it started, by the host's monotonic clock.
struct Stopwatch(Monotonic);

impl NativeObject for Stopwatch {
    fn type_name(&self) -> &'static str {
        "Stopwatch"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The current time by the host's clock.
fn now(vm: &Vm) -> Result<Timestamp, Ctl> {
    let nanos = match vm.host.clock.wall().duration_since(UNIX_EPOCH) {
        Ok(d) => i128::try_from(d.as_nanos()),
        Err(e) => i128::try_from(e.duration().as_nanos()).map(|n| -n),
    };
    nanos.ok().and_then(|n| Timestamp::from_nanosecond(n).ok()).ok_or_else(|| Ctl::internal("the system clock is out of range"))
}

/// Floor division of the nanoseconds since the epoch by `unit` nanoseconds.
fn epoch(t: Timestamp, unit: i128) -> NResult {
    match i64::try_from(t.as_nanosecond().div_euclid(unit)) {
        Ok(n) => Ok(Value::Int(n)),
        Err(_) => Err(Ctl::fail("overflow", "the time is too far from 1970 for nanoseconds in an int")),
    }
}

// ----------------------------------------------------------------------
// Registration

/// Registers `natives` as methods `time.<ty>.<name>` for each type in `types`.
fn methods(r: &mut Registry, types: &[&str], natives: &[(&str, NativeFn)]) {
    for ty in types {
        for &(name, f) in natives {
            r.add_builtin(format!("time.{ty}.{name}"), f);
        }
    }
}

const DATED: &[&str] = &["PlainDate", "PlainDateTime", "ZonedDateTime"];
const TIMED: &[&str] = &["PlainTime", "PlainDateTime", "ZonedDateTime"];

pub fn register(r: &mut Registry) {
    r.source("time", ModKind::Std, include_str!("time.sand"));
    r.add_value_type::<Timestamp>();
    r.add_value_type::<ZonedDateTime>();
    r.add_value_type::<DateTime>();
    r.add_value_type::<Date>();
    r.add_value_type::<Time>();
    r.add_value_type::<PlainYearMonth>();
    r.add_value_type::<PlainMonthDay>();
    r.add_value_type::<Span>();
    r.add_value_type::<TimeZone>();
    register_module(r);
    register_parts(r);
    register_instant(r);
    register_zoned(r);
    register_datetime(r);
    register_date(r);
    register_time(r);
    register_year_month(r);
    register_span(r);
}

fn register_module(r: &mut Registry) {
    r.add_builtin("time.utc", |vm, _| Ok(vm.new_value(TimeZone(tz::TimeZone::UTC))));
    r.add_builtin("time.zone", |vm, a| {
        let name = vm.str(a[0]);
        vm.charge_bytes(name.len())?;
        // Loading a zone's rules the first time parses them.
        vm.charge(64)?;
        let tz = parse_zone(name).map(TimeZone);
        result(vm, tz, "not_found")
    });
    r.add_builtin("time.local_zone", |vm, _| {
        let tz = match vm.host.time_zone.clone() {
            Some(tz) if vm.permit(&Capability::TimeZone)? => tz,
            _ => tz::TimeZone::UTC,
        };
        Ok(vm.new_value(TimeZone(tz)))
    });
    r.add_builtin("time.now", |vm, _| {
        let t = now(vm)?;
        Ok(vm.new_value(t))
    });
    r.add_builtin("time.stopwatch", |vm, _| {
        let start = vm.host.clock.now();
        Ok(vm.heap.alloc(Obj::Native(Box::new(Stopwatch(start)))))
    });
    r.add_builtin("time.Stopwatch.elapsed", |vm, a| {
        let start = vm.heap.native::<Stopwatch>(a[0]).ok_or_else(|| Ctl::internal("expected a time.Stopwatch"))?.0;
        let elapsed = vm.host.clock.now().saturating_duration_since(start);
        Ok(Value::Dur(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)))
    });
    r.add_builtin("time.midnight", |vm, _| Ok(vm.new_value(Time::midnight())));
    r.add_builtin("time.duration_span", |vm, a| {
        let s = jiff::Span::try_from(SignedDuration::from_nanos(a[0].as_int())).and_then(Span::new);
        made(vm, s)
    });

    r.add_builtin("time.date", |vm, a| {
        let [y, m, d] = ints(a, 0);
        let date = exact_date(y, m, d).ok_or_else(|| format!("no such date: {y:04}-{m:02}-{d:02}"));
        result(vm, date, "invalid")
    });
    r.add_builtin("time.time", |vm, a| {
        let parts: [i64; 6] = ints(a, 0);
        let [h, m, s, ..] = parts;
        let t = exact_time(parts).ok_or_else(|| format!("no such time: {h:02}:{m:02}:{s:02}"));
        result(vm, t, "invalid")
    });
    r.add_builtin("time.datetime", |vm, a| {
        let [y, m, d, h, mi, s] = ints(a, 0);
        let dt = match (exact_date(y, m, d), exact_time(ints(a, 3))) {
            (Some(date), Some(time)) => Ok(date.to_datetime(time)),
            (None, _) => Err(format!("no such date: {y:04}-{m:02}-{d:02}")),
            (_, None) => Err(format!("no such time: {h:02}:{mi:02}:{s:02}")),
        };
        result(vm, dt, "invalid")
    });
    r.add_builtin("time.year_month", |vm, a| {
        let [y, m] = ints(a, 0);
        let ym = exact_date(y, m, 1).map(PlainYearMonth).ok_or_else(|| format!("no such month: {y:04}-{m:02}"));
        result(vm, ym, "invalid")
    });
    r.add_builtin("time.month_day", |vm, a| {
        let [m, d] = ints(a, 0);
        let md = exact_date(MONTH_DAY_YEAR.into(), m, d).map(PlainMonthDay).ok_or_else(|| format!("no such day: {m:02}-{d:02}"));
        result(vm, md, "invalid")
    });
    r.add_builtin("time.span", |vm, a| {
        let units: [i64; 10] = ints(a, 0);
        if units.iter().any(|&u| u > 0) && units.iter().any(|&u| u < 0) {
            return Err(Ctl::fail("invalid", "the units of a span must not have different signs"));
        }
        made(vm, build_span(units).and_then(Span::new))
    });
    r.add_builtin("time.TimeZone.id", |vm, a| {
        let id = get::<TimeZone>(vm, a[0])?.to_string();
        Ok(vm.new_str(id))
    });
}

/// The component methods shared by several types.
fn register_parts(r: &mut Registry) {
    methods(
        r,
        &["PlainDate", "PlainDateTime", "ZonedDateTime", "PlainYearMonth"],
        &[
            ("year", |vm, a| Ok(int(date(vm, a[0])?.year()))),
            ("month", |vm, a| Ok(int(date(vm, a[0])?.month()))),
            ("days_in_month", |vm, a| Ok(int(date(vm, a[0])?.days_in_month()))),
            ("days_in_year", |vm, a| Ok(int(date(vm, a[0])?.days_in_year()))),
            ("in_leap_year", |vm, a| Ok(Value::Bool(date(vm, a[0])?.in_leap_year()))),
        ],
    );
    methods(r, &["PlainDate", "PlainDateTime", "ZonedDateTime", "PlainMonthDay"], &[("day", |vm, a| Ok(int(date(vm, a[0])?.day())))]);
    methods(r, &["PlainMonthDay"], &[("month", |vm, a| Ok(int(date(vm, a[0])?.month())))]);
    methods(
        r,
        DATED,
        &[
            ("day_of_week", |vm, a| Ok(int(date(vm, a[0])?.weekday().to_monday_one_offset()))),
            ("day_of_year", |vm, a| Ok(int(date(vm, a[0])?.day_of_year()))),
            ("week_of_year", |vm, a| Ok(int(date(vm, a[0])?.iso_week_date().week()))),
            ("year_of_week", |vm, a| Ok(int(date(vm, a[0])?.iso_week_date().year()))),
        ],
    );
    methods(
        r,
        TIMED,
        &[
            ("hour", |vm, a| Ok(int(time_of(vm, a[0])?.hour()))),
            ("minute", |vm, a| Ok(int(time_of(vm, a[0])?.minute()))),
            ("second", |vm, a| Ok(int(time_of(vm, a[0])?.second()))),
            ("millisecond", |vm, a| Ok(int(time_of(vm, a[0])?.millisecond()))),
            ("microsecond", |vm, a| Ok(int(time_of(vm, a[0])?.microsecond()))),
            ("nanosecond", |vm, a| Ok(int(time_of(vm, a[0])?.nanosecond()))),
        ],
    );
    methods(
        r,
        &["Instant", "ZonedDateTime"],
        &[
            ("epoch_seconds", |vm, a| epoch(instant(vm, a[0])?, 1_000_000_000)),
            ("epoch_millis", |vm, a| epoch(instant(vm, a[0])?, 1_000_000)),
            ("epoch_nanos", |vm, a| epoch(instant(vm, a[0])?, 1)),
        ],
    );
    methods(
        r,
        &["ZonedDateTime", "PlainDateTime"],
        &[
            ("to_plain_date", |vm, a| {
                let d = date(vm, a[0])?;
                Ok(vm.new_value(d))
            }),
            ("to_plain_time", |vm, a| {
                let t = time_of(vm, a[0])?;
                Ok(vm.new_value(t))
            }),
        ],
    );
}

fn register_instant(r: &mut Registry) {
    r.add_builtin("time.Instant.parse", |vm, a| parse_with(vm, a, BrokenDownTime::to_timestamp));
    r.add_builtin("time.Instant.from_epoch_seconds", |vm, a| {
        let t = Timestamp::from_second(a[0].as_int());
        made(vm, t)
    });
    r.add_builtin("time.Instant.from_epoch_millis", |vm, a| {
        let t = Timestamp::from_millisecond(a[0].as_int());
        made(vm, t)
    });
    r.add_builtin("time.Instant.from_epoch_nanos", |vm, a| {
        let t = Timestamp::from_nanosecond(a[0].as_int().into());
        made(vm, t)
    });
    r.add_builtin("time.Instant.add", |vm, a| {
        let t = instant(vm, a[0])?.checked_add(SignedDuration::from_nanos(a[1].as_int()));
        made(vm, t)
    });
    r.add_builtin("time.Instant.since", |vm, a| dur(instant(vm, a[0])?.duration_since(instant(vm, a[1])?)));
    r.add_builtin("time.Instant.round", |vm, a| {
        let (smallest, increment, mode) = rounding(vm, a, 1)?;
        clock_unit(smallest, "an Instant")?;
        let t = instant(vm, a[0])?.round(TimestampRound::new().smallest(smallest).increment(increment).mode(as_if_positive(mode)));
        made(vm, t)
    });
    r.add_builtin("time.Instant.to_zoned_datetime", |vm, a| {
        let z = instant(vm, a[0])?.to_zoned(zone(vm, a[1])?);
        Ok(vm.new_value(ZonedDateTime(z)))
    });
    r.add_builtin("time.Instant.format", |vm, a| {
        let z = instant(vm, a[0])?.to_zoned(tz::TimeZone::UTC);
        format(vm, a, &z)
    });
}

fn register_zoned(r: &mut Registry) {
    r.add_builtin("time.ZonedDateTime.parse", |vm, a| {
        let (text, format) = (vm.str(a[0]), vm.str(a[1]));
        vm.charge_bytes(text.len() + format.len())?;
        let z = if format.is_empty() {
            ZonedDateTime::parse(text)
        } else {
            strtime::parse(format, text)
                .and_then(|b| b.to_zoned_with(&DB))
                .map_err(|_| unformatted(text, "ZonedDateTime", format))
                .and_then(|z| {
                    supported(z.time_zone())?;
                    Ok(ZonedDateTime(z))
                })
        };
        result(vm, z, "parse")
    });
    r.add_builtin("time.ZonedDateTime.hours_in_day", |vm, a| {
        let z = zoned(vm, a[0])?;
        let start = z.start_of_day().map_err(failure)?;
        let end = z.date().tomorrow().and_then(|d| d.to_zoned(z.time_zone().clone())).map_err(failure)?;
        Ok(Value::Float(end.timestamp().duration_since(start.timestamp()).as_secs_f64() / 3600.0))
    });
    r.add_builtin("time.ZonedDateTime.offset", |vm, a| Ok(Value::Dur(i64::from(zoned(vm, a[0])?.offset().seconds()) * 1_000_000_000)));
    r.add_builtin("time.ZonedDateTime.time_zone", |vm, a| {
        let tz = zoned(vm, a[0])?.time_zone().clone();
        Ok(vm.new_value(TimeZone(tz)))
    });
    r.add_builtin("time.ZonedDateTime.with", |vm, a| {
        let [y, m, d] = ints(a, 1);
        let (date, time) = (constrain_date(y, m, d)?, constrain_time(ints(a, 4))?);
        let z = zoned(vm, a[0])?.with().date(date).time(time).disambiguation(disambiguation(vm, a[10])?).build();
        made(vm, z.map(ZonedDateTime))
    });
    r.add_builtin("time.ZonedDateTime.with_plain_time", |vm, a| {
        let z = zoned(vm, a[0])?;
        let z = z.date().to_datetime(time_of(vm, a[1])?).to_zoned(z.time_zone().clone());
        made(vm, z.map(ZonedDateTime))
    });
    r.add_builtin("time.ZonedDateTime.with_time_zone", |vm, a| {
        let z = zoned(vm, a[0])?.with_time_zone(zone(vm, a[1])?);
        Ok(vm.new_value(ZonedDateTime(z)))
    });
    r.add_builtin("time.ZonedDateTime.add", |vm, a| {
        let z = zoned(vm, a[0])?.checked_add(span(vm, a[1])?);
        made(vm, z.map(ZonedDateTime))
    });
    fn diff(vm: &mut Vm, a: &[Value], since: bool) -> NResult {
        let (largest, smallest, increment, mode) = difference(vm, a)?;
        let (z, other) = (zoned(vm, a[0])?, zoned(vm, a[1])?);
        let options = ZonedDifference::new(&other).largest(largest).smallest(smallest).increment(increment).mode(mode);
        let s = if since { z.since(options) } else { z.until(options) };
        made(vm, s.and_then(Span::new))
    }
    r.add_builtin("time.ZonedDateTime.until", |vm, a| diff(vm, a, false));
    r.add_builtin("time.ZonedDateTime.since", |vm, a| diff(vm, a, true));
    r.add_builtin("time.ZonedDateTime.round", |vm, a| {
        let (smallest, increment, mode) = rounding(vm, a, 1)?;
        let z = zoned(vm, a[0])?.round(ZonedRound::new().smallest(smallest).increment(increment).mode(mode));
        made(vm, z.map(ZonedDateTime))
    });
    r.add_builtin("time.ZonedDateTime.start_of_day", |vm, a| {
        let z = zoned(vm, a[0])?.start_of_day();
        made(vm, z.map(ZonedDateTime))
    });
    fn transition(vm: &mut Vm, a: &[Value], next: bool) -> NResult {
        let z = zoned(vm, a[0])?;
        let tz = z.time_zone();
        let t =
            if next { offset_change(vm, tz, tz.following(z.timestamp()))? } else { offset_change(vm, tz, tz.preceding(z.timestamp()))? };
        let t = t.map(|t| vm.new_value(ZonedDateTime(t.to_zoned(tz.clone()))));
        Ok(vm.option(t))
    }
    r.add_builtin("time.ZonedDateTime.next_transition", |vm, a| transition(vm, a, true));
    r.add_builtin("time.ZonedDateTime.previous_transition", |vm, a| transition(vm, a, false));
    r.add_builtin("time.ZonedDateTime.to_instant", |vm, a| {
        let t = instant(vm, a[0])?;
        Ok(vm.new_value(t))
    });
    r.add_builtin("time.ZonedDateTime.to_plain_datetime", |vm, a| {
        let dt = datetime(vm, a[0])?;
        Ok(vm.new_value(dt))
    });
    r.add_builtin("time.ZonedDateTime.format", |vm, a| {
        let z = zoned(vm, a[0])?;
        format(vm, a, &z)
    });
}

fn register_datetime(r: &mut Registry) {
    r.add_builtin("time.PlainDateTime.parse", |vm, a| parse_with(vm, a, BrokenDownTime::to_datetime));
    r.add_builtin("time.PlainDateTime.with", |vm, a| {
        let [y, m, d] = ints(a, 1);
        let dt = constrain_date(y, m, d)?.to_datetime(constrain_time(ints(a, 4))?);
        Ok(vm.new_value(dt))
    });
    r.add_builtin("time.PlainDateTime.with_plain_time", |vm, a| {
        let dt = date(vm, a[0])?.to_datetime(time_of(vm, a[1])?);
        Ok(vm.new_value(dt))
    });
    r.add_builtin("time.PlainDateTime.add_span", |vm, a| {
        let dt = datetime(vm, a[0])?.checked_add(span(vm, a[1])?);
        made(vm, dt)
    });
    fn diff(vm: &mut Vm, a: &[Value], since: bool) -> NResult {
        let (largest, smallest, increment, mode) = difference(vm, a)?;
        let (dt, other) = (datetime(vm, a[0])?, datetime(vm, a[1])?);
        let options = DateTimeDifference::new(other).largest(largest).smallest(smallest).increment(increment).mode(mode);
        let s = if since { dt.since(options) } else { dt.until(options) };
        made(vm, s.and_then(Span::new))
    }
    r.add_builtin("time.PlainDateTime.until", |vm, a| diff(vm, a, false));
    r.add_builtin("time.PlainDateTime.since", |vm, a| diff(vm, a, true));
    r.add_builtin("time.PlainDateTime.round", |vm, a| {
        let (smallest, increment, mode) = rounding(vm, a, 1)?;
        let dt = datetime(vm, a[0])?.round(DateTimeRound::new().smallest(smallest).increment(increment).mode(mode));
        made(vm, dt)
    });
    r.add_builtin("time.PlainDateTime.to_zoned_datetime", |vm, a| {
        let z = zone(vm, a[1])?.to_ambiguous_zoned(datetime(vm, a[0])?).disambiguate(disambiguation(vm, a[2])?);
        made(vm, z.map(ZonedDateTime))
    });
    r.add_builtin("time.PlainDateTime.format", |vm, a| {
        let dt = datetime(vm, a[0])?;
        format(vm, a, dt)
    });
}

fn register_date(r: &mut Registry) {
    r.add_builtin("time.PlainDate.parse", |vm, a| parse_with(vm, a, BrokenDownTime::to_date));
    r.add_builtin("time.PlainDate.with", |vm, a| {
        let [y, m, d] = ints(a, 1);
        let d = constrain_date(y, m, d)?;
        Ok(vm.new_value(d))
    });
    r.add_builtin("time.PlainDate.add_span", |vm, a| {
        let d = date(vm, a[0])?.checked_add(span(vm, a[1])?);
        made(vm, d)
    });
    fn diff(vm: &mut Vm, a: &[Value], since: bool) -> NResult {
        let (largest, smallest, increment, mode) = difference(vm, a)?;
        let (d, other) = (date(vm, a[0])?, date(vm, a[1])?);
        let options = DateDifference::new(other).largest(largest).smallest(smallest).increment(increment).mode(mode);
        let s = if since { d.since(options) } else { d.until(options) };
        made(vm, s.and_then(Span::new))
    }
    r.add_builtin("time.PlainDate.until", |vm, a| diff(vm, a, false));
    r.add_builtin("time.PlainDate.since", |vm, a| diff(vm, a, true));
    r.add_builtin("time.PlainDate.to_plain_datetime", |vm, a| {
        let dt = date(vm, a[0])?.to_datetime(time_of(vm, a[1])?);
        Ok(vm.new_value(dt))
    });
    r.add_builtin("time.PlainDate.to_zoned_datetime", |vm, a| {
        let z = date(vm, a[0])?.to_zoned(zone(vm, a[1])?);
        made(vm, z.map(ZonedDateTime))
    });
    r.add_builtin("time.PlainDate.to_plain_year_month", |vm, a| {
        let d = date(vm, a[0])?.first_of_month();
        Ok(vm.new_value(PlainYearMonth(d)))
    });
    r.add_builtin("time.PlainDate.to_plain_month_day", |vm, a| {
        let d = date(vm, a[0])?;
        let md = exact_date(MONTH_DAY_YEAR.into(), d.month().into(), d.day().into()).ok_or_else(|| Ctl::internal("no month-day"))?;
        Ok(vm.new_value(PlainMonthDay(md)))
    });
    r.add_builtin("time.PlainDate.format", |vm, a| {
        let d = date(vm, a[0])?;
        format(vm, a, d)
    });
}

fn register_time(r: &mut Registry) {
    r.add_builtin("time.PlainTime.parse", |vm, a| parse_with(vm, a, BrokenDownTime::to_time));
    r.add_builtin("time.PlainTime.with", |vm, a| {
        let t = constrain_time(ints(a, 1))?;
        Ok(vm.new_value(t))
    });
    r.add_builtin("time.PlainTime.add_span", |vm, a| {
        let t = time_of(vm, a[0])?.wrapping_add(span(vm, a[1])?);
        Ok(vm.new_value(t))
    });
    r.add_builtin("time.PlainTime.since", |vm, a| dur(time_of(vm, a[0])?.duration_since(time_of(vm, a[1])?)));
    r.add_builtin("time.PlainTime.round", |vm, a| {
        let (smallest, increment, mode) = rounding(vm, a, 1)?;
        clock_unit(smallest, "a PlainTime")?;
        let t = time_of(vm, a[0])?.round(TimeRound::new().smallest(smallest).increment(increment).mode(mode));
        made(vm, t)
    });
    r.add_builtin("time.PlainTime.format", |vm, a| {
        let t = time_of(vm, a[0])?;
        format(vm, a, t)
    });
}

fn register_year_month(r: &mut Registry) {
    r.add_builtin("time.PlainYearMonth.parse", parse::<PlainYearMonth>);
    r.add_builtin("time.PlainYearMonth.with", |vm, a| {
        let [y, m] = ints(a, 1);
        let d = constrain_date(y, m, 1)?;
        Ok(vm.new_value(PlainYearMonth(d)))
    });
    r.add_builtin("time.PlainYearMonth.add_span", |vm, a| {
        let s = span(vm, a[1])?;
        if span_units(&s)[2..].iter().any(|&u| u != 0) {
            return Err(Ctl::fail("invalid", format!("a PlainYearMonth can only add years and months, not {s}")));
        }
        let d = date(vm, a[0])?.checked_add(s);
        made(vm, d.map(PlainYearMonth))
    });
    r.add_builtin("time.PlainYearMonth.until", |vm, a| {
        let largest = unit(vm, a[2])?;
        if !matches!(largest, Unit::Year | Unit::Month) {
            return Err(Ctl::fail("invalid", "the largest unit between months must be years or months"));
        }
        let s = date(vm, a[0])?.until(DateDifference::new(date(vm, a[1])?).largest(largest).smallest(Unit::Month));
        made(vm, s.and_then(Span::new))
    });
    r.add_builtin("time.PlainYearMonth.to_plain_date", |vm, a| {
        let d = date(vm, a[0])?;
        let d = constrain_date(d.year().into(), d.month().into(), a[1].as_int())?;
        Ok(vm.new_value(d))
    });

    r.add_builtin("time.PlainMonthDay.parse", parse::<PlainMonthDay>);
    r.add_builtin("time.PlainMonthDay.with", |vm, a| {
        let [m, d] = ints(a, 1);
        let d = constrain_date(MONTH_DAY_YEAR.into(), m, d)?;
        Ok(vm.new_value(PlainMonthDay(d)))
    });
    r.add_builtin("time.PlainMonthDay.to_plain_date", |vm, a| {
        let md = date(vm, a[0])?;
        let d = constrain_date(a[1].as_int(), md.month().into(), md.day().into())?;
        Ok(vm.new_value(d))
    });
}

fn register_span(r: &mut Registry) {
    r.add_builtin("time.Span.parse", parse::<Span>);
    methods(
        r,
        &["Span"],
        &[
            ("years", |vm, a| Ok(int(span(vm, a[0])?.get_years()))),
            ("months", |vm, a| Ok(int(span(vm, a[0])?.get_months()))),
            ("weeks", |vm, a| Ok(int(span(vm, a[0])?.get_weeks()))),
            ("days", |vm, a| Ok(int(span(vm, a[0])?.get_days()))),
            ("hours", |vm, a| Ok(int(span(vm, a[0])?.get_hours()))),
            ("minutes", |vm, a| Ok(int(span(vm, a[0])?.get_minutes()))),
            ("seconds", |vm, a| Ok(int(span(vm, a[0])?.get_seconds()))),
            ("milliseconds", |vm, a| Ok(int(span(vm, a[0])?.get_milliseconds()))),
            ("microseconds", |vm, a| Ok(int(span(vm, a[0])?.get_microseconds()))),
            ("nanoseconds", |vm, a| Ok(int(span(vm, a[0])?.get_nanoseconds()))),
            ("sign", |vm, a| Ok(int(span(vm, a[0])?.signum()))),
            ("is_zero", |vm, a| Ok(Value::Bool(span(vm, a[0])?.is_zero()))),
            ("negated", |vm, a| {
                let s = span(vm, a[0])?.negate();
                made(vm, Span::new(s))
            }),
            ("abs", |vm, a| {
                let s = span(vm, a[0])?.abs();
                made(vm, Span::new(s))
            }),
        ],
    );
    r.add_builtin("time.Span.add_span", |vm, a| {
        let (s, other, rel) = (span(vm, a[0])?, span(vm, a[1])?, opt_zoned(vm, a[2])?);
        measurable(&[s, other], None, &rel)?;
        let sum = s.checked_add(SpanArithmetic::from((other, relative(&rel))));
        made(vm, sum.and_then(Span::new))
    });
    r.add_builtin("time.Span.round", |vm, a| {
        let (smallest, increment, mode) = (unit(vm, a[1])?, increment(a[3])?, mode(vm, a[4])?);
        let largest = vm.as_option(a[2]).map(|u| unit(vm, u)).transpose()?;
        let (s, rel) = (span(vm, a[0])?, opt_zoned(vm, a[5])?);
        measurable(&[s], largest.or(Some(smallest)), &rel)?;
        let mut options = SpanRound::new().smallest(smallest).increment(increment).mode(mode).relative(relative(&rel));
        if let Some(largest) = largest {
            options = options.largest(largest);
        }
        made(vm, s.round(options).and_then(Span::new))
    });
    r.add_builtin("time.Span.total", |vm, a| {
        let (s, unit, rel) = (span(vm, a[0])?, unit(vm, a[1])?, opt_zoned(vm, a[2])?);
        measurable(&[s], Some(unit), &rel)?;
        s.total((unit, relative(&rel))).map(Value::Float).map_err(failure)
    });
    r.add_builtin("time.Span.compare", |vm, a| {
        let (s, other, rel) = (span(vm, a[0])?, span(vm, a[1])?, opt_zoned(vm, a[2])?);
        if span_units(&s) == span_units(&other) {
            return Ok(int(0));
        }
        measurable(&[s, other], None, &rel)?;
        let o = s.compare((other, relative(&rel))).map_err(failure)?;
        Ok(int(o as i8))
    });
    r.add_builtin("time.Span.to_duration", |vm, a| {
        let (s, rel) = (span(vm, a[0])?, opt_zoned(vm, a[1])?);
        measurable(&[s], None, &rel)?;
        dur(s.to_duration(relative(&rel)).map_err(failure)?)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every type's text form reads back as the same value.
    fn round_trip<T: ValueType + fmt::Debug>(text: &str) {
        let x = T::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(x.to_string(), text);
        assert_eq!(T::parse(&x.to_string()).ok(), Some(x));
    }

    #[test]
    fn text_forms_round_trip() {
        round_trip::<Timestamp>("-009999-01-02T01:59:59Z");
        round_trip::<ZonedDateTime>("2024-11-03T01:30:00-05:00[America/New_York]");
        round_trip::<ZonedDateTime>("2024-01-01T00:00:00+05:30[+05:30]");
        round_trip::<DateTime>("2024-02-29T13:05:09.000000001");
        round_trip::<Date>("-000001-12-31");
        round_trip::<Time>("23:59:59.5");
        round_trip::<PlainYearMonth>("2024-02");
        round_trip::<PlainYearMonth>("-000001-12");
        round_trip::<PlainMonthDay>("02-29");
        round_trip::<Span>("-P1Y2M3W4DT5H6M7.008009001S");
        round_trip::<TimeZone>("Europe/Paris");
        round_trip::<TimeZone>("+05:30");
        assert_eq!(PlainMonthDay::parse("--07-04").ok(), PlainMonthDay::parse("2023-07-04").ok());
        assert!(PlainMonthDay::parse("02-30").is_err());
        assert!(PlainMonthDay::parse("+1-05").is_err());
    }

    /// Every value survives being packed.
    fn packs<T: ValueType + fmt::Debug>(xs: &[T]) {
        let p = T::PACKING.expect("a packed type");
        for x in xs {
            assert_eq!(&(p.unpack)((p.pack)(x)), x);
        }
    }

    #[test]
    fn packed_values_unpack_equal() {
        let d = |y, m, d| Date::new(y, m, d).unwrap();
        let t = |h, m, s, ns| Time::new(h, m, s, ns).unwrap();
        packs(&[
            Timestamp::MIN,
            Timestamp::MAX,
            Timestamp::UNIX_EPOCH,
            Timestamp::new(-1, -1).unwrap(),
            Timestamp::new(1, 999_999_999).unwrap(),
        ]);
        packs(&[d(-9999, 1, 1), d(9999, 12, 31), d(1970, 1, 1), d(-1, 2, 28)]);
        packs(&[t(0, 0, 0, 0), t(23, 59, 59, 999_999_999)]);
        packs(&[DateTime::MIN, DateTime::MAX, d(2024, 2, 29).to_datetime(t(13, 5, 9, 1))]);
        packs(&[PlainYearMonth::new(-9999, 1).unwrap(), PlainYearMonth::new(9999, 12).unwrap()]);
        packs(&[PlainMonthDay::new(2, 29).unwrap(), PlainMonthDay::new(12, 31).unwrap()]);
    }

    #[test]
    fn spans_balance_units_below_a_second() {
        let spans = [
            jiff::Span::new().milliseconds(1500),
            jiff::Span::new().seconds(1).milliseconds(500),
            jiff::Span::new().seconds(1).microseconds(500_000),
            jiff::Span::new().nanoseconds(1_500_000_000),
        ];
        for s in spans {
            let s = Span::new(s).unwrap();
            assert_eq!(s, Span::new(spans[0]).unwrap());
            assert_eq!((s.0.get_seconds(), s.0.get_milliseconds()), (1, 500));
            assert_eq!(s.to_string(), "PT1.5S");
            assert_eq!(Span::parse(&s.to_string()), Ok(s));
        }
    }

    #[test]
    fn only_zones_with_text_that_reads_back_are_accepted() {
        for name in ["America/New_York", "UTC", "+05:30", "-00:01"] {
            assert!(parse_zone(name).is_ok(), "{name}");
        }
        for name in ["+05:30:15", "EST5EDT,M3.2.0,M11.1.0", "Mars/Olympus_Mons", ""] {
            assert!(parse_zone(name).is_err(), "{name}");
        }
    }

    #[test]
    fn zones_compare_by_identity_of_the_zone() {
        let at = |zone: &str| ZonedDateTime(Timestamp::UNIX_EPOCH.to_zoned(parse_zone(zone).unwrap()));
        assert_eq!(at("America/New_York"), at("america/new_york"));
        assert_eq!(at("+05:30"), at("+05:30"));
        assert_ne!(at("UTC"), at("Etc/UTC"));
        assert_ne!(at("Asia/Kolkata"), at("+05:30"));
        assert!(at("Asia/Kolkata") < at("+05:30") || at("Asia/Kolkata") > at("+05:30"));
    }

    #[test]
    fn constrained_components() {
        assert_eq!(constrain_date(2023, 2, 31).ok(), Some(jiff::civil::date(2023, 2, 28)));
        assert_eq!(constrain_date(2024, 14, 1).ok(), Some(jiff::civil::date(2024, 12, 1)));
        assert!(constrain_date(2024, 0, 1).is_err());
        assert!(constrain_date(10_000, 1, 1).is_err());
        assert_eq!(constrain_time([25, 61, -1, 1000, 5, 6]).ok(), Some(jiff::civil::time(23, 59, 0, 999_005_006)));
        assert_eq!(exact_time([23, 59, 59, 999, 999, 999]), Some(jiff::civil::time(23, 59, 59, 999_999_999)));
        assert_eq!(exact_time([24, 0, 0, 0, 0, 0]), None);
        assert_eq!(exact_time([0, 0, 0, 1000, 0, 0]), None);
    }
}
