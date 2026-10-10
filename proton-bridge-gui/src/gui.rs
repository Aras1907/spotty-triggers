use crate::rpc::{self, LoginMethod};
use crate::session::{self, Command, Step, Update};
mod window;
use spotty_proton_login as login;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

/// How long the shared sign-in page waits for Bridge to answer one step.
const LOGIN_WAIT: Duration = Duration::from_secs(120);

/// What Bridge's login stream reported for the step the page is on.
enum LoginAnswer {
    Step(Step),
    Failed { step: Step, message: String },
}

/// The link between the shared sign-in page and Bridge's login stream. The
/// window reads the stream on the GTK thread (`LoginWindow::poll`) and passes
/// each login answer here; the page's backend calls (on a worker thread) send
/// their command and wait for the answer.
pub(crate) struct BridgeLink {
    commands: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Command>>>,
    /// The account the pending login is for. Bridge's cancel needs it.
    account: Mutex<String>,
    answers_tx: mpsc::Sender<LoginAnswer>,
    answers_rx: Mutex<mpsc::Receiver<LoginAnswer>>,
}

impl BridgeLink {
    fn new() -> Arc<Self> {
        let (answers_tx, answers_rx) = mpsc::channel();
        Arc::new(Self {
            commands: Mutex::new(None),
            account: Mutex::new(String::new()),
            answers_tx,
            answers_rx: Mutex::new(answers_rx),
        })
    }

    fn set_commands(&self, commands: Option<tokio::sync::mpsc::UnboundedSender<Command>>) {
        *self.commands.lock().unwrap_or_else(|p| p.into_inner()) = commands;
    }

    fn answer(&self, answer: LoginAnswer) {
        let _ = self.answers_tx.send(answer);
    }

    /// Send one login step to Bridge and wait for its answer.
    fn attempt(&self, method: LoginMethod, account: String, secret: Zeroizing<String>) -> login::Step {
        // Answers left over from an earlier step must not answer this one.
        {
            let answers = self.answers_rx.lock().unwrap_or_else(|p| p.into_inner());
            while answers.try_recv().is_ok() {}
        }
        let sent = self
            .commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|sender| sender.send(Command::Login { method, username: account, secret }).is_ok());
        if !sent {
            return login::Step::Failed("Bridge is disconnected. Reconnect to continue.".into());
        }
        self.wait()
    }

    fn wait(&self) -> login::Step {
        let answers = self.answers_rx.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            match answers.recv_timeout(LOGIN_WAIT) {
                // Cancelling a login reports the password step again; that is not an answer.
                Ok(LoginAnswer::Step(Step::Password)) => continue,
                Ok(LoginAnswer::Step(step)) => return map_step(step, None),
                Ok(LoginAnswer::Failed { step, message }) => return map_step(step, Some(message)),
                Err(_) => return login::Step::Failed("Bridge didn't answer. Try again.".into()),
            }
        }
    }

    fn account(&self) -> String {
        self.account.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// What a Bridge login step means on the shared sign-in page. An error message
/// keeps the page on its step.
fn map_step(step: Step, error: Option<String>) -> login::Step {
    if let Some(message) = error {
        return login::Step::Failed(message);
    }
    match step {
        Step::Finished => login::Step::Done,
        Step::TwoFactor | Step::FactorChoice => login::Step::TwoFactor,
        Step::MailboxPassword => login::Step::SecondPassword {
            title: "Mailbox password".into(),
            label: "Mailbox password".into(),
        },
        Step::SecurityKey | Step::KeyPin | Step::TouchKey => login::Step::Failed(
            "This account needs a security key. Spotty can't use one to sign in yet; use an authenticator code.".into(),
        ),
        Step::Recovery => login::Step::Failed(
            "Proton requires human verification. Follow the recovery steps for your Proton account, then retry sign-in.".into(),
        ),
        Step::Password => login::Step::Failed("Enter your Proton Mail username and password.".into()),
    }
}

/// Bridge's own sign-in, as the shared sign-in page calls it.
pub(crate) struct BridgeLogin(Arc<BridgeLink>);

impl login::Backend for BridgeLogin {
    fn begin(&self, email: &str, password: Zeroizing<String>) -> login::Step {
        let account = email.trim().to_owned();
        *self.0.account.lock().unwrap_or_else(|p| p.into_inner()) = account.clone();
        self.0.attempt(LoginMethod::Password, account, password)
    }

    fn submit_code(&self, code: Zeroizing<String>) -> login::Step {
        self.0.attempt(LoginMethod::TwoFactor, self.0.account(), code)
    }

    fn submit_second(&self, password: Zeroizing<String>) -> login::Step {
        self.0.attempt(LoginMethod::MailboxPassword, self.0.account(), password)
    }

    fn cancel(&self) {
        let account = std::mem::take(&mut *self.0.account.lock().unwrap_or_else(|p| p.into_inner()));
        if let Some(sender) = self.0.commands.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            let _ = sender.send(Command::Cancel(account));
        }
    }
}

