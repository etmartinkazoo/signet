//! Events read from and written to `VEVENT`s: their times in every form iCalendar allows, their
//! occurrences in a range of days (recurrence rules, exceptions and moved occurrences), and the
//! edits Signet makes (to a whole series or to one occurrence).

use crate::ics::{self, Component, Property};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// The most occurrences one series yields for one range: a guard against FREQ=SECONDLY.
const MAX_OCCURRENCES: u16 = 2000;

/// A DTSTART, DTEND, RECURRENCE-ID or EXDATE value, in the form the file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Date(NaiveDate),
    /// No zone: the same wall time wherever the reader is.
    Floating(NaiveDateTime),
    Utc(DateTime<Utc>),
    Zoned(NaiveDateTime, Tz),
}

impl When {
    pub fn parse(property: &Property, local: Tz) -> Option<Self> {
        Self::parse_value(property.value.trim(), property, local)
    }

    fn parse_value(value: &str, property: &Property, local: Tz) -> Option<Self> {
        let is_date = property
            .param("VALUE")
            .is_some_and(|kind| kind.eq_ignore_ascii_case("DATE"))
            || value.len() == 8;
        if is_date {
            return NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(Self::Date);
        }
        if let Some(utc) = value.strip_suffix(['Z', 'z']) {
            let time = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
            return Some(Self::Utc(Utc.from_utc_datetime(&time)));
        }
        let time = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
        Some(match property.param("TZID").and_then(zone) {
            Some(tz) => Self::Zoned(time, tz),
            // An unknown TZID reads as this computer's zone, the best guess there is.
            None if property.param("TZID").is_some() => Self::Zoned(time, local),
            None => Self::Floating(time),
        })
    }

    /// Every value of a multi-valued property such as EXDATE.
    pub fn parse_list(property: &Property, local: Tz) -> Vec<Self> {
        property
            .value
            .split(',')
            .filter_map(|value| Self::parse_value(value.trim(), property, local))
            .collect()
    }

    pub fn is_date(self) -> bool {
        matches!(self, Self::Date(_))
    }

    /// The moment this is, seen in `local`. A date is its midnight.
    pub fn in_zone(self, local: Tz) -> DateTime<Tz> {
        match self {
            Self::Date(date) => resolve(local, date.and_time(NaiveTime::MIN)),
            Self::Floating(time) => resolve(local, time),
            Self::Utc(time) => time.with_timezone(&local),
            Self::Zoned(time, tz) => resolve(tz, time).with_timezone(&local),
        }
    }

    /// A value of the same form at the moment `moment` (or on its date, for a date).
    pub fn same_form(self, moment: DateTime<Tz>) -> Self {
        match self {
            Self::Date(_) => Self::Date(moment.date_naive()),
            Self::Floating(_) => Self::Floating(moment.naive_local()),
            Self::Utc(_) => Self::Utc(moment.with_timezone(&Utc)),
            Self::Zoned(_, tz) => Self::Zoned(moment.with_timezone(&tz).naive_local(), tz),
        }
    }

    /// The property `name` holding this value.
    pub fn property(self, name: &str) -> Property {
        match self {
            Self::Date(date) => {
                Property::new(name, date.format("%Y%m%d").to_string()).with_param("VALUE", "DATE")
            }
            Self::Floating(time) => Property::new(name, time.format("%Y%m%dT%H%M%S").to_string()),
            Self::Utc(time) => Property::new(name, time.format("%Y%m%dT%H%M%SZ").to_string()),
            Self::Zoned(time, tz) => Property::new(name, time.format("%Y%m%dT%H%M%S").to_string())
                .with_param("TZID", tz.name()),
        }
    }

    /// Whether two values name the same occurrence (RECURRENCE-ID against a generated start).
    fn same_occurrence(self, other: Self, local: Tz) -> bool {
        match (self, other) {
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::Date(a), b) | (b, Self::Date(a)) => b.in_zone(local).date_naive() == a,
            (a, b) => a.in_zone(local) == b.in_zone(local),
        }
    }
}

/// A wall time in `tz`; one that falls in a DST gap moves on an hour, one that repeats is the first.
pub fn resolve(tz: Tz, time: NaiveDateTime) -> DateTime<Tz> {
    tz.from_local_datetime(&time)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(time + Duration::hours(1))).earliest())
        .unwrap_or_else(|| tz.from_utc_datetime(&time))
}

