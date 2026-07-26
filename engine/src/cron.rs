//! Cron expressions.
//!
//! Written here rather than pulled from a crate for one reason: this code
//! decides when unattended production backups run, and its semantics have to be
//! the ones the user already has in their head. Standard five-field cron is a
//! small, fixed, fully specifiable language, and the surprising parts of it
//! (below) are worth owning and testing directly.
//!
//! # Matching, not "next occurrence"
//!
//! The primitive is [`CronExpression::matches`] — "does this wall-clock minute
//! satisfy the expression" — exactly as `cron(8)` itself works. Everything else,
//! including [`CronExpression::next_after`], is a scan over that same predicate.
//! One implementation means the "next run at ..." shown in the UI cannot
//! disagree with what the scheduler actually does.
//!
//! # The day-of-month / day-of-week rule
//!
//! In Vixie cron, if *either* the day-of-month or the day-of-week field is a
//! star, the two are combined with AND; if neither is, they are combined with
//! OR. So `0 0 13 * 5` is "the 13th, and also every Friday", not "Friday the
//! 13th". This astonishes people, but it is the behaviour every crontab on
//! every Unix has, so it is the behaviour implemented here.
//!
//! # Daylight saving
//!
//! A local-time expression is evaluated against the wall clock. When the clock
//! springs forward, the skipped hour does not exist, so an expression that only
//! matches inside it does not fire that day; when it falls back, the repeated
//! hour fires once, not twice. Schedules that must never be affected by this
//! should be set to [`ScheduleTimezone::Utc`], which has no discontinuities.

use std::fmt;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use specta::Type;

/// Which clock a cron expression is read against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleTimezone {
    /// The machine's local time. What "back up at 2am" normally means, at the
    /// cost of the daylight-saving behaviour described in the module docs.
    #[default]
    Local,
    /// UTC. Immune to daylight saving; the right choice for a schedule that
    /// must fire exactly once every 24 hours.
    Utc,
}

impl ScheduleTimezone {
    pub const fn as_str(self) -> &'static str {
        match self {
            ScheduleTimezone::Local => "local",
            ScheduleTimezone::Utc => "utc",
        }
    }
}

/// Resolve a wall-clock time in `zone` to an instant.
///
/// Returns `None` for a local time that does not exist — the spring-forward
/// gap — so scanning treats it as "this minute never happens" and moves on.
///
/// For an ambiguous time (the fall-back repeat) the *earlier* instant is used,
/// so the schedule fires once, on the first pass of that wall clock, as cron
/// does. The two candidates are compared directly rather than trusting the
/// order of `LocalResult::Ambiguous`: chrono orders that pair by UTC offset,
/// not by instant, so its `.earliest()` is the later instant whenever the
/// offset decreases across the transition — which is every autumn transition.
fn resolve_in<Tz: TimeZone>(zone: &Tz, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    match zone.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(a, b) => Some(a.with_timezone(&Utc).min(b.with_timezone(&Utc))),
        LocalResult::None => None,
    }
}

/// The wall-clock reading of an instant in `zone`.
fn wall_clock_in<Tz: TimeZone>(zone: &Tz, at: DateTime<Utc>) -> NaiveDateTime {
    at.with_timezone(zone).naive_local()
}

impl FromStr for ScheduleTimezone {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(ScheduleTimezone::Local),
            "utc" => Ok(ScheduleTimezone::Utc),
            other => Err(CronError::UnknownTimezone(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CronError {
    #[error(
        "a cron expression has five fields (minute hour day-of-month month day-of-week); got {0}"
    )]
    WrongFieldCount(usize),
    #[error(
        "{0:?} looks like a six-field expression with seconds; this scheduler uses standard \
         five-field cron, where the smallest interval is one minute"
    )]
    LooksLikeSeconds(String),
    #[error(
        "{0:?} is not a recognised shorthand (try @hourly, @daily, @weekly, @monthly, @yearly)"
    )]
    UnknownShorthand(String),
    #[error("@reboot has no meaning here; use a time-based expression instead")]
    RebootUnsupported,
    #[error("{value:?} is not valid in the {field} field: {reason}")]
    BadField {
        field: &'static str,
        value: String,
        reason: String,
    },
    #[error("the expression is empty")]
    Empty,
    #[error("unknown timezone {0:?}; use \"local\" or \"utc\"")]
    UnknownTimezone(String),
}

