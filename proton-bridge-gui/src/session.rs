// The GUI and asynchronous Bridge session communicate without ever putting
// secrets in process arguments, shell commands, logging, or configuration.
use crate::protocol::{self, login_event, stream_event};
use crate::rpc::{Connection, LoginMethod};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

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
    OpenOfficial,
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
    OfficialGui,
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
    LoggedOut {
        accounts: Vec<(String, String, i32)>,
    },
    Step(Step),
    Error {
        step: Step,
        message: String,
    },
    Autostart(bool),
    Closed,
    ConnectionEstablished,
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
            step: Step::OfficialGui,
            message:
                "Proton requires human verification. Finish sign-in in the official Bridge window."
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
                    Step::OfficialGui,
                    "The security key PIN is blocked. Use the official Bridge window.",
                ),
                7 | 10 => (
                    Step::OfficialGui,
                    "Finish verification in the official Bridge window.",
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
        tonic::Code::AlreadyExists => "Another Bridge login window is active. Use that window, or choose Quit Bridge there before reconnecting here.",
        tonic::Code::Unauthenticated => "Bridge's connection token changed. Reconnect before signing in.",
        _ => "The local Bridge connection failed. Restart Bridge and reconnect.",
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
                step: Step::OfficialGui,
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
            step: Step::OfficialGui,
            message: status_message(&status),
        });
        send(Update::Closed);
        return;
    }
    let request = connection.request(protocol::StreamRequest {
        client_platform: "linux".into(),
    });
    let mut stream = match connection.client.run_event_stream(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => {
            send(Update::Error {
                step: Step::OfficialGui,
                message: status_message(&status),
            });
            send(Update::Closed);
            return;
        }
    };
    // Probe the first message. An occupied stream can be rejected in trailers,
    // after the initial RPC returns successfully. Never call StopEventStream
    // before we know this stream belongs to us.
    match tokio::time::timeout(Duration::from_millis(250), stream.message()).await {
        Ok(Err(status)) => {
            send(Update::Error {
                step: Step::OfficialGui,
                message: status_message(&status),
            });
            send(Update::Closed);
            return;
        }
        Ok(Ok(None)) => {
            send(Update::Error {
                step: Step::OfficialGui,
                message: "Bridge closed the login connection. Reconnect.".into(),
            });
            send(Update::Closed);
            return;
        }
        Ok(Ok(Some(event))) => dispatch(&mut connection, event, &send).await,
        Err(_) => {}
    }
    let ready = async {
        let accounts = get_accounts(&mut connection).await?;
        let request = connection.unary(protocol::Empty {});
        let autostart = connection
            .client
            .is_autostart_on(request)
            .await?
            .into_inner()
            .value;
        Ok::<_, tonic::Status>((accounts, autostart))
    }
    .await;
    match ready {
        Ok((accounts, autostart)) => {
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
                step: Step::OfficialGui,
                message: status_message(&status),
            });
            // A cancelled event stream makes Bridge quit. Always stop it
            // gracefully before dropping our connection, including errors.
            let _ = connection.stop_stream().await;
            send(Update::Closed);
            return;
        }
    }
    let mut username = String::new();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let result = match command {
                    Some(Command::Login { method, username: account, secret }) => {
                        username.clone_from(&account);
                        connection.login(method, account, secret).await
                    }
                    Some(Command::Autostart(value)) => {
                        let request = connection.unary(protocol::Boolean { value });
                        connection.client.set_is_autostart_on(request).await.map(|_| {
                            send(Update::Autostart(value));
                        })
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
                        abort(&mut connection, account).await;
                        username.clear();
                        send(Update::Step(Step::Password));
                        Ok(())
                    }
                    Some(Command::Close) | None => {
                        if !username.is_empty() { abort(&mut connection, username).await; }
                        // Keep Bridge alive after the companion window closes.
                        let result = connection.stop_stream().await;
                        if result.is_err() {
                            send(Update::Error { step: Step::OfficialGui, message: "Could not detach cleanly. Check that Bridge is still running.".into() });
                        }
                        break;
                    }
                    Some(Command::OpenOfficial) => {
                        if !username.is_empty() { abort(&mut connection, username).await; }
                        // A headless --grpc Bridge cannot display a window.
                        // The user requested the official GUI: release our
                        // stream and quit this instance so it can be relaunched.
                        let _ = connection.stop_stream().await;
                        let request = connection.unary(protocol::Empty {});
                        let _ = connection.client.quit(request).await;
                        break;
                    }
                };
                if let Err(status) = result {
                    send(Update::Error { step: Step::OfficialGui, message: status_message(&status) });
                }
            }
            event = stream.message() => {
                match event {
                    Ok(Some(event)) => {
                        if matches!(event.event.as_ref(), Some(stream_event::Event::Login(login)) if matches!(login.event, Some(login_event::Event::Finished(_)) | Some(login_event::Event::AlreadyLoggedIn(_)))) {
                            username.clear();
                        }
                        dispatch(&mut connection, event, &send).await;
                    }
                    Ok(None) | Err(_) => {
                        send(Update::Error { step: Step::OfficialGui, message: "Bridge disconnected. Reconnect to continue.".into() });
                        let _ = connection.stop_stream().await;
                        break;
                    }
                }
            }
        }
    }
    send(Update::Closed);
}

/// Initialize saved accounts at desktop login without retaining a GUI stream.
pub async fn initialize_and_detach(path: PathBuf) {
    let (commands, receiver) = mpsc::unbounded_channel();
    run_session(
        path,
        receiver,
        |update| {
            if matches!(update, Update::Ready { .. } | Update::Error { .. }) {
                let _ = commands.send(Command::Close);
            }
        },
        false,
    )
    .await;
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
            step: Step::OfficialGui,
            message: "Bridge needs a working Linux keyring. Unlock or configure it in the official Bridge window.".into(),
        }),
        _ => {}
    }
}

async fn show_account(connection: &mut Connection, id: String, send: &impl Fn(Update)) {
    let result = async {
        let request = connection.unary(protocol::StringValue { value: id });
        let mut user = connection.client.get_user(request).await?.into_inner();
        // Only connected accounts may expose their generated mail-client secret.
        let bytes = Zeroizing::new(std::mem::take(&mut user.password));
        if user.state != 2 || bytes.is_empty() {
            return Err(tonic::Status::failed_precondition(
                "Account is not connected",
            ));
        }
        let password = Zeroizing::new(
            std::str::from_utf8(&bytes)
                .map(str::to_owned)
                .map_err(|_| tonic::Status::internal("Invalid Bridge password"))?,
        );
        let request = connection.unary(protocol::Empty {});
        let hostname = connection
            .client
            .hostname(request)
            .await?
            .into_inner()
            .value;
        let request = connection.unary(protocol::Empty {});
        let settings = connection
            .client
            .mail_server_settings(request)
            .await?
            .into_inner();
        Ok::<_, tonic::Status>(MailSettings {
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
    .await;
    match result {
        Ok(settings) => send(Update::MailSettings(settings)),
        Err(_) => send(Update::Error {
            step: Step::Finished,
            message: "Cannot read this account's mail-client settings. Reopen its settings after signing in, or use the official Bridge window.".into(),
        }),
    }
}
