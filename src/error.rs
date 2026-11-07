//! The crate's error type.

use crate::crypto::CryptoError;

/// Everything that can go wrong talking to Zalo.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An HTTP request failed (network, TLS, a non-2xx status, an undecodable body).
    #[error("http: {0}")]
    Http(reqwest::Error),
    /// The websocket failed.
    #[error("websocket: {0}")]
    WebSocket(String),
    /// A JSON document could not be read or written.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Encrypting or decrypting a payload failed (wrong key, corrupt data).
    #[error("crypto: {0}")]
    Crypto(String),
    /// Reading or writing a file failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Zalo refused the login: the cookies are expired or the QR login was declined.
    #[error("{0}")]
    Auth(String),
    /// Zalo answered an API call with an error code.
    #[error("Zalo API error {code}: {message}")]
    Api {
        /// The `error_code` of the answer.
        code: i64,
        /// The `error_message` of the answer (may be empty).
        message: String,
    },
    /// Zalo answered with something this crate does not understand (the protocol may have changed).
    #[error("{0}")]
    Protocol(String),
    /// The caller passed something that cannot be sent (an empty id, an unknown icon, ...).
    #[error("{0}")]
    Invalid(String),
    /// A QR image could not be read, sampled or drawn (the `qr` feature).
    #[error("QR: {0}")]
    Qr(String),
    /// A call that needs the realtime listener was made while none is running (see [`crate::Client::listen`]).
    #[error("the realtime listener is not running")]
    NotListening,
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The request URL carries the session parameters (`zcid`, `zcid_ext`, `imei`, ...) in its
/// query, so it is stripped before the error can reach a log or a screen.
impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e.without_url())
    }
}

impl From<CryptoError> for Error {
    fn from(e: CryptoError) -> Self {
        Error::Crypto(e.to_string())
    }
}

impl From<reqwest::header::InvalidHeaderValue> for Error {
    fn from(e: reqwest::header::InvalidHeaderValue) -> Self {
        Error::Invalid(format!("invalid header value: {e}"))
    }
}

impl From<base64::DecodeError> for Error {
    fn from(e: base64::DecodeError) -> Self {
        Error::Protocol(format!("bad base64: {e}"))
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Protocol(format!("bad utf-8: {e}"))
    }
}

impl From<regex::Error> for Error {
    fn from(e: regex::Error) -> Self {
        Error::Protocol(format!("bad pattern: {e}"))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Error::WebSocket(e.to_string())
    }
}

/// Only the `error_code` / `error_message` of a server answer, never the whole document
/// (it can carry account details).
pub(crate) fn brief(v: &serde_json::Value) -> String {
    let code = v.get("error_code").map_or_else(|| "none".to_string(), |c| c.to_string());
    let msg = v.get("error_message").and_then(|m| m.as_str()).unwrap_or_default();
    format!("error_code {code}, error_message {msg:?}")
}

impl Error {
    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    pub(crate) fn bad_url(e: impl std::fmt::Display) -> Self {
        Error::Protocol(format!("bad url: {e}"))
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_errors_do_not_print_the_url_query() {
        // a scheme the client refuses before any connection: no network involved, URL attached
        let raw = reqwest::Client::new()
            .get("ftp://example.invalid/getLoginInfo?zcid=SECRETZCID&imei=SECRETIMEI")
            .send()
            .await
            .unwrap_err();
        assert!(raw.to_string().contains("SECRETZCID"), "the test needs an error that has a URL: {raw}");
        let e = Error::from(raw);
        let text = format!("{e} / {e:?}");
        assert!(!text.contains("SECRET") && !text.contains("example.invalid"), "{text}");
    }

    #[test]
    fn brief_keeps_only_the_code_and_the_message() {
        let v = serde_json::json!({"error_code": 114, "error_message": "bad", "data": {"display_name": "SECRETNAME", "zpw_enk": "KEY"}});
        let text = brief(&v);
        assert_eq!(text, "error_code 114, error_message \"bad\"");
        assert_eq!(brief(&serde_json::json!({"data": "SECRET"})), "error_code none, error_message \"\"");
    }
}
