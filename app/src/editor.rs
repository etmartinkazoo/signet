//! The event editor: one dialog for a new event and for changing or removing one.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use gtk4::{self as gtk, glib, prelude::*};
use libadwaita::{self as adw, prelude::*};
use signet_core::event::{Answer, Attendee, Draft};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct Request {
    pub heading: &'static str,
    pub draft: Draft,
    /// The calendars the event may be in: (id, label). Only its own when it can't move.
    pub calendars: Vec<(i64, String)>,
    pub calendar: i64,
    pub editable: bool,
    pub can_delete: bool,
    /// Shown above the fields, e.g. "Repeats; you'll choose which events change".
    pub note: Option<String>,
    pub organizer: Option<Attendee>,
    pub invitees: Vec<Attendee>,
    /// Whether the invitees can be changed: the event is the user's own, not an invitation.
    pub can_invite: bool,
}

#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    Save { draft: Draft, calendar: i64 },
    Delete,
}

type Listeners = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

/// The invitees: the organizer, each invitee with their answer and a remove button, and a
/// field that adds people by address.
#[derive(Clone)]
struct Invitees {
    group: adw::PreferencesGroup,
    list: Rc<RefCell<Vec<Attendee>>>,
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
    add: adw::EntryRow,
    organizer: Option<Attendee>,
    editable: bool,
}

impl Invitees {
    fn new(organizer: Option<Attendee>, invitees: Vec<Attendee>, editable: bool) -> Self {
        let group = adw::PreferencesGroup::builder().title("Invitees").build();
        let add = adw::EntryRow::builder()
            .title("Add Invitee by Email")
            .show_apply_button(true)
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        let this = Self {
            group,
            list: Rc::new(RefCell::new(invitees)),
            rows: Rc::default(),
            add,
            organizer,
            editable,
        };
        for signal in ["apply", "entry-activated"] {
            let invitees = this.clone();
            this.add.connect_local(signal, false, move |_| {
                invitees.take_typed();
                None
            });
        }
        this.rebuild();
        this
    }

    /// The addresses typed in the add field, or `None` when one isn't an address.
    fn typed(&self) -> Option<Vec<String>> {
        let text = self.add.text();
        let words: Vec<String> = text
            .split([',', ';', ' ', '\n'])
            .map(|word| word.trim().trim_matches(['<', '>']).to_owned())
            .filter(|word| !word.is_empty())
            .collect();
        words.iter().all(|word| signet_core::accounts::is_address(word)).then_some(words)
    }

    /// Moves what's typed into the list.
    fn take_typed(&self) {
        match self.typed() {
            Some(words) => {
                {
                    let mut list = self.list.borrow_mut();
                    for email in words {
                        if !list.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
                            list.push(Attendee { email, name: String::new(), answer: Answer::Waiting });
                        }
                    }
                }
                self.add.set_text("");
                self.add.remove_css_class("error");
                self.rebuild();
            }
            None => self.add.add_css_class("error"),
        }
    }

    fn rebuild(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        if self.add.parent().is_some() {
            self.group.remove(&self.add);
        }
        let mut rows = Vec::new();
        if let Some(organizer) = self.organizer.as_ref().filter(|_| !self.list.borrow().is_empty()) {
            let row = person_row(organizer, "Organizer");
            rows.push(row.upcast());
        }
        for (index, attendee) in self.list.borrow().iter().enumerate() {
            let row = person_row(attendee, attendee.answer.label());
            if self.editable {
                let remove = gtk::Button::builder()
                    .icon_name("list-remove-symbolic")
                    .tooltip_text("Remove Invitee")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let invitees = self.clone();
                remove.connect_clicked(move |_| {
                    invitees.list.borrow_mut().remove(index);
                    invitees.rebuild();
                });
                row.add_suffix(&remove);
            }
            rows.push(row.upcast());
        }
        for row in &rows {
            self.group.add(row);
        }
        if self.editable {
            self.group.add(&self.add);
        }
        self.group.set_visible(self.editable || !rows.is_empty());
        *self.rows.borrow_mut() = rows;
    }

    /// Everyone invited, including an address typed but not yet added.
    fn emails(&self) -> Vec<String> {
        let mut emails: Vec<String> = self.list.borrow().iter().map(|a| a.email.clone()).collect();
        for email in self.typed().unwrap_or_default() {
            if !emails.iter().any(|e| e.eq_ignore_ascii_case(&email)) {
                emails.push(email);
            }
        }
        emails
    }
}

