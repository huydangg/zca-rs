//! QR login against id.zalo.me — port of zca-js `apis/loginQR.js`.
//!
//! Plain HTTP + cookies, no crypto: load login page (scrape version) →
//! logininfo → verify-client → qr/generate → long-poll waiting-scan →
//! waiting-confirm → checksession → jr/userinfo. On success the cookie jar
//! holds the web session.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::error::{brief, Error, Result};
use crate::http::{doc_headers, xhr_headers, Http};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::Value;
use std::time::Duration;

const ID: &str = "https://id.zalo.me";
const REF_PC: &str = "https://id.zalo.me/account?continue=https%3A%2F%2Fzalo.me%2Fpc";
const REF_CHAT: &str = "https://id.zalo.me/account?continue=https%3A%2F%2Fchat.zalo.me%2F";
/// Same expiry zca-js uses before regenerating the code.
const QR_TTL: Duration = Duration::from_secs(100);
const MAX_QR_ATTEMPTS: usize = 5;

/// What happens during [`Client::login_qr`](crate::Client::login_qr), in order: a QR is
/// `Generated` (show it; scan it with the Zalo app), someone `Scanned` it (confirm on the
/// phone) or it `Expired` (a new one follows, up to five times).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QrEvent {
    /// A fresh QR code to show.
    Generated {
        /// The QR as a PNG image.
        png: Vec<u8>,
        /// The same image as a `data:image/png;base64,...` URL (for a web view or `<img>`).
        data_url: String,
        /// How long the code can be scanned; after that comes [`QrEvent::Expired`].
        expires_in: Duration,
    },
    /// The QR was scanned by this account; the user still has to confirm on the phone.
    Scanned {
        /// Display name of the scanning account.
        display_name: String,
    },
    /// The code timed out unscanned; a new [`QrEvent::Generated`] follows.
    Expired,
}

fn ec(v: &Value) -> Option<i64> {
    v.get("error_code").and_then(Value::as_i64)
}

async fn post_form(
    http: &Http,
    path: &str,
    form: &[(&str, &str)],
    referer: &'static str,
) -> Result<Value> {
    let res = http
        .client
        .post(format!("{ID}{path}"))
        .headers(xhr_headers(referer))
        .form(form)
        .send()
        .await?
        .error_for_status()?;
    Ok(res.json::<Value>().await?)
}

async fn load_login_version(http: &Http) -> Result<String> {
    let html = http
        .client
        .get(format!(
            "{ID}/account?continue=https%3A%2F%2Fchat.zalo.me%2F"
        ))
        .headers(doc_headers("https://chat.zalo.me/"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let re = regex::Regex::new(r"https://stc-zlogin\.zdn\.vn/main-([\d.]+)\.js")?;
    re.captures(&html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| Error::protocol("cannot find login version on id.zalo.me (page layout changed?)"))
}

/// Long-poll endpoint: server answers `error_code == 8` while still waiting.
async fn poll(http: &Http, path: &str, form: &[(&str, &str)]) -> Result<Value> {
    loop {
        let v = post_form(http, path, form, REF_CHAT).await?;
        if ec(&v) != Some(8) {
            return Ok(v);
        }
    }
}

async fn wait_scan_and_confirm(
    http: &Http,
    version: &str,
    code: &str,
    on: &mut impl FnMut(QrEvent),
) -> Result<String> {
    let scan = poll(
        http,
        "/account/authen/qr/waiting-scan",
        &[
            ("code", code),
            ("continue", "https://chat.zalo.me/"),
            ("v", version),
        ],
    )
    .await?;
    let data = scan
        .get("data")
        .filter(|d| !d.is_null())
        .ok_or_else(|| Error::Auth(format!("scan failed: {}", brief(&scan))))?;
    let name = data["display_name"].as_str().unwrap_or("?").to_string();
    on(QrEvent::Scanned {
        display_name: name.clone(),
    });

    let confirm = poll(
        http,
        "/account/authen/qr/waiting-confirm",
        &[
            ("code", code),
            ("gToken", ""),
            ("gAction", "CONFIRM_QR"),
            ("continue", "https://chat.zalo.me/"),
            ("v", version),
        ],
    )
    .await?;
    match ec(&confirm) {
        Some(0) => Ok(name),
        Some(-13) => Err(Error::Auth("login was declined on the phone".into())),
        _ => Err(Error::Auth(format!("confirm failed: {}", brief(&confirm)))),
    }
}

/// Run the whole QR flow against `http` (an empty jar); on success the jar holds the web session.
pub(crate) async fn login_qr(http: &Http, mut on: impl FnMut(QrEvent)) -> Result<()> {
    let version = load_login_version(http).await?;
    let v = version.as_str();
    post_form(
        http,
        "/account/logininfo",
        &[("continue", "https://zalo.me/pc"), ("v", v)],
        REF_PC,
    )
    .await?;
    post_form(
        http,
        "/account/verify-client",
        &[
            ("type", "device"),
            ("continue", "https://zalo.me/pc"),
            ("v", v),
        ],
        REF_PC,
    )
    .await?;

    let mut scanned = false;
    for _ in 0..MAX_QR_ATTEMPTS {
        let generated = post_form(
            http,
            "/account/authen/qr/generate",
            &[("continue", "https://zalo.me/pc"), ("v", v)],
            REF_PC,
        )
        .await?;
        let data = generated
            .get("data")
            .filter(|d| !d.is_null())
            .ok_or_else(|| Error::protocol(format!("QR generate failed: {}", brief(&generated))))?;
        let code = data["code"]
            .as_str()
            .ok_or_else(|| Error::protocol("QR without code"))?
            .to_string();
        let image = data["image"].as_str().unwrap_or_default();
        let b64 = image.trim_start_matches("data:image/png;base64,");
        let png = B64.decode(b64)?;
        on(QrEvent::Generated { png, data_url: format!("data:image/png;base64,{b64}"), expires_in: QR_TTL });

        match tokio::time::timeout(QR_TTL, wait_scan_and_confirm(http, v, &code, &mut on)).await {
            Err(_) => on(QrEvent::Expired),
            Ok(r) => {
                r?;
                scanned = true;
                break;
            }
        }
    }
    if !scanned {
        return Err(Error::Auth(format!("QR expired {MAX_QR_ATTEMPTS} times, giving up")));
    }

    // Establishes the chat.zalo.me session cookies (follows redirects).
    http.client
        .get(format!(
            "{ID}/account/checksession?continue=https%3A%2F%2Fchat.zalo.me%2Findex.html"
        ))
        .headers(doc_headers(REF_CHAT))
        .send()
        .await?;

    let mut h = xhr_headers("https://chat.zalo.me/");
    h.insert(
        "sec-fetch-site",
        reqwest::header::HeaderValue::from_static("same-site"),
    );
    let info: Value = http
        .client
        .get("https://jr.chat.zalo.me/jr/userinfo")
        .headers(h)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if info["data"]["logged"].as_bool() != Some(true) {
        return Err(Error::Auth(format!("session not logged in after QR: {}", brief(&info))));
    }
    Ok(())
}