/// The zone a TZID names: an IANA name, an IANA name at the end of a longer one (Mozilla's
/// `/mozilla.org/20050126_1/Europe/Berlin`), or a common Windows name (Outlook invitations).
pub fn zone(tzid: &str) -> Option<Tz> {
    let tzid = tzid.trim().trim_matches('"');
    if let Ok(tz) = tzid.trim_start_matches('/').parse::<Tz>() {
        return Some(tz);
    }
    let segments: Vec<&str> = tzid.split('/').collect();
    for start in 1..segments.len() {
        if let Ok(tz) = segments[start..].join("/").parse::<Tz>() {
            return Some(tz);
        }
    }
    WINDOWS_ZONES
        .iter()
        .find(|(windows, _)| windows.eq_ignore_ascii_case(tzid))
        .and_then(|(_, iana)| iana.parse().ok())
}

const WINDOWS_ZONES: &[(&str, &str)] = &[
    ("UTC", "UTC"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("India Standard Time", "Asia/Kolkata"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("Eastern Standard Time", "America/New_York"),
    ("Central Standard Time", "America/Chicago"),
    ("Mountain Standard Time", "America/Denver"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
];

/// This computer's zone, or UTC when it can't be named.
pub fn local_zone() -> Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse().ok())
        .unwrap_or(Tz::UTC)
}

/// One VEVENT: a single event, a series (with RRULE), or one moved occurrence of a series
/// (with RECURRENCE-ID).
#[derive(Clone, Debug)]
pub struct Event {
    pub uid: String,
    pub summary: String,
    pub location: String,
    pub description: String,
    pub organizer: Option<Attendee>,
    pub attendees: Vec<Attendee>,
    /// The video call's link.
    pub conference: Option<String>,
    pub start: When,
    pub end: When,
    pub rrule: Option<String>,
    pub rdates: Vec<When>,
    pub exdates: Vec<When>,
    pub recurrence_id: Option<When>,
    pub cancelled: bool,
}

impl Event {
    pub fn from_component(event: &Component, local: Tz) -> Option<Self> {
        let start = When::parse(event.property("DTSTART")?, local)?;
        let end = event
            .property("DTEND")
            .and_then(|property| When::parse(property, local))
            .or_else(|| {
                let duration = parse_duration(&event.property("DURATION")?.value)?;
                Some(start.same_form(start.in_zone(local) + duration))
            })
            .filter(|end| end.in_zone(local) > start.in_zone(local))
            .unwrap_or_else(|| match start {
                // RFC 5545 3.6.1: a date without an end lasts the day, a time lasts no time.
                When::Date(date) => When::Date(date + Duration::days(1)),
                other => other,
            });
        Some(Self {
            uid: event.text("UID").unwrap_or_default(),
            summary: event.text("SUMMARY").unwrap_or_default(),
            location: event.text("LOCATION").unwrap_or_default(),
            description: event.text("DESCRIPTION").unwrap_or_default(),
            organizer: event.property("ORGANIZER").and_then(Attendee::from_property),
            attendees: event.properties_named("ATTENDEE").filter_map(Attendee::from_property).collect(),
            conference: conference_link(event),
            start,
            end,
            rrule: event.property("RRULE").map(|rule| rule.value.clone()),
            rdates: event
                .properties_named("RDATE")
                .flat_map(|property| When::parse_list(property, local))
                .collect(),
            exdates: event
                .properties_named("EXDATE")
                .flat_map(|property| When::parse_list(property, local))
                .collect(),
            recurrence_id: event
                .property("RECURRENCE-ID")
                .and_then(|property| When::parse(property, local)),
            cancelled: event
                .property("STATUS")
                .is_some_and(|status| status.value.eq_ignore_ascii_case("CANCELLED")),
        })
    }

    pub fn all_day(&self) -> bool {
        self.start.is_date()
    }

    pub fn is_series(&self) -> bool {
        self.rrule.is_some() || !self.rdates.is_empty()
    }

    fn length(&self, local: Tz) -> Duration {
        self.end.in_zone(local) - self.start.in_zone(local)
    }

    /// The starts of this series' occurrences that overlap `from..to`, in the start's form.
    fn occurrence_starts(&self, from: DateTime<Tz>, to: DateTime<Tz>, local: Tz) -> Vec<When> {
        let length = self.length(local);
        let overlaps = |start: DateTime<Tz>| start < to && start + length > from;
        let Some(rule) = &self.rrule else {
            let mut starts = vec![self.start];
            starts.extend(self.rdates.iter().copied());
            return starts
                .into_iter()
                .filter(|start| overlaps(start.in_zone(local)))
                .collect();
        };
        let zone = match self.start {
            When::Utc(_) => rrule::Tz::UTC,
            When::Zoned(_, tz) => rrule::Tz::Tz(tz),
            When::Date(_) | When::Floating(_) => rrule::Tz::Tz(local),
        };
        let dtstart = self.start.in_zone(local).with_timezone(&zone);
        let Some(set) = rule_set(rule, dtstart) else {
            tracing::debug!("a recurrence rule that can't be read; showing the first occurrence");
            return vec![self.start].into_iter().filter(|s| overlaps(s.in_zone(local))).collect();
        };
        let window_start = (from - length - Duration::days(1)).with_timezone(&zone);
        let window_end = to.with_timezone(&zone);
        let mut set = set.after(window_start).before(window_end);
        for rdate in &self.rdates {
            set = set.rdate(rdate.in_zone(local).with_timezone(&zone));
        }
        set.all(MAX_OCCURRENCES)
            .dates
            .into_iter()
            .map(|start| self.start.same_form(start.with_timezone(&local)))
            .filter(|start| overlaps(start.in_zone(local)))
            .collect()
    }
}

fn rule_set(rule: &str, dtstart: DateTime<rrule::Tz>) -> Option<rrule::RRuleSet> {
    let rule: rrule::RRule<rrule::Unvalidated> = until_in_utc(rule, &dtstart).parse().ok()?;
    rule.build(dtstart).ok()
}

/// The rrule crate takes UNTIL only in UTC; files also write a date (the series' last day) or a
/// wall time in the series' zone.
fn until_in_utc(rule: &str, dtstart: &DateTime<rrule::Tz>) -> String {
    rule.split(';')
        .map(|part| {
            let Some(value) = part.strip_prefix("UNTIL=") else {
                return part.to_owned();
            };
            let wall = if value.len() == 8 {
                NaiveDate::parse_from_str(value, "%Y%m%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(23, 59, 59))
            } else if value.ends_with(['Z', 'z']) {
                None
            } else {
                NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()
            };
            match wall.and_then(|wall| dtstart.timezone().from_local_datetime(&wall).earliest()) {
                Some(until) => format!("UNTIL={}", until.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")),
                None => part.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// An RFC 5545 DURATION: `P1D`, `PT1H30M`, `-PT15M`, `P2W`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let text = text.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut number = String::new();
    for character in text.chars() {
        match character {
            '0'..='9' => number.push(character),
            'T' => {}
            unit => {
                let value: i64 = number.parse().ok()?;
                number.clear();
                total += match unit {
                    'W' => Duration::weeks(value),
                    'D' => Duration::days(value),
                    'H' => Duration::hours(value),
                    'M' => Duration::minutes(value),
                    'S' => Duration::seconds(value),
                    _ => return None,
                };
            }
        }
    }
    number.is_empty().then_some(total * sign)
}

/// One occurrence to show.
#[derive(Clone, Debug)]
pub struct Occurrence {
    pub uid: String,
    pub summary: String,
    pub location: String,
    pub description: String,
    pub organizer: Option<Attendee>,
    pub attendees: Vec<Attendee>,
    pub conference: Option<String>,
    pub start: DateTime<Tz>,
    pub end: DateTime<Tz>,
    pub all_day: bool,
    /// Part of a series: edits and removal ask "this event or all events".
    pub recurring: bool,
    /// Which occurrence of a series this is (its original start); `None` for a single event.
    pub recurrence_id: Option<When>,
}

/// The occurrences of every event in a calendar object that overlap `from..to`.
pub fn occurrences(calendar: &Component, from: DateTime<Tz>, to: DateTime<Tz>, local: Tz) -> Vec<Occurrence> {
    let events: Vec<Event> = calendar
        .children_named("VEVENT")
        .filter_map(|event| Event::from_component(event, local))
        .collect();
    let overrides: Vec<&Event> = events.iter().filter(|e| e.recurrence_id.is_some()).collect();
    let mut found = Vec::new();
    for master in events.iter().filter(|e| e.recurrence_id.is_none()) {
        let series = master.is_series();
        let length = master.length(local);
        for start in master.occurrence_starts(from, to, local) {
            let excluded = master.exdates.iter().any(|ex| ex.same_occurrence(start, local));
            let moved = overrides.iter().any(|o| {
                o.recurrence_id
                    .is_some_and(|id| id.same_occurrence(start, local))
            });
            if excluded || moved || master.cancelled {
                continue;
            }
            let begins = start.in_zone(local);
            found.push(occurrence(master, begins, begins + length, series.then_some(start), series));
        }
    }
    for moved in overrides {
        let (start, end) = (moved.start.in_zone(local), moved.end.in_zone(local));
        if !moved.cancelled && start < to && end > from {
            found.push(occurrence(moved, start, end, moved.recurrence_id, true));
        }
    }
    found
}

fn occurrence(
    event: &Event,
    start: DateTime<Tz>,
    end: DateTime<Tz>,
    recurrence_id: Option<When>,
    recurring: bool,
) -> Occurrence {
    Occurrence {
        uid: event.uid.clone(),
        summary: event.summary.clone(),
        location: event.location.clone(),
        description: event.description.clone(),
        organizer: event.organizer.clone(),
        attendees: event.attendees.clone(),
        conference: event.conference.clone(),
        start,
        end,
        all_day: event.all_day(),
        recurring,
        recurrence_id,
    }
}

/// How an invitee answered (PARTSTAT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Waiting,
    Accepted,
    Declined,
    Tentative,
    Delegated,
}

impl Answer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "Hasn't answered",
            Self::Accepted => "Going",
            Self::Declined => "Not going",
            Self::Tentative => "Maybe",
            Self::Delegated => "Delegated",
        }
    }
}

/// An organizer or invitee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attendee {
    pub email: String,
    /// The CN parameter, or empty.
    pub name: String,
    pub answer: Answer,
}

impl Attendee {
    fn from_property(property: &Property) -> Option<Self> {
        let value = property.value.trim();
        let email = value
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
            .map_or(value, |_| &value[7..])
            .trim()
            .to_owned();
        if email.is_empty() {
            return None;
        }
        let answer = match property.param("PARTSTAT").map(str::to_ascii_uppercase).as_deref() {
            Some("ACCEPTED") => Answer::Accepted,
            Some("DECLINED") => Answer::Declined,
            Some("TENTATIVE") => Answer::Tentative,
            Some("DELEGATED") => Answer::Delegated,
            _ => Answer::Waiting,
        };
        Some(Self { email, name: property.param("CN").unwrap_or_default().to_owned(), answer })
    }

    /// The name when there is one, else the address.
    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }
}

