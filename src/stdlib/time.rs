//! Implementation of the `time` standard library module.

use crate::check::ModKind;
use crate::native::{Binding, HostError, IntoValue, NamedType, NativeModule, NativeObject, Val};
use crate::platform::{Instant as Monotonic, UNIX_EPOCH};
use crate::policy::Capability;
use crate::vm::value::{Packing, ValueType};
use crate::vm::{Ctl, Vm};
use jiff::civil::{Date, DateDifference, DateTime, DateTimeDifference, DateTimeRound, Time, TimeRound};
use jiff::fmt::strtime::{self, BrokenDownTime};
use jiff::fmt::temporal::{DateTimeParser, DateTimePrinter};
use jiff::tz::{self, TimeZoneDatabase, TimeZoneTransition};
use jiff::{
    RoundMode, SignedDuration, SpanArithmetic, SpanRelativeTo, SpanRound, Timestamp, TimestampRound, Unit, Zoned, ZonedDifference,
    ZonedRound,
};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

static PARSER: DateTimeParser = DateTimeParser::new();
/// Bundled IANA database. Used explicitly so the engine never reads host zone files.
static DB: LazyLock<TimeZoneDatabase> = LazyLock::new(TimeZoneDatabase::bundled);
static PRINTER: DateTimePrinter = DateTimePrinter::new();

fn format_parse_error(text: &str, name: &str, format: &str) -> String {
    format!("{} in the format {:?}", parse_error(text, name), super::truncate(format, 64))
}

fn parse_error(text: &str, name: &str) -> String {
    format!("cannot read {:?} as a {name}", super::truncate(text, 64))
}

/// Packs a date into a `u64` whose unsigned bit order matches chronological order.
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

macro_rules! bind_jiff_types {
    ($($rust:ty => $name:literal: $doc:literal),* $(,)?) => {$(
        impl NamedType for $rust {
            const MODULE: &'static str = "time";
            const NAME: &'static str = $name;
            const DOC: &'static str = $doc;
            const PUBLIC: bool = true;
            fn binding() -> Binding<Self> {
                Binding::of_value()
            }
        }
        crate::__value_type_impls!($rust);
    )*};
}

bind_jiff_types! {
    Timestamp => "Instant": "An exact point on the UTC timeline (such as `2024-03-10T07:30:00Z`).",
    DateTime => "PlainDateTime": "A calendar date and wall-clock time without a time zone (such as `2024-03-10T03:30:00`).",
    Date => "PlainDate": "A calendar date without a time zone (such as `2024-03-10`).",
    Time => "PlainTime": "A wall-clock time of day without a date or time zone (such as `03:30:00`).",
}

impl ValueType for Timestamp {
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    // Flip the sign bit so unsigned comparison matches chronological order.
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
        text.parse().map_err(|_| parse_error(text, Self::NAME))
    }
}

impl ValueType for DateTime {
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> = Some(Packing {
        pack: |dt| [pack_date(&dt.date()), pack_time(&dt.time())],
        unpack: |[d, t]| unpack_date(d).to_datetime(unpack_time(t)),
        ordered: true,
    });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| parse_error(text, Self::NAME))
    }
}

impl ValueType for Date {
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> = Some(Packing { pack: |d| [pack_date(d), 0], unpack: |[d, _]| unpack_date(d), ordered: true });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| parse_error(text, Self::NAME))
    }
}

impl ValueType for Time {
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> = Some(Packing { pack: |t| [pack_time(t), 0], unpack: |[t, _]| unpack_time(t), ordered: true });
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().map_err(|_| parse_error(text, Self::NAME))
    }
}

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

/// An exact instant paired with a `TimeZone` (such as
/// `2024-03-10T03:30:00-04:00[America/New_York]`), ordered by instant and then
/// by zone ID.
#[derive(Clone, Debug, crate::SandValue)]
#[sand(module = "time")]
pub struct ZonedDateTime(Zoned);

impl ZonedDateTime {
    /// Wraps `z` using the bundled IANA rules for its zone name.
    /// Returns `None` if the time zone is not supported by [`TimeZone`].
    pub fn new(z: Zoned) -> Option<ZonedDateTime> {
        let tz = match z.time_zone().iana_name() {
            Some(name) => DB.get(name).ok()?,
            None => {
                check_supported_zone(z.time_zone()).ok()?;
                z.time_zone().clone()
            }
        };
        Some(ZonedDateTime(z.timestamp().to_zoned(tz)))
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
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    fn parse(text: &str) -> Result<Self, String> {
        let z = PARSER.parse_zoned_with(&DB, text).map_err(|_| parse_error(text, Self::NAME))?;
        check_supported_zone(z.time_zone())?;
        Ok(ZonedDateTime(z))
    }
}

fn write_year(f: &mut fmt::Formatter<'_>, year: i16) -> fmt::Result {
    if (0..=9999).contains(&year) { write!(f, "{year:04}") } else { write!(f, "{year:+07}") }
}

/// A calendar year and month without a day (such as `2024-03`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, crate::SandValue)]
#[sand(module = "time")]
pub struct PlainYearMonth(Date);

impl PlainYearMonth {
    pub fn new(year: i16, month: i8) -> Option<PlainYearMonth> {
        Date::new(year, month, 1).ok().map(PlainYearMonth)
    }

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
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> =
        Some(Packing { pack: |ym| [pack_date(&ym.0), 0], unpack: |[d, _]| PlainYearMonth(unpack_date(d)), ordered: true });
    fn parse(text: &str) -> Result<Self, String> {
        format!("{text}-01")
            .parse::<Date>()
            .or_else(|_| text.parse::<Date>())
            .map(|d| PlainYearMonth(d.first_of_month()))
            .map_err(|_| parse_error(text, Self::NAME))
    }
}

/// Reference leap year used to store `PlainMonthDay` as a `Date`.
const MONTH_DAY_YEAR: i16 = 1972;

/// A calendar month and day without a year (such as `03-10`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, crate::SandValue)]
#[sand(module = "time")]
pub struct PlainMonthDay(Date);

impl PlainMonthDay {
    /// Constructs a month-day (including February 29), or returns `None` if out of range.
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
    const ORDER: Option<fn(&Self, &Self) -> Ordering> = Some(Ord::cmp);
    const PACKING: Option<Packing<Self>> =
        Some(Packing { pack: |md| [pack_date(&md.0), 0], unpack: |[d, _]| PlainMonthDay(unpack_date(d)), ordered: true });
    fn parse(text: &str) -> Result<Self, String> {
        let short = text.strip_prefix("--").unwrap_or(text);
        let two_digits = |s: &str| (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i8>().ok()).flatten();
        let (month, day) = match short.split_once('-').and_then(|(m, d)| Some((two_digits(m)?, two_digits(d)?))) {
            Some(md) => md,
            None => text.parse::<Date>().map(|d| (d.month(), d.day())).map_err(|_| parse_error(text, Self::NAME))?,
        };
        PlainMonthDay::new(month, day).ok_or_else(|| format!("no such day: {month:02}-{day:02}"))
    }
}

/// A signed calendar and clock duration (such as `P1M2DT3H`). `span`
/// normalizes sub-second units into seconds (`span(milliseconds: 1500)` stores
/// 1 second and 500 milliseconds). Two spans are equal only if all unit fields match
/// (`span(hours: 24) != span(days: 1)`).
#[derive(Clone, Copy, Debug, crate::SandValue)]
#[sand(module = "time")]
pub struct Span(jiff::Span);

impl Span {
    /// Normalizes sub-second units of `s` into seconds so that equal spans have identical unit fields.
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

type SetUnit = fn(jiff::Span, i64) -> Result<jiff::Span, jiff::Error>;

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
    fn parse(text: &str) -> Result<Self, String> {
        text.parse().and_then(Span::new).map_err(|_| parse_error(text, Self::NAME))
    }
}

/// A time zone identifier: an IANA zone (`America/New_York`), `UTC`, or a fixed
/// minute offset (`+05:30`). Two zones are equal if their canonical IDs match
/// (`zone("Etc/UTC") != UTC`).
#[derive(Clone, Debug, crate::SandValue)]
#[sand(module = "time")]
pub struct TimeZone(tz::TimeZone);

impl TimeZone {
    /// Looks up a time zone by IANA name (case-insensitive), `UTC`, or whole-minute offset (`+05:30`).
    pub fn get(name: &str) -> Option<TimeZone> {
        parse_zone(name).ok().map(TimeZone)
    }

