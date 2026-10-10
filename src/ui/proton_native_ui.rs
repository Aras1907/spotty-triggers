//! Spotty's native Proton windows: the Proton Calendar and Proton Drive sign-in
//! windows (each app signs in on its own), a Proton Drive browser and a Proton
//! Calendar agenda. All of it is plain libadwaita on top of
//! `crate::proton_native` — no web view, no browser, no helper program.

use crate::i18n::gettext;
use crate::proton_native as native;
use adw::prelude::*;
use gtk::{gio, glib};
use spotty_proton_login as login;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use login::Zeroizing;

/// Run `work` on a worker thread and hand its result to `done` on the GTK thread.
pub(crate) fn run<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    glib::MainContext::default().spawn_local(async move {
        if let Ok(result) = gio::spawn_blocking(work).await {
            done(result);
        }
    });
}

fn application() -> Option<adw::Application> {
    gio::Application::default().and_then(|app| app.downcast::<adw::Application>().ok())
}

fn notify(title: &str, body: &str) {
    if let Some(app) = gio::Application::default() {
        let notification = gio::Notification::new(title);
        if !body.is_empty() {
            notification.set_body(Some(body));
        }
        gio::prelude::ApplicationExt::send_notification(&app, Some("proton-native"), &notification);
    }
}

fn open_with_default_app(path: &std::path::Path) {
    let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(path).uri(), gio::AppLaunchContext::NONE);
}

/// Handle a `ProtonNative` search action. `parent` is the window that triggered it.
pub fn handle(parent: Option<&gtk::Window>, op: &str, target: &str) {
    match op {
        "calendar" => open_agenda(parent, target),
        "drive" => open_drive(parent, target),
        "download" => download_and_open(target),
        "settings" => open_settings(parent, target),
        _ => {}
    }
}

fn open_settings(parent: Option<&gtk::Window>, service: &str) {
    // Calendar and Drive have nothing to set up until you are signed in.
    let native_service = match service {
        "proton-calendar" => Some(native::Service::Calendar),
        "proton-drive" => Some(native::Service::Drive),
        _ => None,
    };
    if let Some(native_service) = native_service.filter(|s| !native::signed_in(*s)) {
        open_account_window(parent, native_service);
        return;
    }
    crate::ui::settings_window::show_proton_service_settings_for(parent, service);
}

/// Download a Drive file into Downloads and open it. Used by the `drive` trigger.
pub fn download_and_open(id: &str) {
    let Some(node) = native::drive_node(id) else {
        notify(&gettext("Proton Drive"), &gettext("That file isn't loaded any more. Search for it again."));
        return;
    };
    notify(&gettext("Proton Drive"), &gettext("Downloading {name}…").replace("{name}", &node.name));
    run(move || native::drive_download(&node).map(|path| (node.name.clone(), path)), |result| match result {
        Ok((name, path)) => {
            notify(&gettext("Proton Drive"), &gettext("Saved {name} to your Downloads folder").replace("{name}", &name));
            open_with_default_app(&path);
        }
        Err(error) => notify(&gettext("Proton Drive couldn't download the file"), &error),
    });
}

// ── Proton sign-in (Calendar and Drive) ─────────────────────────────────────
//
// Every Proton app shows the same sign-in page (`spotty_proton_login`). Here
// it is wired to Calendar's or Drive's own sign-in in `crate::proton_native`.

thread_local! {
    /// The open sign-in windows, one per app. Closing one clears its slot.
    static ACCOUNT_WINDOWS: RefCell<Vec<(native::Service, glib::WeakRef<adw::ApplicationWindow>)>> = RefCell::new(Vec::new());
}

/// Where "Create a free Proton account" goes.
pub const SIGNUP_URL: &str = "https://account.proton.me/signup";

/// The app's name as shown to the user.
fn service_name(service: native::Service) -> String {
    match service {
        native::Service::Calendar => gettext("Proton Calendar"),
        native::Service::Drive => gettext("Proton Drive"),
    }
}

/// The app as the shared sign-in page shows it.
fn login_app(service: native::Service) -> login::App {
    let icon = match service {
        native::Service::Calendar => "x-office-calendar-symbolic",
        native::Service::Drive => "folder-remote-symbolic",
    };
    login::App { name: service_name(service), icon: icon.into(), signup_url: SIGNUP_URL.into(), translate: gettext }
}

/// Calendar's or Drive's own sign-in, as the shared sign-in page calls it.
struct NativeLogin(native::Service);

