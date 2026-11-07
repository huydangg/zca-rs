//! Authenticated Zalo API calls (zca-js `apiFactory` + `handleZaloResponse`):
//! params are `encodeAES(secret_key, json)`, the response's `data` is
//! `decodeAES(secret_key, …)` → `{error_code, data}`.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::crypto;
use crate::error::{Error, Result};
use crate::http::{api_headers, Http};
use crate::mention::{utf16_spans, Mention, MentionTarget};
use crate::message::{id_of, ThreadType, ZaloRef};
use crate::session::{Session, API_TYPE, API_VERSION};
use serde_json::{json, Value};
use std::collections::HashMap;

fn service_url(session: &Session, service: &str, path: &str) -> Result<reqwest::Url> {
    let base = session.service_map[service][0]
        .as_str()
        .ok_or_else(|| Error::protocol(format!("no '{service}' endpoint in zpw_service_map_v3")))?;
    let mut u = reqwest::Url::parse(&format!("{base}{path}")).map_err(Error::bad_url)?;
    u.query_pairs_mut()
        .append_pair("zpw_ver", &API_VERSION.to_string())
        .append_pair("zpw_type", &API_TYPE.to_string());
    Ok(u)
}

/// `error_code` / `error_message` of an answer that is not a success.
fn api_error(v: &Value) -> Error {
    Error::Api {
        code: v["error_code"].as_i64().unwrap_or(-1),
        message: v["error_message"].as_str().unwrap_or_default().to_string(),
    }
}

fn unwrap_response(session: &Session, res: Value) -> Result<Value> {
    if res["error_code"].as_i64() != Some(0) {
        return Err(api_error(&res));
    }
    let wire = res["data"]
        .as_str()
        .ok_or_else(|| Error::protocol("API response without data"))?;
    let b64 = percent_encoding::percent_decode_str(wire).decode_utf8()?;
    let inner: Value = serde_json::from_str(&crypto::decode_aes(&session.secret_key, &b64)?)?;
    if inner["error_code"].as_i64() != Some(0) {
        return Err(api_error(&inner));
    }
    Ok(inner["data"].clone())
}

pub(crate) async fn get(
    http: &Http,
    session: &Session,
    service: &str,
    path: &str,
    params: &Value,
) -> Result<Value> {
    let mut url = service_url(session, service, path)?;
    url.query_pairs_mut().append_pair(
        "params",
        &crypto::encode_aes(&session.secret_key, &params.to_string())?,
    );
    let res: Value = http
        .client
        .get(url)
        .headers(api_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    unwrap_response(session, res)
}

pub(crate) async fn post(
    http: &Http,
    session: &Session,
    service: &str,
    path: &str,
    params: &Value,
) -> Result<Value> {
    post_url(http, session, service_url(session, service, path)?, params).await
}

async fn post_url(
    http: &Http,
    session: &Session,
    url: reqwest::Url,
    params: &Value,
) -> Result<Value> {
    let enc = crypto::encode_aes(&session.secret_key, &params.to_string())?;
    let res: Value = http
        .client
        .post(url)
        .headers(api_headers())
        .form(&[("params", enc)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    unwrap_response(session, res)
}

/// One of the account's friends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Friend {
    /// The friend's uid.
    pub uid: String,
    /// The name the account gave them (`displayName`); empty when none.
    pub display_name: String,
    /// The name they gave themselves (`zaloName`); empty when none.
    pub zalo_name: String,
}

impl Friend {
    /// The best name: the display name, else the Zalo name; `None` when both are empty.
    pub fn name(&self) -> Option<&str> {
        [self.display_name.as_str(), self.zalo_name.as_str()].into_iter().find(|s| !s.is_empty())
    }
}

/// zca-js `getAllFriends`.
pub(crate) async fn friends(http: &Http, session: &Session, imei: &str) -> Result<Vec<Friend>> {
    let params = json!({"incInvalid": 1, "page": 1, "count": 20000, "avatar_size": 120, "actiontime": 0, "imei": imei});
    let data = get(http, session, "profile", "/api/social/friend/getfriends", &params).await?;
    Ok(parse_friends(&data))
}

fn parse_friends(data: &Value) -> Vec<Friend> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    data.as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some(Friend { uid: id_of(&f["userId"])?, display_name: text(&f["displayName"]), zalo_name: text(&f["zaloName"]) }))
        .collect()
}

