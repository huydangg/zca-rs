//! Incoming messages: the raw websocket JSON decoded into [`ZaloMessage`] and friends.
//! Mirrors zca-js `UserMessage` / `GroupMessage` (threadId / isSelf rules).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Whether a conversation is with one person or a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThreadType {
    /// A direct conversation; the thread id is the other person's uid.
    User,
    /// A group; the thread id is the group id.
    Group,
}

impl ThreadType {
    /// `Group` for `true`.
    pub fn from_group(group: bool) -> Self {
        if group {
            ThreadType::Group
        } else {
            ThreadType::User
        }
    }

    /// True for [`ThreadType::Group`].
    pub fn is_group(self) -> bool {
        self == ThreadType::Group
    }
}

/// An id out of a JSON string or number (empty strings count as missing).
pub(crate) fn id_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The thread a frame item belongs to, as zca-js finds it: a group's `idTo`, a DM's other party.
pub(crate) fn thread_of(raw: &Value, thread_type: ThreadType, own_uid: &str) -> (String, bool) {
    let uid_from = id_of(&raw["uidFrom"]).unwrap_or_else(|| "0".into());
    let id_to = id_of(&raw["idTo"]).unwrap_or_else(|| "0".into());
    let is_self = uid_from == "0" || uid_from == own_uid;
    let thread_id = if thread_type.is_group() {
        id_to
    } else if is_self {
        if id_to == "0" {
            own_uid.to_string()
        } else {
            id_to
        }
    } else {
        uid_from
    };
    (thread_id, is_self)
}

/// What kind of thing a message type (`msgType`) names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Sticker,
    Gif,
    Image,
    Video,
    Audio,
    File,
    Link,
    Other,
}

fn class_of(msg_type: &str) -> Class {
    let t = msg_type.to_ascii_lowercase();
    if t.contains("sticker") {
        Class::Sticker
    } else if t.contains("gif") {
        Class::Gif
    } else if t.contains("photo") || t.contains("doodle") {
        Class::Image
    } else if t.contains("video") {
        Class::Video
    } else if t.contains("voice") {
        Class::Audio
    } else if t.contains("file") {
        Class::File
    } else if t.contains("link") || t.contains("recommended") {
        Class::Link
    } else {
        Class::Other
    }
}

/// The picture URL of a sticker message, from its content `{id, catId, type}` (an object, or
/// that object as a JSON string): the static image Zalo's own web client shows, fetched by
/// sticker id from the CDN with no login. Seen in the open sources `ctdoteam1110/ctdozlite`
/// (`src/chat.rs`), `aizaloapp/zalo-flow` (`src/utils/message-parser.js`) and
/// `rin2401/sticker` (`crawl.py`); zca-js's `getStickersDetail` returns the same files, via a
/// signed API call.
pub fn sticker_url(content: &Value) -> Option<String> {
    let parsed;
    let content = match content {
        Value::String(t) => {
            parsed = serde_json::from_str::<Value>(t).ok()?;
            &parsed
        }
        c => c,
    };
    let id = id_of(&content["id"]).filter(|i| i.bytes().all(|b| b.is_ascii_digit()) && i != "0")?;
    Some(format!("{}eid={id}&size=130", crate::sticker::STICKER_URL))
}

/// `params` is a JSON *string* inside the content object.
fn params_of(content: &Value) -> Value {
    content["params"]
        .as_str()
        .and_then(|p| serde_json::from_str(p).ok())
        .unwrap_or(Value::Null)
}

/// The fields every "card" content has (photo, video, file, link, sticker, ...).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    /// The card's `title` (a file's name, a link's headline); `None` when empty.
    pub title: Option<String>,
    /// The card's `description`; `None` when empty.
    pub description: Option<String>,
    /// The card's `href`; `None` when empty.
    pub href: Option<String>,
    /// The full-resolution URL of a photo (`params.hd`); `None` when absent.
    pub hd: Option<String>,
    /// The preview image URL; `None` when empty.
    pub thumb: Option<String>,
    /// The decoded `params` (a JSON string inside the content); `Null` when absent.
    pub params: Value,
}

impl Card {
    fn of(content: &Value, params: Value) -> Card {
        let text = |v: &Value| v.as_str().filter(|t| !t.is_empty()).map(String::from);
        Card {
            title: text(&content["title"]),
            description: text(&content["description"]),
            href: text(&content["href"]),
            hd: text(&params["hd"]),
            thumb: text(&content["thumb"]),
            params,
        }
    }

    /// The best link to the thing itself: the full-resolution one when there is one.
    pub fn url(&self) -> Option<&str> {
        self.hd.as_deref().or(self.href.as_deref())
    }
}

/// What an [`Attachment`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttachmentKind {
    /// A photo or doodle.
    Image,
    /// A video.
    Video,
    /// A voice message.
    Audio,
    /// A file.
    File,
    /// A card of a type this crate does not know.
    Other,
}

/// A photo, video, voice message, file or unknown card.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    /// What it is, from the message type.
    pub kind: AttachmentKind,
    /// Its title, link, preview and parameters.
    pub card: Card,
}

