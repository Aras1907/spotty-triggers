//! Reading one Proton Pass item into the fields the vault window shows.
//!
//! `pass-cli item view --output json` gives the whole item. The shape follows
//! Proton's item model (`proton-pass-types`): a `content` object holding one
//! variant (`login`, `credit_card`, `identity`, `wifi`, `ssh_key`, `alias`,
//! `custom`, `note`), a top-level `note`, and `extra_fields`. Keys are looked up
//! by name wherever they sit in the document, so a different nesting still
//! works, and a key that is missing is simply not shown.
//!
//! Every value that is a secret (passwords, card numbers, security codes, PINs,
//! private keys, hidden fields, identity numbers) is held in a `Zeroizing`
//! string, and the parsed JSON is wiped before it is dropped.
//!
//! Nothing here touches the network or the disk; the functions are pure, so
//! they are unit-tested with fixtures.

use crate::proton_pass::Kind;
use serde_json::Value;
use spotty_proton_account::{Zeroize, Zeroizing};

/// How a value is shown and what can be done with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Plain text: copyable.
    Text,
    /// A secret: masked until revealed, copyable.
    Secret,
    /// A web address: opened in the browser, copyable.
    Url,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub label: String,
    pub value: Zeroizing<String>,
    pub shown: Shown,
}

/// One item, as far as the vault window needs it.
#[derive(Debug, Clone)]
pub struct Detail {
    pub title: String,
    pub kind: Kind,
    pub fields: Vec<Field>,
    /// The login has a one-time code: the window asks Proton Pass for it.
    pub has_totp: bool,
}

/// The masked form of any secret: a fixed length, so it says nothing about the
/// real one.
pub fn mask() -> &'static str {
    "••••••••"
}

/// Parse `pass-cli item view --output json`. `text` is the client's answer.
pub fn parse(text: &str) -> Result<Detail, String> {
    let mut value: Value = serde_json::from_str(text).map_err(|_| "Unexpected answer from Proton Pass.".to_owned())?;
    let detail = parse_value(&value);
    wipe(&mut value);
    detail.ok_or_else(|| "Unexpected answer from Proton Pass.".to_owned())
}

fn parse_value(root: &Value) -> Option<Detail> {
    // The item may be wrapped in an "item" object (the client's envelope).
    let item = root.get("item").filter(|v| v.is_object()).unwrap_or(root);
    if !item.is_object() {
        return None;
    }
    let title = text_of(lookup(item, "title")).unwrap_or_default();
    let kind = Kind::parse(&text_of(lookup(item, "item_type")).unwrap_or_default().replace('-', "_"));
    let mut fields = Vec::new();
    let mut has_totp = false;
    let content = lookup(item, "content").unwrap_or(item);

    match kind {
        Kind::Login => {
            push(&mut fields, "Email", lookup(content, "email"), Shown::Text);
            push(&mut fields, "Username", lookup(content, "username"), Shown::Text);
            push(&mut fields, "Password", lookup(content, "password"), Shown::Secret);
            for url in urls(lookup(content, "urls")) {
                push_text(&mut fields, "Website", url, Shown::Url);
            }
            has_totp = text_of(lookup(content, "totp_uri")).is_some_and(|t| !t.is_empty());
        }
        Kind::CreditCard => {
            push(&mut fields, "Cardholder", lookup(content, "cardholder_name"), Shown::Text);
            push(&mut fields, "Card type", lookup(content, "card_type"), Shown::Text);
            push(&mut fields, "Number", lookup(content, "number"), Shown::Secret);
            push(&mut fields, "Expiry", lookup(content, "expiration_date"), Shown::Text);
            push(&mut fields, "Security code", lookup(content, "verification_number"), Shown::Secret);
            push(&mut fields, "PIN", lookup(content, "pin"), Shown::Secret);
        }
        Kind::Identity => {
            for (key, shown) in IDENTITY_FIELDS {
                push(&mut fields, &label_of(key), lookup(content, key), *shown);
            }
        }
        Kind::Alias => push(&mut fields, "Alias address", lookup(content, "email"), Shown::Text),
        Kind::Wifi => {
            push(&mut fields, "Network name", lookup(content, "ssid"), Shown::Text);
            push(&mut fields, "Password", lookup(content, "password"), Shown::Secret);
            push(&mut fields, "Security", lookup(content, "security"), Shown::Text);
        }
        Kind::SshKey => {
            push(&mut fields, "Private key", lookup(content, "private_key"), Shown::Secret);
            push(&mut fields, "Public key", lookup(content, "public_key"), Shown::Text);
        }
        Kind::Note | Kind::Custom => {}
    }
    if let Some(sections) = lookup(content, "sections").and_then(Value::as_array) {
        push_sections(&mut fields, sections);
    }
    if let Some(sections) = lookup(content, "extra_sections").and_then(Value::as_array) {
        push_sections(&mut fields, sections);
    }
    push_extra(&mut fields, lookup(item, "extra_fields"));
    push(&mut fields, "Notes", lookup(item, "note"), Shown::Text);

    Some(Detail { title, kind, fields, has_totp })
}