/// group id → group name.
pub(crate) async fn group_names(http: &Http, session: &Session, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let map: serde_json::Map<String, Value> = ids.iter().map(|id| (id.clone(), json!(0))).collect();
    let params = json!({"gridVerMap": Value::Object(map).to_string()});
    let data = post(http, session, "group", "/api/group/getmg-v2", &params).await?;
    let mut out = HashMap::new();
    if let Some(m) = data["gridInfoMap"].as_object() {
        for (id, info) in m {
            if let Some(n) = info["name"].as_str() {
                out.insert(id.clone(), n.to_string());
            }
        }
    }
    Ok(out)
}

/// A group, as `getmg-v2` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupInfo {
    /// The group id.
    pub id: String,
    /// The group's name, when the answer has one.
    pub name: Option<String>,
    /// Every member's uid (`memberIds`, else the ids in `memVerList`; a `_0` suffix dropped).
    pub member_ids: Vec<String>,
    /// The names the answer already carries for some members (`currentMems`: `dName`, else `zaloName`).
    pub member_names: HashMap<String, String>,
    /// The whole `data` object of the answer (`gridInfoMap[<id>]` holds this group's entry).
    pub raw: Value,
}

/// zca-js `getGroupInfo` for one group.
pub(crate) async fn group_info(http: &Http, session: &Session, gid: &str) -> Result<GroupInfo> {
    let params = json!({"gridVerMap": json!({gid: 0}).to_string()});
    let data = post(http, session, "group", "/api/group/getmg-v2", &params).await?;
    let info = &data["gridInfoMap"][gid];
    let (member_ids, member_names) = parse_group_members(info);
    Ok(GroupInfo { id: gid.to_string(), name: info["name"].as_str().map(String::from), member_ids, member_names, raw: data })
}

/// A group's members out of one `getmg-v2` entry: all `memberIds` (a `_0` suffix dropped) and the
/// names `currentMems` already carries (`dName`, else `zaloName`).
fn parse_group_members(info: &Value) -> (Vec<String>, HashMap<String, String>) {
    let strip = |v: &Value| -> Option<String> { Some(id_of(v)?.trim_end_matches("_0").to_string()).filter(|s| !s.is_empty()) };
    // `memberIds` is often empty: then the ids are in `memVerList` ("<uid>_<ver>")
    let mut ids: Vec<String> = info["memberIds"].as_array().into_iter().flatten().filter_map(strip).collect();
    if ids.is_empty() {
        ids = info["memVerList"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .filter_map(|s| s.rsplit_once('_').map_or(Some(s), |(id, _)| Some(id)))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect();
    }
    let mut names = HashMap::new();
    for m in info["currentMems"].as_array().into_iter().flatten() {
        let name = [&m["dName"], &m["zaloName"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()));
        if let (Some(id), Some(n)) = (strip(&m["id"]), name) {
            names.insert(id, n.trim().to_string());
        }
    }
    (ids, names)
}

/// Names out of a `social/group/members` answer (`profiles{id: {displayName, zaloName}}`).
fn parse_profiles(data: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (id, p) in data["profiles"].as_object().into_iter().flatten() {
        let name = [&p["displayName"], &p["zaloName"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()));
        if let Some(n) = name {
            out.insert(id.trim_end_matches("_0").to_string(), n.trim().to_string());
        }
    }
    out
}

/// Most members whose names are looked up for one group.
const MEMBERS_LOOKUP_MAX: usize = 300;

/// A group member with a known name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The member's uid.
    pub uid: String,
    /// Their name.
    pub name: String,
}

