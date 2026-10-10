// The GUI and asynchronous Bridge session communicate without ever putting
// secrets in process arguments, shell commands, logging, or configuration.
use crate::protocol::{self, login_event, stream_event};
use crate::rpc::{Connection, LoginMethod};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use zeroize::{Zeroize, Zeroizing};

pub enum Command {
    Login {
        method: LoginMethod,
        username: String,
        secret: Zeroizing<String>,
    },
    Autostart(bool),
    ShowAccount(String),
    Logout(String),
    Cancel(String),
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Password,
    TwoFactor,
    MailboxPassword,
    SecurityKey,
    FactorChoice,
    KeyPin,
    TouchKey,
    Finished,
    Recovery,
}

pub struct MailSettings {
    pub id: String,
    pub username: String,
    pub addresses: Vec<String>,
    pub password: Zeroizing<String>,
    pub hostname: String,
    pub imap_port: i32,
    pub smtp_port: i32,
    pub imap_ssl: bool,
    pub smtp_ssl: bool,
}

pub enum Update {
    Ready {
        accounts: Vec<(String, String, i32)>,
        autostart: bool,
    },
    MailSettings(MailSettings),
    AccountSettingsFailed {
        id: String,
        accounts: Option<Vec<(String, String, i32)>>,
        message: String,
    },
    LoggedOut {
        accounts: Vec<(String, String, i32)>,
    },
    Step(Step),
    Error {
        step: Step,
        message: String,
    },
    Autostart(bool),
    AutostartFailed(String),
    Closed,
    ConnectionEstablished,
}

enum StreamUpdate {
    Event(protocol::StreamEvent),
    Failed(tonic::Status),
    Ended,
}

pub fn login_update(event: login_event::Event) -> Update {
    use login_event::Event::*;
    match event {
        TfaRequested(_) => Update::Step(Step::TwoFactor),
        TwoPasswordRequested(_) => Update::Step(Step::MailboxPassword),
        FidoRequested(_) => Update::Step(Step::SecurityKey),
        TfaOrFidoRequested(_) => Update::Step(Step::FactorChoice),
        LoginFidoPinRequired(_) => Update::Step(Step::KeyPin),
        LoginFidoTouchRequested(_) | LoginFidoTouchCompleted(_) => Update::Step(Step::TouchKey),
        Finished(_) | AlreadyLoggedIn(_) => Update::Step(Step::Finished),
        HvRequested(_) => Update::Error {
            step: Step::Recovery,
            message:
                "Proton requires human verification. Follow the recovery steps for your Proton account, then retry sign-in."
                    .into(),
        },
        Error(error) => {
            // Do not echo server error strings: they may contain personal data.
            let (step, message) = match error.r#type {
                1 => (
                    Step::Password,
                    "Bridge requires a paid Proton plan that includes Mail.",
                ),
                2 => (
                    Step::Password,
                    "Bridge could not connect to Proton. Check your connection and retry.",
                ),
                3 => (
                    Step::TwoFactor,
                    "The two-factor code was rejected. Try a new code.",
                ),
                5 => (
                    Step::MailboxPassword,
                    "The mailbox password was rejected. Try again.",
                ),
                8 => (
                    Step::KeyPin,
                    "The security key PIN was rejected. Try again.",
                ),
                9 => (
                    Step::Recovery,
                    "The security key PIN is blocked. Unblock the key in your system security settings, then retry.",
                ),
                7 | 10 => (
                    Step::Recovery,
                    "Complete Proton's verification steps, then retry sign-in.",
                ),
                _ => (
                    Step::Password,
                    "Sign-in failed or was cancelled. Check your username and password and retry.",
                ),
            };
            Update::Error {
                step,
                message: message.into(),
            }
        }
    }
}

fn status_message(status: &tonic::Status) -> String {
    match status.code() {
        tonic::Code::AlreadyExists => "Another app is using Bridge's login connection. Close that sign-in form, then reconnect here.",
        tonic::Code::Unauthenticated => "Bridge's connection token changed. Reconnect before signing in.",
        _ => "The local Bridge connection failed. Retry the connection.",
    }.into()
}

