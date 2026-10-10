//! A Proton Pass item in the search window's preview pane: its username, its
//! password (masked), its one-time code and its website, each with a copy
//! button, and "copy all" for a login.
//!
//! What the pane holds, and when:
//! - The item is fetched 250 ms after the selection settles (arrowing through
//!   rows fetches nothing), and only while the pane is on screen and Pass is
//!   unlocked. The fetch runs off the GTK thread.
//! - The fields are held only while the pane is on screen. A different selection,
//!   the pane being hidden (unmapped) or the PIN locking Pass drops them; they are
//!   `Zeroizing`, and a late answer for a dropped selection is discarded.
//! - The password is never shown: it is masked, with no reveal.
//! - Every copy checks the PIN and goes through `proton_pass::copy_value`, which
//!   marks the clipboard as a password and clears it after the delay in Pass's
//!   settings. The one-time code is read again for each copy and once per period
//!   while the pane is up.

use crate::i18n::gettext;
use crate::proton_pass::{self, Kind};
use crate::proton_pass_item::{self as item, Detail};
use crate::proton_pass_pin;
use crate::ui::proton_native_ui::run;
use crate::ui::proton_pass_paste;
use crate::ui::proton_pass_ui::kind_label;
use adw::prelude::*;
use gtk::glib;
use spotty_proton_account::Zeroizing;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The pause between the selection settling and the item being fetched.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often the pane checks the lock and counts down its one-time code.
const TICK: Duration = Duration::from_secs(1);
/// The one-time code's period in seconds.
const PERIOD: u64 = 30;

#[derive(Clone, Copy)]
enum Which {
    Username,
    Password,
    Website,
    Code,
}

impl Which {
    fn label(self) -> String {
        match self {
            Which::Username => gettext("Username"),
            Which::Password => gettext("Password"),
            Which::Website => gettext("Website"),
            Which::Code => gettext("One-time code"),
        }
    }
}

#[derive(Default)]
struct State {
    /// `share_id/item_id` of the item shown or being fetched. Not a secret.
    target: String,
    detail: Option<Detail>,
    loading: bool,
    /// The one-time code and the period it was read in. Empty when the read failed.
    code: Option<(u64, Zeroizing<String>)>,
    code_pending: bool,
}

struct Row {
    row: adw::ActionRow,
    copy: gtk::Button,
}

struct Inner {
    page: gtk::ScrolledWindow,
    title: gtk::Label,
    subtitle: gtk::Label,
    status: gtk::Label,
    group: adw::PreferencesGroup,
    username: Row,
    password: Row,
    code: Row,
    code_left: gtk::Label,
    website: Row,
    hint: gtk::Label,
    copy_all: gtk::Button,
    state: RefCell<State>,
    /// Bumped whenever the fields are dropped: late answers and timers then do nothing.
    generation: Cell<u64>,
}

/// The item preview. Clones share one pane.
#[derive(Clone)]
pub struct PassPreview {
    inner: Rc<Inner>,
}

impl PassPreview {
    pub fn new() -> Self {
        Self { inner: Rc::new_cyclic(|weak| Inner::build(weak.clone())) }
    }

    /// The page the preview pane shows for a Proton Pass item.
    pub fn widget(&self) -> gtk::ScrolledWindow {
        self.inner.page.clone()
    }

    /// Show the item `target` (`share_id/item_id`). Its fields are fetched while
    /// the pane is on screen.
    pub fn show(&self, target: &str) {
        self.inner.show(target);
    }