/// Hosts whose links are video calls, for links that sit in the location or notes.
const CALL_HOSTS: &[&str] = &[
    "meet.google.com",
    "zoom.us",
    "zoom.com",
    "teams.microsoft.com",
    "teams.live.com",
    "meet.jit.si",
    "whereby.com",
    "webex.com",
    "facetime.apple.com",
    "meet.proton.me",
];

fn is_call_link(link: &str) -> bool {
    let Some(rest) = link.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default().to_ascii_lowercase();
    CALL_HOSTS.iter().any(|call| host == *call || host.ends_with(&format!(".{call}")))
}

/// The event's video call: RFC 7986's CONFERENCE, Google Meet's own property, a URL that is a
/// call, or the first call link in the location or the notes.
pub fn conference_link(event: &Component) -> Option<String> {
    let conference = event
        .properties_named("CONFERENCE")
        .find(|p| p.param("FEATURE").is_none_or(|f| f.to_ascii_uppercase().contains("VIDEO")))
        .or_else(|| event.property("CONFERENCE"))
        .map(|p| p.value.trim().to_owned());
    let google = event.property("X-GOOGLE-CONFERENCE").map(|p| p.value.trim().to_owned());
    let url = event.property("URL").map(|p| p.value.trim().to_owned()).filter(|link| is_call_link(link));
    conference
        .or(google)
        .or(url)
        .or_else(|| ["LOCATION", "DESCRIPTION"].iter().find_map(|name| call_link_in(&event.text(name)?)))
}

