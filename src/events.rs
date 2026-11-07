//! Zalo websocket framing + event payload decoding.
//! Port of zca-js `apis/listen.js` (`getHeader`, `sendWs`) and
//! `utils.js::decodeEventData`.
//!
//! Frame: `[version u8][cmd u16 LE][subCmd u8]` + UTF-8 JSON.
//! Payload `{"encrypt": n, "data": "..."}`:
//! - 0: plain JSON
//! - 1: base64 → zlib → JSON
//! - 2: uri-encoded base64 → AES-GCM → zlib → JSON
//! - 3: uri-encoded base64 → AES-GCM → JSON
//!
//! AES-GCM uses a 16-byte IV (bytes 0..16), 16-byte AAD (16..32), then
//! ciphertext‖tag; the key is the base64 `cipher_key` the server sends first.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::aes::{Aes128, Aes256};
use aes_gcm::{AesGcm, Nonce};
use crate::error::{Error, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::Value;
use std::io::Read;

pub(crate) mod cmd {
    pub(crate) const CIPHER_KEY: u16 = 1; // v1 sub 1, {"key": ...}
    pub(crate) const PING: u16 = 2; // v1 sub 1
    pub(crate) const USER_MSGS: u16 = 501;
    pub(crate) const USER_SEEN: u16 = 502;
    pub(crate) const OLD_USER_MSGS: u16 = 510; // sub 1
    pub(crate) const OLD_GROUP_MSGS: u16 = 511; // sub 1
    pub(crate) const GROUP_MSGS: u16 = 521;
    pub(crate) const GROUP_SEEN: u16 = 522;
    pub(crate) const CONTROLS: u16 = 601; // v1 sub 0: file_done, group events, ...
    pub(crate) const REACTIONS: u16 = 612;
    pub(crate) const DUPLICATE: u16 = 3000;
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Frame {
    pub version: u8,
    pub cmd: u16,
    pub sub: u8,
    pub body: Value,
}

/// `None` for empty bodies (keep-alives).
pub(crate) fn parse_frame(bytes: &[u8]) -> Result<Option<Frame>> {
    if bytes.len() < 4 {
        return Err(Error::protocol(format!("frame shorter than header ({} bytes)", bytes.len())));
    }
    let version = bytes[0];
    let cmd = u16::from_le_bytes([bytes[1], bytes[2]]);
    let sub = bytes[3];
    let rest = &bytes[4..];
    if rest.is_empty() {
        return Ok(None);
    }
    let body = serde_json::from_slice(rest)
        .map_err(|e| Error::protocol(format!("frame cmd {cmd}: body is not JSON: {e}")))?;
    Ok(Some(Frame {
        version,
        cmd,
        sub,
        body,
    }))
}

pub(crate) fn encode_frame(version: u8, cmd: u16, sub: u8, body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).expect("json");
    let mut out = Vec::with_capacity(4 + json.len());
    out.push(version);
    out.extend_from_slice(&cmd.to_le_bytes());
    out.push(sub);
    out.extend_from_slice(&json);
    out
}

fn gcm_decrypt(key: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 48 {
        return Err(Error::protocol(format!("encrypted event too short ({} bytes)", blob.len())));
    }
    let (iv, rest) = blob.split_at(16);
    let (aad, ct) = rest.split_at(16);
    let nonce = Nonce::<U16>::from_slice(iv);
    let payload = Payload { msg: ct, aad };
    let out = match key.len() {
        32 => AesGcm::<Aes256, U16>::new_from_slice(key)
            .map_err(|_| Error::Crypto("bad key".into()))?
            .decrypt(nonce, payload),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .map_err(|_| Error::Crypto("bad key".into()))?
            .decrypt(nonce, payload),
        n => return Err(Error::Crypto(format!("unsupported cipher key length {n}"))),
    };
    out.map_err(|_| Error::Crypto("AES-GCM authentication failed (wrong cipher key?)".into()))
}

/// Like pako.inflate (what zca-js uses): zlib, gzip (auto-detected by pako)
/// or raw deflate.
fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if data.starts_with(&[0x1f, 0x8b]) {
        if flate2::read::MultiGzDecoder::new(data).read_to_end(&mut out).is_ok() {
            return Ok(out);
        }
        out.clear();
    }
    if flate2::read::ZlibDecoder::new(data).read_to_end(&mut out).is_ok() {
        return Ok(out);
    }
    out.clear();
    if flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).is_ok() {
        return Ok(out);
    }
    let head: String = data.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Err(Error::protocol(format!("inflate event payload: not zlib/gzip/deflate ({} bytes, head {head})", data.len())))
}

