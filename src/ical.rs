//! Events from iCalendar text (RFC 5545), including repeating events, because
//! ordinary calendars store a weekly meeting as one event with a rule.

use std::collections::HashSet;

use chrono::{DateTime, Days, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rrule::RRuleSet;

const MAX_OCCURRENCES: u16 = 2000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Stamp {
    value: String,
    zone: Option<String>,
}

#[derive(Debug, Default)]
struct RawEvent {
    uid: String,
    summary: String,
    start: Stamp,
    end: Option<Stamp>,
    location: Option<String>,
    description: Option<String>,
    url: Option<String>,
    cancelled: bool,
    rule: Option<String>,
    exclusions: Vec<Stamp>,
    replaces: Option<Stamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub title: String,
    pub start: DateTime<Tz>,
    pub end: Option<DateTime<Tz>>,
    pub all_day: bool,
    pub location: Option<String>,
    pub note: Option<String>,
    pub url: Option<String>,
}

fn parse(text: &str) -> Vec<RawEvent> {
    let mut events = Vec::new();
    let mut current: Option<RawEvent> = None;

    for line in unfold(text) {
        match line.as_str() {
            "BEGIN:VEVENT" => current = Some(RawEvent::default()),
            "END:VEVENT" => {
                events.extend(current.take().filter(|event| !event.start.value.is_empty()));
            }
            _ => {
                let (Some(event), Some((name, value))) = (current.as_mut(), line.split_once(':'))
                else {
                    continue;
                };
                read_property(event, name, value);
            }
        }
    }
    events
}

fn read_property(event: &mut RawEvent, name: &str, value: &str) {
    let mut parts = name.split(';');
    let property = parts.next().unwrap_or_default();
    let zone = parts
        .find_map(|part| part.strip_prefix("TZID="))
        .map(|zone| zone.trim_matches('"').to_owned());
    let stamp = |value: &str| Stamp {
        value: value.trim().to_owned(),
        zone: zone.clone(),
    };
    match property {
        "UID" => event.uid = value.trim().to_owned(),
        "SUMMARY" => event.summary = unescape(value),
        "DTSTART" => event.start = stamp(value),
        "DTEND" => event.end = Some(stamp(value)),
        "LOCATION" => event.location = non_empty(unescape(value)),
        "DESCRIPTION" => event.description = non_empty(unescape(value)),
        "URL" => event.url = non_empty(value.trim().to_owned()),
        "STATUS" => event.cancelled = value.trim().eq_ignore_ascii_case("CANCELLED"),
        "RRULE" => event.rule = non_empty(value.trim().to_owned()),
        "EXDATE" => event.exclusions.extend(value.split(',').map(stamp)),
        "RECURRENCE-ID" => event.replaces = Some(stamp(value)),
        _ => {}
    }
}

/// A line that starts with a space or tab continues the one before.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        match (line.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(continuation), Some(previous)) => previous.push_str(continuation),
            _ => lines.push(line.to_owned()),
        }
    }
    lines
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
            Some(escaped) => out.push(escaped),
            None => {}
        }
    }
    out.trim().to_owned()
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn is_all_day(stamp: &Stamp) -> bool {
    !stamp.value.contains('T')
}

/// The zone a stamp is written in. Unknown names, such as Windows zone names,
/// fall back to the server's zone.
fn zone_of(stamp: &Stamp, fallback: Tz) -> Tz {
    if stamp.value.ends_with('Z') {
        return Tz::UTC;
    }
    stamp
        .zone
        .as_deref()
        .and_then(|zone| zone.parse().ok())
        .unwrap_or(fallback)
}

fn moment(stamp: &Stamp, fallback: Tz) -> Option<DateTime<Tz>> {
    let zone = zone_of(stamp, fallback);
    let value = stamp.value.trim_end_matches('Z');
    let local = if is_all_day(stamp) {
        NaiveDate::parse_from_str(value, "%Y%m%d")
            .ok()?
            .and_hms_opt(0, 0, 0)?
    } else {
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?
    };
    zone.from_local_datetime(&local).earliest()
}

fn written(time: &DateTime<Tz>) -> String {
    time.format("%Y%m%dT%H%M%S").to_string()
}