fn bad(field: &'static str, value: &str, reason: impl Into<String>) -> CronError {
    CronError::BadField {
        field,
        value: value.to_string(),
        reason: reason.into(),
    }
}

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DOW_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// How far ahead [`CronExpression::next_after`] will look before giving up.
///
/// Comfortably over four years, so `0 0 29 2 *` still resolves on a leap year
/// rather than reporting "never".
const MAX_SCAN_DAYS: i64 = 1_600;

/// A parsed five-field cron expression.
///
/// Construction goes through [`FromStr`], so a value of this type is always a
/// valid expression — there is no path by which an unparseable string reaches
/// the scheduler from either the database or the frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpression {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    /// Set when the day-of-month field begins with `*`. See the module docs.
    dom_star: bool,
    dow_star: bool,
    /// The text as the user wrote it, so round-tripping never rewrites it.
    source: String,
}

const fn bit(v: u32) -> u64 {
    1u64 << v
}

const fn has(set: u64, v: u32) -> bool {
    set & bit(v) != 0
}

impl CronExpression {
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Does this wall-clock minute satisfy the expression?
    ///
    /// Seconds are ignored: cron's resolution is one minute.
    pub fn matches(&self, at: NaiveDateTime) -> bool {
        has(self.minute, at.minute()) && has(self.hour, at.hour()) && self.day_matches(at.date())
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        if !has(self.month, date.month()) {
            return false;
        }

        let dom = has(self.dom, date.day());
        let dow = has(self.dow, date.weekday().num_days_from_sunday());

        // The Vixie rule, stated once, in one place. See the module docs.
        if self.dom_star || self.dow_star {
            dom && dow
        } else {
            dom || dow
        }
    }

    /// The first instant strictly after `after` that satisfies the expression.
    ///
    /// `None` means there is no occurrence within [`MAX_SCAN_DAYS`], which for a
    /// well-formed expression only happens for genuinely impossible dates such
    /// as `0 0 30 2 *`.
    pub fn next_after(&self, tz: ScheduleTimezone, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match tz {
            ScheduleTimezone::Utc => self.next_after_in(&Utc, after),
            ScheduleTimezone::Local => self.next_after_in(&Local, after),
        }
    }

    /// The most recent instant at or before `at` that satisfies the expression.
    ///
    /// This is what answers "did we miss a run while the machine was asleep?".
    pub fn prev_at_or_before(
        &self,
        tz: ScheduleTimezone,
        at: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        match tz {
            ScheduleTimezone::Utc => self.prev_at_or_before_in(&Utc, at),
            ScheduleTimezone::Local => self.prev_at_or_before_in(&Local, at),
        }
    }

    /// [`next_after`](Self::next_after) against an arbitrary time zone.
    ///
    /// Public so daylight-saving behaviour can be tested against real IANA
    /// zones rather than against whatever zone the build machine happens to be
    /// in — this machine is on a zone with no DST at all, so a `Local` test
    /// would assert nothing. It is also the seam through which per-schedule
    /// named zones would be added.
    pub fn next_after_in<Tz: TimeZone>(
        &self,
        zone: &Tz,
        after: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        // Start from the minute after the one containing `after`, so a call made
        // during a matching minute returns the *next* occurrence.
        let start = wall_clock_in(zone, after)
            .with_second(0)?
            .with_nanosecond(0)?
            + chrono::Duration::minutes(1);

        self.scan(zone, start, Direction::Forward)
    }

    /// [`prev_at_or_before`](Self::prev_at_or_before) against an arbitrary zone.
    pub fn prev_at_or_before_in<Tz: TimeZone>(
        &self,
        zone: &Tz,
        at: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let start = wall_clock_in(zone, at).with_second(0)?.with_nanosecond(0)?;
        self.scan(zone, start, Direction::Backward)
    }

    /// Walk day by day, and within a matching day only over the minutes the
    /// expression actually names.
    ///
    /// Skipping whole non-matching days is what keeps a worst case like
    /// `0 0 29 2 *` (four years away) to about 1,600 cheap date checks instead
    /// of two million minute checks.
    fn scan<Tz: TimeZone>(
        &self,
        zone: &Tz,
        start: NaiveDateTime,
        direction: Direction,
    ) -> Option<DateTime<Utc>> {
        let mut date = start.date();

        for day in 0..MAX_SCAN_DAYS {
            if self.day_matches(date) {
                // Only the first day is bounded by `start`; later days are
                // scanned in full.
                let bound = (day == 0).then(|| start.time());

                if let Some(found) = self.scan_within_day(zone, date, bound, direction) {
                    return Some(found);
                }
            }

            date = match direction {
                Direction::Forward => date.succ_opt()?,
                Direction::Backward => date.pred_opt()?,
            };
        }

        None
    }