impl login::Backend for NativeLogin {
    fn begin(&self, email: &str, password: Zeroizing<String>) -> login::Step {
        native_step(native::sign_in(self.0, email, &password))
    }

    fn submit_code(&self, code: Zeroizing<String>) -> login::Step {
        native_step(native::submit_two_factor(self.0, &code))
    }

    fn submit_second(&self, password: Zeroizing<String>) -> login::Step {
        native_step(native::submit_mailbox_password(self.0, &password))
    }

    fn cancel(&self) {
        native::cancel_sign_in(self.0);
    }
}

/// What a Calendar or Drive sign-in step means on the shared sign-in page.
fn native_step(outcome: native::Outcome) -> login::Step {
    match outcome {
        native::Outcome::Done => login::Step::Done,
        native::Outcome::TwoFactor => login::Step::TwoFactor,
        native::Outcome::MailboxPassword => login::Step::SecondPassword { title: gettext("Mailbox password"), label: gettext("Mailbox password") },
        native::Outcome::Failed(message) => login::Step::Failed(message),
    }
}

/// The shared sign-in page for one app. `on_done` runs after a sign-in.
fn login_page(service: native::Service, on_done: impl Fn() + 'static) -> login::LoginPage {
    login::LoginPage::new(login_app(service), Arc::new(NativeLogin(service)), on_done)
}

/// Sign this app out (after the user confirmed), then refresh the search.
fn sign_out_now(service: native::Service) {
    run(move || native::sign_out(service), |()| crate::app::refresh_search_window());
}

/// The "Account" group of a signed-in app: its email and a Sign out button.
fn account_group(service: native::Service) -> Rc<login::Account> {
    Rc::new(login::Account::new(&login_app(service), &native::account_label(service), move || sign_out_now(service)))
}

/// The signed-in email, refreshed while the page is on screen.
fn refresh_email(account: &login::Account, service: native::Service) {
    if native::signed_in(service) {
        account.set_email(&native::account_label(service));
    }
}

/// Where Calendar's or Drive's sign-in stands. Its saved session loads on a
/// worker thread, so until that is done the page says it is checking.
fn session_of(service: native::Service) -> login::Session {
    if native::signed_in(service) {
        login::Session::SignedIn
    } else if native::loaded(service) {
        login::Session::SignedOut
    } else {
        login::Session::Checking
    }
}

/// Open the sign-in window of Proton Calendar or Proton Drive, or bring the one
/// that is open forward. It signs that one app in; no other app is touched.
pub fn open_account_window(parent: Option<&gtk::Window>, service: native::Service) {
    let existing = ACCOUNT_WINDOWS.with(|slots| slots.borrow().iter().find(|(s, _)| *s == service).and_then(|(_, w)| w.upgrade()));
    if let Some(window) = existing {
        window.present();
        return;
    }
    let Some(app) = application() else { return };
    let name = service_name(service);
    let title = gettext("{app} sign-in").replace("{app}", &name);
    let window = adw::ApplicationWindow::builder().application(&app).title(&title).default_width(560).default_height(640).build();
    // Not modal: GNOME attaches a modal window to its parent and squares off
    // its top corners, and in Spotty's shared window group another modal
    // window (a Proton settings popup) swallowed its clicks, close included.
    own_window(&window, parent);
    let header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new(&title, &gettext("Built into Spotty"))).build();

    let account = account_group(service);
    let account_page = adw::PreferencesPage::new();
    account_page.add(&account.group);
    let note = adw::PreferencesGroup::builder()
        .description(gettext("Signing out ends this app's session only. Your other Proton apps stay signed in."))
        .build();
    account_page.add(&note);
    let login = {
        let account = account.clone();
        login_page(service, move || {
            refresh_email(&account, service);
            crate::app::refresh_search_window();
        })
    };
    let stack = {
        let account = account.clone();
        login::sign_in_stack(&login, &account_page, move || {
            refresh_email(&account, service);
            session_of(service)
        })
    };
    // Closing with a half-finished sign-in ends it, and wipes what was typed.
    {
        let login = login.clone();
        window.connect_close_request(move |_| {
            login.reset();
            glib::Propagation::Proceed
        });
    }
    window.connect_destroy(move |_| ACCOUNT_WINDOWS.with(|slots| slots.borrow_mut().retain(|(s, _)| *s != service)));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    window.set_content(Some(&toolbar));
    ACCOUNT_WINDOWS.with(|slots| {
        let mut slots = slots.borrow_mut();
        slots.retain(|(_, w)| w.upgrade().is_some());
        slots.push((service, window.downgrade()));
    });
    window.present();
}

