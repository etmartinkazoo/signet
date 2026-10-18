//! The main window: a sidebar with the month and the calendars to show, and the week view.

use crate::editor::{self, Outcome, Request};
use crate::runtime::run;
use crate::theme::{self, Palette, Rgb};
use crate::week::{Item, WeekView};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use gtk4::{self as gtk, gdk, gio, glib, prelude::*};
use libadwaita::{self as adw, prelude::*};
use signet_core::accounts;
use signet_core::event::{self, Answer, Attendee, Draft, Removal, Scope};
use signet_core::ics::{self, Component};
use signet_core::service::{Problem, Service};
use signet_core::store::Calendar;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

struct Parsed {
    resource: i64,
    calendar: i64,
    component: Component,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    service: Arc<Service>,
    week: WeekView,
    title: adw::WindowTitle,
    month: gtk::Calendar,
    month_quiet: Cell<bool>,
    calendar_list: gtk::Box,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    refresh: gtk::Button,
    spinner: adw::Spinner,
    parsed: RefCell<Vec<Parsed>>,
    calendars: RefCell<Vec<Calendar>>,
    first: Cell<NaiveDate>,
    palette: RefCell<Palette>,
    problems: RefCell<Vec<Problem>>,
    refreshing: Cell<bool>,
    last_calendar: Cell<Option<i64>>,
}

impl Window {
    pub fn new(app: &adw::Application, service: Arc<Service>, warnings: Vec<String>) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Signet")
            .default_width(1200)
            .default_height(800)
            .build();
        let palette = Palette::fallback(adw::StyleManager::default().is_dark());
        let week = WeekView::new(palette.clone(), service.local);

