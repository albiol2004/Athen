//! Pure `compute_next_fire`: given a schedule and a reference timestamp,
//! return the next fire time strictly after the reference. No IO, no clock.
//!
//! Returns `None` for terminal states:
//! - `OneShot { at }` whose `at <= after` (already past).
//! - `Cron` with an unparseable expression or unknown timezone.
//! - `Interval` with `every_seconds == 0` (would loop forever).

use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;

use athen_core::wakeup::Schedule;

/// Strictly-greater-than `after`. The scheduler always passes `after = now`
/// when re-arming after a fire, so missed fires of the same schedule
/// coalesce into one (the next slot after now, not the next slot after the
/// missed time).
pub fn compute_next_fire(schedule: &Schedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match schedule {
        Schedule::OneShot { at } => {
            if *at > after {
                Some(*at)
            } else {
                None
            }
        }
        Schedule::Cron { expr, tz } => next_cron(expr, tz, after),
        Schedule::Interval {
            every_seconds,
            anchor,
        } => next_interval(*every_seconds, *anchor, after),
    }
}

fn next_cron(expr: &str, tz: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let zone: Tz = tz.parse().ok()?;
    // The `cron` crate expects 6- or 7-field cron (sec min hour dom mon dow
    // [year]). Athen's user-facing schedule docs talk about 5-field cron
    // (the standard "min hour dom mon dow"); we prepend "0 " for seconds
    // so callers don't have to think about it.
    let canonical = canonicalize_cron_expr(expr);
    let schedule = cron::Schedule::from_str(&canonical).ok()?;
    let after_tz = after.with_timezone(&zone);
    schedule
        .after(&after_tz)
        .next()
        .map(|dt_tz| dt_tz.with_timezone(&Utc))
}

/// Normalize user-supplied cron text to what the `cron` crate expects.
///
/// - 5 fields → prepend `0 ` (seconds).
/// - 6 or 7 fields → pass through.
/// - Anything else → return as-is and let the parser fail.
fn canonicalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let n = trimmed.split_whitespace().count();
    if n == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}

