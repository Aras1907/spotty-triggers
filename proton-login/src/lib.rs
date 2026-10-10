//! The one sign-in page that every Proton integration in Spotty shows:
//! Calendar, Drive, Pass, VPN and Mail Bridge. It is plain libadwaita, built the
//! same way for each app, and the app plugs its own sign-in into it through the
//! [`Backend`] trait.
//!
//! The page has three steps, all on the same page: the email and password,
//! then (when Proton asks) an authenticator code, then (when Proton asks) a
//! second password. Each step swaps its fields in place. Enter submits a step,
//! Back cancels the sign-in in progress and wipes what was typed.
//!
//! Secrets: what is typed goes into a `Zeroizing` buffer and the entry is
//! emptied at once. Only the backend ever gets the buffer. Nothing here keeps a
//! secret after its step.
//!
//! Every backend call blocks (it talks to Proton or to a child process), so the
//! page runs it on a worker thread and handles the answer on the GTK thread.

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
pub use zeroize::Zeroizing;

/// Translates the page's own text. Spotty passes its gettext; others pass the
/// English text unchanged.
pub type Translate = fn(&str) -> String;

/// Which app the page signs in to.
#[derive(Clone)]
pub struct App {
    /// The app's name as shown, for example "Proton Calendar".
    pub name: String,
    /// The icon in the page header, for example "x-office-calendar-symbolic".
    pub icon: String,
    /// Where "Create a free Proton account" goes.
    pub signup_url: String,
    /// Translates the page's text.
    pub translate: Translate,
}

/// What one sign-in step produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Signed in. The app now keeps its own session.
    Done,
    /// Proton asks for an authenticator code (or a recovery code).
    TwoFactor,
    /// Proton asks for a second password. `title` names it on the page and
    /// `label` is the field's label.
    SecondPassword { title: String, label: String },
    /// The step failed. The message is shown on the page; the page stays on
    /// the step it was on, so a wrong code can be typed again.
    Failed(String),
}

/// One app's sign-in, as the page calls it. Every method blocks. The page
/// calls them on a worker thread, one at a time.
pub trait Backend: Send + Sync + 'static {
    /// Start a sign-in with the email and password.
    fn begin(&self, email: &str, password: Zeroizing<String>) -> Step;
    /// Answer the authenticator-code step.
    fn submit_code(&self, code: Zeroizing<String>) -> Step;
    /// Answer the second-password step.
    fn submit_second(&self, password: Zeroizing<String>) -> Step;
    /// Stop a sign-in that is half done, and forget what it kept.
    fn cancel(&self);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Credentials,
    Code,
    Second,
}

impl Page {
    fn name(self) -> &'static str {
        match self {
            Page::Credentials => "credentials",
            Page::Code => "code",
            Page::Second => "second",
        }
    }
}

/// The sign-in page. Put [`LoginPage::widget`] into a stack or a window.
#[derive(Clone)]
pub struct LoginPage {
    inner: Rc<Inner>,
}

struct Inner {
    app: App,
    backend: Arc<dyn Backend>,
    on_done: Box<dyn Fn()>,
    root: adw::StatusPage,
    banner: adw::Banner,
    stack: gtk::Stack,
    email: adw::EntryRow,
    password: adw::PasswordEntryRow,
    sign_in: gtk::Button,
    code: adw::EntryRow,
    code_next: gtk::Button,
    code_back: gtk::Button,
    second_group: adw::PreferencesGroup,
    second: adw::PasswordEntryRow,
    second_next: gtk::Button,
    second_back: gtk::Button,
    spinners: [gtk::Spinner; 3],
    page: Cell<Page>,
    busy: Cell<bool>,
    /// Bumped by every reset, so an answer that arrives after one is dropped.
    generation: Cell<u64>,
}

