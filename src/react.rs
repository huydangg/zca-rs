//! Zalo reactions: the icon codes of the web client, the websocket event (cmd 612)
//! and its decoding. Ported from zca-js (`Reactions` in models/Reaction.js, `addReaction`
//! for the codes, `Listener` for the event shape).
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::message::{id_of, ThreadType};
use serde_json::Value;

/// A reaction icon. The six of the web client's reaction panel can be sent; others only
/// arrive from people using other Zalo apps.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReactionIcon {
    /// Heart, `/-heart`.
    Heart,
    /// Thumbs up, `/-strong`.
    Like,
    /// Laughing, `:>`.
    Haha,
    /// Surprised, `:o`.
    Wow,
    /// Crying, `:-((`.
    Cry,
    /// Angry, `:-h`.
    Angry,
    /// Any other icon, by its `rIcon` code (see [`ReactionIcon::emoji`]).
    Other(String),
    /// No icon: sending this takes your reaction back; receiving it means someone did.
    None,
}

/// The icons of the `e` panel: (icon, emoji, `rIcon`, `rType`). `source` is always 6.
const PANEL: [(ReactionIcon, &str, &str, i32); 6] = [
    (ReactionIcon::Heart, "❤️", "/-heart", 5),
    (ReactionIcon::Like, "👍", "/-strong", 3),
    (ReactionIcon::Haha, "😆", ":>", 0),
    (ReactionIcon::Wow, "😮", ":o", 32),
    (ReactionIcon::Cry, "😭", ":-((", 2),
    (ReactionIcon::Angry, "😡", ":-h", 20),
];

/// More icons zca-js knows, by `rIcon`, for reactions other people made on their phones.
const MORE: [(&str, &str); 22] = [
    (":-*", "😘"),
    (":')", "😂"),
    ("/-shit", "💩"),
    ("/-rose", "🌹"),
    ("/-break", "💔"),
    ("/-weak", "👎"),
    (";xx", "😍"),
    (";-/", "😕"),
    (";-)", "😉"),
    ("/-li", "☀️"),
    ("/-bd", "🎂"),
    ("/-ok", "👌"),
    ("/-v", "✌️"),
    ("/-thanks", "🙏"),
    ("_()_", "🙏"),
    ("/-punch", "👊"),
    ("/-beer", "🍺"),
    ("--b", "😢"),
    (":((", "😭"),
    (":))", "😆"),
    ("/-loveu", "🥰"),
    (":-o", "😮"),
];

impl ReactionIcon {
    /// The six icons of the web client's reaction panel, in its order.
    pub fn panel() -> [ReactionIcon; 6] {
        PANEL.map(|p| p.0)
    }

    /// The icon for a `rIcon` code; `""` is [`ReactionIcon::None`].
    pub fn from_code(code: &str) -> ReactionIcon {
        if code.is_empty() {
            return ReactionIcon::None;
        }
        PANEL.iter().find(|p| p.2 == code).map(|p| p.0.clone()).unwrap_or_else(|| ReactionIcon::Other(code.to_string()))
    }

    /// The `rIcon` code Zalo uses.
    pub fn code(&self) -> &str {
        match self {
            ReactionIcon::Other(c) => c,
            ReactionIcon::None => "",
            panel => PANEL.iter().find(|p| &p.0 == panel).map_or("", |p| p.2),
        }
    }

    /// The `rType` Zalo sends with the icon (-1 for none and for icons outside the panel).
    pub fn r_type(&self) -> i32 {
        PANEL.iter().find(|p| &p.0 == self).map_or(-1, |p| p.3)
    }

    /// The emoji for the icon; an unknown code is shown as it is, [`ReactionIcon::None`] as `""`.
    pub fn emoji(&self) -> String {
        match self {
            ReactionIcon::None => String::new(),
            ReactionIcon::Other(c) => MORE.iter().find(|(i, _)| i == c).map_or_else(|| c.clone(), |(_, e)| (*e).to_string()),
            panel => PANEL.iter().find(|p| &p.0 == panel).map_or("", |p| p.1).to_string(),
        }
    }

    /// The panel icon whose emoji is `emoji` (exactly), if any.
    pub fn from_emoji(emoji: &str) -> Option<ReactionIcon> {
        PANEL.iter().find(|p| p.1 == emoji).map(|p| p.0.clone())
    }
}

