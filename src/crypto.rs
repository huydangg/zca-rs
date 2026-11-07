//! Zalo web-protocol crypto primitives.
//!
//! Ported 1:1 from zca-js (`dist/utils.js`) so the handshake, request-param
//! encryption and response decryption stay byte-compatible with the official
//! Zalo web client. Every primitive here is AES-CBC/PKCS7 with a **zero IV**
//! (CryptoJS default when an all-zero IV WordArray is passed).
//!
//! Reference vectors in the tests below were produced from zca-js's own
//! CryptoJS code, so a green `cargo test` proves the port is faithful.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use aes::{Aes128, Aes192, Aes256};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::{Decryptor, Encryptor};
use cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use md5::{Digest, Md5};

const ZERO_IV: [u8; 16] = [0u8; 16];

#[derive(Debug, thiserror::Error)]
pub(crate) enum CryptoError {
    #[error("unsupported AES key length: {0} bytes (want 16/24/32)")]
    KeyLen(usize),
    #[error("cipher init failed")]
    Init,
    #[error("PKCS7 unpad / decrypt failed")]
    Unpad,
    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("utf8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

type Result<T> = std::result::Result<T, CryptoError>;

/// How to render ciphertext bytes to a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Hex,
    Base64,
}

fn cbc_encrypt(key: &[u8], iv: &[u8; 16], pt: &[u8]) -> Result<Vec<u8>> {
    Ok(match key.len() {
        16 => Encryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::Init)?
            .encrypt_padded_vec_mut::<Pkcs7>(pt),
        24 => Encryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::Init)?
            .encrypt_padded_vec_mut::<Pkcs7>(pt),
        32 => Encryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::Init)?
            .encrypt_padded_vec_mut::<Pkcs7>(pt),
        n => return Err(CryptoError::KeyLen(n)),
    })
}

fn cbc_decrypt(key: &[u8], iv: &[u8; 16], ct: &[u8]) -> Result<Vec<u8>> {
    let out = match key.len() {
        16 => Decryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::Init)?
            .decrypt_padded_vec_mut::<Pkcs7>(ct),
        24 => Decryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::Init)?
            .decrypt_padded_vec_mut::<Pkcs7>(ct),
        32 => Decryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::Init)?
            .decrypt_padded_vec_mut::<Pkcs7>(ct),
        n => return Err(CryptoError::KeyLen(n)),
    };
    out.map_err(|_| CryptoError::Unpad)
}

fn render(bytes: &[u8], enc: Encoding, uppercase: bool) -> String {
    let s = match enc {
        Encoding::Hex => hex::encode(bytes),
        Encoding::Base64 => B64.encode(bytes),
    };
    if uppercase {
        s.to_uppercase()
    } else {
        s
    }
}

/// `ParamsEncryptor.encodeAES` — key is the **UTF-8 bytes** of `key_str`
/// (e.g. the 32-char zcid seed → AES-256). Used to build `zcid`.
pub(crate) fn pe_encode_aes(
    key_str: &str,
    message: &str,
    enc: Encoding,
    uppercase: bool,
) -> Result<String> {
    let ct = cbc_encrypt(key_str.as_bytes(), &ZERO_IV, message.as_bytes())?;
    Ok(render(&ct, enc, uppercase))
}

/// Module-level `encodeAES` — `secret_b64` is a **base64-encoded** key, output
/// is base64 ciphertext. This is the per-request param encryption (session key).
pub(crate) fn encode_aes(secret_b64: &str, data: &str) -> Result<String> {
    let key = B64.decode(secret_b64)?;
    let ct = cbc_encrypt(&key, &ZERO_IV, data.as_bytes())?;
    Ok(B64.encode(ct))
}

/// Module-level `decodeAES` — inverse of [`encode_aes`]. `data` is URI-encoded
/// base64 (callers should URL-decode first; we accept already-decoded here).
pub(crate) fn decode_aes(secret_b64: &str, data_b64: &str) -> Result<String> {
    let key = B64.decode(secret_b64)?;
    let ct = B64.decode(data_b64)?;
    let pt = cbc_decrypt(&key, &ZERO_IV, &ct)?;
    Ok(String::from_utf8(pt)?)
}

/// `decodeRespAES` — login/response decryption where the key is the **UTF-8
/// bytes** of a derived key string (see [`derive_encrypt_key`]).
pub(crate) fn decode_resp_aes(key_str: &str, data_b64: &str) -> Result<String> {
    let ct = B64.decode(data_b64)?;
    let pt = cbc_decrypt(key_str.as_bytes(), &ZERO_IV, &ct)?;
    Ok(String::from_utf8(pt)?)
}

/// `decryptResp` — server payloads arrive as `encodeURIComponent(base64(ct))`.
/// Percent-decode (no `+`→space, like `decodeURIComponent`), then AES-decrypt
/// with the UTF-8 key string.
pub(crate) fn decrypt_resp(key_str: &str, wire: &str) -> Result<String> {
    let b64 = percent_encoding::percent_decode_str(wire)
        .decode_utf8()
        .map_err(|_| CryptoError::Unpad)?;
    decode_resp_aes(key_str, &b64)
}

/// MD5 of `s`, lowercase hex (matches `cryptojs.MD5(x).toString()`).
pub(crate) fn md5_hex(s: &str) -> String {
    hex::encode(Md5::digest(s.as_bytes()))
}