    fn scan_within_day<Tz: TimeZone>(
        &self,
        zone: &Tz,
        date: NaiveDate,
        bound: Option<chrono::NaiveTime>,
        direction: Direction,
    ) -> Option<DateTime<Utc>> {
        let hours: Vec<u32> = (0..24u32).filter(|h| has(self.hour, *h)).collect();
        let minutes: Vec<u32> = (0..60u32).filter(|m| has(self.minute, *m)).collect();

        let mut candidates: Vec<(u32, u32)> = hours
            .iter()
            .flat_map(|h| minutes.iter().map(move |m| (*h, *m)))
            .collect();

        if direction == Direction::Backward {
            candidates.reverse();
        }

        for (h, m) in candidates {
            if let Some(limit) = bound {
                let past_bound = match direction {
                    Direction::Forward => (h, m) < (limit.hour(), limit.minute()),
                    Direction::Backward => (h, m) > (limit.hour(), limit.minute()),
                };
                if past_bound {
                    continue;
                }
            }

            let naive = date.and_hms_opt(h, m, 0)?;

            // A local time inside the spring-forward gap never happens, so it
            // is not an occurrence. Keep scanning rather than reporting it.
            if let Some(instant) = resolve_in(zone, naive) {
                return Some(instant);
            }
        }

        None
    }

    /// A short human reading of the expression, for the UI and the CLI.
    ///
    /// Deliberately only covers the shapes people actually write; anything else
    /// falls back to the expression itself rather than risking a confident
    /// description that is subtly wrong.
    pub fn describe(&self) -> String {
        let minutes: Vec<u32> = (0..60u32).filter(|m| has(self.minute, *m)).collect();
        let hours: Vec<u32> = (0..24u32).filter(|h| has(self.hour, *h)).collect();
        let all_months = (1..=12u32).all(|m| has(self.month, m));

        let time = match (minutes.as_slice(), hours.as_slice()) {
            ([m], [h]) => format!("{h:02}:{m:02}"),
            ([m], hs) if hs.len() == 24 => format!("every hour at :{m:02}"),
            _ => return self.source.clone(),
        };

        if !all_months {
            return self.source.clone();
        }

        let every_dom = (1..=31u32).all(|d| has(self.dom, d));
        let dow_list: Vec<u32> = (0..7u32).filter(|d| has(self.dow, *d)).collect();
        let every_dow = dow_list.len() == 7;

        match (every_dom, every_dow) {
            (true, true) => {
                if time.starts_with("every") {
                    time
                } else {
                    format!("every day at {time}")
                }
            }
            (true, false) if self.dom_star => {
                let days: Vec<&str> = dow_list
                    .iter()
                    .map(|d| DOW_NAMES[*d as usize])
                    .map(capitalise_static)
                    .collect();
                format!("{} at {time}", days.join(", "))
            }
            (false, true) if self.dow_star => {
                let days: Vec<String> = (1..=31u32)
                    .filter(|d| has(self.dom, *d))
                    .map(|d| d.to_string())
                    .collect();
                format!("day {} of the month at {time}", days.join(", "))
            }
            _ => self.source.clone(),
        }
    }
}

fn capitalise_static(day: &str) -> &'static str {
    match day {
        "sun" => "Sunday",
        "mon" => "Monday",
        "tue" => "Tuesday",
        "wed" => "Wednesday",
        "thu" => "Thursday",
        "fri" => "Friday",
        _ => "Saturday",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

impl fmt::Display for CronExpression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl FromStr for CronExpression {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(CronError::Empty);
        }

        if let Some(shorthand) = trimmed.strip_prefix('@') {
            return expand_shorthand(shorthand).and_then(|expanded| {
                let mut parsed = parse_fields(&expanded)?;
                // Report the shorthand back to the user, not its expansion.
                parsed.source = trimmed.to_string();
                Ok(parsed)
            });
        }

        parse_fields(trimmed)
    }
}

