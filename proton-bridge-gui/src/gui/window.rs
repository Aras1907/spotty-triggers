use super::*;
use adw::prelude::*;
use spotty_proton_login as login;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

type Model = Rc<RefCell<LoginWindow>>;

/// The generated Bridge password the mail page shows. Cleared when that page changes.
#[derive(Clone)]
struct Secrets {
    generated: Rc<RefCell<Option<gtk::Entry>>>,
}

#[derive(Clone)]
struct WeakSecrets {
    generated: std::rc::Weak<RefCell<Option<gtk::Entry>>>,
}

impl Secrets {
    fn downgrade(&self) -> WeakSecrets {
        WeakSecrets { generated: Rc::downgrade(&self.generated) }
    }

    fn clear_generated(&self) {
        if let Some(entry) = self.generated.borrow_mut().take() {
            entry.set_text("");
        }
    }
}

impl WeakSecrets {
    fn upgrade(&self) -> Option<Secrets> {
        Some(Secrets { generated: self.generated.upgrade()? })
    }
}

#[derive(Default)]
struct Rendered {
    step: Option<Step>,
    ready: bool,
    mail_revision: Option<u64>,
    mail_error: Option<(String, String)>,
    accounts: Vec<(String, String, i32)>,
    account_rows: Vec<adw::ActionRow>,
}

struct Window {
    model: Model,
    window: gtk::Widget,
    standalone_window: Option<adw::ApplicationWindow>,
    overlay: adw::ToastOverlay,
    status: gtk::Label,
    spinner: gtk::Spinner,
    stack: gtk::Stack,
    login: login::LoginPage,
    secrets: Secrets,
    mail: gtk::Box,
    accounts: adw::PreferencesGroup,
    preferences: adw::PreferencesGroup,
    startup: adw::SwitchRow,
    recovery: gtk::Box,
    add_account: gtk::Button,
    close: gtk::Button,
    rendered: RefCell<Rendered>,
}

fn bridge_app() -> login::App {
    login::App {
        name: "Proton Mail Bridge".into(),
        icon: "mail-send-symbolic".into(),
        signup_url: "https://account.proton.me/signup".into(),
        translate: |text| text.to_owned(),
    }
}

fn request_account_settings(model: &Model, secrets: &WeakSecrets, account_id: String) {
    let mut state = model.borrow_mut();
    if !state.ready || state.busy || state.closing {
        return;
    }
    state.mail_settings = None;
    state.settings_retry_id = None;
    state.busy = true;
    state.message = "Loading saved mail settings…".into();
    state.send(Command::ShowAccount(account_id));
    if let Some(secrets) = secrets.upgrade() {
        secrets.clear_generated();
    }
}