pub async fn run(path: PathBuf, commands: mpsc::UnboundedReceiver<Command>, send: impl Fn(Update)) {
    run_session(path, commands, send, true).await;
}

async fn run_session(
    path: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Command>,
    send: impl Fn(Update),
    show_saved_settings: bool,
) {
    let mut connection = match Connection::connect(&path).await {
        Ok(connection) => connection,
        Err(message) => {
            send(Update::Error {
                // A missing or starting local backend is a normal retry case.
                // Recovery conditions can be handled outside the login form.
                step: Step::Password,
                message,
            });
            send(Update::Closed);
            return;
        }
    };
    send(Update::ConnectionEstablished);
    // Release Bridge's initialization gate before waiting for streamed events.
    // This call is idempotent and does not take over an existing frontend.
    let request = connection.unary(protocol::Empty {});
    if let Err(status) = connection.client.gui_ready(request).await {
        send(Update::Error {
            step: Step::Password,
            message: status_message(&status),
        });
        send(Update::Closed);
        return;
    }
    let request = connection.request(protocol::StreamRequest {
        client_platform: "linux".into(),
    });
    // Bridge's Go server does not send response headers until the first event.
    // Start the stream reader independently, as Bridge's native frontend does,
    // so a quiet stream cannot block ordinary unary startup requests.
    let (stream_updates, mut stream_messages) = mpsc::unbounded_channel();
    let mut stream_client = connection.client.clone();
    let stream_task = tokio::spawn(async move {
        match stream_client.run_event_stream(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(event)) => {
                            if stream_updates.send(StreamUpdate::Event(event)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {
                            let _ = stream_updates.send(StreamUpdate::Ended);
                            return;
                        }
                        Err(status) => {
                            let _ = stream_updates.send(StreamUpdate::Failed(status));
                            return;
                        }
                    }
                }
            }
            Err(status) => {
                let _ = stream_updates.send(StreamUpdate::Failed(status));
            }
        }
    });
    // An occupied stream is normally rejected immediately, while a quiet
    // stream remains pending for headers. The protocol has no ownership ACK.
    let mut stream_owned = true;
    let mut stream_confirmed = false;
    let mut initial_events = Vec::new();
    match tokio::time::timeout(Duration::from_millis(250), stream_messages.recv()).await {
        Ok(Some(StreamUpdate::Failed(status))) => {
            send(Update::Error {
                step: Step::Password,
                message: status_message(&status),
            });
            send(Update::Closed);
            return;
        }
        Ok(Some(StreamUpdate::Event(event))) => {
            stream_confirmed = true;
            initial_events.push(event);
        }
        Ok(Some(StreamUpdate::Ended)) | Ok(None) => {
            send(Update::Error {
                step: Step::Password,
                message: "Bridge closed the login connection. Reconnect.".into(),
            });
            send(Update::Closed);
            return;
        }
        Err(_) => {}
    }
    let ready = async {
        let accounts = get_accounts(&mut connection).await?;
        let autostart = if crate::background::is_flatpak() {
            crate::background::autostart_enabled()
        } else {
            let request = connection.unary(protocol::Empty {});
            connection
                .client
                .is_autostart_on(request)
                .await?
                .into_inner()
                .value
        };
        Ok::<_, tonic::Status>((accounts, autostart))
    }
    .await;
    while let Ok(update) = stream_messages.try_recv() {
        match update {
            StreamUpdate::Event(event) => {
                stream_confirmed = true;
                initial_events.push(event);
            }
            StreamUpdate::Failed(status) => {
                send(Update::Error {
                    step: Step::Password,
                    message: status_message(&status),
                });
                send(Update::Closed);
                let _ = tokio::time::timeout(Duration::from_secs(2), stream_task).await;
                return;
            }
            StreamUpdate::Ended => {
                send(Update::Error {
                    step: Step::Password,
                    message: "Bridge closed the login connection. Reconnect.".into(),
                });
                send(Update::Closed);
                let _ = tokio::time::timeout(Duration::from_secs(2), stream_task).await;
                return;
            }
        }
    }
    match ready {
        Ok((accounts, autostart)) => {
            for event in initial_events {
                dispatch(&mut connection, event, &send).await;
            }
            let first_connected = accounts
                .iter()
                .find(|(_, _, state)| *state == 2)
                .map(|(id, _, _)| id.clone());
            send(Update::Ready {
                accounts,
                autostart,
            });
            // Settings windows are commonly reopened while Bridge is already
            // connected. Refresh Bridge's generated password on every attach.
            if let Some(id) = first_connected.filter(|_| show_saved_settings) {
                show_account(&mut connection, id, &send).await;
            }
        }
        Err(status) => {
            send(Update::Error {
                step: Step::Password,
                message: status_message(&status),
            });
            // A cancelled event stream makes Bridge quit. Always stop it
            // gracefully before dropping our connection, including errors.
            if stream_owned && !stream_confirmed {
                if let Ok(Some(StreamUpdate::Failed(_))) =
                    tokio::time::timeout(Duration::from_millis(750), stream_messages.recv()).await
                {
                    stream_owned = false;
                }
            }
            if stream_owned {
                let _ = connection.stop_stream().await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), stream_task).await;
            send(Update::Closed);
            return;
        }
    }
    let mut username = String::new();
    loop {
        tokio::select! {
            biased;
            event = stream_messages.recv() => {
                match event {
                    Some(StreamUpdate::Event(event)) => {
                        stream_owned = true;
                        stream_confirmed = true;
                        if matches!(event.event.as_ref(), Some(stream_event::Event::Login(login)) if matches!(login.event, Some(login_event::Event::Finished(_)) | Some(login_event::Event::AlreadyLoggedIn(_)))) {
                            username.clear();
                            crate::share::notify(crate::share::LoginEvent::Finished);
                        }
                        dispatch(&mut connection, event, &send).await;
                    }
                    Some(StreamUpdate::Failed(status)) => {
                        stream_owned = false;
                        send(Update::Error { step: Step::Password, message: status_message(&status) });
                        break;
                    }
                    Some(StreamUpdate::Ended) | None => {
                        stream_owned = false;
                        send(Update::Error { step: Step::Password, message: "Bridge disconnected. Reconnect to continue.".into() });
                        break;
                    }
                }
            }
            command = commands.recv() => {
                let result = match command {
                    Some(Command::Login { method, username: account, secret }) => {
                        username.clone_from(&account);
                        crate::share::notify(crate::share::LoginEvent::Entered { method, username: &account, secret: &secret });
                        connection.login(method, account, secret).await
                    }
                    Some(Command::Autostart(value)) => {
                        if crate::background::is_flatpak() {
                            let update = crate::background::request_autostart(value)
                                .await
                                .and_then(|()| crate::background::set_autostart_marker(value));
                            match update {
                                Ok(()) => send(Update::Autostart(value)),
                                Err(message) => send(Update::AutostartFailed(message)),
                            }
                            Ok(())
                        } else {
                            let request = connection.unary(protocol::Boolean { value });
                            connection.client.set_is_autostart_on(request).await.map(|_| {
                                send(Update::Autostart(value));
                            })
                        }
                    }
                    Some(Command::ShowAccount(id)) => {
                        show_account(&mut connection, id, &send).await;
                        Ok(())
                    }
                    Some(Command::Logout(id)) => {
                        let mut request = connection.unary(protocol::StringValue { value: id.clone() });
                        request.set_timeout(Duration::from_secs(3));
                        let result = match connection.client.logout_user(request).await {
                            Ok(_) => wait_for_logout(&mut connection, id).await,
                            Err(status) => Err(status),
                        };
                        match result {
                            Ok(accounts) => send(Update::LoggedOut { accounts }),
                            Err(status) => send(Update::Error {
                                step: Step::Finished,
                                message: if status.code() == tonic::Code::DeadlineExceeded {
                                    "Bridge has not finished signing out yet. Wait a moment, then retry.".into()
                                } else {
                                    status_message(&status)
                                },
                            }),
                        }
                        Ok(())
                    }
                    Some(Command::Cancel(account)) => {
                        crate::share::notify(crate::share::LoginEvent::Abandoned);
                        abort(&mut connection, account).await;
                        username.clear();
                        send(Update::Step(Step::Password));
                        Ok(())
                    }
                    Some(Command::Close) | None => {
                        if !username.is_empty() {
                            crate::share::notify(crate::share::LoginEvent::Abandoned);
                            abort(&mut connection, username).await;
                        }
                        // Keep Bridge alive after the companion window closes.
                        if stream_owned && !stream_confirmed {
                            match tokio::time::timeout(Duration::from_millis(750), stream_messages.recv()).await {
                                Ok(Some(StreamUpdate::Failed(status))) => {
                                    stream_owned = false;
                                    send(Update::Error { step: Step::Password, message: status_message(&status) });
                                }
                                Ok(Some(StreamUpdate::Event(_))) => {}
                                Ok(Some(StreamUpdate::Ended)) | Ok(None) => stream_owned = false,
                                Err(_) => {}
                            }
                        }
                        while let Ok(update) = stream_messages.try_recv() {
                            match update {
                                StreamUpdate::Failed(_) | StreamUpdate::Ended => stream_owned = false,
                                StreamUpdate::Event(_) => stream_owned = true,
                            }
                        }
                        let result = if stream_owned { connection.stop_stream().await } else { Ok(()) };
                        stream_owned = false;
                        if result.is_err() {
                            send(Update::Error { step: Step::Recovery, message: "Could not detach cleanly. Retry after Bridge reconnects.".into() });
                        }
                        break;
                    }
                };
                if let Err(status) = result {
                    send(Update::Error { step: Step::Recovery, message: status_message(&status) });
                }
            }
        }
    }
    // If the stream was still waiting for its first headers, stop it using the
    // Bridge RPC before dropping the reader task; dropping the HTTP/2 request
    // makes Bridge interpret the client as having quit.
    if stream_owned {
        let _ = connection.stop_stream().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), stream_task).await;
    send(Update::Closed);
}

