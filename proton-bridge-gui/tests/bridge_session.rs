//! A TLS-protected Unix-socket server exercises the actual RPC transport and
//! the multi-stage sign-in flow without accessing a real account or keyring.
use base64::{Engine, engine::general_purpose::STANDARD};
use spotty_proton_bridge_gui::protocol::{
    self as p,
    bridge_server::{Bridge, BridgeServer},
};
use spotty_proton_bridge_gui::rpc::{Connection, LoginMethod};
use spotty_proton_bridge_gui::session::{self, Command, Step, Update};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::{
    Stream,
    wrappers::{ReceiverStream, UnixListenerStream},
};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};
use zeroize::Zeroizing;

const TOKEN: &str = "local-test-token";
type Events = mpsc::Sender<Result<p::StreamEvent, Status>>;

#[derive(Default)]
struct State {
    events: Mutex<Option<Events>>,
    login_calls: Mutex<Vec<(String, String)>>,
    startup: AtomicBool,
    locked_account: AtomicBool,
    signed_out: AtomicBool,
    logout_pending: AtomicBool,
    logout_list_polls: AtomicUsize,
    logout_calls: Mutex<Vec<String>>,
    account_settings_reads: AtomicUsize,
    normal_end: AtomicBool,
    unexpected_disconnect: AtomicBool,
    stops: AtomicUsize,
    quits: AtomicUsize,
    busy: AtomicBool,
}

#[derive(Clone)]
struct Mock(Arc<State>);

fn authenticate<T>(request: &Request<T>) -> Result<(), Status> {
    if request
        .metadata()
        .get("server-token")
        .and_then(|value| value.to_str().ok())
        != Some(TOKEN)
    {
        return Err(Status::unauthenticated("invalid token"));
    }
    Ok(())
}

impl Mock {
    async fn login(
        &self,
        request: Request<p::LoginRequest>,
        name: &str,
        next: p::login_event::Event,
    ) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        let request = request.into_inner();
        let decoded = STANDARD
            .decode(&request.password)
            .map_err(|_| Status::invalid_argument("base64 required"))?;
        self.0
            .login_calls
            .lock()
            .unwrap()
            .push((name.into(), String::from_utf8(decoded).unwrap()));
        assert_eq!(request.username, "test@proton.me");
        if matches!(&next, p::login_event::Event::Finished(_)) {
            self.0.signed_out.store(false, Ordering::SeqCst);
        }
        let sender = self.0.events.lock().unwrap().clone().unwrap();
        sender
            .send(Ok(p::StreamEvent {
                event: Some(p::stream_event::Event::Login(p::LoginEvent {
                    event: Some(next),
                })),
            }))
            .await
            .unwrap();
        Ok(Response::new(p::Empty {}))
    }
}

struct EventStream {
    receiver: ReceiverStream<Result<p::StreamEvent, Status>>,
    state: Arc<State>,
}
impl Stream for EventStream {
    type Item = Result<p::StreamEvent, Status>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}
impl Drop for EventStream {
    fn drop(&mut self) {
        if !self.state.normal_end.load(Ordering::SeqCst) {
            self.state
                .unexpected_disconnect
                .store(true, Ordering::SeqCst);
        }
    }
}

