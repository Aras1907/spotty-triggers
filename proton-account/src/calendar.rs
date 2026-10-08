//! Proton Calendar: your calendars and their events, decrypted on this
//! computer. Read-only.

use crate::client::{Client, array, b64};
use crate::error::{Error, Result};
use crate::ical::{self, EventText, Series};
use crate::pgp::Key;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct CalendarInfo {
    pub id: String,
    pub name: String,
    /// "#rrggbb"
    pub color: String,
    /// Hidden in Proton's own calendar view.
    pub hidden: bool,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub id: String,
    pub calendar: String,
    pub color: String,
    pub title: String,
    pub location: String,
    pub description: String,
    /// Unix seconds. All-day events run from UTC midnight to UTC midnight.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
}

const PAGE_SIZE: usize = 100;

impl Client {
    pub fn calendars(&self) -> Result<Vec<CalendarInfo>> {
        let answer = self.api.get("calendar/v1", &[])?;
        let mut out = Vec::new();
        for calendar in array(&answer, "/Calendars") {
            let Some(id) = calendar.get("ID").and_then(Value::as_str) else { continue };
            let member = array(calendar, "/Members").into_iter().next();
            let text = |key: &str| member.and_then(|m| m.get(key)).and_then(Value::as_str).unwrap_or_default().to_owned();
            out.push(CalendarInfo {
                id: id.to_owned(),
                name: text("Name"),
                color: text("Color"),
                hidden: member.and_then(|m| m.get("Display")).and_then(Value::as_u64) == Some(0),
            });
        }
        Ok(out)
    }

    /// Events of every visible calendar that overlap `[from, to)` (unix
    /// seconds), repeating events expanded, sorted by start.
    pub fn calendar_events(&self, from: i64, to: i64) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut first_error = None;
        let mut any_ok = false;
        for calendar in self.calendars()?.into_iter().filter(|c| !c.hidden) {
            match self.calendar_events_of(&calendar, from, to) {
                Ok(found) => {
                    any_ok = true;
                    events.extend(found);
                }
                // One unreadable calendar (a subscription, say) shouldn't hide the rest.
                Err(error) if error.is_signed_out() => return Err(error),
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        if !any_ok {
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.title.cmp(&b.title)));
        Ok(events)
    }

