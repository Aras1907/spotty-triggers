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

pub enum Update {
    Ready {
        accounts: Vec<(String, i32)>,
        autostart: bool,
    },
    Step(Step),
    Error {
        step: Step,
        message: String,
    },
    Autostart(bool),
    Closed,
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

pub async fn run(
    path: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Command>,
    send: impl Fn(Update),
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
        Ok(Ok(Some(event))) => dispatch(event, &send),
        Err(_) => {}
    }
    let ready = async {
        let request = connection.unary(protocol::Empty {});
        let users = connection
            .client
            .get_user_list(request)
            .await?
            .into_inner()
            .users;
        let request = connection.unary(protocol::Empty {});
        let autostart = connection
            .client
            .is_autostart_on(request)
            .await?
            .into_inner()
            .value;
        Ok::<_, tonic::Status>((
            users
                .into_iter()
                .map(|user| (user.username, user.state))
                .collect(),
            autostart,
        ))
    }
    .await;
    match ready {
        Ok((accounts, autostart)) => send(Update::Ready {
            accounts,
            autostart,
        }),
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
                        dispatch(event, &send);
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

async fn abort(connection: &mut Connection, username: String) {
    let request = connection.unary(protocol::Username {
        username: username.clone(),
    });
    let _ = connection.client.fido_assertion_abort(request).await;
    let request = connection.unary(protocol::Username { username });
    let _ = connection.client.login_abort(request).await;
}

fn dispatch(event: protocol::StreamEvent, send: &impl Fn(Update)) {
    match event.event {
        Some(stream_event::Event::Login(login)) => {
            if let Some(event) = login.event { send(login_update(event)); }
        }
        Some(stream_event::Event::Keychain(keychain)) if keychain.event.is_some() => send(Update::Error {
            step: Step::OfficialGui,
            message: "Bridge needs a working Linux keyring. Unlock or configure it in the official Bridge window.".into(),
        }),
        _ => {}
    }
}
