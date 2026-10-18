//! The week view: seven day columns of hours under a header of dates and all-day events.
//! Clicking an event opens it; double-clicking an empty slot, or dragging down over the
//! hours, starts a new one there.

use crate::theme::{Palette, Rgb};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use gtk4::{self as gtk, cairo, glib, pango, prelude::*};
use signet_core::Tz;
use signet_core::event::Occurrence;
use std::cell::RefCell;
use std::rc::Rc;

const GUTTER: f64 = 60.0;
const HOUR: f64 = 48.0;
const DAY_HEADER: f64 = 50.0;
const ALL_DAY_ROW: f64 = 24.0;
/// New events and drags snap to this many minutes.
const SNAP: i64 = 15;

/// An occurrence as the week view shows it.
#[derive(Clone, Debug)]
pub struct Item {
    pub occurrence: Occurrence,
    pub resource: i64,
    pub calendar: i64,
    pub color: Rgb,
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
}

type OpenFn = Box<dyn Fn(Item)>;
type CreateFn = Box<dyn Fn(NaiveDateTime, NaiveDateTime, bool)>;

struct State {
    first: NaiveDate,
    items: Vec<Item>,
    palette: Palette,
    local: Tz,
    timed: Vec<(Rect, usize)>,
    all_day: Vec<(Rect, usize)>,
    all_day_rows: Vec<(usize, usize, usize, usize)>,
    ghost: Option<(usize, f64, f64)>,
    pressed: Option<usize>,
    on_open: Option<OpenFn>,
    on_create: Option<CreateFn>,
}

#[derive(Clone)]
pub struct WeekView {
    pub root: gtk::Box,
    header: gtk::DrawingArea,
    body: gtk::DrawingArea,
    scroller: gtk::ScrolledWindow,
    state: Rc<RefCell<State>>,
}

