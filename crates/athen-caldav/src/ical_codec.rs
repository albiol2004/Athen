//! Minimal iCalendar (RFC 5545) codec — just the VEVENT subset Athen needs.
//!
//! Full RFC 5545 has surprising corners (TZID with VTIMEZONE blocks,
//! RDATE/EXDATE, escape rules, line folding at 75 chars). This module
//! covers what mainstream CalDAV servers (iCloud, Google, Fastmail)
//! actually emit for everyday events:
//!
//! - UTC datetimes (`YYYYMMDDTHHMMSSZ`)
//! - Date-only values (`VALUE=DATE:YYYYMMDD`)
//! - `SUMMARY`, `DESCRIPTION`, `LOCATION`, `UID`, `RRULE`
//! - One `VALARM` block with a `TRIGGER` duration → reminder minutes
//!
//! TZID parameters with IANA names (`America/New_York`, `Europe/Madrid`,
//! ...) are resolved via `chrono-tz`. iPhone Calendar typically writes
//! events with a TZID rather than a `Z`-suffixed UTC timestamp, so this
//! is the common path for iCloud sync. Legacy Apple Windows zone names
//! and arbitrary inline VTIMEZONE blocks still fall back to UTC with a
//! tracing warn.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source::RemoteEvent;

/// Unfold an iCalendar text per RFC 5545 §3.1 — lines that begin with a
/// space or tab are continuations of the previous logical line.
fn unfold(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            let last = out.last_mut().unwrap();
            last.push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// One logical iCalendar property line.
#[derive(Debug, Clone)]
struct Prop {
    name: String,
    /// Parameter key/value pairs (the `;TZID=...;VALUE=DATE` bits).
    params: Vec<(String, String)>,
    value: String,
}

fn parse_prop(line: &str) -> Option<Prop> {
    let colon = line.find(':')?;
    let (head, rest) = line.split_at(colon);
    let value = unescape_text(&rest[1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    let mut params = Vec::new();
    for p in parts {
        if let Some(eq) = p.find('=') {
            let (k, v) = p.split_at(eq);
            params.push((k.trim().to_ascii_uppercase(), v[1..].trim().to_string()));
        }
    }
    Some(Prop {
        name,
        params,
        value,
    })
}

fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\\' {
            match iter.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Parse a date or datetime value with the property's parameters.
///
/// Returns the parsed UTC datetime and an `all_day` flag. Floating times
/// (no `Z`, no recognized TZID) are treated as UTC with a tracing warning.
fn parse_datetime(prop: &Prop) -> Result<(DateTime<Utc>, bool)> {
    let value_param = prop
        .params
        .iter()
        .find(|(k, _)| k == "VALUE")
        .map(|(_, v)| v.as_str());
    let tzid = prop
        .params
        .iter()
        .find(|(k, _)| k == "TZID")
        .map(|(_, v)| v.as_str());

    if value_param == Some("DATE") {
        let nd = NaiveDate::parse_from_str(&prop.value, "%Y%m%d")
            .map_err(|e| AthenError::Other(format!("Bad VALUE=DATE `{}`: {e}", prop.value)))?;
        let ndt = nd
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| AthenError::Other("date midnight overflow".into()))?;
        return Ok((Utc.from_utc_datetime(&ndt), true));
    }

    let val = prop.value.trim();
    if let Some(stripped) = val.strip_suffix('Z') {
        let ndt = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .map_err(|e| AthenError::Other(format!("Bad UTC datetime `{val}`: {e}")))?;
        return Ok((Utc.from_utc_datetime(&ndt), false));
    }

    let ndt = NaiveDateTime::parse_from_str(val, "%Y%m%dT%H%M%S")
        .map_err(|e| AthenError::Other(format!("Bad datetime `{val}`: {e}")))?;

    if let Some(zone) = tzid {
        match zone.parse::<Tz>() {
            Ok(tz) => {
                // Ambiguous local times (DST fall-back hour) — pick the
                // earlier of the two offsets. Nonexistent times (DST
                // spring-forward gap) — bump to the next valid instant.
                let resolved = tz
                    .from_local_datetime(&ndt)
                    .earliest()
                    .or_else(|| tz.from_local_datetime(&ndt).latest())
                    .ok_or_else(|| {
                        AthenError::Other(format!("Datetime `{val}` does not exist in zone {zone}"))
                    })?;
                return Ok((resolved.with_timezone(&Utc), false));
            }
            Err(_) => {
                tracing::warn!(
                    tzid = %zone,
                    value = %val,
                    "Unrecognized TZID (not an IANA name); treating as UTC"
                );
            }
        }
    }
    Ok((Utc.from_utc_datetime(&ndt), false))
}

/// Convert a VALARM `TRIGGER` like `-PT15M` / `-PT1H` / `-P1D` into a
/// positive count of minutes before the event start. Negative-only —
/// alarms after the start are ignored.
fn parse_trigger_minutes(value: &str) -> Option<i64> {
    let v = value.trim();
    let rest = v.strip_prefix('-')?;
    let rest = rest.strip_prefix('P').unwrap_or(rest);
    let mut total = 0i64;
    let mut chars = rest.chars().peekable();
    let mut in_time = false;
    let mut num = String::new();
    while let Some(c) = chars.next() {
        if c == 'T' {
            in_time = true;
            continue;
        }
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        match (in_time, c) {
            (false, 'D') => total += n * 24 * 60,
            (false, 'W') => total += n * 7 * 24 * 60,
            (true, 'H') => total += n * 60,
            (true, 'M') => total += n,
            // Seconds are rounded down — Athen doesn't fire sub-minute reminders.
            (true, 'S') => {}
            _ => return None,
        }
        let _ = chars.peek();
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

/// Parse a VCALENDAR text and return every VEVENT it contains.
///
/// The same `calendar_id` and optional `etag` are stamped onto each
/// returned event because the caller knows which collection and which
/// multistatus row the bytes came from.
pub fn parse_vcalendar(
    text: &str,
    calendar_id: &str,
    remote_id: &str,
    etag: Option<String>,
) -> Result<Vec<RemoteEvent>> {
    let lines = unfold(text);
    let mut events: Vec<RemoteEvent> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            let start_idx = i + 1;
            let end_idx = lines[start_idx..]
                .iter()
                .position(|l| l.trim_end_matches('\r').eq_ignore_ascii_case("END:VEVENT"))
                .map(|off| start_idx + off)
                .ok_or_else(|| AthenError::Other("Unterminated VEVENT".into()))?;
            let ev = parse_vevent_lines(
                &lines[start_idx..end_idx],
                calendar_id,
                remote_id,
                etag.clone(),
            )?;
            events.push(ev);
            i = end_idx + 1;
        } else {
            i += 1;
        }
    }
    Ok(events)
}

fn parse_vevent_lines(
    lines: &[String],
    calendar_id: &str,
    remote_id: &str,
    etag: Option<String>,
) -> Result<RemoteEvent> {
    let mut uid: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut description: Option<String> = None;
    let mut location: Option<String> = None;
    let mut start: Option<(DateTime<Utc>, bool)> = None;
    let mut end: Option<(DateTime<Utc>, bool)> = None;
    let mut rrule: Option<String> = None;
    let mut reminders: Vec<i64> = Vec::new();

    let mut in_alarm = false;
    let mut alarm_trigger: Option<String> = None;

    for raw in lines {
        let line = raw.trim_end_matches('\r');
        if line.eq_ignore_ascii_case("BEGIN:VALARM") {
            in_alarm = true;
            alarm_trigger = None;
            continue;
        }
        if line.eq_ignore_ascii_case("END:VALARM") {
            if let Some(t) = alarm_trigger.take() {
                if let Some(mins) = parse_trigger_minutes(&t) {
                    reminders.push(mins);
                }
            }
            in_alarm = false;
            continue;
        }
        let Some(p) = parse_prop(line) else { continue };
        if in_alarm {
            if p.name == "TRIGGER" {
                alarm_trigger = Some(p.value);
            }
            continue;
        }
        match p.name.as_str() {
            "UID" => uid = Some(p.value),
            "SUMMARY" => summary = Some(p.value),
            "DESCRIPTION" => description = Some(p.value),
            "LOCATION" => location = Some(p.value),
            "DTSTART" => start = Some(parse_datetime(&p)?),
            "DTEND" => end = Some(parse_datetime(&p)?),
            "RRULE" => rrule = Some(p.value),
            _ => {}
        }
    }

    let (start_dt, start_all_day) =
        start.ok_or_else(|| AthenError::Other("VEVENT missing DTSTART".into()))?;
    let (end_dt, _) = end.unwrap_or_else(|| {
        // RFC 5545 default: all-day events end at start+1 day, timed events
        // are treated as instantaneous (start == end).
        if start_all_day {
            (start_dt + chrono::Duration::days(1), true)
        } else {
            (start_dt, false)
        }
    });

    Ok(RemoteEvent {
        remote_id: remote_id.to_string(),
        calendar_id: calendar_id.to_string(),
        etag,
        ical_uid: uid,
        title: summary.unwrap_or_else(|| "(untitled)".into()),
        description,
        start_time: start_dt,
        end_time: end_dt,
        all_day: start_all_day,
        location,
        recurrence_rrule: rrule,
        reminder_minutes: reminders,
    })
}

/// Emit a VCALENDAR text containing one VEVENT for the given event.
/// All datetimes go on the wire as UTC.
pub fn emit_vcalendar(event: &RemoteEvent) -> String {
    let uid = event
        .ical_uid
        .clone()
        .unwrap_or_else(|| format!("{}@athen", uuid::Uuid::new_v4()));
    let now = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Athen//CalDAV//EN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{now}"),
    ];
    if event.all_day {
        lines.push(format!(
            "DTSTART;VALUE=DATE:{}",
            event.start_time.format("%Y%m%d")
        ));
        lines.push(format!(
            "DTEND;VALUE=DATE:{}",
            event.end_time.format("%Y%m%d")
        ));
    } else {
        lines.push(format!(
            "DTSTART:{}",
            event.start_time.format("%Y%m%dT%H%M%SZ")
        ));
        lines.push(format!("DTEND:{}", event.end_time.format("%Y%m%dT%H%M%SZ")));
    }
    lines.push(format!("SUMMARY:{}", escape_text(&event.title)));
    if let Some(d) = &event.description {
        lines.push(format!("DESCRIPTION:{}", escape_text(d)));
    }
    if let Some(l) = &event.location {
        lines.push(format!("LOCATION:{}", escape_text(l)));
    }
    if let Some(r) = &event.recurrence_rrule {
        lines.push(format!("RRULE:{r}"));
    }
    for mins in &event.reminder_minutes {
        lines.push("BEGIN:VALARM".into());
        lines.push("ACTION:DISPLAY".into());
        lines.push(format!("DESCRIPTION:{}", escape_text(&event.title)));
        lines.push(format!("TRIGGER:-PT{mins}M"));
        lines.push("END:VALARM".into());
    }
    lines.push("END:VEVENT".into());
    lines.push("END:VCALENDAR".into());
    fold_lines(&lines)
}

/// Fold long lines at 75 octets per RFC 5545 §3.1. Continuation lines
/// start with a single space.
fn fold_lines(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        let bytes = line.as_bytes();
        if bytes.len() <= 75 {
            out.push_str(line);
            out.push_str("\r\n");
            continue;
        }
        // Fold on character boundaries; iterate by char to avoid splitting
        // a multibyte sequence at a non-boundary.
        let mut buf = String::new();
        let mut len = 0;
        for c in line.chars() {
            let cl = c.len_utf8();
            if len + cl > 75 {
                out.push_str(&buf);
                out.push_str("\r\n ");
                buf.clear();
                len = 1; // for the leading space
            }
            buf.push(c);
            len += cl;
        }
        if !buf.is_empty() {
            out.push_str(&buf);
            out.push_str("\r\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfold_continuations() {
        let raw = "FOO:bar\r\n baz\r\nQUX:hello";
        let out = unfold(raw);
        assert_eq!(out, vec!["FOO:barbaz".to_string(), "QUX:hello".to_string()]);
    }

    #[test]
    fn parse_prop_with_params() {
        let p =
            parse_prop("DTSTART;TZID=America/New_York;VALUE=DATE-TIME:20260520T143000").unwrap();
        assert_eq!(p.name, "DTSTART");
        assert_eq!(p.params.len(), 2);
        assert_eq!(p.params[0], ("TZID".into(), "America/New_York".into()));
        assert_eq!(p.value, "20260520T143000");
    }

    #[test]
    fn escape_unescape_roundtrip() {
        let s = "Hello, World;\nLine 2\\here";
        let round = unescape_text(&escape_text(s));
        assert_eq!(round, s);
    }

    #[test]
    fn parse_trigger_minutes_cases() {
        assert_eq!(parse_trigger_minutes("-PT15M"), Some(15));
        assert_eq!(parse_trigger_minutes("-PT1H"), Some(60));
        assert_eq!(parse_trigger_minutes("-P1D"), Some(24 * 60));
        assert_eq!(parse_trigger_minutes("-P1DT2H"), Some(24 * 60 + 120));
        assert_eq!(parse_trigger_minutes("PT15M"), None); // no leading -
        assert_eq!(parse_trigger_minutes("-PT0M"), None);
    }

    #[test]
    fn parse_real_world_vevent_utc() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
            UID:evt-123@icloud.com\r\nSUMMARY:Team Sync\r\n\
            DESCRIPTION:Weekly catch-up\r\nLOCATION:Zoom\r\n\
            DTSTART:20260520T143000Z\r\nDTEND:20260520T153000Z\r\n\
            BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vcalendar(ics, "cal-1", "evt.ics", Some("\"abc\"".into())).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.ical_uid.as_deref(), Some("evt-123@icloud.com"));
        assert_eq!(e.title, "Team Sync");
        assert_eq!(e.description.as_deref(), Some("Weekly catch-up"));
        assert_eq!(e.location.as_deref(), Some("Zoom"));
        assert_eq!(
            e.start_time.format("%Y%m%dT%H%M%SZ").to_string(),
            "20260520T143000Z"
        );
        assert_eq!(
            e.end_time.format("%Y%m%dT%H%M%SZ").to_string(),
            "20260520T153000Z"
        );
        assert!(!e.all_day);
        assert_eq!(e.reminder_minutes, vec![15]);
        assert_eq!(e.etag.as_deref(), Some("\"abc\""));
        assert_eq!(e.calendar_id, "cal-1");
        assert_eq!(e.remote_id, "evt.ics");
    }

    #[test]
    fn parse_tzid_event_resolves_to_utc() {
        // iPhone-style event: 10:00 in New_York is 14:00 UTC (EDT) or
        // 15:00 UTC (EST). Stick to a clearly-DST date to make the
        // expected offset unambiguous.
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:tz1\r\n\
            SUMMARY:NYC Meeting\r\n\
            DTSTART;TZID=America/New_York:20260620T100000\r\n\
            DTEND;TZID=America/New_York:20260620T110000\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vcalendar(ics, "cal", "tz1.ics", None).unwrap();
        assert_eq!(events.len(), 1);
        // June = EDT (UTC-4) → 10:00 local = 14:00 UTC.
        assert_eq!(
            events[0].start_time.format("%Y%m%dT%H%M%SZ").to_string(),
            "20260620T140000Z"
        );
        assert_eq!(
            events[0].end_time.format("%Y%m%dT%H%M%SZ").to_string(),
            "20260620T150000Z"
        );
    }

    #[test]
    fn parse_tzid_handles_madrid_winter() {
        // January = CET (UTC+1) → 09:00 Madrid = 08:00 UTC.
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:tz2\r\n\
            SUMMARY:Standup\r\n\
            DTSTART;TZID=Europe/Madrid:20260115T090000\r\n\
            DTEND;TZID=Europe/Madrid:20260115T093000\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vcalendar(ics, "cal", "tz2.ics", None).unwrap();
        assert_eq!(
            events[0].start_time.format("%Y%m%dT%H%M%SZ").to_string(),
            "20260115T080000Z"
        );
    }

    #[test]
    fn parse_unknown_tzid_falls_back_to_utc() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:tz3\r\n\
            SUMMARY:Legacy\r\n\
            DTSTART;TZID=Eastern Standard Time:20260620T100000\r\n\
            DTEND;TZID=Eastern Standard Time:20260620T110000\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vcalendar(ics, "cal", "tz3.ics", None).unwrap();
        assert_eq!(
            events[0].start_time.format("%Y%m%dT%H%M%SZ").to_string(),
            "20260620T100000Z"
        );
    }

    #[test]
    fn parse_all_day_event() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:Birthday\r\n\
            DTSTART;VALUE=DATE:20260520\r\nDTEND;VALUE=DATE:20260521\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vcalendar(ics, "cal", "x.ics", None).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].title, "Birthday");
    }

    #[test]
    fn emit_then_parse_roundtrip() {
        let original = RemoteEvent {
            remote_id: "evt.ics".into(),
            calendar_id: "cal".into(),
            etag: None,
            ical_uid: Some("athen-test-1".into()),
            title: "Lunch with Bob".into(),
            description: Some("Discuss; Q3 plans".into()), // semicolon to test escaping
            start_time: Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
            end_time: Utc.with_ymd_and_hms(2026, 6, 1, 13, 0, 0).unwrap(),
            all_day: false,
            location: Some("Joe's Diner".into()),
            recurrence_rrule: Some("FREQ=WEEKLY;BYDAY=MO".into()),
            reminder_minutes: vec![15, 60],
        };
        let ics = emit_vcalendar(&original);
        let parsed = parse_vcalendar(&ics, "cal", "evt.ics", None).unwrap();
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.ical_uid.as_deref(), Some("athen-test-1"));
        assert_eq!(p.title, "Lunch with Bob");
        assert_eq!(p.description.as_deref(), Some("Discuss; Q3 plans"));
        assert_eq!(p.location.as_deref(), Some("Joe's Diner"));
        assert_eq!(p.recurrence_rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert_eq!(p.reminder_minutes, vec![15, 60]);
        assert_eq!(p.start_time, original.start_time);
        assert_eq!(p.end_time, original.end_time);
    }

    #[test]
    fn fold_long_summary() {
        let long = "x".repeat(200);
        let lines = vec![format!("SUMMARY:{long}")];
        let out = fold_lines(&lines);
        // Each non-initial output line begins with " " — count them.
        let folds = out.matches("\r\n ").count();
        assert!(folds > 0, "no fold inserted for long line");
    }
}