/// Initialize saved accounts at desktop login without retaining a GUI stream.
pub async fn initialize_and_detach(path: PathBuf) -> Result<(), String> {
    let (commands, receiver) = mpsc::unbounded_channel();
    let outcome = std::sync::Mutex::new(None);
    run_session(
        path,
        receiver,
        |update| {
            let result = match update {
                Update::Ready { .. } => Some(Ok(())),
                Update::Error { message, .. } => Some(Err(message)),
                _ => None,
            };
            if let Some(result) = result {
                if let Ok(mut outcome) = outcome.lock() {
                    *outcome = Some(result);
                }
                let _ = commands.send(Command::Close);
            }
        },
        false,
    )
    .await;
    outcome
        .into_inner()
        .map_err(|_| "Bridge initialization result was unavailable.".to_owned())?
        .ok_or_else(|| "Bridge closed before saved accounts were initialized.".to_owned())?
}

async fn abort(connection: &mut Connection, username: String) {
    let request = connection.unary(protocol::Username {
        username: username.clone(),
    });
    let _ = connection.client.fido_assertion_abort(request).await;
    let request = connection.unary(protocol::Username { username });
    let _ = connection.client.login_abort(request).await;
}

async fn get_accounts(
    connection: &mut Connection,
) -> Result<Vec<(String, String, i32)>, tonic::Status> {
    let request = connection.unary(protocol::Empty {});
    let users = connection
        .client
        .get_user_list(request)
        .await?
        .into_inner()
        .users;
    Ok(users
        .into_iter()
        .map(|mut user| {
            use zeroize::Zeroize;
            user.password.zeroize();
            (user.id, user.username, user.state)
        })
        .collect())
}