/// `decodeEventData(parsed, cipherKey)`.
pub(crate) fn decode_event_data(parsed: &Value, cipher_key: Option<&str>) -> Result<Value> {
    let data = parsed["data"]
        .as_str()
        .ok_or_else(|| Error::protocol("event without string data"))?;
    let kind = parsed["encrypt"]
        .as_i64()
        .ok_or_else(|| Error::protocol("event without encrypt type"))?;
    let bytes = match kind {
        0 => return Ok(serde_json::from_str(data)?),
        1 => inflate(&B64.decode(data)?)?,
        2 | 3 => {
            let b64 = percent_encoding::percent_decode_str(data).decode_utf8()?;
            let key = B64
                .decode(cipher_key.ok_or_else(|| Error::protocol("encrypted event before cipher key"))?)?;
            let plain = gcm_decrypt(&key, &B64.decode(b64.as_bytes())?)?;
            if kind == 2 {
                inflate(&plain)?
            } else {
                plain
            }
        }
        n => return Err(Error::protocol(format!("unknown encrypt type {n}"))),
    };
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflate_accepts_gzip_zlib_and_raw_deflate() {
        use flate2::{write::{DeflateEncoder, GzEncoder, ZlibEncoder}, Compression};
        use std::io::Write;
        let msg = br#"{"data":{"msgs":[]},"error_code":0}"#;
        let mut g = GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(msg).unwrap();
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(msg).unwrap();
        let mut d = DeflateEncoder::new(Vec::new(), Compression::default());
        d.write_all(msg).unwrap();
        for blob in [g.finish().unwrap(), z.finish().unwrap(), d.finish().unwrap()] {
            assert_eq!(inflate(&blob).unwrap(), msg);
        }
        let e = inflate(&[0xde, 0xad, 0xbe, 0xef]).unwrap_err().to_string();
        assert!(e.contains("head deadbeef"), "{e}");
    }

    // One synthetic record (Alice / "hello" / ts 1700000000000), encoded four ways. Produced by a
    // node script that deflated with zca-js's own `pako`, encrypted with WebCrypto AES-GCM
    // (key bytes 3, 10, 17, ... step 7; IV bytes 200, 199, ...; AAD bytes 1..=16) and then
    // decoded every vector with zca-js 2.2.0's own `decodeEventData`, which had to give the
    // same record back. e0 plain, e1 zlib+base64, e2 AES-GCM(zlib)+uri-encoded base64, e3
    // AES-GCM(plain)+uri-encoded base64.
    const VECTORS: &str = include_str!("testdata/ev_vectors.json");

    #[test]
    fn decodes_all_encrypt_types_like_zca_js() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = v["key"].as_str().unwrap();
        let mut outs = Vec::new();
        for k in ["e0", "e1", "e2", "e3"] {
            let parsed: Value = serde_json::from_str(v[k].as_str().unwrap()).unwrap();
            outs.push(
                decode_event_data(&parsed, Some(key)).unwrap_or_else(|e| panic!("{k}: {e:#}")),
            );
        }
        assert!(outs.windows(2).all(|w| w[0] == w[1]));
        let msg = &outs[0]["data"]["msgs"][0];
        assert_eq!(msg["content"], "hello");
        assert_eq!(msg["msgId"], "7000000000000000001");
        assert_eq!(msg["idTo"], "1000000000000000003");
        assert_eq!(msg["dName"], "Alice");
        assert_eq!(msg["ts"], "1700000000000");
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let parsed: Value = serde_json::from_str(v["e2"].as_str().unwrap()).unwrap();
        let bad = B64.encode([9u8; 32]);
        let err = decode_event_data(&parsed, Some(&bad)).unwrap_err();
        assert!(format!("{err:#}").contains("authentication"));
    }

    #[test]
    fn frame_roundtrip_matches_js_layout() {
        let body = serde_json::json!({"eventId": 1});
        let f = encode_frame(1, cmd::PING, 1, &body);
        assert_eq!(&f[..4], &[1, 2, 0, 1]); // ver, cmd LE, sub
        let back = parse_frame(&f).unwrap().unwrap();
        assert_eq!(
            (back.version, back.cmd, back.sub, back.body),
            (1, 2, 1, body)
        );
        let big = encode_frame(1, cmd::OLD_GROUP_MSGS, 1, &serde_json::json!({}));
        assert_eq!(&big[1..3], &511u16.to_le_bytes());
        assert_eq!(parse_frame(&[1, 2, 0, 1]).unwrap(), None);
    }
}
