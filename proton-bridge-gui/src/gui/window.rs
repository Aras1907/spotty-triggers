use super::*;
use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

type Model = Rc<RefCell<LoginWindow>>;

#[derive(Clone)]
struct Secrets {
    password: adw::PasswordEntryRow,
    code: adw::PasswordEntryRow,
    mailbox: adw::PasswordEntryRow,
    pin: adw::PasswordEntryRow,
    generated: Rc<RefCell<Option<gtk::Entry>>>,
}

impl Secrets {
    fn clear_login(&self) {
        for row in [&self.password, &self.code, &self.mailbox, &self.pin] {
            row.set_text("");
        }
    }

    fn clear_generated(&self) {
        if let Some(entry) = self.generated.borrow_mut().take() {
            entry.set_text("");
        }
    }

    fn clear_all(&self) {
        self.clear_login();
        self.clear_generated();
    }
}

#[derive(Default)]
struct Rendered {
    step: Option<Step>,
    ready: bool,
    secrets_revision: u64,
    mail_revision: Option<u64>,
    accounts: Vec<(String, String, i32)>,
    account_rows: Vec<adw::ActionRow>,
}

struct Window {
    model: Model,
    window: adw::ApplicationWindow,
    overlay: adw::ToastOverlay,
    status: gtk::Label,
    spinner: gtk::Spinner,
    stack: gtk::Stack,
    username: adw::EntryRow,
    secrets: Secrets,
    security_group: adw::PreferencesGroup,
    security_button: gtk::Button,
    factor_key: gtk::Button,
    mail: gtk::Box,
    accounts: adw::PreferencesGroup,
    startup: adw::SwitchRow,
    cancel: gtk::Button,
    recovery: gtk::Box,
    official: gtk::Button,
    menu_official: gtk::Button,
    close: gtk::Button,
    rendered: RefCell<Rendered>,
}

fn label(text: &str, style: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes([style])
        .build()
}

fn primary(text: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(text)
        .halign(gtk::Align::End)
        .css_classes(["suggested-action", "pill"])
        .build()
}

fn secret(title: &str) -> adw::PasswordEntryRow {
    adw::PasswordEntryRow::builder()
        .title(title)
        .input_hints(gtk::InputHints::PRIVATE | gtk::InputHints::NO_SPELLCHECK)
        .build()
}

fn form(title: &str, description: &str) -> (gtk::Box, adw::PreferencesGroup) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 16);
    let group = adw::PreferencesGroup::builder()
        .title(title)
        .description(description)
        .build();
    page.append(&group);
    (page, group)
}

fn submit_action(
    model: &Model,
    username: &adw::EntryRow,
    secrets: &Secrets,
    method: LoginMethod,
) -> impl Fn() + 'static {
    let model = model.clone();
    let username = username.clone();
    let secrets = secrets.clone();
    move || {
        let mut state = model.borrow_mut();
        if !state.ready || state.busy || state.closing {
            return;
        }
        if method == LoginMethod::Password {
            state.username = username.text().to_string();
        }
        match method {
            LoginMethod::Password => {
                state.password = Zeroizing::new(secrets.password.text().to_string())
            }
            LoginMethod::TwoFactor => state.code = Zeroizing::new(secrets.code.text().to_string()),
            LoginMethod::MailboxPassword => {
                state.mailbox = Zeroizing::new(secrets.mailbox.text().to_string())
            }
            LoginMethod::SecurityKey => state.pin = Zeroizing::new(secrets.pin.text().to_string()),
        }
        state.submit(method);
        secrets.clear_all();
    }
}