fn expand_shorthand(name: &str) -> Result<String, CronError> {
    let expanded = match name.trim().to_ascii_lowercase().as_str() {
        "yearly" | "annually" => "0 0 1 1 *",
        "monthly" => "0 0 1 * *",
        "weekly" => "0 0 * * 0",
        "daily" | "midnight" => "0 0 * * *",
        "hourly" => "0 * * * *",
        // Meaningless for a scheduler that only knows wall-clock time, and
        // silently treating it as "never" would lose a user's backups.
        "reboot" => return Err(CronError::RebootUnsupported),
        other => return Err(CronError::UnknownShorthand(format!("@{other}"))),
    };
    Ok(expanded.to_string())
}

fn parse_fields(text: &str) -> Result<CronExpression, CronError> {
    let fields: Vec<&str> = text.split_whitespace().collect();

    if fields.len() == 6 || fields.len() == 7 {
        // Quartz/Spring style. Silently reading it as five fields would shift
        // every field by one and run the job at a wildly different time.
        return Err(CronError::LooksLikeSeconds(text.to_string()));
    }
    if fields.len() != 5 {
        return Err(CronError::WrongFieldCount(fields.len()));
    }

    let (minute, _) = parse_field("minute", fields[0], 0, 59, &[])?;
    let (hour, _) = parse_field("hour", fields[1], 0, 23, &[])?;
    let (dom, dom_star) = parse_field("day-of-month", fields[2], 1, 31, &[])?;
    let (month, _) = parse_field("month", fields[3], 1, 12, &MONTH_NAMES)?;
    let (dow, dow_star) = parse_field("day-of-week", fields[4], 0, 7, &DOW_NAMES)?;

    Ok(CronExpression {
        minute,
        hour,
        dom,
        month,
        // Both 0 and 7 mean Sunday; normalise so matching only tests bit 0.
        dow: if has(dow, 7) {
            (dow & !bit(7)) | bit(0)
        } else {
            dow
        },
        dom_star,
        dow_star,
        source: text.to_string(),
    })
}

/// Parse one field, returning its bit set and whether it began with `*`.
///
/// The star flag is what drives the day-of-month/day-of-week rule, and it is
/// deliberately about the *leading character*, matching Vixie: `*/2` counts as
/// a star even though it does not select every value.
fn parse_field(
    name: &'static str,
    spec: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<(u64, bool), CronError> {
    let is_star = spec.starts_with('*');
    let mut bits = 0u64;

    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(bad(name, spec, "empty entry in a comma-separated list"));
        }

        let (range_spec, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| bad(name, spec, format!("{s:?} is not a step number")))?;
                if step == 0 {
                    return Err(bad(name, spec, "a step of 0 would match nothing"));
                }
                (r, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range_spec == "*" {
            (min, max)
        } else if let Some((a, b)) = range_spec.split_once('-') {
            let lo = parse_value(name, spec, a, min, max, names)?;
            let hi = parse_value(name, spec, b, min, max, names)?;
            if lo > hi {
                return Err(bad(
                    name,
                    spec,
                    format!("range {lo}-{hi} runs backwards; write it as two entries"),
                ));
            }
            (lo, hi)
        } else {
            let v = parse_value(name, spec, range_spec, min, max, names)?;
            // `5/15` means "from 5 to the end of the field, every 15" — a Vixie
            // extension people rely on for things like `0 9/4 * * *`.
            if step > 1 { (v, max) } else { (v, v) }
        };

        let mut v = lo;
        while v <= hi {
            bits |= bit(v);
            v += step;
        }
    }

    if bits == 0 {
        return Err(bad(name, spec, "matches nothing"));
    }

    Ok((bits, is_star))
}

fn parse_value(
    field: &'static str,
    spec: &str,
    token: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<u32, CronError> {
    let token = token.trim();

    if let Some(index) = names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(token.get(..3).unwrap_or(token)))
        .filter(|_| token.len() >= 3 && token.chars().all(|c| c.is_ascii_alphabetic()))
    {
        // Month names are 1-based, weekday names 0-based.
        return Ok(if names == MONTH_NAMES {
            index as u32 + 1
        } else {
            index as u32
        });
    }

    let value: u32 = token
        .parse()
        .map_err(|_| bad(field, spec, format!("{token:?} is not a number or a name")))?;

    if value < min || value > max {
        return Err(bad(
            field,
            spec,
            format!("{value} is outside the valid range {min}-{max}"),
        ));
    }

    Ok(value)
}