/// Identity fields, in the order the item model lists them. Numbers that
/// identify a person are secrets.
const IDENTITY_FIELDS: &[(&str, Shown)] = &[
    ("full_name", Shown::Text),
    ("email", Shown::Text),
    ("phone_number", Shown::Text),
    ("first_name", Shown::Text),
    ("middle_name", Shown::Text),
    ("last_name", Shown::Text),
    ("birthdate", Shown::Text),
    ("gender", Shown::Text),
    ("organization", Shown::Text),
    ("street_address", Shown::Text),
    ("zip_or_postal_code", Shown::Text),
    ("city", Shown::Text),
    ("state_or_province", Shown::Text),
    ("country_or_region", Shown::Text),
    ("floor", Shown::Text),
    ("county", Shown::Text),
    ("social_security_number", Shown::Secret),
    ("passport_number", Shown::Secret),
    ("license_number", Shown::Secret),
    ("website", Shown::Url),
    ("x_handle", Shown::Text),
    ("second_phone_number", Shown::Text),
    ("linkedin", Shown::Url),
    ("reddit", Shown::Url),
    ("facebook", Shown::Url),
    ("yahoo", Shown::Text),
    ("instagram", Shown::Url),
    ("company", Shown::Text),
    ("job_title", Shown::Text),
    ("personal_website", Shown::Url),
    ("work_phone_number", Shown::Text),
    ("work_email", Shown::Text),
];

/// "first_name" → "First name".
fn label_of(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn push(fields: &mut Vec<Field>, label: &str, value: Option<&Value>, shown: Shown) {
    if let Some(text) = text_of(value).filter(|t| !t.is_empty()) {
        push_text(fields, label, text, shown);
    }
}

fn push_text(fields: &mut Vec<Field>, label: &str, text: String, shown: Shown) {
    fields.push(Field { label: label.to_owned(), value: Zeroizing::new(text), shown });
}

/// Custom sections: each field is shown under "Section · name".
fn push_sections(fields: &mut Vec<Field>, sections: &[Value]) {
    for section in sections {
        let name = text_of(section.get("section_name")).unwrap_or_default();
        let entries = section.get("section_fields").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        push_extra_list(fields, entries, &name);
    }
}

fn push_extra(fields: &mut Vec<Field>, extra: Option<&Value>) {
    if let Some(list) = extra.and_then(Value::as_array) {
        push_extra_list(fields, list, "");
    }
}

fn push_extra_list(fields: &mut Vec<Field>, list: &[Value], prefix: &str) {
    for entry in list {
        let name = text_of(entry.get("field_name").or_else(|| entry.get("name"))).unwrap_or_default();
        let label = match (prefix.is_empty(), name.is_empty()) {
            (true, true) => "Field".to_owned(),
            (true, false) => name,
            (false, true) => prefix.to_owned(),
            (false, false) => format!("{prefix} · {name}"),
        };
        let content = entry.get("content").unwrap_or(entry);
        // The content is either one tagged variant or the value itself.
        let (shown, value) = match content {
            Value::Object(map) => {
                if let Some(hidden) = map.get("hidden") {
                    (Shown::Secret, text_of(Some(hidden).and_then(|h| h.get("content")).or(Some(hidden))))
                } else if let Some(text) = map.get("text") {
                    (Shown::Text, text_of(Some(text).and_then(|t| t.get("content")).or(Some(text))))
                } else if let Some(timestamp) = map.get("timestamp") {
                    (Shown::Text, text_of(Some(timestamp).and_then(|t| t.get("seconds")).or(Some(timestamp))))
                } else {
                    (Shown::Text, text_of(Some(content)))
                }
            }
            other => (Shown::Text, text_of(Some(other))),
        };
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            push_text(fields, &label, value, shown);
        }
    }
}