/// The members of group `gid`: zca-js `getGroupInfo` for the ids and the names it carries,
/// then `known` (uid -> name, the friends) and zca-js `getGroupMembersInfo`
/// (`friend_pversion_map: ["<uid>_0", ...]`) for the rest. Members that stay nameless are left out.
pub(crate) async fn group_members(http: &Http, session: &Session, gid: &str, known: &HashMap<String, String>) -> Result<Vec<Member>> {
    let GroupInfo { member_ids: ids, member_names: mut names, .. } = group_info(http, session, gid).await?;
    for id in &ids {
        if let Some(n) = known.get(id).filter(|_| !names.contains_key(id)) {
            names.insert(id.clone(), n.clone());
        }
    }
    let missing: Vec<&String> = ids.iter().filter(|i| !names.contains_key(*i)).take(MEMBERS_LOOKUP_MAX).collect();
    for chunk in missing.chunks(100) {
        let params = json!({"friend_pversion_map": chunk.iter().map(|i| format!("{i}_0")).collect::<Vec<_>>()});
        if let Ok(data) = get(http, session, "profile", "/api/social/group/members", &params).await {
            names.extend(parse_profiles(&data));
        }
    }
    Ok(ids.into_iter().filter_map(|uid| names.remove(&uid).map(|name| Member { uid, name })).collect())
}

/// Every group the account is in (zca-js getAllGroups -> gridVerMap keys).
pub(crate) async fn all_group_ids(http: &Http, session: &Session) -> Result<Vec<String>> {
    let url = service_url(session, "group_poll", "/api/group/getlg/v4")?;
    let res: Value = http.client.get(url).headers(api_headers()).send().await?.error_for_status()?.json().await?;
    let data = unwrap_response(session, res)?;
    Ok(data["gridVerMap"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default())
}

/// A request ready to go: which service of the session's map, the path, whether
/// `nretry=0` is added (the send endpoints do), and the plain JSON params (`post` encrypts them).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Planned {
    pub service: &'static str,
    pub path: &'static str,
    pub nretry: bool,
    pub params: Value,
}

/// Run a planned request.
pub(crate) async fn run(http: &Http, session: &Session, p: &Planned) -> Result<Value> {
    let mut url = service_url(session, p.service, p.path)?;
    if p.nretry {
        url.query_pairs_mut().append_pair("nretry", "0");
    }
    post_url(http, session, url, &p.params).await
}

/// zca-js `getClientMessageType`: the number `qmsgType` carries for a message type.
pub(crate) fn client_message_type(msg_type: &str) -> i64 {
    match msg_type {
        "webchat" => 1,
        "chat.voice" => 31,
        "chat.photo" => 32,
        "chat.sticker" => 36,
        "chat.doodle" => 37,
        "chat.recommended" | "chat.link" => 38,
        "chat.video.msg" => 44,
        "share.file" => 46,
        "chat.gif" => 49,
        "chat.location.new" => 43,
        _ => 1,
    }
}

/// zca-js `prepareQMSG`: the quoted text of a message that is not plain text (only a to-do has any).
fn quote_text(q: &ZaloRef) -> String {
    if q.msg_type == "chat.todo" {
        if let Some(item) = q.content["params"].as_str().and_then(|p| serde_json::from_str::<Value>(p).ok()) {
            return item["item"]["content"].as_str().unwrap_or("").to_string();
        }
    }
    String::new()
}

/// zca-js `prepareQMSGAttach`, JSON-stringified (groups only; `None`: left out).
fn quote_attach(q: &ZaloRef) -> Option<String> {
    let v = if q.content.is_string() {
        q.property_ext.clone()
    } else if q.msg_type == "chat.todo" {
        json!({"properties": {"color": 0, "size": 0, "type": 0, "subType": 0, "ext": "{\"shouldParseLinkOrContact\":0}"}})
    } else {
        let mut v = q.content.clone();
        if let Some(o) = v.as_object_mut() {
            let (thumb, href) = (o.get("thumb").cloned().unwrap_or(Value::Null), o.get("href").cloned().unwrap_or(Value::Null));
            o.insert("thumbUrl".into(), thumb);
            o.insert("oriUrl".into(), href.clone());
            o.insert("normalUrl".into(), href);
        }
        v
    };
    (!v.is_null()).then(|| v.to_string())
}