    /// Returns the operating system's IANA time zone, if available.
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
    fn parse(text: &str) -> Result<Self, String> {
        parse_zone(text).map(TimeZone)
    }
}

fn parse_zone(name: &str) -> Result<tz::TimeZone, String> {
    let unknown = || format!("unknown time zone {:?}", super::truncate(name, 64));
    let tz = PARSER.parse_time_zone_with(&DB, name).map_err(|_| unknown())?;
    check_supported_zone(&tz).map_err(|_| unknown())?;
    Ok(tz)
}

/// Rejects POSIX TZ strings and sub-minute offsets because their formatted output cannot be parsed back.
fn check_supported_zone(tz: &tz::TimeZone) -> Result<(), String> {
    match (tz.iana_name(), tz.to_fixed_offset()) {
        (Some(_), _) => Ok(()),
        (None, Ok(offset)) if offset.seconds() % 60 == 0 && offset.seconds().abs() < 24 * 3600 => Ok(()),
        _ => Err("the time zone must be an IANA zone or an offset in whole minutes up to 23:59".into()),
    }
}

trait HasDate {
    fn date_part(&self) -> Date;
}

impl HasDate for Date {
    fn date_part(&self) -> Date {
        *self
    }
}
impl HasDate for DateTime {
    fn date_part(&self) -> Date {
        self.date()
    }
}
impl HasDate for ZonedDateTime {
    fn date_part(&self) -> Date {
        self.0.date()
    }
}
impl HasDate for PlainYearMonth {
    fn date_part(&self) -> Date {
        self.0
    }
}
impl HasDate for PlainMonthDay {
    fn date_part(&self) -> Date {
        self.0
    }
}

trait HasTime {
    fn time_part(&self) -> Time;
}

impl HasTime for Time {
    fn time_part(&self) -> Time {
        *self
    }
}
impl HasTime for DateTime {
    fn time_part(&self) -> Time {
        self.time()
    }
}
impl HasTime for ZonedDateTime {
    fn time_part(&self) -> Time {
        self.0.time()
    }
}

type DifferenceOptions = (Unit, Unit, i64, RoundMode);

trait Difference: ValueType {
    fn difference(&self, other: &Self, since: bool, options: DifferenceOptions) -> Result<jiff::Span, jiff::Error>;
}

impl Difference for ZonedDateTime {
    fn difference(
        &self,
        other: &Self,
        since: bool,
        (largest, smallest, increment, mode): DifferenceOptions,
    ) -> Result<jiff::Span, jiff::Error> {
        let options = ZonedDifference::new(&other.0).largest(largest).smallest(smallest).increment(increment).mode(mode);
        if since { self.0.since(options) } else { self.0.until(options) }
    }
}
impl Difference for DateTime {
    fn difference(
        &self,
        other: &Self,
        since: bool,
        (largest, smallest, increment, mode): DifferenceOptions,
    ) -> Result<jiff::Span, jiff::Error> {
        let options = DateTimeDifference::new(*other).largest(largest).smallest(smallest).increment(increment).mode(mode);
        if since { self.since(options) } else { self.until(options) }
    }
}
impl Difference for Date {
    fn difference(
        &self,
        other: &Self,
        since: bool,
        (largest, smallest, increment, mode): DifferenceOptions,
    ) -> Result<jiff::Span, jiff::Error> {
        let options = DateDifference::new(*other).largest(largest).smallest(smallest).increment(increment).mode(mode);
        if since { self.since(options) } else { self.until(options) }
    }
}

pub mod enums {
    /// A calendar or clock unit, ordered from largest to smallest.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, crate::SandEnum)]
    #[sand(module = "time")]
    pub enum Unit {
        /// Calendar year.
        Year,
        /// Calendar month.
        Month,
        /// Calendar week.
        Week,
        /// Calendar day.
        Day,
        /// Clock hour.
        Hour,
        /// Clock minute.
        Minute,
        /// Clock second.
        Second,
        /// Millisecond (`10^-3` seconds).
        Millisecond,
        /// Microsecond (`10^-6` seconds).
        Microsecond,
        /// Nanosecond (`10^-9` seconds).
        Nanosecond,
    }