/// The sign-in side of a Proton Calendar or Drive settings page. Make it
/// first (`add_account_group`): it becomes the page's Account group. Then
/// `wrap` the finished page: signed out, the page shows only the sign-in;
/// signed in, the page with the Account group on top.
pub struct SettingsSignIn {
    service: native::Service,
    account: Rc<login::Account>,
    login: login::LoginPage,
}

pub fn add_account_group(page: &adw::PreferencesPage, service: native::Service) -> SettingsSignIn {
    let account = account_group(service);
    page.add(&account.group);
    let login = {
        let account = account.clone();
        login_page(service, move || {
            refresh_email(&account, service);
            crate::app::refresh_search_window();
        })
    };
    SettingsSignIn { service, account, login }
}

impl SettingsSignIn {
    /// The widget for the settings popup: the finished `page`, or the sign-in
    /// page while signed out. Closing the popup ends a half-finished sign-in
    /// (call `reset` on the returned page's login, see `close`).
    pub fn wrap(&self, page: &adw::PreferencesPage) -> gtk::Widget {
        let (account, service) = (self.account.clone(), self.service);
        login::sign_in_stack(&self.login, page, move || {
            refresh_email(&account, service);
            session_of(service)
        })
        .upcast()
    }

    /// Wipe the sign-in page and end a sign-in half done. Call it when the
    /// popup closes.
    pub fn close(&self) {
        self.login.reset();
    }
}

// ── Shared window pieces ────────────────────────────────────────────────────

fn status_page(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder().icon_name(icon).title(title).description(description).build()
}

fn spinner_page(title: &str) -> adw::StatusPage {
    let spinner = gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build();
    adw::StatusPage::builder().title(title).child(&spinner).build()
}

/// Keep a window (account, Drive, agenda) clickable. GTK blocks input to every
/// window in a modal window's group while that modal window is visible, and all
/// of Spotty's windows share one group by default: opened from the modal Proton
/// settings popup, a sign-in window or the search window, such a window
/// couldn't even be closed. Its own group takes it out of their reach.
pub(crate) fn own_window(window: &adw::ApplicationWindow, parent: Option<&gtk::Window>) {
    if let Some(parent) = parent.filter(|p| p.is_visible()) {
        window.set_transient_for(Some(parent));
    }
    gtk::WindowGroup::new().add_window(window);
}

/// While a window shows its signed-out page, move on as soon as its own app is
/// signed in (from any window), so every window for that app follows the sign-in.
fn follow_sign_in(window: &adw::ApplicationWindow, stack: &gtk::Stack, service: native::Service, signed_in: impl Fn() + 'static) {
    let window = window.downgrade();
    let stack = stack.downgrade();
    glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
        let (Some(_window), Some(stack)) = (window.upgrade(), stack.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        if stack.visible_child_name().as_deref() == Some("signedout") && native::signed_in(service) {
            signed_in();
        }
        glib::ControlFlow::Continue
    });
}