/// One entry of zca-js's `mentionInfo`: `pos` / `len` in UTF-16 code units (JS string indices),
/// `uid` `"-1"` and `type` 1 for @All, else the user id and 0. Field order as zca-js writes it.
#[derive(serde::Serialize)]
struct ZMention {
    pos: usize,
    uid: String,
    len: usize,
    #[serde(rename = "type")]
    kind: u8,
}

/// zca-js `JSON.stringify(mentionsFinal)`; `None` when no mention is usable.
fn mention_info(text: &str, mentions: &[Mention]) -> Option<String> {
    let all: Vec<ZMention> = utf16_spans(text, mentions)
        .into_iter()
        .map(|(pos, len, m)| match &m.target {
            MentionTarget::All => ZMention { pos, uid: "-1".into(), len, kind: 1 },
            MentionTarget::User(uid) => ZMention { pos, uid: uid.clone(), len, kind: 0 },
        })
        .collect();
    (!all.is_empty()).then(|| serde_json::to_string(&all).unwrap_or_default())
}

/// A text message, or (with `quote`) the same answering that message as a quote card
/// (zca-js `sendMessage` -> `handleMessage`; the quote endpoint is the send one plus
/// `/quote`). `thread_id` is a uid for DMs or the group id for groups.
pub(crate) fn plan_text(imei: &str, thread_id: &str, thread_type: ThreadType, text: &str, client_id: i64, quote: Option<&ZaloRef>, mentions: &[Mention]) -> Planned {
    let group = thread_type.is_group();
    // zca-js `handleMentions` / `isMentionsValid`: only group messages carry them
    let info = mention_info(text, mentions).filter(|_| group);
    let (service, path) = match (group, quote.is_some()) {
        (true, false) if info.is_some() => ("group", "/api/group/mention"),
        (true, false) => ("group", "/api/group/sendmsg"),
        (true, true) => ("group", "/api/group/quote"),
        (false, false) => ("chat", "/api/message/sms"),
        (false, true) => ("chat", "/api/message/quote"),
    };
    let mut p = if group {
        json!({"message": text, "clientId": client_id, "ttl": 0, "visibility": 0, "grid": thread_id})
    } else {
        json!({"message": text, "clientId": client_id, "imei": imei, "ttl": 0, "toid": thread_id})
    };
    if let (Some(info), Some(o)) = (info, p.as_object_mut()) {
        o.insert("mentionInfo".into(), json!(info));
    }
    if let (Some(q), Some(o)) = (quote, p.as_object_mut()) {
        o.insert("qmsgOwner".into(), json!(q.uid_from));
        o.insert("qmsgId".into(), json!(q.msg_id));
        o.insert("qmsgCliId".into(), json!(q.cli_msg_id));
        o.insert("qmsgType".into(), json!(client_message_type(&q.msg_type)));
        o.insert("qmsgTs".into(), json!(q.ts));
        o.insert("qmsg".into(), json!(q.text().map(str::to_string).unwrap_or_else(|| quote_text(q))));
        o.insert("qmsgTTL".into(), json!(q.ttl));
        if group {
            if let Some(a) = quote_attach(q) {
                o.insert("qmsgAttach".into(), json!(a));
            }
        }
    }
    Planned { service, path, nretry: true, params: p }
}

/// zca-js `forwardMessage` (text, one recipient, no `reference`): `mforward` on the
/// `file` service. A DM carries the device id, a group does not.
pub(crate) fn plan_forward(imei: &str, to_thread: &str, thread_type: ThreadType, text: &str, client_id: i64) -> Planned {
    let group = thread_type.is_group();
    let cid = client_id.to_string();
    let msg_info = json!({"message": text}).to_string();
    let params = if group {
        json!({"grids": [{"clientId": cid, "grid": to_thread, "ttl": 0}], "ttl": 0, "msgType": "1", "totalIds": 1, "msgInfo": msg_info, "decorLog": "null"})
    } else {
        json!({"toIds": [{"clientId": cid, "toUid": to_thread, "ttl": 0}], "imei": imei, "ttl": 0, "msgType": "1", "totalIds": 1, "msgInfo": msg_info, "decorLog": "null"})
    };
    Planned { service: "file", path: if group { "/api/group/mforward" } else { "/api/message/mforward" }, nretry: false, params }
}