    /// Rounding mode for `round`, `until`, and `since`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, crate::SandEnum)]
    #[sand(module = "time")]
    pub enum RoundingMode {
        /// Rounds toward positive infinity.
        Ceil,
        /// Rounds toward negative infinity.
        Floor,
        /// Rounds away from zero.
        Expand,
        /// Rounds toward zero.
        Trunc,
        /// Rounds to the nearest multiple, breaking ties toward positive infinity.
        HalfCeil,
        /// Rounds to the nearest multiple, breaking ties toward negative infinity.
        HalfFloor,
        /// Rounds to the nearest multiple, breaking ties away from zero.
        HalfExpand,
        /// Rounds to the nearest multiple, breaking ties toward zero.
        HalfTrunc,
        /// Rounds to the nearest multiple, breaking ties toward the even multiple.
        HalfEven,
    }

    /// Strategy for resolving wall-clock times that fall in a daylight saving gap
    /// (skipped time) or fold (repeated time).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, crate::SandEnum)]
    #[sand(module = "time")]
    pub enum Disambiguation {
        /// Advances by the gap length in a gap, and selects the earlier occurrence in a fold.
        Compatible,
        /// Selects the earlier instant (moving backward by the gap length in a gap).
        Earlier,
        /// Selects the later instant (moving forward by the gap length in a gap).
        Later,
    }

    impl From<Unit> for jiff::Unit {
        fn from(u: Unit) -> Self {
            match u {
                Unit::Year => jiff::Unit::Year,
                Unit::Month => jiff::Unit::Month,
                Unit::Week => jiff::Unit::Week,
                Unit::Day => jiff::Unit::Day,
                Unit::Hour => jiff::Unit::Hour,
                Unit::Minute => jiff::Unit::Minute,
                Unit::Second => jiff::Unit::Second,
                Unit::Millisecond => jiff::Unit::Millisecond,
                Unit::Microsecond => jiff::Unit::Microsecond,
                Unit::Nanosecond => jiff::Unit::Nanosecond,
            }
        }
    }

    impl From<jiff::Unit> for Unit {
        fn from(u: jiff::Unit) -> Self {
            match u {
                jiff::Unit::Year => Unit::Year,
                jiff::Unit::Month => Unit::Month,
                jiff::Unit::Week => Unit::Week,
                jiff::Unit::Day => Unit::Day,
                jiff::Unit::Hour => Unit::Hour,
                jiff::Unit::Minute => Unit::Minute,
                jiff::Unit::Second => Unit::Second,
                jiff::Unit::Millisecond => Unit::Millisecond,
                jiff::Unit::Microsecond => Unit::Microsecond,
                jiff::Unit::Nanosecond => Unit::Nanosecond,
            }
        }
    }

    impl From<RoundingMode> for jiff::RoundMode {
        fn from(m: RoundingMode) -> Self {
            match m {
                RoundingMode::Ceil => jiff::RoundMode::Ceil,
                RoundingMode::Floor => jiff::RoundMode::Floor,
                RoundingMode::Expand => jiff::RoundMode::Expand,
                RoundingMode::Trunc => jiff::RoundMode::Trunc,
                RoundingMode::HalfCeil => jiff::RoundMode::HalfCeil,
                RoundingMode::HalfFloor => jiff::RoundMode::HalfFloor,
                RoundingMode::HalfExpand => jiff::RoundMode::HalfExpand,
                RoundingMode::HalfTrunc => jiff::RoundMode::HalfTrunc,
                RoundingMode::HalfEven => jiff::RoundMode::HalfEven,
            }
        }
    }

    impl From<Disambiguation> for jiff::tz::Disambiguation {
        fn from(d: Disambiguation) -> Self {
            match d {
                Disambiguation::Compatible => jiff::tz::Disambiguation::Compatible,
                Disambiguation::Earlier => jiff::tz::Disambiguation::Earlier,
                Disambiguation::Later => jiff::tz::Disambiguation::Later,
            }
        }
    }
}

fn rounding_options(smallest: enums::Unit, increment: i64, mode: enums::RoundingMode) -> Result<(Unit, i64, RoundMode), Ctl> {
    Ok((smallest.into(), parse_increment(increment)?, mode.into()))
}

fn parse_increment(increment: i64) -> Result<i64, Ctl> {
    match increment {
        i @ 1..=1_000_000_000 => Ok(i),
        i => Err(Ctl::fail("invalid", format!("the rounding increment must be from 1 to 1000000000, not {i}"))),
    }
}

fn check_clock_unit(smallest: Unit, what: &str) -> Result<(), Ctl> {
    if smallest > Unit::Hour {
        return Err(Ctl::fail("invalid", format!("{what} can only be rounded to hours or smaller units")));
    }
    Ok(())
}

fn difference_options(
    largest: enums::Unit,
    smallest: enums::Unit,
    increment: i64,
    mode: enums::RoundingMode,
) -> Result<DifferenceOptions, Ctl> {
    let (smallest, increment, mode) = rounding_options(smallest, increment, mode)?;
    Ok((Unit::from(largest).max(smallest), smallest, increment, mode))
}

fn jiff_error(e: jiff::Error) -> Ctl {
    if e.is_range() {
        Ctl::fail("overflow", "the result is outside the supported range (years -9999 to 9999)")
    } else {
        Ctl::fail("invalid", e)
    }
}

fn in_range<T: TryFrom<i64> + PartialOrd>(x: i64, range: std::ops::RangeInclusive<T>) -> Option<T> {
    T::try_from(x).ok().filter(|x| range.contains(x))
}

fn exact_date(year: i64, month: i64, day: i64) -> Option<Date> {
    Date::new(in_range(year, -9999..=9999)?, in_range(month, 1..=12)?, in_range(day, 1..=31)?).ok()
}

fn exact_time([h, m, s, ms, us, ns]: [i64; 6]) -> Option<Time> {
    let subsec = |x: i64| in_range::<i32>(x, 0..=999);
    let nanos = subsec(ms)? * 1_000_000 + subsec(us)? * 1000 + subsec(ns)?;
    Time::new(in_range(h, 0..=23)?, in_range(m, 0..=59)?, in_range(s, 0..=59)?, nanos).ok()
}

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

fn constrain_time([h, m, s, ms, us, ns]: [i64; 6]) -> Result<Time, Ctl> {
    let parts = [h.clamp(0, 23), m.clamp(0, 59), s.clamp(0, 59), ms.clamp(0, 999), us.clamp(0, 999), ns.clamp(0, 999)];
    exact_time(parts).ok_or_else(|| Ctl::internal("a clamped time is invalid"))
}

fn span_relative_to(rel: &Option<ZonedDateTime>) -> SpanRelativeTo<'_> {
    match rel {
        Some(z) => (&z.0).into(),
        None => SpanRelativeTo::days_are_24_hours(),
    }
}