impl LoginPage {
    /// Build the page. `on_done` runs on the GTK thread once Proton has signed
    /// the app in; the page has already cleared its fields by then.
    pub fn new(app: App, backend: Arc<dyn Backend>, on_done: impl Fn() + 'static) -> Self {
        let tr = app.translate;
        let title = tr("Sign in to {app}").replace("{app}", &app.name);
        let description = tr("Use your Proton account. This sign-in is only for {app}.").replace("{app}", &app.name);
        let root = adw::StatusPage::builder().icon_name(app.icon.as_str()).title(title).description(description).build();

        let banner = adw::Banner::builder().revealed(false).build();
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();

        // Email and password
        let email = adw::EntryRow::builder().title(tr("Email or username")).input_purpose(gtk::InputPurpose::Email).build();
        let password = adw::PasswordEntryRow::builder().title(tr("Password")).build();
        let credentials = adw::PreferencesGroup::new();
        credentials.add(&email);
        credentials.add(&password);
        let sign_in = gtk::Button::builder().label(tr("Sign in")).css_classes(["suggested-action", "pill"]).build();
        let spinner_a = gtk::Spinner::builder().visible(false).build();
        let buttons_a = button_row(&[&sign_in], &spinner_a);
        let signup = gtk::LinkButton::builder().uri(app.signup_url.as_str()).label(tr("Create a free Proton account")).halign(gtk::Align::Center).build();
        let credentials_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
        credentials_box.append(&credentials);
        credentials_box.append(&buttons_a);
        credentials_box.append(&signup);
        stack.add_named(&credentials_box, Some(Page::Credentials.name()));

        // Authenticator code
        let code_group = adw::PreferencesGroup::builder().title(tr("Two-factor code")).build();
        let code = adw::EntryRow::builder().title(tr("Code")).show_apply_button(true).input_purpose(gtk::InputPurpose::Digits).build();
        code_group.add(&code);
        let code_back = gtk::Button::builder().label(tr("Back")).css_classes(["flat", "pill"]).build();
        let code_next = gtk::Button::builder().label(tr("Continue")).css_classes(["suggested-action", "pill"]).build();
        let spinner_b = gtk::Spinner::builder().visible(false).build();
        let buttons_b = button_row(&[&code_back, &code_next], &spinner_b);
        let code_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
        code_box.append(&code_group);
        code_box.append(&buttons_b);
        stack.add_named(&code_box, Some(Page::Code.name()));

        // Second password (mailbox or extra password)
        let second_group = adw::PreferencesGroup::builder().title(tr("Mailbox password")).build();
        let second = adw::PasswordEntryRow::builder().title(tr("Mailbox password")).show_apply_button(true).build();
        second_group.add(&second);
        let second_back = gtk::Button::builder().label(tr("Back")).css_classes(["flat", "pill"]).build();
        let second_next = gtk::Button::builder().label(tr("Continue")).css_classes(["suggested-action", "pill"]).build();
        let spinner_c = gtk::Spinner::builder().visible(false).build();
        let buttons_c = button_row(&[&second_back, &second_next], &spinner_c);
        let second_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
        second_box.append(&second_group);
        second_box.append(&buttons_c);
        stack.add_named(&second_box, Some(Page::Second.name()));

        let column = adw::Clamp::builder().maximum_size(420).margin_bottom(24).margin_start(16).margin_end(16).build();
        column.set_child(Some(&stack));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.append(&banner);
        content.append(&column);
        root.set_child(Some(&content));

        let inner = Rc::new(Inner {
            app,
            backend,
            on_done: Box::new(on_done),
            root,
            banner,
            stack,
            email,
            password,
            sign_in: sign_in.clone(),
            code: code.clone(),
            code_next: code_next.clone(),
            code_back: code_back.clone(),
            second_group,
            second: second.clone(),
            second_next: second_next.clone(),
            second_back: second_back.clone(),
            spinners: [spinner_a, spinner_b, spinner_c],
            page: Cell::new(Page::Credentials),
            busy: Cell::new(false),
            generation: Cell::new(0),
        });
        {
            let i = inner.clone();
            sign_in.connect_clicked(move |_| i.submit_credentials());
            let i = inner.clone();
            inner.password.connect_entry_activated(move |_| i.submit_credentials());
            let i = inner.clone();
            inner.email.connect_entry_activated(move |_| {
                i.password.grab_focus();
            });
            let i = inner.clone();
            code.connect_apply(move |_| i.submit_code());
            let i = inner.clone();
            code.connect_entry_activated(move |_| i.submit_code());
            let i = inner.clone();
            code_next.connect_clicked(move |_| i.submit_code());
            let i = inner.clone();
            code_back.connect_clicked(move |_| i.back());
            let i = inner.clone();
            second.connect_apply(move |_| i.submit_second());
            let i = inner.clone();
            second.connect_entry_activated(move |_| i.submit_second());
            let i = inner.clone();
            second_next.connect_clicked(move |_| i.submit_second());
            let i = inner.clone();
            second_back.connect_clicked(move |_| i.back());
        }
        Self { inner }
    }