impl WeekView {
    pub fn new(palette: Palette, local: Tz) -> Self {
        let header = gtk::DrawingArea::builder().content_height(DAY_HEADER as i32).hexpand(true).build();
        let body = gtk::DrawingArea::builder()
            .content_height((HOUR * 24.0) as i32)
            .hexpand(true)
            .has_tooltip(true)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&body)
            .build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&scroller);
        let state = Rc::new(RefCell::new(State {
            first: chrono::Local::now().date_naive(),
            items: Vec::new(),
            palette,
            local,
            timed: Vec::new(),
            all_day: Vec::new(),
            all_day_rows: Vec::new(),
            ghost: None,
            pressed: None,
            on_open: None,
            on_create: None,
        }));
        let view = Self { root, header, body, scroller, state };
        view.connect_drawing();
        view.connect_body_input();
        view.connect_header_input();
        // Keep the now line moving.
        let body = view.body.downgrade();
        glib::timeout_add_seconds_local(60, move || match body.upgrade() {
            Some(body) => {
                body.queue_draw();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        view
    }

    pub fn connect_open(&self, open: impl Fn(Item) + 'static) {
        self.state.borrow_mut().on_open = Some(Box::new(open));
    }

    pub fn connect_create(&self, create: impl Fn(NaiveDateTime, NaiveDateTime, bool) + 'static) {
        self.state.borrow_mut().on_create = Some(Box::new(create));
    }

    pub fn set_palette(&self, palette: Palette) {
        self.state.borrow_mut().palette = palette;
        self.redraw();
    }

    /// Shows the week starting on `first` with these items.
    pub fn show(&self, first: NaiveDate, items: Vec<Item>) {
        {
            let mut state = self.state.borrow_mut();
            state.first = first;
            state.all_day_rows = all_day_rows(&items, first, state.local);
            state.items = items;
        }
        let rows = self.state.borrow().all_day_rows.iter().map(|r| r.3 + 1).max().unwrap_or(0);
        let height = DAY_HEADER + rows as f64 * ALL_DAY_ROW + if rows > 0 { 6.0 } else { 0.0 };
        self.header.set_content_height(height as i32);
        self.redraw();
    }

    fn redraw(&self) {
        self.header.queue_draw();
        self.body.queue_draw();
    }

    /// Scrolls the hours so `hour` is at the top, once the view has its size.
    pub fn scroll_to_hour(&self, hour: u32) {
        let scroller = self.scroller.clone();
        let set = move || {
            let adjustment = scroller.vadjustment();
            // A little above the hour, so its label shows whole.
            let top = (f64::from(hour) * HOUR - 12.0).max(0.0);
            adjustment.set_value(top.min(adjustment.upper() - adjustment.page_size()));
        };
        if self.scroller.is_mapped() {
            set();
        } else {
            self.scroller.connect_map(move |_| {
                let set = set.clone();
                glib::idle_add_local_once(set);
            });
        }
    }

    fn connect_drawing(&self) {
        let state = Rc::clone(&self.state);
        self.header.set_draw_func(move |area, cr, width, _height| {
            draw_header(&mut state.borrow_mut(), area, cr, f64::from(width));
        });
        let state = Rc::clone(&self.state);
        self.body.set_draw_func(move |area, cr, width, _height| {
            draw_body(&mut state.borrow_mut(), area, cr, f64::from(width));
        });
    }

    fn connect_body_input(&self) {
        let click = gtk::GestureClick::new();
        let state = Rc::clone(&self.state);
        click.connect_pressed(move |_, _, x, y| {
            let mut state = state.borrow_mut();
            state.pressed = state.timed.iter().rev().find(|(rect, _)| rect.contains(x, y)).map(|(_, i)| *i);
        });
        let state = Rc::clone(&self.state);
        let body = self.body.clone();
        click.connect_released(move |_, presses, x, y| {
            let hit = {
                let state = state.borrow();
                state.timed.iter().rev().find(|(rect, _)| rect.contains(x, y)).map(|(_, i)| *i)
            };
            let pressed = state.borrow_mut().pressed.take();
            if let Some(index) = hit.filter(|hit| Some(*hit) == pressed) {
                let item = state.borrow().items[index].clone();
                if let Some(open) = &state.borrow().on_open {
                    open(item);
                }
            } else if hit.is_none() && presses == 2 {
                let width = f64::from(body.width());
                let start = {
                    let state = state.borrow();
                    slot_at(state.first, width, x, y, 30)
                };
                if let (Some(start), Some(create)) = (start, &state.borrow().on_create) {
                    create(start, start + Duration::hours(1), false);
                }
            }
        });
        self.body.add_controller(click);

        let drag = gtk::GestureDrag::new();
        let state = Rc::clone(&self.state);
        let body = self.body.clone();
        drag.connect_drag_update(move |gesture, _, dy| {
            let Some((x, y)) = gesture.start_point() else { return };
            let width = f64::from(body.width());
            let mut state = state.borrow_mut();
            let on_event = state.timed.iter().any(|(rect, _)| rect.contains(x, y));
            if on_event || dy.abs() < 8.0 || x < GUTTER {
                return;
            }
            let column = ((x - GUTTER) / day_width(width)).floor().clamp(0.0, 6.0) as usize;
            state.ghost = Some((column, y.min(y + dy), y.max(y + dy)));
            drop(state);
            body.queue_draw();
        });
        let state = Rc::clone(&self.state);
        let body = self.body.clone();
        drag.connect_drag_end(move |_, _, _| {
            let ghost = state.borrow_mut().ghost.take();
            body.queue_draw();
            let Some((column, top, bottom)) = ghost else { return };
            let day = state.borrow().first + Duration::days(column as i64);
            let start = time_at(day, top, SNAP);
            let end = time_at(day, bottom, SNAP).max(start + Duration::minutes(SNAP));
            if let Some(create) = &state.borrow().on_create {
                create(start, end, false);
            }
        });
        self.body.add_controller(drag);

        let state = Rc::clone(&self.state);
        self.body.connect_query_tooltip(move |_, x, y, _, tooltip| {
            let state = state.borrow();
            let Some((_, index)) = state.timed.iter().rev().find(|(rect, _)| rect.contains(f64::from(x), f64::from(y))) else {
                return false;
            };
            tooltip.set_text(Some(&describe(&state.items[*index].occurrence)));
            true
        });
    }

    fn connect_header_input(&self) {
        let click = gtk::GestureClick::new();
        let state = Rc::clone(&self.state);
        let header = self.header.clone();
        click.connect_released(move |_, presses, x, y| {
            let hit = {
                let state = state.borrow();
                state.all_day.iter().find(|(rect, _)| rect.contains(x, y)).map(|(_, i)| *i)
            };
            if let Some(index) = hit {
                let item = state.borrow().items[index].clone();
                if let Some(open) = &state.borrow().on_open {
                    open(item);
                }
            } else if presses == 2 && x > GUTTER {
                let width = f64::from(header.width());
                let column = ((x - GUTTER) / day_width(width)).floor().clamp(0.0, 6.0) as i64;
                let day = state.borrow().first + Duration::days(column);
                let start = day.and_time(NaiveTime::MIN);
                if let Some(create) = &state.borrow().on_create {
                    create(start, start, true);
                }
            }
        });
        self.header.add_controller(click);
    }
}

fn day_width(width: f64) -> f64 {
    ((width - GUTTER) / 7.0).max(1.0)
}

fn time_at(day: NaiveDate, y: f64, snap: i64) -> NaiveDateTime {
    let minutes = ((y / HOUR * 60.0) as i64).clamp(0, 24 * 60);
    let minutes = (minutes + snap / 2) / snap * snap;
    day.and_time(NaiveTime::MIN) + Duration::minutes(minutes.min(24 * 60 - snap))
}

fn slot_at(first: NaiveDate, width: f64, x: f64, y: f64, snap: i64) -> Option<NaiveDateTime> {
    if x < GUTTER {
        return None;
    }
    let column = ((x - GUTTER) / day_width(width)).floor().clamp(0.0, 6.0) as i64;
    let day = first + Duration::days(column);
    let minutes = ((y / HOUR * 60.0) as i64).clamp(0, 24 * 60 - snap) / snap * snap;
    Some(day.and_time(NaiveTime::MIN) + Duration::minutes(minutes))
}

pub fn describe(occurrence: &Occurrence) -> String {
    let mut text = if occurrence.summary.is_empty() { "(No title)".to_owned() } else { occurrence.summary.clone() };
    if occurrence.all_day {
        text.push_str("\nAll day");
    } else {
        text.push_str(&format!(
            "\n{} – {}",
            occurrence.start.format("%a %-d %b, %H:%M"),
            occurrence.end.format("%H:%M")
        ));
    }
    if !occurrence.location.is_empty() {
        text.push('\n');
        text.push_str(&occurrence.location);
    }
    if occurrence.conference.is_some() {
        text.push_str("\nVideo call");
    }
    if !occurrence.attendees.is_empty() {
        let count = occurrence.attendees.len();
        text.push_str(&format!("\n{count} invitee{}", if count == 1 { "" } else { "s" }));
    }
    if occurrence.recurring {
        text.push_str("\nRepeats");
    }
    text
}

/// The all-day (and multi-day) items' places: (item, first column, last column, row).
fn all_day_rows(items: &[Item], first: NaiveDate, _local: Tz) -> Vec<(usize, usize, usize, usize)> {
    let mut spans: Vec<(usize, usize, usize)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.occurrence.all_day)
        .filter_map(|(index, item)| {
            let start = (item.occurrence.start.date_naive() - first).num_days();
            // An all-day event ends at the midnight after its last day.
            let end = (item.occurrence.end - Duration::seconds(1)).date_naive();
            let end = (end - first).num_days();
            if end < 0 || start > 6 {
                return None;
            }
            Some((index, start.max(0) as usize, end.min(6) as usize))
        })
        .collect();
    spans.sort_by_key(|(_, start, end)| (*start, std::cmp::Reverse(*end)));
    let mut rows: Vec<usize> = Vec::new(); // the last column used in each row
    spans
        .into_iter()
        .map(|(index, start, end)| {
            let row = match rows.iter().position(|last| *last < start) {
                Some(row) => row,
                None => {
                    rows.push(0);
                    rows.len() - 1
                }
            };
            rows[row] = end;
            (index, start, end, row)
        })
        .collect()
}

