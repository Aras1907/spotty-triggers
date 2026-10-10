//! Spotty's native Proton windows: the Proton account window (one sign-in for
//! every Proton app), a Proton Drive browser and a Proton Calendar agenda. All
//! of it is plain libadwaita on top of `crate::proton_native` — no web view, no
//! browser, no helper program.

use crate::i18n::gettext;
use crate::proton_native as native;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Run `work` on a worker thread and hand its result to `done` on the GTK thread.
fn run<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
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
    if matches!(service, "proton-calendar" | "proton-drive") && !native::signed_in() {
        open_account_window(parent);
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

// ── Proton account window ───────────────────────────────────────────────────

thread_local! {
    /// The open account window, if any. Closing it destroys it and clears this.
    static ACCOUNT_WINDOW: RefCell<Option<glib::WeakRef<adw::ApplicationWindow>>> = RefCell::new(None);
}

/// The Proton integrations, in the order the account window lists them.
const APPS: [(&str, &str); 4] = [
    ("proton-vpn", "network-vpn-symbolic"),
    ("proton-pass", "dialog-password-symbolic"),
    ("proton-calendar", "x-office-calendar-symbolic"),
    ("proton-drive", "folder-symbolic"),
];

fn app_name(id: &str) -> String {
    match id {
        "proton-vpn" => gettext("Proton VPN"),
        "proton-pass" => gettext("Proton Pass"),
        "proton-calendar" => gettext("Proton Calendar"),
        _ => gettext("Proton Drive"),
    }
}

/// Whether a Proton integration is installed in Spotty.
fn installed(id: &str) -> bool {
    crate::app::shared_config().is_some_and(|config| {
        let config = config.borrow();
        config.proton_service_enabled(id)
    })
}

/// Whether this Spotty build contains the integration's client.
fn built(id: &str) -> bool {
    match id {
        "proton-pass" => crate::proton_pass::available(),
        "proton-vpn" => crate::proton_vpn::available(),
        _ => true,
    }
}

/// Open Spotty's Proton account window, or bring the one that is open forward.
/// It signs you in to Proton, shows what each Proton app has and signs out of
/// all of them.
pub fn open_account_window(parent: Option<&gtk::Window>) {
    let existing = ACCOUNT_WINDOW.with(|slot| slot.borrow().as_ref().and_then(|w| w.upgrade()));
    if let Some(window) = existing {
        window.present();
        return;
    }
    let Some(app) = application() else { return };
    let window = adw::ApplicationWindow::builder()
        .application(&app)
        .title(gettext("Proton account"))
        .default_width(560)
        .default_height(720)
        .build();
    // Not modal: GNOME attaches a modal window to its parent and squares off
    // its top corners, and in Spotty's shared window group another modal
    // window (a Proton settings popup) swallowed its clicks, close included.
    own_window(&window, parent);
    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new(&gettext("Proton account"), &gettext("Built into Spotty")))
        .build();
    let banner = adw::Banner::builder().revealed(false).build();
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();

    // Sign in
    let signin_page = adw::PreferencesPage::new();
    let intro = adw::PreferencesGroup::builder()
        .description(gettext("One sign-in for Proton Calendar, Drive, Pass and VPN in Spotty. Signed in to Proton Mail Bridge, Pass or VPN already? Enter your password here once and Calendar and Drive join them. Signing in to Bridge in Spotty signs in the rest too. Your password is turned into a one-time proof and never stored."))
        .build();
    intro.set_header_suffix(Some(&gtk::Image::builder().icon_name("avatar-default-symbolic").pixel_size(32).build()));
    let credentials = adw::PreferencesGroup::builder().title(gettext("Sign in")).build();
    let username = adw::EntryRow::builder().title(gettext("Proton email or username")).input_purpose(gtk::InputPurpose::Email).build();
    let password = adw::PasswordEntryRow::builder().title(gettext("Password")).build();
    // Already signed in to Pass or VPN: start from that account's email. Only
    // the address is reused; the password is always typed here.
    let known_email = crate::proton_vpn::cached_status()
        .map(|s| s.account)
        .filter(|a| !a.is_empty())
        .or_else(|| Some(crate::proton_pass::account()).filter(|a| !a.is_empty()))
        .or_else(spotty_proton_bridge_gui::share::account_email);
    if let Some(email) = known_email {
        username.set_text(&email);
    }
    credentials.add(&username);
    credentials.add(&password);
    let sign_in = gtk::Button::builder()
        .label(gettext("Sign in"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .build();
    let spinner = gtk::Spinner::builder().visible(false).build();
    let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    button_box.set_halign(gtk::Align::Center);
    button_box.set_margin_top(12);
    button_box.append(&sign_in);
    button_box.append(&spinner);
    let sign_in_group = adw::PreferencesGroup::new();
    sign_in_group.add(&button_box);
    let two_factor = adw::PreferencesGroup::builder()
        .title(gettext("Two-factor authentication"))
        .description(gettext("Enter the 6-digit code from your authenticator app, or a recovery code."))
        .visible(false)
        .build();
    let code = adw::EntryRow::builder().title(gettext("Code")).show_apply_button(true).input_purpose(gtk::InputPurpose::Digits).build();
    two_factor.add(&code);
    let mailbox = adw::PreferencesGroup::builder()
        .title(gettext("Mailbox password"))
        .description(gettext("Your account has a second password that unlocks your data."))
        .visible(false)
        .build();
    let mailbox_password = adw::PasswordEntryRow::builder().title(gettext("Mailbox password")).show_apply_button(true).build();
    mailbox.add(&mailbox_password);
    let signup = adw::PreferencesGroup::new();
    signup.add(&gtk::LinkButton::with_label("https://account.proton.me/signup", &gettext("Create a free Proton account")));
    for group in [&intro, &credentials, &sign_in_group, &two_factor, &mailbox, &signup] {
        signin_page.add(group);
    }
    stack.add_named(&signin_page, Some("signin"));

    // Signed in
    let main_page = adw::PreferencesPage::new();
    let account_group = adw::PreferencesGroup::builder().title(gettext("Account")).build();
    let account_row = adw::ActionRow::builder().title(gettext("Signed in")).use_markup(false).build();
    account_row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
    let sign_out = gtk::Button::builder()
        .label(gettext("Sign out"))
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    account_row.add_suffix(&sign_out);
    account_group.add(&account_row);
    main_page.add(&account_group);

    let apps_group = adw::PreferencesGroup::builder()
        .title(gettext("Proton apps"))
        .description(gettext("Each app uses this sign-in. Where an app isn't signed in yet, Sign in gives it this one."))
        .build();
    let mut apps = Vec::new();
    for (id, icon) in APPS {
        let row = adw::ActionRow::builder().title(app_name(id)).use_markup(false).build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        let app_spinner = gtk::Spinner::builder().visible(false).build();
        let app_sign_in = gtk::Button::builder().label(gettext("Sign in")).valign(gtk::Align::Center).css_classes(["pill"]).visible(false).build();
        row.add_suffix(&app_spinner);
        row.add_suffix(&app_sign_in);
        apps_group.add(&row);
        apps.push(AppRow { id, row, spinner: app_spinner, sign_in: app_sign_in });
    }
    main_page.add(&apps_group);
    stack.add_named(&main_page, Some("main"));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&stack);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    let ui = Rc::new(AccountWindow {
        window: window.clone(),
        banner,
        stack,
        credentials,
        username,
        password,
        sign_in_group,
        sign_in,
        spinner,
        two_factor,
        code,
        mailbox,
        mailbox_password,
        account_row,
        sign_out,
        apps,
        busy: Cell::new(false),
        shown: Cell::new(None),
    });
    {
        let u = ui.clone();
        ui.sign_in.connect_clicked(move |_| u.start_sign_in());
        let u = ui.clone();
        ui.password.connect_entry_activated(move |_| u.start_sign_in());
        let u = ui.clone();
        ui.username.connect_entry_activated(move |_| {
            u.password.grab_focus();
        });
        let u = ui.clone();
        ui.code.connect_apply(move |_| u.submit_code());
        let u = ui.clone();
        ui.code.connect_entry_activated(move |_| u.submit_code());
        let u = ui.clone();
        ui.mailbox_password.connect_apply(move |_| u.submit_mailbox());
        let u = ui.clone();
        ui.mailbox_password.connect_entry_activated(move |_| u.submit_mailbox());
        let u = ui.clone();
        ui.sign_out.connect_clicked(move |_| u.confirm_sign_out());
        for app in &ui.apps {
            let id = app.id;
            app.sign_in.connect_clicked(move |_| crate::proton_session::share_with(id));
        }
    }
    // Closing with a half-finished sign-in ends its session at Proton.
    window.connect_close_request(|_| {
        run(native::cancel_sign_in, |()| {});
        glib::Propagation::Proceed
    });
    window.connect_destroy(|_| ACCOUNT_WINDOW.with(|slot| *slot.borrow_mut() = None));
    ui.show_page();
    ui.refresh_apps();
    ui.probe_apps();
    {
        let weak = Rc::downgrade(&ui);
        let window_weak = window.downgrade();
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            let (Some(ui), Some(window)) = (weak.upgrade(), window_weak.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if !window.is_visible() {
                return glib::ControlFlow::Break;
            }
            ui.tick();
            glib::ControlFlow::Continue
        });
    }
    ACCOUNT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.downgrade()));
    window.present();
}

