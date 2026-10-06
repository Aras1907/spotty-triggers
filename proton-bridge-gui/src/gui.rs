use crate::rpc::{self, LoginMethod};
use crate::session::{self, Command, Step, Update};
mod window;
use std::sync::mpsc;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

struct LoginWindow {
    runtime: Option<tokio::runtime::Runtime>,
    commands: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    updates: mpsc::Receiver<Update>,
    username: String,
    password: Zeroizing<String>,
    code: Zeroizing<String>,
    mailbox: Zeroizing<String>,
    pin: Zeroizing<String>,
    step: Step,
    message: String,
    accounts: Vec<(String, String, i32)>,
    mail_settings: Option<session::MailSettings>,
    settings_retry_id: Option<String>,
    auto_start: bool,
    autostart: bool,
    ready: bool,
    busy: bool,
    closing: bool,
    close_finished: bool,
    waiting_for_initialization: bool,
    start_pending: Option<std::time::Instant>,
    preparing: Option<mpsc::Receiver<Result<(), String>>>,
    connecting_since: Option<std::time::Instant>,
    secrets_revision: u64,
    mail_revision: u64,
    embedded: bool,
}

impl LoginWindow {
    fn unconnected() -> Self {
        let (_, updates) = mpsc::channel();
        Self {
            runtime: Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("Cannot start the local connection worker"),
            ),
            commands: None,
            updates,
            username: String::new(),
            password: Zeroizing::new(String::new()),
            code: Zeroizing::new(String::new()),
            mailbox: Zeroizing::new(String::new()),
            pin: Zeroizing::new(String::new()),
            step: Step::Password,
            message: "Connecting to Proton Mail Bridge…".into(),
            accounts: Vec::new(),
            mail_settings: None,
            settings_retry_id: None,
            auto_start: cfg!(feature = "bundled-bridge"),
            autostart: false,
            ready: false,
            busy: false,
            closing: false,
            close_finished: false,
            waiting_for_initialization: false,
            start_pending: None,
            preparing: None,
            connecting_since: None,
            secrets_revision: 0,
            mail_revision: 0,
            embedded: false,
        }
    }

    fn connect(&mut self) {
        if crate::engine::is_initializing() {
            self.waiting_for_initialization = true;
            self.message =
                "Bridge is preparing saved accounts. You can connect when it is ready.".into();
            return;
        }
        if !crate::engine::is_running() {
            self.message =
                "Bridge is not running inside Spotty. Start the service, then reconnect.".into();
            return;
        }
        if self.commands.is_some() {
            return;
        }
        self.start_pending = None;
        let path = match rpc::config_path() {
            Ok(path) => path,
            Err(message) => {
                self.message = message;
                return;
            }
        };
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let (updates, update_receiver) = mpsc::channel();
        self.updates = update_receiver;
        self.commands = Some(commands);
        self.message = "Connecting to Proton Mail Bridge…".into();
        self.ready = false;
        self.busy = true;
        self.connecting_since = Some(std::time::Instant::now());
        self.runtime
            .as_ref()
            .expect("Bridge model runtime is already shutting down")
            .spawn(async move {
                session::run(path, receiver, |update| {
                    let _ = updates.send(update);
                })
                .await;
            });
    }

    fn clear_secrets(&mut self) {
        self.secrets_revision += 1;
        self.password.zeroize();
        self.code.zeroize();
        self.mailbox.zeroize();
        self.pin.zeroize();
    }

    fn send(&mut self, command: Command) {
        if self
            .commands
            .as_ref()
            .is_none_or(|sender| sender.send(command).is_err())
        {
            self.ready = false;
            self.message = "Bridge is disconnected. Reconnect to continue.".into();
        }
    }

    fn submit(&mut self, method: LoginMethod) {
        let secret = Zeroizing::new(match method {
            LoginMethod::Password => std::mem::take(&mut *self.password),
            LoginMethod::TwoFactor => std::mem::take(&mut *self.code),
            LoginMethod::MailboxPassword => std::mem::take(&mut *self.mailbox),
            LoginMethod::SecurityKey => std::mem::take(&mut *self.pin),
        });
        if self.username.trim().is_empty()
            || (secret.is_empty() && method != LoginMethod::SecurityKey)
        {
            self.message = "Enter the required sign-in details.".into();
            return;
        }
        self.mail_settings = None;
        self.settings_retry_id = None;
        self.busy = true;
        self.message = "Waiting for Proton Mail Bridge…".into();
        self.send(Command::Login {
            method,
            username: self.username.trim().to_owned(),
            secret,
        });
    }

    fn logout(&mut self, account_id: String) {
        self.mail_settings = None;
        self.settings_retry_id = None;
        self.mail_revision += 1;
        self.clear_secrets();
        self.busy = true;
        self.message = "Signing out and disconnecting mail clients…".into();
        self.send(Command::Logout(account_id));
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(update) = self.updates.try_recv() {
            changed = true;
            // A close request wins over any queued account or credential
            // response. In particular, never restore a generated password
            // while an embedded page is being detached.
            if self.closing && !matches!(&update, Update::Closed) {
                continue;
            }
            match update {
                Update::ConnectionEstablished => {
                    self.auto_start = false;
                }
                Update::Ready {
                    accounts,
                    autostart,
                } => {
                    self.accounts = accounts;
                    self.auto_start = false;
                    self.start_pending = None;
                    self.connecting_since = None;
                    self.autostart = autostart;
                    self.ready = true;
                    self.busy = false;
                    self.step = Step::Password;
                    self.message = "Bridge is ready. Sign in below or add another account.".into();
                }
                Update::Step(step) => {
                    self.step = step;
                    self.busy = false;
                    self.clear_secrets();
                    self.message = match step {
                        Step::TwoFactor | Step::FactorChoice => {
                            "Verify with your authenticator code or security key."
                        }
                        Step::MailboxPassword => {
                            "Enter your separate mailbox password to unlock your mail."
                        }
                        Step::SecurityKey | Step::KeyPin => {
                            "Connect your security key and continue."
                        }
                        Step::TouchKey => "Touch your security key to continue.",
                        Step::Finished => {
                            "Signed in. You can close this window; Bridge will keep mail connected."
                        }
                        _ => "Enter your Proton Mail username and password.",
                    }
                    .into();
                }
                Update::Error { step, message } => {
                    self.step = step;
                    self.message = message;
                    self.mail_settings = None;
                    self.settings_retry_id = None;
                    self.busy = false;
                    self.connecting_since = None;
                    self.clear_secrets();
                }
                Update::MailSettings(settings) => {
                    self.mail_revision += 1;
                    self.username.clone_from(&settings.username);
                    if let Some(account) = self
                        .accounts
                        .iter_mut()
                        .find(|(id, _, _)| *id == settings.id)
                    {
                        account.2 = 2;
                    } else {
                        self.accounts
                            .push((settings.id.clone(), settings.username.clone(), 2));
                    }
                    self.mail_settings = Some(settings);
                    self.settings_retry_id = None;
                    self.step = Step::Finished;
                    self.busy = false;
                    self.clear_secrets();
                    self.message = "Connected to Proton Mail.".into();
                }
                Update::AccountSettingsFailed {
                    id,
                    accounts,
                    message,
                } => {
                    // Keep the Bridge session and connected account available
                    // so the account popup can retry its generated credentials.
                    if let Some(accounts) = accounts {
                        self.accounts = accounts;
                    }
                    self.step = if self
                        .accounts
                        .iter()
                        .any(|(account_id, _, state)| account_id == &id && *state == 2)
                    {
                        Step::Finished
                    } else {
                        Step::Password
                    };
                    self.settings_retry_id = (self.step == Step::Finished).then_some(id);
                    self.mail_settings = None;
                    self.busy = false;
                    self.clear_secrets();
                    self.message = message;
                }
                Update::LoggedOut { accounts } => {
                    self.mail_revision += 1;
                    self.mail_settings = None;
                    self.settings_retry_id = None;
                    self.accounts = accounts;
                    self.step = Step::Password;
                    self.busy = false;
                    self.clear_secrets();
                    self.message = "Signed out of Proton Mail Bridge.".into();
                }
                Update::Autostart(value) => {
                    self.autostart = value;
                    self.busy = false;
                    self.message = if value {
                        "Bridge will open at desktop login."
                    } else {
                        "Bridge startup at desktop login is disabled."
                    }
                    .into();
                }
                Update::AutostartFailed(message) => {
                    // Keep account settings and the login step intact. Rendering
                    // the unchanged autostart value also restores the switch.
                    self.busy = false;
                    self.message = message;
                }
                Update::Closed => {
                    self.mail_settings = None;
                    self.settings_retry_id = None;
                    self.commands = None;
                    self.ready = false;
                    self.busy = false;
                    self.connecting_since = None;
                    self.clear_secrets();
                    if self.closing {
                        self.close_finished = true;
                    }
                }
            }
        }
        changed
    }

    fn begin_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.start_pending = None;
        self.connecting_since = None;
        self.mail_settings = None;
        self.settings_retry_id = None;
        self.clear_secrets();
        if self.commands.is_none() {
            self.close_finished = true;
        } else {
            self.message = "Disconnecting the login window while keeping Bridge running…".into();
            if self
                .commands
                .as_ref()
                .is_none_or(|sender| sender.send(Command::Close).is_err())
            {
                // A dead worker has already detached its stream. Do not keep
                // the embedded controller alive waiting for an update it
                // cannot deliver.
                self.commands = None;
                self.close_finished = true;
            }
        }
    }

    fn detach_embedded(&mut self) {
        self.username.zeroize();
        self.accounts.clear();
        self.mail_settings = None;
        self.settings_retry_id = None;
        self.mail_revision += 1;
        self.waiting_for_initialization = false;
        self.begin_close();
    }

    fn start_bridge(&mut self) {
        if self.preparing.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.preparing = Some(receiver);
        self.busy = true;
        self.message = "Starting the in-process Bridge service…".into();
        self.runtime
            .as_ref()
            .expect("Bridge model runtime is already shutting down")
            .spawn_blocking(move || {
                let result = crate::engine::start();
                let _ = sender.send(result);
            });
    }

    // Only the worker uses Tokio. GTK polls its messages on the main thread;
    // the widget tree is retained, so edits and focus survive worker updates.
    fn tick(&mut self) -> bool {
        let mut changed = self.poll();
        if self.waiting_for_initialization && !crate::engine::is_initializing() {
            self.waiting_for_initialization = false;
            self.connect();
            changed = true;
        }
        if self
            .connecting_since
            .is_some_and(|started| started.elapsed() > Duration::from_secs(40))
        {
            self.connecting_since = None;
            // Ask the session to detach gracefully. Aborting its task can
            // make Bridge interpret the dropped stream as a request to quit.
            self.busy = true;
            self.message =
                "Bridge is taking longer than expected to connect. Closing its login stream…"
                    .into();
            self.send(Command::Close);
            changed = true;
        }
        if let Some(result) = self
            .preparing
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            self.preparing = None;
            self.busy = false;
            changed = true;
            match result {
                Ok(()) if !self.closing => {
                    self.start_pending = Some(std::time::Instant::now());
                    self.message =
                        "Starting Bridge. Waiting for its keyring and local connection…".into();
                }
                Err(message) if !self.closing => self.message = message,
                _ => {}
            }
        }
        if let Some(started) = self.start_pending.filter(|_| !self.closing) {
            if started.elapsed() > Duration::from_secs(45) {
                self.start_pending = None;
                self.message = "Bridge has not become ready. Check that your Linux keyring is unlocked, then retry.".into();
                changed = true;
            } else if self.commands.is_none()
                && rpc::config_path().is_ok_and(|path| path.exists())
                && started.elapsed() > Duration::from_secs(1)
            {
                self.connect();
                changed = true;
            }
        }
        if self.auto_start
            && self.commands.is_none()
            && self.start_pending.is_none()
            && self.preparing.is_none()
            && !self.closing
        {
            self.auto_start = false;
            self.start_bridge();
            changed = true;
        }
        changed
    }
}

