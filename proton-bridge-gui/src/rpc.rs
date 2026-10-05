use crate::protocol::{self, bridge_client::BridgeClient};
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper_util::rt::TokioIo;
use native_tls::Protocol;
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{TcpStream, UnixStream};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use tower::service_fn;
use zeroize::{Zeroize, Zeroizing};

trait BridgeIo: tokio::io::AsyncRead + tokio::io::AsyncWrite {}
impl<T> BridgeIo for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite {}
type BoxedBridgeIo = Box<dyn BridgeIo + Unpin + Send>;

#[derive(Deserialize)]
struct ServerConfig {
    cert: String,
    token: String,
    #[serde(default, rename = "fileSocketPath")]
    socket: PathBuf,
    #[serde(default)]
    port: u16,
}

pub struct Connection {
    pub client: BridgeClient<Channel>,
    token: MetadataValue<Ascii>,
}

pub fn config_path() -> Result<PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("Cannot locate your user configuration directory.")?;
    Ok(config.join("protonmail/bridge-v3/grpcServerConfig.json"))
}

fn read_config(path: &Path) -> Result<ServerConfig, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "Bridge is not running. Start Bridge, then reconnect.".to_owned())?;
    validate_file(&file)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.by_ref()
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read Bridge's local connection settings.")?;
    if bytes.len() > 65_536 {
        return Err("Bridge's local connection settings are too large.".into());
    }
    let config: ServerConfig = serde_json::from_slice(&bytes)
        .map_err(|_| "Bridge's connection settings are invalid. Restart Bridge.".to_owned())?;
    if config.cert.is_empty() || config.token.is_empty() {
        return Err("Bridge's local certificate or authentication token is missing.".into());
    }
    if !config.socket.as_os_str().is_empty() && !config.socket.is_absolute() {
        return Err("Bridge's local socket path must be absolute.".into());
    }
    if config.socket.as_os_str().is_empty() && config.port == 0 {
        return Err("Bridge has not provided a local socket or port.".into());
    }
    Ok(config)
}

fn validate_file(file: &File) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect Bridge's connection settings.")?;
    // Never use another user's file or a device/pipe as authentication material.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err("Bridge's connection settings must be a regular file owned by you.".into());
    }
    Ok(())
}

impl Connection {
    pub async fn connect(path: &Path) -> Result<Self, String> {
        let mut config = read_config(path)?;
        let token = config
            .token
            .parse()
            .map_err(|_| "Bridge's local authentication token is invalid.")?;
        config.token.zeroize();
        // Bridge's local certificate is self-signed and marked as a CA by its
        // Go TLS server. Use OpenSSL's normal certificate/hostname validation
        // with only this certificate as a trust root; do not use system roots.
        let certificate = native_tls::Certificate::from_pem(config.cert.as_bytes())
            .map_err(|_| "Bridge's local certificate is invalid.")?;
        config.cert.zeroize();
        let mut tls_builder = native_tls::TlsConnector::builder();
        tls_builder
            .disable_built_in_roots(true)
            .add_root_certificate(certificate)
            .min_protocol_version(Some(Protocol::Tlsv12))
            .request_alpns(&["h2"]);
        let tls = tokio_native_tls::TlsConnector::from(
            tls_builder
                .build()
                .map_err(|_| "Cannot configure Bridge's local TLS connection.")?,
        );
        let port = config.port.max(1);
        let endpoint = Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
            .map_err(|_| "Invalid local Bridge endpoint.")?
            .connect_timeout(Duration::from_secs(3));
        let socket_path = config.socket;
        // `connect_timeout` bounds the gRPC handshake, but a local socket
        // connector can still wait on its own. Bound the whole transport setup
        // so the GUI can always return to its retry state.
        let connect = endpoint.connect_with_connector(service_fn(move |_| {
            let socket = socket_path.clone();
            let tls = tls.clone();
            async move {
                let stream: BoxedBridgeIo = if socket.as_os_str().is_empty() {
                    Box::new(TcpStream::connect(("127.0.0.1", port)).await?)
                } else {
                    Box::new(UnixStream::connect(socket).await?)
                };
                let stream = tls
                    .connect("127.0.0.1", stream)
                    .await
                    .map_err(std::io::Error::other)?;
                let alpn = stream
                    .get_ref()
                    .negotiated_alpn()
                    .map_err(std::io::Error::other)?;
                if alpn.as_deref() != Some(b"h2") {
                    return Err(std::io::Error::other("Bridge did not negotiate HTTP/2"));
                }
                Ok(TokioIo::new(stream))
            }
        }));
        let channel = tokio::time::timeout(Duration::from_secs(5), connect)
            .await
            .map_err(
                |_| "Timed out connecting to Bridge. Start or restart Bridge, then reconnect.",
            )?
            .map_err(
                |_| "Cannot connect securely to Bridge. Start or restart Bridge, then reconnect.",
            )?;
        Ok(Self {
            client: BridgeClient::new(channel),
            token,
        })
    }

    pub fn request<T>(&self, value: T) -> Request<T> {
        let mut request = Request::new(value);
        request
            .metadata_mut()
            .insert("server-token", self.token.clone());
        request
    }

    pub fn unary<T>(&self, value: T) -> Request<T> {
        let mut request = self.request(value);
        request.set_timeout(Duration::from_secs(10));
        request
    }

    pub async fn login(
        &mut self,
        method: LoginMethod,
        username: String,
        secret: Zeroizing<String>,
    ) -> Result<(), Status> {
        let encoded = Zeroizing::new(STANDARD.encode(secret.as_bytes()));
        let request = self.unary(protocol::LoginRequest {
            username,
            password: encoded.as_bytes().to_vec(),
            use_hv_details: Some(false),
        });
        match method {
            LoginMethod::Password => self.client.login(request).await,
            LoginMethod::TwoFactor => self.client.login2_fa(request).await,
            LoginMethod::MailboxPassword => self.client.login2_passwords(request).await,
            LoginMethod::SecurityKey => self.client.login_fido(request).await,
        }?;
        Ok(())
    }

    pub async fn stop_stream(&mut self) -> Result<(), Status> {
        let request = self.unary(protocol::Empty {});
        self.client.stop_event_stream(request).await?;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    Password,
    TwoFactor,
    MailboxPassword,
    SecurityKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_untrusted_or_invalid_configuration() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let file = directory.path().join("config.json");
        std::fs::write(
            &file,
            r#"{"cert":"test","token":"test","fileSocketPath":"relative"}"#,
        )
        .unwrap();
        assert!(read_config(&file).is_err());
        std::fs::write(&file, r#"{"cert":"test","token":"test","port":9000}"#).unwrap();
        assert!(read_config(&file).is_ok());
        let link = directory.path().join("symlink");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_config(&link).is_err());
        std::fs::write(&file, vec![b' '; 65_537]).unwrap();
        assert!(read_config(&file).is_err());
    }
}