/// MD5 of `s`, hex, uppercase (matches `cryptojs.MD5(x).toString().toUpperCase()`).
pub(crate) fn md5_upper(s: &str) -> String {
    md5_hex(s).to_uppercase()
}

/// `getSignKey(type, params)` — `md5("zsecure" + type + values-in-sorted-key-order)`.
/// Every request to the `wpa`/API hosts carries this as `signkey`.
pub(crate) fn sign_key(kind: &str, params: &[(&str, String)]) -> String {
    let mut sorted: Vec<&(&str, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut s = format!("zsecure{kind}");
    for (_, v) in sorted {
        s.push_str(v);
    }
    md5_hex(&s)
}

fn chars_by_parity(s: &str, even: bool) -> Vec<char> {
    s.chars()
        .enumerate()
        .filter(|(i, _)| (i % 2 == 0) == even)
        .map(|(_, c)| c)
        .collect()
}

/// `ParamsEncryptor.createEncryptKey` — derive the response key from the
/// session `zcid` (hex) and the random `zcid_ext`. Result is a 32-char string
/// whose UTF-8 bytes form the AES-256 key used by [`decode_resp_aes`].
pub(crate) fn derive_encrypt_key(zcid: &str, zcid_ext: &str) -> String {
    let md5_ext = md5_upper(zcid_ext);
    let ne = chars_by_parity(&md5_ext, true); // even chars of md5(zcid_ext)
    let ze = chars_by_parity(zcid, true); // even chars of zcid
    let mut zo = chars_by_parity(zcid, false); // odd chars of zcid
    zo.reverse();

    let part1: String = ne.into_iter().take(8).collect();
    let part2: String = ze.into_iter().take(12).collect();
    let part3: String = zo.into_iter().take(12).collect();
    format!("{part1}{part2}{part3}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors generated from zca-js's own crypto (crypto-js). See repo history.
    const ZCID_SEED: &str = "3FC4F0D2AB50057BCE0D90D9187A22B1";
    const ZCID_MSG: &str = "1,TESTIMEI123,1700000000000";

    #[test]
    fn pe_encode_aes_hex_matches_zca_js() {
        let got = pe_encode_aes(ZCID_SEED, ZCID_MSG, Encoding::Hex, true).unwrap();
        assert_eq!(
            got,
            "9C49B8513EF830EB4F37BB7FE14E4DA56EC6DA0D71721E0ED68C164FE0BEAE6A"
        );
    }

    #[test]
    fn pe_encode_aes_base64_matches_zca_js() {
        let got = pe_encode_aes(ZCID_SEED, ZCID_MSG, Encoding::Base64, false).unwrap();
        assert_eq!(got, "nEm4UT74MOtPN7t/4U5NpW7G2g1xch4O1owWT+C+rmo=");
    }

    #[test]
    fn encode_aes_matches_zca_js_and_roundtrips() {
        // base64 of the 32-byte key "0123456789abcdef0123456789abcdef"
        let secret = B64.encode(b"0123456789abcdef0123456789abcdef");
        let enc = encode_aes(&secret, "hello zalo").unwrap();
        assert_eq!(enc, "5ykuu0VqTSLdAx8KhgjGww==");
        assert_eq!(decode_aes(&secret, &enc).unwrap(), "hello zalo");
    }

    #[test]
    fn md5_upper_matches() {
        assert_eq!(
            md5_upper("abc123def456"),
            "30CB73276DF970492B1148CC0E3F23A7"
        );
    }

    #[test]
    fn sign_key_matches_zca_js() {
        // insertion order deliberately != sorted order
        let p = [
            ("zcid", "ZC".to_string()),
            ("zcid_ext", "ext1".to_string()),
            ("enc_ver", "v2".to_string()),
            ("params", "P+/=".to_string()),
            ("type", "30".to_string()),
            ("client_version", "685".to_string()),
        ];
        assert_eq!(
            sign_key("getlogininfo", &p),
            "a3998ed425136b44466c560a169c24d6"
        );
        let s = [
            ("imei", "IMEI-1".to_string()),
            ("type", "30".to_string()),
            ("client_version", "685".to_string()),
            ("computer_name", "Web".to_string()),
        ];
        assert_eq!(
            sign_key("getserverinfo", &s),
            "f6f7b702c612f77bab03448286f55221"
        );
    }

    #[test]
    fn md5_hex_of_default_ua_matches() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0";
        assert_eq!(md5_hex(ua), "a69b52f9d7f760edf3fd052bcda2542f");
    }

    #[test]
    fn decrypt_resp_matches_zca_js() {
        let wire = "Zesvo5A08%2FYdL54pvCDpaWftZRdjyk9lihgLoPjvFq0VWAdKIUJx%2Bch4ewky%2FeiRDXsO2sLVQSfF2Y3RnyA%2BvA%3D%3D";
        let got = decrypt_resp("3C726F7494B53F3E43B7AEE0F6C6EE21", wire).unwrap();
        assert_eq!(
            got,
            r#"{"error_code":0,"data":{"uid":"123","zpw_enk":"c2VjcmV0"}}"#
        );
    }

    #[test]
    fn derive_encrypt_key_matches_zca_js() {
        let zcid = "9C49B8513EF830EB4F37BB7FE14E4DA56EC6DA0D71721E0ED68C164FE0BEAE6A";
        let key = derive_encrypt_key(zcid, "abc123def456");
        assert_eq!(key, "3C726F7494B53F3E43B7AEE0F6C6EE21");
        assert_eq!(key.len(), 32); // → AES-256 key bytes
    }
}