/// A sticker message.
#[derive(Clone, Debug, PartialEq)]
pub struct StickerContent {
    /// Sticker id (`None` when the content has no usable one).
    pub id: Option<u64>,
    /// Sticker pack id.
    pub cat_id: Option<u64>,
    /// Sticker type (`type` in the content).
    pub kind: Option<i64>,
    /// The static CDN picture of the sticker, built from its id.
    pub url: Option<String>,
    /// The rest of the content.
    pub card: Card,
}

/// A GIF message (`chat.gif`).
///
/// The content is a card like a photo's (`href`, `thumb`, and a `params` JSON string); the
/// size is read from `params.width` / `params.height`, else from the content itself. What
/// `href` points to (a `.gif`, an animated WebP, a video) is up to Zalo: look at the bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct GifContent {
    /// The link to the moving picture (`params.hd`, else `href`); `None` when the card has none.
    pub url: Option<String>,
    /// The preview image URL; `None` when empty.
    pub thumb: Option<String>,
    /// Width in pixels, when the card says.
    pub width: Option<u32>,
    /// Height in pixels, when the card says.
    pub height: Option<u32>,
    /// The rest of the content (the message's whole JSON stays in [`ZaloMessage::raw`]).
    pub card: Card,
}

/// What a message says. The undecoded content stays in [`ZaloMessage::raw`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Content {
    /// A plain text.
    Text(String),
    /// A photo, video, voice message, file or other card.
    Attachment(Attachment),
    /// A sticker.
    Sticker(StickerContent),
    /// A GIF (`chat.gif`).
    Gif(GifContent),
    /// A shared link (`chat.recommended` / `chat.link`).
    Link(Card),
    /// A call log card.
    Call {
        /// How long the call lasted, when the card says.
        duration_secs: Option<u32>,
    },
    /// An action card that is not a call (Zalo's `...Message` / `...Action` bubbles).
    Notice {
        /// The card's internal action name.
        title: String,
    },
    /// Anything else (the content is not a string or an object).
    Other(Value),
}

/// A readable kind of "bubble"/action card, or `None` if it's a normal message. These cards
/// use an internal action name as their title (`sendBubbleMessage` for call logs).
fn bubble(title: &str, content: &Value, params: &Value) -> Option<Content> {
    if title.is_empty() {
        return None;
    }
    if !(title.ends_with("Message") || title.ends_with("Action")) {
        return None;
    }
    // call logs: params/content often mention a call or a duration (seconds)
    let blob = format!("{content} {params}").to_lowercase();
    let is_call = blob.contains("call") || blob.contains("cuộc gọi") || blob.contains("voice") || blob.contains("video");
    if !is_call {
        return Some(Content::Notice { title: title.to_string() });
    }
    let dur = ["callDuration", "duration", "call_duration", "time"]
        .iter()
        .find_map(|k| params[*k].as_i64().or_else(|| params[*k].as_str().and_then(|v| v.parse().ok())))
        .filter(|s| *s > 0 && *s < 86_400);
    Some(Content::Call { duration_secs: dur.map(|d| d as u32) })
}

impl Content {
    fn parse(content: &Value, msg_type: &str) -> Content {
        let class = class_of(msg_type);
        // a sticker's content is sometimes that object as a JSON string
        let sticker_obj = match content {
            Value::String(t) if class == Class::Sticker => serde_json::from_str::<Value>(t).ok().filter(Value::is_object),
            _ => None,
        };
        match sticker_obj.as_ref().unwrap_or(content) {
            Value::String(t) => Content::Text(t.clone()),
            c @ Value::Object(_) => {
                let params = params_of(c);
                if let Some(b) = bubble(c["title"].as_str().unwrap_or(""), c, &params) {
                    return b;
                }
                let card = Card::of(c, params);
                match class {
                    Class::Sticker => Content::Sticker(StickerContent {
                        id: sticker_id(&c["id"]),
                        cat_id: sticker_id(&c["catId"]),
                        kind: c["type"].as_i64(),
                        url: sticker_url(c),
                        card,
                    }),
                    Class::Gif => Content::Gif(GifContent {
                        url: card.url().map(String::from),
                        thumb: card.thumb.clone(),
                        width: pixels(&card.params["width"]).or_else(|| pixels(&c["width"])),
                        height: pixels(&card.params["height"]).or_else(|| pixels(&c["height"])),
                        card,
                    }),
                    Class::Link => Content::Link(card),
                    Class::Image => Content::Attachment(Attachment { kind: AttachmentKind::Image, card }),
                    Class::Video => Content::Attachment(Attachment { kind: AttachmentKind::Video, card }),
                    Class::Audio => Content::Attachment(Attachment { kind: AttachmentKind::Audio, card }),
                    Class::File => Content::Attachment(Attachment { kind: AttachmentKind::File, card }),
                    Class::Other => Content::Attachment(Attachment { kind: AttachmentKind::Other, card }),
                }
            }
            other => Content::Other(other.clone()),
        }
    }
}

/// A size in pixels out of a JSON number or a string of digits (0 is no size).
fn pixels(v: &Value) -> Option<u32> {
    sticker_id(v).filter(|n| *n > 0).and_then(|n| u32::try_from(n).ok())
}

