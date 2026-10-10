//! Proton Pass's window in Spotty, and the parts of its settings that are UI.
//!
//! The vault window is a view and copy window, like Proton Pass's browser
//! extension: pick a vault, search, open an item and see its fields. Nothing
//! here creates, edits, trashes or generates anything.
//!
//! Secrets follow these rules:
//! - The list holds titles, kinds, usernames and ids only (from `proton_pass`).
//! - An item's fields are fetched when it is opened and held only while its
//!   detail is on screen. Switching item, locking, or closing the window drops
//!   them, and they are wiped (`Zeroizing`).
//! - A revealed secret is masked again after 30 seconds, or on switching item.
//! - The vault window and every copy need the PIN when one is set
//!   (`proton_pass_pin::unlocked`). A lock that runs out while the window is
//!   open wipes what is on screen.
//! - Copies go through `proton_pass::copy_secret`, which marks them as
//!   passwords and clears them after the delay chosen in the settings.

use crate::i18n::gettext;
use crate::proton_pass::{self, Item, Kind, State};
use crate::proton_pass_item::{self as item, Detail, Shown};
use crate::proton_pass_settings::{self as settings, CLIPBOARD_CHOICES};
use crate::proton_pass_login;
use crate::proton_pass_pin;
use crate::proton_session::SigningIn;
use crate::ui::proton_native_ui::{own_window, run, SIGNUP_URL};
use adw::prelude::*;
use spotty_proton_account::Zeroizing;
use spotty_proton_login as login;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a revealed secret stays visible.
const REVEAL_FOR: Duration = Duration::from_secs(30);
/// The one-time code's period in seconds.
const TOTP_PERIOD: u64 = 30;

fn application() -> Option<adw::Application> {
    gtk::gio::Application::default().and_then(|app| app.downcast::<adw::Application>().ok())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn toast(overlay: &adw::ToastOverlay, text: &str) {
    overlay.add_toast(adw::Toast::new(text));
}

pub(crate) fn kind_label(kind: Kind) -> String {
    match kind {
        Kind::Login => gettext("Login"),
        Kind::Note => gettext("Note"),
        Kind::Alias => gettext("Alias"),
        Kind::CreditCard => gettext("Card"),
        Kind::Identity => gettext("Identity"),
        Kind::SshKey => gettext("SSH key"),
        Kind::Wifi => gettext("Wi-Fi"),
        Kind::Custom => gettext("Item"),
    }
}

fn kind_icon(kind: Kind) -> &'static str {
    kind.icon()
}

// ── The vault window ────────────────────────────────────────────────────────

thread_local! {
    /// The open vault window, if any. Closing it clears this.
    static VAULT_WINDOW: RefCell<Option<glib::WeakRef<adw::ApplicationWindow>>> = RefCell::new(None);
}