/// One Proton app's row in the account window.
struct AppRow {
    id: &'static str,
    row: adw::ActionRow,
    spinner: gtk::Spinner,
    sign_in: gtk::Button,
}

/// What an app's row says about its Proton sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppLine {
    NotInstalled,
    NotInBuild,
    SigningIn,
    SignedIn,
    /// Not signed in; `share` when the Proton sign-in can be given to it now.
    SignedOut { share: bool },
    /// Not known yet (Proton Pass is still looking at its session).
    Checking,
}

impl AppLine {
    fn text(self) -> String {
        match self {
            AppLine::NotInstalled => gettext("Not installed"),
            AppLine::NotInBuild => gettext("Not included in this Spotty build"),
            AppLine::SigningIn => gettext("Signing in…"),
            AppLine::SignedIn => gettext("Signed in"),
            AppLine::SignedOut { .. } => gettext("Not signed in"),
            AppLine::Checking => gettext("Checking…"),
        }
    }
}

/// Decide an app's row from what Spotty knows. `signed_in` is None while the
/// app has not reported yet; `can_share` says the Proton sign-in can be handed
/// over right now.
fn app_line(installed: bool, built: bool, signing_in: bool, signed_in: Option<bool>, can_share: bool) -> AppLine {
    if !installed {
        return AppLine::NotInstalled;
    }
    if !built {
        return AppLine::NotInBuild;
    }
    if signing_in {
        return AppLine::SigningIn;
    }
    match signed_in {
        Some(true) => AppLine::SignedIn,
        Some(false) => AppLine::SignedOut { share: can_share },
        None => AppLine::Checking,
    }
}

