//! iCalendar (RFC 5545) text as a tree of components that round-trips: Signet edits only the
//! properties it shows (summary, times, location, notes) and writes every other line back as it
//! came, so attendees, alarms and a server's own properties survive an update.

use std::fmt::Write as _;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub properties: Vec<Property>,
    pub children: Vec<Component>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    pub name: String,
    /// Parameter names upper-cased, values unquoted.
    pub params: Vec<(String, String)>,
    /// The raw value, still escaped for TEXT properties.
    pub value: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("line {0} is not a property")]
    Line(usize),
    #[error("END:{0} without a matching BEGIN")]
    Unbalanced(String),
    #[error("no VCALENDAR")]
    Empty,
}

impl Property {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            params: Vec::new(),
            value: value.into(),
        }
    }

    pub fn with_param(mut self, name: &str, value: &str) -> Self {
        self.params.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The value of a TEXT property, unescaped.
    pub fn text(&self) -> String {
        unescape(&self.value)
    }
}

impl Component {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties
            .iter()
            .find(|property| property.name.eq_ignore_ascii_case(name))
    }

    pub fn properties_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> {
        self.properties
            .iter()
            .filter(move |property| property.name.eq_ignore_ascii_case(name))
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.property(name).map(Property::text)
    }

    pub fn remove(&mut self, name: &str) {
        self.properties
            .retain(|property| !property.name.eq_ignore_ascii_case(name));
    }

    /// Replaces every `name` property with `property`, keeping the first one's place.
    pub fn set(&mut self, property: Property) {
        let place = self
            .properties
            .iter()
            .position(|existing| existing.name.eq_ignore_ascii_case(&property.name));
        self.remove(&property.name);
        match place {
            Some(index) => self.properties.insert(index.min(self.properties.len()), property),
            None => self.properties.push(property),
        }
    }

    /// Sets a TEXT property, or removes it when `text` is empty.
    pub fn set_text(&mut self, name: &str, text: &str) {
        if text.trim().is_empty() {
            self.remove(name);
        } else {
            self.set(Property::new(name, escape(text)));
        }
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> {
        self.children
            .iter()
            .filter(move |child| child.name.eq_ignore_ascii_case(name))
    }

    pub fn to_ics(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        fold_into(out, &format!("BEGIN:{}", self.name));
        for property in &self.properties {
            let mut line = property.name.clone();
            for (name, value) in &property.params {
                let quote = value.contains([':', ';', ',']);
                if quote {
                    let _ = write!(line, ";{name}=\"{value}\"");
                } else {
                    let _ = write!(line, ";{name}={value}");
                }
            }
            line.push(':');
            line.push_str(&property.value);
            fold_into(out, &line);
        }
        for child in &self.children {
            child.write(out);
        }
        fold_into(out, &format!("END:{}", self.name));
    }
}

/// Parses a whole iCalendar object; returns its top component (normally VCALENDAR).
pub fn parse(text: &str) -> Result<Component, ParseError> {
    let mut stack: Vec<Component> = Vec::new();
    let mut root = None;
    for (number, line) in unfold(text).into_iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let property = parse_line(&line).ok_or(ParseError::Line(number + 1))?;
        if property.name.eq_ignore_ascii_case("BEGIN") {
            stack.push(Component::new(&property.value.to_ascii_uppercase()));
        } else if property.name.eq_ignore_ascii_case("END") {
            let done = stack
                .pop()
                .filter(|open| open.name.eq_ignore_ascii_case(&property.value))
                .ok_or_else(|| ParseError::Unbalanced(property.value.clone()))?;
            match stack.last_mut() {
                Some(parent) => parent.children.push(done),
                None => {
                    root.get_or_insert(done);
                }
            }
        } else if let Some(open) = stack.last_mut() {
            open.properties.push(property);
        }
    }
    root.ok_or(ParseError::Empty)
}

fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => lines.push(raw.to_owned()),
        }
    }
    lines
}

fn parse_line(line: &str) -> Option<Property> {
    // The name and parameters end at the first colon outside quotes.
    let mut in_quotes = false;
    let mut colon = None;
    for (index, character) in line.char_indices() {
        match character {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(index);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = split_unquoted(head, ';').into_iter();
    let name = parts.next()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = parts
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.trim().to_ascii_uppercase(), value.trim_matches('"').to_owned()))
        })
        .collect();
    Some(Property {
        name,
        params,
        value: value.to_owned(),
    })
}

fn split_unquoted(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if character == '"' {
            in_quotes = !in_quotes;
        } else if character == separator && !in_quotes {
            parts.push(&text[start..index]);
            start = index + 1;
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Folds a content line at 75 octets, never inside a UTF-8 character, ending it with CRLF.
fn fold_into(out: &mut String, line: &str) {
    let mut width = 0;
    for character in line.chars() {
        let size = character.len_utf8();
        if width + size > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(character);
        width += size;
    }
    out.push_str("\r\n");
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:1@x\r\n\
        SUMMARY:Lunch\\, with Ada\r\nDTSTART;TZID=Europe/Berlin:20261018T120000\r\n\
        ATTENDEE;CN=\"Lovelace, Ada\";PARTSTAT=ACCEPTED:mailto:ada@example.com\r\n\
        DESCRIPTION:A long line that goes on and on so that it has to be folded by\r\n  the writer\r\n\
        BEGIN:VALARM\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_params_quotes_and_folds() {
        let calendar = parse(SAMPLE).unwrap();
        let event = calendar.children_named("VEVENT").next().unwrap();
        assert_eq!(event.text("SUMMARY").unwrap(), "Lunch, with Ada");
        let start = event.property("DTSTART").unwrap();
        assert_eq!(start.param("tzid"), Some("Europe/Berlin"));
        assert_eq!(start.value, "20261018T120000");
        let attendee = event.property("ATTENDEE").unwrap();
        assert_eq!(attendee.param("CN"), Some("Lovelace, Ada"));
        assert_eq!(attendee.value, "mailto:ada@example.com");
        assert!(event.text("DESCRIPTION").unwrap().ends_with("folded by the writer"));
        assert_eq!(event.children_named("VALARM").count(), 1);
    }

    #[test]
    fn round_trips() {
        let calendar = parse(SAMPLE).unwrap();
        let again = parse(&calendar.to_ics()).unwrap();
        assert_eq!(calendar, again);
        for line in calendar.to_ics().split("\r\n") {
            assert!(line.len() <= 75, "{line}");
        }
    }

    #[test]
    fn text_escapes_round_trip() {
        let text = "a;b,c\\d\nnext";
        assert_eq!(unescape(&escape(text)), text);
    }
}