// LogoutUser starts sign-out in a goroutine and may return before Bridge updates
// its account list. Only tell the GUI logout succeeded after the target account
// disappears or Bridge reports it as signed out.
async fn wait_for_logout(
    connection: &mut Connection,
    id: String,
) -> Result<Vec<(String, String, i32)>, tonic::Status> {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let accounts = get_accounts(connection).await?;
            if accounts
                .iter()
                .find(|(account_id, _, _)| account_id == &id)
                .map_or(true, |(_, _, state)| *state == 0)
            {
                return Ok(accounts);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| tonic::Status::deadline_exceeded("logout state is still pending"))?
}

async fn dispatch(
    connection: &mut Connection,
    event: protocol::StreamEvent,
    send: &impl Fn(Update),
) {
    match event.event {
        Some(stream_event::Event::Login(login)) => {
            if let Some(event) = login.event {
                let id = match &event {
                    login_event::Event::Finished(user) | login_event::Event::AlreadyLoggedIn(user) => Some(user.user_id.clone()),
                    _ => None,
                };
                send(login_update(event));
                if let Some(id) = id { show_account(connection, id, send).await; }
            }
        }
        Some(stream_event::Event::Keychain(keychain)) if keychain.event.is_some() => send(Update::Error {
            step: Step::Recovery,
            message: "Bridge needs a working Linux keyring. Unlock or configure it in your desktop keyring settings, then retry.".into(),
        }),
        _ => {}
    }
}