/// Open the vault window, or bring the open one forward. `parent` is the
/// window that asked (the settings popup, or none for the launcher).
pub fn open_vault(parent: Option<&gtk::Window>) {
    let existing = VAULT_WINDOW.with(|slot| slot.borrow().as_ref().and_then(|w| w.upgrade()));
    if let Some(window) = existing {
        window.present();
        return;
    }
    let Some(app) = application() else { return };
    let window = adw::ApplicationWindow::builder().application(&app).title(gettext("Proton Pass")).default_width(900).default_height(640).build();
    own_window(&window, parent);

    let vault_pick = gtk::DropDown::new(Some(gtk::StringList::new(&[])), gtk::Expression::NONE);
    vault_pick.set_tooltip_text(Some(&gettext("Vault")));
    let search = gtk::SearchEntry::builder().placeholder_text(gettext("Search titles")).hexpand(true).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).css_classes(["navigation-sidebar"]).build();
    let list_scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    let sidebar_bar = adw::HeaderBar::builder().show_end_title_buttons(false).build();
    sidebar_bar.set_title_widget(Some(&vault_pick));
    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.append(&search);
    sidebar_box.append(&list_scroll);
    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_bar);
    sidebar_toolbar.set_content(Some(&sidebar_box));
    let sidebar = adw::NavigationPage::builder().title(gettext("Items")).child(&sidebar_toolbar).build();

    let lock_now = gtk::Button::from_icon_name("system-lock-screen-symbolic");
    lock_now.set_tooltip_text(Some(&gettext("Lock now")));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some(&gettext("Reload your vault")));
    let title = adw::WindowTitle::new(&gettext("Proton Pass"), "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_end(&lock_now);
    header.pack_end(&refresh);

    let detail_page = adw::PreferencesPage::new();
    let detail_empty = proton_pass_status(
        "dialog-password-symbolic",
        &gettext("Choose an item"),
        &gettext("Its fields appear here. Passwords stay hidden until you reveal them."),
    );
    let detail_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    detail_stack.add_named(&detail_empty, Some("empty"));
    detail_stack.add_named(&detail_page, Some("item"));
    let detail_toasts = adw::ToastOverlay::new();
    detail_toasts.set_child(Some(&detail_stack));
    let detail_toolbar = adw::ToolbarView::new();
    detail_toolbar.add_top_bar(&adw::HeaderBar::new());
    detail_toolbar.set_content(Some(&detail_toasts));
    let content = adw::NavigationPage::builder().title(gettext("Item")).child(&detail_toolbar).build();

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&content));
    split.set_min_sidebar_width(260.0);
    split.set_max_sidebar_width(340.0);

    // Whole-window states: locked, signed out, loading, failed, or the vault.
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    let locked = proton_pass_status("system-lock-screen-symbolic", &gettext("Proton Pass is locked"), &gettext("Type your PIN to open your vault."));
    let unlock = gtk::Button::builder().label(gettext("Unlock")).css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();
    locked.set_child(Some(&unlock));
    let sign_in_view = vault_sign_in_page();
    let failed = proton_pass_status("dialog-warning-symbolic", &gettext("Proton Pass isn't available right now"), "");
    let retry = gtk::Button::builder().label(gettext("Try again")).css_classes(["pill"]).halign(gtk::Align::Center).build();
    failed.set_child(Some(&retry));
    let loading = adw::StatusPage::builder()
        .title(gettext("Loading your vault…"))
        .child(&gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build())
        .build();
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&locked, Some("locked"));
    stack.add_named(&sign_in_view.widget(), Some("signedout"));
    stack.add_named(&failed, Some("failed"));
    stack.add_named(&split, Some("vault"));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let vault = Rc::new(Vault {
        window: window.clone(),
        stack,
        title,
        failed,
        vault_pick,
        search: search.clone(),
        list,
        detail_page,
        detail_stack,
        detail_toasts,
        split,
        shown_generation: Cell::new(u64::MAX),
        rows: RefCell::new(Vec::new()),
        selected: RefCell::new(None),
        detail: RefCell::new(None),
        detail_groups: RefCell::new(Vec::new()),
        fields: RefCell::new(Vec::new()),
        selection: Cell::new(0),
        totp: RefCell::new(None),
        locked_shown: Cell::new(false),
    });

    {
        let v = vault.clone();
        vault.vault_pick.connect_selected_notify(move |_| v.render_list());
        let v = vault.clone();
        search.connect_search_changed(move |_| v.render_list());
        let v = vault.clone();
        vault.list.connect_row_activated(move |_, row| {
            let Ok(index) = usize::try_from(row.index()) else { return };
            let target = v.rows.borrow().get(index).map(Item::target);
            if let Some(target) = target {
                v.select(target);
            }
        });
        let v = vault.clone();
        lock_now.connect_clicked(move |_| {
            proton_pass_pin::lock_now();
            v.lock_view();
        });
        refresh.connect_clicked(move |_| {
            if proton_pass_pin::unlocked() {
                proton_pass::refresh();
            }
        });
        let v = vault.clone();
        unlock.connect_clicked(move |_| v.unlock());
        retry.connect_clicked(move |_| proton_pass::refresh());
    }
    {
        let weak = Rc::downgrade(&vault);
        let sign_in_close = sign_in_view.clone();
        window.connect_close_request(move |_| {
            if let Some(v) = weak.upgrade() {
                v.wipe();
            }
            // A sign-in half done ends with the window, and what was typed is wiped.
            sign_in_close.reset();
            glib::Propagation::Proceed
        });
    }
    window.connect_destroy(|_| VAULT_WINDOW.with(|slot| *slot.borrow_mut() = None));

    // Follow the lock, the listing and the one-time code while the window is open.
    {
        let weak = Rc::downgrade(&vault);
        let window_weak = window.downgrade();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let (Some(v), Some(window)) = (weak.upgrade(), window_weak.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if !window.is_visible() {
                return glib::ControlFlow::Break;
            }
            v.tick();
            glib::ControlFlow::Continue
        });
    }
    VAULT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.downgrade()));
    window.present();
    vault.tick();
    proton_pass::ensure_loaded();
}