// A cron expression crosses the process boundary as the string the user typed.
// Parsing on the way in means an unparseable expression cannot be persisted or
// handed to the scheduler in the first place.
impl Serialize for CronExpression {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for CronExpression {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn cron(s: &str) -> CronExpression {
        s.parse().expect("expression should parse")
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    // ── Parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parses_a_plain_daily_expression() {
        let c = cron("30 2 * * *");
        assert!(c.matches(at(2026, 7, 26, 2, 30)));
        assert!(!c.matches(at(2026, 7, 26, 2, 31)));
        assert!(!c.matches(at(2026, 7, 26, 3, 30)));
    }

    #[test]
    fn parses_lists_ranges_and_steps() {
        let c = cron("0,30 9-17/4 * * *");
        // Hours 9, 13, 17 at minutes 0 and 30.
        assert!(c.matches(at(2026, 7, 26, 9, 0)));
        assert!(c.matches(at(2026, 7, 26, 13, 30)));
        assert!(c.matches(at(2026, 7, 26, 17, 0)));
        assert!(!c.matches(at(2026, 7, 26, 10, 0)));
        assert!(!c.matches(at(2026, 7, 26, 9, 15)));
    }

    #[test]
    fn star_slash_is_a_step_over_the_whole_range() {
        let c = cron("*/15 * * * *");
        for m in [0, 15, 30, 45] {
            assert!(c.matches(at(2026, 7, 26, 4, m)), "minute {m} should match");
        }
        assert!(!c.matches(at(2026, 7, 26, 4, 14)));
    }

    #[test]
    fn a_bare_value_with_a_step_runs_to_the_end_of_the_field() {
        // The Vixie extension: `9/4` in the hour field is 9, 13, 17, 21.
        let c = cron("0 9/4 * * *");
        for h in [9, 13, 17, 21] {
            assert!(c.matches(at(2026, 7, 26, h, 0)), "hour {h} should match");
        }
        assert!(!c.matches(at(2026, 7, 26, 5, 0)));
    }

    #[test]
    fn month_and_weekday_names_are_accepted() {
        let c = cron("0 3 * JAN,jul Mon");
        assert!(c.matches(at(2026, 7, 27, 3, 0)), "27 Jul 2026 is a Monday");
        assert!(!c.matches(at(2026, 8, 3, 3, 0)), "August is not selected");
    }

    #[test]
    fn seven_and_zero_both_mean_sunday() {
        let sunday = at(2026, 7, 26, 5, 0);
        assert_eq!(sunday.date().weekday().num_days_from_sunday(), 0);
        assert!(cron("0 5 * * 7").matches(sunday));
        assert!(cron("0 5 * * 0").matches(sunday));
        // A range that wraps through 7 must still include Sunday.
        assert!(cron("0 5 * * 5-7").matches(sunday));
    }

    #[test]
    fn shorthands_expand_but_display_as_written() {
        let daily = cron("@daily");
        assert!(daily.matches(at(2026, 7, 26, 0, 0)));
        assert!(!daily.matches(at(2026, 7, 26, 0, 1)));
        assert_eq!(daily.as_str(), "@daily", "the user's text is preserved");

        assert!(cron("@hourly").matches(at(2026, 7, 26, 13, 0)));
        assert!(cron("@weekly").matches(at(2026, 7, 26, 0, 0)));
        assert!(cron("@monthly").matches(at(2026, 7, 1, 0, 0)));
        assert!(cron("@yearly").matches(at(2026, 1, 1, 0, 0)));
    }

    #[test]
    fn reboot_is_rejected_with_an_explanation() {
        // Accepting it and never firing would silently lose every backup.
        assert_eq!(
            "@reboot".parse::<CronExpression>(),
            Err(CronError::RebootUnsupported)
        );
    }

    #[test]
    fn six_field_expressions_are_rejected_not_misread() {
        // "0 0 2 * * *" as five fields would be 00:00 on day 2 — a Quartz user
        // means 02:00 daily. Shifting silently is the worst possible outcome.
        let err = "0 0 2 * * *".parse::<CronExpression>().unwrap_err();
        assert!(matches!(err, CronError::LooksLikeSeconds(_)));
        assert!(err.to_string().contains("five-field"));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!("0 25 * * *".parse::<CronExpression>().is_err());
        assert!("60 * * * *".parse::<CronExpression>().is_err());
        assert!("0 0 32 * *".parse::<CronExpression>().is_err());
        assert!("0 0 * 13 *".parse::<CronExpression>().is_err());
        assert!("0 0 * * 8".parse::<CronExpression>().is_err());
    }

