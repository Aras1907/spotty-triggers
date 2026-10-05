use crate::rpc::{self, LoginMethod};
use crate::session::{self, Command, Step, Update};
use eframe::egui;
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
    reveal_password: bool,
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
}

impl LoginWindow {
    fn new(ctx: &egui::Context) -> Self {
        let mut window = Self::unconnected();
        if std::env::args().any(|argument| argument == "--no-auto-start") {
            window.auto_start = false;
        }
        window.connect(ctx);
        window
    }

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
            reveal_password: true,
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
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
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
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            session::run(path, receiver, |update| {
                let _ = updates.send(update);
                ctx.request_repaint();
            })
            .await;
        });
    }

    fn clear_secrets(&mut self) {
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

    fn poll(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
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
                    self.reveal_password = true;
                    self.step = Step::Finished;
                    self.busy = false;
                    self.clear_secrets();
                    self.message =
                        "Signed in. Copy the Bridge password and settings into your mail client."
                            .into();
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
}

fn secret_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .password(true)
            .desired_width(f32::INFINITY),
    );
}

impl LoginWindow {
    fn draw(&mut self, ctx: &egui::Context) {
        self.poll();
        if let Some(preparing) = &self.preparing {
            ctx.request_repaint_after(Duration::from_millis(200));
            if let Ok(result) = preparing.try_recv() {
                self.preparing = None;
                self.busy = false;
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
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.begin_close();
            if !self.close_finished || self.open_official || self.official_started.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if self.closing && self.close_finished {
            if self.open_official {
                self.open_official = false;
                let backend = self.official_wait.take();
                let (started, receiver) = mpsc::channel();
                self.official_started = Some(receiver);
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let launcher = crate::launch::official_launcher()
                        .unwrap_or_else(|_| "protonmail-bridge".into());
                    let mut command = Process::new(&launcher);
                    crate::bundle::configure_libraries(&launcher, &mut command);
                    // Proton's launcher waits for this backend to release its
                    // lock before starting the GUI. Do not use a timed delay.
                    if let Some(backend) = backend {
                        command.arg("--wait").arg(backend);
                    }
                    let child = command
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                    let _ = started.send(child.is_ok());
                    ctx.request_repaint();
                    if let Ok(mut child) = child {
                        let _ = child.wait();
                    }
                });
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
                    self.message = "Cannot start the official Bridge GUI. Check that protonmail-bridge is on PATH.".into();
                }
            }
            if self.closing && self.official_started.is_none() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        if let Some(started) = self.start_pending.filter(|_| !self.closing) {
            ctx.request_repaint_after(Duration::from_millis(200));
            if rpc::config_path().is_ok_and(|path| path.exists())
                && started.elapsed() > Duration::from_secs(1)
            {
                self.connect(ctx);
            } else if started.elapsed() > Duration::from_secs(45) {
                self.start_pending = None;
                self.message = "Bridge has not become ready. Open the official Bridge window to check its keyring, then reconnect.".into();
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
        }
        egui::CentralPanel::default().show(ctx, |ui| {
          egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
            ui.heading("Proton Mail Bridge");
            ui.label("Connect your Proton account to the mail bridge.");
            ui.separator();
            ui.label(&self.message);
            if self.busy || self.closing || self.start_pending.is_some() { ui.spinner(); }
            if !self.accounts.is_empty() {
                ui.collapsing("Saved accounts", |ui| {
                    let mut selected = None;
                    for (id, name, state) in &self.accounts {
                        let status = match state { 2 => "Connected", 1 => "Locked", _ => "Signed out" };
                        ui.horizontal(|ui| {
                            ui.label(format!("{name} · {status}"));
                            if *state == 2 && ui.add_enabled(self.ready && !self.busy, egui::Button::new("Mail settings")).clicked() {
                                selected = Some(id.clone());
                            }
                        });
                    }
                    if let Some(id) = selected {
                        self.mail_settings = None;
                        self.busy = true;
                        self.send(Command::ShowAccount(id));
                    }
                });
            }
            if let Some(settings) = &self.mail_settings {
                ui.separator();
                ui.heading("Mail-client settings");
                ui.label(format!("Account: {}", settings.username));
                for address in &settings.addresses {
                    ui.horizontal(|ui| {
                        ui.label(format!("Username: {address}"));
                        if ui.button("Copy username").clicked() { ui.ctx().copy_text(address.clone()); }
                    });
                }
                ui.label("Bridge password");
                ui.horizontal(|ui| {
                    let mut password = settings.password.as_str();
                    ui.add(egui::TextEdit::singleline(&mut password).password(!self.reveal_password));
                    if ui.button("Copy password").clicked() { ui.ctx().copy_text(settings.password.to_string()); }
                });
                ui.checkbox(&mut self.reveal_password, "Show Bridge password");
                ui.small("Use this generated password in your mail client. Your Proton account password stays private.");
                for (label, port, ssl) in [("IMAP", settings.imap_port, settings.imap_ssl), ("SMTP", settings.smtp_port, settings.smtp_ssl)] {
                    ui.label(format!("{label}: {}:{port} · {}", settings.hostname, if ssl { "SSL/TLS" } else { "STARTTLS" }));
                }
            }
            ui.add_enabled_ui(self.ready && !self.busy && !self.closing, |ui| {
                match self.step {
                    Step::Password => {
                        ui.label("Proton email or username");
                        ui.add(egui::TextEdit::singleline(&mut self.username).desired_width(f32::INFINITY));
                        secret_field(ui, "Account password", &mut self.password);
                        if ui.button("Sign in").clicked() { self.submit(LoginMethod::Password); }
                    }
                    Step::TwoFactor | Step::FactorChoice => {
                        secret_field(ui, "Two-factor authentication code", &mut self.code);
                        if ui.button("Verify code").clicked() { self.submit(LoginMethod::TwoFactor); }
                        if self.step == Step::FactorChoice && ui.button("Use security key").clicked() {
                            self.submit(LoginMethod::SecurityKey);
                        }
                    }
                    Step::MailboxPassword => {
                        secret_field(ui, "Mailbox password", &mut self.mailbox);
                        if ui.button("Unlock mailbox").clicked() { self.submit(LoginMethod::MailboxPassword); }
                    }
                    Step::SecurityKey => {
                        if ui.button("Authenticate with security key").clicked() { self.submit(LoginMethod::SecurityKey); }
                    }
                    Step::KeyPin => {
                        secret_field(ui, "Security key PIN", &mut self.pin);
                        if ui.button("Verify security key").clicked() { self.submit(LoginMethod::SecurityKey); }
                    }
                    Step::TouchKey => { ui.label("Follow the prompts on your security key."); }
                    Step::Finished => {
                        if ui.button("Add another account").clicked() {
                            self.username.clear(); self.mail_settings = None; self.step = Step::Password;
                        }
                    }
                    Step::OfficialGui => {}
                }
                ui.separator();
                let mut startup = self.autostart;
                if ui.checkbox(&mut startup, "Open Bridge at desktop login").changed() {
                    self.busy = true; self.send(Command::Autostart(startup));
                }
            });
            ui.separator();
            if self.ready && !self.closing && self.step != Step::Finished && self.step != Step::OfficialGui
                && (self.busy || self.step != Step::Password) && ui.button("Cancel sign-in").clicked() {
                self.clear_secrets(); self.busy = true;
                self.send(Command::Cancel(self.username.clone()));
            }
            ui.add_enabled_ui(!self.closing && self.start_pending.is_none() && self.preparing.is_none(), |ui| {
                if !self.ready && self.commands.is_none() {
                    ui.horizontal(|ui| {
                        if ui.button("Start Bridge").clicked() { self.start_bridge(); }
                        if ui.button("Reconnect").clicked() { self.connect(ctx); }
                    });
                }
                if ui.button("Open official Bridge window").clicked() {
                    // Detach gracefully before allowing the official frontend
                    // to take ownership. A second frontend cannot share login.
                    self.open_official = true;
                    self.official_wait = if self.ready {
                        crate::launch::backend_launch().ok().map(|plan| plan.executable)
                    } else { None };
                    self.closing = true;
                    self.mail_settings = None;
                    self.clear_secrets();
                    self.message = "Opening the official Bridge window. Mail may reconnect briefly.".into();
                    if self.commands.is_none() { self.close_finished = true; }
                    else { self.send(Command::OpenOfficial); }
                }
                if ui.button("Installation guide").clicked() {
                    let _ = Process::new("xdg-open").arg("https://proton.me/support/protonmail-bridge-install")
                        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
                }
                if ui.button("Close and keep Bridge running").clicked() { self.begin_close(); }
            });
            ui.separator();
            ui.small("Credentials are sent only to your local Bridge over verified TLS. Bridge manages the saved login in its keyring. This window does not save passwords.");
            ui.small("A paid Proton Mail plan and an unlocked Linux keyring are required. Bridge keeps running when you close this window.");
          });
        });
    }
}