#[tonic::async_trait]
impl Bridge for Mock {
    type RunEventStreamStream = EventStream;
    async fn quit(&self, request: Request<p::Empty>) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        self.0.quits.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(p::Empty {}))
    }
    async fn gui_ready(&self, request: Request<p::Empty>) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        Ok(Response::new(p::Empty {}))
    }
    async fn get_user_list(
        &self,
        request: Request<p::Empty>,
    ) -> Result<Response<p::UserList>, Status> {
        authenticate(&request)?;
        if self.0.logout_pending.load(Ordering::SeqCst)
            && self.0.logout_list_polls.fetch_add(1, Ordering::SeqCst) >= 2
        {
            self.0.signed_out.store(true, Ordering::SeqCst);
            self.0.logout_pending.store(false, Ordering::SeqCst);
        }
        Ok(Response::new(p::UserList {
            users: vec![p::User {
                id: "test-user".into(),
                username: "test@proton.me".into(),
                state: if self.0.signed_out.load(Ordering::SeqCst) {
                    0
                } else if self.0.locked_account.load(Ordering::SeqCst) {
                    1
                } else {
                    2
                },
                password: b"secret-must-be-cleared".to_vec(),
                addresses: vec![],
            }],
        }))
    }
    async fn get_user(
        &self,
        request: Request<p::StringValue>,
    ) -> Result<Response<p::User>, Status> {
        authenticate(&request)?;
        self.0.account_settings_reads.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.into_inner().value, "test-user");
        Ok(Response::new(p::User {
            id: "test-user".into(),
            username: "test@proton.me".into(),
            state: if self.0.signed_out.load(Ordering::SeqCst) {
                0
            } else if self.0.locked_account.load(Ordering::SeqCst) {
                1
            } else {
                2
            },
            password: b"BridgePasswordAbC123".to_vec(),
            addresses: vec!["test@proton.me".into()],
        }))
    }
    async fn logout_user(
        &self,
        request: Request<p::StringValue>,
    ) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        let id = request.into_inner().value;
        assert_eq!(id, "test-user");
        self.0.logout_calls.lock().unwrap().push(id);
        self.0.logout_list_polls.store(0, Ordering::SeqCst);
        self.0.logout_pending.store(true, Ordering::SeqCst);
        Ok(Response::new(p::Empty {}))
    }
    async fn hostname(
        &self,
        request: Request<p::Empty>,
    ) -> Result<Response<p::StringValue>, Status> {
        authenticate(&request)?;
        Ok(Response::new(p::StringValue {
            value: "127.0.0.1".into(),
        }))
    }
    async fn mail_server_settings(
        &self,
        request: Request<p::Empty>,
    ) -> Result<Response<p::ImapSmtpSettings>, Status> {
        authenticate(&request)?;
        Ok(Response::new(p::ImapSmtpSettings {
            imap_port: 1143,
            smtp_port: 1025,
            use_ssl_for_imap: false,
            use_ssl_for_smtp: true,
        }))
    }
    async fn login(&self, request: Request<p::LoginRequest>) -> Result<Response<p::Empty>, Status> {
        self.login(
            request,
            "password",
            p::login_event::Event::TfaRequested(p::Username {
                username: "test@proton.me".into(),
            }),
        )
        .await
    }
    async fn login2_fa(
        &self,
        request: Request<p::LoginRequest>,
    ) -> Result<Response<p::Empty>, Status> {
        self.login(
            request,
            "2fa",
            p::login_event::Event::TwoPasswordRequested(p::Username {
                username: "test@proton.me".into(),
            }),
        )
        .await
    }
    async fn login2_passwords(
        &self,
        request: Request<p::LoginRequest>,
    ) -> Result<Response<p::Empty>, Status> {
        self.login(
            request,
            "mailbox",
            p::login_event::Event::Finished(p::Finished {
                user_id: "test-user".into(),
            }),
        )
        .await
    }
    async fn login_fido(
        &self,
        request: Request<p::LoginRequest>,
    ) -> Result<Response<p::Empty>, Status> {
        self.login(
            request,
            "key",
            p::login_event::Event::LoginFidoTouchRequested(p::Username {
                username: "test@proton.me".into(),
            }),
        )
        .await
    }
    async fn login_abort(
        &self,
        request: Request<p::Username>,
    ) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        Ok(Response::new(p::Empty {}))
    }
    async fn fido_assertion_abort(
        &self,
        request: Request<p::Username>,
    ) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        Ok(Response::new(p::Empty {}))
    }
    async fn is_autostart_on(
        &self,
        request: Request<p::Empty>,
    ) -> Result<Response<p::Boolean>, Status> {
        authenticate(&request)?;
        Ok(Response::new(p::Boolean {
            value: self.0.startup.load(Ordering::SeqCst),
        }))
    }
    async fn set_is_autostart_on(
        &self,
        request: Request<p::Boolean>,
    ) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        self.0
            .startup
            .store(request.into_inner().value, Ordering::SeqCst);
        Ok(Response::new(p::Empty {}))
    }
    async fn run_event_stream(
        &self,
        request: Request<p::StreamRequest>,
    ) -> Result<Response<Self::RunEventStreamStream>, Status> {
        authenticate(&request)?;
        if self.0.busy.load(Ordering::SeqCst) {
            return Err(Status::already_exists("another frontend owns login"));
        }
        let (sender, receiver) = mpsc::channel(16);
        sender
            .send(Ok(p::StreamEvent { event: None }))
            .await
            .unwrap();
        *self.0.events.lock().unwrap() = Some(sender);
        Ok(Response::new(EventStream {
            receiver: ReceiverStream::new(receiver),
            state: self.0.clone(),
        }))
    }
    async fn stop_event_stream(
        &self,
        request: Request<p::Empty>,
    ) -> Result<Response<p::Empty>, Status> {
        authenticate(&request)?;
        self.0.normal_end.store(true, Ordering::SeqCst);
        self.0.stops.fetch_add(1, Ordering::SeqCst);
        self.0.events.lock().unwrap().take();
        Ok(Response::new(p::Empty {}))
    }
}

