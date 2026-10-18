//! Exercises the CalDAV client against a real server: discovery, the calendar list, and an
//! event created, read, changed and removed. For a test server only; it writes to the first
//! calendar it finds.
//!
//! `SIGNET_TEST_ROOTS=cert.pem cargo run -p signet-core --example caldav_live -- URL USER PASSWORD`

use chrono::{Duration, Utc};
use signet_core::caldav::{Client, Condition, Credentials};
use signet_core::event::{Draft, new_calendar, occurrences};
use signet_core::ics;
use zeroize::Zeroizing;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [url, user, password] = args.as_slice() else {
        anyhow::bail!("usage: caldav_live URL USER PASSWORD");
    };
    let client = Client::new(Credentials::Password { username: user.clone(), password: Zeroizing::new(password.clone()) })?;
    let home = client.discover(&url.parse()?).await?;
    println!("home: {home}");
    let calendars = client.calendars(&home).await?;
    for calendar in &calendars {
        println!("calendar: {} {:?} ctag={:?} ro={}", calendar.name, calendar.color, calendar.ctag, calendar.read_only);
    }
    let calendar = calendars.first().ok_or_else(|| anyhow::anyhow!("no calendar"))?;
    let local = signet_core::event::local_zone();
    let start = (Utc::now() + Duration::days(1)).with_timezone(&local).naive_local();
    let draft = Draft {
        summary: "Signet live check".into(),
        location: "Here".into(),
        description: "first line\nsecond, line".into(),
        attendees: Vec::new(),
        conference: String::new(),
        organizer: None,
        all_day: false,
        start,
        end: start + Duration::hours(1),
    };
    let uid = uuid::Uuid::new_v4().to_string();
    let ics_text = new_calendar(&uid, &draft, local).to_ics();
    let url = calendar.url.join(&format!("{uid}.ics"))?;
    let etag = client.put(&url, &ics_text, Condition::Create).await?;
    println!("created, etag {etag:?}");
    assert!(client.put(&url, &ics_text, Condition::Create).await.is_err(), "a second create must fail");

    let (from, to) = (Utc::now(), Utc::now() + Duration::days(3));
    let found = client.events(&calendar.url, from, to).await?;
    let ours = found.iter().find(|r| r.url == url).ok_or_else(|| anyhow::anyhow!("created event not listed"))?;
    let parsed = ics::parse(&ours.ics)?;
    let shown = occurrences(&parsed, from.with_timezone(&local), to.with_timezone(&local), local);
    assert_eq!(shown[0].description, "first line\nsecond, line");
    println!("read back: {} at {}", shown[0].summary, shown[0].start);

    let changed = ours.ics.replace("Signet live check", "Signet live check (edited)");
    let current = ours.etag.clone().or(etag).ok_or_else(|| anyhow::anyhow!("no etag"))?;
    assert!(
        matches!(client.put(&url, &changed, Condition::Match("\"stale\"")).await, Err(signet_core::caldav::Error::Changed)),
        "a stale etag must be refused"
    );
    let new_etag = client.put(&url, &changed, Condition::Match(&current)).await?;
    println!("updated, etag {new_etag:?}");
    client.delete(&url, new_etag.as_deref()).await?;
    let after = client.events(&calendar.url, from, to).await?;
    assert!(after.iter().all(|r| r.url != url), "deleted event still listed");
    println!("deleted; all checks passed");
    Ok(())
}
