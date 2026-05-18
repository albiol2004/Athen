//! End-to-end smoke test for the CalDAV adapter.
//!
//! Run against any CalDAV server using credentials from the environment:
//!
//! ```bash
//! ATHEN_CALDAV_URL='https://caldav.icloud.com/' \
//! ATHEN_CALDAV_USER='alex@me.com' \
//! ATHEN_CALDAV_PASSWORD='xxxx-xxxx-xxxx-xxxx' \
//! cargo run -p athen-caldav --example smoke
//! ```
//!
//! Pass `--write` as the first arg to round-trip a single test event
//! (create → read → delete) in the first writable calendar:
//!
//! ```bash
//! cargo run -p athen-caldav --example smoke -- --write
//! ```
//!
//! The example never touches the filesystem and never writes outside the
//! one event it creates and then immediately deletes. Output is plain
//! text so it can be diffed across runs.

use std::env;
use std::time::Duration;

use chrono::Utc;
use url::Url;

use athen_caldav::CalDavSource;
use athen_core::traits::calendar_source::{CalendarSource, RemoteEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,athen_caldav=debug")),
        )
        .init();

    let url = env::var("ATHEN_CALDAV_URL").map_err(|_| "set ATHEN_CALDAV_URL")?;
    let user = env::var("ATHEN_CALDAV_USER").map_err(|_| "set ATHEN_CALDAV_USER")?;
    let pass = env::var("ATHEN_CALDAV_PASSWORD").map_err(|_| "set ATHEN_CALDAV_PASSWORD")?;
    let want_write = env::args().any(|a| a == "--write");

    let source = CalDavSource::new(
        "smoke",
        format!("Smoke ({user})"),
        Url::parse(&url)?,
        &user,
        &pass,
    )?;

    println!("→ test_connection");
    source.test_connection().await?;
    println!("  OK");

    println!("→ list_calendars");
    let cals = source.list_calendars().await?;
    if cals.is_empty() {
        println!("  (no calendar collections returned)");
        return Ok(());
    }
    for c in &cals {
        println!(
            "  • {:<32} read_only={:<5} id={}",
            c.name, c.read_only, c.id
        );
    }

    let pick = cals.iter().find(|c| !c.read_only).unwrap_or(&cals[0]);
    println!("\n→ list_events on `{}` (next 7 days)", pick.name);
    let start = Utc::now();
    let end = start + chrono::Duration::days(7);
    let events = source.list_events(&pick.id, start, end).await?;
    println!("  {} event(s)", events.len());
    for e in events.iter().take(10) {
        println!(
            "  • {}  {}  {}",
            e.start_time.format("%Y-%m-%d %H:%MZ"),
            if e.all_day { "[all-day]" } else { "         " },
            truncate(&e.title, 60),
        );
    }
    if events.len() > 10 {
        println!("  ... (+{} more)", events.len() - 10);
    }

    if !want_write {
        println!("\n(skipping write round-trip — pass --write to enable)");
        return Ok(());
    }

    println!("\n→ create_event in `{}`", pick.name);
    let start = Utc::now() + chrono::Duration::minutes(5);
    let end = start + chrono::Duration::minutes(30);
    let new_event = RemoteEvent {
        remote_id: String::new(),
        calendar_id: pick.id.clone(),
        etag: None,
        ical_uid: None,
        title: "Athen CalDAV smoke test (safe to delete)".into(),
        description: Some(
            "Created by `cargo run -p athen-caldav --example smoke`. Auto-deleted moments later."
                .into(),
        ),
        start_time: start,
        end_time: end,
        all_day: false,
        categories: None,
        location: None,
        recurrence_rrule: None,
        reminder_minutes: vec![],
    };
    let (remote_id, etag) = source.create_event(&pick.id, &new_event).await?;
    println!("  created: id={remote_id} etag={etag:?}");

    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("→ list_events again (should see the new event)");
    let after = source
        .list_events(&pick.id, start - chrono::Duration::minutes(1), end)
        .await?;
    let found = after.iter().any(|e| e.remote_id == remote_id);
    println!("  found = {found}");

    println!("→ delete_event");
    source
        .delete_event(&pick.id, &remote_id, etag.as_deref())
        .await?;
    println!("  deleted");

    println!("\nSmoke test passed ✓");
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}