/// The crate wants `UNTIL` in UTC, while feeds also write it as a plain date
/// or a local time, so it is rewritten and keeps the whole last day.
fn normal_rule(rule: &str) -> String {
    rule.split(';')
        .map(|part| match part.strip_prefix("UNTIL=") {
            Some(until) if until.len() == 8 => format!("UNTIL={until}T235959Z"),
            Some(until) if !until.ends_with('Z') => format!("UNTIL={until}Z"),
            _ => part.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn starts_of(
    event: &RawEvent,
    start: DateTime<Tz>,
    fallback: Tz,
    after: DateTime<Tz>,
    before: DateTime<Tz>,
) -> Vec<DateTime<Tz>> {
    let Some(rule) = &event.rule else {
        return vec![start];
    };
    let zone = start.timezone();
    let mut text = format!(
        "DTSTART;TZID={}:{}\nRRULE:{}",
        zone.name(),
        written(&start),
        normal_rule(rule)
    );
    for excluded in &event.exclusions {
        if let Some(excluded) = moment(excluded, fallback) {
            let excluded = excluded.with_timezone(&zone);
            text.push_str(&format!(
                "\nEXDATE;TZID={}:{}",
                zone.name(),
                written(&excluded)
            ));
        }
    }
    let Ok(set) = text.parse::<RRuleSet>() else {
        return vec![start];
    };
    let crate_zone = rrule::Tz::from(zone);
    set.after(after.with_timezone(&crate_zone))
        .before(before.with_timezone(&crate_zone))
        .all(MAX_OCCURRENCES)
        .dates
        .into_iter()
        .map(|date| date.with_timezone(&zone))
        .collect()
}

/// The events that touch the days from `first` to `last`, in order, with
/// times in `zone`.
pub fn events_between(text: &str, zone: Tz, first: NaiveDate, last: NaiveDate) -> Vec<Event> {
    let raw = parse(text);
    let replaced: HashSet<(&str, DateTime<Utc>)> = raw
        .iter()
        .filter_map(|event| {
            let time = moment(event.replaces.as_ref()?, zone)?;
            Some((event.uid.as_str(), time.with_timezone(&Utc)))
        })
        .collect();
    let midnight = |day: NaiveDate| {
        day.and_hms_opt(0, 0, 0)
            .and_then(|time| zone.from_local_datetime(&time).earliest())
    };
    let (Some(from), Some(until)) = (
        midnight(first),
        last.checked_add_days(Days::new(1)).and_then(midnight),
    ) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for event in raw.iter().filter(|event| !event.cancelled) {
        let Some(start) = moment(&event.start, zone) else {
            continue;
        };
        let end = event.end.as_ref().and_then(|end| moment(end, zone));
        let length = end.map_or(Duration::zero(), |end| end - start);
        let all_day = is_all_day(&event.start);
        let earliest = from - length;
        for occurrence in starts_of(event, start, zone, earliest, until) {
            let is_replaced = event.replaces.is_none()
                && replaced.contains(&(event.uid.as_str(), occurrence.with_timezone(&Utc)));
            let finish = occurrence + length;
            let touches = occurrence < until && (occurrence >= from || finish > from);
            if is_replaced || !touches {
                continue;
            }
            found.push(Event {
                title: event.summary.clone(),
                start: occurrence.with_timezone(&zone),
                end: end.map(|_| finish.with_timezone(&zone)),
                all_day,
                location: event.location.clone(),
                note: event.description.clone(),
                url: event.url.clone(),
            });
        }
    }
    found.sort_by_key(|event| event.start);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const BERLIN: Tz = chrono_tz::Europe::Berlin;

    fn day(text: &str) -> NaiveDate {
        text.parse().expect("a date")
    }

    fn feed(events: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{events}END:VCALENDAR\r\n")
    }

    fn titles_and_starts(events: &[Event]) -> Vec<(String, String)> {
        events
            .iter()
            .map(|event| {
                (
                    event.title.clone(),
                    event.start.format("%Y-%m-%d %H:%M").to_string(),
                )
            })
            .collect()
    }

    fn pair(title: &str, start: &str) -> (String, String) {
        (title.to_owned(), start.to_owned())
    }

    #[test]
    fn reads_a_campusonline_event_with_folded_lines_and_escapes() {
        let text = feed(
            "BEGIN:VEVENT\r\nUID:1@example.org\r\n\
             URL:https://online.example/wbLv?pStpS\r\n pNr=580489\r\n\
             SUMMARY:41.09000 Messtechnik VO\\, Standardgruppe\r\n\
             DESCRIPTION:fix\\; Abhaltung\\; \r\n\
             DTSTART:20261021T103000Z\r\nDTEND:20261021T120000Z\r\n\
             LOCATION:H09 (1385|220)\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-10-21"), day("2026-10-21"));
        let event = &events[0];
        assert_eq!(event.title, "41.09000 Messtechnik VO, Standardgruppe");
        assert_eq!(event.note.as_deref(), Some("fix; Abhaltung;"));
        assert_eq!(event.location.as_deref(), Some("H09 (1385|220)"));
        assert_eq!(
            event.url.as_deref(),
            Some("https://online.example/wbLv?pStpSpNr=580489")
        );
        assert_eq!(event.start.to_rfc3339(), "2026-10-21T12:30:00+02:00");
        assert_eq!(
            event.end.expect("an end").to_rfc3339(),
            "2026-10-21T14:00:00+02:00"
        );
    }

    #[test]
    fn the_local_day_decides_not_the_utc_day() {
        let text =
            feed("BEGIN:VEVENT\r\nSUMMARY:Night\r\nDTSTART:20261021T223000Z\r\nEND:VEVENT\r\n");
        assert_eq!(
            events_between(&text, BERLIN, day("2026-10-22"), day("2026-10-22")).len(),
            1
        );
        assert!(events_between(&text, BERLIN, day("2026-10-21"), day("2026-10-21")).is_empty());
    }

    #[test]
    fn a_time_zone_in_the_feed_is_respected() {
        let text = feed(
            "BEGIN:VEVENT\r\nSUMMARY:Call\r\nDTSTART;TZID=America/New_York:20261021T090000\r\n\
             END:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:Outlook\r\n\
             DTSTART;TZID=\"W. Europe Standard Time\":20261021T090000\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-10-21"), day("2026-10-21"));
        assert_eq!(
            titles_and_starts(&events),
            [
                pair("Outlook", "2026-10-21 09:00"),
                pair("Call", "2026-10-21 15:00")
            ]
        );
    }

    #[test]
    fn whole_days_and_days_in_between_are_found() {
        let text = feed(
            "BEGIN:VEVENT\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261221\r\n\
             DTEND;VALUE=DATE:20261228\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-12-24"), day("2026-12-24"));
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert!(events_between(&text, BERLIN, day("2026-12-28"), day("2026-12-28")).is_empty());
    }

    #[test]
    fn a_weekly_rule_repeats_at_the_same_local_time_across_the_clock_change() {
        let text = feed(
            "BEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Seminar\r\n\
             DTSTART;TZID=Europe/Berlin:20261019T100000\r\n\
             DTEND;TZID=Europe/Berlin:20261019T113000\r\n\
             RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-10-26"), day("2026-11-02"));
        assert_eq!(
            titles_and_starts(&events),
            [
                pair("Seminar", "2026-10-26 10:00"),
                pair("Seminar", "2026-11-02 10:00")
            ]
        );
        assert_eq!(
            events[1].end.expect("an end").format("%H:%M").to_string(),
            "11:30"
        );
    }

    #[test]
    fn a_rule_ends_at_its_until_date_even_when_written_as_a_plain_date() {
        for until in ["20261102", "20261102T120000Z", "20261102T120000"] {
            let text = feed(&format!(
                "BEGIN:VEVENT\r\nSUMMARY:Seminar\r\nDTSTART;TZID=Europe/Berlin:20261019T100000\r\n\
                 RRULE:FREQ=WEEKLY;UNTIL={until}\r\nEND:VEVENT\r\n"
            ));
            let events = events_between(&text, BERLIN, day("2026-10-19"), day("2026-12-31"));
            assert_eq!(events.len(), 3, "{until}");
        }
    }

    #[test]
    fn excluded_and_moved_occurrences_appear_only_where_they_belong() {
        let text = feed(
            "BEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Seminar\r\n\
             DTSTART;TZID=Europe/Berlin:20261019T100000\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\n\
             EXDATE;TZID=Europe/Berlin:20261026T100000\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Seminar moved\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20261102T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20261103T140000\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Seminar\r\nSTATUS:CANCELLED\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20261109T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20261109T100000\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-10-19"), day("2026-12-31"));
        assert_eq!(
            titles_and_starts(&events),
            [
                pair("Seminar", "2026-10-19 10:00"),
                pair("Seminar moved", "2026-11-03 14:00")
            ]
        );
    }

    #[test]
    fn a_rule_on_whole_days_and_in_utc_works() {
        let text = feed(
            "BEGIN:VEVENT\r\nSUMMARY:Birthday\r\nDTSTART;VALUE=DATE:20200315\r\n\
             RRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:Standup\r\nDTSTART:20261001T070000Z\r\n\
             RRULE:FREQ=DAILY;COUNT=2\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2027-03-15"), day("2027-03-15"));
        assert_eq!(
            titles_and_starts(&events),
            [pair("Birthday", "2027-03-15 00:00")]
        );
        let events = events_between(&text, BERLIN, day("2026-10-02"), day("2026-10-02"));
        assert_eq!(
            titles_and_starts(&events),
            [pair("Standup", "2026-10-02 09:00")]
        );
    }

    #[test]
    fn a_broken_rule_still_lists_the_first_date_and_bad_events_are_skipped() {
        let text = feed(
            "BEGIN:VEVENT\r\nSUMMARY:Odd\r\nDTSTART:20261021T080000Z\r\nRRULE:NONSENSE\r\n\
             END:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:Broken\r\nDTSTART:tomorrow\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:No start\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-10-21"), day("2026-10-21"));
        assert_eq!(
            titles_and_starts(&events),
            [pair("Odd", "2026-10-21 10:00")]
        );
    }

    #[test]
    fn events_come_in_order_and_only_for_the_asked_days() {
        let text = feed(
            "BEGIN:VEVENT\r\nSUMMARY:Late\r\nDTSTART:20261022T140000Z\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:Before\r\nDTSTART:20261020T080000Z\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:Early\r\nDTSTART:20261022T060000Z\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nSUMMARY:After\r\nDTSTART:20261023T080000Z\r\nEND:VEVENT\r\n",
        );
        let events = events_between(&text, BERLIN, day("2026-10-21"), day("2026-10-22"));
        assert_eq!(
            titles_and_starts(&events),
            [
                pair("Early", "2026-10-22 08:00"),
                pair("Late", "2026-10-22 16:00")
            ]
        );
    }
}