fn person_row(person: &Attendee, status: &str) -> adw::ActionRow {
    let subtitle = if person.name.is_empty() { status.to_owned() } else { format!("{} · {status}", person.email) };
    adw::ActionRow::builder()
        .title(person.display())
        .subtitle(subtitle)
        .use_markup(false)
        .build()
}

/// The video call's link, with a button that opens it.
fn conference_row(link: &str, editable: bool) -> adw::EntryRow {
    let row = adw::EntryRow::builder()
        .title("Video Call Link")
        .text(link)
        .input_purpose(gtk::InputPurpose::Url)
        .editable(editable)
        .build();
    let join = gtk::Button::builder()
        .label("Join")
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .tooltip_text("Open the Call")
        .build();
    let joinable = |text: &str| text.trim().starts_with("https://");
    join.set_sensitive(joinable(link));
    row.add_suffix(&join);
    let button = join.clone();
    row.connect_changed(move |row| button.set_sensitive(joinable(&row.text())));
    let entry = row.downgrade();
    join.connect_clicked(move |button| {
        let Some(row) = entry.upgrade() else { return };
        let window = button.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(row.text().trim()).launch(window.as_ref(), gtk4::gio::Cancellable::NONE, |result| {
            if let Err(error) = result {
                tracing::warn!("couldn't open the call: {error}");
            }
        });
    });
    row
}

/// A quiet caption ("Starts") on its own line over a date button with a month popover, and hour
/// and minute fields.
#[derive(Clone)]
struct DateTimeRow {
    row: gtk::ListBoxRow,
    date: Rc<Cell<NaiveDate>>,
    button: gtk::MenuButton,
    calendar: gtk::Calendar,
    hour: gtk::SpinButton,
    minute: gtk::SpinButton,
    time: gtk::Box,
    changed: Listeners,
    quiet: Rc<Cell<bool>>,
}

fn two_digits(spin: &gtk::SpinButton) -> glib::Propagation {
    spin.set_text(&format!("{:02}", spin.value() as i64));
    glib::Propagation::Stop
}

impl DateTimeRow {
    fn new(title: &str, value: NaiveDateTime) -> Self {
        let calendar = gtk::Calendar::new();
        let popover = gtk::Popover::builder().child(&calendar).build();
        let button = gtk::MenuButton::builder().popover(&popover).valign(gtk::Align::Center).build();
        let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
        let minute = gtk::SpinButton::with_range(0.0, 59.0, 5.0);
        for spin in [&hour, &minute] {
            spin.set_wrap(true);
            spin.set_numeric(true);
            spin.set_width_chars(2);
            spin.set_valign(gtk::Align::Center);
            spin.connect_output(two_digits);
        }
        let time = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        time.append(&hour);
        time.append(&gtk::Label::new(Some(":")));
        time.append(&minute);
        button.set_halign(gtk::Align::Start);
        button.set_hexpand(true);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.append(&button);
        controls.append(&time);
        let caption = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["caption", "dimmed"])
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&caption);
        content.append(&controls);
        let row = gtk::ListBoxRow::builder().activatable(false).selectable(false).child(&content).build();
        row.update_property(&[gtk::accessible::Property::Label(title)]);
        let this = Self {
            row,
            date: Rc::new(Cell::new(value.date())),
            button,
            calendar,
            hour,
            minute,
            time,
            changed: Rc::default(),
            quiet: Rc::new(Cell::new(false)),
        };
        this.set(value);
        let row = this.clone();
        this.calendar.connect_day_selected(move |calendar| {
            let picked = calendar.date();
            if let Some(date) = NaiveDate::from_ymd_opt(picked.year(), picked.month() as u32, picked.day_of_month() as u32) {
                row.date.set(date);
                row.button.set_label(&date.format("%a %-d %b %Y").to_string());
                row.button.popdown();
                row.emit();
            }
        });
        for spin in [&this.hour, &this.minute] {
            let row = this.clone();
            spin.connect_value_changed(move |_| row.emit());
        }
        this
    }

    fn emit(&self) {
        if !self.quiet.get() {
            for callback in self.changed.borrow().iter() {
                callback();
            }
        }
    }

    fn connect_changed(&self, callback: impl Fn() + 'static) {
        self.changed.borrow_mut().push(Box::new(callback));
    }

    fn get(&self) -> NaiveDateTime {
        let time = NaiveTime::from_hms_opt(self.hour.value() as u32, self.minute.value() as u32, 0).unwrap_or(NaiveTime::MIN);
        self.date.get().and_time(time)
    }

    /// Sets the value without telling listeners.
    fn set(&self, value: NaiveDateTime) {
        self.quiet.set(true);
        self.date.set(value.date());
        self.button.set_label(&value.date().format("%a %-d %b %Y").to_string());
        if let Ok(day) = glib::DateTime::from_local(value.year(), value.month() as i32, value.day() as i32, 12, 0, 0.0) {
            self.calendar.select_day(&day);
        }
        self.hour.set_value(f64::from(value.hour()));
        self.minute.set_value(f64::from(value.minute()));
        self.quiet.set(false);
    }

    fn set_time_visible(&self, visible: bool) {
        self.time.set_visible(visible);
    }

    fn set_sensitive(&self, sensitive: bool) {
        self.button.set_sensitive(sensitive);
        self.hour.set_sensitive(sensitive);
        self.minute.set_sensitive(sensitive);
    }
}