        // Sidebar: the month, then the calendars by account.
        let month = gtk::Calendar::builder().margin_start(12).margin_end(12).margin_top(6).build();
        month.add_css_class("card");
        let calendar_list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        calendar_list.set_margin_start(12);
        calendar_list.set_margin_end(12);
        calendar_list.set_margin_top(12);
        calendar_list.set_margin_bottom(12);
        let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        sidebar_box.append(&month);
        sidebar_box.append(&calendar_list);
        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&sidebar_box)
            .vexpand(true)
            .build();
        let sidebar_header = adw::HeaderBar::builder()
            .show_end_title_buttons(false)
            .title_widget(&adw::WindowTitle::new("Signet", ""))
            .build();
        let sidebar = adw::ToolbarView::new();
        sidebar.add_top_bar(&sidebar_header);
        sidebar.set_content(Some(&sidebar_scroll));

        // Content: navigation, the banner for accounts that couldn't refresh, the week.
        let title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        let toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Show Sidebar")
            .active(true)
            .build();
        let previous = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous Week (←)").action_name("win.previous").build();
        let today = gtk::Button::builder().label("Today").tooltip_text("Today (T)").action_name("win.today").build();
        let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next Week (→)").action_name("win.next").build();
        let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        navigation.add_css_class("linked");
        navigation.append(&previous);
        navigation.append(&today);
        navigation.append(&next);
        header.pack_start(&toggle);
        header.pack_start(&navigation);
        let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("New Event (N)").action_name("win.new").build();
        let refresh = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh (R)").action_name("win.refresh").build();
        let spinner = adw::Spinner::builder().visible(false).margin_start(8).margin_end(8).build();
        header.pack_end(&add);
        header.pack_end(&refresh);
        header.pack_end(&spinner);
        let banner = adw::Banner::builder().button_label("Details").build();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&week.root));
        let content = adw::ToolbarView::new();
        content.add_top_bar(&header);
        content.add_top_bar(&banner);
        content.set_content(Some(&toasts));

        let split = adw::OverlaySplitView::builder()
            .sidebar(&sidebar)
            .content(&content)
            .min_sidebar_width(260.0)
            .max_sidebar_width(300.0)
            .build();
        toggle.bind_property("active", &split, "show-sidebar").bidirectional().sync_create().build();
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            760.0,
            adw::LengthUnit::Sp,
        ));
        narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(narrow);
        window.set_content(Some(&split));

        let this = Rc::new(Self {
            window,
            service,
            week,
            title,
            month,
            month_quiet: Cell::new(false),
            calendar_list,
            toasts,
            banner,
            refresh,
            spinner,
            parsed: RefCell::default(),
            calendars: RefCell::default(),
            first: Cell::new(Local::now().date_naive()),
            palette: RefCell::new(palette),
            problems: RefCell::default(),
            refreshing: Cell::new(false),
            last_calendar: Cell::new(None),
        });
        this.connect(&this);
        install_result_actions(&this);
        let weak = Rc::downgrade(&this);
        let palette = theme::follow(move |palette| {
            if let Some(this) = weak.upgrade() {
                this.week.set_palette(palette.clone());
                *this.palette.borrow_mut() = palette;
                this.reload();
            }
        });
        this.week.set_palette(palette.clone());
        *this.palette.borrow_mut() = palette;

        this.week.scroll_to_hour(this.service.config.day_starts.unwrap_or(7).min(23));
        this.go_to(Local::now().date_naive());
        this.reload();
        for warning in warnings {
            this.toast(&warning);
        }
        this.refresh_now(Local::now().date_naive());
        // Pick up changes made elsewhere; calendars whose tag is unchanged aren't read again.
        let weak = Rc::downgrade(&this);
        glib::timeout_add_seconds_local(300, move || match weak.upgrade() {
            Some(this) => {
                this.refresh_now(this.first.get());
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        this
    }

    fn connect(&self, this: &Rc<Self>) {
        let action = |name: &str, run: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(this);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    run(&this);
                }
            });
            self.window.add_action(&action);
        };
        action("previous", |this| this.go_to(this.first.get() - Duration::days(7)));
        action("next", |this| this.go_to(this.first.get() + Duration::days(7)));
        action("today", |this| this.go_to(Local::now().date_naive()));
        action("refresh", |this| this.refresh_now(this.first.get()));
        action("new", |this| {
            let now = Local::now().naive_local();
            let day = if (this.first.get()..this.first.get() + Duration::days(7)).contains(&now.date()) {
                now.date()
            } else {
                this.first.get()
            };
            let hour = if day == now.date() { (now.hour() + 1).min(23) } else { 9 };
            let start = day.and_time(NaiveTime::from_hms_opt(hour, 0, 0).unwrap_or(NaiveTime::MIN));
            this.create(start, start + Duration::hours(1), false);
        });

        // One-key shortcuts, as in Sealwax; they wait until no dialog or text field wants the key.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Bubble);
        let weak = Rc::downgrade(this);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if this.window.visible_dialog().is_some()
                || modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
            {
                return glib::Propagation::Proceed;
            }
            let name = match key {
                gdk::Key::Left | gdk::Key::h | gdk::Key::Page_Up | gdk::Key::k => "previous",
                gdk::Key::Right | gdk::Key::l | gdk::Key::Page_Down | gdk::Key::j => "next",
                gdk::Key::t | gdk::Key::Home => "today",
                gdk::Key::n => "new",
                gdk::Key::r | gdk::Key::F5 => "refresh",
                _ => return glib::Propagation::Proceed,
            };
            let _ = WidgetExt::activate_action(&this.window, &format!("win.{name}"), None);
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);

        let weak = Rc::downgrade(this);
        self.month.connect_day_selected(move |month| {
            let Some(this) = weak.upgrade() else { return };
            if this.month_quiet.get() {
                return;
            }
            let picked = month.date();
            if let Some(date) = NaiveDate::from_ymd_opt(picked.year(), picked.month() as u32, picked.day_of_month() as u32) {
                this.go_to(date);
            }
        });
        for signal in ["next-month", "prev-month", "next-year", "prev-year"] {
            let weak = Rc::downgrade(this);
            self.month.connect_local(signal, false, move |_| {
                if let Some(this) = weak.upgrade() {
                    this.mark_month();
                }
                None
            });
        }

        let weak = Rc::downgrade(this);
        self.week.connect_open(move |item| {
            if let Some(this) = weak.upgrade() {
                this.open(item);
            }
        });
        let weak = Rc::downgrade(this);
        self.week.connect_create(move |start, end, all_day| {
            if let Some(this) = weak.upgrade() {
                this.create(start, end, all_day);
            }
        });
        let weak = Rc::downgrade(this);
        self.banner.connect_button_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let sign_in = this.problems.borrow().iter().find_map(|p| p.sign_in.clone());
            match sign_in {
                Some(key) => this.sign_in(key),
                None => this.show_problems(),
            }
        });
    }

    pub(crate) fn service(&self) -> &Service {
        &self.service
    }

    pub(crate) fn is_refreshing(&self) -> bool {
        self.refreshing.get()
    }

    pub(crate) fn is_writing(&self) -> bool {
        WRITES.with(Cell::get) > 0
    }

    /// The first shown event with this title.
    pub(crate) fn item_named(&self, summary: &str) -> Option<Item> {
        let first = self.first.get();
        self.items(first, first + Duration::days(7)).into_iter().find(|item| item.occurrence.summary == summary)
    }

    pub(crate) fn shown_count(&self) -> usize {
        let first = self.first.get();
        self.items(first, first + Duration::days(7)).len()
    }

    fn week_start(&self, date: NaiveDate) -> NaiveDate {
        let offset = if self.service.config.week_starts_sunday() {
            date.weekday().num_days_from_sunday()
        } else {
            date.weekday().num_days_from_monday()
        };
        date - Duration::days(i64::from(offset))
    }

    fn go_to(&self, date: NaiveDate) {
        self.first.set(self.week_start(date));
        self.month_quiet.set(true);
        if let Ok(day) = glib::DateTime::from_local(date.year(), date.month() as i32, date.day() as i32, 12, 0, 0.0) {
            self.month.select_day(&day);
        }
        self.month_quiet.set(false);
        self.render();
        let from = self.first.get() - Duration::days(7);
        let (from, to) = (utc(self.service.local, from), utc(self.service.local, from + Duration::days(21)));
        if !self.service.covers(from, to) {
            self.refresh_now(self.first.get());
        }
    }

    fn toast(&self, message: &str) {
        let toast = adw::Toast::builder().title(glib::markup_escape_text(message)).timeout(6).build();
        self.toasts.add_toast(toast);
    }

    /// Reads calendars and events from the store again and redraws.
    fn reload(&self) {
        let calendars = match self.service.store.calendars() {
            Ok(calendars) => calendars,
            Err(error) => {
                self.toast(&error.to_string());
                return;
            }
        };
        let resources = self.service.store.resources().unwrap_or_default();
        let parsed = resources
            .into_iter()
            .filter_map(|resource| match ics::parse(&resource.ics) {
                Ok(component) => Some(Parsed { resource: resource.id, calendar: resource.calendar, component }),
                Err(error) => {
                    tracing::warn!("an event that couldn't be read ({}): {error}", resource.url);
                    None
                }
            })
            .collect();
        *self.parsed.borrow_mut() = parsed;
        *self.calendars.borrow_mut() = calendars;
        self.rebuild_sidebar();
        self.render();
    }

    fn color(&self, calendar: &Calendar) -> Rgb {
        let palette = self.palette.borrow();
        if calendar.is_local() {
            return palette.accent;
        }
        calendar
            .color
            .as_deref()
            .and_then(theme::parse_hex)
            .or_else(|| self.service.account_color(&calendar.account).as_deref().and_then(accounts::color_rgb))
            .unwrap_or_else(|| palette.series[calendar.id.unsigned_abs() as usize % palette.series.len()])
    }

    fn items(&self, from: NaiveDate, to: NaiveDate) -> Vec<Item> {
        let local = self.service.local;
        let (from, to) = (
            event::resolve(local, from.and_time(NaiveTime::MIN)),
            event::resolve(local, to.and_time(NaiveTime::MIN)),
        );
        let calendars = self.calendars.borrow();
        let mut items = Vec::new();
        for parsed in self.parsed.borrow().iter() {
            let Some(calendar) = calendars.iter().find(|c| c.id == parsed.calendar && c.visible) else { continue };
            let color = self.color(calendar);
            for occurrence in event::occurrences(&parsed.component, from, to, local) {
                items.push(Item {
                    occurrence,
                    resource: parsed.resource,
                    calendar: calendar.id,
                    color,
                    read_only: calendar.read_only,
                });
            }
        }
        items.sort_by_key(|item| (item.occurrence.start, std::cmp::Reverse(item.occurrence.end)));
        items
    }

    fn render(&self) {
        let first = self.first.get();
        let last = first + Duration::days(6);
        self.week.show(first, self.items(first, first + Duration::days(7)));
        let heading = if first.month() == last.month() {
            first.format("%B %Y").to_string()
        } else if first.year() == last.year() {
            format!("{} – {}", first.format("%b"), last.format("%b %Y"))
        } else {
            format!("{} – {}", first.format("%b %Y"), last.format("%b %Y"))
        };
        self.title.set_title(&heading);
        self.title.set_subtitle(&format!(
            "Week {} · {} – {}",
            first.iso_week().week(),
            first.format("%-d %b"),
            last.format("%-d %b")
        ));
        self.mark_month();
    }

    /// Dots under the days of the shown month that have events.
    fn mark_month(&self) {
        self.month.clear_marks();
        let shown = self.month.date();
        let Some(start) = NaiveDate::from_ymd_opt(shown.year(), shown.month() as u32, 1) else { return };
        let end = start + chrono::Months::new(1);
        let days: BTreeSet<u32> = self
            .items(start, end)
            .iter()
            .flat_map(|item| {
                let first = item.occurrence.start.date_naive().max(start);
                let last = (item.occurrence.end - Duration::seconds(1)).date_naive().min(end - Duration::days(1));
                first.iter_days().take_while(move |day| *day <= last).map(|day| day.day())
            })
            .collect();
        for day in days {
            self.month.mark_day(day);
        }
    }

    fn rebuild_sidebar(&self) {
        while let Some(child) = self.calendar_list.first_child() {
            self.calendar_list.remove(&child);
        }
        let calendars = self.calendars.borrow().clone();
        let mut account = None;
        for calendar in calendars {
            if account.as_ref() != Some(&calendar.account) {
                let heading = gtk::Label::builder()
                    .label(self.service.account_label(&calendar.account))
                    .xalign(0.0)
                    .margin_top(if account.is_some() { 12 } else { 0 })
                    .margin_bottom(4)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["heading", "dimmed"])
                    .build();
                self.calendar_list.append(&heading);
                account = Some(calendar.account.clone());
            }
            let color = self.color(&calendar);
            let dot = gtk::DrawingArea::builder().content_width(12).content_height(12).valign(gtk::Align::Center).build();
            dot.set_draw_func(move |_, cr, width, height| {
                cr.set_source_rgb(color.0, color.1, color.2);
                cr.arc(f64::from(width) / 2.0, f64::from(height) / 2.0, 5.5, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
            });
            let mut name = calendar.name.clone();
            if calendar.read_only {
                name.push_str(" (read-only)");
            }
            let label = gtk::Label::builder().label(name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).hexpand(true).build();
            let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            inner.append(&dot);
            inner.append(&label);
            let check = gtk::CheckButton::builder().child(&inner).active(calendar.visible).build();
            let id = calendar.id;
            let service = Arc::clone(&self.service);
            let weak_window = self.window.downgrade();
            check.connect_toggled(move |check| {
                if let Err(error) = service.store.set_visible(id, check.is_active()) {
                    tracing::warn!("{error}");
                }
                if let Some(window) = weak_window.upgrade() {
                    let _ = WidgetExt::activate_action(&window, "win.redraw", None);
                }
            });
            self.calendar_list.append(&check);
        }
    }

    fn set_refreshing(&self, refreshing: bool) {
        self.refreshing.set(refreshing);
        self.spinner.set_visible(refreshing);
        self.refresh.set_visible(!refreshing);
    }

    fn refresh_now(&self, around: NaiveDate) {
        if self.refreshing.get() {
            return;
        }
        self.set_refreshing(true);
        let service = Arc::clone(&self.service);
        let around = utc(service.local, around);
        let window = self.window.downgrade();
        glib::spawn_future_local(async move {
            let problems = run(async move { service.refresh(around).await }).await;
            let Some(window) = window.upgrade() else { return };
            let _ = WidgetExt::activate_action(&window, "win.refreshed", Some(&problems_variant(&problems)));
        });
    }

    fn refreshed(&self, problems: Vec<Problem>) {
        self.set_refreshing(false);
        let signing_in = problems.iter().find(|p| p.sign_in.is_some());
        self.banner.set_button_label(Some(if signing_in.is_some() { "Sign In" } else { "Details" }));
        match problems.as_slice() {
            [] => self.banner.set_revealed(false),
            _ if let Some(one) = signing_in => {
                self.banner.set_title(&glib::markup_escape_text(&format!(
                    "Sign in with Google to see the calendars of {}",
                    one.account
                )));
                self.banner.set_revealed(true);
            }
            [one] => {
                self.banner.set_title(&glib::markup_escape_text(&format!("{}: {}", one.account, one.message)));
                self.banner.set_revealed(true);
            }
            many => {
                self.banner.set_title(&format!("{} accounts couldn't be refreshed", many.len()));
                self.banner.set_revealed(true);
            }
        }
        *self.problems.borrow_mut() = problems;
        self.reload();
    }

    /// Signs a Google account in through the browser, then refreshes.
    fn sign_in(&self, key: String) {
        self.toast("Finish signing in with Google in your browser");
        let service = Arc::clone(&self.service);
        let window = self.window.downgrade();
        glib::spawn_future_local(async move {
            let result = run(async move {
                service
                    .google_sign_in(&key, |url| {
                        glib::MainContext::default().invoke(move || {
                            if let Err(error) = gio::AppInfo::launch_default_for_uri(&url, gio::AppLaunchContext::NONE) {
                                tracing::warn!("couldn't open the browser: {error}");
                            }
                        });
                    })
                    .await
            })
            .await;
            let Some(window) = window.upgrade() else { return };
            let message = match result {
                Ok(()) => "Signed in with Google".to_owned(),
                Err(error) => error,
            };
            let _ = WidgetExt::activate_action(&window, "win.signed-in", Some(&message.to_variant()));
        });
    }

    fn show_problems(&self) {
        let body = self
            .problems
            .borrow()
            .iter()
            .map(|p| format!("{}: {}", p.account, p.message))
            .collect::<Vec<_>>()
            .join("\n\n");
        let alert = adw::AlertDialog::new(Some("Calendars That Couldn't Be Refreshed"), Some(&body));
        alert.add_response("close", "_Close");
        alert.present(Some(&self.window));
    }

    fn writable(&self) -> Vec<(i64, String)> {
        self.calendars
            .borrow()
            .iter()
            .filter(|calendar| !calendar.read_only)
            .map(|calendar| (calendar.id, format!("{} · {}", calendar.name, self.service.account_label(&calendar.account))))
            .collect()
    }

    pub(crate) fn create(&self, start: NaiveDateTime, end: NaiveDateTime, all_day: bool) {
        self.create_with(
            Draft {
                summary: String::new(),
                location: String::new(),
                description: String::new(),
                attendees: Vec::new(),
                conference: String::new(),
                organizer: None,
                all_day,
                start,
                end,
            },
            None,
        );
    }

    /// Opens the editor on a new event made from an email that Sealwax sent.
    pub(crate) fn create_from_email(&self, input: &signet_core::email::PluginInput) {
        let Some(message) = input.messages.first() else { return };
        let draft = signet_core::email::draft(input, message, Local::now().naive_local());
        self.go_to(draft.start.date());
        self.window.present();
        self.create_with(draft, input.account.as_ref().map(|account| account.key.as_str()));
    }

    /// Opens the editor on a new event; `account` prefers that Sealwax account's calendars.
    fn create_with(&self, draft: Draft, account: Option<&str>) {
        let calendars = self.writable();
        let visible: Vec<i64> = self.calendars.borrow().iter().filter(|c| c.visible && !c.read_only).map(|c| c.id).collect();
        let of_account = account.and_then(|key| {
            let calendars = self.calendars.borrow();
            let mine = |c: &&Calendar| c.account == key && !c.read_only;
            calendars
                .iter()
                .filter(mine)
                .find(|c| c.visible && Some(c.id) == self.last_calendar.get())
                .or_else(|| calendars.iter().filter(mine).find(|c| c.visible))
                .map(|c| c.id)
        });
        let calendar = of_account
            .or(self.last_calendar.get())
            .filter(|id| calendars.iter().any(|(c, _)| c == id))
            .or_else(|| visible.iter().copied().find(|id| self.calendars.borrow().iter().any(|c| c.id == *id && !c.is_local())))
            .or_else(|| visible.first().copied())
            .or_else(|| calendars.first().map(|(id, _)| *id));
        let Some(calendar) = calendar else {
            self.toast("There's no calendar to add events to");
            return;
        };
        let invitees = draft
            .attendees
            .iter()
            .map(|email| Attendee { email: email.clone(), name: String::new(), answer: Answer::Waiting })
            .collect();
        let request = Request {
            heading: "New Event",
            draft,
            calendars,
            calendar,
            editable: true,
            can_delete: false,
            note: None,
            organizer: None,
            invitees,
            can_invite: true,
        };
        let service = Arc::clone(&self.service);
        let window = self.window.clone();
        editor::open(&self.window, request, move |outcome| {
            let Outcome::Save { mut draft, calendar } = outcome else { return };
            draft.organizer = organizer_for(&service, calendar);
            let _ = WidgetExt::activate_action(&window, "win.used-calendar", Some(&calendar.to_variant()));
            let uid = event::new_uid();
            let ics = event::new_calendar(&uid, &draft, service.local).to_ics();
            let service = Arc::clone(&service);
            write(&window, async move { service.create(calendar, &uid, &ics).await }, "Event added");
        });
    }

    fn component(&self, resource: i64) -> Option<Component> {
        self.parsed.borrow().iter().find(|p| p.resource == resource).map(|p| p.component.clone())
    }

    pub(crate) fn open(&self, item: Item) {
        let occurrence = item.occurrence.clone();
        let all_day_end = (occurrence.end - Duration::days(1)).naive_local().max(occurrence.start.naive_local());
        let draft = Draft {
            summary: occurrence.summary.clone(),
            location: occurrence.location.clone(),
            description: occurrence.description.clone(),
            attendees: occurrence.attendees.iter().map(|a| a.email.clone()).collect(),
            conference: occurrence.conference.clone().unwrap_or_default(),
            organizer: None,
            all_day: occurrence.all_day,
            start: occurrence.start.naive_local(),
            end: if occurrence.all_day { all_day_end } else { occurrence.end.naive_local() },
        };
        let own = self
            .calendars
            .borrow()
            .iter()
            .find(|c| c.id == item.calendar)
            .map(|c| (c.id, format!("{} · {}", c.name, self.service.account_label(&c.account))));
        let calendars = if item.read_only || occurrence.recurring {
            own.into_iter().collect()
        } else {
            self.writable()
        };
        let note = if item.read_only {
            Some("This calendar is read-only.".to_owned())
        } else if occurrence.recurring {
            Some("This event repeats. When you save or delete, you choose this event or all of them.".to_owned())
        } else {
            None
        };
        // An invitation's invitees are the organizer's to change.
        let me = organizer_for(&self.service, item.calendar).map(|(_, email)| email);
        let can_invite = occurrence
            .organizer
            .as_ref()
            .is_none_or(|organizer| me.as_deref().is_some_and(|me| me.eq_ignore_ascii_case(&organizer.email)));
        let request = Request {
            heading: if item.read_only { "Event" } else { "Edit Event" },
            draft,
            calendars,
            calendar: item.calendar,
            editable: !item.read_only,
            can_delete: !item.read_only,
            note,
            organizer: occurrence.organizer.clone(),
            invitees: occurrence.attendees.clone(),
            can_invite,
        };
        let Some(component) = self.component(item.resource) else { return };
        let service = Arc::clone(&self.service);
        let window = self.window.clone();
        editor::open(&self.window, request, move |outcome| {
            let (service, window, item, component) = (Arc::clone(&service), window.clone(), item.clone(), component.clone());
            glib::spawn_future_local(async move {
                let scope = match item.occurrence.recurrence_id {
                    Some(id) if item.occurrence.recurring => {
                        let deleting = matches!(outcome, Outcome::Delete);
                        match ask_scope(&window, deleting).await {
                            Some(true) => Scope::This(id),
                            Some(false) => Scope::All,
                            None => return,
                        }
                    }
                    _ => {
                        if matches!(outcome, Outcome::Delete) && !confirm_delete(&window, &item.occurrence.summary).await {
                            return;
                        }
                        Scope::All
                    }
                };
                apply(&window, &service, &item, component, outcome, scope);
            });
        });
    }
}

