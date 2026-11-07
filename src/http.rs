//! HTTP client with a persistable cookie jar (Zalo auth lives in cookies).

use crate::credentials::CookieJar;
use crate::error::{Error, Result};
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, ORIGIN, REFERER,
};
use reqwest_cookie_store::CookieStoreMutex;
use std::sync::Arc;

/// The user agent zca-js sends by default. Zalo ties the session `imei` to it, so a
/// [`Credentials`](crate::Credentials) keeps the one it was created with.
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0";

/// A `reqwest` client dressed like the official web client, sharing one cookie jar.
pub(crate) struct Http {
    pub client: reqwest::Client,
    jar: Arc<CookieStoreMutex>,
}

impl Http {
    pub fn new(user_agent: &str, cookies: CookieJar) -> Result<Self> {
        let jar = Arc::new(CookieStoreMutex::new(cookies.into_store()));
        let mut h = HeaderMap::new();
        h.insert(
            ACCEPT_LANGUAGE,
            HeaderValue::from_static("vi-VN,vi;q=0.9,en-US;q=0.6,en;q=0.5"),
        );
        h.insert(ORIGIN, HeaderValue::from_static("https://chat.zalo.me"));
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .default_headers(h)
            .cookie_provider(jar.clone())
            .build()?;
        Ok(Self { client, jar })
    }

    /// A copy of the whole jar, *including* session (non-persistent) cookies; Zalo's auth
    /// cookies are session cookies, so a plain persistent-only save would drop them.
    pub fn cookies(&self) -> Result<CookieJar> {
        let store = self
            .jar
            .lock()
            .map_err(|_| Error::protocol("cookie jar poisoned"))?;
        Ok(CookieJar::from_store(store.clone()))
    }

    /// `Cookie:` header value for `url` (websocket handshakes need it by hand).
    pub fn cookie_header(&self, url: &str) -> Result<String> {
        let url = reqwest::Url::parse(url).map_err(Error::bad_url)?;
        let store = self
            .jar
            .lock()
            .map_err(|_| Error::protocol("cookie jar poisoned"))?;
        Ok(store
            .get_request_values(&url)
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// Headers for XHR-style calls (form POSTs / JSON GETs).
pub(crate) fn xhr_headers(referer: &'static str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(ACCEPT, HeaderValue::from_static("*/*"));
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    h.insert("sec-fetch-dest", HeaderValue::from_static("empty"));
    h.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
    h.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    h.insert(REFERER, HeaderValue::from_static(referer));
    h
}

/// Headers for top-level page navigations.
pub(crate) fn doc_headers(referer: &'static str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    h.insert("sec-fetch-dest", HeaderValue::from_static("document"));
    h.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
    h.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
    h.insert(REFERER, HeaderValue::from_static(referer));
    h
}

/// Headers for the chat API hosts (`wpa.chat.zalo.me`, ...).
pub(crate) fn api_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    h.insert(REFERER, HeaderValue::from_static("https://chat.zalo.me/"));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_headers_look_like_the_web_client() {
        let h = api_headers();
        assert_eq!(h[REFERER], "https://chat.zalo.me/");
        assert_eq!(h[CONTENT_TYPE], "application/x-www-form-urlencoded");
        assert_eq!(h[ACCEPT], "application/json, text/plain, */*");
    }

    #[test]
    fn the_jar_round_trips_through_the_http_client() {
        let http = Http::new(DEFAULT_USER_AGENT, CookieJar::default()).unwrap();
        assert_eq!(http.cookie_header("https://chat.zalo.me").unwrap(), "");
        assert!(http.cookies().unwrap().is_empty());
    }
}