    /// The pane shows something else: forget the item and wipe its fields.
    pub fn forget(&self) {
        self.inner.drop_secrets();
        self.inner.state.borrow_mut().target.clear();
        self.inner.title.set_label("");
        self.inner.subtitle.set_label("");
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// "Vault · Kind", or just the kind when the vault is unknown.
fn place(vault: &str, kind: Kind) -> String {
    let kind = kind_label(kind);
    if vault.is_empty() {
        kind
    } else {
        format!("{vault} · {kind}")
    }
}

fn field_row(title: &str, before_copy: Option<&gtk::Label>) -> Row {
    let row = adw::ActionRow::builder().title(title).use_markup(false).build();
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_valign(gtk::Align::Center);
    copy.add_css_class("flat");
    copy.set_tooltip_text(Some(&gettext("Copy")));
    if let Some(label) = before_copy {
        row.add_suffix(label);
    }
    row.add_suffix(&copy);
    row.set_visible(false);
    Row { row, copy }
}

/// Show a row with `value` as its subtitle, or hide it when there is no value.
fn set_row(row: &Row, value: Option<&str>) {
    match value {
        Some(text) => {
            row.row.set_subtitle(text);
            row.row.set_visible(true);
        }
        None => {
            row.row.set_subtitle("");
            row.row.set_visible(false);
        }
    }
}

impl Inner {
    fn build(weak: Weak<Inner>) -> Inner {
        let title = gtk::Label::builder().css_classes(["title-4"]).xalign(0.0).wrap(true).build();
        let subtitle = gtk::Label::builder().css_classes(["caption", "dim-label"]).xalign(0.0).wrap(true).build();
        let status = gtk::Label::builder().css_classes(["dim-label"]).wrap(true).justify(gtk::Justification::Center).visible(false).build();
        let code_left = gtk::Label::builder().css_classes(["dim-label", "numeric"]).valign(gtk::Align::Center).build();
        let username = field_row(&gettext("Username"), None);
        let password = field_row(&gettext("Password"), None);
        let code = field_row(&gettext("One-time code"), Some(&code_left));
        let website = field_row(&gettext("Website"), None);
        let group = adw::PreferencesGroup::new();
        for row in [&username, &password, &code, &website] {
            group.add(&row.row);
        }
        let hint = gtk::Label::builder().css_classes(["caption", "dim-label"]).xalign(0.0).wrap(true).visible(false).build();
        let copy_all = gtk::Button::builder()
            .label(gettext("Copy username, password and code"))
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .visible(false)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&title);
        content.append(&subtitle);
        content.append(&status);
        content.append(&group);
        content.append(&hint);
        content.append(&copy_all);
        let page = gtk::ScrolledWindow::builder().child(&content).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();

        for (which, row) in [(Which::Username, &username), (Which::Password, &password), (Which::Website, &website), (Which::Code, &code)] {
            let weak = weak.clone();
            row.copy.connect_clicked(move |_| {
                if let Some(me) = weak.upgrade() {
                    me.copy(which);
                }
            });
        }
        {
            let weak = weak.clone();
            copy_all.connect_clicked(move |_| {
                if let Some(me) = weak.upgrade() {
                    me.copy_all();
                }
            });
        }
        {
            let weak = weak.clone();
            page.connect_map(move |_| {
                if let Some(me) = weak.upgrade() {
                    me.on_map();
                }
            });
        }
        page.connect_unmap(move |_| {
            if let Some(me) = weak.upgrade() {
                me.drop_secrets();
            }
        });

        Inner {
            page,
            title,
            subtitle,
            status,
            group,
            username,
            password,
            code,
            code_left,
            website,
            hint,
            copy_all,
            state: RefCell::new(State::default()),
            generation: Cell::new(0),
        }
    }

    fn show(self: &Rc<Self>, target: &str) {
        {
            let st = self.state.borrow();
            if st.target == target && (st.detail.is_some() || st.loading) {
                return;
            }
        }
        self.drop_secrets();
        let listed = proton_pass::find(target);
        let title = match &listed {
            Some(item) if !item.title.is_empty() => item.title.clone(),
            Some(_) => gettext("(untitled)"),
            None => String::new(),
        };
        self.title.set_label(&title);
        self.subtitle.set_label(&listed.as_ref().map(|item| place(&item.vault, item.kind)).unwrap_or_default());
        self.state.borrow_mut().target = target.to_owned();
        self.set_status(&gettext("Loading…"));
        self.schedule_fetch();
    }

    /// Fetch the item after the debounce, if the pane is on screen.
    fn schedule_fetch(self: &Rc<Self>) {
        if !self.page.is_mapped() {
            return;
        }
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(DEBOUNCE, move || {
            if let Some(me) = weak.upgrade() {
                me.fetch(generation);
            }
        });
    }

    fn fetch(self: &Rc<Self>, generation: u64) {
        if self.generation.get() != generation || !self.page.is_mapped() {
            return;
        }
        if !proton_pass_pin::unlocked() {
            self.show_locked();
            return;
        }
        let target = {
            let mut st = self.state.borrow_mut();
            if st.loading || st.detail.is_some() || st.target.is_empty() {
                return;
            }
            st.loading = true;
            st.target.clone()
        };
        let weak = Rc::downgrade(self);
        run(
            move || proton_pass::view_item(&target).map(|detail| (target.clone(), detail)),
            move |result| {
                if let Some(me) = weak.upgrade() {
                    me.fetched(generation, result);
                }
            },
        );
    }

    fn fetched(self: &Rc<Self>, generation: u64, result: Result<(String, Detail), String>) {
        // A newer selection, a hide or a lock dropped this fetch: discard it. Its
        // fields are wiped as the result is dropped.
        if self.generation.get() != generation {
            return;
        }
        self.state.borrow_mut().loading = false;
        match result {
            Ok((target, detail)) => {
                if self.state.borrow().target != target {
                    return;
                }
                if !proton_pass_pin::unlocked() {
                    self.show_locked();
                    return;
                }
                let has_totp = detail.has_totp;
                self.state.borrow_mut().detail = Some(detail);
                self.render();
                self.start_tick(generation);
                if has_totp {
                    self.fetch_code(generation);
                }
            }
            Err(message) => self.set_status(&message),
        }
    }

    /// Put the fetched fields on screen: rows only for what the item has.
    fn render(&self) {
        let st = self.state.borrow();
        let Some(detail) = st.detail.as_ref() else { return };
        let vault = proton_pass::find(&st.target).map(|item| item.vault).unwrap_or_default();
        let title = if detail.title.is_empty() { gettext("(untitled)") } else { detail.title.clone() };
        self.title.set_label(&title);
        self.subtitle.set_label(&place(&vault, detail.kind));
        let username = item::username(detail).map(|field| field.value.to_string());
        let has_password = item::password(detail).is_some();
        let website = item::website(detail).map(|field| field.value.to_string());
        set_row(&self.username, username.as_deref());
        set_row(&self.password, has_password.then_some(item::mask()));
        set_row(&self.website, website.as_deref());
        self.code_left.set_label("");
        self.code.row.set_subtitle("…");
        self.code.row.set_visible(detail.has_totp);
        let steps = [username.is_some(), has_password, detail.has_totp].iter().filter(|present| **present).count();
        let any_row = steps > 0 || website.is_some();
        self.group.set_visible(any_row);
        self.status.set_visible(!any_row);
        if !any_row {
            self.status.set_label(&gettext("This item has no username, password, one-time code or website to copy."));
        }
        self.hint.set_label(
            &gettext("Paste one after another. After each paste the clipboard moves on to the next value, and it clears after {seconds} s.")
                .replace("{seconds}", &proton_pass::clear_after_secs().to_string()),
        );
        self.hint.set_visible(steps >= 2);
        self.copy_all.set_visible(steps >= 2);
    }

    fn start_tick(self: &Rc<Self>, generation: u64) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(TICK, move || match weak.upgrade() {
            Some(me) => me.tick(generation),
            None => glib::ControlFlow::Break,
        });
    }

