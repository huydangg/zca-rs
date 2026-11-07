//! Cookie login → API session. Port of zca-js `apis/login.js` +
//! `Zalo.loginCookie`. Run on every start: it trades the stored web cookies for
//! a fresh `secret_key` (`zpw_enk`) and the websocket/service endpoints.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::crypto::{self, Encoding};
use crate::error::{brief, Error, Result};
use crate::http::{api_headers, Http};
use crate::params::ParamsEncryptor;
use crate::Credentials;
use serde::Serialize;
use serde_json::Value;

/// `zpw_type` / `type`: the web client identity (zca-js `createContext` default).
pub const API_TYPE: u32 = 30;
/// `zpw_ver` / `client_version`: the web client version zca-js mirrors.
pub const API_VERSION: u32 = 685;
const WPA: &str = "https://wpa.chat.zalo.me/api/login";

#[derive(Clone)]
pub(crate) struct Session {
    pub uid: String,
    /// base64 AES key for all subsequent API params/responses. Never log it.
    pub secret_key: String,
    pub ws_urls: Vec<String>,
    pub service_map: Value,
    pub settings: Value,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("uid", &self.uid)
            .field("secret_key", &"<redacted>")
            .field("ws_urls", &self.ws_urls)
            .finish_non_exhaustive()
    }
}

/// Field order matters: it is the plaintext the server decrypts.
#[derive(Serialize)]
struct LoginData<'a> {
    computer_name: &'a str,
    imei: &'a str,
    language: &'a str,
    ts: i64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

async fn get_login_info(http: &Http, creds: &Credentials) -> Result<Value> {
    let ts = now_ms();
    let pe = ParamsEncryptor::new(API_TYPE, &creds.imei, ts)?;
    let plain = serde_json::to_string(&LoginData {
        computer_name: "Web",
        imei: &creds.imei,
        language: &creds.language,
        ts,
    })?;
    let encrypted = crypto::pe_encode_aes(&pe.encrypt_key, &plain, Encoding::Base64, false)?;

    let mut params: Vec<(&str, String)> = pe.params().into_iter().collect();
    params.push(("params", encrypted));
    params.push(("type", API_TYPE.to_string()));
    params.push(("client_version", API_VERSION.to_string()));
    let sign = crypto::sign_key("getlogininfo", &params);
    params.push(("signkey", sign));
    params.push(("nretry", "0".into()));
    params.push(("zpw_ver", API_VERSION.to_string()));
    params.push(("zpw_type", API_TYPE.to_string()));

    let url = reqwest::Url::parse_with_params(&format!("{WPA}/getLoginInfo"), &params).map_err(Error::bad_url)?;
    let res: Value = http
        .client
        .get(url)
        .headers(api_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let wire = res["data"].as_str().ok_or_else(|| {
        Error::Auth(format!("getLoginInfo rejected (cookies expired? log in again): {}", brief(&res)))
    })?;
    let decrypted: Value = serde_json::from_str(&crypto::decrypt_resp(&pe.encrypt_key, wire)?)?;
    if decrypted["error_code"].as_i64() != Some(0) {
        return Err(Error::Auth(format!("getLoginInfo error: {}", brief(&decrypted))));
    }
    Ok(decrypted["data"].clone())
}

async fn get_server_info(http: &Http, creds: &Credentials) -> Result<Value> {
    let mut params: Vec<(&str, String)> = vec![
        ("imei", creds.imei.clone()),
        ("type", API_TYPE.to_string()),
        ("client_version", API_VERSION.to_string()),
        ("computer_name", "Web".into()),
    ];
    let sign = crypto::sign_key("getserverinfo", &params);
    params.push(("signkey", sign));
    let url = reqwest::Url::parse_with_params(&format!("{WPA}/getServerInfo"), &params).map_err(Error::bad_url)?;
    let res: Value = http
        .client
        .get(url)
        .headers(api_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let data = res["data"].clone();
    if data.is_null() {
        return Err(Error::protocol(format!("getServerInfo error: {}", brief(&res))));
    }
    Ok(data)
}

pub(crate) async fn login_cookie(http: &Http, creds: &Credentials) -> Result<Session> {
    let info = get_login_info(http, creds).await?;
    let server = get_server_info(http, creds).await?;

    let secret_key = info["zpw_enk"]
        .as_str()
        .ok_or_else(|| Error::protocol("login info without zpw_enk"))?
        .to_string();
    let uid = match &info["uid"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => return Err(Error::protocol(format!("login info without a usable uid ({})", match other { Value::Null => "missing", _ => "not a string or number" }))),
    };
    let ws_urls = info["zpw_ws"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // Zalo spells it "setttings" today; keep the fallback in case they fix it.
    let settings = if server["setttings"].is_null() {
        server["settings"].clone()
    } else {
        server["setttings"].clone()
    };

    Ok(Session {
        uid,
        secret_key,
        ws_urls,
        service_map: info["zpw_service_map_v3"].clone(),
        settings,
    })
}