fn password_clipboard_content(value: &str) -> gtk::gdk::ContentProvider {
    let text = value.to_value();
    let text_provider = gtk::gdk::ContentProvider::for_value(&text);
    let secret_hint = gtk::gdk::ContentProvider::for_bytes(
        "x-kde-passwordManagerHint",
        &glib::Bytes::from_static(b"secret"),
    );
    gtk::gdk::ContentProvider::new_union(&[text_provider, secret_hint])
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
        Self::build(window.clone().upcast(), Some(window), model, None)
    }

    fn build(
        window: gtk::Widget,
        standalone_window: Option<adw::ApplicationWindow>,
        model: Model,
        on_back: Option<Rc<dyn Fn()>>,
    ) -> Rc<Self> {
        let embedded = model.borrow().embedded;
        let overlay = adw::ToastOverlay::new();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("Proton Mail Bridge", "Spotty"))
            .build();
        if !embedded {
            toolbar.add_top_bar(&header);
        }
        let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
        for margin in ["margin-start", "margin-end", "margin-top", "margin-bottom"] {
            body.set_property(margin, 18i32);
        }
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

        let secrets = Secrets { generated: Rc::new(RefCell::new(None)) };
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(180)
            .vhomogeneous(false)
            .hhomogeneous(false)
            .build();
        // Sign-in: the one sign-in page every Proton app shows, wired to Bridge's login stream.
        let link = model.borrow().link.clone();
        let login = login::LoginPage::new(bridge_app(), Arc::new(BridgeLogin(link)), || {});
        stack.add_named(&login.widget(), Some("password"));

        let mail = gtk::Box::new(gtk::Orientation::Vertical, 16);
        stack.add_named(&mail, Some("finished"));
        let verification = adw::StatusPage::builder()
            .icon_name("dialog-information-symbolic")
            .title("Check your keyring")
            .description("Unlock your Linux keyring in your desktop settings, then retry sign-in. Human verification may require a Proton account recovery step.")
            .build();
        stack.add_named(&verification, Some("recovery"));
        body.append(&stack);

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
        let menu = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Bridge options")
            .build();
        let popover = gtk::Popover::new();
        let options = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for margin in ["margin-start", "margin-end", "margin-top", "margin-bottom"] {
            options.set_property(margin, 6i32);
        }
        let add_account = gtk::Button::builder()
            .label("Add account")
            .css_classes(["flat"])
            .build();
        {
            let model = model.clone();
            let secrets = secrets.downgrade();
            let login = login.clone();
            let popover = popover.downgrade();
            add_account.connect_clicked(move |_| {
                let Some(secrets) = secrets.upgrade() else {
                    return;
                };
                let mut state = model.borrow_mut();
                if !state.ready || state.busy || state.closing {
                    return;
                }
                state.username.clear();
                state.mail_settings = None;
                state.step = Step::Password;
                state.message = "Sign in to another Proton account.".into();
                login.clear();
                secrets.clear_generated();
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
            });
        }
        options.append(&add_account);
        let help = gtk::LinkButton::with_label(
            "https://proton.me/support/protonmail-bridge-install",
            "Bridge setup guide",
        );
        options.append(&help);
        popover.set_child(Some(&options));
        menu.set_popover(Some(&popover));
        if embedded {
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            actions.set_halign(gtk::Align::End);
            actions.append(&menu);
            body.prepend(&actions);
        } else {
            header.pack_end(&menu);
        }
        let close = gtk::Button::builder()
            .label("Close and keep Bridge running")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        if let Some(on_back) = on_back {
            let login = login.clone();
            close.set_label("Back to Spotty Settings");
            close.connect_clicked(move |_| {
                login.clear();
                on_back();
            });
        } else {
            let weak_window = standalone_window.as_ref().unwrap().downgrade();
            close.connect_clicked(move |_| {
                if let Some(window) = weak_window.upgrade() {
                    window.close();
                }
            });
        }
        body.append(&close);
        let clamp = adw::Clamp::builder()
            .maximum_size(540)
            .tightening_threshold(420)
            .child(&body)
            .build();
        if embedded {
            // PreferencesPage already scrolls its contents. Nesting a second
            // ScrolledWindow here prevents the page from negotiating a useful
            // natural height for the embedded Bridge controls.
            overlay.set_child(Some(&clamp));
        } else {
            let scroll = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vscrollbar_policy(gtk::PolicyType::Automatic)
                .child(&clamp)
                .build();
            toolbar.set_content(Some(&scroll));
            overlay.set_child(Some(&toolbar));
        }
        if let Some(standalone) = &standalone_window {
            standalone.set_content(Some(&overlay));
            let model = model.clone();
            let login = login.clone();
            standalone.connect_close_request(move |_| {
                let mut state = model.borrow_mut();
                state.begin_close();
                login.clear();
                if state.close_finished {
                    glib::Propagation::Proceed
                } else {
                    glib::Propagation::Stop
                }
            });
        }
        let ui = Rc::new(Self {
            model,
            window,
            standalone_window,
            overlay,
            status,
            spinner,
            stack,
            login,
            secrets,
            mail,
            accounts,
            preferences,
            startup,
            recovery,
            add_account,
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
                let provider = password_clipboard_content(settings.password.as_str());
                let _ = button.clipboard().set_content(Some(&provider));
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
            .visibility(false)
            .hexpand(true)
            .css_classes(["flat", "monospace"])
            .input_hints(gtk::InputHints::PRIVATE)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label("Bridge password")]);
        let reveal = gtk::ToggleButton::builder()
            .icon_name("view-reveal-symbolic")
            .tooltip_text("Show Bridge password")
            .active(false)
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

    fn render_mail_settings_error(&self, message: &str, account_id: String) {
        let status = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Couldn't load mail settings")
            .description(format!(
                "{message}\nChoose Settings beside the connected account below to try again."
            ))
            .build();
        self.mail.append(&status);
        let retry = gtk::Button::builder()
            .label("Retry")
            .halign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        let model = self.model.clone();
        let secrets = self.secrets.downgrade();
        retry.connect_clicked(move |_| {
            request_account_settings(&model, &secrets, account_id.clone());
        });
        self.mail.append(&retry);
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
        self.preferences.set_visible(!state.accounts.is_empty());
        self.accounts.set_sensitive(enabled);
        let can_recover =
            !state.closing && state.start_pending.is_none() && state.preparing.is_none();
        self.recovery
            .set_visible(!state.ready && state.commands.is_none() && can_recover);
        self.recovery.set_sensitive(can_recover);
        self.add_account.set_sensitive(enabled);
        self.close
            .set_visible(state.embedded || state.step == Step::Finished);
        let mail_error = if state.mail_settings.is_none()
            && state.step == Step::Finished
            && !state.busy
        {
            // The retry target is part of the rendered error state: two
            // accounts can share the same sanitized status message.
            state
                .settings_retry_id
                .clone()
                .map(|id| (state.message.clone(), id))
        } else {
            None
        };
        let mail_changed = if state.mail_settings.is_some() {
            rendered.mail_revision != Some(state.mail_revision)
        } else {
            rendered.mail_revision.is_some() || rendered.mail_error != mail_error
        };
        if mail_changed {
            self.secrets.clear_generated();
            while let Some(child) = self.mail.first_child() {
                self.mail.remove(&child);
            }
            rendered.mail_revision = None;
            rendered.mail_error = None;
            if let Some(settings) = &state.mail_settings {
                self.render_mail(settings);
                rendered.mail_revision = Some(state.mail_revision);
            } else if let Some((message, account_id)) = &mail_error {
                self.render_mail_settings_error(message, account_id.clone());
                rendered.mail_error = Some((message.clone(), account_id.clone()));
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
                        .label("Settings")
                        .valign(gtk::Align::Center)
                        .build();
                    let id = id.clone();
                    let account_id = id.clone();
                    let model = self.model.clone();
                    let secrets = self.secrets.downgrade();
                    button.connect_clicked(move |_| {
                        request_account_settings(&model, &secrets, id.clone());
                    });
                    row.add_suffix(&button);
                    let logout = gtk::Button::builder()
                        .label("Sign out")
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    let model = self.model.clone();
                    let secrets = self.secrets.downgrade();
                    let parent = self.window.downgrade();
                    logout.connect_clicked(move |_| {
                        let Some(parent) = parent.upgrade() else { return };
                        let dialog = adw::AlertDialog::new(
                            Some("Sign out of Proton Mail?"),
                            Some("Mail apps using this account will disconnect. You can sign in again later."),
                        );
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("sign-out", "Sign out");
                        dialog.set_response_appearance("sign-out", adw::ResponseAppearance::Destructive);
                        dialog.set_default_response(Some("cancel"));
                        let model = model.clone();
                        let secrets = secrets.clone();
                        let account_id = account_id.clone();
                        glib::MainContext::default().spawn_local(async move {
                            if dialog.choose_future(&parent).await.as_str() == "sign-out" {
                                let mut state = model.borrow_mut();
                                state.logout(account_id);
                                if let Some(secrets) = secrets.upgrade() {
                                    secrets.clear_generated();
                                }
                            }
                        });
                    });
                    row.add_suffix(&logout);
                } else {
                    let button = gtk::Button::builder()
                        .label("Sign in")
                        .valign(gtk::Align::Center)
                        .build();
                    let account_name = name.clone();
                    let model = self.model.clone();
                    let login = self.login.clone();
                    let secrets = self.secrets.downgrade();
                    button.connect_clicked(move |_| {
                        let Some(secrets) = secrets.upgrade() else {
                            return;
                        };
                        let mut state = model.borrow_mut();
                        state.username.clone_from(&account_name);
                        state.mail_settings = None;
                        state.step = Step::Password;
                        login.clear();
                        login.set_email(&account_name);
                        secrets.clear_generated();
                    });
                    row.add_suffix(&button);
                }
                self.accounts.add(&row);
                rendered.account_rows.push(row);
            }
            rendered.accounts.clone_from(&state.accounts);
        }
        // The shared sign-in page runs the sign-in steps itself; only the
        // finished and recovery pages are switched here.
        let page = match state.step {
            Step::Finished => "finished",
            Step::Recovery => "recovery",
            _ => "password",
        };
        self.stack.set_visible_child_name(page);
        rendered.step = Some(state.step);
        rendered.ready = state.ready;
    }
}

