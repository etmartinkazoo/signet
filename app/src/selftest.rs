//! A scripted run through the real window, for checking a build without a person at the
//! keyboard: `SIGNET_SELFTEST=<dir>` adds an event, changes one occurrence of a series and
//! deletes an event through the same dialogs a person uses, saves a PNG of the window after each
//! step into `<dir>`, writes `report.txt` there and quits. Run it under `GDK_BACKEND=broadway`
//! to keep it off the screen. It writes to the calendars it finds: use a test account.

use crate::window::Window;
use chrono::{Duration, Local, NaiveTime};
use gtk4::{self as gtk, glib, prelude::*};
use libadwaita::{self as adw, prelude::*};
use std::path::PathBuf;
use std::rc::Rc;

pub const ENV: &str = "SIGNET_SELFTEST";

pub fn start(window: &Rc<Window>, dir: PathBuf) {
    let window = Rc::clone(window);
    glib::spawn_future_local(async move {
        let mut report = Vec::new();
        let outcome = script(&window, &dir, &mut report).await;
        report.push(match outcome {
            Ok(()) => "PASS".to_owned(),
            Err(error) => format!("FAIL: {error}"),
        });
        let _ = std::fs::write(dir.join("report.txt"), report.join("\n") + "\n");
        if let Some(app) = window.window.application() {
            app.quit();
        }
    });
}

async fn pause(ms: u64) {
    glib::timeout_future(std::time::Duration::from_millis(ms)).await;
}

async fn settle(window: &Window) -> Result<(), String> {
    for _ in 0..150 {
        pause(100).await;
        if !window.is_refreshing() && !window.is_writing() {
            pause(200).await;
            return Ok(());
        }
    }
    Err("timed out waiting for the network".to_owned())
}

/// Saves the window as it is drawn, waiting for a finished frame when one is under way.
async fn shot(window: &Window, dir: &std::path::Path, name: &str) {
    // The window's child: libadwaita's dialog host, which draws the content and any dialog.
    let Some(widget) = window.window.first_child() else { return };
    for _ in 0..40 {
        let paintable = gtk::WidgetPaintable::new(Some(&widget));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, f64::from(widget.width()), f64::from(widget.height()));
        let node = snapshot.to_node();
        let renderer = widget.native().and_then(|native| native.renderer());
        if let (Some(node), Some(renderer)) = (node, renderer) {
            let texture = renderer.render_texture(&node, None);
            if let Err(error) = texture.save_to_png(dir.join(format!("{name}.png"))) {
                eprintln!("selftest: {name}: {error}");
            }
            return;
        }
        pause(50).await;
    }
    eprintln!("selftest: {name}: nothing drawn");
}

fn descendants(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut found = Vec::new();
    let mut child = root.first_child();
    while let Some(widget) = child {
        found.push(widget.clone());
        found.extend(descendants(&widget));
        child = widget.next_sibling();
    }
    found
}

fn dialog(window: &Window) -> Result<adw::Dialog, String> {
    window.window.visible_dialog().ok_or_else(|| "no dialog is open".to_owned())
}

fn entry(dialog: &adw::Dialog, title: &str) -> Result<adw::EntryRow, String> {
    descendants(dialog.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<adw::EntryRow>().ok())
        .find(|row| row.title() == title)
        .ok_or_else(|| format!("no {title} field"))
}

fn button(dialog: &adw::Dialog, label: &str) -> Result<gtk::Button, String> {
    descendants(dialog.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .find(|b| b.label().as_deref() == Some(label))
        .ok_or_else(|| format!("no {label} button"))
}

/// Answers the alert that is open with `response`.
fn answer(window: &Window, response: &str) -> Result<(), String> {
    let alert = dialog(window)?
        .downcast::<adw::AlertDialog>()
        .map_err(|_| "the open dialog isn't a question".to_owned())?;
    alert.emit_by_name::<()>("response", &[&response]);
    alert.close();
    Ok(())
}

fn find_ics(window: &Window, needle: &str) -> Vec<String> {
    window
        .service()
        .store
        .resources()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.ics.contains(needle))
        .map(|r| r.ics)
        .collect()
}