fn parse_with<T: ValueType>(
    vm: &mut Vm,
    text: Val<String>,
    format: Val<String>,
    formatted: fn(&BrokenDownTime) -> Result<T, jiff::Error>,
) -> Result<T, HostError> {
    let (text, format) = (text.str(&vm.heap), format.str(&vm.heap));
    vm.charge_bytes(text.len() + format.len())?;
    let x = if format.is_empty() {
        T::parse(text)
    } else {
        strtime::parse(format, text).and_then(|b| formatted(&b)).map_err(|_| format_parse_error(text, T::NAME, format))
    };
    x.map_err(|e| HostError::new("parse", e))
}

fn parse<T: ValueType>(vm: &mut Vm, text: Val<String>) -> Result<T, HostError> {
    let text = text.str(&vm.heap);
    vm.charge_bytes(text.len())?;
    let x = T::parse(text);
    x.map_err(|e| HostError::new("parse", e))
}

fn format(vm: &mut Vm, format: Val<String>, t: impl Into<BrokenDownTime>) -> Result<String, Ctl> {
    let format = format.str(&vm.heap);
    // Bound output size before formatting.
    vm.check_memory(format.len().saturating_mul(64))?;
    vm.charge_slow_bytes(format.len())?;
    strtime::format(format, t).map_err(|e| Ctl::fail("invalid", e))
}

/// Returns the first transition that changes the UTC offset (ignoring abbreviation-only transitions).
fn find_offset_transition<'t>(
    vm: &Vm,
    tz: &tz::TimeZone,
    transitions: impl Iterator<Item = TimeZoneTransition<'t>>,
) -> Result<Option<Timestamp>, Ctl> {
    for t in transitions {
        vm.charge(1)?;
        let before = t.timestamp().checked_sub(SignedDuration::from_secs(1)).map(|b| tz.to_offset(b));
        if before.is_ok_and(|o| o != t.offset()) {
            return Ok(Some(t.timestamp()));
        }
    }
    Ok(None)
}

/// Maps signed rounding modes (`Trunc`, `Expand`) to directional modes (`Floor`, `Ceil`) so
/// negative pre-epoch timestamps round in the same direction as positive timestamps.
fn positive_epoch_round_mode(mode: RoundMode) -> RoundMode {
    match mode {
        RoundMode::Trunc => RoundMode::Floor,
        RoundMode::Expand => RoundMode::Ceil,
        RoundMode::HalfTrunc => RoundMode::HalfFloor,
        RoundMode::HalfExpand => RoundMode::HalfCeil,
        mode => mode,
    }
}

fn require_relative_to(spans: &[jiff::Span], largest: Option<Unit>, rel: &Option<ZonedDateTime>) -> Result<(), Ctl> {
    let calendar = spans.iter().any(|s| s.get_years() != 0 || s.get_months() != 0) || matches!(largest, Some(Unit::Year | Unit::Month));
    if calendar && rel.is_none() {
        return Err(Ctl::fail("invalid", "a span with years or months needs `relative_to`"));
    }
    Ok(())
}

/// A monotonic timer for measuring elapsed time.
#[derive(crate::SandObject)]
#[sand(module = "time")]
struct Stopwatch(Monotonic);

impl NativeObject for Stopwatch {}

fn now(vm: &Vm) -> Result<Timestamp, Ctl> {
    let nanos = match vm.host.clock.wall().duration_since(UNIX_EPOCH) {
        Ok(d) => i128::try_from(d.as_nanos()),
        Err(e) => i128::try_from(e.duration().as_nanos()).map(|n| -n),
    };
    nanos.ok().and_then(|n| Timestamp::from_nanosecond(n).ok()).ok_or_else(|| Ctl::internal("the system clock is out of range"))
}

fn epoch_div(t: Timestamp, unit: i128) -> Result<i64, Ctl> {
    i64::try_from(t.as_nanosecond().div_euclid(unit))
        .map_err(|_| Ctl::fail("overflow", "the time is too far from 1970 for nanoseconds in an int"))
}