fn set(cr: &cairo::Context, (r, g, b): Rgb, alpha: f64) {
    cr.set_source_rgba(r, g, b, alpha);
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

fn readable_on(color: Rgb) -> Rgb {
    let luminance = 0.2126 * color.0 + 0.7152 * color.1 + 0.0722 * color.2;
    if luminance > 0.55 { (0.08, 0.08, 0.08) } else { (1.0, 1.0, 1.0) }
}

fn rounded(cr: &cairo::Context, rect: Rect, radius: f64) {
    let r = radius.min(rect.h / 2.0).min(rect.w / 2.0);
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
}

#[allow(clippy::too_many_arguments)]
fn text(area: &gtk::DrawingArea, cr: &cairo::Context, markup: &str, x: f64, y: f64, width: f64, height: Option<f64>, color: Rgb, alpha: f64) {
    let layout = area.create_pango_layout(None);
    layout.set_markup(markup);
    layout.set_width((width.max(1.0) * f64::from(pango::SCALE)) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    match height {
        // Wrapping a narrow box breaks words apart; give each line one line instead.
        Some(height) if width >= 100.0 => {
            layout.set_wrap(pango::WrapMode::Word);
            layout.set_height((height.max(1.0) * f64::from(pango::SCALE)) as i32);
        }
        _ => layout.set_height(-1),
    }
    set(cr, color, alpha);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
}

fn small(text: &str) -> String {
    format!("<span size='small'>{}</span>", glib::markup_escape_text(text))
}

fn draw_header(state: &mut State, area: &gtk::DrawingArea, cr: &cairo::Context, width: f64) {
    let palette = state.palette.clone();
    let column = day_width(width);
    let today = chrono::Local::now().date_naive();
    set(cr, palette.background, 1.0);
    let _ = cr.paint();
    for day in 0..7 {
        let date = state.first + Duration::days(day);
        let x = GUTTER + column * day as f64;
        let is_today = date == today;
        text(area, cr, &small(&date.format("%a").to_string().to_uppercase()), x + 10.0, 6.0, column - 12.0, None,
            if is_today { palette.accent } else { palette.muted }, 1.0);
        let number = date.format("%-d").to_string();
        if is_today {
            set(cr, palette.accent, 1.0);
            // A circle where corners are round, a square where they're square (Omarchy at 0).
            let marker = Rect { x: x + 10.0, y: 21.0, w: 26.0, h: 26.0 };
            rounded(cr, marker, if palette.radius > 0.0 { 13.0 } else { 0.0 });
            let _ = cr.fill();
        }
        let markup = format!("<span size='large' weight='bold'>{number}</span>");
        let layout = area.create_pango_layout(None);
        layout.set_markup(&markup);
        let (_, logical) = layout.pixel_extents();
        set(cr, if is_today { palette.background } else { palette.foreground }, 1.0);
        cr.move_to(x + 23.0 - f64::from(logical.width()) / 2.0, 34.0 - f64::from(logical.height()) / 2.0);
        pangocairo::functions::show_layout(cr, &layout);
        if day > 0 {
            set(cr, palette.foreground, 0.08);
            cr.rectangle(x.round() - 0.5, DAY_HEADER - 8.0, 1.0, 400.0);
            let _ = cr.fill();
        }
    }
    text(area, cr, &small("all-day"), 6.0, DAY_HEADER + 4.0, GUTTER - 10.0, None, palette.muted,
        if state.all_day_rows.is_empty() { 0.0 } else { 1.0 });

    state.all_day.clear();
    for &(index, start, end, row) in &state.all_day_rows {
        let item = &state.items[index];
        let rect = Rect {
            x: GUTTER + column * start as f64 + 2.0,
            y: DAY_HEADER + row as f64 * ALL_DAY_ROW,
            w: column * (end - start + 1) as f64 - 4.0,
            h: ALL_DAY_ROW - 3.0,
        };
        rounded(cr, rect, palette.small_radius());
        set(cr, item.color, 0.9);
        let _ = cr.fill();
        let title = if item.occurrence.summary.is_empty() { "(No title)" } else { &item.occurrence.summary };
        text(area, cr, &small(title), rect.x + 6.0, rect.y + 3.0, rect.w - 10.0, None, readable_on(item.color), 1.0);
        state.all_day.push((rect, index));
    }
}

/// A timed item's part on one day, and its place among the day's overlapping parts.
struct Placed {
    index: usize,
    top: f64,
    bottom: f64,
    column: usize,
    columns: usize,
}

fn place_day(state: &State, day: NaiveDate) -> Vec<Placed> {
    let local = state.local;
    let day_start = signet_core::event::resolve(local, day.and_time(NaiveTime::MIN));
    let day_end = signet_core::event::resolve(local, (day + Duration::days(1)).and_time(NaiveTime::MIN));
    let minutes = |moment: DateTime<Tz>| -> f64 {
        if moment <= day_start {
            0.0
        } else if moment >= day_end {
            24.0 * 60.0
        } else {
            f64::from(moment.hour() * 60 + moment.minute())
        }
    };
    let mut parts: Vec<Placed> = state
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.occurrence.all_day)
        .filter(|(_, item)| item.occurrence.start < day_end && item.occurrence.end > day_start
            || item.occurrence.start == item.occurrence.end && item.occurrence.start >= day_start && item.occurrence.start < day_end)
        .map(|(index, item)| {
            let top = minutes(item.occurrence.start) / 60.0 * HOUR;
            let bottom = (minutes(item.occurrence.end) / 60.0 * HOUR).max(top + HOUR * 0.4);
            Placed { index, top, bottom, column: 0, columns: 1 }
        })
        .collect();
    parts.sort_by(|a, b| a.top.total_cmp(&b.top).then(b.bottom.total_cmp(&a.bottom)));
    // Overlapping parts form clusters; each takes the first free column of its cluster.
    let mut cluster_start = 0;
    let mut ends: Vec<f64> = Vec::new();
    let mut cluster_bottom = f64::MIN;
    for i in 0..parts.len() {
        if parts[i].top >= cluster_bottom {
            let columns = ends.len().max(1);
            for part in &mut parts[cluster_start..i] {
                part.columns = columns;
            }
            cluster_start = i;
            ends.clear();
        }
        let column = match ends.iter().position(|end| *end <= parts[i].top) {
            Some(column) => column,
            None => {
                ends.push(0.0);
                ends.len() - 1
            }
        };
        ends[column] = parts[i].bottom;
        parts[i].column = column;
        cluster_bottom = if i == cluster_start { parts[i].bottom } else { cluster_bottom.max(parts[i].bottom) };
    }
    let columns = ends.len().max(1);
    for part in &mut parts[cluster_start..] {
        part.columns = columns;
    }
    parts
}

fn draw_body(state: &mut State, area: &gtk::DrawingArea, cr: &cairo::Context, width: f64) {
    let palette = state.palette.clone();
    let column = day_width(width);
    let now = chrono::Local::now();
    let today = now.date_naive();
    set(cr, palette.background, 1.0);
    let _ = cr.paint();

    let today_column = (today - state.first).num_days();
    if (0..7).contains(&today_column) {
        set(cr, palette.accent, 0.05);
        cr.rectangle(GUTTER + column * today_column as f64, 0.0, column, HOUR * 24.0);
        let _ = cr.fill();
    }
    for hour in 0..24 {
        let y = (f64::from(hour) * HOUR).round() + 0.5;
        if hour > 0 {
            text(area, cr, &small(&format!("{hour:02}:00")), 8.0, y - 9.0, GUTTER - 14.0, None, palette.muted, 1.0);
        }
        set(cr, palette.foreground, 0.09);
        cr.move_to(GUTTER - 6.0, y);
        cr.line_to(width, y);
        let _ = cr.stroke();
        set(cr, palette.foreground, 0.04);
        cr.move_to(GUTTER, y + HOUR / 2.0);
        cr.line_to(width, y + HOUR / 2.0);
        let _ = cr.stroke();
    }
    for day in 0..=7 {
        let x = (GUTTER + column * f64::from(day)).round() + 0.5;
        set(cr, palette.foreground, 0.09);
        cr.move_to(x, 0.0);
        cr.line_to(x, HOUR * 24.0);
        let _ = cr.stroke();
    }

    state.timed.clear();
    for day in 0..7 {
        let date = state.first + Duration::days(day);
        let left = GUTTER + column * day as f64;
        for part in place_day(state, date) {
            let item = &state.items[part.index];
            let slot = (column - 6.0) / part.columns as f64;
            let rect = Rect {
                x: left + 2.0 + slot * part.column as f64,
                y: part.top + 1.0,
                w: slot - 2.0,
                h: part.bottom - part.top - 2.0,
            };
            rounded(cr, rect, palette.small_radius());
            let fill = mix(palette.background, item.color, if palette.dark { 0.32 } else { 0.22 });
            set(cr, fill, 1.0);
            let _ = cr.fill_preserve();
            set(cr, item.color, 0.55);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
            set(cr, item.color, 1.0);
            rounded(cr, Rect { w: 4.0, ..rect }, palette.small_radius().min(2.0));
            let _ = cr.fill();

            let occurrence = &item.occurrence;
            let title = glib::markup_escape_text(if occurrence.summary.is_empty() { "(No title)" } else { &occurrence.summary });
            let mut markup = format!("<span size='small' weight='bold'>{title}</span>");
            if rect.h > 34.0 {
                let mut detail = format!("{} – {}", occurrence.start.format("%H:%M"), occurrence.end.format("%H:%M"));
                if !occurrence.location.is_empty() {
                    detail.push_str(" · ");
                    detail.push_str(&occurrence.location);
                }
                markup.push('\n');
                markup.push_str(&small(&detail));
            }
            let _ = cr.save();
            cr.rectangle(rect.x, rect.y, rect.w, rect.h);
            cr.clip();
            text(area, cr, &markup, rect.x + 8.0, rect.y + 2.0, rect.w - 11.0, Some(rect.h - 4.0), palette.foreground,
                if item.read_only { 0.8 } else { 1.0 });
            let _ = cr.restore();
            state.timed.push((rect, part.index));
        }
    }

    if let Some((day, top, bottom)) = state.ghost {
        let rect = Rect { x: GUTTER + column * day as f64 + 2.0, y: top, w: column - 6.0, h: (bottom - top).max(4.0) };
        rounded(cr, rect, palette.small_radius());
        set(cr, palette.accent, 0.3);
        let _ = cr.fill_preserve();
        set(cr, palette.accent, 0.9);
        let _ = cr.stroke();
    }

    if (0..7).contains(&today_column) {
        let y = f64::from(now.hour() * 60 + now.minute()) / 60.0 * HOUR;
        let x = GUTTER + column * today_column as f64;
        set(cr, palette.now, 1.0);
        cr.rectangle(x, y - 1.0, column, 2.0);
        let _ = cr.fill();
        cr.arc(x, y, 5.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_snap_to_the_grid() {
        let first = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let width = GUTTER + 700.0;
        let slot = slot_at(first, width, GUTTER + 150.0, HOUR * 9.6, 30).unwrap();
        assert_eq!(slot, NaiveDate::from_ymd_opt(2026, 10, 13).unwrap().and_hms_opt(9, 30, 0).unwrap());
        assert_eq!(slot_at(first, width, 10.0, 0.0, 30), None);
        assert_eq!(time_at(first, HOUR * 24.0, 15).time(), NaiveTime::from_hms_opt(23, 45, 0).unwrap());
    }
}