struct AccountWindow {
    window: adw::ApplicationWindow,
    banner: adw::Banner,
    stack: gtk::Stack,
    credentials: adw::PreferencesGroup,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    sign_in_group: adw::PreferencesGroup,
    sign_in: gtk::Button,
    spinner: gtk::Spinner,
    two_factor: adw::PreferencesGroup,
    code: adw::EntryRow,
    mailbox: adw::PreferencesGroup,
    mailbox_password: adw::PasswordEntryRow,
    account_row: adw::ActionRow,
    sign_out: gtk::Button,
    apps: Vec<AppRow>,
    busy: Cell<bool>,
    /// Which page is showing: Some(true) signed in, Some(false) signing in.
    shown: Cell<Option<bool>>,
}

impl AccountWindow {
    /// Follow changes made elsewhere: a sign-in or sign-out, and each app's state.
    fn tick(&self) {
        if self.shown.get() != Some(native::signed_in()) {
            self.show_page();
        }
        self.refresh_apps();
    }

    /// Show the signed-in page or the sign-in form, whichever the session calls for.
    fn show_page(&self) {
        let signed_in = native::signed_in();
        self.shown.set(Some(signed_in));
        self.stack.set_visible_child_name(if signed_in { "main" } else { "signin" });
        if signed_in {
            self.account_row.set_subtitle(&native::account_label());
        }
    }