type Part<P, R> = (&'static str, fn(P) -> R);

fn methods<T: ValueType, P: 'static, R: IntoValue + 'static>(m: &mut NativeModule, part: fn(&T) -> P, natives: &[Part<P, R>]) {
    for &(name, f) in natives {
        m.function(&format!("{}.{name}", T::NAME), move |x: T| f(part(&x)));
    }
}

const YEAR_MONTH: &[Part<Date, i64>] = &[
    ("year", |d| d.year().into()),
    ("month", |d| d.month().into()),
    ("days_in_month", |d| d.days_in_month().into()),
    ("days_in_year", |d| d.days_in_year().into()),
];
const LEAP_YEAR: &[Part<Date, bool>] = &[("in_leap_year", Date::in_leap_year)];
const MONTH: &[Part<Date, i64>] = &[("month", |d| d.month().into())];
const DAY: &[Part<Date, i64>] = &[("day", |d| d.day().into())];
const WEEK: &[Part<Date, i64>] = &[
    ("day_of_week", |d| d.weekday().to_monday_one_offset().into()),
    ("day_of_year", |d| d.day_of_year().into()),
    ("week_of_year", |d| d.iso_week_date().week().into()),
    ("year_of_week", |d| d.iso_week_date().year().into()),
];
const CLOCK: &[Part<Time, i64>] = &[
    ("hour", |t| t.hour().into()),
    ("minute", |t| t.minute().into()),
    ("second", |t| t.second().into()),
    ("millisecond", |t| t.millisecond().into()),
    ("microsecond", |t| t.microsecond().into()),
    ("nanosecond", |t| t.nanosecond().into()),
];
const TO_PLAIN_DATE: &[Part<Date, Date>] = &[("to_plain_date", |d| d)];
const TO_PLAIN_TIME: &[Part<Time, Time>] = &[("to_plain_time", |t| t)];
const SPAN_UNITS: &[Part<jiff::Span, i64>] = &[
    ("years", |s| s.get_years().into()),
    ("months", |s| s.get_months().into()),
    ("weeks", |s| s.get_weeks().into()),
    ("days", |s| s.get_days().into()),
    ("hours", |s| s.get_hours().into()),
    ("minutes", |s| s.get_minutes()),
    ("seconds", |s| s.get_seconds()),
    ("milliseconds", |s| s.get_milliseconds()),
    ("microseconds", |s| s.get_microseconds()),
    ("nanoseconds", |s| s.get_nanoseconds()),
    ("sign", |s| s.signum().into()),
];
const SPAN_IS_ZERO: &[Part<jiff::Span, bool>] = &[("is_zero", |s| s.is_zero())];

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("time", ModKind::Std, include_str!("time.sand"));
    m.bind::<enums::Unit>();
    m.bind::<enums::RoundingMode>();
    m.bind::<enums::Disambiguation>();
    m.bind::<Timestamp>();
    m.bind::<ZonedDateTime>();
    m.bind::<DateTime>();
    m.bind::<Date>();
    m.bind::<Time>();
    m.bind::<PlainYearMonth>();
    m.bind::<PlainMonthDay>();
    m.bind::<Span>();
    m.bind::<TimeZone>();
    m.bind::<Stopwatch>();
    register_module(&mut m);
    register_parts(&mut m);
    register_instant(&mut m);
    register_zoned(&mut m);
    register_datetime(&mut m);
    register_date(&mut m);
    register_time(&mut m);
    register_year_month(&mut m);
    register_span(&mut m);
    m
}

fn register_module(m: &mut NativeModule) {
    m.function("utc", || TimeZone(tz::TimeZone::UTC));
    m.vm_function("TimeZone.init", |vm: &mut Vm, name: Val<String>| {
        let name = name.str(&vm.heap);
        vm.charge_bytes(name.len())?;
        vm.charge(64)?;
        let tz = parse_zone(name).map(TimeZone);
        tz.map_err(|e| HostError::new("not_found", e))
    });
    m.vm_function("local_zone", |vm: &mut Vm| -> Result<_, Ctl> {
        let tz = match vm.host.time_zone.clone() {
            Some(tz) if vm.permit(&Capability::TimeZone)? => tz,
            _ => tz::TimeZone::UTC,
        };
        Ok(TimeZone(tz))
    });
    m.vm_function("now", |vm: &mut Vm| now(vm));
    m.vm_function("Stopwatch.init", |vm: &mut Vm| {
        let start = vm.host.clock.now();
        vm.new_object(Stopwatch(start))
    });
    m.vm_function("Stopwatch.elapsed", |vm: &mut Vm, watch: Val<Stopwatch>| -> Result<_, Ctl> {
        let start = watch.get(&vm.heap)?.0;
        let elapsed = vm.host.clock.now().saturating_duration_since(start);
        Ok(SignedDuration::from_nanos(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)))
    });
    m.function("midnight", Time::midnight);
    m.function("duration_span", |d: SignedDuration| jiff::Span::try_from(d).and_then(Span::new).map_err(jiff_error));

    m.function("PlainDate.init", |y: i64, m: i64, d: i64| {
        let date = exact_date(y, m, d).ok_or_else(|| format!("no such date: {y:04}-{m:02}-{d:02}"));
        date.map_err(|e| HostError::new("invalid", e))
    });
    m.function("PlainTime.init", |h: i64, m: i64, s: i64, ms: i64, us: i64, ns: i64| {
        let t = exact_time([h, m, s, ms, us, ns]).ok_or_else(|| format!("no such time: {h:02}:{m:02}:{s:02}"));
        t.map_err(|e| HostError::new("invalid", e))
    });
    m.function("PlainDateTime.init", |y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64, ms: i64, us: i64, ns: i64| {
        let dt = match (exact_date(y, m, d), exact_time([h, mi, s, ms, us, ns])) {
            (Some(date), Some(time)) => Ok(date.to_datetime(time)),
            (None, _) => Err(format!("no such date: {y:04}-{m:02}-{d:02}")),
            (_, None) => Err(format!("no such time: {h:02}:{mi:02}:{s:02}")),
        };
        dt.map_err(|e| HostError::new("invalid", e))
    });
    m.function("PlainYearMonth.init", |y: i64, m: i64| {
        let ym = exact_date(y, m, 1).map(PlainYearMonth).ok_or_else(|| format!("no such month: {y:04}-{m:02}"));
        ym.map_err(|e| HostError::new("invalid", e))
    });
    m.function("PlainMonthDay.init", |m: i64, d: i64| {
        let md = exact_date(MONTH_DAY_YEAR.into(), m, d).map(PlainMonthDay).ok_or_else(|| format!("no such day: {m:02}-{d:02}"));
        md.map_err(|e| HostError::new("invalid", e))
    });
    m.function("Span.init", |y: i64, mo: i64, w: i64, d: i64, h: i64, mi: i64, s: i64, ms: i64, us: i64, ns: i64| {
        let units = [y, mo, w, d, h, mi, s, ms, us, ns];
        if units.iter().any(|&u| u > 0) && units.iter().any(|&u| u < 0) {
            return Err(Ctl::fail("invalid", "the units of a span must not have different signs"));
        }
        build_span(units).and_then(Span::new).map_err(jiff_error)
    });
    m.function("TimeZone.id", |z: TimeZone| z.to_string());
}