/// The first video call link in some text.
pub fn call_link_in(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '(' | ')'))
        .map(|word| word.trim_end_matches(['.', ',', ';']))
        .find(|word| is_call_link(word))
        .map(str::to_owned)
}

/// Writes the invitees (keeping the answers of those already invited) and the video call.
fn set_people_and_call(event: &mut Component, draft: &Draft) {
    let existing: Vec<Property> = event.properties_named("ATTENDEE").cloned().collect();
    let email_of = |property: &Property| Attendee::from_property(property).map(|a| a.email.to_ascii_lowercase());
    let mut attendees = Vec::new();
    for email in &draft.attendees {
        let wanted = email.trim().to_ascii_lowercase();
        let property = existing
            .iter()
            .find(|property| email_of(property).as_deref() == Some(wanted.as_str()))
            .cloned()
            .unwrap_or_else(|| {
                Property::new("ATTENDEE", format!("mailto:{}", email.trim()))
                    .with_param("ROLE", "REQ-PARTICIPANT")
                    .with_param("PARTSTAT", "NEEDS-ACTION")
                    .with_param("RSVP", "TRUE")
            });
        attendees.push(property);
    }
    let place = event.properties.iter().position(|p| p.name.eq_ignore_ascii_case("ATTENDEE"));
    event.remove("ATTENDEE");
    let at = place.unwrap_or(event.properties.len()).min(event.properties.len());
    event.properties.splice(at..at, attendees);
    if !draft.attendees.is_empty()
        && event.property("ORGANIZER").is_none()
        && let Some((name, email)) = &draft.organizer
    {
        let mut organizer = Property::new("ORGANIZER", format!("mailto:{email}"));
        if !name.trim().is_empty() {
            organizer = organizer.with_param("CN", name.trim());
        }
        event.properties.push(organizer);
    }

    let before = conference_link(event);
    let link = draft.conference.trim();
    if before.as_deref() == Some(link) {
        return;
    }
    event.remove("CONFERENCE");
    event.remove("X-GOOGLE-CONFERENCE");
    if before.is_some() && event.property("URL").is_some_and(|url| Some(url.value.trim()) == before.as_deref()) {
        event.remove("URL");
    }
    if !link.is_empty() {
        event.properties.push(
            Property::new("CONFERENCE", link)
                .with_param("VALUE", "URI")
                .with_param("FEATURE", "VIDEO")
                .with_param("LABEL", "Video call"),
        );
        // Calendars that don't read CONFERENCE (Apple Calendar) show URL.
        if event.property("URL").is_none() {
            event.properties.push(Property::new("URL", link));
        }
    }
}