pub fn open(parent: &impl IsA<gtk::Widget>, request: Request, done: impl Fn(Outcome) + 'static) {
    let dialog = adw::Dialog::builder().title(request.heading).content_width(480).build();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    if request.editable {
        header.pack_end(&save);
    }

    let page = adw::PreferencesPage::new();
    if let Some(note) = &request.note {
        let group = adw::PreferencesGroup::builder().description(note.as_str()).build();
        page.add(&group);
    }

    let details = adw::PreferencesGroup::new();
    let title = adw::EntryRow::builder().title("Title").text(request.draft.summary.as_str()).build();
    let location = adw::EntryRow::builder().title("Location").text(request.draft.location.as_str()).build();
    let conference = conference_row(&request.draft.conference, request.editable);
    details.add(&title);
    details.add(&location);
    details.add(&conference);
    page.add(&details);

    let times = adw::PreferencesGroup::new();
    let all_day = adw::SwitchRow::builder().title("All Day").active(request.draft.all_day).build();
    let starts = DateTimeRow::new("Starts", request.draft.start);
    let ends = DateTimeRow::new("Ends", request.draft.end);
    times.add(&all_day);
    times.add(&starts.row);
    times.add(&ends.row);
    page.add(&times);

    let place = adw::PreferencesGroup::new();
    let labels: Vec<&str> = request.calendars.iter().map(|(_, label)| label.as_str()).collect();
    let calendar_row = adw::ComboRow::builder().title("Calendar").model(&gtk::StringList::new(&labels)).build();
    if let Some(index) = request.calendars.iter().position(|(id, _)| *id == request.calendar) {
        calendar_row.set_selected(index as u32);
    }
    place.add(&calendar_row);
    page.add(&place);

    let invitees = Invitees::new(request.organizer.clone(), request.invitees.clone(), request.editable && request.can_invite);
    if request.editable && !request.can_invite {
        invitees.group.set_description(Some("You were invited: only the organizer can change who's invited."));
    }
    page.add(&invitees.group);

    let notes_group = adw::PreferencesGroup::builder().title("Notes").build();
    let notes = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .accepts_tab(false)
        .build();
    notes.buffer().set_text(&request.draft.description);
    let frame = gtk::Frame::builder().child(&notes).height_request(110).css_classes(["card"]).build();
    notes_group.add(&frame);
    page.add(&notes_group);

    let editable = request.editable;
    for widget in [title.upcast_ref::<gtk::Widget>(), location.upcast_ref(), all_day.upcast_ref(), calendar_row.upcast_ref()] {
        widget.set_sensitive(editable);
    }
    notes.set_editable(editable);
    starts.set_sensitive(editable);
    ends.set_sensitive(editable);
    if request.calendars.len() < 2 {
        calendar_row.set_sensitive(false);
    }

    if request.can_delete && editable {
        let group = adw::PreferencesGroup::new();
        // A button of the header's height, not a full-height list row.
        let delete = gtk::Button::builder()
            .label("Delete Event")
            .halign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        group.add(&delete);
        page.add(&group);
        let dialog_ref = dialog.downgrade();
        let done_delete = Rc::new(done);
        let done_save = Rc::clone(&done_delete);
        delete.connect_clicked(move |_| {
            if let Some(dialog) = dialog_ref.upgrade() {
                dialog.close();
            }
            done_delete(Outcome::Delete);
        });
        let fields = Fields { title: title.clone(), location: location.clone(), conference: conference.clone(), invitees: invitees.clone() };
        wire_save(&dialog, &save, fields, &all_day, &starts, &ends, &calendar_row, &notes, &request, move |o| done_save(o));
    } else {
        let fields = Fields { title: title.clone(), location: location.clone(), conference: conference.clone(), invitees: invitees.clone() };
        wire_save(&dialog, &save, fields, &all_day, &starts, &ends, &calendar_row, &notes, &request, done);
    }

    // Times follow the switch; the end keeps the event's length when the start moves.
    let sync_switch = {
        let (starts, ends) = (starts.clone(), ends.clone());
        move |all_day: bool| {
            starts.set_time_visible(!all_day);
            ends.set_time_visible(!all_day);
        }
    };
    sync_switch(request.draft.all_day);
    all_day.connect_active_notify(move |row| sync_switch(row.is_active()));
    let previous = Rc::new(Cell::new(starts.get()));
    {
        let (starts_c, ends_c) = (starts.clone(), ends.clone());
        let previous = Rc::clone(&previous);
        starts.connect_changed(move || {
            let now = starts_c.get();
            let shift = now - previous.get();
            previous.set(now);
            ends_c.set(ends_c.get() + shift);
            ends_c.emit();
        });
    }
    let validate = {
        let (starts, ends, all_day, save, ends_row) = (starts.clone(), ends.clone(), all_day.clone(), save.clone(), ends.row.clone());
        move || {
            let valid = if all_day.is_active() {
                ends.get().date() >= starts.get().date()
            } else {
                ends.get() >= starts.get()
            };
            save.set_sensitive(valid);
            if valid {
                ends_row.remove_css_class("error");
            } else {
                ends_row.add_css_class("error");
            }
        }
    };
    let validate = Rc::new(validate);
    for row in [&starts, &ends] {
        let validate = Rc::clone(&validate);
        row.connect_changed(move || validate());
    }
    {
        let validate = Rc::clone(&validate);
        all_day.connect_active_notify(move |_| validate());
    }
    validate();

    let dialog_ref = dialog.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(dialog) = dialog_ref.upgrade() {
            dialog.close();
        }
    });

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&save));
    dialog.present(Some(parent));
    if editable {
        title.grab_focus();
    }
}