/// The name and address a calendar's account invites people as; none for the local calendar.
fn organizer_for(service: &Service, calendar: i64) -> Option<(String, String)> {
    let calendar = service.store.calendar(calendar).ok().flatten()?;
    service
        .accounts()
        .into_iter()
        .find(|account| account.key == calendar.account)
        .map(|account| (account.name, account.address))
}

/// Carries out an edit or removal on the event's calendar object.
fn apply(window: &adw::ApplicationWindow, service: &Arc<Service>, item: &Item, mut component: Component, outcome: Outcome, scope: Scope) {
    let local = service.local;
    let resource = item.resource;
    let service = Arc::clone(service);
    match (outcome, scope) {
        (Outcome::Delete, Scope::All) => {
            write(window, async move { service.delete(resource).await }, "Event deleted");
        }
        (Outcome::Delete, Scope::This(id)) => match event::remove_occurrence(&mut component, id, local) {
            Ok(Removal::Everything) => write(window, async move { service.delete(resource).await }, "Event deleted"),
            Ok(Removal::Updated) => {
                let ics = component.to_ics();
                write(window, async move { service.update(resource, &ics).await }, "Event deleted");
            }
            Err(error) => toast_on(window, &error.to_string()),
        },
        (Outcome::Save { mut draft, calendar }, scope) => {
            draft.organizer = organizer_for(&service, calendar);
            if let Err(error) = event::edit(&mut component, &item.occurrence, &draft, scope, local) {
                toast_on(window, &error.to_string());
                return;
            }
            let ics = component.to_ics();
            if calendar != item.calendar && scope == Scope::All {
                let uid = item.occurrence.uid.clone();
                let uid = if uid.is_empty() { event::new_uid() } else { uid };
                write(window, async move { service.move_to(resource, calendar, &uid, &ics).await }, "Event moved");
            } else {
                write(window, async move { service.update(resource, &ics).await }, "Event saved");
            }
        }
    }
}