/// What the editor sends back. Times are wall times in the local zone; an all-day event's end
/// date is its last day (inclusive), as people say it.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub summary: String,
    pub location: String,
    pub description: String,
    /// The invitees' addresses; those already invited keep their names and answers.
    pub attendees: Vec<String>,
    /// The video call's link, or empty.
    pub conference: String,
    /// Who invites: the account's name and address, written as ORGANIZER when there are
    /// invitees and the event has no organizer yet.
    pub organizer: Option<(String, String)>,
    pub all_day: bool,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

impl Draft {
    fn times(&self, local: Tz) -> (When, When) {
        if self.all_day {
            let last = self.end.date().max(self.start.date());
            (When::Date(self.start.date()), When::Date(last + Duration::days(1)))
        } else {
            let start = resolve(local, self.start);
            let end = resolve(local, self.end).max(start);
            (When::Utc(start.with_timezone(&Utc)), When::Utc(end.with_timezone(&Utc)))
        }
    }
}

/// Which events an edit or removal of a recurring occurrence reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The single event, or the whole series.
    All,
    /// Only the occurrence with this RECURRENCE-ID.
    This(When),
}

/// A new event's UID: a random UUID, which also names its file on the server.
pub fn new_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A new calendar object holding one event.
pub fn new_calendar(uid: &str, draft: &Draft, local: Tz) -> Component {
    let mut calendar = Component::new("VCALENDAR");
    calendar.properties.push(Property::new("VERSION", "2.0"));
    calendar
        .properties
        .push(Property::new("PRODID", "-//Signet//Signet Calendar//EN"));
    let mut event = Component::new("VEVENT");
    event.properties.push(Property::new("UID", ics::escape(uid)));
    event.properties.push(stamp("DTSTAMP"));
    event.properties.push(stamp("CREATED"));
    fill(&mut event, draft, local);
    calendar.children.push(event);
    calendar
}

fn stamp(name: &str) -> Property {
    Property::new(name, Utc::now().format("%Y%m%dT%H%M%SZ").to_string())
}

fn fill(event: &mut Component, draft: &Draft, local: Tz) {
    let (start, end) = draft.times(local);
    event.set_text("SUMMARY", &draft.summary);
    event.set_text("LOCATION", &draft.location);
    event.set_text("DESCRIPTION", &draft.description);
    set_people_and_call(event, draft);
    event.set(start.property("DTSTART"));
    event.remove("DURATION");
    event.set(end.property("DTEND"));
    touch(event);
}

/// Marks a changed event: a new DTSTAMP and LAST-MODIFIED, and the next SEQUENCE.
fn touch(event: &mut Component) {
    let sequence = event
        .property("SEQUENCE")
        .and_then(|property| property.value.trim().parse::<u32>().ok())
        .map_or(0, |sequence| sequence + 1);
    event.set(Property::new("SEQUENCE", sequence.to_string()));
    event.set(stamp("DTSTAMP"));
    event.set(stamp("LAST-MODIFIED"));
}

fn master_index(calendar: &Component) -> Option<usize> {
    calendar.children.iter().position(|child| {
        child.name.eq_ignore_ascii_case("VEVENT") && child.property("RECURRENCE-ID").is_none()
    })
}

fn override_index(calendar: &Component, id: When, local: Tz) -> Option<usize> {
    calendar.children.iter().position(|child| {
        child.name.eq_ignore_ascii_case("VEVENT")
            && child
                .property("RECURRENCE-ID")
                .and_then(|property| When::parse(property, local))
                .is_some_and(|found| found.same_occurrence(id, local))
    })
}