impl Window {
    fn new(app: &adw::Application, model: Model) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Proton Mail Bridge")
            .default_width(560)
            .default_height(760)
            .width_request(360)
            .build();
        let overlay = adw::ToastOverlay::new();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("Proton Mail Bridge", "Spotty"))
            .build();
        toolbar.add_top_bar(&header);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 24);
        for margin in ["margin-start", "margin-end", "margin-top", "margin-bottom"] {
            body.set_property(margin, 24i32);
        }
        let hero = gtk::Box::new(gtk::Orientation::Vertical, 8);
        hero.append(
            &gtk::Image::builder()
                .icon_name("mail-send-receive-symbolic")
                .pixel_size(48)
                .margin_bottom(8)
                .css_classes(["accent"])
                .build(),
        );
        hero.append(&label("Your Proton mail, connected", "title-1"));
        hero.append(&label(
            "Sign in to use Proton Mail with your favourite mail app.",
            "dim-label",
        ));
        body.append(&hero);
        let status_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        status_box.set_halign(gtk::Align::Center);
        let spinner = gtk::Spinner::new();
        let status = gtk::Label::builder()
            .wrap(true)
            .hexpand(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        status_box.append(&spinner);
        status_box.append(&status);
        body.append(&status_box);

        let username = adw::EntryRow::builder()
            .title("Proton email or username")
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        let secrets = Secrets {
            password: secret("Account password"),
            code: secret("Two-factor authentication code"),
            mailbox: secret("Mailbox password"),
            pin: secret("Security key PIN"),
            generated: Rc::new(RefCell::new(None)),
        };
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(180)
            .vhomogeneous(false)
            .hhomogeneous(false)
            .build();
        let (login, login_group) = form(
            "Sign in to Proton",
            "Use your Proton account details to connect Bridge.",
        );
        login_group.add(&username);
        login_group.add(&secrets.password);
        let sign_in = primary("Sign in");
        let action = submit_action(&model, &username, &secrets, LoginMethod::Password);
        sign_in.connect_clicked(move |_| action());
        let action = submit_action(&model, &username, &secrets, LoginMethod::Password);
        secrets.password.connect_entry_activated(move |_| action());
        let password = secrets.password.downgrade();
        username.connect_entry_activated(move |_| {
            if let Some(row) = password.upgrade() {
                row.grab_focus();
            }
        });
        login.append(&sign_in);
        login.append(&label(
            "Requires a paid Proton Mail plan and an unlocked desktop keyring.",
            "caption",
        ));
        stack.add_named(&login, Some("password"));

        let (factor, factor_group) = form(
            "Verify your account",
            "Enter the code from your authenticator app.",
        );
        factor_group.add(&secrets.code);
        let verify = primary("Verify code");
        let action = submit_action(&model, &username, &secrets, LoginMethod::TwoFactor);
        verify.connect_clicked(move |_| action());
        let action = submit_action(&model, &username, &secrets, LoginMethod::TwoFactor);
        secrets.code.connect_entry_activated(move |_| action());
        factor.append(&verify);
        let factor_key = gtk::Button::with_label("Use a security key instead");
        factor_key.add_css_class("flat");
        let action = submit_action(&model, &username, &secrets, LoginMethod::SecurityKey);
        factor_key.connect_clicked(move |_| action());
        factor.append(&factor_key);
        stack.add_named(&factor, Some("factor"));

        let (mailbox, mailbox_group) = form(
            "Unlock your mailbox",
            "Enter your separate mailbox password to unlock your mail.",
        );
        mailbox_group.add(&secrets.mailbox);
        let unlock = primary("Unlock mailbox");
        let action = submit_action(&model, &username, &secrets, LoginMethod::MailboxPassword);
        unlock.connect_clicked(move |_| action());
        let action = submit_action(&model, &username, &secrets, LoginMethod::MailboxPassword);
        secrets.mailbox.connect_entry_activated(move |_| action());
        mailbox.append(&unlock);
        stack.add_named(&mailbox, Some("mailbox"));

        let (security, security_group) = form(
            "Use your security key",
            "Connect your security key to continue.",
        );
        security_group.add(&secrets.pin);
        let security_button = primary("Authenticate with security key");
        let action = submit_action(&model, &username, &secrets, LoginMethod::SecurityKey);
        security_button.connect_clicked(move |_| action());
        let action = submit_action(&model, &username, &secrets, LoginMethod::SecurityKey);
        secrets.pin.connect_entry_activated(move |_| action());
        security.append(&security_button);
        stack.add_named(&security, Some("security"));

        let mail = gtk::Box::new(gtk::Orientation::Vertical, 16);
        let (finished, _) = form(
            "You're connected",
            "Copy these settings into your mail app. Bridge stays connected when you close this window.",
        );
        finished.append(&mail);
        let another = gtk::Button::builder()
            .label("Add another account")
            .halign(gtk::Align::End)
            .css_classes(["pill"])
            .build();
        {
            let model = model.clone();
            let secrets = secrets.clone();
            let username = username.clone();
            another.connect_clicked(move |_| {
                let mut state = model.borrow_mut();
                state.clear_secrets();
                state.username.clear();
                state.mail_settings = None;
                state.step = Step::Password;
                username.set_text("");
                secrets.clear_all();
            });
        }
        finished.append(&another);
        stack.add_named(&finished, Some("finished"));
        let verification = adw::StatusPage::builder()
            .icon_name("dialog-information-symbolic")
            .title("Continue in Proton Bridge")
            .description(
                "Use the official Bridge window to finish verification or unlock your keyring.",
            )
            .build();
        stack.add_named(&verification, Some("official"));
        body.append(&stack);

        let cancel = gtk::Button::builder()
            .label("Cancel sign-in")
            .halign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let model = model.clone();
            let secrets = secrets.clone();
            cancel.connect_clicked(move |_| {
                let mut state = model.borrow_mut();
                state.clear_secrets();
                state.mail_settings = None;
                state.busy = true;
                let username = state.username.clone();
                state.send(Command::Cancel(username));
                secrets.clear_all();
            });
        }
        body.append(&cancel);
        let accounts = adw::PreferencesGroup::builder()
            .title("Saved accounts")
            .build();
        body.append(&accounts);
        let preferences = adw::PreferencesGroup::new();
        let startup = adw::SwitchRow::builder()
            .title("Start Bridge at desktop login")
            .subtitle("Keep your mail connected when you sign in to your desktop.")
            .use_markup(false)
            .build();
        preferences.add(&startup);
        {
            let model = model.clone();
            startup.connect_active_notify(move |row| {
                let Ok(mut state) = model.try_borrow_mut() else {
                    return;
                };
                if state.ready
                    && !state.busy
                    && !state.closing
                    && state.autostart != row.is_active()
                {
                    state.busy = true;
                    state.send(Command::Autostart(row.is_active()));
                }
            });
        }
        body.append(&preferences);
        let recovery = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        recovery.set_halign(gtk::Align::Center);
        let start = gtk::Button::with_label("Start Bridge");
        {
            let model = model.clone();
            start.connect_clicked(move |_| model.borrow_mut().start_bridge());
        }
        let reconnect = gtk::Button::with_label("Reconnect");
        {
            let model = model.clone();
            reconnect.connect_clicked(move |_| model.borrow_mut().connect());
        }
        recovery.append(&start);
        recovery.append(&reconnect);
        body.append(&recovery);
        let official = gtk::Button::builder()
            .label("Open official Bridge window")
            .css_classes(["flat"])
            .build();
        {
            let model = model.clone();
            let secrets = secrets.clone();
            official.connect_clicked(move |_| {
                model.borrow_mut().open_official();
                secrets.clear_all();
            });
        }
        body.append(&official);
        let menu = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Bridge options")
            .build();
        let popover = gtk::Popover::new();
        let options = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for margin in ["margin-start", "margin-end", "margin-top", "margin-bottom"] {
            options.set_property(margin, 6i32);
        }
        let menu_official = gtk::Button::builder()
            .label("Open official Bridge window")
            .css_classes(["flat"])
            .build();
        {
            let model = model.clone();
            let secrets = secrets.clone();
            let popover = popover.downgrade();
            menu_official.connect_clicked(move |_| {
                model.borrow_mut().open_official();
                secrets.clear_all();
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
            });
        }
        options.append(&menu_official);
        let help = gtk::LinkButton::with_label(
            "https://proton.me/support/protonmail-bridge-install",
            "Bridge setup guide",
        );
        options.append(&help);
        popover.set_child(Some(&options));
        menu.set_popover(Some(&popover));
        header.pack_end(&menu);
        let close = gtk::Button::builder()
            .label("Close and keep Bridge running")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        let weak_window = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
        });
        body.append(&close);
        let clamp = adw::Clamp::builder()
            .maximum_size(540)
            .tightening_threshold(420)
            .child(&body)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&clamp)
            .build();
        toolbar.set_content(Some(&scroll));
        overlay.set_child(Some(&toolbar));
        window.set_content(Some(&overlay));
        {
            let model = model.clone();
            let secrets = secrets.clone();
            window.connect_close_request(move |_| {
                let mut state = model.borrow_mut();
                state.begin_close();
                secrets.clear_all();
                if state.close_finished && !state.open_official && state.official_started.is_none()
                {
                    glib::Propagation::Proceed
                } else {
                    glib::Propagation::Stop
                }
            });
        }
        let ui = Rc::new(Self {
            model,
            window,
            overlay,
            status,
            spinner,
            stack,
            username,
            secrets,
            security_group,
            security_button,
            factor_key,
            mail,
            accounts,
            startup,
            cancel,
            recovery,
            official,
            menu_official,
            close,
            rendered: RefCell::new(Rendered::default()),
        });
        ui.render();
        ui
    }

    fn copy_button(&self, title: &str, password: bool, value: Option<String>) -> gtk::Button {
        let button = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(title)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(title)]);
        let model = self.model.clone();
        let overlay = self.overlay.downgrade();
        button.connect_clicked(move |button| {
            if password {
                let state = model.borrow();
                let Some(settings) = &state.mail_settings else {
                    return;
                };
                button.clipboard().set_text(settings.password.as_str());
            } else if let Some(value) = &value {
                button.clipboard().set_text(value);
            }
            if let Some(overlay) = overlay.upgrade() {
                overlay.add_toast(adw::Toast::new(if password {
                    "Bridge password copied"
                } else {
                    "Mail username copied"
                }));
            }
        });
        button
    }

    fn render_mail(&self, settings: &session::MailSettings) {
        let credentials = adw::PreferencesGroup::builder()
            .title("Mail credentials")
            .description("Use the generated Bridge password in your mail app.")
            .build();
        for address in &settings.addresses {
            let row = adw::ActionRow::builder()
                .title("Mail username")
                .subtitle(address)
                .use_markup(false)
                .build();
            row.add_suffix(&self.copy_button("Copy mail username", false, Some(address.clone())));
            credentials.add(&row);
        }
        let password_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for margin in ["margin-start", "margin-end", "margin-top", "margin-bottom"] {
            password_box.set_property(margin, 12i32);
        }
        let caption = gtk::Label::builder()
            .label("Bridge password")
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build();
        password_box.append(&caption);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let entry = gtk::Entry::builder()
            .text(settings.password.as_str())
            .editable(false)
            .visibility(true)
            .hexpand(true)
            .css_classes(["flat", "monospace"])
            .input_hints(gtk::InputHints::PRIVATE)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label("Bridge password")]);
        let reveal = gtk::ToggleButton::builder()
            .icon_name("view-conceal-symbolic")
            .tooltip_text("Hide Bridge password")
            .active(true)
            .css_classes(["flat"])
            .build();
        let weak_entry = entry.downgrade();
        reveal.connect_toggled(move |button| {
            if let Some(entry) = weak_entry.upgrade() {
                entry.set_visibility(button.is_active());
            }
            button.set_icon_name(if button.is_active() {
                "view-conceal-symbolic"
            } else {
                "view-reveal-symbolic"
            });
            button.set_tooltip_text(Some(if button.is_active() {
                "Hide Bridge password"
            } else {
                "Show Bridge password"
            }));
        });
        controls.append(&entry);
        controls.append(&reveal);
        controls.append(&self.copy_button("Copy Bridge password", true, None));
        password_box.append(&controls);
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_child(Some(&password_box));
        credentials.add(&row);
        *self.secrets.generated.borrow_mut() = Some(entry);
        self.mail.append(&credentials);
        let servers = adw::PreferencesGroup::builder()
            .title("Mail server settings")
            .build();
        for (title, port, ssl) in [
            (
                "Incoming mail · IMAP",
                settings.imap_port,
                settings.imap_ssl,
            ),
            (
                "Outgoing mail · SMTP",
                settings.smtp_port,
                settings.smtp_ssl,
            ),
        ] {
            servers.add(
                &adw::ActionRow::builder()
                    .title(title)
                    .subtitle(format!(
                        "{}:{port} · {}",
                        settings.hostname,
                        if ssl { "SSL/TLS" } else { "STARTTLS" }
                    ))
                    .use_markup(false)
                    .build(),
            );
        }
        self.mail.append(&servers);
    }

    fn render(&self) {
        let state = self.model.borrow();
        let mut rendered = self.rendered.borrow_mut();
        self.status.set_label(&state.message);
        let waiting = state.busy || state.closing || state.start_pending.is_some();
        self.spinner.set_visible(waiting);
        self.spinner.set_spinning(waiting);
        let enabled = state.ready && !state.busy && !state.closing;
        self.stack.set_sensitive(enabled);
        self.startup.set_sensitive(enabled);
        self.startup.set_active(state.autostart);
        self.accounts.set_visible(!state.accounts.is_empty());
        self.accounts.set_sensitive(enabled);
        self.factor_key
            .set_visible(state.step == Step::FactorChoice);
        self.secrets.pin.set_visible(state.step == Step::KeyPin);
        self.security_button
            .set_visible(state.step != Step::TouchKey);
        self.security_button
            .set_label(if state.step == Step::KeyPin {
                "Verify security key"
            } else {
                "Authenticate with security key"
            });
        self.security_group
            .set_description(Some(if state.step == Step::TouchKey {
                "Touch your security key to finish verification."
            } else if state.step == Step::KeyPin {
                "Enter the PIN for your security key."
            } else {
                "Connect your security key to continue."
            }));
        self.cancel.set_visible(
            state.ready
                && !state.closing
                && state.step != Step::Finished
                && state.step != Step::OfficialGui
                && (state.busy || state.step != Step::Password),
        );
        let can_recover =
            !state.closing && state.start_pending.is_none() && state.preparing.is_none();
        self.recovery
            .set_visible(!state.ready && state.commands.is_none() && can_recover);
        self.recovery.set_sensitive(can_recover);
        self.official.set_sensitive(can_recover);
        self.official.set_visible(
            state.step == Step::OfficialGui
                || (!state.ready && state.commands.is_none() && can_recover),
        );
        self.menu_official.set_sensitive(can_recover);
        self.close.set_visible(state.step == Step::Finished);
        if state.secrets_revision != rendered.secrets_revision {
            self.secrets.clear_login();
            rendered.secrets_revision = state.secrets_revision;
        }
        if state.mail_settings.is_none() || rendered.mail_revision != Some(state.mail_revision) {
            self.secrets.clear_generated();
            while let Some(child) = self.mail.first_child() {
                self.mail.remove(&child);
            }
            rendered.mail_revision = None;
            if let Some(settings) = &state.mail_settings {
                self.render_mail(settings);
                rendered.mail_revision = Some(state.mail_revision);
            }
        }
        if state.accounts != rendered.accounts {
            for row in rendered.account_rows.drain(..) {
                self.accounts.remove(&row);
            }
            for (id, name, status) in &state.accounts {
                let row = adw::ActionRow::builder()
                    .title(name)
                    .use_markup(false)
                    .subtitle(match status {
                        2 => "Connected",
                        1 => "Locked",
                        _ => "Signed out",
                    })
                    .build();
                if *status == 2 {
                    let button = gtk::Button::builder()
                        .label("Mail settings")
                        .valign(gtk::Align::Center)
                        .build();
                    let id = id.clone();
                    let model = self.model.clone();
                    let secrets = self.secrets.clone();
                    button.connect_clicked(move |_| {
                        let mut state = model.borrow_mut();
                        state.mail_settings = None;
                        state.clear_secrets();
                        state.busy = true;
                        state.send(Command::ShowAccount(id.clone()));
                        secrets.clear_all();
                    });
                    row.add_suffix(&button);
                }
                self.accounts.add(&row);
                rendered.account_rows.push(row);
            }
            rendered.accounts.clone_from(&state.accounts);
        }
        let page = match state.step {
            Step::Password => "password",
            Step::TwoFactor | Step::FactorChoice => "factor",
            Step::MailboxPassword => "mailbox",
            Step::SecurityKey | Step::KeyPin | Step::TouchKey => "security",
            Step::Finished => "finished",
            Step::OfficialGui => "official",
        };
        self.stack.set_visible_child_name(page);
        if rendered.step != Some(state.step) || (!rendered.ready && state.ready) {
            if enabled {
                match state.step {
                    Step::Password => {
                        self.username.grab_focus();
                    }
                    Step::TwoFactor | Step::FactorChoice => {
                        self.secrets.code.grab_focus();
                    }
                    Step::MailboxPassword => {
                        self.secrets.mailbox.grab_focus();
                    }
                    Step::KeyPin => {
                        self.secrets.pin.grab_focus();
                    }
                    _ => {}
                }
            }
            rendered.step = Some(state.step);
        }
        rendered.ready = state.ready;
    }
}

pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
    adw::init()?;
    let app = adw::Application::builder()
        .application_id("com.spotty.ProtonBridge")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        let mut model = LoginWindow::unconnected();
        if std::env::args().any(|argument| argument == "--no-auto-start") {
            model.auto_start = false;
        }
        model.connect();
        let ui = Window::new(app, Rc::new(RefCell::new(model)));
        ui.window.present();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            ui.model.borrow_mut().tick();
            ui.render();
            let state = ui.model.borrow();
            let close = state.closing
                && state.close_finished
                && !state.open_official
                && state.official_started.is_none();
            drop(state);
            if close {
                ui.secrets.clear_all();
                ui.window.close();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    });
    // The helper flags are handled above; GTK must not parse Spotty's argv.
    let exit = app.run_with_args(&["spotty-proton-bridge-gui"]);
    if exit == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        Err("The Proton Bridge window could not start.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(ui: &Window, name: &str) {
        let Some(directory) = std::env::var_os("SPOTTY_PROTON_TEST_SCREENSHOTS") else {
            return;
        };
        ui.window.present();
        let context = glib::MainContext::default();
        for _ in 0..40 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let paintable = gtk::WidgetPaintable::new(Some(&ui.window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            ui.window.width() as f64,
            ui.window.height() as f64,
        );
        let node = snapshot.to_node().expect("The native window should render");
        ui.window
            .renderer()
            .unwrap()
            .render_texture(&node, None)
            .save_to_png(std::path::PathBuf::from(directory).join(format!("{name}.png")))
            .unwrap();
    }

    #[test]
    #[ignore = "requires a native desktop display; run with --ignored --test-threads=1"]
    fn adwaita_login_flow_keeps_edits_and_clears_credentials() {
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("com.spotty.ProtonBridge.Test")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let mut state = LoginWindow::unconnected();
        state.auto_start = false;
        state.ready = true;
        state.message = "Sign in to connect your Proton account.".into();
        let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        state.commands = Some(commands);
        let (updates, update_receiver) = mpsc::channel();
        state.updates = update_receiver;
        let model = Rc::new(RefCell::new(state));
        let ui = Window::new(&app, model.clone());
        snapshot(&ui, "login");
        if std::env::var_os("SPOTTY_PROTON_TEST_SCREENSHOTS").is_some() {
            adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
            snapshot(&ui, "login-dark");
            adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
        }
        ui.username.set_text("test@proton.me");
        ui.secrets.password.set_text("account-secret");
        // A background status refresh must never reset the form mid-edit.
        ui.render();
        assert_eq!(ui.secrets.password.text(), "account-secret");
        submit_action(&model, &ui.username, &ui.secrets, LoginMethod::Password)();
        assert!(ui.secrets.password.text().is_empty());
        assert!(matches!(receiver.try_recv().unwrap(), Command::Login {
            method: LoginMethod::Password, username, secret
        } if username == "test@proton.me" && *secret == "account-secret"));

        for (step, page, method) in [
            (Step::FactorChoice, "factor", LoginMethod::TwoFactor),
            (
                Step::MailboxPassword,
                "mailbox",
                LoginMethod::MailboxPassword,
            ),
            (Step::KeyPin, "security", LoginMethod::SecurityKey),
        ] {
            updates.send(Update::Step(step)).unwrap();
            model.borrow_mut().poll();
            ui.render();
            assert_eq!(ui.stack.visible_child_name().as_deref(), Some(page));
            snapshot(&ui, page);
            assert!(ui.stack.is_sensitive());
            let row = match method {
                LoginMethod::TwoFactor => &ui.secrets.code,
                LoginMethod::MailboxPassword => &ui.secrets.mailbox,
                _ => &ui.secrets.pin,
            };
            row.set_text("verification-secret");
            submit_action(&model, &ui.username, &ui.secrets, method)();
            assert!(row.text().is_empty());
            assert!(
                matches!(receiver.try_recv().unwrap(), Command::Login { secret, .. }
                if *secret == "verification-secret")
            );
        }
        updates.send(Update::Step(Step::TouchKey)).unwrap();
        model.borrow_mut().poll();
        ui.render();
        assert!(!ui.secrets.pin.is_visible());
        assert!(!ui.security_button.is_visible());

        updates
            .send(Update::MailSettings(session::MailSettings {
                id: "account-id".into(),
                username: "test@proton.me".into(),
                addresses: vec!["test@proton.me".into()],
                password: Zeroizing::new("GeneratedBridgeSecret".into()),
                hostname: "127.0.0.1".into(),
                imap_port: 1143,
                smtp_port: 1025,
                imap_ssl: false,
                smtp_ssl: true,
            }))
            .unwrap();
        model.borrow_mut().poll();
        ui.render();
        assert_eq!(ui.stack.visible_child_name().as_deref(), Some("finished"));
        snapshot(&ui, "mail-settings");
        if std::env::var_os("SPOTTY_PROTON_TEST_SCREENSHOTS").is_some() {
            ui.window.set_default_size(380, 760);
            snapshot(&ui, "mail-settings-narrow");
        }
        let entry = ui.secrets.generated.borrow().as_ref().unwrap().clone();
        assert_eq!(entry.text(), "GeneratedBridgeSecret");
        assert!(entry.property::<bool>("visibility"));
        assert!(!entry.is_editable());
        let controls = entry.parent().unwrap();
        let reveal = entry
            .next_sibling()
            .unwrap()
            .downcast::<gtk::ToggleButton>()
            .unwrap();
        reveal.set_active(false);
        assert!(!entry.property::<bool>("visibility"));
        reveal.set_active(true);
        assert!(entry.property::<bool>("visibility"));
        assert_eq!(
            controls.last_child().unwrap().tooltip_text().as_deref(),
            Some("Copy Bridge password")
        );

        // Switching account errors must discard the displayed generated secret.
        updates
            .send(Update::Error {
                step: Step::Password,
                message: "Account is locked".into(),
            })
            .unwrap();
        model.borrow_mut().poll();
        ui.render();
        assert!(entry.text().is_empty());
        assert!(ui.secrets.generated.borrow().is_none());
        assert!(model.borrow().mail_settings.is_none());
        ui.secrets.password.set_text("unsent-secret");
        ui.window.emit_by_name::<bool>("close-request", &[]);
        assert!(ui.secrets.password.text().is_empty());
        assert!(matches!(receiver.try_recv().unwrap(), Command::Close));
        assert!(!model.borrow().close_finished);
        updates.send(Update::Closed).unwrap();
        model.borrow_mut().poll();
        assert!(model.borrow().close_finished);
        ui.window.destroy();
    }
}