    /// Once a second while the pane is up: check the lock, count down the code.
    fn tick(self: &Rc<Self>, generation: u64) -> glib::ControlFlow {
        if self.generation.get() != generation {
            return glib::ControlFlow::Break;
        }
        if !proton_pass_pin::unlocked() {
            self.show_locked();
            return glib::ControlFlow::Break;
        }
        let has_code = self.state.borrow().detail.as_ref().is_some_and(|detail| detail.has_totp);
        if has_code {
            let now = unix_now();
            self.code_left.set_label(&format!("{} s", item::countdown(now, PERIOD)));
            let period = now / PERIOD;
            let stale = !matches!(&self.state.borrow().code, Some((read, _)) if *read == period);
            if stale {
                self.fetch_code(generation);
            }
        }
        glib::ControlFlow::Continue
    }

    /// Read the one-time code in the background. One read at a time.
    fn fetch_code(self: &Rc<Self>, generation: u64) {
        let target = {
            let mut st = self.state.borrow_mut();
            if st.code_pending || !st.detail.as_ref().is_some_and(|detail| detail.has_totp) {
                return;
            }
            st.code_pending = true;
            st.target.clone()
        };
        let period = unix_now() / PERIOD;
        let weak = Rc::downgrade(self);
        run(
            move || proton_pass::totp_now(&target),
            move |result| {
                let Some(me) = weak.upgrade() else { return };
                if me.generation.get() != generation {
                    return;
                }
                let mut st = me.state.borrow_mut();
                st.code_pending = false;
                match result {
                    Ok(code) => {
                        me.code.row.set_subtitle(&code);
                        st.code = Some((period, code));
                    }
                    Err(message) => {
                        me.code.row.set_subtitle(&message);
                        st.code = Some((period, Zeroizing::new(String::new())));
                    }
                }
            },
        );
    }