/// A key's text: a string, a number, or the string inside a one-field wrapper.
fn text_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn urls(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(list)) => list.iter().filter_map(|v| text_of(Some(v))).filter(|u| !u.is_empty()).collect(),
        Some(Value::String(text)) => text.split(',').map(|u| u.trim().to_owned()).filter(|u| !u.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// The first value stored under `key`, searched breadth-first.
fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    let mut queue = std::collections::VecDeque::from([root]);
    let mut seen = 0usize;
    while let Some(value) = queue.pop_front() {
        seen += 1;
        if seen > 10_000 {
            break;
        }
        match value {
            Value::Object(map) => {
                if let Some(found) = map.get(key) {
                    return Some(found);
                }
                queue.extend(map.values());
            }
            Value::Array(list) => queue.extend(list.iter()),
            _ => {}
        }
    }
    None
}

/// Overwrite every string in the parsed document before it is dropped.
fn wipe(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(list) => list.iter_mut().for_each(wipe),
        Value::Object(map) => map.values_mut().for_each(wipe),
        _ => {}
    }
}

/// The one-time code in `pass-cli item totp --output json`: the first string
/// that is all digits and 6 to 8 long.
pub fn totp_code(text: &str) -> Option<Zeroizing<String>> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    let found = find_code(&value).map(|code| Zeroizing::new(code.to_owned()));
    wipe(&mut value);
    found.or_else(|| {
        // A bare code on a line, if the client didn't answer in JSON.
        let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
        is_code(line).then(|| Zeroizing::new(line.to_owned()))
    })
}

fn find_code(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) if is_code(s) => Some(s.as_str()),
        Value::Array(list) => list.iter().find_map(find_code),
        Value::Object(map) => map.values().find_map(find_code),
        _ => None,
    }
}