    /// Back to the empty sign-in form.
    fn reset_form(&self) {
        self.credentials.set_visible(true);
        self.sign_in_group.set_visible(true);
        self.two_factor.set_visible(false);
        self.mailbox.set_visible(false);
        self.code.set_text("");
        self.password.set_text("");
        self.mailbox_password.set_text("");
        self.banner.set_revealed(false);
    }

    fn refresh_apps(&self) {
        let native_in = native::signed_in();
        for app in &self.apps {
            let signed_in = match app.id {
                // Proton Pass hasn't looked yet: don't claim it is signed out.
                "proton-pass" if matches!(crate::proton_pass::state(), crate::proton_pass::State::Idle) => None,
                _ => Some(crate::proton_session::app_signed_in(app.id)),
            };
            let shares = matches!(app.id, "proton-pass" | "proton-vpn");
            let line = app_line(
                installed(app.id),
                built(app.id),
                crate::proton_session::signing_in(app.id),
                signed_in,
                shares && native_in,
            );
            app.row.set_subtitle(&line.text());
            let signing = line == AppLine::SigningIn;
            app.spinner.set_visible(signing);
            app.spinner.set_spinning(signing);
            app.sign_in.set_visible(matches!(line, AppLine::SignedOut { share: true }));
        }
    }

    /// Ask the apps for their state now, so the rows are right before the first tick.
    fn probe_apps(&self) {
        if installed("proton-vpn") && crate::proton_vpn::available() {
            run(
                || {
                    let _ = crate::proton_vpn::status();
                },
                |()| {},
            );
        }
        if installed("proton-pass") && crate::proton_pass::available() && matches!(crate::proton_pass::state(), crate::proton_pass::State::Idle) {
            crate::proton_pass::check_session();
        }
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.sign_in.set_sensitive(!busy);
        self.code.set_sensitive(!busy);
        self.mailbox_password.set_sensitive(!busy);
        self.sign_out.set_sensitive(!busy);
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
    }

    fn error(&self, message: &str) {
        self.banner.set_title(message);
        self.banner.set_revealed(true);
    }

    fn start_sign_in(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let user = self.username.text().to_string();
        let secret = spotty_proton_account::Zeroizing::new(self.password.text().to_string());
        // The field is emptied right away; only the proof leaves this process.
        self.password.set_text("");
        self.banner.set_revealed(false);
        self.set_busy(true);
        let ui = self.clone();
        run(move || native::sign_in(&user, &secret), move |outcome| ui.after(outcome));
    }

    fn submit_code(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let code = self.code.text().to_string();
        self.banner.set_revealed(false);
        self.set_busy(true);
        let ui = self.clone();
        run(move || native::submit_two_factor(&code), move |outcome| ui.after(outcome));
    }

    fn submit_mailbox(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let secret = spotty_proton_account::Zeroizing::new(self.mailbox_password.text().to_string());
        self.mailbox_password.set_text("");
        self.banner.set_revealed(false);
        self.set_busy(true);
        let ui = self.clone();
        run(move || native::submit_mailbox_password(&secret), move |outcome| ui.after(outcome));
    }