impl Drop for LoginWindow {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Bundled runtime extraction runs on Tokio's blocking pool. Its
            // cleanup must not stall GTK while this model is dropped.
            runtime.shutdown_background();
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|argument| argument == "--help") {
        println!(
            "Spotty Proton Mail Bridge login window\nLaunch without arguments. Sign-in details are entered only in the GUI.\nIncludes Proton Bridge on Linux x86_64 with bundled-bridge enabled. Requires a working Linux keyring."
        );
        return Ok(());
    }
    window::run()
}

/// Build the Bridge controls inside a host widget. The popup owns the same
/// login/session model as the standalone helper UI and calls `on_back` when
/// its navigation action is selected.
pub struct EmbeddedBridge(window::EmbeddedBridge);

impl EmbeddedBridge {
    /// Build Bridge controls under a GTK widget. Keep this handle for the
    /// popup's lifetime and close it when the popup is dismissed.
    pub fn new(parent: &impl gtk::prelude::IsA<gtk::Widget>, on_back: impl Fn() + 'static) -> Self {
        Self(window::EmbeddedBridge::new(parent, on_back))
    }

    /// Return the widget to place in the host popup.
    pub fn widget(&self) -> gtk::Widget {
        self.0.widget()
    }

    /// Detach the login RPC stream and release the embedded UI after Bridge
    /// acknowledges `Close`. The Bridge service itself remains running.
    pub fn close(self) {
        self.0.close();
    }