/// The message id of a forward that went through; Err: why one did not (`fail[].error_code`).
pub(crate) fn forward_result(data: &Value) -> Result<Option<String>> {
    if let Some(f) = data["fail"].as_array().and_then(|a| a.first()) {
        return Err(Error::protocol(format!("Zalo refused the forward (error {})", f["error_code"])));
    }
    Ok(data["success"].as_array().and_then(|a| a.first()).and_then(|s| id_of(&s["msgId"])))
}

/// zca-js `addReaction`: `react_list[0].message` is itself JSON (the reacted message's
/// global and client ids, the icon, its type, `source` 6). A DM names the other user
/// (`toid`), a group its id and our device (`grid`, `imei`). `icon`/`r_type` of `""`/-1
/// take the reaction back.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_reaction(imei: &str, thread_id: &str, thread_type: ThreadType, msg_id: u64, cli_msg_id: u64, icon: &str, r_type: i32, client_id: i64) -> Planned {
    let group = thread_type.is_group();
    let message = json!({"rMsg": [{"gMsgID": msg_id, "cMsgID": cli_msg_id, "msgType": 1}], "rIcon": icon, "rType": r_type, "source": 6}).to_string();
    let mut params = json!({"react_list": [{"message": message, "clientId": client_id}]});
    if let Some(o) = params.as_object_mut() {
        if group {
            o.insert("grid".into(), json!(thread_id));
            o.insert("imei".into(), json!(imei));
        } else {
            o.insert("toid".into(), json!(thread_id));
        }
    }
    Planned { service: "reaction", path: if group { "/api/group/reaction" } else { "/api/message/reaction" }, nretry: false, params }
}