async fn show_account(connection: &mut Connection, id: String, send: &impl Fn(Update)) {
    match read_account_settings(connection, id.clone()).await {
        Ok(settings) => {
            crate::share::remember_account(&settings);
            send(Update::MailSettings(settings))
        }
        Err(message) => {
            let accounts = tokio::time::timeout(Duration::from_secs(3), get_accounts(connection))
                .await
                .ok()
                .and_then(Result::ok);
            send(Update::AccountSettingsFailed {
                id,
                accounts,
                message: message.into(),
            });
        }
    }
}

// Keep the three Bridge reads separate so the UI can give a useful recovery
// hint without ever forwarding a server error string (which may contain data).
// Tests exercise the same path through a fake RPC implementation.
trait AccountSettingsRpc {
    fn get_user(
        &mut self,
        id: String,
    ) -> impl std::future::Future<Output = Result<protocol::User, tonic::Status>> + Send;
    fn hostname(
        &mut self,
    ) -> impl std::future::Future<Output = Result<String, tonic::Status>> + Send;
    fn mail_server_settings(
        &mut self,
    ) -> impl std::future::Future<Output = Result<protocol::ImapSmtpSettings, tonic::Status>> + Send;
}

impl AccountSettingsRpc for Connection {
    fn get_user(
        &mut self,
        id: String,
    ) -> impl std::future::Future<Output = Result<protocol::User, tonic::Status>> + Send {
        async move {
            let request = self.unary(protocol::StringValue { value: id });
            Ok(self.client.get_user(request).await?.into_inner())
        }
    }

    fn hostname(
        &mut self,
    ) -> impl std::future::Future<Output = Result<String, tonic::Status>> + Send {
        async move {
            let request = self.unary(protocol::Empty {});
            Ok(self.client.hostname(request).await?.into_inner().value)
        }
    }

    fn mail_server_settings(
        &mut self,
    ) -> impl std::future::Future<Output = Result<protocol::ImapSmtpSettings, tonic::Status>> + Send
    {
        async move {
            let request = self.unary(protocol::Empty {});
            Ok(self
                .client
                .mail_server_settings(request)
                .await?
                .into_inner())
        }
    }
}