fn sticker_id(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The message another one answers (a quote card).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    /// Uid of the quoted message's sender, when given.
    pub owner_id: Option<String>,
    /// Display name of the quoted sender (`fromD`).
    pub from_name: Option<String>,
    /// The quoted text (`msg`); `None` when empty (a photo, say).
    pub text: Option<String>,
    /// Global id of the quoted message; `None` when absent or `0`.
    pub msg_id: Option<String>,
    /// Client id of the quoted message, when given.
    pub cli_msg_id: Option<String>,
}

/// One `@mention` in an incoming message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageMention {
    /// Uid of the tagged person; `"-1"` for @All.
    pub uid: String,
    /// Offset of the tag in the text, in UTF-16 code units (JS string indices).
    pub pos: usize,
    /// Length of the tag, in UTF-16 code units.
    pub len: usize,
    /// True for @All.
    pub is_all: bool,
}

/// What a quote, a reaction and a forward of one Zalo message need: the ids and type
/// zca-js sends as `qmsg*` / `rMsg`, plus the raw content the quote card shows
/// (`qmsg` / `qmsgAttach`). Serializable, so a caller can keep one next to a stored message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ZaloRef {
    /// Global message id.
    #[serde(rename = "msgId")]
    pub msg_id: String,
    /// The client-side id (a reaction needs it; a message just sent by us has it only once its echo arrives).
    #[serde(rename = "cliMsgId", default)]
    pub cli_msg_id: String,
    /// Sender (my own uid for my messages, never "0").
    #[serde(rename = "uidFrom", default)]
    pub uid_from: String,
    /// Message type (`webchat`, `chat.photo`, ...).
    #[serde(rename = "msgType", default)]
    pub msg_type: String,
    /// As Zalo sent it (a string of milliseconds).
    #[serde(default)]
    pub ts: String,
    /// Time to live in seconds (0: forever).
    #[serde(default)]
    pub ttl: i64,
    /// Raw `content`: the text, or the attachment card.
    #[serde(default)]
    pub content: Value,
    /// Raw `propertyExt` (text styles) or null.
    #[serde(rename = "propertyExt", default)]
    pub property_ext: Value,
}

impl ZaloRef {
    /// The reference as JSON text.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// `None` when the JSON is not a Zalo reference or has no message id.
    pub fn from_json(s: &str) -> Option<ZaloRef> {
        serde_json::from_str(s).ok().filter(|r: &ZaloRef| !r.msg_id.is_empty())
    }

    /// The text quoted as `qmsg`: the message itself, for a text.
    pub fn text(&self) -> Option<&str> {
        self.content.as_str()
    }

    /// The sticker picture URL, when this is a sticker message with an id.
    pub fn sticker_url(&self) -> Option<String> {
        (class_of(&self.msg_type) == Class::Sticker).then(|| sticker_url(&self.content)).flatten()
    }
}

/// A message received from Zalo (or echoed back after we sent one).
#[derive(Clone, Debug, PartialEq)]
pub struct ZaloMessage {
    /// Global message id (`msgId`; `realMsgId` when that is missing).
    pub msg_id: String,
    /// Client-side message id (`cliMsgId`); empty when absent.
    pub cli_msg_id: String,
    /// Sender uid; the logged-in account's own uid for messages sent by us (Zalo says `"0"`).
    pub uid_from: String,
    /// Recipient (`idTo`): the group id in groups; `"0"` or the other person in DMs.
    pub id_to: String,
    /// Sender's display name (`dName`); empty when absent.
    pub d_name: String,
    /// Time sent, Unix milliseconds.
    pub ts: i64,
    /// Message type (`webchat`, `chat.photo`, `chat.sticker`, `share.file`, ...).
    pub msg_type: String,
    /// The conversation: the other person's uid in a DM, the group id in a group.
    pub thread_id: String,
    /// DM or group.
    pub thread_type: ThreadType,
    /// True when the logged-in account sent it.
    pub is_self: bool,
    /// What it says.
    pub content: Content,
    /// The message this one answers, if it is a reply.
    pub quote: Option<Quote>,
    /// People tagged in it (groups only).
    pub mentions: Vec<MessageMention>,
    /// True when it tags the logged-in account or @All, and we did not send it.
    pub mentions_me: bool,
    /// Time to live in seconds (0: forever).
    pub ttl: i64,
    /// Raw `propertyExt` (text styles) or null.
    pub property_ext: Value,
    /// The undecoded message JSON as Zalo sent it.
    pub raw: Value,
}