/// zca-js `undo`: recall my own message `msg_id` (its client id `cli_msg_id` too). A DM
/// names the other user (`toid`), a group its id and our device (`grid`, `visibility` 0,
/// `imei`); `clientId` is the time. The ids go as strings, as zca-js passes the message's own
/// (an id over 2^53 would be rounded as a JS number).
pub(crate) fn plan_undo(imei: &str, thread_id: &str, thread_type: ThreadType, msg_id: &str, cli_msg_id: &str, client_id: i64) -> Planned {
    let group = thread_type.is_group();
    let mut params = json!({"msgId": msg_id, "clientId": client_id, "cliMsgIdUndo": cli_msg_id});
    if let Some(o) = params.as_object_mut() {
        if group {
            o.insert("grid".into(), json!(thread_id));
            o.insert("visibility".into(), json!(0));
            o.insert("imei".into(), json!(imei));
        } else {
            o.insert("toid".into(), json!(thread_id));
        }
    }
    Planned { service: if group { "group" } else { "chat" }, path: if group { "/api/group/undomsg" } else { "/api/message/undo" }, nretry: false, params }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ThreadType::{Group, User};

    fn text_ref() -> ZaloRef {
        ZaloRef {
            msg_id: "7000000000000000001".into(),
            cli_msg_id: "1700000000123".into(),
            uid_from: "42".into(),
            msg_type: "webchat".into(),
            ts: "1700000000500".into(),
            ttl: 0,
            content: json!("meeting at 3 ok"),
            property_ext: Value::Null,
        }
    }

    #[test]
    fn a_plain_text_is_as_before() {
        let p = plan_text("IMEI", "42", User, "hi", 77, None, &[]);
        assert_eq!((p.service, p.path, p.nretry), ("chat", "/api/message/sms", true));
        assert_eq!(p.params, json!({"message": "hi", "clientId": 77, "imei": "IMEI", "ttl": 0, "toid": "42"}));
        let g = plan_text("IMEI", "2", Group, "hi", 77, None, &[]);
        assert_eq!((g.service, g.path), ("group", "/api/group/sendmsg"));
        assert_eq!(g.params, json!({"message": "hi", "clientId": 77, "ttl": 0, "visibility": 0, "grid": "2"}));
    }

    /// The `/quote` endpoint of a DM with every `qmsg*` field zca-js sends.
    #[test]
    fn a_dm_quote_goes_to_message_quote_with_the_qmsg_fields() {
        let p = plan_text("IMEI", "42", User, "ok anh", 77, Some(&text_ref()), &[]);
        assert_eq!((p.service, p.path, p.nretry), ("chat", "/api/message/quote", true));
        assert_eq!(
            p.params,
            json!({
                "message": "ok anh", "clientId": 77, "imei": "IMEI", "ttl": 0, "toid": "42",
                "qmsgOwner": "42", "qmsgId": "7000000000000000001", "qmsgCliId": "1700000000123",
                "qmsgType": 1, "qmsgTs": "1700000000500", "qmsg": "meeting at 3 ok", "qmsgTTL": 0
            })
        );
    }

    #[test]
    fn a_group_quote_has_grid_visibility_and_no_imei_and_the_attach_only_when_there_is_one() {
        let p = plan_text("IMEI", "2", Group, "ok", 77, Some(&text_ref()), &[]);
        assert_eq!((p.service, p.path), ("group", "/api/group/quote"));
        let o = p.params.as_object().unwrap();
        assert_eq!(o["grid"], "2");
        assert_eq!(o["visibility"], 0);
        assert!(!o.contains_key("imei") && !o.contains_key("toid") && !o.contains_key("qmsgAttach"), "a text without styles has no attach");
        // styled text: propertyExt goes as the JSON string qmsgAttach
        let mut styled = text_ref();
        styled.property_ext = json!({"color": 0, "size": 0, "type": 0, "subType": 0, "ext": "{}"});
        let o = plan_text("IMEI", "2", Group, "ok", 77, Some(&styled), &[]).params;
        assert_eq!(serde_json::from_str::<Value>(o["qmsgAttach"].as_str().unwrap()).unwrap(), styled.property_ext);
        // a DM never has one
        assert!(!plan_text("IMEI", "42", User, "ok", 77, Some(&styled), &[]).params.as_object().unwrap().contains_key("qmsgAttach"));
    }

    #[test]
    fn quoting_a_card_carries_its_urls_and_the_type_number() {
        let q = ZaloRef {
            msg_id: "5".into(),
            cli_msg_id: "6".into(),
            uid_from: "42".into(),
            msg_type: "chat.photo".into(),
            ts: "9".into(),
            ttl: 30,
            content: json!({"title": "", "href": "https://f.zdn.vn/a.jpg", "thumb": "https://f.zdn.vn/t.jpg"}),
            property_ext: Value::Null,
        };
        let o = plan_text("I", "1", Group, "nice", 1, Some(&q), &[]).params;
        assert_eq!((o["qmsgType"].clone(), o["qmsg"].clone(), o["qmsgTTL"].clone()), (json!(32), json!(""), json!(30)));
        let attach: Value = serde_json::from_str(o["qmsgAttach"].as_str().unwrap()).unwrap();
        assert_eq!((attach["thumbUrl"].clone(), attach["oriUrl"].clone(), attach["normalUrl"].clone()), (json!("https://f.zdn.vn/t.jpg"), json!("https://f.zdn.vn/a.jpg"), json!("https://f.zdn.vn/a.jpg")));
        assert_eq!(attach["title"], "");
        for (t, n) in [("webchat", 1), ("chat.voice", 31), ("chat.sticker", 36), ("chat.doodle", 37), ("chat.recommended", 38), ("chat.link", 38), ("chat.video.msg", 44), ("share.file", 46), ("chat.gif", 49), ("chat.location.new", 43), ("whatever", 1)] {
            assert_eq!(client_message_type(t), n, "{t}");
        }
    }

    /// zca-js `sendMessage` with `mentions`: a group message goes to `/api/group/mention` with
    /// `mentionInfo` = `JSON.stringify([{pos, uid, len, type}])`; @All is uid "-1", type 1.
    #[test]
    fn a_group_message_with_mentions_goes_to_the_mention_endpoint_with_mention_info() {
        let text = "@Zoë An xem 😀 @All";
        let p = plan_text("IMEI", "2", Group, text, 77, None, &[Mention::user(0, 7, "1"), Mention::all(14, 4)]);
        assert_eq!((p.service, p.path, p.nretry), ("group", "/api/group/mention", true));
        // the same bytes JSON.stringify gives (pos/len in UTF-16 units: the emoji counts twice)
        assert_eq!(p.params["mentionInfo"], r#"[{"pos":0,"uid":"1","len":7,"type":0},{"pos":15,"uid":"-1","len":4,"type":1}]"#);
        assert_eq!(p.params["message"], text);
        assert_eq!((p.params["grid"].clone(), p.params["visibility"].clone()), (json!("2"), json!(0)));
        // a mention that is out of range is not sent; none left means the plain endpoint
        let q = plan_text("IMEI", "2", Group, "hi", 77, None, &[Mention::user(0, 9, "1")]);
        assert_eq!(q.path, "/api/group/sendmsg");
        assert!(q.params.get("mentionInfo").is_none());
        // a quote keeps the quote endpoint and carries the mentions too
        let r = plan_text("IMEI", "2", Group, "@An ok", 77, Some(&text_ref()), &[Mention::user(0, 3, "5")]);
        assert_eq!(r.path, "/api/group/quote");
        assert_eq!(r.params["mentionInfo"], r#"[{"pos":0,"uid":"5","len":3,"type":0}]"#);
        // a DM never has mentions
        let d = plan_text("IMEI", "42", User, "@An ok", 77, None, &[Mention::user(0, 3, "5")]);
        assert_eq!(d.path, "/api/message/sms");
        assert!(d.params.get("mentionInfo").is_none());
    }

    #[test]
    fn group_info_gives_member_ids_with_the_names_it_carries_and_profiles_name_the_rest() {
        let info = json!({
            "memberIds": ["1", "2_0", 3],
            "currentMems": [{"id": "1", "dName": "Zoë An", "zaloName": "zoean"}, {"id": "2_0", "dName": "", "zaloName": "bo"}, {"id": "9", "dName": " ", "zaloName": ""}]
        });
        let (ids, names) = parse_group_members(&info);
        assert_eq!(ids, ["1", "2", "3"]);
        assert_eq!(names.get("1").map(String::as_str), Some("Zoë An"));
        assert_eq!(names.get("2").map(String::as_str), Some("bo"), "zaloName stands in for an empty dName");
        assert!(!names.contains_key("3") && !names.contains_key("9"));
        assert!(parse_group_members(&json!(null)).0.is_empty());
        // big groups: no memberIds / currentMems, the ids are in memVerList
        let (ids, names) = parse_group_members(&json!({"memberIds": [], "currentMems": [], "memVerList": ["11_0", "22_3", ""]}));
        assert_eq!((ids, names.len()), (vec!["11".to_string(), "22".to_string()], 0));
        let profiles = json!({"profiles": {"3_0": {"displayName": "Ba", "zaloName": "x"}, "4": {"displayName": "", "zaloName": ""}}, "unchangeds_profile": []});
        let got = parse_profiles(&profiles);
        assert_eq!((got.get("3").map(String::as_str), got.len()), (Some("Ba"), 1));
    }

    #[test]
    fn friends_are_listed_with_the_best_name() {
        let data = json!([
            {"userId": "1", "displayName": "Dave", "zaloName": "dave"},
            {"userId": 2, "displayName": "", "zaloName": "bo"},
            {"userId": "3", "displayName": "", "zaloName": ""},
            {"displayName": "no id"}
        ]);
        let got = parse_friends(&data);
        assert_eq!(got.iter().map(|f| (f.uid.as_str(), f.name())).collect::<Vec<_>>(), [("1", Some("Dave")), ("2", Some("bo")), ("3", None)]);
        assert!(parse_friends(&json!(null)).is_empty());
    }

    #[test]
    fn a_dm_recall_names_the_user_and_both_ids() {
        let p = plan_undo("IMEI", "42", User, "7000000000000000001", "1700000000123", 1700000099000);
        assert_eq!((p.service, p.path, p.nretry), ("chat", "/api/message/undo", false));
        assert_eq!(p.params, json!({"msgId": "7000000000000000001", "clientId": 1700000099000i64, "cliMsgIdUndo": "1700000000123", "toid": "42"}));
        // the big id is written exactly, not rounded as a JS double would
        assert!(p.params.to_string().contains("\"7000000000000000001\""));
    }

    #[test]
    fn a_group_recall_names_the_group_visibility_and_the_device() {
        let p = plan_undo("IMEI", "2000000000000000002", Group, "5", "6", 7);
        assert_eq!((p.service, p.path, p.nretry), ("group", "/api/group/undomsg", false));
        assert_eq!(p.params, json!({"msgId": "5", "clientId": 7, "cliMsgIdUndo": "6", "grid": "2000000000000000002", "visibility": 0, "imei": "IMEI"}));
        assert!(p.params.get("toid").is_none());
    }

    #[test]
    fn a_forward_is_mforward_on_the_file_service() {
        let p = plan_forward("IMEI", "42", User, "meeting at 3", 1700000000123);
        assert_eq!((p.service, p.path, p.nretry), ("file", "/api/message/mforward", false));
        assert_eq!(
            p.params,
            json!({
                "toIds": [{"clientId": "1700000000123", "toUid": "42", "ttl": 0}], "imei": "IMEI", "ttl": 0, "msgType": "1", "totalIds": 1,
                "msgInfo": "{\"message\":\"meeting at 3\"}", "decorLog": "null"
            })
        );
        let g = plan_forward("IMEI", "2", Group, "x", 5);
        assert_eq!((g.service, g.path), ("file", "/api/group/mforward"));
        assert_eq!(g.params["grids"], json!([{"clientId": "5", "grid": "2", "ttl": 0}]));
        assert!(g.params.get("imei").is_none() && g.params.get("toIds").is_none());
        assert_eq!(forward_result(&json!({"success": [{"clientId": "5", "msgId": "99"}], "fail": []})).unwrap().as_deref(), Some("99"));
        assert_eq!(forward_result(&json!({"success": [], "fail": []})).unwrap(), None);
        assert!(forward_result(&json!({"success": [], "fail": [{"clientId": "5", "error_code": "114"}]})).unwrap_err().to_string().contains("114"));
    }

    #[test]
    fn a_dm_reaction_names_the_user_and_the_message_ids() {
        let p = plan_reaction("IMEI", "42", User, 7000000000000000001, 1700000000123, "/-heart", 5, 1700000099000);
        assert_eq!((p.service, p.path, p.nretry), ("reaction", "/api/message/reaction", false));
        let want_msg = r#"{"rIcon":"/-heart","rMsg":[{"cMsgID":1700000000123,"gMsgID":7000000000000000001,"msgType":1}],"rType":5,"source":6}"#;
        assert_eq!(p.params, json!({"react_list": [{"message": want_msg, "clientId": 1700000099000i64}], "toid": "42"}));
        // the big id is written exactly, not rounded as a JS double would
        assert!(p.params.to_string().contains("7000000000000000001"));
    }

    #[test]
    fn a_group_reaction_names_the_group_and_the_device_and_removal_is_an_empty_icon() {
        let p = plan_reaction("IMEI", "2", Group, 5, 6, "", -1, 1);
        assert_eq!((p.service, p.path), ("reaction", "/api/group/reaction"));
        let o = p.params.as_object().unwrap();
        assert_eq!((o["grid"].clone(), o["imei"].clone()), (json!("2"), json!("IMEI")));
        assert!(!o.contains_key("toid"));
        let inner: Value = serde_json::from_str(o["react_list"][0]["message"].as_str().unwrap()).unwrap();
        assert_eq!((inner["rIcon"].clone(), inner["rType"].clone(), inner["source"].clone()), (json!(""), json!(-1), json!(6)));
        assert_eq!(inner["rMsg"][0], json!({"gMsgID": 5, "cMsgID": 6, "msgType": 1}));
    }

    #[test]
    fn an_error_answer_carries_its_code_and_message() {
        let e = api_error(&json!({"error_code": 114, "error_message": "bad"}));
        assert_eq!(e.to_string(), "Zalo API error 114: bad");
        assert!(matches!(api_error(&json!({})), Error::Api { code: -1, .. }));
    }
}