async fn read_account_settings(
    rpc: &mut impl AccountSettingsRpc,
    id: String,
) -> Result<MailSettings, &'static str> {
    // LoginFinished may precede the user list update by a short interval. Give
    // Bridge up to two seconds to publish the connected account before showing
    // a retry state. This mirrors upstream's own bounded user-change wait.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut user = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("Bridge has not published this account yet. Retry the account settings.");
        }
        match tokio::time::timeout(remaining, rpc.get_user(id.clone())).await {
            Err(_) => {
                return Err(
                    "Bridge is taking too long to publish this account. Retry the account settings.",
                );
            }
            Ok(Ok(user)) if user.state == 2 => break user,
            Ok(Ok(mut user)) => {
                user.password.zeroize();
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining > Duration::from_millis(100) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                } else {
                    tokio::time::sleep(remaining).await;
                    return Err("This account is no longer connected. Sign in again, then retry.");
                }
            }
            Ok(Err(status))
                if status.code() == tonic::Code::NotFound
                    && tokio::time::Instant::now() < deadline =>
            {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
            }
            Ok(Err(status)) => {
                return Err(match status.code() {
                    tonic::Code::Unauthenticated => {
                        "Bridge's local connection expired. Reconnect, then retry."
                    }
                    tonic::Code::NotFound => {
                        "Bridge no longer has this account. Refresh the account list, then retry."
                    }
                    _ => "Bridge could not read this account. Reconnect, then retry.",
                });
            }
        }
    };
    if user.id != id {
        user.password.zeroize();
        return Err("Bridge returned a different account. Refresh the account list, then retry.");
    }
    // The password is generated by Bridge and is only used for connected users.
    let bytes = Zeroizing::new(std::mem::take(&mut user.password));
    if bytes.is_empty() {
        return Err(
            "Bridge has no generated mail-client password for this account. Sign in again, then retry.",
        );
    }
    let password = Zeroizing::new(std::str::from_utf8(&bytes).map(str::to_owned).map_err(
        |_| "Bridge returned an invalid mail-client password. Sign in again, then retry.",
    )?);
    let hostname = rpc.hostname().await.map_err(|status| match status.code() {
        tonic::Code::Unauthenticated => "Bridge's local connection expired. Reconnect, then retry.",
        _ => "Bridge could not read its mail-server address. Reconnect, then retry.",
    })?;
    if !matches!(hostname.as_str(), "127.0.0.1" | "localhost" | "::1") {
        return Err(
            "Bridge returned an unexpected mail-server address. Reconnect before using these settings.",
        );
    }
    let settings = rpc
        .mail_server_settings()
        .await
        .map_err(|status| match status.code() {
            tonic::Code::Unauthenticated => {
                "Bridge's local connection expired. Reconnect, then retry."
            }
            _ => "Bridge could not read its mail-server settings. Reconnect, then retry.",
        })?;
    if !(1..=65_535).contains(&settings.imap_port) || !(1..=65_535).contains(&settings.smtp_port) {
        return Err(
            "Bridge returned invalid mail-server ports. Reconnect before using these settings.",
        );
    }
    Ok(MailSettings {
        id: user.id,
        username: user.username,
        addresses: user.addresses,
        password,
        hostname,
        imap_port: settings.imap_port,
        smtp_port: settings.smtp_port,
        imap_ssl: settings.use_ssl_for_imap,
        smtp_ssl: settings.use_ssl_for_smtp,
    })
}

