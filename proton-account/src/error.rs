use std::fmt;

/// Everything that can go wrong talking to Proton, phrased for the UI.
#[derive(Debug, Clone)]
pub enum Error {
    /// Proton answered with an error code (`status` is the HTTP status).
    Api { status: u16, code: i64, message: String },
    /// No connection, timeout or TLS failure.
    Network(String),
    /// A key or message couldn't be opened.
    Crypto(String),
    /// Proton sent something this client doesn't understand.
    Protocol(String),
    /// The account needs something Spotty can't do natively yet.
    Unsupported(String),
    /// No saved session, or Proton ended it.
    SignedOut,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Proton's code for "prove you're human first" (a CAPTCHA that only exists
/// on its web pages).
pub const CODE_HUMAN_VERIFICATION: i64 = 9001;

impl Error {
    pub fn is_signed_out(&self) -> bool {
        matches!(self, Error::SignedOut) || matches!(self, Error::Api { status: 401, .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api { code, message, .. } if *code == CODE_HUMAN_VERIFICATION => write!(
                f,
                "Proton wants to check that you're human before it allows this sign-in ({message}). \
                 That check only exists on Proton's web page; wait a while and try again."
            ),
            Error::Api { message, .. } if !message.is_empty() => f.write_str(message),
            Error::Api { status, code, .. } => write!(f, "Proton returned an error (HTTP {status}, code {code})"),
            Error::Network(message) => write!(f, "Couldn't reach Proton: {message}"),
            Error::Crypto(message) => write!(f, "Couldn't open your encrypted data: {message}"),
            Error::Protocol(message) => write!(f, "Unexpected answer from Proton: {message}"),
            Error::Unsupported(message) => f.write_str(message),
            Error::SignedOut => f.write_str("You're not signed in to Proton"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Protocol(error.to_string())
    }
}