    /// Copy one value. The PIN is checked first; the password, username and
    /// website are copied from what the pane holds, the code is read for the copy.
    fn copy(self: &Rc<Self>, which: Which) {
        if !proton_pass_pin::unlocked() {
            self.show_locked();
            return;
        }
        let (target, title, value) = {
            let st = self.state.borrow();
            let Some(detail) = st.detail.as_ref() else { return };
            let value = match which {
                Which::Username => item::username(detail).map(|field| Zeroizing::new(field.value.to_string())),
                Which::Password => item::password(detail).map(|field| Zeroizing::new(field.value.to_string())),
                Which::Website => item::website(detail).map(|field| Zeroizing::new(field.value.to_string())),
                Which::Code => None,
            };
            let title = if detail.title.is_empty() { gettext("this item") } else { detail.title.clone() };
            (st.target.clone(), title, value)
        };
        let label = which.label();
        match (which, value) {
            (Which::Code, _) => run(
                move || proton_pass::totp_now(&target),
                move |result| match result {
                    Ok(code) => proton_pass::copy_value(&label, &title, &code),
                    Err(message) => proton_pass::notify(&gettext("Proton Pass"), &message),
                },
            ),
            (_, Some(value)) => proton_pass::copy_value(&label, &title, &value),
            (_, None) => {}
        }
    }

    /// Start the paste sequence for the shown login.
    fn copy_all(self: &Rc<Self>) {
        if !proton_pass_pin::unlocked() {
            self.show_locked();
            return;
        }
        let login = {
            let st = self.state.borrow();
            let Some(detail) = st.detail.as_ref() else { return };
            item::login_values(detail, &st.target)
        };
        proton_pass_paste::start(login);
    }

    /// The pane came on screen: fetch if an item is waiting for its fields.
    fn on_map(self: &Rc<Self>) {
        let waiting = {
            let st = self.state.borrow();
            !st.target.is_empty() && st.detail.is_none() && !st.loading
        };
        if waiting {
            self.schedule_fetch();
        }
    }

    fn show_locked(&self) {
        self.drop_secrets();
        self.set_status(&gettext("Proton Pass is locked. Type your PIN to see this item."));
    }

    fn set_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(true);
    }

    /// Wipe the fields and the code, and hide what showed them. The item's
    /// target stays, so the pane can fetch it again when it comes back.
    fn drop_secrets(&self) {
        self.generation.set(self.generation.get() + 1);
        {
            let mut st = self.state.borrow_mut();
            st.detail = None;
            st.code = None;
            st.loading = false;
            st.code_pending = false;
        }
        for row in [&self.username, &self.password, &self.code, &self.website] {
            row.row.set_subtitle("");
            row.row.set_visible(false);
        }
        self.code_left.set_label("");
        self.group.set_visible(false);
        self.hint.set_visible(false);
        self.copy_all.set_visible(false);
        self.status.set_visible(false);
    }
}