impl ZaloMessage {
    /// Decode one item of a `msgs` / `groupMsgs` list. `None` for frames that aren't chat
    /// messages (missing ids). `own_uid` is the logged-in account.
    pub fn from_json(raw: &Value, thread_type: ThreadType, own_uid: &str) -> Option<ZaloMessage> {
        let uid_from = id_of(&raw["uidFrom"])?;
        let id_to = id_of(&raw["idTo"]).unwrap_or_else(|| "0".into());
        let (thread_id, is_self) = thread_of(raw, thread_type, own_uid);
        let uid_from = if uid_from == "0" { own_uid.to_string() } else { uid_from };
        let msg_id = id_of(&raw["msgId"]).or_else(|| id_of(&raw["realMsgId"]))?;
        let ts = id_of(&raw["ts"]).and_then(|t| t.parse().ok()).unwrap_or_default();
        let msg_type = raw["msgType"].as_str().unwrap_or("").to_string();
        let content = Content::parse(&raw["content"], &msg_type);
        let quote = raw.get("quote").filter(|q| q.is_object()).map(|q| Quote {
            owner_id: id_of(&q["ownerId"]),
            from_name: q["fromD"].as_str().map(String::from),
            text: q["msg"].as_str().filter(|m| !m.is_empty()).map(String::from),
            msg_id: id_of(&q["globalMsgId"]).filter(|i| i != "0"),
            cli_msg_id: id_of(&q["cliMsgId"]),
        });
        // group messages carry `mentions: [{uid, pos, len, type}]`; uid "-1" = @All
        let mentions: Vec<MessageMention> = raw["mentions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let uid = id_of(&m["uid"])?;
                Some(MessageMention {
                    is_all: uid == "-1",
                    pos: m["pos"].as_u64().unwrap_or(0) as usize,
                    len: m["len"].as_u64().unwrap_or(0) as usize,
                    uid,
                })
            })
            .collect();
        let mentions_me = !is_self && mentions.iter().any(|m| m.uid == own_uid || m.is_all);
        Some(ZaloMessage {
            msg_id,
            cli_msg_id: id_of(&raw["cliMsgId"]).unwrap_or_default(),
            uid_from,
            id_to,
            d_name: raw["dName"].as_str().unwrap_or("").to_string(),
            ts,
            msg_type,
            thread_id,
            thread_type,
            is_self,
            content,
            quote,
            mentions,
            mentions_me,
            ttl: raw["ttl"].as_i64().unwrap_or(0),
            property_ext: raw["propertyExt"].clone(),
            raw: raw.clone(),
        })
    }

    /// What a quote, a reaction or a forward of this message needs.
    pub fn reference(&self) -> ZaloRef {
        ZaloRef {
            msg_id: self.msg_id.clone(),
            cli_msg_id: self.cli_msg_id.clone(),
            uid_from: self.uid_from.clone(),
            msg_type: self.msg_type.clone(),
            ts: id_of(&self.raw["ts"]).unwrap_or_default(),
            ttl: self.ttl,
            content: self.raw["content"].clone(),
            property_ext: self.property_ext.clone(),
        }
    }

    /// The text, when this is a text message.
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            Content::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// A message was recalled (zca-js `Undo`), by its sender or by us on another device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undo {
    /// The conversation: the other person's uid in a DM, the group id in a group.
    pub thread_id: String,
    /// DM or group.
    pub thread_type: ThreadType,
    /// Global id of the recalled message.
    pub msg_id: Option<String>,
    /// Client id of the recalled message.
    pub cli_msg_id: Option<String>,
}

impl Undo {
    /// An item of a `msgs` / `groupMsgs` list that is a recall (`content` is an object with
    /// `deleteMsg`, plus `globalMsgId`, `cliMsgId`, `srcId`, `destId`), else `None`.
    pub fn from_json(raw: &Value, thread_type: ThreadType, own_uid: &str) -> Option<Undo> {
        let content = raw["content"].as_object().filter(|c| c.contains_key("deleteMsg"))?;
        let (thread_id, _) = thread_of(raw, thread_type, own_uid);
        let id = |k: &str| content.get(k).and_then(id_of).filter(|v| v != "0");
        Some(Undo { thread_id, thread_type, msg_id: id("globalMsgId"), cli_msg_id: id("cliMsgId") })
    }
}

/// Someone saw messages: `reader_id` has seen the message(s) `msg_ids` (last one seen first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The conversation: the other person's uid in a DM, the group id in a group.
    pub thread_id: String,
    /// DM or group.
    pub thread_type: ThreadType,
    /// Who saw them.
    pub reader_id: String,
    /// The ids of the messages seen (`msgId`, then `realMsgId` when different).
    pub msg_ids: Vec<String>,
}

impl Seen {
    /// Decode the `data` object of a seen frame (cmd 502 DMs, 522 groups; zca-js
    /// `UserSeenMessage` / `GroupSeenMessage`). DMs carry `seens: [{idTo, msgId,
    /// realMsgId}]` where `idTo` is the other party; groups carry `groupSeens:
    /// [{msgId, groupId, seenUids: [..]}]`. The account's own seens (read on another
    /// device) are dropped.
    pub fn from_json(data: &Value, thread_type: ThreadType, own_uid: &str) -> Vec<Seen> {
        let mut out = Vec::new();
        let ids = |e: &Value| -> Vec<String> {
            let mut v: Vec<String> = [&e["msgId"], &e["realMsgId"]].into_iter().filter_map(id_of).collect();
            v.dedup();
            v
        };
        if thread_type.is_group() {
            for e in data["groupSeens"].as_array().into_iter().flatten() {
                let (Some(gid), msg_ids) = (id_of(&e["groupId"]), ids(e)) else { continue };
                for uid in e["seenUids"].as_array().into_iter().flatten().filter_map(id_of) {
                    if uid != own_uid && !msg_ids.is_empty() {
                        out.push(Seen { thread_id: gid.clone(), thread_type, reader_id: uid, msg_ids: msg_ids.clone() });
                    }
                }
            }
        } else {
            for e in data["seens"].as_array().into_iter().flatten() {
                let msg_ids = ids(e);
                if let Some(uid) = id_of(&e["idTo"]).filter(|u| u != own_uid && u != "0") {
                    if !msg_ids.is_empty() {
                        out.push(Seen { thread_id: uid.clone(), thread_type, reader_id: uid, msg_ids });
                    }
                }
            }
        }
        out
    }
}