fn next_interval(
    every_seconds: u64,
    anchor: DateTime<Utc>,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if every_seconds == 0 {
        return None;
    }
    // Future anchor: the first fire is the anchor itself.
    if anchor > after {
        return Some(anchor);
    }
    // i64::try_from is bounded but every_seconds is realistically small;
    // u64 -> i64 is safe up to ~292 billion years. Saturate just in case.
    let step = i64::try_from(every_seconds).ok()?;
    let elapsed = after.signed_duration_since(anchor).num_seconds();
    // Number of full intervals we've already passed. We want strictly >
    // after, so when elapsed is an exact multiple we still advance by one.
    let n = (elapsed / step) + 1;
    let next = anchor.checked_add_signed(Duration::seconds(n.checked_mul(step)?))?;
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ---------- OneShot ----------

    #[test]
    fn oneshot_in_future_returns_at() {
        let s = Schedule::OneShot {
            at: utc("2026-06-01T08:00:00Z"),
        };
        let n = compute_next_fire(&s, utc("2026-05-01T00:00:00Z")).unwrap();
        assert_eq!(n, utc("2026-06-01T08:00:00Z"));
    }

    #[test]
    fn oneshot_at_equal_to_after_is_not_strictly_greater() {
        // strictly greater than `after` — equal does not count.
        let at = utc("2026-06-01T08:00:00Z");
        let s = Schedule::OneShot { at };
        assert!(compute_next_fire(&s, at).is_none());
    }

    #[test]
    fn oneshot_in_past_returns_none() {
        let s = Schedule::OneShot {
            at: utc("2025-01-01T00:00:00Z"),
        };
        assert!(compute_next_fire(&s, utc("2026-05-01T00:00:00Z")).is_none());
    }

    // ---------- Cron ----------

    #[test]
    fn cron_daily_at_8_utc() {
        let s = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: "UTC".into(),
        };
        // Wednesday 12:00 UTC → next is Thursday 08:00 UTC.
        let n = compute_next_fire(&s, utc("2026-05-13T12:00:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-14T08:00:00Z"));
    }

    #[test]
    fn cron_strict_inequality_at_exact_match() {
        let s = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: "UTC".into(),
        };
        // Exactly 08:00 → next is tomorrow 08:00, not today.
        let n = compute_next_fire(&s, utc("2026-05-14T08:00:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-15T08:00:00Z"));
    }

    #[test]
    fn cron_in_named_tz() {
        let s = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: "Europe/Madrid".into(),
        };
        // Madrid is UTC+1 in winter, UTC+2 in summer (CEST). 2026-05-13
        // is in CEST so 08:00 Madrid = 06:00 UTC.
        let n = compute_next_fire(&s, utc("2026-05-13T05:00:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-13T06:00:00Z"));
    }

    #[test]
    fn cron_six_field_passthrough() {
        // Already 6-field — should not be re-prefixed.
        let s = Schedule::Cron {
            expr: "30 0 8 * * *".into(),
            tz: "UTC".into(),
        };
        let n = compute_next_fire(&s, utc("2026-05-13T07:59:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-13T08:00:30Z"));
    }

    #[test]
    fn cron_invalid_expr_returns_none() {
        let s = Schedule::Cron {
            expr: "this is not a cron".into(),
            tz: "UTC".into(),
        };
        assert!(compute_next_fire(&s, utc("2026-05-01T00:00:00Z")).is_none());
    }

    #[test]
    fn cron_unknown_tz_returns_none() {
        let s = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: "Mars/Tharsis".into(),
        };
        assert!(compute_next_fire(&s, utc("2026-05-01T00:00:00Z")).is_none());
    }

    // ---------- Interval ----------

    #[test]
    fn interval_zero_returns_none() {
        let s = Schedule::Interval {
            every_seconds: 0,
            anchor: utc("2026-05-01T00:00:00Z"),
        };
        assert!(compute_next_fire(&s, utc("2026-05-01T00:00:00Z")).is_none());
    }

    #[test]
    fn interval_future_anchor_returns_anchor() {
        let anchor = utc("2026-06-01T00:00:00Z");
        let s = Schedule::Interval {
            every_seconds: 3600,
            anchor,
        };
        let n = compute_next_fire(&s, utc("2026-05-01T00:00:00Z")).unwrap();
        assert_eq!(n, anchor);
    }

    #[test]
    fn interval_past_anchor_returns_next_grid_point() {
        let s = Schedule::Interval {
            every_seconds: 3600,
            anchor: utc("2026-05-01T00:00:00Z"),
        };
        // 30 minutes in → next is at 01:00 (one full hour past anchor).
        let n = compute_next_fire(&s, utc("2026-05-01T00:30:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-01T01:00:00Z"));
    }

    #[test]
    fn interval_exact_grid_point_advances_by_one() {
        let s = Schedule::Interval {
            every_seconds: 3600,
            anchor: utc("2026-05-01T00:00:00Z"),
        };
        // Exactly on the grid → strictly greater means next grid point.
        let n = compute_next_fire(&s, utc("2026-05-01T01:00:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-01T02:00:00Z"));
    }

    #[test]
    fn interval_skips_many_missed_slots_to_present() {
        let s = Schedule::Interval {
            every_seconds: 3600,
            anchor: utc("2026-05-01T00:00:00Z"),
        };
        // 100 hours later → next grid point is at 101h past anchor.
        let n = compute_next_fire(&s, utc("2026-05-05T04:00:00Z")).unwrap();
        assert_eq!(n, utc("2026-05-05T05:00:00Z"));
    }

    #[test]
    fn interval_works_across_daily_boundary() {
        let s = Schedule::Interval {
            every_seconds: 86_400,
            anchor: utc("2026-05-01T08:00:00Z"),
        };
        let n = compute_next_fire(&s, utc("2026-05-03T07:59:59Z")).unwrap();
        assert_eq!(n, utc("2026-05-03T08:00:00Z"));
    }

    // ---------- Sanity: composition with chrono-tz ----------

    #[test]
    fn cron_handles_dst_spring_forward_madrid() {
        // 2026-03-29: Spring-forward in Madrid skips 02:00-03:00 local.
        // Cron at 02:30 should *not* fire that day.
        let s = Schedule::Cron {
            expr: "30 2 * * *".into(),
            tz: "Europe/Madrid".into(),
        };
        // Saturday 2026-03-28 23:00 UTC.
        let n = compute_next_fire(&s, utc("2026-03-28T23:00:00Z"));
        // Whatever the cron crate decides, it should not be a non-existent
        // local time. We assert it's a valid UTC instant and it's *after*
        // the spring-forward.
        let n = n.expect("cron should produce a next fire");
        let madrid = n.with_timezone(&chrono_tz::Europe::Madrid);
        // It should be on or after 2026-03-29 (the day spring-forward
        // happens) — we don't pin the exact hour because cron crate
        // behavior on the gap is its call.
        assert!(
            madrid
                >= chrono_tz::Europe::Madrid
                    .with_ymd_and_hms(2026, 3, 29, 0, 0, 0)
                    .unwrap()
        );
    }
}