    #[test]
    fn malformed_expressions_are_rejected() {
        assert!("".parse::<CronExpression>().is_err());
        assert!("* * *".parse::<CronExpression>().is_err());
        assert!("*/0 * * * *".parse::<CronExpression>().is_err());
        assert!("17-5 * * * *".parse::<CronExpression>().is_err());
        assert!("0,,5 * * * *".parse::<CronExpression>().is_err());
        assert!("nonsense * * * *".parse::<CronExpression>().is_err());
        assert!("@nope".parse::<CronExpression>().is_err());
    }

    #[test]
    fn errors_name_the_offending_field() {
        let err = "0 99 * * *".parse::<CronExpression>().unwrap_err();
        assert!(err.to_string().contains("hour"), "got: {err}");
    }

    // ── The day-of-month / day-of-week rule ─────────────────────────────

    #[test]
    fn restricted_dom_and_dow_are_combined_with_or() {
        // Vixie's rule: "the 13th, or any Friday" — NOT "Friday the 13th".
        let c = cron("0 0 13 * 5");

        let the_13th_a_monday = at(2026, 7, 13, 0, 0);
        assert_eq!(the_13th_a_monday.date().weekday().num_days_from_sunday(), 1);
        assert!(c.matches(the_13th_a_monday), "the 13th matches on its own");

        let a_friday_the_24th = at(2026, 7, 24, 0, 0);
        assert_eq!(a_friday_the_24th.date().weekday().num_days_from_sunday(), 5);
        assert!(
            c.matches(a_friday_the_24th),
            "any Friday matches on its own"
        );

        assert!(!c.matches(at(2026, 7, 25, 0, 0)), "a Saturday the 25th");
    }

    #[test]
    fn a_star_in_either_day_field_switches_to_and() {
        // With dom = *, only the weekday constrains.
        let weekdays = cron("0 6 * * 1-5");
        assert!(weekdays.matches(at(2026, 7, 27, 6, 0)), "Monday");
        assert!(!weekdays.matches(at(2026, 7, 26, 6, 0)), "Sunday");

        // With dow = *, only the day-of-month constrains.
        let first = cron("0 6 1 * *");
        assert!(first.matches(at(2026, 7, 1, 6, 0)));
        assert!(!first.matches(at(2026, 7, 2, 6, 0)));
    }

    // ── Occurrence scanning ─────────────────────────────────────────────

    #[test]
    fn next_after_is_strictly_after() {
        let c = cron("0 3 * * *");
        let now = Utc.with_ymd_and_hms(2026, 7, 26, 3, 0, 0).unwrap();

        let next = c.next_after(ScheduleTimezone::Utc, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 27, 3, 0, 0).unwrap());
    }