    /// Detach the login stream and invoke `on_closed` after Bridge confirms
    /// that the previous frontend has released its event stream.
    pub fn close_with_completion(self, on_closed: impl FnOnce() + 'static) {
        self.0.close_with_completion(on_closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_clears_gui_buffer_and_sends_only_to_local_worker() {
        let mut window = LoginWindow::unconnected();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        window.commands = Some(sender);
        window.username = "test@proton.me".into();
        window.password = Zeroizing::new("test-password".into());
        window.submit(LoginMethod::Password);
        assert!(window.password.is_empty());
        assert!(
            matches!(receiver.try_recv().unwrap(), Command::Login { method: LoginMethod::Password, secret, .. } if *secret == "test-password")
        );
        window.code = Zeroizing::new("012345".into());
        window.begin_close();
        assert!(window.code.is_empty());
        assert!(matches!(receiver.try_recv().unwrap(), Command::Close));
    }

    #[test]
    fn connection_watchdog_requests_graceful_close_instead_of_aborting_stream() {
        let mut window = LoginWindow::unconnected();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        window.commands = Some(sender);
        window.connecting_since = Some(std::time::Instant::now() - Duration::from_secs(41));
        window.busy = true;
        window.tick();
        assert!(window.busy);
        assert!(window.commands.is_some());
        assert!(window.message.contains("Closing its login stream"));
        assert!(matches!(receiver.try_recv(), Ok(Command::Close)));
    }

    #[test]
    fn ordinary_connection_error_returns_to_retryable_password_step() {
        let mut window = LoginWindow::unconnected();
        let (updates, receiver) = mpsc::channel();
        window.updates = receiver;
        updates
            .send(Update::Error {
                step: Step::Password,
                message: "Bridge is not running. Start Bridge, then reconnect.".into(),
            })
            .unwrap();
        updates.send(Update::Closed).unwrap();
        window.poll();
        assert_eq!(window.step, Step::Password);
        assert!(!window.busy);
        assert!(window.commands.is_none());
    }

    #[test]
    fn logout_clears_generated_and_login_secrets_before_sending_command() {
        let mut window = LoginWindow::unconnected();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        window.commands = Some(sender);
        let (updates, update_receiver) = mpsc::channel();
        window.updates = update_receiver;
        window.ready = true;
        window.password = Zeroizing::new("account-secret".into());
        window.code = Zeroizing::new("verification-secret".into());
        window.mailbox = Zeroizing::new("mailbox-secret".into());
        window.pin = Zeroizing::new("pin-secret".into());
        window.mail_settings = Some(session::MailSettings {
            id: "account-id".into(),
            username: "test@proton.me".into(),
            addresses: vec!["test@proton.me".into()],
            password: Zeroizing::new("generated-secret".into()),
            hostname: "127.0.0.1".into(),
            imap_port: 1143,
            smtp_port: 1025,
            imap_ssl: false,
            smtp_ssl: true,
        });
        window.logout("account-id".into());
        assert!(window.password.is_empty());
        assert!(window.code.is_empty());
        assert!(window.mailbox.is_empty());
        assert!(window.pin.is_empty());
        assert!(window.mail_settings.is_none());
        assert!(window.busy);
        assert!(matches!(receiver.try_recv().unwrap(), Command::Logout(id) if id == "account-id"));
        updates
            .send(Update::LoggedOut {
                accounts: vec![("account-id".into(), "test@proton.me".into(), 0)],
            })
            .unwrap();
        window.poll();
        assert!(!window.busy);
        assert_eq!(window.accounts[0].2, 0);
        assert_eq!(window.step, Step::Password);
    }

    #[test]
    fn embedded_detach_clears_account_state_and_waits_for_rpc_close() {
        let mut window = LoginWindow::unconnected();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        window.commands = Some(sender);
        window.username = "test@proton.me".into();
        window.accounts = vec![("account-id".into(), "test@proton.me".into(), 2)];
        window.password = Zeroizing::new("account-secret".into());
        window.mail_settings = Some(session::MailSettings {
            id: "account-id".into(),
            username: "test@proton.me".into(),
            addresses: vec!["test@proton.me".into()],
            password: Zeroizing::new("generated-secret".into()),
            hostname: "127.0.0.1".into(),
            imap_port: 1143,
            smtp_port: 1025,
            imap_ssl: false,
            smtp_ssl: true,
        });

        window.detach_embedded();

        assert!(window.username.is_empty());
        assert!(window.accounts.is_empty());
        assert!(window.password.is_empty());
        assert!(window.mail_settings.is_none());
        assert!(!window.close_finished);
        assert!(matches!(receiver.try_recv(), Ok(Command::Close)));

        let (updates, update_receiver) = mpsc::channel();
        window.updates = update_receiver;
        updates.send(Update::Closed).unwrap();
        window.poll();
        assert!(window.close_finished);
        assert!(window.commands.is_none());
    }

    #[test]
    fn embedded_detach_finishes_if_the_rpc_worker_is_already_gone() {
        let mut window = LoginWindow::unconnected();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        window.commands = Some(sender);
        drop(receiver);

        window.detach_embedded();

        assert!(window.close_finished);
        assert!(window.commands.is_none());
    }
}