/// One reaction (or its removal) seen on the websocket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    /// The conversation: the other person's uid in a DM, the group id in a group.
    pub thread_id: String,
    /// DM or group.
    pub thread_type: ThreadType,
    /// The reacted message (`gMsgID`, the global message id).
    pub msg_id: String,
    /// The reacted message's client id (`cMsgID`), when given.
    pub cli_msg_id: Option<String>,
    /// Who reacted: the sender's uid, or the logged-in account's own uid for our own (Zalo says `"0"`).
    pub user_id: String,
    /// True when the logged-in account reacted (maybe from another device).
    pub is_self: bool,
    /// The reacting person's display name (`dName`); empty for our own.
    pub user_name: String,
    /// The icon; [`ReactionIcon::None`] when the reaction was taken back.
    pub icon: ReactionIcon,
}

impl Reaction {
    /// Decode the `data` object of a cmd-612 frame: `reacts` (DMs) and `reactGroups`
    /// (groups), each `{msgId, cliMsgId, msgType, uidFrom, idTo, dName, ts, content}`
    /// where `content` is a JSON string `{rMsg: [{gMsgID, cMsgID, msgType}], rIcon, rType, source}`.
    /// Oldest first: a person's newer reaction on a message replaces their older one.
    pub fn from_json(data: &Value, own_uid: &str) -> Vec<Reaction> {
        let mut out = Vec::new();
        for (key, thread_type) in [("reacts", ThreadType::User), ("reactGroups", ThreadType::Group)] {
            for item in data[key].as_array().into_iter().flatten() {
                let content: Value = match &item["content"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                    v => v.clone(),
                };
                let (Some(from), id_to) = (id_of(&item["uidFrom"]), id_of(&item["idTo"]).unwrap_or_else(|| "0".into())) else { continue };
                let mine = from == "0" || from == own_uid;
                let thread_id = if thread_type.is_group() {
                    id_to
                } else if mine {
                    if id_to == "0" { own_uid.to_string() } else { id_to }
                } else {
                    from.clone()
                };
                let icon = ReactionIcon::from_code(content["rIcon"].as_str().unwrap_or(""));
                for r in content["rMsg"].as_array().into_iter().flatten() {
                    let Some(msg_id) = id_of(&r["gMsgID"]) else { continue };
                    out.push(Reaction {
                        thread_id: thread_id.clone(),
                        thread_type,
                        msg_id,
                        cli_msg_id: id_of(&r["cMsgID"]),
                        user_id: if mine { own_uid.to_string() } else { from.clone() },
                        is_self: mine,
                        user_name: if mine { String::new() } else { item["dName"].as_str().unwrap_or("").to_string() },
                        icon: icon.clone(),
                    });
                }
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

    #[test]
    fn the_panel_maps_to_zca_js_codes() {
        let panel = ReactionIcon::panel();
        let codes: Vec<(&str, i32)> = panel.iter().map(|i| (i.code(), i.r_type())).collect();
        assert_eq!(codes, [("/-heart", 5), ("/-strong", 3), (":>", 0), (":o", 32), (":-((", 2), (":-h", 20)]);
        assert_eq!(ReactionIcon::from_emoji("❤️"), Some(ReactionIcon::Heart));
        assert_eq!(ReactionIcon::from_emoji("🎉"), None);
        // taking a reaction back: an empty icon with rType -1
        assert_eq!((ReactionIcon::None.code(), ReactionIcon::None.r_type()), ("", -1));
        for icon in ReactionIcon::panel() {
            assert_eq!(ReactionIcon::from_code(icon.code()), icon);
        }
    }

    #[test]
    fn icons_map_back_to_emoji_unknown_ones_stay_as_they_are() {
        assert_eq!(ReactionIcon::from_code("/-heart").emoji(), "❤️");
        assert_eq!(ReactionIcon::from_code("/-strong").emoji(), "👍");
        assert_eq!(ReactionIcon::from_code(":')").emoji(), "😂");
        assert_eq!(ReactionIcon::from_code("/-weird").emoji(), "/-weird");
        assert_eq!(ReactionIcon::from_code("").emoji(), "");
    }

    /// A cmd-612 body as `decode_event_data` gives it: DM and group reactions, content as a string.
    fn frame() -> Value {
        json!({
            "reacts": [
                {"actionId": "1", "msgId": "900", "cliMsgId": "0", "msgType": "1", "uidFrom": "42", "idTo": "0", "dName": "Alice", "ts": "1700000041000",
                 "content": "{\"rMsg\":[{\"gMsgID\":7000000000000000001,\"cMsgID\":1700000000000,\"msgType\":1}],\"rIcon\":\"/-heart\",\"rType\":5,\"source\":6}"},
                {"msgId": "901", "uidFrom": "0", "idTo": "42", "dName": "", "ts": "1700000042000",
                 "content": "{\"rMsg\":[{\"gMsgID\":\"7000000000000000002\",\"cMsgID\":\"7\",\"msgType\":1}],\"rIcon\":\"/-strong\",\"rType\":3,\"source\":6}"}
            ],
            "reactGroups": [
                {"msgId": "902", "uidFrom": "77", "idTo": "2000000000000000002", "dName": "Carol", "ts": "1700000043000",
                 "content": "{\"rMsg\":[{\"gMsgID\":\"7000000000000000003\",\"cMsgID\":\"8\",\"msgType\":1}],\"rIcon\":\":o\",\"rType\":32,\"source\":6}"},
                {"msgId": "903", "uidFrom": ME, "idTo": "2000000000000000002", "dName": "Bob", "ts": "1700000044000",
                 "content": "{\"rMsg\":[{\"gMsgID\":\"7000000000000000003\",\"cMsgID\":\"8\",\"msgType\":1}],\"rIcon\":\"\",\"rType\":-1,\"source\":6}"}
            ]
        })
    }

    #[test]
    fn a_612_frame_decodes_dm_and_group_reactions() {
        let got = Reaction::from_json(&frame(), ME);
        assert_eq!(got.len(), 4);
        // a DM reaction from them: the chat is them; the message id may be a JSON number
        assert_eq!(
            got[0],
            Reaction { thread_id: "42".into(), thread_type: ThreadType::User, msg_id: "7000000000000000001".into(), cli_msg_id: Some("1700000000000".into()), user_id: "42".into(), is_self: false, user_name: "Alice".into(), icon: ReactionIcon::Heart }
        );
        // uidFrom "0" is me: the chat is the other party (idTo)
        assert_eq!((got[1].thread_id.as_str(), got[1].user_id.as_str(), got[1].is_self, got[1].user_name.as_str(), &got[1].icon), ("42", ME, true, "", &ReactionIcon::Like));
        // groups: the group id, the sender's uid and name
        assert_eq!(
            got[2],
            Reaction { thread_id: "2000000000000000002".into(), thread_type: ThreadType::Group, msg_id: "7000000000000000003".into(), cli_msg_id: Some("8".into()), user_id: "77".into(), is_self: false, user_name: "Carol".into(), icon: ReactionIcon::Wow }
        );
        // my own uid counts as me too, and an empty icon is the reaction taken back
        assert_eq!((got[3].is_self, &got[3].icon), (true, &ReactionIcon::None));
    }

    #[test]
    fn junk_frames_decode_to_nothing_and_never_panic() {
        assert!(Reaction::from_json(&json!({}), ME).is_empty());
        assert!(Reaction::from_json(&json!({"reacts": [{"uidFrom": "42", "content": "not json"}]}), ME).is_empty());
        assert!(Reaction::from_json(&json!({"reacts": [{"content": "{}"}, {"uidFrom": "42", "content": {"rMsg": [{}], "rIcon": "/-heart"}}]}), ME).is_empty());
        // content as an object (not a string) is accepted too
        let ok = Reaction::from_json(&json!({"reacts": [{"uidFrom": "42", "idTo": "0", "content": {"rMsg": [{"gMsgID": "5"}], "rIcon": ":>"}}]}), ME);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].icon, ReactionIcon::Haha);
    }

    /// The whole path of a cmd-612 frame: framing, the (plain) event encoding, then the decoder.
    #[test]
    fn a_612_websocket_frame_goes_through_the_event_decoder_to_reactions() {
        use crate::events::{cmd, decode_event_data, encode_frame, parse_frame};
        let inner = json!({"data": frame(), "error_code": 0}).to_string();
        let body = json!({"encrypt": 0, "data": inner});
        let bytes = encode_frame(1, cmd::REACTIONS, 0, &body);
        let f = parse_frame(&bytes).unwrap().unwrap();
        assert_eq!((f.version, f.cmd, f.sub), (1, 612, 0));
        let data = decode_event_data(&f.body, None).unwrap();
        let got = Reaction::from_json(&data["data"], ME);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].icon, ReactionIcon::Heart);
        assert_eq!(got[3].icon, ReactionIcon::None);
    }
}
