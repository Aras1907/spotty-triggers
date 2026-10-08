//! The little of iCalendar Proton Calendar needs: reading event text out of
//! the decrypted cards, and expanding repeating events.

use jiff::civil::{Date, DateTime, Weekday};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

/// Text properties of a VEVENT, with iCalendar escaping undone.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct EventText {
    pub summary: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
}

impl EventText {
    /// Fill gaps from `other` (cards of one event are read one after another).
    pub fn merge(&mut self, other: EventText) {
        self.summary = self.summary.take().or(other.summary);
        self.location = self.location.take().or(other.location);
        self.description = self.description.take().or(other.description);
    }
}

pub fn parse_event_text(ics: &str) -> EventText {
    // Unfold: a line starting with a space or tab continues the previous one.
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix([' ', '\t']) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(raw.to_owned());
    }
    let mut text = EventText::default();
    let mut in_event = false;
    for line in lines {
        let upper = line.to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            in_event = true;
            continue;
        }
        if upper == "END:VEVENT" {
            in_event = false;
            continue;
        }
        if !in_event {
            continue;
        }
        let Some((head, value)) = line.split_once(':') else { continue };
        let name = head.split(';').next().unwrap_or_default().to_ascii_uppercase();
        let slot = match name.as_str() {
            "SUMMARY" => &mut text.summary,
            "LOCATION" => &mut text.location,
            "DESCRIPTION" => &mut text.description,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(unescape(value));
        }
    }
    text
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone)]
struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<usize>,
    until: Option<i64>,
    by_day: Vec<(i8, Weekday)>,
    by_month_day: Vec<i8>,
    by_month: Vec<i8>,
}

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Monday,
        "TU" => Weekday::Tuesday,
        "WE" => Weekday::Wednesday,
        "TH" => Weekday::Thursday,
        "FR" => Weekday::Friday,
        "SA" => Weekday::Saturday,
        "SU" => Weekday::Sunday,
        _ => return None,
    })
}

fn parse_until(value: &str) -> Option<i64> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return None;
    }
    let date = Date::new(digits[0..4].parse().ok()?, digits[4..6].parse().ok()?, digits[6..8].parse().ok()?).ok()?;
    let (h, m, s) = if digits.len() >= 14 {
        (digits[8..10].parse().ok()?, digits[10..12].parse().ok()?, digits[12..14].parse().ok()?)
    } else {
        // A bare date means the whole day.
        (23, 59, 59)
    };
    let civil = date.at(h, m, s, 0);
    // UNTIL is UTC when it ends in Z, and the event's own time otherwise; the
    // day is what matters for display, so treat both as UTC.
    Some(civil.to_zoned(TimeZone::UTC).ok()?.timestamp().as_second())
}

fn parse_rule(rrule: &str) -> Option<Rule> {
    let rrule = rrule.trim().trim_start_matches("RRULE:");
    let mut rule = Rule {
        freq: Freq::Daily,
        interval: 1,
        count: None,
        until: None,
        by_day: Vec::new(),
        by_month_day: Vec::new(),
        by_month: Vec::new(),
    };
    let mut freq = None;
    for part in rrule.split(';') {
        let (key, value) = part.split_once('=')?;
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match value.to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => return None,
                })
            }
            "INTERVAL" => rule.interval = value.parse::<i64>().ok()?.max(1),
            "COUNT" => rule.count = Some(value.parse().ok()?),
            "UNTIL" => rule.until = parse_until(value),
            "BYDAY" => {
                for item in value.split(',') {
                    let item = item.trim();
                    let split = item.len().checked_sub(2)?;
                    let ordinal = if split == 0 { 0 } else { item[..split].parse::<i8>().ok()? };
                    rule.by_day.push((ordinal, weekday(&item[split..].to_ascii_uppercase())?));
                }
            }
            "BYMONTHDAY" => rule.by_month_day = value.split(',').filter_map(|v| v.trim().parse().ok()).collect(),
            "BYMONTH" => rule.by_month = value.split(',').filter_map(|v| v.trim().parse().ok()).collect(),
            _ => {}
        }
    }
    rule.freq = freq?;
    Some(rule)
}

/// A repeating event as Proton stores it.
pub struct Series<'a> {
    pub start: i64,
    pub end: i64,
    pub timezone: &'a str,
    pub all_day: bool,
    pub rrule: &'a str,
    /// Start times (unix seconds) that were deleted or moved.
    pub excluded: &'a [i64],
}

const MAX_STEPS: usize = 30_000;

