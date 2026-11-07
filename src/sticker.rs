//! Zalo stickers: searching them (zca-js `getStickers` + `getStickersDetail`) and sending one
//! (zca-js `sendSticker`).
//!
//! * `getStickers(keyword)`: GET `{sticker}/api/message/sticker/suggest/stickers`, params
//!   `{keyword, gif: 1, guggy: 0, imei}`; the answer's `sugg_sticker` lists `{type, cate_id,
//!   sticker_id}`. Only the ids are used.
//! * `getStickersDetail(ids)`: one GET `{sticker}/api/message/sticker/sticker_detail` per id,
//!   params `{sid}`; the answer is a `StickerDetail` (`id`, `cateId`, `type`, `text`,
//!   `stickerUrl`, `stickerSpriteUrl`, `stickerWebpUrl`, ...). One that fails is left out.
//! * `sendSticker({id, cateId, type}, threadId, threadType)`: POST `{chat}/api/message/sticker`
//!   (DM) or `{group}/api/group/sticker` (group), `nretry=0`, params `{stickerId, cateId, type,
//!   clientId, imei, zsource: 101, toid | grid}`; the answer is `{msgId}`.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::api::{self, Planned};
use crate::error::{Error, Result};
use crate::http::Http;
use crate::message::ThreadType;
use crate::session::Session;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Where a sticker's static picture lives on the CDN (`eid=<id>&size=130` follows). Its
/// animated version is [`sprite_url`] of it: an animated WebP, or for older stickers a PNG
/// row of square frames.
pub const STICKER_URL: &str = "https://zalo-api.zadn.vn/api/emoticon/sticker/webpc?";
const SPRITE_URL: &str = "https://zalo-api.zadn.vn/api/emoticon/sprite?";

/// The animated version of the sticker whose static picture is `href` (see
/// [`ZaloRef::sticker_url`](crate::ZaloRef::sticker_url)), or `None` when `href` is not one.
pub fn sprite_url(href: &str) -> Option<String> {
    Some(format!("{SPRITE_URL}{}", href.strip_prefix(STICKER_URL)?))
}

/// Most stickers one search lists (a keyword may suggest many more).
pub const SEARCH_MAX: usize = 24;
/// Detail lookups in flight at once.
const DETAIL_PARALLEL: usize = 8;
/// zca-js `sendSticker`'s `zsource`.
const ZSOURCE: u32 = 101;

/// A sticker, as a search lists it and as sending one needs it (zca-js `SendStickerPayload`:
/// `id`, `cateId`, `type`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    /// Sticker id.
    pub id: u64,
    /// Pack (category) id; may be 0.
    pub cate_id: u64,
    /// Sticker type (`type`).
    pub kind: u32,
    /// The sticker's `text` (what it says / is called; may be empty).
    pub text: String,
    /// The sticker's own picture as Zalo's detail gives it: the sprite sheet, else the WebP,
    /// else the still. Empty when it is only known by id.
    pub url: String,
    /// The static CDN picture by id (what a sticker message links to).
    pub static_url: String,
}

/// A GET to a service of the session's map, with its plain JSON params (`api::get` encrypts them).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Lookup {
    pub service: &'static str,
    pub path: &'static str,
    pub params: Value,
}

/// zca-js `getStickers`.
pub(crate) fn plan_search(imei: &str, keyword: &str) -> Result<Lookup> {
    if keyword.trim().is_empty() {
        return Err(Error::invalid("Missing keyword"));
    }
    Ok(Lookup { service: "sticker", path: "/api/message/sticker/suggest/stickers", params: json!({"keyword": keyword, "gif": 1, "guggy": 0, "imei": imei}) })
}

/// zca-js `getStickersDetail`, for one id.
pub(crate) fn plan_detail(id: u64) -> Lookup {
    Lookup { service: "sticker", path: "/api/message/sticker/sticker_detail", params: json!({"sid": id}) }
}

/// zca-js `sendSticker`: a DM names the other user (`toid`), a group its id (`grid`); both carry
/// our device id. Err: what zca-js refuses before sending (no id, no type).
pub(crate) fn plan_send(imei: &str, thread_id: &str, thread_type: ThreadType, sticker: &Sticker, client_id: i64) -> Result<Planned> {
    if thread_id.is_empty() {
        return Err(Error::invalid("Missing threadId"));
    }
    if sticker.id == 0 {
        return Err(Error::invalid("Missing sticker id"));
    }
    if sticker.kind == 0 {
        return Err(Error::invalid("Missing sticker type"));
    }
    let group = thread_type.is_group();
    let mut params = json!({"stickerId": sticker.id, "cateId": sticker.cate_id, "type": sticker.kind, "clientId": client_id, "imei": imei, "zsource": ZSOURCE});
    if let Some(o) = params.as_object_mut() {
        o.insert(if group { "grid" } else { "toid" }.into(), json!(thread_id));
    }
    Ok(Planned { service: if group { "group" } else { "chat" }, path: if group { "/api/group/sticker" } else { "/api/message/sticker" }, nretry: true, params })
}