fn proton_pass_status(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder().icon_name(icon).title(title).description(description).build()
}

/// What the detail pane shows for one field: its row and, for a secret, when it was revealed.
struct FieldView {
    row: adw::ActionRow,
    value: Zeroizing<String>,
    revealed_at: Cell<Option<Instant>>,
    reveal: Option<gtk::Button>,
}

/// The one-time code on screen, with its countdown.
struct TotpView {
    target: String,
    row: adw::ActionRow,
    countdown: gtk::Label,
    code: Option<Zeroizing<String>>,
    period: u64,
    pending: bool,
}

struct Vault {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    title: adw::WindowTitle,
    failed: adw::StatusPage,
    vault_pick: gtk::DropDown,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    detail_page: adw::PreferencesPage,
    detail_stack: gtk::Stack,
    detail_toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    /// The cache generation the list was last drawn from.
    shown_generation: Cell<u64>,
    /// The items the list shows, in row order. Titles and ids only.
    rows: RefCell<Vec<Item>>,
    selected: RefCell<Option<String>>,
    /// The open item's fields. Dropped (and wiped) on every switch, lock and close.
    detail: RefCell<Option<Detail>>,
    detail_groups: RefCell<Vec<adw::PreferencesGroup>>,
    fields: RefCell<Vec<FieldView>>,
    /// Counts selections, so a late answer for an old item is ignored.
    selection: Cell<u64>,
    totp: RefCell<Option<TotpView>>,
    locked_shown: Cell<bool>,
}

