//! `ParamsEncryptor` — the per-session key agreement for Zalo web requests.
//! Ported from zca-js `ParamsEncryptor` (dist/utils.js).
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::crypto::{self, Encoding};
use crate::error::Result;
use rand::Rng;

/// The fixed seed Zalo uses to derive the session `zcid`.
const ZCID_SEED: &str = "3FC4F0D2AB50057BCE0D90D9187A22B1";

#[derive(Debug, Clone)]
pub(crate) struct ParamsEncryptor {
    pub zcid: String,
    pub zcid_ext: String,
    pub enc_ver: &'static str,
    pub encrypt_key: String,
}

impl ParamsEncryptor {
    /// `type` is the client type code, `imei` the device id, `first_launch_time`
    /// the app's first-launch epoch millis — together they seed the session.
    pub fn new(client_type: u32, imei: &str, first_launch_time: i64) -> Result<Self> {
        let msg = format!("{client_type},{imei},{first_launch_time}");
        let zcid = crypto::pe_encode_aes(ZCID_SEED, &msg, Encoding::Hex, true)?;
        let zcid_ext = random_hex_string();
        let encrypt_key = crypto::derive_encrypt_key(&zcid, &zcid_ext);
        Ok(Self {
            zcid,
            zcid_ext,
            enc_ver: "v2",
            encrypt_key,
        })
    }

    /// Query params attached to the login/handshake request.
    pub fn params(&self) -> [(&'static str, String); 3] {
        [
            ("zcid", self.zcid.clone()),
            ("zcid_ext", self.zcid_ext.clone()),
            ("enc_ver", self.enc_ver.to_string()),
        ]
    }
}

/// `ParamsEncryptor.randomString` — a random lowercase-hex string, length 6..=12.
fn random_hex_string() -> String {
    let mut rng = rand::thread_rng();
    let len: usize = rng.gen_range(6..=12);
    let mut s = String::with_capacity(len);
    while s.len() < len {
        // random hex nibble
        let n: u8 = rng.gen_range(0..16);
        s.push(std::char::from_digit(n as u32, 16).unwrap());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_is_internally_consistent() {
        let pe = ParamsEncryptor::new(1, "TESTIMEI123", 1_700_000_000_000).unwrap();
        // zcid is 64 hex chars (AES block-padded 27-byte message → 32 bytes → 64 hex)
        assert_eq!(pe.zcid.len(), 64);
        assert_eq!(pe.encrypt_key.len(), 32);
        assert_eq!(pe.enc_ver, "v2");
        // ext length within documented bounds
        assert!((6..=12).contains(&pe.zcid_ext.len()));
    }
}