fn is_code(text: &str) -> bool {
    (6..=8).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// Seconds left in the current `period`-second TOTP window at unix time `now`.
pub fn countdown(now: u64, period: u64) -> u64 {
    period - now % period
}

/// Whether `text` is a web address Spotty may open (http or https only).
pub fn is_web_address(text: &str) -> bool {
    crate::security::http_uri(text).is_ok()
}

/// Filter `items` by the search text (case-insensitive, all words must match).
pub fn matches(search: &str, haystack: &str) -> bool {
    let haystack = haystack.to_lowercase();
    search.split_whitespace().all(|word| haystack.contains(&word.to_lowercase()))
}

/// The login's username: its "Username", or its "Email" when that is empty.
pub fn username(detail: &Detail) -> Option<&Field> {
    ["Username", "Email"]
        .iter()
        .find_map(|label| detail.fields.iter().find(|f| f.label == *label && !f.value.is_empty()))
}

/// The login's password (a secret field labelled "Password").
pub fn password(detail: &Detail) -> Option<&Field> {
    detail.fields.iter().find(|f| f.label == "Password" && f.shown == Shown::Secret && !f.value.is_empty())
}

/// The login's first web address.
pub fn website(detail: &Detail) -> Option<&Field> {
    detail.fields.iter().find(|f| f.shown == Shown::Url && !f.value.is_empty())
}

/// The values a paste sequence pastes for this item. The one-time code is not
/// copied: the sequence reads it when its step is pasted.
pub fn login_values(detail: &Detail, target: &str) -> crate::proton_pass_sequence::LoginValues {
    let copy = |field: Option<&Field>| field.map(|f| Zeroizing::new(f.value.to_string()));
    crate::proton_pass_sequence::LoginValues {
        target: target.to_owned(),
        username: copy(username(detail)),
        password: copy(password(detail)),
        has_totp: detail.has_totp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(detail: &Detail) -> Vec<(String, String, Shown)> {
        detail.fields.iter().map(|f| (f.label.clone(), f.value.to_string(), f.shown)).collect()
    }

    #[test]
    fn a_login_shows_its_fields_with_secrets_marked() {
        let detail = parse(
            r#"{"id":"i1","item_type":"login","content":{"title":"GitHub","content":{"Login":{
                "email":"sam@example.test","username":"sam","password":"pw-123",
                "urls":["https://github.com","http://example.test"],"totp_uri":"otpauth://totp/x?secret=ABC"}}},
                "note":"recovery codes in the drawer"}"#,
        )
        .unwrap();
        assert_eq!(detail.kind, Kind::Login);
        assert!(detail.has_totp);
        let got = fields(&detail);
        assert!(got.contains(&("Password".into(), "pw-123".into(), Shown::Secret)));
        assert!(got.contains(&("Email".into(), "sam@example.test".into(), Shown::Text)));
        assert!(got.contains(&("Website".into(), "https://github.com".into(), Shown::Url)));
        assert!(got.contains(&("Notes".into(), "recovery codes in the drawer".into(), Shown::Text)));
        // The TOTP secret never becomes a field: the code comes from Proton Pass.
        assert!(!got.iter().any(|(_, v, _)| v.contains("otpauth")));
    }

    #[test]
    fn a_card_masks_its_number_and_code() {
        let detail = parse(r#"{"item_type":"credit_card","content":{"cardholder_name":"S Lee","number":"4111111111111111","verification_number":"737","expiration_date":"2030-01","pin":"0000"}}"#).unwrap();
        let got = fields(&detail);
        assert!(got.contains(&("Number".into(), "4111111111111111".into(), Shown::Secret)));
        assert!(got.contains(&("Security code".into(), "737".into(), Shown::Secret)));
        assert!(got.contains(&("Expiry".into(), "2030-01".into(), Shown::Text)));
        assert!(!detail.has_totp);
    }

    #[test]
    fn wifi_and_ssh_keys_keep_their_secrets_secret() {
        let wifi = parse(r#"{"item_type":"wifi","content":{"ssid":"Home","password":"hunter22","security":"WPA2"}}"#).unwrap();
        assert!(fields(&wifi).contains(&("Password".into(), "hunter22".into(), Shown::Secret)));
        let ssh = parse(r#"{"item_type":"ssh_key","content":{"private_key":"-----BEGIN","public_key":"ssh-ed25519 AAAA"}}"#).unwrap();
        let got = fields(&ssh);
        assert!(got.contains(&("Private key".into(), "-----BEGIN".into(), Shown::Secret)));
        assert!(got.contains(&("Public key".into(), "ssh-ed25519 AAAA".into(), Shown::Text)));
    }

    #[test]
    fn extra_fields_follow_their_kind_and_sections_get_prefixes() {
        let detail = parse(
            r#"{"item_type":"custom","title":"Server","content":{"sections":[{"section_name":"Ops","section_fields":[{"field_name":"pin","content":{"hidden":{"content":"9911"}}}]}]},
                "extra_fields":[{"field_name":"Recovery","content":{"text":{"content":"ask Lee"}}}]}"#,
        )
        .unwrap();
        let got = fields(&detail);
        assert!(got.contains(&("Ops · pin".into(), "9911".into(), Shown::Secret)));
        assert!(got.contains(&("Recovery".into(), "ask Lee".into(), Shown::Text)));
    }

    #[test]
    fn identity_numbers_are_secret_and_empty_fields_are_skipped() {
        let detail = parse(r#"{"item_type":"identity","content":{"identity":{"first_name":"Sam","passport_number":"X1234567","city":""}}}"#).unwrap();
        let got = fields(&detail);
        assert!(got.contains(&("First name".into(), "Sam".into(), Shown::Text)));
        assert!(got.contains(&("Passport number".into(), "X1234567".into(), Shown::Secret)));
        assert!(!got.iter().any(|(label, _, _)| label == "City"));
    }

    #[test]
    fn an_answer_that_is_not_json_is_refused() {
        assert!(parse("not json").is_err());
        assert!(parse("[1,2]").is_err());
    }

    #[test]
    fn totp_codes_are_six_to_eight_digits() {
        assert_eq!(totp_code(r#"{"totp":"123456"}"#).as_deref().map(String::as_str), Some("123456"));
        assert_eq!(totp_code("87654321\n").as_deref().map(String::as_str), Some("87654321"));
        assert!(totp_code(r#"{"valid_for":30}"#).is_none());
        assert!(totp_code("12345").is_none());
    }

    #[test]
    fn countdown_counts_down_within_the_period() {
        assert_eq!(countdown(0, 30), 30);
        assert_eq!(countdown(29, 30), 1);
        assert_eq!(countdown(30, 30), 30);
    }

    #[test]
    fn masks_have_a_fixed_length_and_search_needs_every_word() {
        assert_eq!(mask().chars().count(), 8);
        assert!(matches("git hub", "GitHub work"));
        assert!(!matches("git bank", "GitHub work"));
        assert!(matches("", "anything"));
    }

    #[test]
    fn only_web_addresses_open() {
        assert!(is_web_address("https://example.test/login"));
        assert!(!is_web_address("javascript:alert(1)"));
        assert!(!is_web_address("file:///etc/passwd"));
    }

    #[test]
    fn proton_pass_item_login_values_take_username_password_and_code_flag() {
        let detail = parse(
            r#"{"item_type":"login","content":{"content":{"Login":{"email":"sam@example.test","password":"pw-123",
                "urls":["https://example.test"],"totp_uri":"otpauth://totp/x?secret=ABC"}}}}"#,
        )
        .unwrap();
        // With no username, the email is the username.
        assert_eq!(username(&detail).map(|f| f.value.to_string()).as_deref(), Some("sam@example.test"));
        assert_eq!(password(&detail).map(|f| f.value.to_string()).as_deref(), Some("pw-123"));
        assert_eq!(website(&detail).map(|f| f.value.to_string()).as_deref(), Some("https://example.test"));
        let values = login_values(&detail, "s-1/i1");
        assert_eq!(values.target, "s-1/i1");
        assert_eq!(values.username.as_deref().map(String::as_str), Some("sam@example.test"));
        assert_eq!(values.password.as_deref().map(String::as_str), Some("pw-123"));
        assert!(values.has_totp);
    }

    #[test]
    fn proton_pass_item_username_prefers_the_username_field() {
        let detail = parse(r#"{"item_type":"login","content":{"content":{"Login":{"email":"mail@example.test","username":"sam","password":""}}}}"#).unwrap();
        assert_eq!(username(&detail).map(|f| f.value.to_string()).as_deref(), Some("sam"));
        // An empty password is not a value to paste.
        assert!(password(&detail).is_none());
        let values = login_values(&detail, "s-1/i1");
        assert!(values.password.is_none());
        assert!(!values.has_totp);
    }

    #[test]
    fn proton_pass_item_a_card_has_no_login_values() {
        let detail = parse(r#"{"item_type":"credit_card","content":{"number":"4111111111111111"}}"#).unwrap();
        assert!(username(&detail).is_none());
        assert!(password(&detail).is_none());
        assert!(website(&detail).is_none());
        let values = login_values(&detail, "s-1/i2");
        assert!(values.username.is_none() && values.password.is_none() && !values.has_totp);
    }
}