thread_local! {
    /// Writes under way, for the self-test to wait on.
    static WRITES: Cell<u32> = const { Cell::new(0) };
}

/// Runs a write in the background, then reloads the window or shows why it failed.
fn write<F>(window: &adw::ApplicationWindow, future: F, done: &'static str)
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let window = window.downgrade();
    WRITES.with(|writes| writes.set(writes.get() + 1));
    glib::spawn_future_local(async move {
        let result = run(future).await;
        WRITES.with(|writes| writes.set(writes.get() - 1));
        let Some(window) = window.upgrade() else { return };
        let message = match result {
            Ok(()) => done.to_owned(),
            Err(error) => format!("Not saved: {error}"),
        };
        let _ = WidgetExt::activate_action(&window, "win.written", Some(&message.to_variant()));
    });
}

fn toast_on(window: &adw::ApplicationWindow, message: &str) {
    let _ = WidgetExt::activate_action(window, "win.written", Some(&format!("Not saved: {message}").to_variant()));
}

/// "This event" (true), "all events" (false), or cancelled.
async fn ask_scope(window: &adw::ApplicationWindow, deleting: bool) -> Option<bool> {
    let (heading, body) = if deleting {
        ("Delete Repeating Event?", "Delete only this event, or every event in the series?")
    } else {
        ("Change Repeating Event?", "Change only this event, or every event in the series?")
    };
    let alert = adw::AlertDialog::new(Some(heading), Some(body));
    alert.add_responses(&[("cancel", "_Cancel"), ("all", "_All Events"), ("this", "_This Event")]);
    alert.set_default_response(Some("this"));
    alert.set_close_response("cancel");
    if deleting {
        alert.set_response_appearance("all", adw::ResponseAppearance::Destructive);
        alert.set_response_appearance("this", adw::ResponseAppearance::Destructive);
    } else {
        alert.set_response_appearance("this", adw::ResponseAppearance::Suggested);
    }
    match alert.choose_future(Some(window)).await.as_str() {
        "this" => Some(true),
        "all" => Some(false),
        _ => None,
    }
}