    /// The page, to put in a stack or a window.
    pub fn widget(&self) -> gtk::Widget {
        self.inner.root.clone().upcast()
    }

    /// Back to the empty email-and-password step. Anything typed is wiped, and
    /// a sign-in half done is cancelled. Call it when the page is shown again
    /// after a sign-out, or when the window holding it closes.
    pub fn reset(&self) {
        self.inner.reset(true);
    }

    /// Clear what was typed and go back to the first step, without stopping a
    /// sign-in that runs at the backend (use [`LoginPage::reset`] for that).
    pub fn clear(&self) {
        self.inner.reset(false);
    }

    /// Fill in the email (for example the account picked to sign in again).
    pub fn set_email(&self, email: &str) {
        self.inner.email.set_text(email);
    }

    /// Whether a sign-in is running right now.
    pub fn is_busy(&self) -> bool {
        self.inner.busy.get()
    }
}

/// A row of buttons with a spinner for the work they start.
fn button_row(buttons: &[&gtk::Button], spinner: &gtk::Spinner) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    for button in buttons {
        row.append(*button);
    }
    row.append(spinner);
    row
}

/// Take what was typed: copied into a `Zeroizing` buffer, and the entry emptied.
fn take(row: &impl IsA<gtk::Editable>) -> Zeroizing<String> {
    let text = Zeroizing::new(row.text().to_string());
    row.set_text("");
    text
}

impl Inner {
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        for row in [self.email.upcast_ref::<gtk::Widget>(), self.password.upcast_ref(), self.code.upcast_ref(), self.second.upcast_ref()] {
            row.set_sensitive(!busy);
        }
        for button in [&self.sign_in, &self.code_next, &self.code_back, &self.second_next, &self.second_back] {
            button.set_sensitive(!busy);
        }
        for spinner in &self.spinners {
            spinner.set_visible(busy);
            spinner.set_spinning(busy);
        }
    }

    fn show(&self, page: Page) {
        self.page.set(page);
        self.stack.set_visible_child_name(page.name());
    }

    fn error(&self, message: &str) {
        self.banner.set_title(message);
        self.banner.set_revealed(true);
    }

    fn submit_credentials(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let email = self.email.text().trim().to_owned();
        let password = take(&self.password);
        if email.is_empty() || password.is_empty() {
            self.error(&(self.app.translate)("Enter your Proton email and password."));
            return;
        }
        self.run(move |backend| backend.begin(&email, password));
    }

    fn submit_code(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let code = take(&self.code);
        if code.trim().is_empty() {
            self.error(&(self.app.translate)("Enter the code from your authenticator app."));
            return;
        }
        self.run(move |backend| backend.submit_code(code));
    }

    fn submit_second(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let password = take(&self.second);
        if password.is_empty() {
            self.error(&(self.app.translate)("Enter the password."));
            return;
        }
        self.run(move |backend| backend.submit_second(password));
    }

    /// Run one backend call on a worker thread and handle its step here.
    fn run(self: &Rc<Self>, work: impl FnOnce(&dyn Backend) -> Step + Send + 'static) {
        self.banner.set_revealed(false);
        self.set_busy(true);
        let generation = self.generation.get();
        let backend = self.backend.clone();
        let inner = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let step = match gio::spawn_blocking(move || work(backend.as_ref())).await {
                Ok(step) => step,
                Err(_) => Step::Failed(String::new()),
            };
            // A reset (sign-out, window closed) while the call ran drops its answer.
            if inner.generation.get() == generation {
                inner.after(step);
            }
        });
    }

    fn after(self: &Rc<Self>, step: Step) {
        self.set_busy(false);
        match step {
            Step::Done => {
                self.reset(false);
                (self.on_done)();
            }
            Step::TwoFactor => {
                self.code.set_text("");
                self.show(Page::Code);
                self.code.grab_focus();
            }
            Step::SecondPassword { title, label } => {
                self.second_group.set_title(&title);
                self.second.set_title(&label);
                self.second.set_text("");
                self.show(Page::Second);
                self.second.grab_focus();
            }
            Step::Failed(message) => {
                let message = if message.is_empty() { (self.app.translate)("Sign-in failed. Try again.") } else { message };
                self.error(&message);
            }
        }
    }

    /// Back to the email step. `cancel` also stops the sign-in at the backend.
    fn back(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        self.reset(true);
    }

    /// Clear every field and return to the first step. With `cancel`, a
    /// sign-in half done is stopped too (on a worker thread: it may talk to
    /// Proton).
    fn reset(&self, cancel: bool) {
        self.generation.set(self.generation.get() + 1);
        self.email.set_text("");
        self.password.set_text("");
        self.code.set_text("");
        self.second.set_text("");
        self.banner.set_revealed(false);
        self.set_busy(false);
        self.show(Page::Credentials);
        if cancel {
            let backend = self.backend.clone();
            std::thread::spawn(move || backend.cancel());
        }
    }
}