/// Only "#rrggbb" may enter Pango markup.
fn valid_color(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn color_dot(color: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    if valid_color(color) {
        label.set_markup(&format!("<span foreground=\"{color}\">●</span>"));
    } else {
        label.set_text("●");
        label.add_css_class("dim-label");
    }
    label
}

// ── Proton Drive ────────────────────────────────────────────────────────────

struct DriveWindow {
    stack: gtk::Stack,
    list: gtk::ListBox,
    error: adw::StatusPage,
    title: adw::WindowTitle,
    back: gtk::Button,
    filter: gtk::SearchEntry,
    toasts: adw::ToastOverlay,
    path: RefCell<Vec<native::Node>>,
    entries: RefCell<Vec<native::Node>>,
    generation: Cell<u64>,
}

/// Open the Drive browser at the folder with this id (empty: My files).
pub fn open_drive(parent: Option<&gtk::Window>, folder_id: &str) {
    let Some(app) = application() else { return };
    let window = adw::ApplicationWindow::builder().application(&app).title(gettext("Proton Drive")).default_width(760).default_height(680).build();
    own_window(&window, parent);
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some(&gettext("Up one folder")));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some(&gettext("Refresh")));
    let title = adw::WindowTitle::new(&gettext("Proton Drive"), "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_start(&back);
    header.pack_end(&refresh);

    let filter = gtk::SearchEntry::builder().placeholder_text(gettext("Filter this folder")).hexpand(true).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let clamp = adw::Clamp::builder().maximum_size(700).margin_top(12).margin_bottom(12).margin_start(12).margin_end(12).build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
    column.append(&filter);
    column.append(&list);
    clamp.set_child(Some(&column));
    let scroller = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    stack.add_named(&spinner_page(&gettext("Opening Proton Drive…")), Some("loading"));
    stack.add_named(&scroller, Some("list"));
    stack.add_named(&status_page("folder-symbolic", &gettext("This folder is empty"), ""), Some("empty"));
    let error = status_page("dialog-warning-symbolic", &gettext("Couldn't open Proton Drive"), "");
    let retry = gtk::Button::builder().label(gettext("Try again")).css_classes(["pill"]).halign(gtk::Align::Center).build();
    error.set_child(Some(&retry));
    stack.add_named(&error, Some("error"));
    let drive_login = login_page(native::Service::Drive, || crate::app::refresh_search_window());
    stack.add_named(&drive_login.widget(), Some("signedout"));
    {
        let login = drive_login.clone();
        window.connect_close_request(move |_| {
            login.reset();
            glib::Propagation::Proceed
        });
    }

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let drive = Rc::new(DriveWindow {
        stack,
        list,
        error,
        title,
        back: back.clone(),
        filter: filter.clone(),
        toasts,
        path: RefCell::new(Vec::new()),
        entries: RefCell::new(Vec::new()),
        generation: Cell::new(0),
    });
    {
        let d = drive.clone();
        back.connect_clicked(move |_| d.up());
        let d = drive.clone();
        refresh.connect_clicked(move |_| d.reload());
        let d = drive.clone();
        retry.connect_clicked(move |_| d.reload());
        let d = drive.clone();
        filter.connect_search_changed(move |entry| d.apply_filter(&entry.text()));
    }
    {
        let weak = Rc::downgrade(&drive);
        let folder_id = folder_id.to_owned();
        follow_sign_in(&window, &drive.stack, native::Service::Drive, move || {
            if let Some(d) = weak.upgrade() {
                d.start(folder_id.clone());
            }
        });
    }
    window.present();
    drive.start(folder_id.to_owned());
}

impl DriveWindow {
    fn current(&self) -> Option<native::Node> {
        self.path.borrow().last().cloned()
    }

    /// Work out the folder chain for `folder_id` and show it.
    fn start(self: &Rc<Self>, folder_id: String) {
        if !native::signed_in(native::Service::Drive) {
            self.stack.set_visible_child_name("signedout");
            return;
        }
        self.stack.set_visible_child_name("loading");
        let d = self.clone();
        run(
            move || -> Result<Vec<native::Node>, String> {
                let root = native::drive_root()?;
                let mut chain = vec![root.clone()];
                if !folder_id.is_empty() && folder_id != root.id {
                    let mut up = Vec::new();
                    let mut at = native::drive_node(&folder_id);
                    while let Some(node) = at {
                        let parent = node.parent.clone();
                        if node.id == root.id {
                            break;
                        }
                        up.push(node);
                        at = parent.and_then(|p| native::drive_node(&p));
                    }
                    up.reverse();
                    chain.extend(up);
                }
                Ok(chain)
            },
            move |result| match result {
                Ok(chain) => {
                    *d.path.borrow_mut() = chain;
                    d.reload();
                }
                Err(error) => d.fail(&error),
            },
        );
    }

    fn fail(&self, message: &str) {
        self.error.set_description(Some(message));
        self.stack.set_visible_child_name("error");
    }

    fn up(self: &Rc<Self>) {
        if self.path.borrow().len() > 1 {
            self.path.borrow_mut().pop();
            self.reload();
        }
    }

    fn open_folder(self: &Rc<Self>, node: native::Node) {
        self.path.borrow_mut().push(node);
        self.reload();
    }