/// The text fields a save reads.
struct Fields {
    title: adw::EntryRow,
    location: adw::EntryRow,
    conference: adw::EntryRow,
    invitees: Invitees,
}

#[allow(clippy::too_many_arguments)]
fn wire_save(
    dialog: &adw::Dialog,
    save: &gtk::Button,
    fields: Fields,
    all_day: &adw::SwitchRow,
    starts: &DateTimeRow,
    ends: &DateTimeRow,
    calendar_row: &adw::ComboRow,
    notes: &gtk::TextView,
    request: &Request,
    done: impl Fn(Outcome) + 'static,
) {
    let calendars: Vec<i64> = request.calendars.iter().map(|(id, _)| *id).collect();
    let fallback = request.calendar;
    let Fields { title, location, conference, invitees } = fields;
    let (all_day, starts, ends, calendar_row, notes) =
        (all_day.clone(), starts.clone(), ends.clone(), calendar_row.clone(), notes.clone());
    let dialog_ref = dialog.downgrade();
    let title_entry = title.clone();
    let submit = Rc::new(move || {
        let buffer = notes.buffer();
        let description = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
        let all_day = all_day.is_active();
        let start = starts.get();
        let mut end = ends.get();
        if !all_day && end == start {
            end = start + Duration::minutes(30);
        }
        let draft = Draft {
            summary: title.text().trim().to_owned(),
            location: location.text().trim().to_owned(),
            description: description.trim_end().to_owned(),
            attendees: invitees.emails(),
            conference: conference.text().trim().to_owned(),
            organizer: None,
            all_day,
            start,
            end,
        };
        let calendar = calendars.get(calendar_row.selected() as usize).copied().unwrap_or(fallback);
        if let Some(dialog) = dialog_ref.upgrade() {
            dialog.close();
        }
        done(Outcome::Save { draft, calendar });
    });
    let on_click = Rc::clone(&submit);
    save.connect_clicked(move |_| on_click());
    // Enter in the title saves, as in a form.
    let save_ref = save.downgrade();
    title_entry.connect_entry_activated(move |_| {
        if save_ref.upgrade().is_some_and(|save| save.is_sensitive() && save.parent().is_some()) {
            submit();
        }
    });
}