impl Vault {
    /// The vault names in the listing, in order, with "All vaults" first.
    fn vault_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        proton_pass::with_items(|_, items| {
            for item in items {
                if !item.vault.is_empty() && !names.contains(&item.vault) {
                    names.push(item.vault.clone());
                }
            }
        });
        names
    }

    fn tick(self: &Rc<Self>) {
        if !proton_pass_pin::unlocked() {
            if !self.locked_shown.get() {
                self.lock_view();
            }
            return;
        }
        if self.locked_shown.get() {
            self.locked_shown.set(false);
            self.stack.set_visible_child_name("loading");
            proton_pass::ensure_loaded();
        }
        let generation = proton_pass::generation();
        if generation != self.shown_generation.get() {
            self.shown_generation.set(generation);
            self.refresh_state();
        }
        self.expire_reveals();
        self.update_totp();
    }

    /// Show the state the cache is in: the vault, or why there is none.
    fn refresh_state(self: &Rc<Self>) {
        let state = proton_pass::state();
        match state {
            State::Ready => {
                self.stack.set_visible_child_name("vault");
                self.title.set_subtitle(&proton_pass::account());
                self.refill_vaults();
                self.render_list();
            }
            State::SignedOut => {
                self.clear_items();
                self.stack.set_visible_child_name("signedout");
            }
            State::Failed(message) => {
                self.clear_items();
                self.failed.set_description(Some(&message));
                self.stack.set_visible_child_name("failed");
            }
            State::Idle | State::Loading => {
                if self.stack.visible_child_name().as_deref() != Some("vault") {
                    self.stack.set_visible_child_name("loading");
                }
            }
        }
    }

    /// Rebuild the vault picker, keeping the choice when it still exists.
    fn refill_vaults(&self) {
        let names = self.vault_names();
        let mut labels = vec![gettext("All vaults")];
        labels.extend(names.iter().cloned());
        let wanted = settings::get().default_vault;
        let current = self.current_vault();
        let model = gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>());
        self.vault_pick.set_model(Some(&model));
        let keep = current.filter(|c| names.contains(c)).or_else(|| (!wanted.is_empty() && names.contains(&wanted)).then_some(wanted));
        let position = keep.and_then(|name| names.iter().position(|n| *n == name)).map(|i| i as u32 + 1).unwrap_or(0);
        self.vault_pick.set_selected(position);
    }

    /// The vault chosen in the picker, or None for all vaults.
    fn current_vault(&self) -> Option<String> {
        let selected = self.vault_pick.selected();
        if selected == 0 {
            return None;
        }
        let names = self.vault_names();
        names.get(selected as usize - 1).cloned()
    }

    /// Redraw the item list for the chosen vault and the search text.
    fn render_list(self: &Rc<Self>) {
        let vault = self.current_vault();
        let search = self.search.text().to_string();
        let show_username = settings::get().show_username;
        let mut chosen: Vec<Item> = Vec::new();
        proton_pass::with_items(|state, items| {
            if *state != State::Ready {
                return;
            }
            for item in items {
                if vault.as_deref().is_some_and(|v| v != item.vault) {
                    continue;
                }
                let haystack = format!("{} {}", item.title, item.username);
                if item::matches(&search, &haystack) {
                    chosen.push(item.clone());
                }
            }
        });
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        for item in &chosen {
            let subtitle = if show_username && !item.username.is_empty() { item.username.clone() } else { kind_label(item.kind) };
            let title = if item.title.is_empty() { gettext("(untitled)") } else { item.title.clone() };
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&title)).subtitle(glib::markup_escape_text(&subtitle)).activatable(true).build();
            row.set_use_markup(true);
            row.add_prefix(&gtk::Image::from_icon_name(kind_icon(item.kind)));
            self.list.append(&row);
        }
        *self.rows.borrow_mut() = chosen;
        let selected = self.selected.borrow().clone();
        if let Some(target) = selected {
            if let Some(index) = self.rows.borrow().iter().position(|i| i.target() == target) {
                if let Some(row) = self.list.row_at_index(index as i32) {
                    self.list.select_row(Some(&row));
                }
            }
        }
    }

    fn clear_items(&self) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        self.rows.borrow_mut().clear();
    }

    /// Open an item: fetch its fields and show them. Old fields are wiped first.
    fn select(self: &Rc<Self>, target: String) {
        if !proton_pass_pin::unlocked() {
            self.lock_view();
            return;
        }
        self.drop_detail();
        *self.selected.borrow_mut() = Some(target.clone());
        self.detail_stack.set_visible_child_name("empty");
        self.split.set_show_content(true);
        let generation = self.selection.get() + 1;
        self.selection.set(generation);
        let v = self.clone();
        run(move || proton_pass::view_item(&target).map(|detail| (target, detail)), move |result| {
            if v.selection.get() != generation {
                return;
            }
            match result {
                Ok((target, detail)) => v.show(target, detail),
                Err(message) => toast(&v.detail_toasts, &message),
            }
        });
    }

    /// Put an item's fields on screen.
    fn show(self: &Rc<Self>, target: String, detail: Detail) {
        if !proton_pass_pin::unlocked() {
            self.lock_view();
            return;
        }
        self.title.set_title(&if detail.title.is_empty() { gettext("(untitled)") } else { detail.title.clone() });
        self.title.set_subtitle(&kind_label(detail.kind));
        let page = &self.detail_page;
        let mut groups = Vec::new();
        let mut fields = Vec::new();
        let mut current: Option<adw::PreferencesGroup> = None;
        for field in &detail.fields {
            let group = current.get_or_insert_with(|| {
                let group = adw::PreferencesGroup::new();
                page.add(&group);
                groups.push(group.clone());
                group
            });
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&field.label)).use_markup(true).subtitle_selectable(true).build();
            let value = field.value.clone();
            let mut reveal = None;
            match field.shown {
                Shown::Secret => {
                    row.set_subtitle(&glib::markup_escape_text(item::mask()));
                    let button = gtk::Button::from_icon_name("view-reveal-symbolic");
                    button.set_valign(gtk::Align::Center);
                    button.add_css_class("flat");
                    button.set_tooltip_text(Some(&gettext("Reveal")));
                    row.add_suffix(&button);
                    reveal = Some(button);
                }
                Shown::Url => {
                    row.set_subtitle(&glib::markup_escape_text(&field.value));
                    let open = gtk::Button::from_icon_name("web-browser-symbolic");
                    open.set_valign(gtk::Align::Center);
                    open.add_css_class("flat");
                    open.set_tooltip_text(Some(&gettext("Open in browser")));
                    let url = field.value.to_string();
                    open.connect_clicked(move |_| {
                        if item::is_web_address(&url) {
                            let _ = gtk::gio::AppInfo::launch_default_for_uri(&url, gtk::gio::AppLaunchContext::NONE);
                        }
                    });
                    row.add_suffix(&open);
                }
                Shown::Text => row.set_subtitle(&glib::markup_escape_text(&field.value)),
            }
            let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
            copy.set_valign(gtk::Align::Center);
            copy.add_css_class("flat");
            copy.set_tooltip_text(Some(&gettext("Copy")));
            row.add_suffix(&copy);
            group.add(&row);
            fields.push(FieldView { row: row.clone(), value: value.clone(), revealed_at: Cell::new(None), reveal: reveal.clone() });
            let index = fields.len() - 1;
            {
                let v = self.clone();
                copy.connect_clicked(move |_| v.copy_field(index));
            }
            if reveal.is_some() {
                let v = self.clone();
                reveal.unwrap().connect_clicked(move |_| v.toggle_reveal(index));
            }
        }
        if detail.has_totp {
            let group = adw::PreferencesGroup::builder().title(gettext("One-time code")).build();
            let row = adw::ActionRow::builder().title(gettext("Code")).subtitle("…").build();
            let countdown = gtk::Label::builder().css_classes(["dim-label", "numeric"]).valign(gtk::Align::Center).build();
            let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
            copy.set_valign(gtk::Align::Center);
            copy.add_css_class("flat");
            copy.set_tooltip_text(Some(&gettext("Copy")));
            row.add_suffix(&countdown);
            row.add_suffix(&copy);
            group.add(&row);
            page.add(&group);
            groups.push(group);
            {
                let v = self.clone();
                copy.connect_clicked(move |_| v.copy_totp());
            }
            *self.totp.borrow_mut() = Some(TotpView { target: target.clone(), row, countdown, code: None, period: 0, pending: false });
        }
        if fields.is_empty() && !detail.has_totp {
            let group = adw::PreferencesGroup::new();
            group.add(&adw::ActionRow::builder().title(gettext("This item has no fields Spotty can show")).build());
            page.add(&group);
            groups.push(group);
        }
        *self.detail_groups.borrow_mut() = groups;
        *self.fields.borrow_mut() = fields;
        *self.detail.borrow_mut() = Some(detail);
        self.detail_stack.set_visible_child_name("item");
        self.update_totp();
    }

    /// Forget the open item: wipe its fields and its one-time code.
    fn drop_detail(&self) {
        *self.detail.borrow_mut() = None;
        self.fields.borrow_mut().clear();
        *self.totp.borrow_mut() = None;
        for group in self.detail_groups.borrow_mut().drain(..) {
            self.detail_page.remove(&group);
        }
        self.title.set_title(&gettext("Proton Pass"));
        self.title.set_subtitle("");
    }

    fn toggle_reveal(self: &Rc<Self>, index: usize) {
        if !proton_pass_pin::unlocked() {
            self.lock_view();
            return;
        }
        let fields = self.fields.borrow();
        let Some(field) = fields.get(index) else { return };
        if field.revealed_at.get().is_some() {
            self.mask(field);
        } else {
            field.row.set_subtitle(&glib::markup_escape_text(&field.value));
            field.revealed_at.set(Some(Instant::now()));
            if let Some(button) = &field.reveal {
                button.set_icon_name("view-conceal-symbolic");
                button.set_tooltip_text(Some(&gettext("Hide")));
            }
        }
    }

    fn mask(&self, field: &FieldView) {
        field.row.set_subtitle(&glib::markup_escape_text(item::mask()));
        field.revealed_at.set(None);
        if let Some(button) = &field.reveal {
            button.set_icon_name("view-reveal-symbolic");
            button.set_tooltip_text(Some(&gettext("Reveal")));
        }
    }

    fn expire_reveals(&self) {
        for field in self.fields.borrow().iter() {
            if field.revealed_at.get().is_some_and(|at| at.elapsed() >= REVEAL_FOR) {
                self.mask(field);
            }
        }
    }

    /// Copy one field from the open item. The PIN is checked first.
    fn copy_field(self: &Rc<Self>, index: usize) {
        if !proton_pass_pin::unlocked() {
            self.lock_view();
            return;
        }
        let value = {
            let fields = self.fields.borrow();
            let Some(field) = fields.get(index) else { return };
            Zeroizing::new(field.value.to_string())
        };
        if value.is_empty() {
            return;
        }
        proton_pass::copy_secret(&value);
        let seconds = proton_pass::clear_after_secs();
        toast(&self.detail_toasts, &gettext("Copied · clears from the clipboard in {seconds} s").replace("{seconds}", &seconds.to_string()));
    }

    fn copy_totp(self: &Rc<Self>) {
        if !proton_pass_pin::unlocked() {
            self.lock_view();
            return;
        }
        let code = self.totp.borrow().as_ref().and_then(|t| t.code.as_ref().map(|c| Zeroizing::new(c.to_string())));
        match code {
            Some(code) => {
                proton_pass::copy_secret(&code);
                let seconds = proton_pass::clear_after_secs();
                toast(&self.detail_toasts, &gettext("Copied · clears from the clipboard in {seconds} s").replace("{seconds}", &seconds.to_string()));
            }
            None => toast(&self.detail_toasts, &gettext("The one-time code isn't ready yet.")),
        }
    }

    /// Keep the one-time code current and its countdown right, while the item is open.
    fn update_totp(self: &Rc<Self>) {
        let now = unix_now();
        let period = now / TOTP_PERIOD;
        let left = item::countdown(now, TOTP_PERIOD);
        let fetch = {
            let mut totp = self.totp.borrow_mut();
            let Some(view) = totp.as_mut() else { return };
            view.countdown.set_text(&format!("{left} s"));
            if view.period != period && !view.pending {
                view.pending = true;
                Some(view.target.clone())
            } else {
                None
            }
        };
        let Some(target) = fetch else { return };
        let generation = self.selection.get();
        let v = self.clone();
        run(move || proton_pass::totp_now(&target).map(|code| (target, code)), move |result| {
            let mut totp = v.totp.borrow_mut();
            let Some(view) = totp.as_mut() else { return };
            if v.selection.get() != generation {
                return;
            }
            view.pending = false;
            match result {
                Ok((target, code)) if target == view.target => {
                    view.row.set_subtitle(&glib::markup_escape_text(&code));
                    view.code = Some(code);
                    view.period = period;
                }
                Ok(_) => {}
                Err(message) => {
                    // Try again in the next period, not on every tick.
                    view.row.set_subtitle("…");
                    view.code = None;
                    view.period = period;
                    drop(totp);
                    toast(&v.detail_toasts, &message);
                }
            }
        });
    }

    /// The PIN is needed again: blank everything and show the lock page.
    fn lock_view(&self) {
        self.selection.set(self.selection.get() + 1);
        self.selected.borrow_mut().take();
        self.drop_detail();
        self.clear_items();
        self.split.set_show_content(false);
        self.shown_generation.set(u64::MAX);
        self.locked_shown.set(true);
        self.stack.set_visible_child_name("locked");
    }

    fn unlock(self: &Rc<Self>) {
        let v = self.clone();
        proton_pass_pin::unlock_then(Some(self.window.upcast_ref()), move || {
            v.locked_shown.set(false);
            v.stack.set_visible_child_name("loading");
            proton_pass::ensure_loaded();
            v.tick();
        });
    }

    /// Wipe the open item and its code when the window closes.
    fn wipe(&self) {
        self.selection.set(self.selection.get() + 1);
        self.selected.borrow_mut().take();
        self.drop_detail();
        self.clear_items();
    }
}

