//! The local day: one calendar date in the machine's time zone, and the instants it spans.
//!
//! Every per-day computation (today's spend, daily history keys, price-cut days, walking a range
//! of days) goes through [`LocalDay`], so none of them assumes a day is 24 hours long. Days where
//! DST starts or ends are 23 or 25 hours; where DST starts at midnight the day begins at the end
//! of the gap (e.g. 01:00).

use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

/// One local calendar date, with its bounds resolved in the zone it was built in.
///
/// Ordered by date. [`LocalDay::of`]`(ms).contains(ms)` holds for every instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalDay {
    date: NaiveDate,
    start_ms: i64,
    end_ms: i64,
    noon_ms: i64,
}

impl LocalDay {
    /// The local day containing the instant `ms` (today if `ms` is out of range).
    pub fn of(ms: i64) -> Self {
        Self::of_in(ms, &Local)
    }

    /// The local day containing now.
    pub fn today() -> Self {
        Self::on_in(Local::now().date_naive(), &Local)
    }

    /// The local day with key `YYYY-MM-DD`.
    pub fn parse(key: &str) -> Option<Self> {
        NaiveDate::parse_from_str(key, "%Y-%m-%d").ok().map(|d| Self::on_in(d, &Local))
    }

    /// The day before, in the local zone.
    pub fn prev(&self) -> Self {
        self.date.pred_opt().map_or(*self, |d| Self::on_in(d, &Local))
    }

    /// The day after, in the local zone.
    pub fn next(&self) -> Self {
        self.date.succ_opt().map_or(*self, |d| Self::on_in(d, &Local))
    }

    /// The day `n` days before this one, in the local zone.
    pub fn back(&self, n: usize) -> Self {
        self.date.checked_sub_days(chrono::Days::new(n as u64)).map_or(*self, |d| Self::on_in(d, &Local))
    }

    /// `YYYY-MM-DD`, as the usage history keys its days.
    pub fn key(&self) -> String {
        self.date.format("%Y-%m-%d").to_string()
    }

    pub fn date(&self) -> NaiveDate {
        self.date
    }

    /// First instant of the day, in Unix ms.
    pub fn start_ms(&self) -> i64 {
        self.start_ms
    }

    /// First instant after the day: the next day's start.
    pub fn end_ms(&self) -> i64 {
        self.end_ms
    }

    /// Local 12:00 (wall clock), not `start + 12h`: the two differ on DST days.
    pub fn noon_ms(&self) -> i64 {
        self.noon_ms
    }

    /// True when the instant `ms` falls in `[start_ms, end_ms)`.
    pub fn contains(&self, ms: i64) -> bool {
        self.start_ms <= ms && ms < self.end_ms
    }

    /// The day containing `ms` in `tz`.
    pub(crate) fn of_in<Tz: TimeZone>(ms: i64, tz: &Tz) -> Self {
        let date = match tz.timestamp_millis_opt(ms).single() {
            Some(dt) => dt.date_naive(),
            None => tz.from_utc_datetime(&Utc::now().naive_utc()).date_naive(),
        };
        let day = Self::on_in(date, tz);
        // Where clocks fall back across midnight (e.g. 00:30 back to 23:30), the wall clock shows
        // the previous date again after the new day began; such instants belong to the new day.
        match date.succ_opt() {
            Some(next) if ms >= day.end_ms => Self::on_in(next, tz),
            _ => day,
        }
    }

    /// The day `date` in `tz`.
    pub(crate) fn on_in<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Self {
        let start_ms =
            day_start_in(date, tz).unwrap_or_else(|| date.and_time(NaiveTime::MIN).and_utc().timestamp_millis());
        let end_ms = date.succ_opt().and_then(|d| day_start_in(d, tz)).unwrap_or(start_ms + 86_400_000).max(start_ms);
        let noon = date.and_hms_opt(12, 0, 0).and_then(|t| first_instant_from(t, tz));
        let noon_ms = noon.unwrap_or(start_ms + (end_ms - start_ms) / 2).clamp(start_ms, end_ms);
        Self { date, start_ms, end_ms, noon_ms }
    }
}

/// First instant of `date` in `tz`, in Unix ms. Usually local 00:00, but where DST starts at
/// midnight (America/Santiago, Asia/Beirut, ...) 00:00 does not exist and the day begins at the
/// end of the gap, e.g. 01:00. `None` only if no valid local minute exists within 24h of midnight
/// (a day skipped entirely, like Samoa's 2011-12-30, yields the next day's start).
fn day_start_in<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Option<i64> {
    first_instant_from(date.and_time(NaiveTime::MIN), tz)
}