    fn after(self: &Rc<Self>, outcome: native::Outcome) {
        self.set_busy(false);
        match outcome {
            native::Outcome::Done => {
                self.username.set_text("");
                self.reset_form();
                self.show_page();
                self.refresh_apps();
                // Proton Pass and Proton VPN take the new sign-in too.
                crate::proton_session::share_sign_in();
                crate::app::refresh_search_window();
            }
            native::Outcome::TwoFactor => {
                self.credentials.set_visible(false);
                self.sign_in_group.set_visible(false);
                self.two_factor.set_visible(true);
                self.code.set_text("");
                self.code.grab_focus();
            }
            native::Outcome::MailboxPassword => {
                self.credentials.set_visible(false);
                self.sign_in_group.set_visible(false);
                self.two_factor.set_visible(false);
                self.mailbox.set_visible(true);
                self.mailbox_password.grab_focus();
            }
            native::Outcome::Failed(message) => self.error(&message),
        }
    }

    fn confirm_sign_out(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Sign out of Proton everywhere in Spotty?"))
            .body(gettext("Spotty signs out of your Proton account, Proton Pass and Proton VPN, and deletes the saved sign-ins and everything decrypted from them. Your Proton accounts and data are not affected."))
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("signout", &gettext("Sign out"));
        dialog.set_response_appearance("signout", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let ui = self.clone();
        dialog.connect_response(Some("signout"), move |_, _| ui.sign_out_everywhere());
        dialog.present(Some(&self.window));
    }

    fn sign_out_everywhere(self: &Rc<Self>) {
        self.set_busy(true);
        self.banner.set_revealed(false);
        let ui = self.clone();
        run(
            || {
                // Proton VPN and Proton Pass first: they can only sign out while
                // their sessions still work.
                crate::proton_pass::sign_out();
                if crate::proton_vpn::available() && crate::proton_vpn::status().is_ok_and(|s| s.logged_in) {
                    if let Err(error) = crate::proton_vpn::logout() {
                        log::warn!("proton vpn: sign-out failed: {error}");
                    }
                }
                native::sign_out();
            },
            move |()| {
                // Older Spotty versions kept a private web profile for a
                // Proton web window that no longer exists; erase what is left.
                for dir in [dirs::data_dir(), dirs::cache_dir()].into_iter().flatten() {
                    let _ = std::fs::remove_dir_all(dir.join("spotty").join("proton-web"));
                }
                ui.set_busy(false);
                ui.reset_form();
                ui.show_page();
                ui.refresh_apps();
                crate::app::refresh_search_window();
            },
        );
    }
}

/// The Proton account group of a Proton app's settings page: a signed-in row
/// with a way to manage the account, or a row that opens the account window.
pub fn add_account_row(page: &adw::PreferencesPage) {
    let group = adw::PreferencesGroup::builder().title(gettext("Proton account")).build();
    let signed_in_row = adw::ActionRow::builder().title(gettext("Signed in")).use_markup(false).build();
    signed_in_row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
    let manage = gtk::Button::builder().label(gettext("Manage…")).valign(gtk::Align::Center).build();
    signed_in_row.add_suffix(&manage);
    let signed_out_row = adw::ActionRow::builder()
        .title(gettext("Sign in to Proton"))
        .subtitle(gettext("One sign-in for all Proton apps in Spotty"))
        .use_markup(false)
        .activatable(true)
        .build();
    signed_out_row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
    signed_out_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    group.add(&signed_in_row);
    group.add(&signed_out_row);
    page.add(&group);

    let manage_page = page.downgrade();
    manage.connect_clicked(move |_| open_account_window(window_of(&manage_page).as_ref()));
    let sign_in_page = page.downgrade();
    signed_out_row.connect_activated(move |_| open_account_window(window_of(&sign_in_page).as_ref()));

    // Follow a sign-in or sign-out made anywhere else while this page is open.
    let update = {
        let (signed_in_row, signed_out_row) = (signed_in_row.clone(), signed_out_row.clone());
        move || {
            let signed_in = native::signed_in();
            signed_in_row.set_visible(signed_in);
            signed_out_row.set_visible(!signed_in);
            if signed_in {
                let label = native::account_label();
                if signed_in_row.subtitle().as_deref() != Some(label.as_str()) {
                    signed_in_row.set_subtitle(&label);
                }
            }
        }
    };
    update();
    let weak_page = page.downgrade();
    glib::timeout_add_seconds_local(1, move || {
        if weak_page.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        update();
        glib::ControlFlow::Continue
    });
}

/// The window a widget sits in, if it is in one.
fn window_of(page: &glib::WeakRef<adw::PreferencesPage>) -> Option<gtk::Window> {
    let page = page.upgrade()?;
    page.root().and_then(|root| root.downcast::<gtk::Window>().ok())
}

// ── Shared window pieces ────────────────────────────────────────────────────

fn status_page(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder().icon_name(icon).title(title).description(description).build()
}

fn spinner_page(title: &str) -> adw::StatusPage {
    let spinner = gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build();
    adw::StatusPage::builder().title(title).child(&spinner).build()
}

fn signed_out_page(service: &str, parent: gtk::Window) -> adw::StatusPage {
    let page = status_page(
        "avatar-default-symbolic",
        &gettext("Sign in to Proton"),
        &gettext("Sign in once with your Proton account. It takes your Proton email and password, and a two-factor code if you use one."),
    );
    let button = gtk::Button::builder().label(gettext("Sign in…")).css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();
    let service = service.to_owned();
    button.connect_clicked(move |_| open_settings(Some(&parent), &service));
    page.set_child(Some(&button));
    page
}

/// Keep a window (account, Drive, agenda) clickable. GTK blocks input to every
/// window in a modal window's group while that modal window is visible, and all
/// of Spotty's windows share one group by default: opened from the modal Proton
/// settings popup, the account window or the search window, such a window
/// couldn't even be closed. Its own group takes it out of their reach.
fn own_window(window: &adw::ApplicationWindow, parent: Option<&gtk::Window>) {
    if let Some(parent) = parent.filter(|p| p.is_visible()) {
        window.set_transient_for(Some(parent));
    }
    gtk::WindowGroup::new().add_window(window);
}

/// While a window shows its signed-out page, move on as soon as Spotty is
/// signed in to Proton (from any window), so every Proton window follows the
/// one sign-in.
fn follow_sign_in(window: &adw::ApplicationWindow, stack: &gtk::Stack, signed_in: impl Fn() + 'static) {
    let window = window.downgrade();
    let stack = stack.downgrade();
    glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
        let (Some(_window), Some(stack)) = (window.upgrade(), stack.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        if stack.visible_child_name().as_deref() == Some("signedout") && native::signed_in() {
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
    stack.add_named(&signed_out_page("proton-drive", window.clone().upcast()), Some("signedout"));

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
        follow_sign_in(&window, &drive.stack, move || {
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
        if !native::signed_in() {
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
                    if !native::signed_in() {
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
    stack.add_named(&signed_out_page("proton-calendar", window.clone().upcast()), Some("signedout"));
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
        follow_sign_in(&window, &agenda.stack, move || {
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
        if !native::signed_in() {
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
                    if !native::signed_in() {
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

    #[test]
    fn proton_app_rows_follow_install_build_and_sign_in() {
        // Not installed wins over everything else.
        assert_eq!(app_line(false, true, false, Some(true), true), AppLine::NotInstalled);
        assert_eq!(app_line(false, false, true, None, true), AppLine::NotInstalled);
        // Installed but not in this build.
        assert_eq!(app_line(true, false, false, Some(true), true), AppLine::NotInBuild);
        // A sign-in in progress shows before the signed-in state.
        assert_eq!(app_line(true, true, true, Some(false), true), AppLine::SigningIn);
        assert_eq!(app_line(true, true, false, Some(true), true), AppLine::SignedIn);
        assert_eq!(app_line(true, true, false, Some(false), true), AppLine::SignedOut { share: true });
        // Signed out of the Proton account: nothing to hand over.
        assert_eq!(app_line(true, true, false, Some(false), false), AppLine::SignedOut { share: false });
        // Not reported yet.
        assert_eq!(app_line(true, true, false, None, true), AppLine::Checking);
    }
}