    #[test]
    fn next_after_finds_the_same_day_when_it_can() {
        let c = cron("0 3 * * *");
        let now = Utc.with_ymd_and_hms(2026, 7, 26, 1, 0, 0).unwrap();
        let next = c.next_after(ScheduleTimezone::Utc, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 26, 3, 0, 0).unwrap());
    }

    #[test]
    fn next_after_crosses_a_leap_day_four_years_out() {
        // The scan budget exists for exactly this case.
        let c = cron("0 0 29 2 *");
        let now = Utc.with_ymd_and_hms(2025, 3, 1, 0, 0, 0).unwrap();
        let next = c.next_after(ScheduleTimezone::Utc, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2028, 2, 29, 0, 0, 0).unwrap());
    }

    #[test]
    fn an_impossible_date_reports_no_occurrence() {
        // 30 February never happens; the scan must terminate, not spin.
        let c = cron("0 0 30 2 *");
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert!(c.next_after(ScheduleTimezone::Utc, now).is_none());
    }

    #[test]
    fn prev_at_or_before_includes_the_current_minute() {
        let c = cron("0 3 * * *");
        let now = Utc.with_ymd_and_hms(2026, 7, 26, 3, 0, 30).unwrap();

        // 03:00:30 is inside the matching minute, so that minute is the
        // occurrence — this is what makes a tick a few seconds late still fire.
        let prev = c.prev_at_or_before(ScheduleTimezone::Utc, now).unwrap();
        assert_eq!(prev, Utc.with_ymd_and_hms(2026, 7, 26, 3, 0, 0).unwrap());
    }

    #[test]
    fn prev_at_or_before_reaches_back_to_yesterday() {
        let c = cron("0 3 * * *");
        let now = Utc.with_ymd_and_hms(2026, 7, 26, 2, 0, 0).unwrap();
        let prev = c.prev_at_or_before(ScheduleTimezone::Utc, now).unwrap();
        assert_eq!(prev, Utc.with_ymd_and_hms(2026, 7, 25, 3, 0, 0).unwrap());
    }

    #[test]
    fn next_and_prev_agree_with_matches() {
        // The whole point of scanning over `matches` rather than computing
        // occurrences separately: the two can never disagree.
        let c = cron("*/7 1-5 * * 1-5");
        let mut cursor = Utc.with_ymd_and_hms(2026, 7, 26, 0, 0, 0).unwrap();

        for _ in 0..50 {
            let next = c.next_after(ScheduleTimezone::Utc, cursor).unwrap();
            assert!(
                c.matches(next.naive_utc()),
                "next_after returned {next}, which does not match"
            );
            assert_eq!(
                c.prev_at_or_before(ScheduleTimezone::Utc, next).unwrap(),
                next,
                "prev of an occurrence is that occurrence"
            );
            cursor = next;
        }
    }

    #[test]
    fn scanning_never_returns_a_non_matching_minute() {
        let c = cron("15 22 1 * *");
        let now = Utc.with_ymd_and_hms(2026, 7, 26, 12, 0, 0).unwrap();
        let next = c.next_after(ScheduleTimezone::Utc, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 8, 1, 22, 15, 0).unwrap());
    }

    // ── Timezone handling ───────────────────────────────────────────────

    #[test]
    fn utc_and_local_agree_only_when_the_offset_is_zero() {
        let c = cron("0 12 * * *");
        let now = Utc.with_ymd_and_hms(2026, 7, 26, 0, 0, 0).unwrap();

        let utc_next = c.next_after(ScheduleTimezone::Utc, now).unwrap();
        let local_next = c.next_after(ScheduleTimezone::Local, now).unwrap();

        // Both must be real occurrences in their own frame of reference.
        assert_eq!(utc_next.naive_utc().hour(), 12);
        assert_eq!(local_next.with_timezone(&Local).hour(), 12);
    }

    #[test]
    fn a_utc_schedule_is_exactly_24_hours_apart() {
        // The reason UTC is offered at all: no daylight-saving discontinuity.
        let c = cron("0 2 * * *");
        let mut cursor = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();

        for _ in 0..120 {
            let next = c.next_after(ScheduleTimezone::Utc, cursor).unwrap();
            if cursor.hour() == 2 && cursor.minute() == 0 {
                assert_eq!(
                    next - cursor,
                    chrono::Duration::hours(24),
                    "UTC schedules must never drift"
                );
            }
            cursor = next;
        }
    }

    // ── Daylight saving, against real zones ─────────────────────────────
    //
    // Driven through `*_in` with IANA zones rather than through `Local`: the
    // machine this is developed on is in a zone that has had no DST since 2016,
    // so a `Local` assertion here would pass without testing anything.

    #[test]
    fn a_time_inside_the_spring_forward_gap_never_fires() {
        // New York, 8 March 2026: 02:00 becomes 03:00, so 02:30 does not exist.
        let ny = chrono_tz::America::New_York;
        let c = cron("30 2 * * *");

        let before = ny
            .with_ymd_and_hms(2026, 3, 7, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let next = c.next_after_in(&ny, before).unwrap();

        // The 8th is skipped entirely; the next 02:30 is on the 9th.
        assert_eq!(next.with_timezone(&ny).day(), 9);
        assert_eq!(next.with_timezone(&ny).hour(), 2);
        assert_eq!(next.with_timezone(&ny).minute(), 30);
    }

    #[test]
    fn a_daily_backup_outside_the_gap_still_runs_on_the_transition_day() {
        // The mitigation the docs recommend: pick an hour that always exists.
        let ny = chrono_tz::America::New_York;
        let c = cron("30 4 * * *");

        let before = ny
            .with_ymd_and_hms(2026, 3, 7, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let next = c.next_after_in(&ny, before).unwrap();
        assert_eq!(next.with_timezone(&ny).day(), 8, "must not skip a day");
    }

    #[test]
    fn an_ambiguous_autumn_time_fires_once_on_the_first_pass() {
        // New York, 1 November 2026: 01:30 happens twice, at 05:30 UTC (EDT)
        // and again at 06:30 UTC (EST). It must fire once, on the first.
        let ny = chrono_tz::America::New_York;
        let c = cron("30 1 * * *");

        let before = ny
            .with_ymd_and_hms(2026, 10, 31, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let next = c.next_after_in(&ny, before).unwrap();

        assert_eq!(
            next,
            Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap(),
            "the earlier of the two instants — chrono's own ordering of \
             LocalResult::Ambiguous is by offset, not by instant"
        );

        // And the following occurrence is the next day, not the repeat.
        let after = c.next_after_in(&ny, next).unwrap();
        assert_eq!(after.with_timezone(&ny).day(), 2, "must not fire twice");
    }

    #[test]
    fn southern_hemisphere_transitions_behave_the_same_way() {
        // Sydney runs its transitions in the opposite months; the logic must
        // not have a northern-hemisphere assumption baked into it.
        let sydney = chrono_tz::Australia::Sydney;
        let c = cron("30 2 * * *");

        // 4 October 2026: 02:00 -> 03:00, so 02:30 does not exist.
        let before = sydney
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let next = c.next_after_in(&sydney, before).unwrap();
        assert_eq!(next.with_timezone(&sydney).day(), 5);
    }

    #[test]
    fn a_utc_schedule_is_unaffected_by_a_local_transition() {
        // The documented reason UTC is offered: this is the same wall-clock
        // expression, over the same dates, with no skipped day.
        let c = cron("30 2 * * *");
        let mut cursor = Utc.with_ymd_and_hms(2026, 3, 6, 0, 0, 0).unwrap();
        let mut days = Vec::new();

        for _ in 0..5 {
            cursor = c.next_after(ScheduleTimezone::Utc, cursor).unwrap();
            days.push(cursor.day());
        }
        assert_eq!(days, vec![6, 7, 8, 9, 10], "no day may be skipped");
    }

    #[test]
    fn every_scanned_occurrence_is_a_real_instant_in_its_zone() {
        // A returned instant must read back as a matching wall clock — the
        // property that would break if a gap time were ever returned.
        let ny = chrono_tz::America::New_York;
        let c = cron("*/30 * * * *");
        let mut cursor = ny
            .with_ymd_and_hms(2026, 3, 7, 20, 0, 0)
            .unwrap()
            .with_timezone(&Utc);

        for _ in 0..200 {
            cursor = c.next_after_in(&ny, cursor).unwrap();
            let wall = cursor.with_timezone(&ny).naive_local();
            assert!(
                c.matches(wall),
                "{cursor} reads as {wall} locally, which does not match"
            );
        }
    }

    #[test]
    fn timezone_parses_from_its_stored_form() {
        assert_eq!(
            "local".parse::<ScheduleTimezone>().unwrap(),
            ScheduleTimezone::Local
        );
        assert_eq!(
            "UTC".parse::<ScheduleTimezone>().unwrap(),
            ScheduleTimezone::Utc
        );
        assert!("mars".parse::<ScheduleTimezone>().is_err());
    }

    // ── Serialisation ───────────────────────────────────────────────────

    #[test]
    fn round_trips_as_the_original_string() {
        let c = cron("30 2 * * 1-5");
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"30 2 * * 1-5\"");
        assert_eq!(serde_json::from_str::<CronExpression>(&json).unwrap(), c);
    }

    #[test]
    fn an_invalid_expression_cannot_be_deserialised() {
        // The guarantee that a CronExpression is always valid depends on this.
        assert!(serde_json::from_str::<CronExpression>("\"not a cron\"").is_err());
        assert!(serde_json::from_str::<CronExpression>("\"0 0 2 * * *\"").is_err());
    }

    // ── Descriptions ────────────────────────────────────────────────────

    #[test]
    fn describes_the_common_shapes() {
        assert_eq!(cron("30 2 * * *").describe(), "every day at 02:30");
        assert_eq!(cron("0 0 * * *").describe(), "every day at 00:00");
        assert_eq!(cron("15 * * * *").describe(), "every hour at :15");
        assert_eq!(cron("0 6 * * 1").describe(), "Monday at 06:00");
        assert_eq!(cron("0 4 1 * *").describe(), "day 1 of the month at 04:00");
    }

    #[test]
    fn an_expression_it_cannot_phrase_describes_itself() {
        // Better a raw expression than a confident description that is wrong.
        let odd = "*/7 1-5 13 * 5";
        assert_eq!(cron(odd).describe(), odd);
    }
}
