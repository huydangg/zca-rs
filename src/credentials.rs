//! What a caller persists to log in again without scanning a QR: [`Credentials`] and its
//! [`CookieJar`]. The crate never writes them anywhere itself.

use crate::error::{Error, Result};
use cookie_store::CookieStore;
use serde::{Deserialize, Serialize};
use std::io::BufReader;

/// The cookies of a Zalo web session, exportable and importable as JSON.
///
/// Zalo's auth cookies are *session* cookies, so this keeps non-persistent and expired
/// cookies too (a plain browser-style save would drop exactly the ones that matter).
/// It holds secrets: treat the exported text like a password.
#[derive(Clone, Default)]
pub struct CookieJar {
    store: CookieStore,
}

impl CookieJar {
    /// An empty jar.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn from_store(store: CookieStore) -> Self {
        Self { store }
    }

    pub(crate) fn into_store(self) -> CookieStore {
        self.store
    }

    /// True when the jar holds no cookie.
    pub fn is_empty(&self) -> bool {
        self.store.iter_any().next().is_none()
    }

    /// Export every cookie as pretty JSON (an array of cookie objects).
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&self.store, &mut buf)
            .map_err(|e| Error::protocol(format!("save cookies: {e}")))?;
        Ok(buf)
    }

    /// Import what [`CookieJar::to_json`] exported.
    pub fn from_json(json: &[u8]) -> Result<Self> {
        let store = cookie_store::serde::json::load_all(BufReader::new(json))
            .map_err(|e| Error::protocol(format!("load cookies: {e}")))?;
        Ok(Self { store })
    }
}

impl std::fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never print the cookies: they are the login
        f.debug_struct("CookieJar")
            .field("cookies", &self.store.iter_any().count())
            .finish()
    }
}

impl Serialize for CookieJar {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let json = self.to_json().map_err(serde::ser::Error::custom)?;
        let value: serde_json::Value = serde_json::from_slice(&json).map_err(serde::ser::Error::custom)?;
        value.serialize(s)
    }
}

impl<'de> Deserialize<'de> for CookieJar {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        Self::from_json(value.to_string().as_bytes()).map_err(serde::de::Error::custom)
    }
}

/// Everything needed to log in with [`Client::from_credentials`](crate::Client::from_credentials):
/// the device id (`imei`), the user agent it is tied to, the language and the cookie jar.
///
/// Serializable; the caller decides where it lives. It holds secrets (the cookies).
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// The device id Zalo ties the session to; see [`generate_imei`].
    pub imei: String,
    /// The user agent the session was created with; keep sending the same one.
    pub user_agent: String,
    /// Language sent at login (`"vi"` for the Vietnamese web client).
    #[serde(default = "default_language")]
    pub language: String,
    /// The web session's cookies.
    #[serde(default)]
    pub cookies: CookieJar,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the imei identifies the device the session is tied to: keep it out of logs
        f.debug_struct("Credentials")
            .field("imei", &"<redacted>")
            .field("user_agent", &self.user_agent)
            .field("language", &self.language)
            .field("cookies", &self.cookies)
            .finish()
    }
}

fn default_language() -> String {
    "vi".into()
}

impl Credentials {
    /// Fresh credentials for the default user agent and the given cookies.
    pub fn new(cookies: CookieJar) -> Self {
        let user_agent = crate::http::DEFAULT_USER_AGENT;
        Self {
            imei: generate_imei(user_agent),
            user_agent: user_agent.into(),
            language: default_language(),
            cookies,
        }
    }

    /// Read credentials from a JSON file written by [`Credentials::save_private`].
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    /// Write the credentials as pretty JSON to `path`, readable by you only.
    ///
    /// On unix the file is created with mode `0600`, and the mode is set again after opening,
    /// so overwriting an existing, more permissive file (say `0644`) tightens it too. The
    /// file holds your session cookies: never commit it.
    pub fn save_private(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        use std::io::Write;
        let json = serde_json::to_vec_pretty(self)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path)?;
        #[cfg(unix)]
        file.set_permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600))?;
        file.write_all(&json)?;
        Ok(())
    }
}