async fn script(window: &Window, dir: &std::path::Path, report: &mut Vec<String>) -> Result<(), String> {
    settle(window).await?;
    shot(window, dir, "01-week").await;
    report.push(format!("week shows {} events", window.shown_count()));

    // Add.
    let today = Local::now().date_naive();
    let start = today.and_time(NaiveTime::from_hms_opt(15, 0, 0).expect("a time"));
    window.create(start, start + Duration::hours(1), false);
    pause(1000).await;
    let editor = dialog(window)?;
    entry(&editor, "Title")?.set_text("Dentist");
    entry(&editor, "Location")?.set_text("Main St");
    entry(&editor, "Video Call Link")?.set_text("https://meet.google.com/abc-defg-hij");
    let invite = entry(&editor, "Add Invitee by Email")?;
    invite.set_text("grace@example.com");
    invite.emit_by_name::<()>("apply", &[]);
    invite.set_text("charles@example.com");
    shot(window, dir, "02-new-event").await;
    button(&editor, "Save")?.emit_clicked();
    settle(window).await?;
    shot(window, dir, "03-added").await;
    let added = find_ics(window, "SUMMARY:Dentist");
    let added = added.first().ok_or("the new event isn't stored")?;
    let parsed = signet_core::ics::parse(added).map_err(|e| e.to_string())?;
    let event = parsed
        .children_named("VEVENT")
        .next()
        .and_then(|e| signet_core::event::Event::from_component(e, window.service().local))
        .ok_or("the new event can't be read")?;
    let invited: Vec<&str> = event.attendees.iter().map(|a| a.email.as_str()).collect();
    if invited != ["grace@example.com", "charles@example.com"] {
        return Err(format!("the new event invites {invited:?}"));
    }
    if event.organizer.is_none() || event.conference.as_deref() != Some("https://meet.google.com/abc-defg-hij") {
        return Err("the new event has no organizer or video call".to_owned());
    }
    report.push("added Dentist with two invitees and a video call".to_owned());

    // Change one occurrence of a series.
    let standup = window
        .item_named("Standup")
        .ok_or("no Standup in this week")?;
    let day = standup.occurrence.start.date_naive();
    window.open(standup);
    pause(1000).await;
    let editor = dialog(window)?;
    shot(window, dir, "04-edit-recurring").await;
    entry(&editor, "Title")?.set_text("Standup (moved)");
    button(&editor, "Save")?.emit_clicked();
    pause(1000).await;
    shot(window, dir, "05-which-events").await;
    answer(window, "this")?;
    settle(window).await?;
    shot(window, dir, "06-one-changed").await;
    let series = find_ics(window, "UID:standup");
    let changed = series.first().ok_or("the series is gone")?;
    if !changed.contains("RECURRENCE-ID") || !changed.contains("Standup (moved)") || !changed.contains("RRULE") {
        return Err("the occurrence wasn't changed on its own".to_owned());
    }
    report.push(format!("changed the Standup of {day} only"));

    // Delete.
    let lunch = window.item_named("Lunch with Charles").ok_or("no lunch in this week")?;
    window.open(lunch);
    pause(1000).await;
    let editor = dialog(window)?;
    button(&editor, "Delete Event")?.emit_clicked();
    pause(1000).await;
    shot(window, dir, "07-confirm-delete").await;
    answer(window, "delete")?;
    settle(window).await?;
    shot(window, dir, "08-deleted").await;
    if !find_ics(window, "UID:lunch").is_empty() {
        return Err("the deleted event is still stored".to_owned());
    }
    report.push("deleted Lunch with Charles".to_owned());

    // Navigate.
    WidgetExt::activate_action(&window.window, "win.next", None).map_err(|e| e.to_string())?;
    settle(window).await?;
    shot(window, dir, "09-next-week").await;
    report.push(format!("next week shows {} events", window.shown_count()));
    Ok(())
}