fn register_parts(m: &mut NativeModule) {
    macro_rules! parts {
        ($has:ident::$part:ident: $($t:ty),+ => $natives:expr) => {$(methods(m, <$t as $has>::$part, $natives);)+};
    }
    parts!(HasDate::date_part: Date, DateTime, ZonedDateTime, PlainYearMonth => YEAR_MONTH);
    parts!(HasDate::date_part: Date, DateTime, ZonedDateTime, PlainYearMonth => LEAP_YEAR);
    parts!(HasDate::date_part: Date, DateTime, ZonedDateTime, PlainMonthDay => DAY);
    parts!(HasDate::date_part: PlainMonthDay => MONTH);
    parts!(HasDate::date_part: Date, DateTime, ZonedDateTime => WEEK);
    parts!(HasTime::time_part: Time, DateTime, ZonedDateTime => CLOCK);
    parts!(HasDate::date_part: ZonedDateTime, DateTime => TO_PLAIN_DATE);
    parts!(HasTime::time_part: ZonedDateTime, DateTime => TO_PLAIN_TIME);
    for (name, unit) in [("epoch_seconds", 1_000_000_000), ("epoch_millis", 1_000_000), ("epoch_nanos", 1)] {
        m.function(&format!("Instant.{name}"), move |t: Timestamp| epoch_div(t, unit));
        m.function(&format!("ZonedDateTime.{name}"), move |z: ZonedDateTime| epoch_div(z.0.timestamp(), unit));
    }
}

fn register_difference<T: Difference>(m: &mut NativeModule) {
    for (name, since) in [("until", false), ("since", true)] {
        m.function(
            &format!("{}.{name}", T::NAME),
            move |x: T, other: T, largest: enums::Unit, smallest: enums::Unit, increment: i64, mode: enums::RoundingMode| {
                let options = difference_options(largest, smallest, increment, mode)?;
                x.difference(&other, since, options).and_then(Span::new).map_err(jiff_error)
            },
        );
    }
}

fn register_instant(m: &mut NativeModule) {
    m.vm_function("Instant.parse", |vm: &mut Vm, text: Val<String>, format: Val<String>| {
        parse_with(vm, text, format, BrokenDownTime::to_timestamp)
    });
    m.function("Instant.from_epoch_seconds", |s: i64| Timestamp::from_second(s).map_err(jiff_error));
    m.function("Instant.from_epoch_millis", |ms: i64| Timestamp::from_millisecond(ms).map_err(jiff_error));
    m.function("Instant.from_epoch_nanos", |ns: i64| Timestamp::from_nanosecond(ns.into()).map_err(jiff_error));
    m.function("Instant.add", |t: Timestamp, d: SignedDuration| t.checked_add(d).map_err(jiff_error));
    m.function("Instant.since", |t: Timestamp, other: Timestamp| t.duration_since(other));
    m.function("Instant.round", |t: Timestamp, smallest: enums::Unit, increment: i64, mode: enums::RoundingMode| {
        let (smallest, increment, mode) = rounding_options(smallest, increment, mode)?;
        check_clock_unit(smallest, "an Instant")?;
        t.round(TimestampRound::new().smallest(smallest).increment(increment).mode(positive_epoch_round_mode(mode))).map_err(jiff_error)
    });
    m.function("Instant.to_zoned_datetime", |t: Timestamp, zone: TimeZone| ZonedDateTime(t.to_zoned(zone.0)));
    m.vm_function("Instant.format", |vm: &mut Vm, t: Timestamp, pattern: Val<String>| {
        let z = t.to_zoned(tz::TimeZone::UTC);
        format(vm, pattern, &z)
    });
}

fn register_zoned(m: &mut NativeModule) {
    m.vm_function("ZonedDateTime.parse", |vm: &mut Vm, text: Val<String>, format: Val<String>| {
        let (text, format) = (text.str(&vm.heap), format.str(&vm.heap));
        vm.charge_bytes(text.len() + format.len())?;
        let z = if format.is_empty() {
            ZonedDateTime::parse(text)
        } else {
            strtime::parse(format, text)
                .and_then(|b| b.to_zoned_with(&DB))
                .map_err(|_| format_parse_error(text, "ZonedDateTime", format))
                .and_then(|z| {
                    check_supported_zone(z.time_zone())?;
                    Ok(ZonedDateTime(z))
                })
        };
        z.map_err(|e| HostError::new("parse", e))
    });
    m.function("ZonedDateTime.hours_in_day", |z: ZonedDateTime| -> Result<_, Ctl> {
        let z = z.0;
        let start = z.start_of_day().map_err(jiff_error)?;
        let end = z.date().tomorrow().and_then(|d| d.to_zoned(z.time_zone().clone())).map_err(jiff_error)?;
        Ok(end.timestamp().duration_since(start.timestamp()).as_secs_f64() / 3600.0)
    });
    m.function("ZonedDateTime.offset", |z: ZonedDateTime| SignedDuration::from_secs(z.0.offset().seconds().into()));
    m.function("ZonedDateTime.time_zone", |z: ZonedDateTime| TimeZone(z.0.time_zone().clone()));
    m.function(
        "ZonedDateTime.with",
        |z: ZonedDateTime, y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64, ms: i64, us: i64, ns: i64, dis: enums::Disambiguation| {
            let (date, time) = (constrain_date(y, m, d)?, constrain_time([h, mi, s, ms, us, ns])?);
            let z = z.0.with().date(date).time(time).disambiguation(dis.into()).build();
            z.map(ZonedDateTime).map_err(jiff_error)
        },
    );
    m.function("ZonedDateTime.with_plain_time", |z: ZonedDateTime, t: Time| {
        let z = z.0.date().to_datetime(t).to_zoned(z.0.time_zone().clone());
        z.map(ZonedDateTime).map_err(jiff_error)
    });
    m.function("ZonedDateTime.with_time_zone", |z: ZonedDateTime, zone: TimeZone| ZonedDateTime(z.0.with_time_zone(zone.0)));
    m.function("ZonedDateTime.add", |z: ZonedDateTime, s: Span| z.0.checked_add(s.0).map(ZonedDateTime).map_err(jiff_error));
    register_difference::<ZonedDateTime>(m);
    m.function("ZonedDateTime.round", |z: ZonedDateTime, smallest: enums::Unit, increment: i64, mode: enums::RoundingMode| {
        let (smallest, increment, mode) = rounding_options(smallest, increment, mode)?;
        let z = z.0.round(ZonedRound::new().smallest(smallest).increment(increment).mode(mode));
        z.map(ZonedDateTime).map_err(jiff_error)
    });
    m.function("ZonedDateTime.start_of_day", |z: ZonedDateTime| z.0.start_of_day().map(ZonedDateTime).map_err(jiff_error));
    fn transition(vm: &mut Vm, z: ZonedDateTime, next: bool) -> Result<Option<ZonedDateTime>, Ctl> {
        let tz = z.0.time_zone();
        let t = if next {
            find_offset_transition(vm, tz, tz.following(z.0.timestamp()))?
        } else {
            find_offset_transition(vm, tz, tz.preceding(z.0.timestamp()))?
        };
        Ok(t.map(|t| ZonedDateTime(t.to_zoned(tz.clone()))))
    }
    m.vm_function("ZonedDateTime.next_transition", |vm: &mut Vm, z: ZonedDateTime| transition(vm, z, true));
    m.vm_function("ZonedDateTime.previous_transition", |vm: &mut Vm, z: ZonedDateTime| transition(vm, z, false));
    m.function("ZonedDateTime.to_instant", |z: ZonedDateTime| z.0.timestamp());
    m.function("ZonedDateTime.to_plain_datetime", |z: ZonedDateTime| z.0.datetime());
    m.vm_function("ZonedDateTime.format", |vm: &mut Vm, z: ZonedDateTime, pattern: Val<String>| format(vm, pattern, &z.0));
}