/// zca-js `generateZaloUUID`: `uuid4 + "-" + md5(userAgent)`.
pub fn generate_imei(user_agent: &str) -> String {
    format!(
        "{}-{}",
        uuid::Uuid::new_v4(),
        crate::crypto::md5_hex(user_agent)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imei_is_a_uuid_dash_the_md5_of_the_user_agent() {
        let imei = generate_imei(crate::http::DEFAULT_USER_AGENT);
        let (id, md5) = imei.split_at(36);
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_eq!(md5, "-a69b52f9d7f760edf3fd052bcda2542f");
    }

    #[test]
    fn credentials_survive_serde_and_an_old_file_without_cookies_or_language_still_loads() {
        let c = Credentials::new(CookieJar::new());
        let back: Credentials = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!((back.imei.as_str(), back.language.as_str()), (c.imei.as_str(), "vi"));
        assert!(back.cookies.is_empty());
        let old: Credentials = serde_json::from_str(r#"{"imei":"I","user_agent":"U"}"#).unwrap();
        assert_eq!((old.language.as_str(), old.cookies.is_empty()), ("vi", true));
    }

    /// A cookie that is a session cookie (no Expires), as Zalo's auth cookies are, survives export/import.
    #[test]
    fn session_cookies_survive_the_json_export() {
        let url = reqwest::Url::parse("https://chat.zalo.me/").unwrap();
        let mut store = CookieStore::default();
        store.parse("zpsid=SECRET; Path=/; Secure", &url).unwrap();
        let jar = CookieJar::from_store(store);
        assert!(!jar.is_empty());
        let json = jar.to_json().unwrap();
        let back = CookieJar::from_json(&json).unwrap();
        assert_eq!(back.to_json().unwrap(), json);
        assert!(String::from_utf8(json).unwrap().contains("zpsid"));
        // through serde (as part of Credentials) too, and never in Debug output
        let creds = Credentials { cookies: jar, ..Credentials::new(CookieJar::new()) };
        let text = serde_json::to_string(&creds).unwrap();
        let again: Credentials = serde_json::from_str(&text).unwrap();
        assert!(!again.cookies.is_empty());
        assert!(!format!("{creds:?}").contains("SECRET"));
    }

    #[test]
    fn debug_hides_the_imei_and_the_cookies() {
        let url = reqwest::Url::parse("https://chat.zalo.me/").unwrap();
        let mut store = CookieStore::default();
        store.parse("zpsid=SECRET; Path=/; Secure", &url).unwrap();
        let creds = Credentials { imei: "IMEI-SECRET".into(), cookies: CookieJar::from_store(store), ..Credentials::new(CookieJar::new()) };
        let text = format!("{creds:?}");
        assert!(!text.contains("IMEI-SECRET") && !text.contains("SECRET"), "{text}");
        assert!(text.contains("<redacted>") && text.contains("user_agent"), "{text}");
    }

    #[test]
    fn save_private_and_load_roundtrip_with_owner_only_permissions() {
        let dir = std::env::temp_dir().join(format!("zca-rust-creds-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zalo.json");
        let creds = Credentials::new(CookieJar::new());
        creds.save_private(&path).unwrap();
        let back = Credentials::load(&path).unwrap();
        assert_eq!((back.imei.as_str(), back.user_agent.as_str()), (creds.imei.as_str(), creds.user_agent.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            // overwriting a world-readable file tightens it, and replaces the content
            let loose = dir.join("loose.json");
            std::fs::write(&loose, vec![b'x'; 10_000]).unwrap();
            std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(mode(&loose), 0o644);
            creds.save_private(&loose).unwrap();
            assert_eq!(mode(&loose), 0o600);
            assert_eq!(Credentials::load(&loose).unwrap().imei, creds.imei);
        }
        assert!(Credentials::load(dir.join("missing.json")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