struct ServerFixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    state: Arc<State>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for ServerFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn server() -> ServerFixture {
    let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let socket = directory.path().join("bridge.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let path = directory.path().join("grpcServerConfig.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "cert": certificate.cert.pem(), "token": TOKEN, "fileSocketPath": socket,
        }))
        .unwrap(),
    )
    .unwrap();
    let state = Arc::new(State::default());
    let service = Mock(state.clone());
    let task = tokio::spawn(async move {
        Server::builder()
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(
                certificate.cert.pem(),
                certificate.signing_key.serialize_pem(),
            )))
            .unwrap()
            .add_service(BridgeServer::new(service))
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
            .unwrap();
    });
    ServerFixture {
        _directory: directory,
        path,
        state,
        server: task,
    }
}

async fn next(updates: &mut mpsc::UnboundedReceiver<Update>) -> Update {
    loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .unwrap()
            .unwrap();
        if !matches!(update, Update::ConnectionEstablished) {
            return update;
        }
    }
}

#[tokio::test]
async fn login_challenges_startup_and_close_use_verified_local_transport() {
    let fixture = server().await;
    let (commands, receiver) = mpsc::unbounded_channel();
    let (updates, mut messages) = mpsc::unbounded_channel();
    let session = tokio::spawn(session::run(
        fixture.path.clone(),
        receiver,
        move |message| {
            let _ = updates.send(message);
        },
    ));
    assert!(matches!(
        next(&mut messages).await,
        Update::Ready {
            autostart: false,
            ..
        }
    ));
    match next(&mut messages).await {
        Update::MailSettings(settings) => {
            assert_eq!(settings.id, "test-user");
            assert_eq!(*settings.password, "BridgePasswordAbC123");
        }
        _ => panic!("Existing connected account settings were not refreshed on attach"),
    }
    for (method, secret, expected) in [
        (LoginMethod::Password, "password-example", Step::TwoFactor),
        (LoginMethod::TwoFactor, "012345", Step::MailboxPassword),
        (
            LoginMethod::MailboxPassword,
            "mailbox-example",
            Step::Finished,
        ),
    ] {
        commands
            .send(Command::Login {
                method,
                username: "test@proton.me".into(),
                secret: Zeroizing::new(secret.into()),
            })
            .unwrap();
        assert!(matches!(next(&mut messages).await, Update::Step(step) if step == expected));
    }
    match next(&mut messages).await {
        Update::MailSettings(settings) => {
            assert_eq!(*settings.password, "BridgePasswordAbC123");
            assert_ne!(*settings.password, "password-example");
            assert_eq!(settings.addresses, ["test@proton.me"]);
            assert_eq!(settings.hostname, "127.0.0.1");
            assert_eq!((settings.imap_port, settings.smtp_port), (1143, 1025));
            assert!(!settings.imap_ssl);
            assert!(settings.smtp_ssl);
        }
        _ => panic!("Missing Bridge-generated credentials after login"),
    }
    // Reopening an existing account retrieves its generated password again.
    commands
        .send(Command::ShowAccount("test-user".into()))
        .unwrap();
    assert!(matches!(next(&mut messages).await, Update::MailSettings(_)));
    // Locked/signed-out accounts must never display a cached mail secret.
    fixture.state.locked_account.store(true, Ordering::SeqCst);
    commands
        .send(Command::ShowAccount("test-user".into()))
        .unwrap();
    assert!(matches!(next(&mut messages).await, Update::Error { .. }));
    fixture.state.locked_account.store(false, Ordering::SeqCst);
    commands.send(Command::Logout("test-user".into())).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        messages.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    match next(&mut messages).await {
        Update::LoggedOut { accounts } => {
            assert!(fixture.state.signed_out.load(Ordering::SeqCst));
            assert!(fixture.state.logout_list_polls.load(Ordering::SeqCst) >= 3);
            assert_eq!(accounts, [("test-user".into(), "test@proton.me".into(), 0)]);
        }
        _ => panic!("Successful logout did not refresh saved accounts"),
    }
    commands
        .send(Command::ShowAccount("test-user".into()))
        .unwrap();
    assert!(matches!(next(&mut messages).await, Update::Error { .. }));
    for (method, secret, expected) in [
        (LoginMethod::Password, "again-password", Step::TwoFactor),
        (LoginMethod::TwoFactor, "654321", Step::MailboxPassword),
        (
            LoginMethod::MailboxPassword,
            "again-mailbox-password",
            Step::Finished,
        ),
    ] {
        commands
            .send(Command::Login {
                method,
                username: "test@proton.me".into(),
                secret: Zeroizing::new(secret.into()),
            })
            .unwrap();
        assert!(matches!(next(&mut messages).await, Update::Step(step) if step == expected));
    }
    assert!(matches!(
        next(&mut messages).await,
        Update::MailSettings(settings) if *settings.password == "BridgePasswordAbC123"
    ));
    commands.send(Command::Autostart(true)).unwrap();
    assert!(matches!(next(&mut messages).await, Update::Autostart(true)));
    commands.send(Command::Close).unwrap();
    assert!(matches!(next(&mut messages).await, Update::Closed));
    session.await.unwrap();
    assert_eq!(fixture.state.login_calls.lock().unwrap().len(), 6);
    assert_eq!(&*fixture.state.logout_calls.lock().unwrap(), &["test-user"]);
    assert_eq!(fixture.state.stops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state.quits.load(Ordering::SeqCst), 0);
    assert!(!fixture.state.unexpected_disconnect.load(Ordering::SeqCst));
}