/// The start/end (unix seconds) of every occurrence overlapping `[from, to)`.
pub fn occurrences(series: &Series, from: i64, to: i64) -> Vec<(i64, i64)> {
    let Some(rule) = parse_rule(series.rrule) else { return Vec::new() };
    let tz = if series.all_day { TimeZone::UTC } else { TimeZone::get(series.timezone).unwrap_or(TimeZone::UTC) };
    let Ok(start_ts) = Timestamp::from_second(series.start) else { return Vec::new() };
    let first = start_ts.to_zoned(tz.clone()).datetime();
    let length = series.end - series.start;
    let mut out = Vec::new();
    let mut emitted = 0usize;

    let mut visit = |candidate: DateTime| -> bool {
        let Ok(zoned) = candidate.to_zoned(tz.clone()) else { return true };
        let start = zoned.timestamp().as_second();
        if start < series.start {
            return true;
        }
        if rule.until.is_some_and(|u| start > u) || rule.count.is_some_and(|c| emitted >= c) || start >= to {
            return false;
        }
        emitted += 1;
        if start + length > from && !series.excluded.contains(&start) {
            out.push((start, start + length));
        }
        true
    };

    let time = first.time();
    let day = first.date();
    let at = |date: Date| date.to_datetime(time);
    let mut steps = 0usize;
    match rule.freq {
        Freq::Daily => {
            let mut k = 0i64;
            loop {
                steps += 1;
                let Ok(date) = day.checked_add(Span::new().days(k * rule.interval)) else { break };
                let keep = (rule.by_month.is_empty() || rule.by_month.contains(&date.month()))
                    && (rule.by_day.is_empty() || rule.by_day.iter().any(|(_, w)| *w == date.weekday()))
                    && (rule.by_month_day.is_empty() || rule.by_month_day.contains(&date.day()));
                if keep && !visit(at(date)) || steps > MAX_STEPS {
                    break;
                }
                k += 1;
            }
        }
        Freq::Weekly => {
            let days: Vec<Weekday> = if rule.by_day.is_empty() { vec![day.weekday()] } else { rule.by_day.iter().map(|(_, w)| *w).collect() };
            // Weeks start on Monday.
            let monday = day.checked_sub(Span::new().days(i64::from(day.weekday().to_monday_zero_offset()))).unwrap_or(day);
            let mut k = 0i64;
            'weeks: loop {
                let week = monday.checked_add(Span::new().weeks(k * rule.interval)).ok();
                let Some(week) = week else { break };
                let mut dates: Vec<Date> = days
                    .iter()
                    .filter_map(|w| week.checked_add(Span::new().days(i64::from(w.to_monday_zero_offset()))).ok())
                    .collect();
                dates.sort();
                for date in dates {
                    steps += 1;
                    if steps > MAX_STEPS || !visit(at(date)) {
                        break 'weeks;
                    }
                }
                k += 1;
            }
        }
        Freq::Monthly => {
            let mut k = 0i64;
            'months: loop {
                let Ok(month_start) = Date::new(first.year(), first.month(), 1).and_then(|d| d.checked_add(Span::new().months(k * rule.interval))) else { break };
                let mut dates: Vec<Date> = Vec::new();
                if !rule.by_day.is_empty() {
                    for (ordinal, wd) in &rule.by_day {
                        dates.extend(nth_weekday(month_start, *wd, *ordinal));
                    }
                } else {
                    let wanted = if rule.by_month_day.is_empty() { vec![day.day()] } else { rule.by_month_day.clone() };
                    for d in wanted {
                        let d = if d < 0 { month_start.days_in_month() + d + 1 } else { d };
                        if let Ok(date) = Date::new(month_start.year(), month_start.month(), d) {
                            dates.push(date);
                        }
                    }
                }
                dates.sort();
                for date in dates {
                    steps += 1;
                    if steps > MAX_STEPS || !visit(at(date)) {
                        break 'months;
                    }
                }
                steps += 1;
                if steps > MAX_STEPS {
                    break;
                }
                k += 1;
            }
        }
        Freq::Yearly => {
            let mut k = 0i64;
            'years: loop {
                let year = i64::from(first.year()) + k * rule.interval;
                let Ok(year) = i16::try_from(year) else { break };
                let months = if rule.by_month.is_empty() { vec![first.month()] } else { rule.by_month.clone() };
                let mut dates: Vec<Date> = Vec::new();
                for month in months {
                    let Ok(month_start) = Date::new(year, month, 1) else { continue };
                    if !rule.by_day.is_empty() {
                        for (ordinal, wd) in &rule.by_day {
                            dates.extend(nth_weekday(month_start, *wd, *ordinal));
                        }
                    } else {
                        let wanted = if rule.by_month_day.is_empty() { vec![day.day()] } else { rule.by_month_day.clone() };
                        for d in wanted {
                            let d = if d < 0 { month_start.days_in_month() + d + 1 } else { d };
                            if let Ok(date) = Date::new(year, month, d) {
                                dates.push(date);
                            }
                        }
                    }
                }
                dates.sort();
                for date in dates {
                    steps += 1;
                    if steps > MAX_STEPS || !visit(at(date)) {
                        break 'years;
                    }
                }
                steps += 1;
                if steps > MAX_STEPS {
                    break;
                }
                k += 1;
            }
        }
    }
    out
}