/// The "Account" group at the top of a settings page, once the app is signed
/// in: the signed-in email, and a Sign out button that asks first.
pub struct Account {
    pub group: adw::PreferencesGroup,
    row: adw::ActionRow,
}

impl Account {
    /// `sign_out` runs on the GTK thread after the user confirms.
    pub fn new(app: &App, email: &str, sign_out: impl Fn() + 'static) -> Self {
        let tr = app.translate;
        let group = adw::PreferencesGroup::builder().title(tr("Account")).build();
        let row = adw::ActionRow::builder().title(email).use_markup(false).build();
        row.add_prefix(&gtk::Image::from_icon_name(app.icon.as_str()));
        let button = gtk::Button::builder().label(tr("Sign out")).valign(gtk::Align::Center).css_classes(["destructive-action"]).build();
        row.add_suffix(&button);
        group.add(&row);
        let app = app.clone();
        let sign_out = Rc::new(sign_out);
        button.connect_clicked(move |button| {
            let sign_out = sign_out.clone();
            confirm_sign_out(button, &app, move || sign_out());
        });
        Self { group, row }
    }

    /// Show a different signed-in email (after a sign-in as someone else).
    pub fn set_email(&self, email: &str) {
        self.row.set_title(email);
    }
}

/// Ask before signing out. `on_confirm` runs only on "Sign out".
pub fn confirm_sign_out(parent: &impl IsA<gtk::Widget>, app: &App, on_confirm: impl Fn() + 'static) {
    let tr = app.translate;
    let dialog = adw::AlertDialog::builder()
        .heading(tr("Sign out of {app} in Spotty?").replace("{app}", &app.name))
        .body(tr("Spotty ends this sign-in and deletes what it kept for {app} on this computer. Your Proton account and your other Proton apps are not affected.").replace("{app}", &app.name))
        .build();
    dialog.add_response("cancel", &tr("Cancel"));
    dialog.add_response("signout", &tr("Sign out"));
    dialog.set_response_appearance("signout", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("signout"), move |_, _| on_confirm());
    dialog.present(Some(parent));
}

/// Where an app's sign-in stands, as a settings page asks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    /// Not known yet (the saved session is still loading).
    Checking,
    SignedIn,
    SignedOut,
}

/// Shows the sign-in page while the app is signed out, `signed_in_page` once it
/// is signed in, and a short "checking" page until the state is known.
/// `session` is asked once a second on the GTK thread, so it must not block.
/// The page follows a sign-in or sign-out made anywhere; when the app goes back
/// to signed out, the sign-in page starts afresh.
pub fn sign_in_stack(login: &LoginPage, signed_in_page: &impl IsA<gtk::Widget>, session: impl Fn() -> Session + 'static) -> gtk::Stack {
    const CHECKING: &str = "checking";
    const SIGNED_IN: &str = "account";
    const SIGNED_OUT: &str = "signin";
    fn page_for(session: Session) -> &'static str {
        match session {
            Session::Checking => CHECKING,
            Session::SignedIn => SIGNED_IN,
            Session::SignedOut => SIGNED_OUT,
        }
    }
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    let checking = adw::StatusPage::builder()
        .title((login.inner.app.translate)("Checking your sign-in…"))
        .child(&gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build())
        .build();
    stack.add_named(&checking, Some(CHECKING));
    stack.add_named(&login.widget(), Some(SIGNED_OUT));
    stack.add_named(signed_in_page, Some(SIGNED_IN));
    let shown = Cell::new(session());
    stack.set_visible_child_name(page_for(shown.get()));
    let weak = stack.downgrade();
    let login = login.clone();
    glib::timeout_add_seconds_local(1, move || {
        let Some(stack) = weak.upgrade() else { return glib::ControlFlow::Break };
        let now = session();
        if now != shown.get() {
            if now == Session::SignedOut {
                login.reset();
            }
            shown.set(now);
            stack.set_visible_child_name(page_for(now));
        }
        glib::ControlFlow::Continue
    });
    stack
}