#[cfg(test)]
mod account_settings_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::future::Future;

    struct FakeAccountRpc {
        users: VecDeque<Result<protocol::User, tonic::Status>>,
        last_user: protocol::User,
        hostname: Option<Result<String, tonic::Status>>,
        settings: Option<Result<protocol::ImapSmtpSettings, tonic::Status>>,
        hostname_calls: usize,
        settings_calls: usize,
    }

    impl AccountSettingsRpc for FakeAccountRpc {
        fn get_user(
            &mut self,
            _id: String,
        ) -> impl Future<Output = Result<protocol::User, tonic::Status>> + Send {
            async move {
                self.users
                    .pop_front()
                    .unwrap_or_else(|| Ok(self.last_user.clone()))
            }
        }

        fn hostname(&mut self) -> impl Future<Output = Result<String, tonic::Status>> + Send {
            async move {
                self.hostname_calls += 1;
                self.hostname
                    .take()
                    .expect("fake hostname result is used once")
            }
        }

        fn mail_server_settings(
            &mut self,
        ) -> impl Future<Output = Result<protocol::ImapSmtpSettings, tonic::Status>> + Send
        {
            async move {
                self.settings_calls += 1;
                self.settings
                    .take()
                    .expect("fake settings result is used once")
            }
        }
    }

    fn fake(user_state: i32, password: Vec<u8>) -> FakeAccountRpc {
        FakeAccountRpc {
            users: VecDeque::new(),
            last_user: protocol::User {
                id: "fake-account-id".into(),
                username: "person@example.test".into(),
                state: user_state,
                password,
                addresses: vec!["person@example.test".into()],
            },
            hostname: Some(Ok("127.0.0.1".into())),
            settings: Some(Ok(protocol::ImapSmtpSettings {
                imap_port: 1143,
                smtp_port: 1025,
                use_ssl_for_imap: false,
                use_ssl_for_smtp: true,
            })),
            hostname_calls: 0,
            settings_calls: 0,
        }
    }

    #[tokio::test]
    async fn reads_generated_password_and_server_settings_for_connected_account() {
        let mut rpc = fake(2, b"generated-mail-secret".to_vec());
        let settings = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .unwrap();
        assert_eq!(settings.id, "fake-account-id");
        assert_eq!(settings.username, "person@example.test");
        assert_eq!(&*settings.password, "generated-mail-secret");
        assert_eq!(settings.hostname, "127.0.0.1");
        assert_eq!((settings.imap_port, settings.smtp_port), (1143, 1025));
        assert!(!settings.imap_ssl && settings.smtp_ssl);
        assert_eq!(rpc.hostname_calls, 1);
        assert_eq!(rpc.settings_calls, 1);
    }

    #[tokio::test]
    async fn retries_until_login_finished_account_is_published() {
        let mut rpc = fake(2, b"generated-mail-secret".to_vec());
        rpc.users
            .push_back(Err(tonic::Status::not_found("private detail")));
        let result = read_account_settings(&mut rpc, "fake-account-id".into()).await;
        assert!(result.is_ok());
        assert_eq!(rpc.hostname_calls, 1);
        assert_eq!(rpc.settings_calls, 1);
    }

    #[tokio::test]
    async fn never_requests_or_returns_password_for_disconnected_account() {
        let mut rpc = fake(0, b"must-not-be-used".to_vec());
        let error = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .err()
            .expect("disconnected account must fail");
        assert!(error.contains("no longer connected"));
        assert_eq!(rpc.hostname_calls, 0);
        assert_eq!(rpc.settings_calls, 0);
    }

    #[tokio::test]
    async fn server_errors_are_sanitized_and_identify_the_failed_stage() {
        let mut rpc = fake(2, b"generated-mail-secret".to_vec());
        rpc.hostname = Some(Err(tonic::Status::internal("secret-token-in-server-error")));
        let error = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .err()
            .expect("hostname RPC failure must be reported");
        assert!(error.contains("mail-server address"));
        assert!(!error.contains("secret-token"));
        assert_eq!(rpc.hostname_calls, 1);
        assert_eq!(rpc.settings_calls, 0);
    }

    #[tokio::test]
    async fn rejects_non_utf8_password_bytes_without_exposing_them() {
        let mut rpc = fake(2, vec![0xff, 0xfe]);
        let error = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .err()
            .expect("invalid password bytes must be rejected");
        assert!(error.contains("invalid mail-client password"));
        assert_eq!(rpc.hostname_calls, 0);
    }

    #[tokio::test]
    async fn rejects_a_user_response_for_a_different_account() {
        let mut rpc = fake(2, b"generated-mail-secret".to_vec());
        rpc.last_user.id = "different-account-id".into();
        let error = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .err()
            .expect("a different user must be rejected");
        assert!(error.contains("different account"));
        assert_eq!(rpc.hostname_calls, 0);
        assert_eq!(rpc.settings_calls, 0);
    }

    #[tokio::test]
    async fn rejects_non_loopback_hostnames_and_invalid_ports() {
        let mut rpc = fake(2, b"generated-mail-secret".to_vec());
        rpc.hostname = Some(Ok("mail.attacker.example".into()));
        let error = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .err()
            .expect("non-loopback host must be rejected");
        assert!(error.contains("unexpected mail-server address"));
        assert_eq!(rpc.settings_calls, 0);

        let mut rpc = fake(2, b"generated-mail-secret".to_vec());
        rpc.settings = Some(Ok(protocol::ImapSmtpSettings {
            imap_port: 0,
            smtp_port: 65_536,
            use_ssl_for_imap: true,
            use_ssl_for_smtp: true,
        }));
        let error = read_account_settings(&mut rpc, "fake-account-id".into())
            .await
            .err()
            .expect("invalid ports must be rejected");
        assert!(error.contains("invalid mail-server ports"));
    }
}
