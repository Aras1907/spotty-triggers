use crate::protocol::{self, bridge_client::BridgeClient};
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Status};
use tower::service_fn;
use zeroize::{Zeroize, Zeroizing};

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
        // Trust only the certificate supplied by this local Bridge instance.
        // System roots, insecure TLS, and remote endpoints are never enabled.
        let tls = ClientTlsConfig::new()
            .domain_name("127.0.0.1")
            .ca_certificate(Certificate::from_pem(config.cert));
        let endpoint = Endpoint::from_shared(format!("https://127.0.0.1:{}", config.port.max(1)))
            .map_err(|_| "Invalid local Bridge endpoint.")?
            .connect_timeout(Duration::from_secs(3))
            .tls_config(tls)
            .map_err(|_| "Cannot verify Bridge's local TLS certificate.")?;
        let channel = if config.socket.as_os_str().is_empty() {
            endpoint.connect().await
        } else {
            let socket = config.socket;
            endpoint
                .connect_with_connector(service_fn(move |_| {
                    let socket = socket.clone();
                    async move { UnixStream::connect(socket).await.map(TokioIo::new) }
                }))
                .await
        }
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