    fn reload(self: &Rc<Self>) {
        let Some(folder) = self.current() else {
            self.start(String::new());
            return;
        };
        self.filter.set_text("");
        let depth = self.path.borrow().len();
        self.back.set_sensitive(depth > 1);
        self.title.set_title(&folder.name);
        let trail: Vec<String> = self.path.borrow().iter().map(|n| n.name.clone()).collect();
        self.title.set_subtitle(&trail.join(" / "));
        self.stack.set_visible_child_name("loading");
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let d = self.clone();
        run(move || native::drive_children(&folder), move |result| {
            if d.generation.get() != generation {
                return;
            }
            match result {
                Ok(children) => d.show(children),
                Err(error) => {
                    if !native::signed_in(native::Service::Drive) {
                        d.stack.set_visible_child_name("signedout");
                    } else {
                        d.fail(&error);
                    }
                }
            }
        });
    }

    fn show(self: &Rc<Self>, children: Vec<native::Node>) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        for node in &children {
            self.list.append(&self.row_for(node));
        }
        *self.entries.borrow_mut() = children;
        self.stack.set_visible_child_name(if self.entries.borrow().is_empty() { "empty" } else { "list" });
    }

    fn row_for(self: &Rc<Self>, node: &native::Node) -> adw::ActionRow {
        let when = glib::DateTime::from_unix_local(node.modified).ok().and_then(|d| d.format("%-d %b %Y").ok()).map(|t| t.to_string()).unwrap_or_default();
        let subtitle = match node.size.filter(|_| !node.folder) {
            Some(size) => format!("{} · {when}", human_size(size)),
            None => when,
        };
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&node.name)).subtitle(subtitle).activatable(true).build();
        row.set_use_markup(true);
        row.add_prefix(&gtk::Image::from_icon_name(icon_for(node)));
        if node.folder {
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let (d, node) = (self.clone(), node.clone());
            row.connect_activated(move |_| d.open_folder(node.clone()));
        } else {
            let save = gtk::Button::builder().icon_name("document-save-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text(gettext("Save to Downloads")).build();
            row.add_suffix(&save);
            let (d, n) = (self.clone(), node.clone());
            save.connect_clicked(move |_| d.download(&n, false));
            let (d, n) = (self.clone(), node.clone());
            row.connect_activated(move |_| d.download(&n, true));
        }
        row
    }

    fn download(self: &Rc<Self>, node: &native::Node, open: bool) {
        self.toasts.add_toast(adw::Toast::new(&gettext("Downloading {name}…").replace("{name}", &node.name)));
        let (d, node) = (self.clone(), node.clone());
        run(
            {
                let node = node.clone();
                move || native::drive_download(&node)
            },
            move |result| match result {
                Ok(path) => {
                    d.toasts.add_toast(adw::Toast::new(&gettext("Saved {name} to Downloads").replace("{name}", &node.name)));
                    if open {
                        open_with_default_app(&path);
                    }
                }
                Err(error) => d.toasts.add_toast(adw::Toast::new(&error)),
            },
        );
    }

    fn apply_filter(&self, text: &str) {
        let needle = text.trim().to_lowercase();
        let entries = self.entries.borrow();
        let mut index = 0;
        while let Some(row) = self.list.row_at_index(index) {
            let visible = needle.is_empty() || entries.get(index as usize).is_some_and(|n| n.name.to_lowercase().contains(&needle));
            row.set_visible(visible);
            index += 1;
        }
    }
}