    fn calendar_events_of(&self, calendar: &CalendarInfo, from: i64, to: i64) -> Result<Vec<Event>> {
        let keys = self.calendar_keys_of(&calendar.id)?;
        let ring = self.keyring()?;
        let address_keys = ring.all_address_keys();

        // Part-day / full-day events inside the window, and repeating ones that
        // began before it.
        let mut raw: Vec<Value> = Vec::new();
        let mut seen = HashSet::new();
        for kind in 0..4 {
            let mut page = 0usize;
            loop {
                let answer = self.api.get(
                    &format!("calendar/v1/{}/events", calendar.id),
                    &[
                        ("Start", from.to_string()),
                        ("End", to.to_string()),
                        ("Timezone", "UTC".into()),
                        ("Type", kind.to_string()),
                        ("PageSize", PAGE_SIZE.to_string()),
                        ("Page", page.to_string()),
                    ],
                )?;
                let events = array(&answer, "/Events");
                let count = events.len();
                for event in events {
                    if event.get("ID").and_then(Value::as_str).is_some_and(|id| seen.insert(id.to_owned())) {
                        raw.push(event.clone());
                    }
                }
                if count < PAGE_SIZE {
                    break;
                }
                page += 1;
            }
        }

        // Single edits of a repeating event replace that one occurrence.
        let replaced: HashSet<(String, i64)> = raw
            .iter()
            .filter_map(|e| Some((e.get("UID")?.as_str()?.to_owned(), e.get("RecurrenceID")?.as_i64()?)))
            .collect();

        let mut out = Vec::new();
        for event in &raw {
            let text = self.event_text(event, &keys, &address_keys).unwrap_or_default();
            let int = |key: &str| event.get(key).and_then(Value::as_i64);
            let (Some(start), Some(end)) = (int("StartTime"), int("EndTime")) else { continue };
            let all_day = int("FullDay") == Some(1);
            let uid = event.get("UID").and_then(Value::as_str).unwrap_or_default();
            let make = |start: i64, end: i64| Event {
                id: event.get("ID").and_then(Value::as_str).unwrap_or_default().to_owned(),
                calendar: calendar.name.clone(),
                color: calendar.color.clone(),
                title: text.summary.clone().unwrap_or_else(|| "(busy)".to_owned()),
                location: text.location.clone().unwrap_or_default(),
                description: text.description.clone().unwrap_or_default(),
                start,
                end,
                all_day,
            };
            match event.get("RRule").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                Some(rrule) => {
                    let mut excluded: Vec<i64> = event
                        .get("Exdates")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_i64).collect())
                        .unwrap_or_default();
                    excluded.extend(replaced.iter().filter(|(u, _)| u == uid).map(|(_, t)| *t));
                    let series = Series {
                        start,
                        end,
                        timezone: event.get("StartTimezone").and_then(Value::as_str).unwrap_or("UTC"),
                        all_day,
                        rrule,
                        excluded: &excluded,
                    };
                    out.extend(ical::occurrences(&series, from, to).into_iter().map(|(s, e)| make(s, e)));
                }
                None if end > from && start < to => out.push(make(start, end)),
                None => {}
            }
        }
        Ok(out)
    }

    /// Open the encrypted cards of one event.
    fn event_text(&self, event: &Value, calendar_keys: &[Key], address_keys: &[Key]) -> Result<EventText> {
        let packet_key = |field: &str, keys: &[Key]| -> Option<crate::pgp::SessionKey> {
            let packet = event.get(field).and_then(Value::as_str).filter(|p| !p.is_empty())?;
            self.pgp.session_key(&b64(packet).ok()?, keys).ok()
        };
        let shared = packet_key("SharedKeyPacket", calendar_keys).or_else(|| packet_key("SharedKeyPacket", address_keys));
        let own = packet_key("CalendarKeyPacket", calendar_keys).or_else(|| packet_key("CalendarKeyPacket", address_keys));

        let mut text = EventText::default();
        for (field, session) in [("SharedEvents", &shared), ("CalendarEvents", &own)] {
            for card in array(event, &format!("/{field}")) {
                let data = card.get("Data").and_then(Value::as_str).unwrap_or_default();
                let kind = card.get("Type").and_then(Value::as_u64).unwrap_or(0);
                // 0 clear text, 1 encrypted, 2 signed, 3 encrypted and signed.
                let ics = match kind {
                    0 | 2 => Some(data.to_owned()),
                    1 | 3 => session
                        .as_ref()
                        .zip(b64(data).ok())
                        .and_then(|(session, bytes)| self.pgp.decrypt_with(&bytes, session).ok())
                        .and_then(|bytes| String::from_utf8(bytes).ok()),
                    _ => None,
                };
                if let Some(ics) = ics {
                    text.merge(ical::parse_event_text(&ics));
                }
            }
        }
        Ok(text)
    }

    /// The unlocked keys of one calendar (cached).
    fn calendar_keys_of(&self, id: &str) -> Result<Arc<Vec<Key>>> {
        if let Some(keys) = self.calendar_keys.lock().unwrap().get(id) {
            return Ok(keys.clone());
        }
        let boot = self.api.get(&format!("calendar/v2/{id}/bootstrap"), &[])?;
        let ring = self.keyring()?;
        let address_keys = ring.all_address_keys();
        // The calendar's passphrase is encrypted to the member's address key.
        let mut passphrase = None;
        for member in array(&boot, "/Passphrase/MemberPassphrases") {
            if let Some(armored) = member.get("Passphrase").and_then(Value::as_str) {
                if let Ok(plain) = self.pgp.decrypt_armored(armored, &address_keys) {
                    passphrase = Some(plain);
                    break;
                }
            }
        }
        let passphrase = passphrase.ok_or_else(|| Error::Crypto("couldn't open the calendar passphrase".into()))?;
        let keys: Vec<Key> = array(&boot, "/Keys")
            .into_iter()
            .filter_map(|k| k.get("PrivateKey").and_then(Value::as_str))
            .filter_map(|armored| self.pgp.unlock(armored, &passphrase).ok())
            .collect();
        if keys.is_empty() {
            return Err(Error::Crypto("couldn't unlock the calendar keys".into()));
        }
        let keys = Arc::new(keys);
        self.calendar_keys.lock().unwrap().insert(id.to_owned(), keys.clone());
        Ok(keys)
    }
}