async fn confirm_delete(window: &adw::ApplicationWindow, summary: &str) -> bool {
    let name = if summary.is_empty() { "this event".to_owned() } else { format!("“{summary}”") };
    let alert = adw::AlertDialog::new(Some("Delete Event?"), Some(&format!("Delete {name}?")));
    alert.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
    alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("delete"));
    alert.set_close_response("cancel");
    alert.choose_future(Some(window)).await == "delete"
}

fn utc(local: signet_core::Tz, day: NaiveDate) -> chrono::DateTime<Utc> {
    event::resolve(local, day.and_time(NaiveTime::MIN)).with_timezone(&Utc)
}

fn problems_variant(problems: &[Problem]) -> glib::Variant {
    problems
        .iter()
        .map(|p| (p.account.clone(), p.message.clone(), p.sign_in.clone().unwrap_or_default()))
        .collect::<Vec<(String, String, String)>>()
        .to_variant()
}

/// Actions that carry results from background work back to the window.
pub fn install_result_actions(this: &Rc<Window>) {
    let weak = Rc::downgrade(this);
    let refreshed = gio::SimpleAction::new("refreshed", Some(glib::VariantTy::new("a(sss)").expect("a type")));
    refreshed.connect_activate(move |_, value| {
        let (Some(this), Some(value)) = (weak.upgrade(), value) else { return };
        let problems: Vec<(String, String, String)> = value.get().unwrap_or_default();
        this.refreshed(
            problems
                .into_iter()
                .map(|(account, message, key)| Problem { account, message, sign_in: (!key.is_empty()).then_some(key) })
                .collect(),
        );
    });
    this.window.add_action(&refreshed);

    let weak = Rc::downgrade(this);
    let written = gio::SimpleAction::new("written", Some(glib::VariantTy::STRING));
    written.connect_activate(move |_, value| {
        let (Some(this), Some(value)) = (weak.upgrade(), value) else { return };
        let message: String = value.get().unwrap_or_default();
        this.toast(&message);
        this.reload();
    });
    this.window.add_action(&written);

    let weak = Rc::downgrade(this);
    let signed_in = gio::SimpleAction::new("signed-in", Some(glib::VariantTy::STRING));
    signed_in.connect_activate(move |_, value| {
        let (Some(this), Some(value)) = (weak.upgrade(), value) else { return };
        this.toast(&value.get::<String>().unwrap_or_default());
        this.refresh_now(this.first.get());
    });
    this.window.add_action(&signed_in);

    let weak = Rc::downgrade(this);
    let redraw = gio::SimpleAction::new("redraw", None);
    redraw.connect_activate(move |_, _| {
        if let Some(this) = weak.upgrade() {
            this.reload();
        }
    });
    this.window.add_action(&redraw);

    let weak = Rc::downgrade(this);
    let used = gio::SimpleAction::new("used-calendar", Some(glib::VariantTy::INT64));
    used.connect_activate(move |_, value| {
        if let (Some(this), Some(id)) = (weak.upgrade(), value.and_then(|v| v.get::<i64>())) {
            this.last_calendar.set(Some(id));
        }
    });
    this.window.add_action(&used);
}