fn icon_for(node: &native::Node) -> &'static str {
    if node.folder {
        return "folder-symbolic";
    }
    match node.mime.split('/').next().unwrap_or_default() {
        "image" => "image-x-generic-symbolic",
        "video" => "video-x-generic-symbolic",
        "audio" => "audio-x-generic-symbolic",
        _ if node.mime.contains("pdf") => "x-office-document-symbolic",
        _ if node.mime.contains("zip") || node.mime.contains("compressed") => "package-x-generic-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

fn human_size(bytes: u64) -> String {
    glib::format_size(bytes).to_string()
}

// ── Proton Calendar ─────────────────────────────────────────────────────────

struct AgendaWindow {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    page: adw::PreferencesPage,
    error: adw::StatusPage,
    title: adw::WindowTitle,
    view: RefCell<String>,
    anchor: RefCell<glib::DateTime>,
    generation: Cell<u64>,
}

/// Open the agenda on a date (`YYYY-MM-DD`, empty for today).
pub fn open_agenda(parent: Option<&gtk::Window>, date: &str) {
    let Some(app) = application() else { return };
    let Some(config) = crate::app::shared_config() else { return };
    let view = crate::search::proton::calendar_view(&config.borrow()).to_owned();
    let anchor = parse_date(date).or_else(|| glib::DateTime::now_local().ok());
    let Some(anchor) = anchor else { return };

    let window = adw::ApplicationWindow::builder().application(&app).title(gettext("Proton Calendar")).default_width(620).default_height(720).build();
    own_window(&window, parent);
    let previous = gtk::Button::from_icon_name("go-previous-symbolic");
    previous.set_tooltip_text(Some(&gettext("Previous")));
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    next.set_tooltip_text(Some(&gettext("Next")));
    let today = gtk::Button::with_label(&gettext("Today"));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some(&gettext("Refresh")));
    let views = gtk::StringList::new(&[&gettext("Day"), &gettext("Week"), &gettext("Month")]);
    let view_picker = gtk::DropDown::new(Some(views), gtk::Expression::NONE);
    view_picker.set_selected(crate::search::proton::CALENDAR_VIEWS.iter().position(|v| *v == view).unwrap_or(1) as u32);
    let title = adw::WindowTitle::new(&gettext("Proton Calendar"), "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_start(&previous);
    header.pack_start(&next);
    header.pack_start(&today);
    header.pack_end(&refresh);
    header.pack_end(&view_picker);

    let page = adw::PreferencesPage::new();
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    stack.add_named(&spinner_page(&gettext("Loading your calendar…")), Some("loading"));
    stack.add_named(&page, Some("list"));
    stack.add_named(&status_page("x-office-calendar-symbolic", &gettext("No events"), &gettext("Nothing is scheduled in this period.")), Some("empty"));
    let error = status_page("dialog-warning-symbolic", &gettext("Couldn't load your calendar"), "");
    let retry = gtk::Button::builder().label(gettext("Try again")).css_classes(["pill"]).halign(gtk::Align::Center).build();
    error.set_child(Some(&retry));
    stack.add_named(&error, Some("error"));
    let agenda_login = login_page(native::Service::Calendar, || crate::app::refresh_search_window());
    stack.add_named(&agenda_login.widget(), Some("signedout"));
    {
        let login = agenda_login.clone();
        window.connect_close_request(move |_| {
            login.reset();
            glib::Propagation::Proceed
        });
    }
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    window.set_content(Some(&toolbar));

    let agenda = Rc::new(AgendaWindow {
        window: window.clone(),
        stack,
        page,
        error,
        title,
        view: RefCell::new(view),
        anchor: RefCell::new(anchor),
        generation: Cell::new(0),
    });
    {
        let a = agenda.clone();
        previous.connect_clicked(move |_| a.shift(-1));
        let a = agenda.clone();
        next.connect_clicked(move |_| a.shift(1));
        let a = agenda.clone();
        today.connect_clicked(move |_| {
            if let Ok(now) = glib::DateTime::now_local() {
                *a.anchor.borrow_mut() = now;
                a.reload();
            }
        });
        let a = agenda.clone();
        refresh.connect_clicked(move |_| a.reload());
        let a = agenda.clone();
        retry.connect_clicked(move |_| a.reload());
        let a = agenda.clone();
        view_picker.connect_selected_notify(move |picker| {
            if let Some(name) = crate::search::proton::CALENDAR_VIEWS.get(picker.selected() as usize) {
                *a.view.borrow_mut() = (*name).to_owned();
                a.reload();
            }
        });
    }
    {
        let weak = Rc::downgrade(&agenda);
        follow_sign_in(&window, &agenda.stack, native::Service::Calendar, move || {
            if let Some(a) = weak.upgrade() {
                a.reload();
            }
        });
    }
    window.present();
    agenda.reload();
}

fn parse_date(text: &str) -> Option<glib::DateTime> {
    let mut parts = text.split('-').map(|p| p.parse::<i32>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    glib::DateTime::from_local(y, m, d, 12, 0, 0.0).ok()
}

fn midnight(date: &glib::DateTime) -> glib::DateTime {
    glib::DateTime::from_local(date.year(), date.month(), date.day_of_month(), 0, 0, 0.0).unwrap_or_else(|_| date.clone())
}

/// The period `view` shows around `anchor`: local midnight start and exclusive end.
fn period(view: &str, anchor: &glib::DateTime) -> (glib::DateTime, glib::DateTime) {
    let day = midnight(anchor);
    match view {
        "day" => {
            let end = day.add_days(1).unwrap_or_else(|_| day.clone());
            (day, end)
        }
        "month" => {
            let first = glib::DateTime::from_local(day.year(), day.month(), 1, 0, 0, 0.0).unwrap_or_else(|_| day.clone());
            let end = first.add_months(1).unwrap_or_else(|_| first.clone());
            (first, end)
        }
        _ => {
            // Weeks start on Monday (day_of_week: 1 = Monday).
            let start = day.add_days(1 - day.day_of_week()).unwrap_or_else(|_| day.clone());
            let end = start.add_days(7).unwrap_or_else(|_| start.clone());
            (start, end)
        }
    }
}

fn period_title(view: &str, start: &glib::DateTime, end: &glib::DateTime) -> String {
    let fmt = |d: &glib::DateTime, f: &str| d.format(f).map(|t| t.to_string()).unwrap_or_default();
    match view {
        "day" => fmt(start, "%A %-d %B %Y"),
        "month" => fmt(start, "%B %Y"),
        _ => {
            let last = end.add_days(-1).unwrap_or_else(|_| end.clone());
            format!("{} – {}", fmt(start, "%-d %b"), fmt(&last, "%-d %b %Y"))
        }
    }
}

/// Where an event sits in local time: all-day events are dates.
fn span(event: &native::Event) -> (i64, i64) {
    if event.all_day {
        let local = |unix: i64| {
            glib::DateTime::from_unix_utc(unix)
                .ok()
                .and_then(|d| glib::DateTime::from_local(d.year(), d.month(), d.day_of_month(), 0, 0, 0.0).ok())
                .map(|d| d.to_unix())
                .unwrap_or(unix)
        };
        (local(event.start), local(event.end))
    } else {
        (event.start, event.end.max(event.start + 1))
    }
}

impl AgendaWindow {
    fn shift(self: &Rc<Self>, direction: i32) {
        let view = self.view.borrow().clone();
        let anchor = self.anchor.borrow().clone();
        let (start, _) = period(&view, &anchor);
        let moved = match view.as_str() {
            "day" => start.add_days(direction),
            "month" => start.add_months(direction),
            _ => start.add_days(7 * direction),
        };
        if let Ok(moved) = moved {
            *self.anchor.borrow_mut() = moved;
            self.reload();
        }
    }

    fn reload(self: &Rc<Self>) {
        if !native::signed_in(native::Service::Calendar) {
            self.stack.set_visible_child_name("signedout");
            return;
        }
        let view = self.view.borrow().clone();
        let (start, end) = period(&view, &self.anchor.borrow());
        self.title.set_subtitle(&period_title(&view, &start, &end));
        self.stack.set_visible_child_name("loading");
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let (from, to) = (start.to_unix(), end.to_unix());
        let a = self.clone();
        run(move || native::calendar_events(from, to), move |result| {
            if a.generation.get() != generation {
                return;
            }
            match result {
                Ok(events) => a.show(&view, &start, &end, events),
                Err(error) => {
                    if !native::signed_in(native::Service::Calendar) {
                        a.stack.set_visible_child_name("signedout");
                    } else {
                        a.error.set_description(Some(&error));
                        a.stack.set_visible_child_name("error");
                    }
                }
            }
        });
    }

    fn show(self: &Rc<Self>, view: &str, start: &glib::DateTime, end: &glib::DateTime, events: Vec<native::Event>) {
        // Rebuild the page: one group per day that has events.
        while let Some(group) = self.page_groups().pop() {
            self.page.remove(&group);
        }
        let now = glib::DateTime::now_local().ok();
        let mut day = start.clone();
        let mut shown = 0;
        while day.to_unix() < end.to_unix() {
            let next = day.add_days(1).unwrap_or_else(|_| end.clone());
            let (from, to) = (day.to_unix(), next.to_unix());
            let todays: Vec<&native::Event> = events
                .iter()
                .filter(|e| {
                    let (s, f) = span(e);
                    f > from && s < to
                })
                .collect();
            if !todays.is_empty() {
                let mut name = day.format("%A %-d %B").map(|t| t.to_string()).unwrap_or_default();
                if let Some(now) = &now {
                    if midnight(now).to_unix() == from {
                        name = format!("{} · {name}", gettext("Today"));
                    }
                }
                let group = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&name)).build();
                for event in todays {
                    group.add(&self.event_row(event));
                    shown += 1;
                }
                self.page.add(&group);
            }
            if next.to_unix() <= day.to_unix() {
                break;
            }
            day = next;
        }
        let _ = view;
        self.stack.set_visible_child_name(if shown == 0 { "empty" } else { "list" });
    }

    fn page_groups(&self) -> Vec<adw::PreferencesGroup> {
        // Groups are the page's direct children inside its internal box; walk
        // the widget tree for them.
        fn collect(widget: &gtk::Widget, out: &mut Vec<adw::PreferencesGroup>) {
            let mut child = widget.first_child();
            while let Some(c) = child {
                if let Ok(group) = c.clone().downcast::<adw::PreferencesGroup>() {
                    out.push(group);
                } else {
                    collect(&c, out);
                }
                child = c.next_sibling();
            }
        }
        let mut out = Vec::new();
        collect(self.page.upcast_ref(), &mut out);
        out
    }

    fn event_row(self: &Rc<Self>, event: &native::Event) -> adw::ActionRow {
        let now = glib::DateTime::now_local().ok();
        let time = if event.all_day {
            gettext("All day")
        } else {
            let clock = |unix: i64| glib::DateTime::from_unix_local(unix).ok().and_then(|d| d.format("%H:%M").ok()).map(|t| t.to_string()).unwrap_or_default();
            format!("{}–{}", clock(event.start), clock(event.end))
        };
        let mut subtitle = time;
        if let Some(place) = event.location.lines().next().filter(|l| !l.is_empty()) {
            subtitle.push_str(&format!(" · {place}"));
        }
        let _ = now;
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&event.title)).subtitle(glib::markup_escape_text(&subtitle)).activatable(true).build();
        row.add_prefix(&color_dot(&event.color));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let (a, event) = (self.clone(), event.clone());
        row.connect_activated(move |_| a.details(&event));
        row
    }

    fn details(&self, event: &native::Event) {
        let now = glib::DateTime::now_local().unwrap_or_else(|_| glib::DateTime::from_unix_local(0).unwrap());
        let mut body = crate::search::proton::when_text(event, &now);
        if !event.location.is_empty() {
            body.push_str(&format!("\n{}", event.location));
        }
        if !event.calendar.is_empty() {
            body.push_str(&format!("\n{}", event.calendar));
        }
        if !event.description.is_empty() {
            body.push_str(&format!("\n\n{}", event.description));
        }
        let dialog = adw::AlertDialog::builder().heading(&event.title).body(body).build();
        dialog.add_response("close", &gettext("Close"));
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proton_native_outcomes_map_onto_the_shared_sign_in_steps() {
        assert_eq!(native_step(native::Outcome::Done), login::Step::Done);
        assert_eq!(native_step(native::Outcome::TwoFactor), login::Step::TwoFactor);
        assert_eq!(native_step(native::Outcome::Failed("no".into())), login::Step::Failed("no".into()));
        match native_step(native::Outcome::MailboxPassword) {
            login::Step::SecondPassword { title, label } => {
                assert!(title.contains("Mailbox"));
                assert!(label.contains("Mailbox"));
            }
            other => panic!("the mailbox step is a second password, got {other:?}"),
        }
    }

    fn at(y: i32, m: i32, d: i32) -> glib::DateTime {
        glib::DateTime::from_local(y, m, d, 15, 30, 0.0).unwrap()
    }

    #[test]
    fn periods_follow_the_view() {
        // Thursday 8 October 2026.
        let (s, e) = period("week", &at(2026, 10, 8));
        assert_eq!((s.year(), s.month(), s.day_of_month()), (2026, 10, 5));
        assert_eq!((e.year(), e.month(), e.day_of_month()), (2026, 10, 12));
        let (s, e) = period("month", &at(2026, 10, 8));
        assert_eq!((s.month(), s.day_of_month(), e.month(), e.day_of_month()), (10, 1, 11, 1));
        let (s, e) = period("day", &at(2026, 10, 8));
        assert_eq!((s.hour(), e.day_of_month()), (0, 9));
        assert_eq!(period_title("week", &period("week", &at(2026, 10, 8)).0, &period("week", &at(2026, 10, 8)).1), "5 Oct – 11 Oct 2026");
    }

    #[test]
    fn dates_parse_and_bad_ones_do_not() {
        assert!(parse_date("2026-10-08").is_some());
        assert!(parse_date("2026-13-40").is_none());
        assert!(parse_date("soon").is_none());
        assert!(parse_date("").is_none());
    }

    #[test]
    fn colors_are_validated_before_entering_markup() {
        assert!(valid_color("#8080ff"));
        assert!(!valid_color("red\"><b>"));
        assert!(!valid_color("#12345"));
    }
}
