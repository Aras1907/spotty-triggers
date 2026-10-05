use crate::rpc::{self, LoginMethod};
use crate::session::{self, Command, Step, Update};
mod window;
use std::os::unix::process::CommandExt;
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

struct LoginWindow {
    runtime: tokio::runtime::Runtime,
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
    auto_start: bool,
    autostart: bool,
    ready: bool,
    busy: bool,
    closing: bool,
    close_finished: bool,
    open_official: bool,
    official_wait: Option<std::path::PathBuf>,
    official_started: Option<mpsc::Receiver<bool>>,
    start_pending: Option<std::time::Instant>,
    preparing: Option<mpsc::Receiver<Result<(), String>>>,
    secrets_revision: u64,
    mail_revision: u64,
}

impl LoginWindow {
    fn unconnected() -> Self {
        let (_, updates) = mpsc::channel();
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("Cannot start the local connection worker"),
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
            auto_start: cfg!(feature = "bundled-bridge"),
            autostart: false,
            ready: false,
            busy: false,
            closing: false,
            close_finished: false,
            open_official: false,
            official_wait: None,
            official_started: None,
            start_pending: None,
            preparing: None,
            secrets_revision: 0,
            mail_revision: 0,
        }
    }

    fn connect(&mut self) {
        if self.commands.is_some() {
            return;
        }
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
        self.runtime.spawn(async move {
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
                    self.busy = false;
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
                    self.step = Step::Finished;
                    self.busy = false;
                    self.clear_secrets();
                    self.message = "Connected to Proton Mail.".into();
                }
                Update::LoggedOut { accounts } => {
                    self.mail_revision += 1;
                    self.mail_settings = None;
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
                Update::Closed => {
                    self.mail_settings = None;
                    self.commands = None;
                    self.ready = false;
                    self.busy = false;
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
        self.mail_settings = None;
        self.clear_secrets();
        if self.commands.is_none() {
            self.close_finished = true;
        } else {
            self.message = "Disconnecting the login window while keeping Bridge running…".into();
            self.send(Command::Close);
        }
    }

    fn start_bridge(&mut self) {
        if self.preparing.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.preparing = Some(receiver);
        self.busy = true;
        self.message = "Preparing the packaged Bridge runtime…".into();
        self.runtime.spawn_blocking(move || {
            let result = (|| -> Result<(), String> {
                let plan = crate::launch::backend_launch()?;
                // Independent lifetime; credentials never appear in argv.
                let mut process = Process::new(plan.executable);
                crate::bundle::configure_libraries(&plan.launcher, &mut process);
                process
                    .args(plan.arguments)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                unsafe {
                    process.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let mut child = process
                    .spawn()
                    .map_err(|_| "Cannot start the native Bridge runtime.".to_owned())?;
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(())
            })();
            let _ = sender.send(result);
        });
    }

    fn open_official(&mut self) {
        self.open_official = true;
        self.official_wait = if self.ready {
            crate::launch::backend_launch()
                .ok()
                .map(|plan| plan.executable)
        } else {
            None
        };
        self.closing = true;
        self.start_pending = None;
        self.mail_settings = None;
        self.clear_secrets();
        self.message = "Opening the official Bridge window. Mail may reconnect briefly.".into();
        if self.commands.is_none() {
            self.close_finished = true;
        } else {
            self.send(Command::OpenOfficial);
        }
    }

    // Only the worker uses Tokio. GTK polls its messages on the main thread;
    // the widget tree is retained, so edits and focus survive worker updates.
    fn tick(&mut self) -> bool {
        let mut changed = self.poll();
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
        if self.closing && self.close_finished && self.open_official {
            self.open_official = false;
            let backend = self.official_wait.take();
            let (started, receiver) = mpsc::channel();
            self.official_started = Some(receiver);
            std::thread::spawn(move || {
                let launcher = crate::launch::official_launcher()
                    .unwrap_or_else(|_| "protonmail-bridge".into());
                let mut command = Process::new(&launcher);
                crate::bundle::configure_libraries(&launcher, &mut command);
                if let Some(backend) = backend {
                    command.arg("--wait").arg(backend);
                }
                let child = command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                let _ = started.send(child.is_ok());
                if let Ok(mut child) = child {
                    let _ = child.wait();
                }
            });
            changed = true;
        }
        if let Some(started) = self
            .official_started
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            self.official_started = None;
            if !started {
                self.closing = false;
                self.close_finished = false;
                self.message =
                    "Cannot open the official Bridge window. Reconnect to try again.".into();
            }
            changed = true;
        }
        if let Some(started) = self.start_pending.filter(|_| !self.closing) {
            if started.elapsed() > Duration::from_secs(45) {
                self.start_pending = None;
                self.message = "Bridge has not become ready. Open the official Bridge window to check its keyring, then reconnect.".into();
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|argument| argument == "--background") {
        // No window at desktop login; release the stream for later interaction.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Cannot start the Bridge worker");
        runtime.block_on(async {
            let Ok(path) = rpc::config_path() else { return };
            if rpc::Connection::connect(&path).await.is_err() {
                let Ok(plan) = crate::launch::backend_launch() else {
                    return;
                };
                let mut process = Process::new(plan.executable);
                crate::bundle::configure_libraries(&plan.launcher, &mut process);
                process
                    .args(plan.arguments)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                unsafe {
                    process.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let Ok(mut child) = process.spawn() else {
                    return;
                };
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
            loop {
                if rpc::Connection::connect(&path).await.is_ok() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            session::initialize_and_detach(path).await;
        });
        return Ok(());
    }
    if std::env::args().any(|argument| argument == "--help") {
        println!(
            "Spotty Proton Mail Bridge login window\nLaunch without arguments. Sign-in details are entered only in the GUI.\nIncludes Proton Bridge on Linux x86_64 with bundled-bridge enabled. Requires a working Linux keyring."
        );
        return Ok(());
    }
    window::run()
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
}