struct LoginWindow {
    runtime: Option<tokio::runtime::Runtime>,
    commands: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    updates: mpsc::Receiver<Update>,
    username: String,
    /// The link to the shared sign-in page (see `BridgeLogin`).
    link: Arc<BridgeLink>,
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
            link: BridgeLink::new(),
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
        self.link.set_commands(Some(commands.clone()));
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

    fn logout(&mut self, account_id: String) {
        self.mail_settings = None;
        self.settings_retry_id = None;
        self.mail_revision += 1;
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
                    self.link.answer(LoginAnswer::Step(step));
                    self.busy = false;
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
                    self.link.answer(LoginAnswer::Failed { step, message: message.clone() });
                    self.message = message;
                    self.mail_settings = None;
                    self.settings_retry_id = None;
                    self.busy = false;
                    self.connecting_since = None;
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
                    self.message = message;
                }
                Update::LoggedOut { accounts } => {
                    self.mail_revision += 1;
                    self.mail_settings = None;
                    self.settings_retry_id = None;
                    self.accounts = accounts;
                    self.step = Step::Password;
                    self.busy = false;
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
    use login::Backend;

    #[test]
    fn a_login_step_is_sent_to_bridge_with_its_secret_and_answered_by_the_stream() {
        let link = BridgeLink::new();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        link.set_commands(Some(sender));
        let worker = link.clone();
        let answer = std::thread::spawn(move || {
            BridgeLogin(worker).begin("test@proton.me", Zeroizing::new("test-password".into()))
        });
        let command = receiver.blocking_recv().expect("the login is sent");
        assert!(matches!(&command, Command::Login { method: LoginMethod::Password, username, secret } if username == "test@proton.me" && **secret == "test-password"));
        link.answer(LoginAnswer::Step(Step::TwoFactor));
        assert_eq!(answer.join().unwrap(), login::Step::TwoFactor);
    }

    #[test]
    fn bridge_steps_map_onto_the_shared_page() {
        assert_eq!(map_step(Step::Finished, None), login::Step::Done);
        assert_eq!(map_step(Step::TwoFactor, None), login::Step::TwoFactor);
        assert_eq!(map_step(Step::FactorChoice, None), login::Step::TwoFactor);
        assert!(matches!(map_step(Step::MailboxPassword, None), login::Step::SecondPassword { .. }));
        assert!(matches!(map_step(Step::KeyPin, None), login::Step::Failed(_)));
        assert!(matches!(map_step(Step::TouchKey, None), login::Step::Failed(_)));
        assert_eq!(map_step(Step::TwoFactor, Some("The two-factor code was rejected.".into())), login::Step::Failed("The two-factor code was rejected.".into()));
    }

    #[test]
    fn a_stale_or_cancel_answer_does_not_end_a_login() {
        let link = BridgeLink::new();
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        link.set_commands(Some(sender));
        link.answer(LoginAnswer::Step(Step::Password));
        link.answer(LoginAnswer::Step(Step::Finished));
        assert_eq!(link.wait(), login::Step::Done);
    }

    #[test]
    fn submission_is_refused_when_bridge_is_not_connected() {
        let link = BridgeLink::new();
        assert!(matches!(
            BridgeLogin(link).begin("test@proton.me", Zeroizing::new("x".into())),
            login::Step::Failed(message) if message.contains("disconnected")
        ));
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
    fn logout_clears_generated_secrets_before_sending_command() {
        let mut window = LoginWindow::unconnected();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        window.commands = Some(sender);
        let (updates, update_receiver) = mpsc::channel();
        window.updates = update_receiver;
        window.ready = true;
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