impl eframe::App for LoginWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

pub fn run() -> eframe::Result {
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
    eframe::run_native(
        "Spotty · Proton Mail Bridge",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([500.0, 630.0])
                .with_min_inner_size([440.0, 500.0]),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(LoginWindow::new(&cc.egui_ctx)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_text(shape: &egui::Shape, text: &mut String) {
        match shape {
            egui::Shape::Text(shape) => text.push_str(&shape.galley.job.text),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    shape_text(shape, text);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn gui_renders_required_fields_without_exposing_secrets() {
        let mut window = LoginWindow::unconnected();
        window.auto_start = false;
        window.ready = true;
        window.username = "test@proton.me".into();
        let ctx = egui::Context::default();
        for (step, label) in [
            (Step::Password, "Account password"),
            (Step::TwoFactor, "Two-factor authentication code"),
            (Step::MailboxPassword, "Mailbox password"),
            (Step::KeyPin, "Security key PIN"),
        ] {
            window.step = step;
            window.password = Zeroizing::new("secret-sentinel".into());
            window.code = Zeroizing::new("secret-sentinel".into());
            window.mailbox = Zeroizing::new("secret-sentinel".into());
            window.pin = Zeroizing::new("secret-sentinel".into());
            // The same form-rendering function used by the native window.
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(500.0, 630.0),
                    )),
                    ..Default::default()
                },
                |ctx| window.draw(ctx),
            );
            let mut text = String::new();
            for shape in output.shapes {
                shape_text(&shape.shape, &mut text);
            }
            assert!(text.contains(label), "Missing {label}");
            assert!(
                !text.contains("secret-sentinel"),
                "Unmasked secret in {label}"
            );
            assert!(text.contains("Open Bridge at desktop login"));
        }
    }

    #[test]
    fn signed_in_gui_shows_generated_credentials_and_can_hide_and_discard_them() {
        let mut window = LoginWindow::unconnected();
        window.auto_start = false;
        window.ready = true;
        window.step = Step::Finished;
        window.mail_settings = Some(session::MailSettings {
            id: "account-id".into(),
            username: "test@proton.me".into(),
            addresses: vec!["test@proton.me".into()],
            password: Zeroizing::new("GeneratedBridgeSecret".into()),
            hostname: "127.0.0.1".into(),
            imap_port: 1143,
            smtp_port: 1025,
            imap_ssl: false,
            smtp_ssl: true,
        });
        let ctx = egui::Context::default();
        for reveal in [true, false] {
            window.reveal_password = reveal;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| window.draw(ctx),
            );
            let mut text = String::new();
            for shape in output.shapes {
                shape_text(&shape.shape, &mut text);
            }
            assert_eq!(text.contains("GeneratedBridgeSecret"), reveal);
            assert!(text.contains("127.0.0.1:1143"));
            assert!(text.contains("127.0.0.1:1025"));
            assert!(text.contains("STARTTLS"));
            assert!(text.contains("SSL/TLS"));
            assert!(text.contains("Copy password"));
        }
        window.begin_close();
        assert!(window.mail_settings.is_none());
    }

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
}