#[tokio::test]
async fn occupied_bridge_frontend_is_never_stopped_or_given_credentials() {
    let fixture = server().await;
    fixture.state.busy.store(true, Ordering::SeqCst);
    let (_commands, receiver) = mpsc::unbounded_channel();
    let (updates, mut messages) = mpsc::unbounded_channel();
    session::run(fixture.path.clone(), receiver, |message| {
        let _ = updates.send(message);
    })
    .await;
    assert!(matches!(
        next(&mut messages).await,
        Update::Error {
            step: Step::OfficialGui,
            ..
        }
    ));
    assert!(matches!(next(&mut messages).await, Update::Closed));
    assert_eq!(fixture.state.stops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.state.quits.load(Ordering::SeqCst), 0);
    assert!(fixture.state.login_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wrong_certificate_cannot_receive_credentials() {
    let fixture = server().await;
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.path).unwrap()).unwrap();
    config["cert"] = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()])
        .unwrap()
        .cert
        .pem()
        .into();
    std::fs::write(&fixture.path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert!(Connection::connect(&fixture.path).await.is_err());
    assert!(fixture.state.login_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn security_key_and_official_gui_handoff_release_the_owned_backend() {
    let fixture = server().await;
    fixture.state.signed_out.store(true, Ordering::SeqCst);
    let (commands, receiver) = mpsc::unbounded_channel();
    let (updates, mut messages) = mpsc::unbounded_channel();
    let session = tokio::spawn(session::run(
        fixture.path.clone(),
        receiver,
        move |message| {
            let _ = updates.send(message);
        },
    ));
    assert!(matches!(next(&mut messages).await, Update::Ready { .. }));
    commands
        .send(Command::Login {
            method: LoginMethod::SecurityKey,
            username: "test@proton.me".into(),
            secret: Zeroizing::new("test-pin".into()),
        })
        .unwrap();
    assert!(matches!(
        next(&mut messages).await,
        Update::Step(Step::TouchKey)
    ));
    commands.send(Command::OpenOfficial).unwrap();
    assert!(matches!(next(&mut messages).await, Update::Closed));
    session.await.unwrap();
    assert_eq!(
        fixture.state.login_calls.lock().unwrap()[0],
        ("key".into(), "test-pin".into())
    );
    assert_eq!(fixture.state.stops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state.quits.load(Ordering::SeqCst), 1);
    assert!(!fixture.state.unexpected_disconnect.load(Ordering::SeqCst));
}

#[test]
fn retries_stay_on_correct_challenge_and_sensitive_server_messages_are_not_shown() {
    for (kind, expected) in [
        (3, Step::TwoFactor),
        (5, Step::MailboxPassword),
        (8, Step::KeyPin),
        (9, Step::OfficialGui),
    ] {
        let update = session::login_update(p::login_event::Event::Error(p::LoginError {
            r#type: kind,
            message: "secret-sentinel".into(),
        }));
        assert!(
            matches!(update, Update::Error { step, message } if step == expected && !message.contains("secret-sentinel"))
        );
    }
    assert!(matches!(
        session::login_update(p::login_event::Event::HvRequested(p::HumanVerification {
            hv_url: "https://untrusted.invalid".into()
        })),
        Update::Error {
            step: Step::OfficialGui,
            ..
        }
    ));
}

#[tokio::test]
async fn desktop_startup_releases_frontend_and_leaves_mail_backend_running() {
    let fixture = server().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        session::initialize_and_detach(fixture.path.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        fixture.state.account_settings_reads.load(Ordering::SeqCst),
        0
    );
    assert_eq!(fixture.state.stops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state.quits.load(Ordering::SeqCst), 0);
    assert!(fixture.state.login_calls.lock().unwrap().is_empty());
    assert!(!fixture.state.unexpected_disconnect.load(Ordering::SeqCst));
    // The next login window can acquire a fresh session and detach normally.
    let (commands, receiver) = mpsc::unbounded_channel();
    commands.send(Command::Close).unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        session::run(fixture.path.clone(), receiver, |_| {}),
    )
    .await
    .unwrap();
    assert_eq!(fixture.state.stops.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.state.quits.load(Ordering::SeqCst), 0);
}