/// Applies an edit made on `occurrence`. For a whole series the times move by as much as the
/// occurrence moved, so every occurrence keeps its day pattern and the series its zone.
pub fn edit(calendar: &mut Component, occurrence: &Occurrence, draft: &Draft, scope: Scope, local: Tz) -> Result<(), EditError> {
    match scope {
        Scope::All => {
            let index = match occurrence.recurrence_id {
                Some(_) if occurrence.recurring => master_index(calendar),
                _ => calendar.children.iter().position(|child| {
                    child.name.eq_ignore_ascii_case("VEVENT")
                }),
            }
            .ok_or(EditError::Missing)?;
            let event = &mut calendar.children[index];
            let is_series = event.property("RRULE").is_some() || event.property("RDATE").is_some();
            if !is_series {
                fill(event, draft, local);
                return Ok(());
            }
            let original = Event::from_component(event, local).ok_or(EditError::Missing)?;
            let (new_start, new_end) = draft.times(local);
            let shift = new_start.in_zone(local) - occurrence.start;
            let length = new_end.in_zone(local) - new_start.in_zone(local);
            let (start, end) = if draft.all_day == original.all_day() {
                let start = original.start.same_form(original.start.in_zone(local) + shift);
                (start, start.same_form(start.in_zone(local) + length))
            } else {
                // Changing between all-day and timed restarts the series at the new start.
                event.remove("EXDATE");
                (new_start, new_end)
            };
            event.set_text("SUMMARY", &draft.summary);
            event.set_text("LOCATION", &draft.location);
            event.set_text("DESCRIPTION", &draft.description);
            set_people_and_call(event, draft);
            event.set(start.property("DTSTART"));
            event.remove("DURATION");
            event.set(end.property("DTEND"));
            touch(event);
            Ok(())
        }
        Scope::This(id) => {
            let index = match override_index(calendar, id, local) {
                Some(index) => index,
                None => {
                    let master = master_index(calendar).ok_or(EditError::Missing)?;
                    let mut moved = calendar.children[master].clone();
                    for name in ["RRULE", "RDATE", "EXDATE", "EXRULE", "CREATED"] {
                        moved.remove(name);
                    }
                    moved.set(id.property("RECURRENCE-ID"));
                    calendar.children.push(moved);
                    calendar.children.len() - 1
                }
            };
            fill(&mut calendar.children[index], draft, local);
            Ok(())
        }
    }
}

/// What removing an occurrence leaves.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// Nothing is left: delete the whole object.
    Everything,
    /// The object stays, with the occurrence excluded.
    Updated,
}