// ── Settings ────────────────────────────────────────────────────────────────

// ── Sign-in (the shared sign-in page) ───────────────────────────────────────
//
// Proton Pass signs in through `pass-cli` (see `proton_pass_login`). The
// shared page (`spotty_proton_login`) asks for the email and password, then
// for whatever Pass asks next; this file only maps each answer onto it.

/// The Pass sign-in as the shared page calls it. Holds the "signing in"
/// marker while a step is waiting for input.
#[derive(Default)]
struct PassLogin {
    signing: Mutex<Option<SigningIn>>,
}

impl login::Backend for PassLogin {
    fn begin(&self, email: &str, password: Zeroizing<String>) -> login::Step {
        *self.signing.lock().unwrap_or_else(|p| p.into_inner()) = Some(SigningIn::start("proton-pass"));
        self.answer(proton_pass_login::begin(email, password))
    }

    fn submit_code(&self, code: Zeroizing<String>) -> login::Step {
        self.answer(proton_pass_login::submit_code(code))
    }

    fn submit_second(&self, password: Zeroizing<String>) -> login::Step {
        self.answer(proton_pass_login::submit_mailbox(password))
    }

    fn cancel(&self) {
        proton_pass_login::cancel();
        *self.signing.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

impl PassLogin {
    fn answer(&self, step: proton_pass_login::Step) -> login::Step {
        let step = login_step(step);
        if !matches!(step, login::Step::TwoFactor | login::Step::SecondPassword { .. }) {
            *self.signing.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        step
    }
}

/// What a `pass-cli` sign-in step means on the shared sign-in page.
fn login_step(step: proton_pass_login::Step) -> login::Step {
    use proton_pass_login::Step;
    match step {
        Step::Done => login::Step::Done,
        Step::TwoFactor => login::Step::TwoFactor,
        Step::MailboxPassword => login::Step::SecondPassword { title: gettext("Mailbox password"), label: gettext("Mailbox password") },
        Step::ExtraPassword => login::Step::SecondPassword { title: gettext("Proton Pass extra password"), label: gettext("Extra password") },
        Step::Failed(message) => login::Step::Failed(message),
    }
}

/// The shared sign-in page for Proton Pass. `on_done` runs after a sign-in.
fn sign_in_page(on_done: impl Fn() + 'static) -> login::LoginPage {
    let app = login::App {
        name: gettext("Proton Pass"),
        icon: "dialog-password-symbolic".into(),
        signup_url: SIGNUP_URL.into(),
        translate: gettext,
    };
    login::LoginPage::new(app, Arc::new(PassLogin::default()), on_done)
}

/// Where Pass's sign-in stands. Until its session has been checked the page
/// says so, rather than showing the sign-in page.
fn pass_session() -> login::Session {
    match proton_pass::state() {
        State::Ready => login::Session::SignedIn,
        State::Loading if !proton_pass::account().is_empty() => login::Session::SignedIn,
        State::Idle | State::Loading => login::Session::Checking,
        State::SignedOut | State::Failed(_) => login::Session::SignedOut,
    }
}

/// The sign-in side of the Proton Pass settings popup. Make it first: its
/// Account group leads the signed-in page. Then `wrap` the finished page.
pub struct SettingsSignIn {
    account: Rc<login::Account>,
    login: login::LoginPage,
}

pub fn settings_sign_in(page: &adw::PreferencesPage) -> SettingsSignIn {
    let account = Rc::new(login::Account::new(&pass_app(), &proton_pass::account(), proton_pass::sign_out));
    page.add(&account.group);
    let login = {
        let account = account.clone();
        sign_in_page(move || {
            account.set_email(&proton_pass::account());
            proton_pass::signed_in_now();
            crate::app::refresh_search_window();
        })
    };
    SettingsSignIn { account, login }
}

fn pass_app() -> login::App {
    login::App { name: gettext("Proton Pass"), icon: "dialog-password-symbolic".into(), signup_url: SIGNUP_URL.into(), translate: gettext }
}

impl SettingsSignIn {
    /// The popup content: the sign-in page while signed out, `page` once signed in.
    pub fn wrap(&self, page: &adw::PreferencesPage) -> gtk::Widget {
        let account = self.account.clone();
        login::sign_in_stack(&self.login, page, move || {
            let session = pass_session();
            if session == login::Session::SignedIn {
                account.set_email(&proton_pass::account());
            }
            session
        })
        .upcast()
    }

    /// Wipe the sign-in page and end a sign-in half done (the popup closed).
    pub fn close(&self) {
        self.login.reset();
    }
}

/// The shared sign-in page of the vault window (signed out).
fn vault_sign_in_page() -> login::LoginPage {
    sign_in_page(|| {
        proton_pass::signed_in_now();
        crate::app::refresh_search_window();
    })
}

#[cfg(test)]
mod signin_tests {
    use super::*;

    #[test]
    fn pass_steps_map_onto_the_shared_page() {
        use proton_pass_login::Step;
        assert_eq!(login_step(Step::Done), login::Step::Done);
        assert_eq!(login_step(Step::TwoFactor), login::Step::TwoFactor);
        assert_eq!(login_step(Step::Failed("nope".into())), login::Step::Failed("nope".into()));
        match login_step(Step::MailboxPassword) {
            login::Step::SecondPassword { title, .. } => assert!(title.contains("Mailbox")),
            other => panic!("the mailbox step is a second password, got {other:?}"),
        }
        match login_step(Step::ExtraPassword) {
            login::Step::SecondPassword { title, label } => {
                assert!(title.contains("extra password"));
                assert_eq!(label, "Extra password");
            }
            other => panic!("the extra password step is a second password, got {other:?}"),
        }
    }
}

// ── Settings ────────────────────────────────────────────────────────────────

/// The extension-style settings of Proton Pass: clipboard delay, usernames,
/// default vault and title search. Each change is saved at once.
pub fn add_settings_group(page: &adw::PreferencesPage, popup: &adw::Window) {
    let group = adw::PreferencesGroup::builder().title(gettext("Vault")).build();

    let clipboard = adw::ComboRow::builder()
        .title(gettext("Clear copied passwords after"))
        .subtitle(gettext("Copied secrets leave the clipboard after this long, if they are still there"))
        .build();
    let labels: Vec<String> = CLIPBOARD_CHOICES
        .iter()
        .map(|secs| match secs {
            s if *s < 60 => gettext("{n} s").replace("{n}", &s.to_string()),
            s => gettext("{n} min").replace("{n}", &(s / 60).to_string()),
        })
        .collect();
    clipboard.set_model(Some(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())));
    let current = settings::get().clipboard_secs;
    clipboard.set_selected(CLIPBOARD_CHOICES.iter().position(|s| *s == current).unwrap_or(1) as u32);
    clipboard.connect_selected_notify(move |row| {
        if let Some(secs) = CLIPBOARD_CHOICES.get(row.selected() as usize) {
            let _ = settings::update(|s| s.clipboard_secs = *secs);
        }
    });
    group.add(&clipboard);

    let usernames = adw::SwitchRow::builder()
        .title(gettext("Show usernames"))
        .subtitle(gettext("Under each item in the vault window and the trigger's results"))
        .active(settings::get().show_username)
        .build();
    usernames.connect_active_notify(|row| {
        let on = row.is_active();
        let _ = settings::update(|s| s.show_username = on);
    });
    group.add(&usernames);

    let vault_names = {
        let mut names = Vec::new();
        proton_pass::with_items(|_, items| {
            for item in items {
                if !item.vault.is_empty() && !names.contains(&item.vault) {
                    names.push(item.vault.clone());
                }
            }
        });
        names
    };
    let default_vault = settings::get().default_vault;
    let mut labels = vec![gettext("All vaults")];
    labels.extend(vault_names.iter().cloned());
    if !default_vault.is_empty() && !vault_names.contains(&default_vault) {
        labels.push(default_vault.clone());
    }
    let vault_row = adw::ComboRow::builder()
        .title(gettext("Default vault"))
        .subtitle(gettext("The vault the vault window opens on"))
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .build();
    let position = if default_vault.is_empty() { 0 } else { labels.iter().position(|l| *l == default_vault).unwrap_or(0) };
    vault_row.set_selected(position as u32);
    let vault_choices = labels.clone();
    vault_row.connect_selected_notify(move |row| {
        let index = row.selected() as usize;
        let name = match index {
            0 => String::new(),
            i => vault_choices.get(i).cloned().unwrap_or_default(),
        };
        let _ = settings::update(|s| s.default_vault = name);
    });
    group.add(&vault_row);

    let titles = adw::SwitchRow::builder()
        .title(gettext("Search item titles with pass"))
        .subtitle(gettext("Off: the pass trigger shows only its sign-in, lock and vault rows. Titles are never stored."))
        .active(settings::get().search_titles)
        .build();
    titles.connect_active_notify(|row| {
        let on = row.is_active();
        let _ = settings::update(|s| s.search_titles = on);
    });
    group.add(&titles);

    let open = adw::ActionRow::builder()
        .title(gettext("Open vault…"))
        .subtitle(gettext("Browse, view and copy items in a window"))
        .activatable(true)
        .build();
    open.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let parent = popup.downgrade();
    open.connect_activated(move |_| open_vault(parent.upgrade().as_ref().map(|p| p.upcast_ref::<gtk::Window>())));
    group.add(&open);
    page.add(&group);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_choices_are_the_offered_delays() {
        assert_eq!(CLIPBOARD_CHOICES, [15, 30, 60, 120, 300]);
        assert!(CLIPBOARD_CHOICES.contains(&settings::Settings::default().clipboard_secs));
    }
}