/// Messages reached somebody's device (not yet read). Best effort: decoded from the
/// `delivereds` / `groupDelivereds` lists next to the seen lists in the receipt frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    /// The conversation: the other person's uid in a DM, the group id in a group.
    pub thread_id: String,
    /// DM or group.
    pub thread_type: ThreadType,
    /// Who the messages reached.
    pub user_ids: Vec<String>,
    /// The id of the delivered message.
    pub msg_id: String,
}

impl Delivered {
    /// Decode the `data` object of a receipt frame (cmd 502 DMs, 522 groups). The account's
    /// own devices are dropped.
    pub fn from_json(data: &Value, thread_type: ThreadType, own_uid: &str) -> Vec<Delivered> {
        let mut out = Vec::new();
        let key = if thread_type.is_group() { "groupDelivereds" } else { "delivereds" };
        for e in data[key].as_array().into_iter().flatten() {
            let Some(msg_id) = id_of(&e["msgId"]).or_else(|| id_of(&e["realMsgId"])) else { continue };
            let user_ids: Vec<String> = e["deliveredUids"].as_array().into_iter().flatten().filter_map(id_of).filter(|u| u != own_uid).collect();
            let thread = if thread_type.is_group() { id_of(&e["groupId"]) } else { user_ids.first().cloned() };
            if let (Some(thread_id), false) = (thread, user_ids.is_empty()) {
                out.push(Delivered { thread_id, thread_type, user_ids, msg_id });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ME: &str = "1000000000000000001";

    fn dm(raw: &Value) -> ZaloMessage {
        ZaloMessage::from_json(raw, ThreadType::User, ME).unwrap()
    }

    fn group(raw: &Value) -> ZaloMessage {
        ZaloMessage::from_json(raw, ThreadType::Group, ME).unwrap()
    }

    #[test]
    fn dm_from_other_and_from_self() {
        let incoming = json!({"msgId":"1","uidFrom":"42","idTo":"0","dName":"Alice","ts":"1700000000001","msgType":"webchat","content":"hello"});
        let m = dm(&incoming);
        assert_eq!((m.thread_id.as_str(), m.is_self, m.d_name.as_str(), m.ts), ("42", false, "Alice", 1700000000001));
        assert_eq!(m.text(), Some("hello"));
        let mine = json!({"msgId":"2","uidFrom":"0","idTo":"42","dName":"","ts":1700000000002u64,"msgType":"webchat","content":"test message"});
        let m = dm(&mine);
        assert_eq!((m.thread_id.as_str(), m.is_self, m.uid_from.as_str()), ("42", true, ME));
        // the thread of my own message to nobody in particular is my own chat
        let notes = json!({"msgId":"3","uidFrom":"0","idTo":"0","ts":"1","msgType":"webchat","content":"x"});
        assert_eq!(dm(&notes).thread_id, ME);
        assert!(ZaloMessage::from_json(&json!({"foo": 1}), ThreadType::User, ME).is_none());
    }

    #[test]
    fn call_bubble_is_a_call_and_other_action_cards_are_notices() {
        let call = json!({"msgId":"9","uidFrom":"0","idTo":"42","ts":"1","msgType":"chat.recommended",
            "content":{"title":"sendBubbleMessage","params":"{\"callDuration\":28}","action":"recommened.voiceCall"}});
        assert_eq!(dm(&call).content, Content::Call { duration_secs: Some(28) });
        let other = json!({"msgId":"8","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.recommended",
            "content":{"title":"sendBubbleMessage","params":"{}"}});
        assert_eq!(dm(&other).content, Content::Notice { title: "sendBubbleMessage".into() });
        // a call without a usable duration
        let no_dur = json!({"msgId":"7","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.recommended","content":{"title":"sendBubbleMessage","params":"{\"callDuration\":0,\"x\":\"call\"}"}});
        assert_eq!(dm(&no_dur).content, Content::Call { duration_secs: None });
    }

    #[test]
    fn group_photo_prefers_hd_and_keeps_quote() {
        let raw = json!({
            "msgId": 7000000000000000001u64, "uidFrom": "42", "idTo": "2000000000000000002", "dName": "Alice",
            "ts": "1700000000100", "msgType": "chat.photo",
            "content": {"title":"","description":"","href":"https://f.zdn.vn/small.jpg","thumb":"https://f.zdn.vn/t.jpg",
                        "params":"{\"hd\":\"https://f.zdn.vn/hd.jpg\",\"width\":1920}"},
            "quote": {"ownerId": 1, "fromD": "Bob", "msg": "send the photo", "globalMsgId": 7000000000000000000u64}
        });
        let m = group(&raw);
        assert_eq!((m.thread_id.as_str(), m.thread_type, m.msg_id.as_str()), ("2000000000000000002", ThreadType::Group, "7000000000000000001"));
        let Content::Attachment(a) = &m.content else { panic!("{:?}", m.content) };
        assert_eq!(a.kind, AttachmentKind::Image);
        assert_eq!(a.card.url(), Some("https://f.zdn.vn/hd.jpg"));
        assert_eq!(a.card.thumb.as_deref(), Some("https://f.zdn.vn/t.jpg"));
        let q = m.quote.as_ref().unwrap();
        assert_eq!((q.text.as_deref(), q.from_name.as_deref(), q.msg_id.as_deref()), (Some("send the photo"), Some("Bob"), Some("7000000000000000000")));
        assert_eq!(m.raw["msgType"], "chat.photo", "the raw JSON is kept");
    }

    #[test]
    fn group_mentions_me_or_all() {
        let mk = |uid_from: &str, mentions: Value| {
            let raw = json!({"msgId":"9","uidFrom":uid_from,"idTo":"2000000000000000002","dName":"Alice","ts":"1700000000100",
                "msgType":"webchat","content":"@Bob look","mentions":mentions});
            group(&raw)
        };
        let me = json!([{"uid": ME, "pos": 0, "len": 4, "type": 0}]);
        let other = json!([{"uid": "555", "pos": 0, "len": 4, "type": 0}]);
        let all = json!([{"uid": "-1", "pos": 0, "len": 4, "type": 1}]);
        assert!(mk("42", me.clone()).mentions_me);
        assert!(!mk("42", other).mentions_me);
        let m = mk("42", all);
        assert!(m.mentions_me && m.mentions[0].is_all);
        assert!(!mk("42", json!([])).mentions_me);
        assert!(!mk(ME, me).mentions_me); // my own message
        let none = json!({"msgId":"1","uidFrom":"42","idTo":"0","ts":"1","msgType":"webchat","content":"hi"});
        assert!(!group(&none).mentions_me);
        assert_eq!((mk("42", json!([{"uid": ME, "pos": 3, "len": 4}])).mentions[0].pos, 3), (3, 3));
    }

    #[test]
    fn file_keeps_its_title_and_link_is_a_link() {
        let file = json!({"msgId":"3","uidFrom":"42","idTo":"0","ts":"1","msgType":"share.file",
            "content":{"title":"bao-cao-Q3.xlsx","href":"https://f.zdn.vn/f.xlsx","params":"{\"fileSize\":\"12345\"}"}});
        let Content::Attachment(a) = dm(&file).content else { panic!() };
        assert_eq!((a.kind, a.card.title.as_deref(), a.card.url()), (AttachmentKind::File, Some("bao-cao-Q3.xlsx"), Some("https://f.zdn.vn/f.xlsx")));
        assert_eq!(a.card.params["fileSize"], "12345");
        let link = json!({"msgId":"4","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.recommended",
            "content":{"title":"MR !377","href":"https://gitlab.example/mr/377"}});
        let Content::Link(c) = dm(&link).content else { panic!() };
        assert_eq!((c.title.as_deref(), c.href.as_deref()), (Some("MR !377"), Some("https://gitlab.example/mr/377")));
        // a content that is neither text nor a card
        let odd = json!({"msgId":"5","uidFrom":"42","idTo":"0","ts":"1","msgType":"webchat","content":null});
        assert_eq!(dm(&odd).content, Content::Other(Value::Null));
    }

    #[test]
    fn a_gif_message_is_typed_with_its_link_preview_and_size() {
        // sizes in `params` (numbers or strings); a high-resolution link wins over `href`
        let raw = json!({"msgId":"20","uidFrom":"42","idTo":"0","dName":"Alice","ts":"1700000000000","msgType":"chat.gif",
            "content":{"title":"","description":"","href":"https://f.example.test/cat.gif","thumb":"https://f.example.test/cat-t.jpg",
                       "params":"{\"width\":\"480\",\"height\":270}"}});
        let m = dm(&raw);
        let Content::Gif(g) = &m.content else { panic!("{:?}", m.content) };
        assert_eq!(g.url.as_deref(), Some("https://f.example.test/cat.gif"));
        assert_eq!((g.thumb.as_deref(), g.width, g.height), (Some("https://f.example.test/cat-t.jpg"), Some(480), Some(270)));
        assert_eq!(g.card.params["width"], "480");
        let hd = json!({"msgId":"21","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.gif",
            "content":{"href":"https://f.example.test/a.gif","params":"{\"hd\":\"https://f.example.test/hd.gif\"}","width":64,"height":"0"}});
        let Content::Gif(g) = dm(&hd).content else { panic!() };
        // the size may sit in the content itself; 0 is no size
        assert_eq!((g.url.as_deref(), g.width, g.height), (Some("https://f.example.test/hd.gif"), Some(64), None));
        // a GIF without a link is still a GIF; a photo stays an attachment
        let bare = json!({"msgId":"22","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.gif","content":{"title":""}});
        let Content::Gif(g) = dm(&bare).content else { panic!() };
        assert_eq!((g.url, g.thumb, g.width, g.height), (None, None, None, None));
        let photo = json!({"msgId":"23","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.photo","content":{"href":"https://f.example.test/p.jpg"}});
        assert!(matches!(dm(&photo).content, Content::Attachment(_)));
        assert_eq!(m.raw["msgType"], "chat.gif", "the raw JSON is kept");
    }

    #[test]
    fn a_sticker_has_the_cdn_picture_url_built_from_its_id() {
        let want = "https://zalo-api.zadn.vn/api/emoticon/sticker/webpc?eid=2005&size=130";
        // as an object, with the id as a number or a string, and as a JSON string
        assert_eq!(sticker_url(&json!({"id": 2005, "catId": 1002, "type": 1})).as_deref(), Some(want));
        assert_eq!(sticker_url(&json!({"id": "2005", "catId": "1002", "type": 1})).as_deref(), Some(want));
        assert_eq!(sticker_url(&json!("{\"catId\":1002,\"id\":2005,\"type\":1}")).as_deref(), Some(want));
        // no usable id: nothing to build (and nothing a crafted id could inject)
        for bad in [json!({"catId": 1}), json!({"id": 0}), json!({"id": "1&x=2"}), json!("hello"), json!(null)] {
            assert_eq!(sticker_url(&bad), None, "{bad}");
        }
        let raw = json!({"msgId":"77","uidFrom":"42","idTo":"0","dName":"Dave","ts":"1","msgType":"chat.sticker","content":{"id":2005,"catId":1002,"type":1}});
        let Content::Sticker(s) = dm(&raw).content else { panic!() };
        assert_eq!((s.id, s.cat_id, s.kind, s.url.as_deref()), (Some(2005), Some(1002), Some(1), Some(want)));
        // the same content as a JSON string is a sticker too, not a text of raw JSON
        let raw = json!({"msgId":"78","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.sticker","content":"{\"catId\":1002,\"id\":2005,\"type\":1}"});
        let Content::Sticker(s) = dm(&raw).content else { panic!() };
        assert_eq!(s.url.as_deref(), Some(want));
        // a photo keeps its own URL and has no sticker picture
        let photo = json!({"msgId":"79","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.photo","content":{"href":"https://f.zdn.vn/p.jpg","id":5}});
        let Content::Attachment(a) = dm(&photo).content else { panic!() };
        assert_eq!(a.card.url(), Some("https://f.zdn.vn/p.jpg"));
    }

    #[test]
    fn the_sticker_url_is_rebuilt_from_a_reference() {
        let raw = json!({"msgId":"77","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.sticker","content":{"id":2005,"catId":1002,"type":1}});
        let r = dm(&raw).reference();
        assert_eq!(r.sticker_url().as_deref(), Some("https://zalo-api.zadn.vn/api/emoticon/sticker/webpc?eid=2005&size=130"));
        let text = dm(&json!({"msgId":"1","uidFrom":"42","idTo":"0","ts":"1","msgType":"webchat","content":"hi"})).reference();
        assert_eq!(text.sticker_url(), None);
    }

    #[test]
    fn a_recall_frame_item_names_the_chat_and_the_message_by_global_and_client_id() {
        // zca-js `Undo`: content = {globalMsgId, cliMsgId, deleteMsg, srcId, destId}
        let dm_undo = json!({"msgId":"900","cliMsgId":"901","uidFrom":"42","idTo":"0","ts":"5","msgType":"chat.undo",
            "content":{"globalMsgId":7000000000000000001u64,"cliMsgId":1700000000123u64,"deleteMsg":7000000000000000009u64,"srcId":1,"destId":2}});
        assert_eq!(
            Undo::from_json(&dm_undo, ThreadType::User, ME),
            Some(Undo { thread_id: "42".into(), thread_type: ThreadType::User, msg_id: Some("7000000000000000001".into()), cli_msg_id: Some("1700000000123".into()) })
        );
        // my own recall from another device: the chat is the one I wrote to
        let mine = json!({"msgId":"1","uidFrom":"0","idTo":"42","content":{"globalMsgId":"55","cliMsgId":"0","deleteMsg":3}});
        let u = Undo::from_json(&mine, ThreadType::User, ME).unwrap();
        assert_eq!((u.thread_id.as_str(), u.msg_id.as_deref(), u.cli_msg_id), ("42", Some("55"), None));
        // a group: the group, whoever recalled
        let grp = json!({"msgId":"1","uidFrom":"42","idTo":"2000000000000000002","content":{"globalMsgId":"56","cliMsgId":"57","deleteMsg":3}});
        let u = Undo::from_json(&grp, ThreadType::Group, ME).unwrap();
        assert_eq!((u.thread_id.as_str(), u.msg_id.as_deref(), u.cli_msg_id.as_deref()), ("2000000000000000002", Some("56"), Some("57")));
        // ordinary messages (text, cards) are not recalls
        let text = json!({"msgId":"2","uidFrom":"42","idTo":"0","ts":"1","msgType":"webchat","content":"hi"});
        let card = json!({"msgId":"3","uidFrom":"42","idTo":"0","ts":"1","msgType":"chat.photo","content":{"href":"https://f.zdn.vn/a.jpg"}});
        assert_eq!((Undo::from_json(&text, ThreadType::User, ME), Undo::from_json(&card, ThreadType::User, ME)), (None, None));
    }

    #[test]
    fn dm_seen_event_names_the_other_party() {
        // decoded `data` of a cmd-502 frame: delivered + seen lists side by side
        let data = json!({"delivereds": [{"deliveredUids": ["42"], "msgId": "7000000000000000002"}],
            "seens": [{"idTo": "42", "msgId": "7000000000000000002", "realMsgId": "7000000000000000009"}]});
        assert_eq!(
            Seen::from_json(&data, ThreadType::User, ME),
            [Seen { thread_id: "42".into(), thread_type: ThreadType::User, reader_id: "42".into(), msg_ids: vec!["7000000000000000002".into(), "7000000000000000009".into()] }]
        );
        assert_eq!(
            Delivered::from_json(&data, ThreadType::User, ME),
            [Delivered { thread_id: "42".into(), thread_type: ThreadType::User, user_ids: vec!["42".into()], msg_id: "7000000000000000002".into() }]
        );
        // numeric ids, and my own read on another device, are handled
        let data = json!({"seens": [{"idTo": 42, "msgId": 5}, {"idTo": ME, "msgId": "6"}, {"idTo": "43"}]});
        let got = Seen::from_json(&data, ThreadType::User, ME);
        assert_eq!((got.len(), got[0].reader_id.as_str(), got[0].msg_ids.clone()), (1, "42", vec!["5".to_string()]));
        assert!(Seen::from_json(&json!({}), ThreadType::User, ME).is_empty());
        assert!(Delivered::from_json(&json!({}), ThreadType::User, ME).is_empty());
    }

    #[test]
    fn group_seen_event_lists_every_reader_but_me() {
        let data = json!({"groupSeens": [
            {"msgId": "7000000000000000003", "groupId": "2000000000000000002", "seenUids": ["42", ME, "77"]},
            {"msgId": "8", "groupId": "11", "seenUids": []}
        ]});
        let got = Seen::from_json(&data, ThreadType::Group, ME);
        assert_eq!(got.iter().map(|g| (g.thread_id.as_str(), g.reader_id.as_str())).collect::<Vec<_>>(), [("2000000000000000002", "42"), ("2000000000000000002", "77")]);
        assert_eq!(got[0].msg_ids, ["7000000000000000003"]);
        // a DM-shaped payload on the group path decodes to nothing
        assert!(Seen::from_json(&json!({"seens": [{"idTo": "42", "msgId": "1"}]}), ThreadType::Group, ME).is_empty());
    }

    #[test]
    fn every_message_carries_the_reference_a_quote_a_reaction_and_a_forward_need() {
        let raw = json!({"msgId": 7000000000000000001u64, "cliMsgId": "1700000000123", "uidFrom": "42", "idTo": "0", "dName": "Dave", "ts": "1700000000500",
            "msgType": "webchat", "ttl": 30, "content": "meeting at 3", "propertyExt": {"color": 0, "size": 0, "type": 0, "subType": 0, "ext": "{}"}});
        let z = dm(&raw).reference();
        assert_eq!(
            (z.msg_id.as_str(), z.cli_msg_id.as_str(), z.uid_from.as_str(), z.msg_type.as_str(), z.ts.as_str(), z.ttl, z.text()),
            ("7000000000000000001", "1700000000123", "42", "webchat", "1700000000500", 30, Some("meeting at 3"))
        );
        assert_eq!(z.property_ext["ext"], "{}");
        // my own message: the sender is my uid, never "0"
        let mine = json!({"msgId": "2", "cliMsgId": 77, "uidFrom": "0", "idTo": "42", "ts": 5, "msgType": "webchat", "content": "ok"});
        let z = dm(&mine).reference();
        assert_eq!((z.uid_from.as_str(), z.cli_msg_id.as_str(), z.ts.as_str()), (ME, "77", "5"));
        // an attachment keeps its raw card (the quote's qmsgAttach is made of it) and its type
        let photo = json!({"msgId": "3", "cliMsgId": "4", "uidFrom": "42", "idTo": "0", "ts": "1", "msgType": "chat.photo",
            "content": {"title": "", "href": "https://f.zdn.vn/a.jpg", "thumb": "https://f.zdn.vn/t.jpg", "params": "{}"}});
        let z = ZaloRef::from_json(&dm(&photo).reference().to_json()).unwrap();
        assert_eq!((z.msg_type.as_str(), z.text(), z.content["href"].as_str()), ("chat.photo", None, Some("https://f.zdn.vn/a.jpg")));
        // not a reference: no id, or not JSON
        assert!(ZaloRef::from_json("{}").is_none() && ZaloRef::from_json("nope").is_none());
    }
}