pub(super) struct EmbeddedBridge {
    ui: Option<Rc<Window>>,
    timer: Option<glib::SourceId>,
}

impl EmbeddedBridge {
    /// Build embedded Bridge controls with the caller's widget as their host
    /// and their parent for confirmation dialogs.
    pub(super) fn new(parent: &impl IsA<gtk::Widget>, on_back: impl Fn() + 'static) -> Self {
        let mut model = LoginWindow::unconnected();
        model.embedded = true;
        model.connect();
        let ui = Window::build(
            parent.clone().upcast(),
            None,
            Rc::new(RefCell::new(model)),
            Some(Rc::new(on_back)),
        );
        Self::from_ui(ui)
    }

    fn from_ui(ui: Rc<Window>) -> Self {
        let weak_ui = Rc::downgrade(&ui);
        let timer = glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(ui) = weak_ui.upgrade() else {
                return glib::ControlFlow::Break;
            };
            ui.model.borrow_mut().tick();
            ui.render();
            glib::ControlFlow::Continue
        });
        Self {
            ui: Some(ui),
            timer: Some(timer),
        }
    }

    pub(super) fn widget(&self) -> gtk::Widget {
        self.ui
            .as_ref()
            .expect("embedded Bridge handle is closed")
            .overlay
            .clone()
            .upcast()
    }

    pub(super) fn close(mut self) {
        self.close_inner(None);
    }

    pub(super) fn close_with_completion(mut self, on_closed: impl FnOnce() + 'static) {
        self.close_inner(Some(Box::new(on_closed)));
    }

    fn close_inner(&mut self, mut on_closed: Option<Box<dyn FnOnce()>>) {
        let Some(ui) = self.ui.take() else {
            if let Some(on_closed) = on_closed.take() {
                on_closed();
            }
            return;
        };
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        ui.secrets.clear_generated();
        ui.login.clear();
        {
            let mut state = ui.model.borrow_mut();
            // Uninstalling should detach the embedded login stream.
            state.detach_embedded();
        }

        if ui.model.borrow().close_finished {
            ui.secrets.clear_generated();
            if let Some(on_closed) = on_closed.take() {
                on_closed();
            }
            return;
        }

        // Keep the controller (and its Tokio runtime) alive while the session
        // sends the RPC Close frame and reports Update::Closed. The callback
        // owns the UI only during this short drain, then drops it on Break.
        let on_closed = RefCell::new(on_closed);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            ui.model.borrow_mut().poll();
            ui.secrets.clear_generated();
            let finished = ui.model.borrow().close_finished;
            if finished {
                ui.secrets.clear_generated();
                let on_closed = on_closed.borrow_mut().take();
                if let Some(on_closed) = on_closed {
                    on_closed();
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

impl Drop for EmbeddedBridge {
    fn drop(&mut self) {
        self.close_inner(None);
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
        ui.standalone_window.as_ref().unwrap().present();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            ui.model.borrow_mut().tick();
            ui.render();
            let state = ui.model.borrow();
            let close = ui.standalone_window.is_some() && state.closing && state.close_finished;
            drop(state);
            if close {
                ui.secrets.clear_generated();
                ui.standalone_window.as_ref().unwrap().close();
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

    fn pump() {
        let context = glib::MainContext::default();
        for _ in 0..30 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn button_in(widget: &impl IsA<gtk::Widget>, text: &str) -> Option<gtk::Button> {
        let widget = widget.as_ref();
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            if button.label().as_deref() == Some(text) {
                return Some(button.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(button) = button_in(&widget, text) {
                return Some(button);
            }
            child = widget.next_sibling();
        }
        None
    }

    fn snapshot(ui: &Window, name: &str) {
        let Some(directory) = std::env::var_os("SPOTTY_PROTON_TEST_SCREENSHOTS") else {
            return;
        };
        let window = ui.standalone_window.as_ref().unwrap();
        window.present();
        let context = glib::MainContext::default();
        for _ in 0..40 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let paintable = gtk::WidgetPaintable::new(Some(window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
        let node = snapshot.to_node().expect("The native window should render");
        window
            .renderer()
            .unwrap()
            .render_texture(&node, None)
            .save_to_png(std::path::PathBuf::from(directory).join(format!("{name}.png")))
            .unwrap();
    }

    #[test]
    #[ignore = "requires a native desktop display; run with --ignored --test-threads=1"]
    fn embedded_page_removal_drains_rpc_then_releases_ui_and_model() {
        adw::init().unwrap();
        let parent = adw::PreferencesWindow::new();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let mut state = LoginWindow::unconnected();
        state.embedded = true;
        state.auto_start = false;
        state.ready = true;
        state.username = "test@proton.me".into();
        state.accounts = vec![("account-id".into(), "test@proton.me".into(), 2)];
        state.mail_settings = Some(session::MailSettings {
            id: "account-id".into(),
            username: "test@proton.me".into(),
            addresses: vec!["test@proton.me".into()],
            password: Zeroizing::new("generated-bridge-secret".into()),
            hostname: "127.0.0.1".into(),
            imap_port: 1143,
            smtp_port: 1025,
            imap_ssl: false,
            smtp_ssl: true,
        });
        let (commands, mut command_receiver) = tokio::sync::mpsc::unbounded_channel();
        state.commands = Some(commands);
        let (updates, update_receiver) = mpsc::channel();
        state.updates = update_receiver;
        let model = Rc::new(RefCell::new(state));
        let ui = Window::build(
            parent.clone().upcast(),
            None,
            model.clone(),
            Some(Rc::new(|| {})),
        );
        ui.render();
        let generated = ui.secrets.generated.borrow().as_ref().unwrap().clone();
        let weak_ui = Rc::downgrade(&ui);
        let weak_model = Rc::downgrade(&model);
        let handle = EmbeddedBridge::from_ui(ui.clone());
        group.add(&handle.widget());
        page.add(&group);
        parent.add(&page);
        parent.set_visible_page(&page);
        parent.set_default_size(640, 720);
        parent.present();
        pump();
        {
            let ui = weak_ui
                .upgrade()
                .expect("embedded UI should remain installed");
            assert!(
                ui.overlay.height() >= 300,
                "embedded UI collapsed vertically"
            );
        }
        drop(ui);
        drop(model);

        let drained = Rc::new(std::cell::Cell::new(false));
        {
            let drained = drained.clone();
            handle.close_with_completion(move || drained.set(true));
        }
        parent.remove(&page);
        drop(group);
        drop(page);

        assert!(generated.text().is_empty());
        assert!(weak_model.upgrade().is_some());
        {
            let ui = weak_ui.upgrade().expect("drain timer should retain the UI");
            assert!(!ui.login.is_busy());
            assert!(ui.model.borrow().username.is_empty());
            assert!(ui.model.borrow().accounts.is_empty());
            assert!(ui.model.borrow().mail_settings.is_none());
        }
        assert!(matches!(command_receiver.try_recv(), Ok(Command::Close)));
        assert!(weak_ui.upgrade().is_some());
        assert!(!drained.get(), "the old frontend remains in drain until ack");

        updates.send(Update::Closed).unwrap();
        pump();
        assert!(drained.get(), "reopen may proceed after the RPC close ack");
        assert!(weak_ui.upgrade().is_none());
        assert!(weak_model.upgrade().is_none());
        drop(generated);
        drop(command_receiver);
    }

    #[test]
    #[ignore = "requires a native desktop display; run with --ignored --test-threads=1"]
    fn account_settings_failure_keeps_retry_and_masks_sensitive_credentials() {
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("com.spotty.ProtonBridge.RetryTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();

        let mut state = LoginWindow::unconnected();
        state.ready = true;
        state.step = Step::Finished;
        state.message = "Connected to Proton Mail.".into();
        state.accounts = vec![("test-account".into(), "test@proton.me".into(), 2)];
        state.mail_settings = Some(session::MailSettings {
            id: "test-account".into(),
            username: "test@proton.me".into(),
            addresses: vec!["test@proton.me".into()],
            password: Zeroizing::new("generated-secret".into()),
            hostname: "127.0.0.1".into(),
            imap_port: 1143,
            smtp_port: 1025,
            imap_ssl: false,
            smtp_ssl: true,
        });
        let (commands, mut command_receiver) = tokio::sync::mpsc::unbounded_channel();
        state.commands = Some(commands);
        let (updates, update_receiver) = mpsc::channel();
        state.updates = update_receiver;
        let model = Rc::new(RefCell::new(state));
        let ui = Window::new(&app, model.clone());
        let generated = ui.secrets.generated.borrow().as_ref().unwrap().clone();
        assert!(!generated.property::<bool>("visibility"));
        assert!(password_clipboard_content("generated-secret")
            .formats()
            .contain_mime_type("x-kde-passwordManagerHint"));

        updates
            .send(Update::AccountSettingsFailed {
                id: "test-account".into(),
                accounts: Some(vec![
                    ("test-account".into(), "test@proton.me".into(), 2),
                    ("second-account".into(), "second@proton.me".into(), 2),
                ]),
                message: "Bridge could not load these account settings. Retry the request.".into(),
            })
            .unwrap();
        model.borrow_mut().poll();
        ui.render();
        assert!(generated.text().is_empty());
        assert_eq!(model.borrow().settings_retry_id.as_deref(), Some("test-account"));
        assert!(button_in(&ui.rendered.borrow().account_rows[0], "Settings").is_some());

        // The local error text can match across accounts. The Retry closure
        // must refresh when its target ID changes even if the message does not.
        updates
            .send(Update::AccountSettingsFailed {
                id: "second-account".into(),
                accounts: Some(vec![
                    ("test-account".into(), "test@proton.me".into(), 2),
                    ("second-account".into(), "second@proton.me".into(), 2),
                ]),
                message: "Bridge could not load these account settings. Retry the request.".into(),
            })
            .unwrap();
        model.borrow_mut().poll();
        ui.render();
        assert_eq!(
            model.borrow().settings_retry_id.as_deref(),
            Some("second-account")
        );

        button_in(&ui.mail, "Retry").unwrap().emit_clicked();
        assert!(matches!(
            command_receiver.try_recv().unwrap(),
            Command::ShowAccount(id) if id == "second-account"
        ));
        assert!(model.borrow().settings_retry_id.is_none());
        // Production's 100 ms UI timer renders this model transition; drive
        // the same render pass explicitly in this fake-controller test.
        ui.render();
        assert!(ui.mail.first_child().is_none());

        model.borrow_mut().begin_close();
        assert!(matches!(command_receiver.try_recv(), Ok(Command::Close)));
        updates.send(Update::Closed).unwrap();
        model.borrow_mut().poll();
        ui.standalone_window.as_ref().unwrap().destroy();
        drop(ui);
        drop(model);
        drop(command_receiver);
        app.quit();
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
        // Signed out: the shared sign-in page is what the window shows.
        ui.render();
        assert_eq!(ui.stack.visible_child_name().as_deref(), Some("password"));
        ui.login.set_email("test@proton.me");
        snapshot(&ui, "login-filled");
        assert!(matches!(receiver.try_recv(), Err(_)), "nothing is sent until the page asks");

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
            ui.standalone_window
                .as_ref()
                .unwrap()
                .set_default_size(380, 760);
            snapshot(&ui, "mail-settings-narrow");
        }
        let mut entry = ui.secrets.generated.borrow().as_ref().unwrap().clone();
        assert_eq!(entry.text(), "GeneratedBridgeSecret");
        assert!(!entry.property::<bool>("visibility"));
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

        ui.standalone_window.as_ref().unwrap().present();
        pump();
        let account_row = ui.rendered.borrow().account_rows[0].clone();
        let sign_out = button_in(&account_row, "Sign out").unwrap();
        sign_out.emit_clicked();
        pump();
        let dialog = ui
            .standalone_window
            .as_ref()
            .unwrap()
            .visible_dialog()
            .unwrap();
        button_in(&dialog, "Cancel").unwrap().emit_clicked();
        pump();
        assert!(receiver.try_recv().is_err());
        assert_eq!(entry.text(), "GeneratedBridgeSecret");

        sign_out.emit_clicked();
        pump();
        let dialog = ui
            .standalone_window
            .as_ref()
            .unwrap()
            .visible_dialog()
            .unwrap();
        button_in(&dialog, "Sign out").unwrap().emit_clicked();
        pump();
        assert!(matches!(receiver.try_recv().unwrap(), Command::Logout(id) if id == "account-id"));
        assert!(entry.text().is_empty());
        assert!(model.borrow().mail_settings.is_none());
        assert!(model.borrow().busy);
        updates
            .send(Update::LoggedOut {
                accounts: vec![("account-id".into(), "test@proton.me".into(), 0)],
            })
            .unwrap();
        model.borrow_mut().poll();
        ui.render();
        assert_eq!(ui.stack.visible_child_name().as_deref(), Some("password"));
        let account_row = ui.rendered.borrow().account_rows[0].clone();
        button_in(&account_row, "Sign in").unwrap().emit_clicked();
        assert_eq!(model.borrow().step, Step::Password);
        assert_eq!(model.borrow().username, "test@proton.me");

        updates
            .send(Update::MailSettings(session::MailSettings {
                id: "account-id".into(),
                username: "test@proton.me".into(),
                addresses: vec!["test@proton.me".into()],
                password: Zeroizing::new("CurrentBridgeSecret".into()),
                hostname: "127.0.0.1".into(),
                imap_port: 1143,
                smtp_port: 1025,
                imap_ssl: false,
                smtp_ssl: true,
            }))
            .unwrap();
        model.borrow_mut().poll();
        ui.render();
        entry = ui.secrets.generated.borrow().as_ref().unwrap().clone();
        assert_eq!(entry.text(), "CurrentBridgeSecret");
        assert_eq!(ui.stack.visible_child_name().as_deref(), Some("finished"));

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
        ui.standalone_window
            .as_ref()
            .unwrap()
            .emit_by_name::<bool>("close-request", &[]);
        assert!(matches!(receiver.try_recv().unwrap(), Command::Close));
        assert!(!model.borrow().close_finished);
        updates.send(Update::Closed).unwrap();
        model.borrow_mut().poll();
        assert!(model.borrow().close_finished);
        ui.standalone_window.as_ref().unwrap().destroy();
    }
}