fn register_datetime(m: &mut NativeModule) {
    m.vm_function("PlainDateTime.parse", |vm: &mut Vm, text: Val<String>, format: Val<String>| {
        parse_with(vm, text, format, BrokenDownTime::to_datetime)
    });
    m.function(
        "PlainDateTime.with",
        |_: DateTime, y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64, ms: i64, us: i64, ns: i64| -> Result<_, Ctl> {
            Ok(constrain_date(y, m, d)?.to_datetime(constrain_time([h, mi, s, ms, us, ns])?))
        },
    );
    m.function("PlainDateTime.with_plain_time", |dt: DateTime, t: Time| dt.date().to_datetime(t));
    m.function("PlainDateTime.add_span", |dt: DateTime, s: Span| dt.checked_add(s.0).map_err(jiff_error));
    register_difference::<DateTime>(m);
    m.function("PlainDateTime.round", |dt: DateTime, smallest: enums::Unit, increment: i64, mode: enums::RoundingMode| {
        let (smallest, increment, mode) = rounding_options(smallest, increment, mode)?;
        dt.round(DateTimeRound::new().smallest(smallest).increment(increment).mode(mode)).map_err(jiff_error)
    });
    m.function("PlainDateTime.to_zoned_datetime", |dt: DateTime, zone: TimeZone, dis: enums::Disambiguation| {
        let z = zone.0.to_ambiguous_zoned(dt).disambiguate(dis.into());
        z.map(ZonedDateTime).map_err(jiff_error)
    });
    m.vm_function("PlainDateTime.format", |vm: &mut Vm, dt: DateTime, pattern: Val<String>| format(vm, pattern, dt));
}

fn register_date(m: &mut NativeModule) {
    m.vm_function("PlainDate.parse", |vm: &mut Vm, text: Val<String>, format: Val<String>| {
        parse_with(vm, text, format, BrokenDownTime::to_date)
    });
    m.function("PlainDate.with", |_: Date, y: i64, m: i64, d: i64| constrain_date(y, m, d));
    m.function("PlainDate.add_span", |d: Date, s: Span| d.checked_add(s.0).map_err(jiff_error));
    register_difference::<Date>(m);
    m.function("PlainDate.to_plain_datetime", |d: Date, t: Time| d.to_datetime(t));
    m.function("PlainDate.to_zoned_datetime", |d: Date, zone: TimeZone| d.to_zoned(zone.0).map(ZonedDateTime).map_err(jiff_error));
    m.function("PlainDate.to_plain_year_month", |d: Date| PlainYearMonth(d.first_of_month()));
    m.function("PlainDate.to_plain_month_day", |d: Date| {
        PlainMonthDay::new(d.month(), d.day()).ok_or_else(|| Ctl::internal("no month-day"))
    });
    m.vm_function("PlainDate.format", |vm: &mut Vm, d: Date, pattern: Val<String>| format(vm, pattern, d));
}

fn register_time(m: &mut NativeModule) {
    m.vm_function("PlainTime.parse", |vm: &mut Vm, text: Val<String>, format: Val<String>| {
        parse_with(vm, text, format, BrokenDownTime::to_time)
    });
    m.function("PlainTime.with", |_: Time, h: i64, m: i64, s: i64, ms: i64, us: i64, ns: i64| constrain_time([h, m, s, ms, us, ns]));
    m.function("PlainTime.add_span", |t: Time, s: Span| t.wrapping_add(s.0));
    m.function("PlainTime.since", |t: Time, other: Time| t.duration_since(other));
    m.function("PlainTime.round", |t: Time, smallest: enums::Unit, increment: i64, mode: enums::RoundingMode| {
        let (smallest, increment, mode) = rounding_options(smallest, increment, mode)?;
        check_clock_unit(smallest, "a PlainTime")?;
        t.round(TimeRound::new().smallest(smallest).increment(increment).mode(mode)).map_err(jiff_error)
    });
    m.vm_function("PlainTime.format", |vm: &mut Vm, t: Time, pattern: Val<String>| format(vm, pattern, t));
}

fn register_year_month(m: &mut NativeModule) {
    m.vm_function("PlainYearMonth.parse", parse::<PlainYearMonth>);
    m.function("PlainYearMonth.with", |_: PlainYearMonth, y: i64, m: i64| -> Result<_, Ctl> {
        Ok(PlainYearMonth(constrain_date(y, m, 1)?))
    });
    m.function("PlainYearMonth.add_span", |ym: PlainYearMonth, s: Span| {
        let s = s.0;
        if span_units(&s)[2..].iter().any(|&u| u != 0) {
            return Err(Ctl::fail("invalid", format!("a PlainYearMonth can only add years and months, not {s}")));
        }
        ym.0.checked_add(s).map(PlainYearMonth).map_err(jiff_error)
    });
    m.function("PlainYearMonth.until", |ym: PlainYearMonth, other: PlainYearMonth, largest: enums::Unit| {
        let largest = Unit::from(largest);
        if !matches!(largest, Unit::Year | Unit::Month) {
            return Err(Ctl::fail("invalid", "the largest unit between months must be years or months"));
        }
        let s = ym.0.until(DateDifference::new(other.0).largest(largest).smallest(Unit::Month));
        s.and_then(Span::new).map_err(jiff_error)
    });
    m.function("PlainYearMonth.to_plain_date", |ym: PlainYearMonth, day: i64| constrain_date(ym.0.year().into(), ym.0.month().into(), day));

    m.vm_function("PlainMonthDay.parse", parse::<PlainMonthDay>);
    m.function("PlainMonthDay.with", |_: PlainMonthDay, m: i64, d: i64| -> Result<_, Ctl> {
        Ok(PlainMonthDay(constrain_date(MONTH_DAY_YEAR.into(), m, d)?))
    });
    m.function("PlainMonthDay.to_plain_date", |md: PlainMonthDay, year: i64| constrain_date(year, md.month().into(), md.day().into()));
}