/// The `ordinal`-th `wd` of the month (0 = every one; negative counts from the end).
fn nth_weekday(month_start: Date, wd: Weekday, ordinal: i8) -> Vec<Date> {
    let mut all = Vec::new();
    let mut date = month_start;
    while date.month() == month_start.month() {
        if date.weekday() == wd {
            all.push(date);
        }
        match date.tomorrow() {
            Ok(next) => date = next,
            Err(_) => break,
        }
    }
    match ordinal {
        0 => all,
        n if n > 0 => all.get(usize::from(n.unsigned_abs()) - 1).copied().into_iter().collect(),
        n => all.len().checked_sub(usize::from(n.unsigned_abs())).and_then(|i| all.get(i).copied()).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(text: &str) -> i64 {
        text.parse::<Timestamp>().unwrap().as_second()
    }

    fn series<'a>(start: &str, end: &str, tz: &'a str, rrule: &'a str, excluded: &'a [i64]) -> Series<'a> {
        Series { start: ts(start), end: ts(end), timezone: tz, all_day: false, rrule, excluded }
    }

    #[test]
    fn text_is_unfolded_and_unescaped() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Lunch\\, with\r\n  Sam\r\nLOCATION;LANGUAGE=en:Cafe\\nBar\r\nDESCRIPTION:a\\;b\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let text = parse_event_text(ics);
        assert_eq!(text.summary.as_deref(), Some("Lunch, with Sam"));
        assert_eq!(text.location.as_deref(), Some("Cafe\nBar"));
        assert_eq!(text.description.as_deref(), Some("a;b"));
    }

    #[test]
    fn daily_with_count_and_exclusion() {
        let skipped = [ts("2026-03-02T09:00:00Z")];
        let s = series("2026-03-01T09:00:00Z", "2026-03-01T10:00:00Z", "UTC", "FREQ=DAILY;COUNT=4", &skipped);
        let got = occurrences(&s, ts("2026-01-01T00:00:00Z"), ts("2027-01-01T00:00:00Z"));
        let starts: Vec<i64> = got.iter().map(|o| o.0).collect();
        assert_eq!(starts, vec![ts("2026-03-01T09:00:00Z"), ts("2026-03-03T09:00:00Z"), ts("2026-03-04T09:00:00Z")]);
        assert!(got.iter().all(|o| o.1 - o.0 == 3600));
    }

    #[test]
    fn weekly_by_day_keeps_wall_clock_across_dst() {
        // Berlin: UTC+1 until 2026-03-29, UTC+2 afterwards. Monday 2026-03-23 09:00 local.
        let s = series("2026-03-23T08:00:00Z", "2026-03-23T09:00:00Z", "Europe/Berlin", "FREQ=WEEKLY;BYDAY=MO,WE", &[]);
        let got = occurrences(&s, ts("2026-03-23T00:00:00Z"), ts("2026-04-03T00:00:00Z"));
        let starts: Vec<i64> = got.iter().map(|o| o.0).collect();
        assert_eq!(
            starts,
            vec![
                ts("2026-03-23T08:00:00Z"),
                ts("2026-03-25T08:00:00Z"),
                ts("2026-03-30T07:00:00Z"),
                ts("2026-04-01T07:00:00Z"),
            ]
        );
    }

    #[test]
    fn monthly_last_friday_and_yearly() {
        let s = series("2026-01-30T12:00:00Z", "2026-01-30T13:00:00Z", "UTC", "FREQ=MONTHLY;BYDAY=-1FR;COUNT=3", &[]);
        let got: Vec<i64> = occurrences(&s, 0, ts("2027-01-01T00:00:00Z")).iter().map(|o| o.0).collect();
        assert_eq!(got, vec![ts("2026-01-30T12:00:00Z"), ts("2026-02-27T12:00:00Z"), ts("2026-03-27T12:00:00Z")]);

        let y = series("2024-02-29T10:00:00Z", "2024-02-29T11:00:00Z", "UTC", "FREQ=YEARLY", &[]);
        let got: Vec<i64> = occurrences(&y, ts("2024-01-01T00:00:00Z"), ts("2029-01-01T00:00:00Z")).iter().map(|o| o.0).collect();
        // Feb 29 only exists in leap years.
        assert_eq!(got, vec![ts("2024-02-29T10:00:00Z"), ts("2028-02-29T10:00:00Z")]);
    }

    #[test]
    fn until_and_window_limit_results() {
        let s = series("2026-05-01T09:00:00Z", "2026-05-01T09:30:00Z", "UTC", "FREQ=DAILY;UNTIL=20260510T000000Z", &[]);
        let got = occurrences(&s, ts("2026-05-08T00:00:00Z"), ts("2026-06-01T00:00:00Z"));
        assert_eq!(got.len(), 2);
        // Unknown rules produce nothing instead of garbage.
        let bad = series("2026-05-01T09:00:00Z", "2026-05-01T09:30:00Z", "UTC", "FREQ=SECONDLY", &[]);
        assert!(occurrences(&bad, 0, i64::MAX / 2).is_empty());
    }
}
