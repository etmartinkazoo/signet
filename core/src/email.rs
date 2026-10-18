//! An event made from an email: what Sealwax's plugin protocol sends for a message action
//! (`signet --from-sealwax`), turned into a draft for the editor. The subject becomes the
//! title, the people on the message the invitees (except the account itself), a call link in
//! the body the video call, and the sender and text the notes.

use crate::event::{Draft, call_link_in};
use chrono::{Duration, NaiveDateTime, Timelike};
use serde::Deserialize;

/// The longest part of the email kept in the notes, in characters.
const MAX_NOTES: usize = 4000;

/// What Sealwax sends a plugin (version 1). Fields Signet doesn't use are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct PluginInput {
    pub version: u32,
    #[serde(default)]
    pub account: Option<PluginAccount>,
    #[serde(default)]
    pub messages: Vec<PluginMessage>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PluginAccount {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub address: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct PluginMessage {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub from: Vec<PluginAddress>,
    #[serde(default)]
    pub to: Vec<PluginAddress>,
    #[serde(default)]
    pub cc: Vec<PluginAddress>,
    #[serde(default)]
    /// Unix seconds.
    pub date: Option<i64>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PluginAddress {
    #[serde(default)]
    pub name: String,
    pub address: String,
}

impl PluginAddress {
    fn display(&self) -> String {
        if self.name.trim().is_empty() {
            self.address.clone()
        } else {
            format!("{} <{}>", self.name.trim(), self.address)
        }
    }
}

/// The subject without reply and forward prefixes, however many and in which language.
fn clean_subject(subject: &str) -> String {
    let mut subject = subject.trim();
    loop {
        let lower = subject.to_ascii_lowercase();
        let prefix = ["re:", "fw:", "fwd:", "aw:", "wg:", "sv:", "vs:", "tr:", "rif:"]
            .iter()
            .find(|prefix| lower.starts_with(*prefix));
        match prefix {
            Some(prefix) => subject = subject[prefix.len()..].trim_start(),
            None => return subject.to_owned(),
        }
    }
}

/// A draft for a new event about `message`, starting at the next full hour after `now`.
pub fn draft(input: &PluginInput, message: &PluginMessage, now: NaiveDateTime) -> Draft {
    let me = input
        .account
        .as_ref()
        .map(|account| account.address.to_ascii_lowercase())
        .unwrap_or_default();
    let mut attendees: Vec<String> = Vec::new();
    for person in message.from.iter().chain(&message.to).chain(&message.cc) {
        let address = person.address.trim();
        let known = attendees.iter().any(|a| a.eq_ignore_ascii_case(address));
        if !address.is_empty() && !known && address.to_ascii_lowercase() != me {
            attendees.push(address.to_owned());
        }
    }
    let text = message.text.as_deref().unwrap_or_default().trim();
    let mut notes = String::new();
    if let Some(sender) = message.from.first() {
        notes.push_str(&format!("From: {}\n", sender.display()));
    }
    if !message.subject.trim().is_empty() {
        notes.push_str(&format!("Subject: {}\n", message.subject.trim()));
    }
    if !text.is_empty() {
        notes.push('\n');
        notes.extend(text.chars().take(MAX_NOTES));
        if text.chars().count() > MAX_NOTES {
            notes.push('…');
        }
    }
    let start = (now + Duration::hours(1))
        .with_minute(0)
        .and_then(|time| time.with_second(0))
        .unwrap_or(now);
    Draft {
        summary: clean_subject(&message.subject),
        location: String::new(),
        description: notes,
        attendees,
        conference: call_link_in(text).unwrap_or_default(),
        organizer: None,
        all_day: false,
        start,
        end: start + Duration::hours(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_email_becomes_a_draft() {
        let input: PluginInput = serde_json::from_str(
            r#"{"version": 1, "event": "action", "action": "create-event",
                "account": {"key": "work", "address": "ada@example.com", "label": "Work"},
                "messages": [{"subject": "Re: RE: Fwd: Planning call",
                  "from": [{"name": "Grace Hopper", "address": "grace@example.com"}],
                  "to": [{"name": "", "address": "Ada@example.com"}, {"name": "Bob", "address": "bob@example.com"}],
                  "cc": [{"name": "", "address": "grace@example.com"}],
                  "date": 1792314720, "folder": "INBOX", "raw": null,
                  "text": "Let's talk Thursday.\nJoin: https://meet.google.com/abc-defg-hij\nThanks"}]}"#,
        )
        .unwrap();
        let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).unwrap().and_hms_opt(10, 25, 0).unwrap();
        let draft = draft(&input, &input.messages[0], now);
        assert_eq!(draft.summary, "Planning call");
        assert_eq!(draft.attendees, vec!["grace@example.com", "bob@example.com"]);
        assert_eq!(draft.conference, "https://meet.google.com/abc-defg-hij");
        assert!(draft.description.starts_with("From: Grace Hopper <grace@example.com>\nSubject: Re: RE: Fwd: Planning call\n\nLet's talk"));
        assert_eq!(draft.start.format("%H:%M").to_string(), "11:00");
        assert_eq!(draft.end - draft.start, Duration::hours(1));
    }

    #[test]
    fn a_bare_message_still_makes_a_draft() {
        let input: PluginInput = serde_json::from_str(r#"{"version": 1, "messages": [{}]}"#).unwrap();
        let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).unwrap().and_hms_opt(23, 30, 0).unwrap();
        let draft = draft(&input, &input.messages[0], now);
        assert!(draft.summary.is_empty() && draft.attendees.is_empty() && draft.conference.is_empty());
        assert_eq!(draft.start.format("%d %H:%M").to_string(), "19 00:00");
    }
}