fn register_span(m: &mut NativeModule) {
    m.vm_function("Span.parse", parse::<Span>);
    methods(m, Span::span, SPAN_UNITS);
    methods(m, Span::span, SPAN_IS_ZERO);
    m.function("Span.negated", |s: Span| Span::new(s.0.negate()).map_err(jiff_error));
    m.function("Span.abs", |s: Span| Span::new(s.0.abs()).map_err(jiff_error));
    m.function("Span.add_span", |s: Span, other: Span, rel: Option<ZonedDateTime>| {
        let (s, other) = (s.0, other.0);
        require_relative_to(&[s, other], None, &rel)?;
        let sum = s.checked_add(SpanArithmetic::from((other, span_relative_to(&rel))));
        sum.and_then(Span::new).map_err(jiff_error)
    });
    m.function(
        "Span.round",
        |s: Span,
         smallest: enums::Unit,
         largest: Option<enums::Unit>,
         increment: i64,
         mode: enums::RoundingMode,
         rel: Option<ZonedDateTime>| {
            let (smallest, increment, mode) = rounding_options(smallest, increment, mode)?;
            let largest = largest.map(Unit::from);
            require_relative_to(&[s.0], largest.or(Some(smallest)), &rel)?;
            let mut options = SpanRound::new().smallest(smallest).increment(increment).mode(mode).relative(span_relative_to(&rel));
            if let Some(largest) = largest {
                options = options.largest(largest);
            }
            s.0.round(options).and_then(Span::new).map_err(jiff_error)
        },
    );
    m.function("Span.total", |s: Span, unit: enums::Unit, rel: Option<ZonedDateTime>| {
        let unit = Unit::from(unit);
        require_relative_to(&[s.0], Some(unit), &rel)?;
        s.0.total((unit, span_relative_to(&rel))).map_err(jiff_error)
    });
    m.function("Span.compare", |s: Span, other: Span, rel: Option<ZonedDateTime>| -> Result<_, Ctl> {
        if span_units(&s.0) == span_units(&other.0) {
            return Ok(0);
        }
        require_relative_to(&[s.0, other.0], None, &rel)?;
        let o = s.0.compare((other.0, span_relative_to(&rel))).map_err(jiff_error)?;
        Ok(i64::from(o as i8))
    });
    m.function("Span.to_duration", |s: Span, rel: Option<ZonedDateTime>| {
        require_relative_to(&[s.0], None, &rel)?;
        s.0.to_duration(span_relative_to(&rel)).map_err(jiff_error)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn assert_pack_round_trip<T: ValueType + fmt::Debug>(xs: &[T]) {
        let p = T::PACKING.expect("a packed type");
        for x in xs {
            assert_eq!(&(p.unpack)((p.pack)(x)), x);
        }
    }

    #[test]
    fn packed_values_unpack_equal() {
        let d = |y, m, d| Date::new(y, m, d).unwrap();
        let t = |h, m, s, ns| Time::new(h, m, s, ns).unwrap();
        assert_pack_round_trip(&[
            Timestamp::MIN,
            Timestamp::MAX,
            Timestamp::UNIX_EPOCH,
            Timestamp::new(-1, -1).unwrap(),
            Timestamp::new(1, 999_999_999).unwrap(),
        ]);
        assert_pack_round_trip(&[d(-9999, 1, 1), d(9999, 12, 31), d(1970, 1, 1), d(-1, 2, 28)]);
        assert_pack_round_trip(&[t(0, 0, 0, 0), t(23, 59, 59, 999_999_999)]);
        assert_pack_round_trip(&[DateTime::MIN, DateTime::MAX, d(2024, 2, 29).to_datetime(t(13, 5, 9, 1))]);
        assert_pack_round_trip(&[PlainYearMonth::new(-9999, 1).unwrap(), PlainYearMonth::new(9999, 12).unwrap()]);
        assert_pack_round_trip(&[PlainMonthDay::new(2, 29).unwrap(), PlainMonthDay::new(12, 31).unwrap()]);
    }

    #[test]
    fn spans_normalize_subsecond_units() {
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
    fn unsupported_zone_formats_are_rejected() {
        for name in ["America/New_York", "UTC", "+05:30", "-00:01"] {
            assert!(parse_zone(name).is_ok(), "{name}");
        }
        for name in ["+05:30:15", "EST5EDT,M3.2.0,M11.1.0", "Mars/Olympus_Mons", ""] {
            assert!(parse_zone(name).is_err(), "{name}");
        }
    }

    #[test]
    fn host_zones_use_bundled_iana_rules() {
        // Minimal TZif v1 header for a fixed +09:00 (JST) zone.
        let mut tzif = b"TZif".to_vec();
        tzif.extend([0; 16]);
        for count in [0u32, 0, 0, 0, 1, 4] {
            tzif.extend(count.to_be_bytes());
        }
        tzif.extend(32_400i32.to_be_bytes());
        tzif.extend([0, 0]);
        tzif.extend(b"JST\0");
        let fake = tz::TimeZone::tzif("America/New_York", &tzif).unwrap();
        let z = ZonedDateTime::new(Timestamp::UNIX_EPOCH.to_zoned(fake)).unwrap();
        assert_eq!(z.to_string(), "1969-12-31T19:00:00-05:00[America/New_York]");
        assert_eq!(ZonedDateTime::parse(&z.to_string()).ok(), Some(z));
    }

    #[test]
    fn zones_are_compared_by_zone_id() {
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