/// The earliest instant whose local time is `local`, or the first valid local minute after it
/// (scanning up to 24h) when `local` falls in a DST gap.
fn first_instant_from<Tz: TimeZone>(local: NaiveDateTime, tz: &Tz) -> Option<i64> {
    (0..=24 * 60).find_map(|m| {
        let t = local.checked_add_signed(chrono::Duration::minutes(m))?;
        t.and_local_timezone(tz.clone()).earliest().map(|dt| dt.timestamp_millis())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::FixedOffset;

    const HOUR: i64 = 3_600_000;

    /// A zone with one offset change at `at_ms` (UTC), so DST cases are testable without
    /// depending on the machine's timezone or a tz database.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct OneShift {
        pub at_ms: i64,
        pub before_secs: i32,
        pub after_secs: i32,
    }

    impl TimeZone for OneShift {
        type Offset = FixedOffset;

        fn from_offset(offset: &FixedOffset) -> Self {
            let secs = offset.local_minus_utc();
            OneShift { at_ms: i64::MAX, before_secs: secs, after_secs: secs }
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> chrono::MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> chrono::MappedLocalTime<FixedOffset> {
            let utc_ms = |off: i32| local.and_utc().timestamp_millis() - i64::from(off) * 1000;
            let fixed = |off: i32| FixedOffset::east_opt(off).unwrap();
            let before = (utc_ms(self.before_secs) < self.at_ms).then(|| fixed(self.before_secs));
            let after = (utc_ms(self.after_secs) >= self.at_ms).then(|| fixed(self.after_secs));
            match (before, after) {
                (Some(a), Some(b)) => chrono::MappedLocalTime::Ambiguous(a, b),
                (Some(o), None) | (None, Some(o)) => chrono::MappedLocalTime::Single(o),
                (None, None) => chrono::MappedLocalTime::None,
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            let secs = if utc.and_utc().timestamp_millis() < self.at_ms { self.before_secs } else { self.after_secs };
            FixedOffset::east_opt(secs).unwrap()
        }
    }

    /// Spring forward at 02:00 (+01 → +02) on 2026-03-29: a 23-hour day.
    pub(crate) fn spring_forward() -> OneShift {
        OneShift { at_ms: utc_ms("2026-03-29T01:00:00Z"), before_secs: 3600, after_secs: 2 * 3600 }
    }

    /// Fall back at 03:00 (+02 → +01) on 2026-10-25: a 25-hour day.
    pub(crate) fn fall_back() -> OneShift {
        OneShift { at_ms: utc_ms("2026-10-25T01:00:00Z"), before_secs: 2 * 3600, after_secs: 3600 }
    }

    pub(crate) fn utc_ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    pub(crate) fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn day_start_is_midnight_on_ordinary_days() {
        assert_eq!(day_start_in(date("2026-11-27"), &Utc), Some(utc_ms("2026-11-27T00:00:00Z")));
        let plus8 = FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(day_start_in(date("2026-11-27"), &plus8), Some(utc_ms("2026-11-26T16:00:00Z")));
    }

    #[test]
    fn day_start_skips_a_dst_gap_at_midnight() {
        // Santiago-style: at local 00:00 (-04) clocks jump to 01:00 (-03).
        let tz = OneShift { at_ms: utc_ms("2026-09-06T04:00:00Z"), before_secs: -4 * 3600, after_secs: -3 * 3600 };
        let midnight = date("2026-09-06").and_time(NaiveTime::MIN);
        assert!(midnight.and_local_timezone(tz).earliest().is_none(), "00:00 must not exist in this zone");
        assert_eq!(day_start_in(date("2026-09-06"), &tz), Some(utc_ms("2026-09-06T04:00:00Z")));
        // Neighbouring days are unaffected.
        assert_eq!(day_start_in(date("2026-09-05"), &tz), Some(utc_ms("2026-09-05T04:00:00Z")));
        assert_eq!(day_start_in(date("2026-09-07"), &tz), Some(utc_ms("2026-09-07T03:00:00Z")));
        // The day before ends where the gap day starts: 23 hours.
        let before = LocalDay::on_in(date("2026-09-05"), &tz);
        assert_eq!(before.end_ms() - before.start_ms(), 24 * HOUR);
        let gap_day = LocalDay::on_in(date("2026-09-06"), &tz);
        assert_eq!(gap_day.end_ms() - gap_day.start_ms(), 23 * HOUR);
        assert_eq!(gap_day.noon_ms(), utc_ms("2026-09-06T15:00:00Z"));
    }

    #[test]
    fn day_start_takes_the_earlier_of_a_repeated_midnight() {
        // At local 01:00 (+01) clocks fall back to 00:00 (+00), so 00:00–01:00 happens twice.
        let tz = OneShift { at_ms: utc_ms("2026-10-25T00:00:00Z"), before_secs: 3600, after_secs: 0 };
        assert_eq!(day_start_in(date("2026-10-25"), &tz), Some(utc_ms("2026-10-24T23:00:00Z")));
    }

    #[test]
    fn day_start_of_a_skipped_day_is_the_next_day_start() {
        // Samoa 2011: 2011-12-29 24:00 (-10) became 2011-12-31 00:00 (+14).
        let tz = OneShift { at_ms: utc_ms("2011-12-30T10:00:00Z"), before_secs: -10 * 3600, after_secs: 14 * 3600 };
        assert_eq!(day_start_in(date("2011-12-30"), &tz), Some(utc_ms("2011-12-30T10:00:00Z")));
        assert_eq!(day_start_in(date("2011-12-31"), &tz), Some(utc_ms("2011-12-30T10:00:00Z")));
        let skipped = LocalDay::on_in(date("2011-12-30"), &tz);
        assert_eq!(skipped.start_ms(), skipped.end_ms(), "a skipped day spans no instant");
        let before = LocalDay::of_in(utc_ms("2011-12-30T09:59:59Z"), &tz);
        let after = LocalDay::of_in(utc_ms("2011-12-30T10:00:00Z"), &tz);
        assert_eq!((before.key(), after.key()), ("2011-12-29".to_string(), "2011-12-31".to_string()));
        assert_eq!(before.end_ms(), after.start_ms());
    }

    #[test]
    fn ordinary_day_spans_24_hours_with_noon_in_the_middle() {
        let plus8 = FixedOffset::east_opt(8 * 3600).unwrap();
        let day = LocalDay::of_in(utc_ms("2026-11-27T03:00:00Z"), &plus8);
        assert_eq!(day.key(), "2026-11-27");
        assert_eq!(day.start_ms(), utc_ms("2026-11-26T16:00:00Z"));
        assert_eq!(day.end_ms(), utc_ms("2026-11-27T16:00:00Z"));
        assert_eq!(day.noon_ms(), utc_ms("2026-11-27T04:00:00Z"));
        assert!(day.contains(day.start_ms()) && !day.contains(day.end_ms()));
    }

    #[test]
    fn spring_forward_day_is_23_hours_and_noon_is_wall_clock() {
        let tz = spring_forward();
        let day = LocalDay::on_in(date("2026-03-29"), &tz);
        assert_eq!(day.start_ms(), utc_ms("2026-03-28T23:00:00Z"));
        assert_eq!(day.end_ms(), utc_ms("2026-03-29T22:00:00Z"));
        assert_eq!(day.end_ms() - day.start_ms(), 23 * HOUR);
        assert_eq!(day.noon_ms(), utc_ms("2026-03-29T10:00:00Z"));
        assert_eq!(day.noon_ms() - day.start_ms(), 11 * HOUR);
        // The last hour before local midnight is still this day; `start + 24h` is not.
        assert_eq!(LocalDay::of_in(day.end_ms() - 1, &tz), day);
        assert_eq!(LocalDay::of_in(day.start_ms() + 24 * HOUR - 1, &tz).key(), "2026-03-30");
        assert_eq!(LocalDay::of_in(day.end_ms(), &tz), LocalDay::on_in(date("2026-03-30"), &tz));
    }

    #[test]
    fn fall_back_day_is_25_hours_and_noon_is_wall_clock() {
        let tz = fall_back();
        let day = LocalDay::on_in(date("2026-10-25"), &tz);
        assert_eq!(day.start_ms(), utc_ms("2026-10-24T22:00:00Z"));
        assert_eq!(day.end_ms(), utc_ms("2026-10-25T23:00:00Z"));
        assert_eq!(day.end_ms() - day.start_ms(), 25 * HOUR);
        assert_eq!(day.noon_ms(), utc_ms("2026-10-25T11:00:00Z"));
        assert_eq!(day.noon_ms() - day.start_ms(), 13 * HOUR);
        // The 25th hour still belongs to this day.
        let last_hour = day.start_ms() + 24 * HOUR + 30 * 60_000;
        assert!(day.contains(last_hour));
        assert_eq!(LocalDay::of_in(last_hour, &tz), day);
    }

    #[test]
    fn fall_back_across_midnight_keeps_days_contiguous() {
        // At local 00:30 (+02) on 2026-10-25 clocks go back to 23:30 (+01) on 2026-10-24.
        let tz = OneShift { at_ms: utc_ms("2026-10-24T22:30:00Z"), before_secs: 2 * 3600, after_secs: 3600 };
        let after = utc_ms("2026-10-24T22:45:00Z"); // wall clock 23:45 on the 24th, again
        let day = LocalDay::of_in(after, &tz);
        assert_eq!(day.key(), "2026-10-25");
        assert!(day.contains(after));
        assert_eq!(day.start_ms(), utc_ms("2026-10-24T22:00:00Z"));
    }

    #[test]
    fn walks_days_by_date_not_by_24_hours() {
        let tz = fall_back();
        let d = LocalDay::of_in(utc_ms("2026-10-24T12:00:00Z"), &tz);
        let next = LocalDay::on_in(d.date().succ_opt().unwrap(), &tz);
        assert_eq!(d.end_ms(), next.start_ms());
        assert_eq!(next.key(), "2026-10-25");

        let today = LocalDay::today();
        assert_eq!(today.next().prev(), today);
        assert_eq!(today.next().start_ms(), today.end_ms());
        assert!(today.prev() < today && today < today.next());
        assert_eq!(today.back(0), today);
        assert_eq!(today.back(2), today.prev().prev());
        assert_eq!(LocalDay::parse(&today.key()), Some(today));
        assert_eq!(LocalDay::parse("not-a-day"), None);
    }
}