/// A JSON number, or a string of digits, as a number.
fn num(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The sticker ids of a `getStickers` answer (`sugg_sticker[].sticker_id`), in order, once each.
pub(crate) fn parse_search(data: &Value) -> Vec<u64> {
    let mut ids: Vec<u64> = Vec::new();
    for s in data["sugg_sticker"].as_array().into_iter().flatten() {
        if let Some(id) = num(&s["sticker_id"]).filter(|id| *id != 0 && !ids.contains(id)) {
            ids.push(id);
        }
    }
    ids
}

/// A `StickerDetail` as a [`Sticker`] (`None`: one that could not be sent, with no id or type).
pub(crate) fn parse_detail(data: &Value) -> Option<Sticker> {
    let id = num(&data["id"]).filter(|id| *id != 0)?;
    let kind = num(&data["type"]).filter(|k| *k != 0)? as u32;
    let text = |k: &str| data[k].as_str().map(str::trim).filter(|s| !s.is_empty());
    Some(Sticker {
        id,
        cate_id: num(&data["cateId"]).unwrap_or(0),
        kind,
        text: text("text").unwrap_or_default().to_string(),
        url: text("stickerSpriteUrl").or_else(|| text("stickerWebpUrl")).or_else(|| text("stickerUrl")).unwrap_or_default().to_string(),
        static_url: crate::message::sticker_url(&json!({"id": id}))?,
    })
}

async fn lookup(http: &Http, session: &Session, l: &Lookup) -> Result<Value> {
    api::get(http, session, l.service, l.path, &l.params).await
}

/// One sticker's details; `None` when Zalo has none (or not a sendable one).
pub(crate) async fn detail(http: &Http, session: &Session, id: u64) -> Result<Option<Sticker>> {
    Ok(parse_detail(&lookup(http, session, &plan_detail(id)).await?))
}

/// The stickers `keyword` suggests, with their details (at most [`SEARCH_MAX`]).
pub(crate) async fn search(http: &Http, session: &Session, imei: &str, keyword: &str) -> Result<Vec<Sticker>> {
    let ids: Vec<u64> = parse_search(&lookup(http, session, &plan_search(imei, keyword)?).await?).into_iter().take(SEARCH_MAX).collect();
    let mut items = Vec::new();
    for chunk in ids.chunks(DETAIL_PARALLEL) {
        let got = futures_util::future::join_all(chunk.iter().map(|id| async move { lookup(http, session, &plan_detail(*id)).await.ok().and_then(|d| parse_detail(&d)) })).await;
        items.extend(got.into_iter().flatten());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `getStickers` answer as zca-js types it (`StickerSuggestions`): `sugg_gif` / `sugg_guggy`
    /// may be null, ids may repeat.
    fn suggestions() -> Value {
        json!({
            "sugg_sticker": [
                {"type": 1, "cate_id": 1002, "sticker_id": 2005},
                {"type": 3, "cate_id": 2150, "sticker_id": 38590},
                {"type": 1, "cate_id": 1002, "sticker_id": 2005},
                {"type": 1, "cate_id": 7, "sticker_id": "4411"},
                {"type": 1, "cate_id": 7, "sticker_id": 0}
            ],
            "sugg_guggy": null,
            "sugg_gif": null
        })
    }

    /// A `StickerDetail` as zca-js types it.
    fn detail_json() -> Value {
        json!({
            "id": 38590, "cateId": 2150, "type": 3, "text": "  haha ", "uri": "", "fkey": 0, "status": 1,
            "stickerUrl": "https://stc-zsticker.zdn.vn/static/38590.png",
            "stickerSpriteUrl": "https://zalo-api.zadn.vn/api/emoticon/sprite?eid=38590&size=130",
            "stickerWebpUrl": "https://stc-zsticker.zdn.vn/static/38590.webp",
            "totalFrames": 20, "duration": 2000, "effectId": 0, "checksum": "abc", "ext": 0, "source": 0,
            "fss": null, "fssInfo": null, "version": 3, "extInfo": null
        })
    }

    #[test]
    fn the_animated_version_of_a_sticker_is_its_sprite_url_with_the_same_id_and_size() {
        let still = crate::message::sticker_url(&json!({"id": 38590, "catId": 1, "type": 1})).unwrap();
        assert_eq!(sprite_url(&still).as_deref(), Some("https://zalo-api.zadn.vn/api/emoticon/sprite?eid=38590&size=130"));
        // anything that is not a Zalo sticker picture has no sprite
        for other in ["tg-sticker:42", "https://example.com/a.png", "https://zalo-api.zadn.vn/api/emoticon/sprite?eid=1&size=130"] {
            assert_eq!(sprite_url(other), None, "{other}");
        }
    }

    #[test]
    fn a_search_is_a_get_on_the_sticker_service_with_the_keyword_and_the_device() {
        let l = plan_search("IMEI", "vui vẻ").unwrap();
        assert_eq!((l.service, l.path), ("sticker", "/api/message/sticker/suggest/stickers"));
        assert_eq!(l.params, json!({"keyword": "vui vẻ", "gif": 1, "guggy": 0, "imei": "IMEI"}));
        assert!(plan_search("IMEI", "  ").unwrap_err().to_string().contains("Missing keyword"));
    }

    #[test]
    fn a_detail_lookup_is_one_get_per_sticker_id() {
        let l = plan_detail(38590);
        assert_eq!((l.service, l.path), ("sticker", "/api/message/sticker/sticker_detail"));
        assert_eq!(l.params, json!({"sid": 38590}));
    }

    #[test]
    fn suggestions_give_the_ids_in_order_once_each_and_nothing_when_there_are_none() {
        assert_eq!(parse_search(&suggestions()), [2005, 38590, 4411]);
        assert!(parse_search(&json!({"sugg_sticker": null, "sugg_guggy": null, "sugg_gif": null})).is_empty());
        assert!(parse_search(&json!(null)).is_empty());
    }

    #[test]
    fn a_detail_becomes_a_sticker_with_the_sprite_as_its_url_and_the_cdn_still_by_id() {
        let it = parse_detail(&detail_json()).unwrap();
        assert_eq!((it.id, it.cate_id, it.kind, it.text.as_str()), (38590, 2150, 3, "haha"));
        assert_eq!(it.url, "https://zalo-api.zadn.vn/api/emoticon/sprite?eid=38590&size=130");
        assert_eq!(it.static_url, "https://zalo-api.zadn.vn/api/emoticon/sticker/webpc?eid=38590&size=130");
        // no sprite: the WebP, then the still
        let mut d = detail_json();
        d["stickerSpriteUrl"] = json!("");
        assert_eq!(parse_detail(&d).unwrap().url, "https://stc-zsticker.zdn.vn/static/38590.webp");
        d["stickerWebpUrl"] = json!(null);
        assert_eq!(parse_detail(&d).unwrap().url, "https://stc-zsticker.zdn.vn/static/38590.png");
        // a sticker with no id or no type could not be sent: left out
        assert!(parse_detail(&json!({"id": 0, "type": 1})).is_none());
        assert!(parse_detail(&json!({"id": 5, "type": 0})).is_none());
        assert!(parse_detail(&json!(null)).is_none());
        // cateId 0 happens and is still sendable
        assert_eq!(parse_detail(&json!({"id": 5, "cateId": 0, "type": 1})).unwrap().cate_id, 0);
    }

    #[test]
    fn a_dm_sticker_names_the_user_and_a_group_sticker_the_group_both_with_the_device() {
        let s = Sticker { id: 38590, cate_id: 2150, kind: 3, ..Default::default() };
        let p = plan_send("IMEI", "42", ThreadType::User, &s, 1700000000123).unwrap();
        assert_eq!((p.service, p.path, p.nretry), ("chat", "/api/message/sticker", true));
        assert_eq!(p.params, json!({"stickerId": 38590, "cateId": 2150, "type": 3, "clientId": 1700000000123i64, "imei": "IMEI", "zsource": 101, "toid": "42"}));
        let g = plan_send("IMEI", "2000000000000000002", ThreadType::Group, &s, 7).unwrap();
        assert_eq!((g.service, g.path, g.nretry), ("group", "/api/group/sticker", true));
        assert_eq!(g.params, json!({"stickerId": 38590, "cateId": 2150, "type": 3, "clientId": 7, "imei": "IMEI", "zsource": 101, "grid": "2000000000000000002"}));
        assert!(g.params.get("toid").is_none() && p.params.get("grid").is_none());
    }

    #[test]
    fn what_zca_js_refuses_is_not_sent() {
        let ok = Sticker { id: 1, cate_id: 0, kind: 1, ..Default::default() };
        assert!(plan_send("I", "42", ThreadType::User, &ok, 1).is_ok(), "cateId 0 is allowed");
        let err = |s: &Sticker, to: &str| plan_send("I", to, ThreadType::User, s, 1).unwrap_err().to_string();
        assert_eq!(err(&Sticker { id: 0, ..ok.clone() }, "42"), "Missing sticker id");
        assert_eq!(err(&Sticker { kind: 0, ..ok.clone() }, "42"), "Missing sticker type");
        assert_eq!(err(&ok, ""), "Missing threadId");
    }
}