/// Removes one occurrence of a series: an EXDATE on the series, and its moved copy if any.
pub fn remove_occurrence(calendar: &mut Component, id: When, local: Tz) -> Result<Removal, EditError> {
    while let Some(index) = override_index(calendar, id, local) {
        calendar.children.remove(index);
    }
    let Some(master) = master_index(calendar) else {
        let any_left = calendar.children_named("VEVENT").next().is_some();
        return Ok(if any_left { Removal::Updated } else { Removal::Everything });
    };
    let event = &mut calendar.children[master];
    let form = Event::from_component(event, local).ok_or(EditError::Missing)?.start;
    event.properties.push(form.same_form(id.in_zone(local)).property("EXDATE"));
    touch(event);
    Ok(Removal::Updated)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("the event is no longer in its calendar")]
    Missing,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ics::parse;

    const BERLIN: Tz = chrono_tz::Europe::Berlin;

    fn at(day: u32, hour: u32) -> DateTime<Tz> {
        resolve(BERLIN, NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(hour, 0, 0).unwrap())
    }

    fn week() -> (DateTime<Tz>, DateTime<Tz>) {
        (at(12, 0), at(19, 0))
    }

    fn calendar(body: &str) -> Component {
        parse(&format!("BEGIN:VCALENDAR\nVERSION:2.0\n{body}END:VCALENDAR\n")).unwrap()
    }

    #[test]
    fn a_single_event_in_another_zone() {
        let cal = calendar(
            "BEGIN:VEVENT\nUID:a\nSUMMARY:Call\nDTSTART;TZID=America/New_York:20261014T090000\n\
             DURATION:PT1H30M\nEND:VEVENT\n",
        );
        let (from, to) = week();
        let found = occurrences(&cal, from, to, BERLIN);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, at(14, 15));
        assert_eq!(found[0].end - found[0].start, Duration::minutes(90));
        assert!(!found[0].recurring);
    }

    #[test]
    fn a_weekly_series_with_an_exception_and_a_moved_occurrence() {
        let cal = calendar(
            "BEGIN:VEVENT\nUID:s\nSUMMARY:Standup\nDTSTART;TZID=Europe/Berlin:20260901T100000\n\
             DTEND;TZID=Europe/Berlin:20260901T101500\nRRULE:FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR\n\
             EXDATE;TZID=Europe/Berlin:20261013T100000\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:s\nSUMMARY:Standup (late)\nRECURRENCE-ID;TZID=Europe/Berlin:20261014T100000\n\
             DTSTART;TZID=Europe/Berlin:20261014T110000\nDTEND;TZID=Europe/Berlin:20261014T111500\nEND:VEVENT\n",
        );
        let (from, to) = week();
        let mut found = occurrences(&cal, from, to, BERLIN);
        found.sort_by_key(|o| o.start);
        let starts: Vec<_> = found.iter().map(|o| o.start).collect();
        assert_eq!(starts, vec![at(12, 10), at(14, 11), at(15, 10), at(16, 10)]);
        assert_eq!(found[1].summary, "Standup (late)");
        assert!(found.iter().all(|o| o.recurring));
    }

    #[test]
    fn all_day_events_and_utc_until() {
        let cal = calendar(
            "BEGIN:VEVENT\nUID:d\nSUMMARY:Trip\nDTSTART;VALUE=DATE:20261015\nDTEND;VALUE=DATE:20261017\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:w\nSUMMARY:Bins\nDTSTART;VALUE=DATE:20260902\nRRULE:FREQ=WEEKLY;UNTIL=20261231\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:u\nSUMMARY:Gym\nDTSTART:20261001T160000Z\nDTEND:20261001T170000Z\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261015T235959Z\nEND:VEVENT\n",
        );
        let (from, to) = week();
        let found = occurrences(&cal, from, to, BERLIN);
        let trip = found.iter().find(|o| o.uid == "d").unwrap();
        assert!(trip.all_day);
        assert_eq!((trip.start, trip.end), (at(15, 0), at(17, 0)));
        assert_eq!(found.iter().filter(|o| o.uid == "w").count(), 1);
        let gym: Vec<_> = found.iter().filter(|o| o.uid == "u").collect();
        assert_eq!(gym.len(), 1);
        assert_eq!(gym[0].start, at(15, 18));
    }

    #[test]
    fn editing_a_series_shifts_it_and_keeps_its_zone() {
        let mut cal = calendar(
            "BEGIN:VEVENT\nUID:s\nSUMMARY:Standup\nDTSTART;TZID=Europe/Berlin:20260901T100000\n\
             DTEND;TZID=Europe/Berlin:20260901T101500\nRRULE:FREQ=WEEKLY\nATTENDEE:mailto:a@b.c\nEND:VEVENT\n",
        );
        let (from, to) = week();
        let occurrence = occurrences(&cal, from, to, BERLIN).remove(0);
        let draft = Draft {
            summary: "Standup".into(),
            location: "Room 1".into(),
            description: String::new(),
            attendees: vec!["a@b.c".into()],
            conference: String::new(),
            organizer: None,
            all_day: false,
            start: occurrence.start.naive_local() + Duration::minutes(30),
            end: occurrence.start.naive_local() + Duration::minutes(60),
        };
        edit(&mut cal, &occurrence, &draft, Scope::All, BERLIN).unwrap();
        let event = &cal.children[0];
        assert_eq!(event.property("DTSTART").unwrap().value, "20260901T103000");
        assert_eq!(event.property("DTSTART").unwrap().param("TZID"), Some("Europe/Berlin"));
        assert_eq!(event.property("DTEND").unwrap().value, "20260901T110000");
        assert_eq!(event.text("LOCATION").unwrap(), "Room 1");
        assert!(event.property("ATTENDEE").is_some());
        assert_eq!(event.property("SEQUENCE").unwrap().value, "0");
    }

    #[test]
    fn editing_and_removing_one_occurrence() {
        let mut cal = calendar(
            "BEGIN:VEVENT\nUID:s\nSUMMARY:Standup\nDTSTART;TZID=Europe/Berlin:20260901T100000\n\
             DTEND;TZID=Europe/Berlin:20260901T101500\nRRULE:FREQ=DAILY\nEND:VEVENT\n",
        );
        let (from, to) = week();
        let all = occurrences(&cal, from, to, BERLIN);
        assert_eq!(all.len(), 7);
        let tuesday = all.iter().find(|o| o.start == at(13, 10)).unwrap().clone();
        let draft = Draft {
            summary: "Moved".into(),
            location: String::new(),
            description: String::new(),
            attendees: Vec::new(),
            conference: String::new(),
            organizer: None,
            all_day: false,
            start: at(13, 15).naive_local(),
            end: at(13, 16).naive_local(),
        };
        edit(&mut cal, &tuesday, &draft, Scope::This(tuesday.recurrence_id.unwrap()), BERLIN).unwrap();
        let after = occurrences(&cal, from, to, BERLIN);
        assert_eq!(after.len(), 7);
        assert!(after.iter().any(|o| o.summary == "Moved" && o.start == at(13, 15)));

        let wednesday = after.iter().find(|o| o.start == at(14, 10)).unwrap().clone();
        let removal = remove_occurrence(&mut cal, wednesday.recurrence_id.unwrap(), BERLIN).unwrap();
        assert_eq!(removal, Removal::Updated);
        let after = occurrences(&cal, from, to, BERLIN);
        assert_eq!(after.len(), 6);
        let moved = after.iter().find(|o| o.summary == "Moved").unwrap().clone();
        remove_occurrence(&mut cal, moved.recurrence_id.unwrap(), BERLIN).unwrap();
        assert_eq!(occurrences(&cal, from, to, BERLIN).len(), 5);
    }

    #[test]
    fn a_new_all_day_event_ends_the_day_after_its_last_day() {
        let draft = Draft {
            summary: "Holiday".into(),
            location: String::new(),
            description: "Two\nlines".into(),
            attendees: Vec::new(),
            conference: String::new(),
            organizer: None,
            all_day: true,
            start: at(15, 0).naive_local(),
            end: at(16, 0).naive_local(),
        };
        let cal = new_calendar("x@signet", &draft, BERLIN);
        let reparsed = parse(&cal.to_ics()).unwrap();
        let event = &reparsed.children[0];
        assert_eq!(event.property("DTEND").unwrap().value, "20261017");
        assert_eq!(event.text("DESCRIPTION").unwrap(), "Two\nlines");
        let found = occurrences(&reparsed, week().0, week().1, BERLIN);
        assert_eq!((found[0].start, found[0].end), (at(15, 0), at(17, 0)));
    }

    #[test]
    fn invitees_keep_their_answers_and_calls_are_found() {
        let mut cal = calendar(
            "BEGIN:VEVENT\nUID:i\nSUMMARY:Plan\nDTSTART:20261014T150000Z\nDTEND:20261014T160000Z\n\
             ORGANIZER;CN=Grace:mailto:grace@example.com\n\
             ATTENDEE;CN=Ada;PARTSTAT=ACCEPTED:mailto:ada@example.com\n\
             ATTENDEE;PARTSTAT=DECLINED:MAILTO:bob@example.com\n\
             DESCRIPTION:Join at https://us02web.zoom.us/j/123?pwd=x.\nEND:VEVENT\n",
        );
        let (from, to) = week();
        let found = occurrences(&cal, from, to, BERLIN).remove(0);
        assert_eq!(found.organizer.as_ref().unwrap().display(), "Grace");
        assert_eq!(found.attendees.len(), 2);
        assert_eq!(found.attendees[0].answer, Answer::Accepted);
        assert_eq!(found.attendees[1].email, "bob@example.com");
        assert_eq!(found.conference.as_deref(), Some("https://us02web.zoom.us/j/123?pwd=x"));

        let draft = Draft {
            summary: "Plan".into(),
            location: String::new(),
            description: String::new(),
            attendees: vec!["ADA@example.com".into(), "carol@example.com".into()],
            conference: "https://meet.google.com/abc-defg-hij".into(),
            organizer: Some(("Me".into(), "me@example.com".into())),
            all_day: false,
            start: found.start.naive_local(),
            end: found.end.naive_local(),
        };
        edit(&mut cal, &found, &draft, Scope::All, BERLIN).unwrap();
        let after = occurrences(&cal, from, to, BERLIN).remove(0);
        let emails: Vec<_> = after.attendees.iter().map(|a| (a.email.as_str(), a.answer)).collect();
        assert_eq!(emails, vec![("ada@example.com", Answer::Accepted), ("carol@example.com", Answer::Waiting)]);
        assert_eq!(after.organizer.unwrap().email, "grace@example.com", "an existing organizer stays");
        assert_eq!(after.conference.as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        let event = &cal.children[0];
        assert_eq!(event.property("CONFERENCE").unwrap().param("FEATURE"), Some("VIDEO"));

        let mut new = new_calendar("n", &draft, BERLIN);
        assert_eq!(Event::from_component(&new.children[0], BERLIN).unwrap().organizer.unwrap().email, "me@example.com");
        let cleared = Draft { attendees: Vec::new(), conference: String::new(), ..draft };
        let occurrence = occurrences(&new, from, to, BERLIN).remove(0);
        edit(&mut new, &occurrence, &cleared, Scope::All, BERLIN).unwrap();
        let event = &new.children[0];
        assert!(event.property("ATTENDEE").is_none());
        assert!(event.property("CONFERENCE").is_none() && event.property("URL").is_none());
    }

    #[test]
    fn durations_and_zones() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1W"), Some(Duration::days(7)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("P1X"), None);
        assert_eq!(zone("/mozilla.org/20050126_1/Europe/Berlin"), Some(BERLIN));
        assert_eq!(zone("Pacific Standard Time"), Some(chrono_tz::America::Los_Angeles));
    }
}
